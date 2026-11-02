//! Whole-machine debugger commands. All guest operations use the lab daemon.

use std::collections::BTreeMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use clap::{Args, Subcommand, ValueEnum};
use vmlab_hv_proto::debug::{
    AddressSpace, Breakpoint, BreakpointKind, DebugRequest, DebugResponse, GENERAL_REGISTERS,
    MEMORY_LIMIT, SEGMENT_REGISTERS,
};
use vmlab_hv_proto::gdb::DebugTarget;

use super::daemon::remote;
use super::lab::{lab_client_for, rt, split_vm_ref};
use crate::proto::client::LabClient;
use crate::proto::{CommandError, LabRequest};

#[derive(Args)]
pub struct DebugArgs {
    /// Machine, as [lab/]machine. It must already be running.
    machine: String,
    /// Emit the typed response as JSON.
    #[arg(long, global = true)]
    json: bool,
    /// vCPU used for registers, stepping, and virtual addresses.
    #[arg(long, default_value_t = 0, global = true)]
    vcpu: u8,
    /// Interpret memory addresses as linear addresses in the selected vCPU.
    #[arg(long = "virtual", global = true)]
    virtual_address: bool,
    #[command(subcommand)]
    operation: DebugCommand,
}

#[derive(Subcommand)]
enum DebugCommand {
    /// Report run state, stop reason, vCPUs, and hardware breakpoints.
    Status,
    /// Stop all vCPUs and wait until their pending exits complete.
    Pause,
    /// Resume every vCPU, retaining hardware breakpoints.
    Continue,
    /// Execute one instruction on --vcpu, keeping other vCPUs stopped.
    Step,
    /// Remove debugger state and resume, unless --leave-paused is set.
    Detach {
        #[arg(long)]
        leave_paused: bool,
    },
    /// Read up to 65536 bytes. Addresses accept decimal or 0x hexadecimal.
    Read {
        #[arg(value_parser = number)]
        address: u64,
        length: usize,
    },
    /// Write contiguous hex bytes to RAM. The VM must be paused.
    Write {
        #[arg(value_parser = number)]
        address: u64,
        #[arg(value_parser = hex_bytes)]
        data: HexBytes,
    },
    /// Search up to 16 MiB for a hex byte sequence, including overlapping matches.
    Search {
        #[arg(value_parser = number)]
        address: u64,
        length: usize,
        #[arg(value_parser = hex_bytes)]
        pattern: HexBytes,
    },
    /// Dump a paused memory range to a new file. Existing files are preserved.
    Dump {
        #[arg(value_parser = number)]
        address: u64,
        length: usize,
        path: PathBuf,
    },
    /// Inspect registers, or apply name=value edits before inspecting them.
    Registers { values: Vec<String> },
    /// Add a linear-address hardware breakpoint to every vCPU. Four slots total.
    Break {
        #[arg(value_parser = number)]
        address: u64,
        #[arg(long, value_enum, default_value = "execute")]
        kind: Kind,
        #[arg(long, default_value_t = 1)]
        length: u8,
    },
    /// Remove the breakpoint matching the address, kind, and length.
    Unbreak {
        #[arg(value_parser = number)]
        address: u64,
        #[arg(long, value_enum, default_value = "execute")]
        kind: Kind,
        #[arg(long, default_value_t = 1)]
        length: u8,
    },
    /// Serve one GDB or LLDB connection. Attaching pauses the VM.
    Gdb {
        #[arg(long, default_value = "127.0.0.1:1234")]
        listen: SocketAddr,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Execute,
    Write,
    ReadWrite,
}

impl From<Kind> for BreakpointKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Execute => Self::Execute,
            Kind::Write => Self::Write,
            Kind::ReadWrite => Self::ReadWrite,
        }
    }
}

fn number(text: &str) -> std::result::Result<u64, String> {
    let value = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    value.map_err(|_| format!("invalid address or value {text:?}; use decimal or 0x hexadecimal"))
}

#[derive(Clone)]
struct HexBytes(Vec<u8>);

fn hex_bytes(text: &str) -> std::result::Result<HexBytes, String> {
    let data = hex::decode(text).map_err(|e| format!("invalid hex bytes: {e}"))?;
    if data.is_empty() || data.len() > MEMORY_LIMIT {
        return Err("hex data must contain 1..=65536 bytes".into());
    }
    Ok(HexBytes(data))
}

async fn request(
    client: &LabClient,
    machine: &str,
    request: DebugRequest,
) -> Result<DebugResponse> {
    let value = client
        .send(LabRequest::MachineDebug {
            machine: machine.into(),
            request,
        })
        .await
        .map_err(remote)?;
    serde_json::from_value(value).context("invalid debugger response from lab daemon")
}

/// Parsing and local validation finish before a daemon is contacted.
pub fn run(args: DebugArgs) -> Result<()> {
    let space = if args.virtual_address {
        AddressSpace::Virtual { vcpu: args.vcpu }
    } else {
        AddressSpace::Physical
    };
    let mut edits = BTreeMap::new();
    if let DebugCommand::Registers { values } = &args.operation {
        for assignment in values {
            let (name, value) = assignment
                .split_once('=')
                .ok_or_else(|| CommandError::invalid("register edit must be name=value"))?;
            let known = GENERAL_REGISTERS.contains(&name)
                || SEGMENT_REGISTERS.contains(&name)
                || [
                    "cr0", "cr2", "cr3", "cr4", "cr8", "efer", "fs_base", "gs_base",
                ]
                .contains(&name);
            ensure!(
                known,
                CommandError::invalid(format!("unknown or read-only register {name:?}"))
            );
            ensure!(
                edits
                    .insert(name.into(), number(value).map_err(CommandError::invalid)?)
                    .is_none(),
                CommandError::invalid(format!("duplicate register {name:?}"))
            );
        }
    }
    if let DebugCommand::Gdb { listen } = &args.operation {
        ensure!(
            listen.ip().is_loopback(),
            CommandError::invalid("the debugger listener must use a loopback address")
        );
    }
    if let DebugCommand::Dump {
        address,
        length,
        path,
    } = &args.operation
    {
        ensure!(
            *length > 0 && address.checked_add(*length as u64).is_some(),
            CommandError::invalid("invalid dump range")
        );
        ensure!(
            !path.exists(),
            CommandError::conflict(format!(
                "dump destination {} already exists",
                path.display()
            ))
        );
    }
    let runtime = rt()?;
    runtime.block_on(async {
        let (lab, machine) = split_vm_ref(&args.machine)?;
        let (_, client) = lab_client_for(lab).await?;
        let operation = match args.operation {
            DebugCommand::Status => DebugRequest::Status,
            DebugCommand::Pause => DebugRequest::Pause,
            DebugCommand::Continue => DebugRequest::Continue,
            DebugCommand::Step => {
                let before = request(&client, &machine, DebugRequest::Status).await?;
                let DebugResponse::Status { status } = before else {
                    bail!("invalid debugger status");
                };
                let sequence = status.stop.map_or(0, |stop| stop.sequence);
                request(&client, &machine, DebugRequest::Step { vcpu: args.vcpu }).await?;
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                loop {
                    let response = request(&client, &machine, DebugRequest::Status).await?;
                    if let DebugResponse::Status { status } = &response
                        && !status.stepping
                        && status.state == vmlab_hv_proto::control::RunState::Paused
                        && status
                            .stop
                            .as_ref()
                            .is_some_and(|stop| stop.sequence > sequence)
                    {
                        return if args.json {
                            super::print_json(&serde_json::to_value(response)?)
                        } else {
                            print!("{}", render(&response));
                            Ok(())
                        };
                    }
                    if std::time::Instant::now() >= deadline {
                        request(&client, &machine, DebugRequest::Pause).await?;
                        bail!("single-step did not stop within 5 seconds; VM paused");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            DebugCommand::Detach { leave_paused } => DebugRequest::Detach {
                resume: !leave_paused,
            },
            DebugCommand::Read { address, length } => DebugRequest::ReadMemory {
                space,
                address,
                length,
            },
            DebugCommand::Write { address, data } => DebugRequest::WriteMemory {
                space,
                address,
                data: data.0,
            },
            DebugCommand::Search {
                address,
                length,
                pattern,
            } => DebugRequest::SearchMemory {
                space,
                address,
                length,
                pattern: pattern.0,
            },
            DebugCommand::Break {
                address,
                kind,
                length,
            } => DebugRequest::AddBreakpoint {
                breakpoint: Breakpoint {
                    address,
                    length,
                    kind: kind.into(),
                },
            },
            DebugCommand::Unbreak {
                address,
                kind,
                length,
            } => DebugRequest::RemoveBreakpoint {
                breakpoint: Breakpoint {
                    address,
                    length,
                    kind: kind.into(),
                },
            },
            DebugCommand::Registers { .. } => {
                if !edits.is_empty() {
                    request(
                        &client,
                        &machine,
                        DebugRequest::SetRegisters {
                            vcpu: args.vcpu,
                            values: edits,
                        },
                    )
                    .await?;
                }
                DebugRequest::ReadRegisters { vcpu: args.vcpu }
            }
            DebugCommand::Dump {
                address,
                length,
                path,
            } => {
                let parent = path
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| std::path::Path::new("."));
                let mut output = tempfile::NamedTempFile::new_in(parent)?;
                let mut offset = 0;
                while offset < length {
                    let count = MEMORY_LIMIT.min(length - offset);
                    match request(
                        &client,
                        &machine,
                        DebugRequest::ReadMemory {
                            space,
                            address: address + offset as u64,
                            length: count,
                        },
                    )
                    .await?
                    {
                        DebugResponse::Memory { data } if data.len() == count => {
                            output.write_all(&data)?
                        }
                        _ => bail!("invalid memory response during dump"),
                    }
                    offset += count;
                }
                output.flush()?;
                output
                    .persist_noclobber(&path)
                    .with_context(|| format!("creating dump {}", path.display()))?;
                return if args.json {
                    super::print_json(&serde_json::json!({"path":path,"bytes":length}))
                } else {
                    println!("{} bytes written to {}", length, path.display());
                    Ok(())
                };
            }
            DebugCommand::Gdb { listen } => {
                // Bind before announcing readiness. The listener is never exposed on a public interface.
                let listener = tokio::net::TcpListener::bind(listen).await?;
                let status = request(&client, &machine, DebugRequest::Status).await?;
                ensure!(
                    matches!(status, DebugResponse::Status { .. }),
                    "invalid debugger status"
                );
                eprintln!("debug feature listening on {}", listener.local_addr()?);
                let (stream, _) = tokio::select! {
                    connection = listener.accept() => connection?,
                    signal = tokio::signal::ctrl_c() => { signal?; return Ok(()); }
                };
                let stream = stream.into_std()?;
                let interrupt = stream.try_clone()?;
                let backend = DaemonTarget {
                    client: client.clone(),
                    machine: machine.clone(),
                    runtime: tokio::runtime::Handle::current(),
                };
                let mut bridge = tokio::task::spawn_blocking(move || {
                    vmlab_hv_proto::gdb::serve(stream, backend).map_err(anyhow::Error::msg)
                });
                return tokio::select! {
                    result = &mut bridge => result.context("debugger bridge task")?,
                    signal = tokio::signal::ctrl_c() => {
                        signal?;
                        interrupt.shutdown(std::net::Shutdown::Both)?;
                        let _ = bridge.await.context("interrupted debugger bridge task")?;
                        request(&client, &machine, DebugRequest::Pause).await?;
                        request(&client, &machine, DebugRequest::Detach { resume: false }).await?;
                        eprintln!("debugger stopped; VM paused");
                        Ok(())
                    }
                };
            }
        };
        let response = request(&client, &machine, operation).await?;
        if args.json {
            super::print_json(&serde_json::to_value(response)?)
        } else {
            print!("{}", render(&response));
            Ok(())
        }
    })
}

struct DaemonTarget {
    client: LabClient,
    machine: String,
    runtime: tokio::runtime::Handle,
}

impl DebugTarget for DaemonTarget {
    fn request(&mut self, operation: DebugRequest) -> std::result::Result<DebugResponse, String> {
        self.runtime
            .block_on(request(&self.client, &self.machine, operation))
            .map_err(|error| format!("{error:#}"))
    }
}

fn render(response: &DebugResponse) -> String {
    use std::fmt::Write as _;
    let mut output = String::new();
    match response {
        DebugResponse::Done => output.push_str("ok\n"),
        DebugResponse::Status { status } => {
            let _ = writeln!(
                output,
                "state: {:?}; stepping: {}; vCPUs: {:?}",
                status.state, status.stepping, status.vcpus
            );
            if let Some(stop) = &status.stop {
                let _ = writeln!(
                    output,
                    "stop {}: vCPU {} at {:#x}: {:?}",
                    stop.sequence, stop.vcpu, stop.pc, stop.reason
                );
            }
            for (slot, breakpoint) in status.breakpoints.iter().enumerate() {
                let _ = writeln!(
                    output,
                    "slot {slot}: {:?} {:#x} length {}",
                    breakpoint.kind, breakpoint.address, breakpoint.length
                );
            }
        }
        DebugResponse::Memory { data } => {
            for (row, bytes) in data.chunks(16).enumerate() {
                let _ = writeln!(output, "+{:04x}  {}", row * 16, hex::encode(bytes));
            }
        }
        DebugResponse::Matches {
            addresses,
            truncated,
        } => {
            for address in addresses {
                let _ = writeln!(output, "{address:#x}");
            }
            if *truncated {
                output.push_str("match limit reached; narrow the search\n");
            }
        }
        DebugResponse::Registers { vcpu, registers } => {
            let _ = writeln!(output, "vCPU {vcpu}");
            for (name, value) in GENERAL_REGISTERS.iter().zip(registers.general) {
                let _ = writeln!(output, "{name:<8} {value:#018x}");
            }
            for (name, value) in SEGMENT_REGISTERS.iter().zip(registers.segments) {
                let _ = writeln!(output, "{name:<8} {value:#06x}");
            }
            for (name, value) in &registers.system {
                let _ = writeln!(output, "{name:<8} {value:#018x}");
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_memory_bytes_have_unambiguous_syntax() {
        assert_eq!(number("0Xff").unwrap(), 255);
        assert_eq!(number("16").unwrap(), 16);
        assert!(number("-1").is_err());
        assert_eq!(hex_bytes("0012ff").unwrap().0, [0, 0x12, 0xff]);
        for invalid in ["", "f", "gg"] {
            assert!(hex_bytes(invalid).is_err());
        }
    }
}
