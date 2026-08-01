//! The wire protocol's request vocabulary (ADR-0007).
//!
//! Every command a daemon serves is one variant of one enumeration, carrying
//! that command's argument shape. The command string is still what goes on the
//! wire — this is not a format break — but no surface spells it. The CLI, the
//! REST layer and the console all construct variants, so a misspelled command
//! or a wrong argument shape is a compile error, and the daemon's dispatch is
//! an exhaustive `match` that cannot silently miss one.
//!
//! There are two vocabularies because there are two daemons. [`SupRequest`] is
//! what `vmlabd` serves on the supervisor socket; [`LabRequest`] is what a lab
//! daemon serves on its own. A few names appear in both (`ping`, `status`,
//! `shutdown`) — they are different commands answered by different processes,
//! and keeping them apart is what lets each daemon match exhaustively.

use ipnet::Ipv4Net;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use super::error::CommandError;

/// One argument of one command, as declared in the vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: &'static str,
    /// The Rust type, normalised (`stringify!` spaces removed). Generated
    /// clients and the protocol report map this onto their own type systems.
    pub ty: &'static str,
}

/// One command of one vocabulary: what it is called on the wire, what it
/// takes, and what it is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    /// The Rust variant name, which is also how a surface spells the call.
    pub variant: &'static str,
    /// The serialised command string.
    pub cmd: &'static str,
    pub args: &'static [ArgSpec],
    /// The variant's doc comment, one line per `///`.
    pub doc: &'static str,
}

/// A request vocabulary: an enumeration that knows how to become, and be read
/// back from, the `cmd` + `args` pair on the wire.
pub trait WireRequest: Serialize + DeserializeOwned + Sized {
    /// Every command, in declaration order.
    const COMMANDS: &'static [CommandSpec];

    /// This request's command string.
    fn cmd(&self) -> &'static str;

    /// Legacy argument spellings a daemon still accepts, normalised before
    /// decoding. The default does nothing.
    fn normalise_args(_cmd: &str, args: Value) -> Value {
        args
    }

    /// Split into the pair the wire carries.
    fn to_wire(&self) -> (&'static str, Value) {
        let mut wire = serde_json::to_value(self).expect("a request always serialises");
        let args = wire
            .get_mut("args")
            .map(Value::take)
            .unwrap_or_else(|| json!({}));
        (self.cmd(), args)
    }

    /// Read back what the wire carried.
    ///
    /// An unrecognised command and ill-formed arguments are different
    /// failures, and the caller can tell them apart by code.
    fn from_wire(cmd: &str, args: Value) -> Result<Self, CommandError> {
        if !Self::COMMANDS.iter().any(|spec| spec.cmd == cmd) {
            return Err(CommandError::unknown_command(cmd));
        }
        // A client with nothing to say sends `null`, or omits `args`
        // altogether; both mean "no arguments".
        let args = match args {
            Value::Null => Value::Object(Map::new()),
            other => other,
        };
        serde_json::from_value(json!({"cmd": cmd, "args": Self::normalise_args(cmd, args)}))
            .map_err(|e| CommandError::invalid(format!("{cmd}: {e}")))
    }

    /// The spec for one command string, if it is in this vocabulary.
    fn spec(cmd: &str) -> Option<&'static CommandSpec> {
        Self::COMMANDS.iter().find(|spec| spec.cmd == cmd)
    }
}

/// Declare a request vocabulary: one enumeration, its wire spellings, its
/// argument shapes and the metadata the protocol report reads.
///
/// Each variant is `Name = "wire.command" { field: Type, ... }`. Serde
/// attributes on a field carry through, which is where an argument's default
/// and its legacy aliases live. An optional `=> path::to::fn` names a pre-pass
/// over the raw arguments, for legacy spellings serde alone cannot express.
macro_rules! vocabulary {
    (
        $(#[$enum_meta:meta])*
        $name:ident $(=> $normalise:path)? {
            $(
                $(#[doc = $doc:literal])*
                $variant:ident = $cmd:literal {
                    $(
                        $(#[$field_meta:meta])*
                        $field:ident : $ty:ty
                    ),* $(,)?
                }
            ),* $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
        #[serde(tag = "cmd", content = "args")]
        pub enum $name {
            $(
                $(#[doc = $doc])*
                #[serde(rename = $cmd)]
                $variant {
                    $(
                        $(#[$field_meta])*
                        $field: $ty,
                    )*
                },
            )*
        }

        impl WireRequest for $name {
            const COMMANDS: &'static [CommandSpec] = &[
                $(
                    CommandSpec {
                        variant: stringify!($variant),
                        cmd: $cmd,
                        args: &[
                            $( ArgSpec { name: stringify!($field), ty: stringify!($ty) } ),*
                        ],
                        doc: concat!($($doc, "\n",)*),
                    },
                )*
            ];

            fn cmd(&self) -> &'static str {
                match self {
                    $( $name::$variant { .. } => $cmd, )*
                }
            }

            fn normalise_args(cmd: &str, args: Value) -> Value {
                let _ = cmd;
                $( let args = $normalise(cmd, args); )?
                args
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Argument types that are more than a scalar
// ---------------------------------------------------------------------------

/// A rectangle of a machine's framebuffer, on the wire as `[x, y, w, h]`.
///
/// Negative coordinates clamp to zero — a caller doing arithmetic on a match
/// position should not have to; anything other than four numbers is a bad
/// argument rather than a silently truncated region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Region {
    pub fn as_tuple(self) -> (u32, u32, u32, u32) {
        (self.x, self.y, self.w, self.h)
    }
}

impl Serialize for Region {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        [self.x, self.y, self.w, self.h].serialize(s)
    }
}

impl<'de> Deserialize<'de> for Region {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Region, D::Error> {
        let raw = Vec::<i64>::deserialize(d)?;
        if raw.len() != 4 {
            return Err(serde::de::Error::custom(format!(
                "region needs [x, y, w, h], got {} elements",
                raw.len()
            )));
        }
        let at = |i: usize| raw[i].max(0) as u32;
        Ok(Region {
            x: at(0),
            y: at(1),
            w: at(2),
            h: at(3),
        })
    }
}

/// `Option<Ipv4Net>` on the wire as an optional CIDR string (`ipnet` is built
/// without its serde feature).
mod opt_subnet {
    use super::*;

    pub fn serialize<S: serde::Serializer>(v: &Option<Ipv4Net>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            Some(net) => s.serialize_some(&net.to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: serde::Deserializer<'de>>(
        d: D,
    ) -> Result<Option<Ipv4Net>, D::Error> {
        let raw = Option::<String>::deserialize(d)?;
        raw.map(|s| {
            s.parse()
                .map_err(|_| serde::de::Error::custom(format!("bad subnet `{s}`")))
        })
        .transpose()
    }
}

fn default_button() -> String {
    "left".to_string()
}
fn default_threshold() -> f64 {
    0.9
}
fn default_exec_timeout() -> u64 {
    120
}
fn default_osinfo_timeout() -> u64 {
    30
}
fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}
fn default_log_lines() -> usize {
    100
}

/// The spellings of "which machine". `machine`/`machines` are current;
/// `vm`, `container` and `vms` are what the wire carried before VMs and
/// containers collapsed into one command set (PRD §18).
const MACHINE_SPELLINGS: [&[&str]; 2] = [&["machine", "vm", "container"], &["machines", "vms"]];

/// Serde accepts any one spelling through field aliases, but two at once would
/// be a duplicate field. A client sending both keeps working: the spelling the
/// command actually declares wins, as it always has.
fn drop_shadowed_machine_aliases(cmd: &str, mut args: Value) -> Value {
    let Some(spec) = LabRequest::spec(cmd) else {
        return args;
    };
    let Some(obj) = args.as_object_mut() else {
        return args;
    };
    for group in MACHINE_SPELLINGS {
        let Some(canonical) = spec
            .args
            .iter()
            .map(|arg| arg.name)
            .find(|name| group.contains(name))
        else {
            continue;
        };
        if obj.contains_key(canonical) {
            for shadowed in group.iter().filter(|s| **s != canonical) {
                obj.remove(*shadowed);
            }
        }
    }
    args
}

// ---------------------------------------------------------------------------
// The lab daemon's vocabulary
// ---------------------------------------------------------------------------

vocabulary! {
    /// What a lab daemon serves on its lab control socket.
    ///
    /// One command set for VMs and containers alike (PRD §7, §18): where a
    /// machine genuinely cannot serve a command it says so through its
    /// capabilities, not through its kind.
    LabRequest => drop_shadowed_machine_aliases {
        /// Liveness check; answers `"pong"`.
        Ping = "ping" {},
        /// The whole lab's runtime status: machines, segments, readiness.
        Status = "status" {},
        /// The DNS zones the lab's segments serve.
        DnsTable = "dns.table" {},

        /// Bring the lab up, or just the named machines (empty = all).
        /// Streams provisioning output.
        Up = "up" {
            #[serde(default, alias = "vms")] machines: Vec<String>,
        },
        /// Download every pending template and image without starting
        /// anything, over the code path `up` runs first.
        Pull = "pull" {
            #[serde(default, alias = "vms")] machines: Vec<String>,
        },
        /// Abort one machine's running download; whatever waits on it fails
        /// with "download cancelled".
        PullCancel = "pull.cancel" {
            machine: String,
        },
        /// Run an ad-hoc wscript against the lab (PRD §12), streaming output.
        Run = "run" {
            script: String,
        },
        /// Stop the lab, or just the named machines (empty = all).
        Down = "down" {
            #[serde(default, alias = "vms")] machines: Vec<String>,
            #[serde(default)] force: bool,
        },
        /// Stop the lab and delete everything it materialised.
        Destroy = "destroy" {},

        /// Start one machine, pulling its template or image first.
        MachineStart = "machine.start" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// Stop one machine; `force` kills instead of the graceful ladder.
        MachineStop = "machine.stop" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] force: bool,
        },
        /// Stop one machine, wait for it to settle, and boot it again.
        MachineRestart = "machine.restart" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] force: bool,
        },
        /// Stop one machine and delete everything it materialised.
        MachineDestroy = "machine.destroy" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// What this machine can do beyond the universal commands, probed
        /// live: a display, a console log, in-place reboot, and whichever
        /// features its agent negotiated.
        MachineCapabilities = "machine.capabilities" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// The machine's guest IP, optionally for one NIC index.
        MachineIp = "machine.ip" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] nic: Option<usize>,
        },

        /// Write a PNG of the machine's framebuffer to a host path.
        MachineScreenshot = "machine.screenshot" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            path: String,
        },
        /// Send a key chord to the machine's display.
        MachineSendKeys = "machine.sendkeys" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            keys: String,
        },
        /// Move the pointer to an absolute framebuffer position.
        MachineMouseMove = "machine.mouse_move" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            x: i64,
            y: i64,
        },
        /// Click a mouse button, optionally moving there first (both `x` and
        /// `y`, or neither).
        MachineMouseClick = "machine.mouse_click" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default = "default_button")] button: String,
            #[serde(default)] x: Option<i64>,
            #[serde(default)] y: Option<i64>,
        },
        /// Press at one point, drag, release at another.
        MachineMouseDrag = "machine.mouse_drag" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            x1: i64,
            y1: i64,
            x2: i64,
            y2: i64,
        },
        /// Read text off the machine's display, whole screen or one region.
        MachineOcr = "machine.ocr" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] region: Option<Region>,
        },
        /// Find a template image on the machine's display; null when no match
        /// scores above `threshold`.
        MachineFindImage = "machine.find_image" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            image: String,
            #[serde(default = "default_threshold")] threshold: f64,
            #[serde(default)] region: Option<Region>,
        },

        /// Run a command in the guest through the agent and collect its
        /// output.
        MachineExec = "machine.exec" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            cmd: String,
            #[serde(default)] args: Vec<String>,
            /// Seconds before the guest command is given up on.
            #[serde(default = "default_exec_timeout")] timeout: u64,
        },
        /// What the guest OS says it is.
        MachineOsInfo = "machine.osinfo" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            /// Seconds to wait for the guest to answer.
            #[serde(default = "default_osinfo_timeout")] timeout: u64,
        },
        /// Open an interactive terminal, re-exposed as a raw-byte unix socket
        /// the caller connects to. Every open gets its own shell.
        MachineTtyOpen = "machine.tty_open" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default = "default_cols")] cols: u16,
            #[serde(default = "default_rows")] rows: u16,
        },
        /// Resize an open terminal session.
        MachineTtyResize = "machine.tty_resize" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            session: u32,
            #[serde(default = "default_cols")] cols: u16,
            #[serde(default = "default_rows")] rows: u16,
        },
        /// Copy a file into the guest: either `from`, a host path the daemon
        /// can see, or `data`, base64 for a caller that holds bytes.
        MachinePushFile = "machine.push_file" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            to: String,
            #[serde(default)] from: Option<String>,
            #[serde(default)] data: Option<String>,
            #[serde(default)] mode: Option<u32>,
        },
        /// Copy a file out of the guest to a host path.
        MachinePullFile = "machine.pull_file" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            from: String,
            to: String,
        },
        /// Follow a guest file (`tail -F` semantics), streamed as chunks
        /// until the caller hangs up or the machine stops.
        MachineTail = "machine.tail" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            path: String,
        },
        /// Follow the Windows event log, streamed as chunks.
        MachineEventLog = "machine.eventlog" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] filter: Option<String>,
        },
        /// Latest guest metrics; subscribes the sampler on first use.
        MachineStats = "machine.stats" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// Read the guest clipboard.
        MachineClipboardGet = "machine.clipboard_get" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// Write the guest clipboard.
        MachineClipboardSet = "machine.clipboard_set" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            text: String,
        },
        /// The machine's console log: the last `lines`, then, with `follow`,
        /// streamed growth until the machine stops.
        MachineLogs = "machine.logs" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default = "default_log_lines")] lines: usize,
            #[serde(default)] follow: bool,
        },

        /// Ensure a loopback forward for a declared web page and return the
        /// address to dial, plus the page's auth spec (host-side only).
        WebForward = "web.forward" {
            machine: String,
            page: String,
        },

        /// Every playbook assignment in the lab, one row per (machine, block).
        PlaybookList = "playbook.list" {},
        /// Dry-run a playbook against one machine, streaming its output.
        PlaybookCheck = "playbook.check" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] playbook: Option<String>,
            #[serde(default)] play: Option<String>,
        },
        /// Apply a playbook to one machine, streaming its output.
        PlaybookApply = "playbook.apply" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] playbook: Option<String>,
            #[serde(default)] play: Option<String>,
        },
        /// Which playbook runs are in flight.
        PlaybookOpStatus = "playbook.op_status" {},

        /// Snapshot one machine, or the whole lab when `machine` is omitted.
        SnapshotTake = "snapshot.take" {
            name: String,
            #[serde(default, alias = "vm", alias = "container")] machine: Option<String>,
        },
        /// Restore one machine, or every machine when `machine` is omitted.
        SnapshotRestore = "snapshot.restore" {
            name: String,
            #[serde(default, alias = "vm", alias = "container")] machine: Option<String>,
        },
        /// Delete one machine's snapshot.
        SnapshotDelete = "snapshot.delete" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            name: String,
        },
        /// One machine's snapshots.
        SnapshotList = "snapshot.list" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },

        /// Tear the lab daemon down; the reply is sent before it exits.
        Shutdown = "shutdown" {},
    }
}

// ---------------------------------------------------------------------------
// The supervisor's vocabulary
// ---------------------------------------------------------------------------

vocabulary! {
    /// What `vmlabd` serves on the supervisor socket: the lab registry, global
    /// segments, and the template operations that outlive any one lab daemon.
    SupRequest {
        /// Liveness check; answers `"pong"`.
        Ping = "ping" {},
        /// The supervisor's own build version.
        Version = "version" {},
        /// Which network fast-path tier this host selected (PRD §9.1), and
        /// why the skipped tiers were unavailable.
        FastPath = "fastpath" {},
        /// Every lab in the registry.
        Status = "status" {},

        /// Spawn (or find) a lab's daemon; answers with its socket path.
        LabEnsure = "lab.ensure" {
            name: String,
            root: std::path::PathBuf,
        },
        /// Stop a lab's daemon, after `down` or `destroy`.
        LabRelease = "lab.release" {
            name: String,
        },
        /// Restart a lab's daemon so it re-reads its config; answers with the
        /// new socket path.
        LabRestart = "lab.restart" {
            name: String,
            root: std::path::PathBuf,
        },

        /// Join a global segment (PRD §9.2), creating it on first use;
        /// answers with the trunk socket to bridge to.
        GlobalAttach = "global.attach" {
            name: String,
            #[serde(default, with = "opt_subnet")] subnet: Option<Ipv4Net>,
            #[serde(default)] peer: Option<String>,
        },
        /// Leave a global segment.
        GlobalDetach = "global.detach" {
            name: String,
        },
        /// Every global segment this host knows.
        GlobalList = "global.list" {},

        /// The templates a lab declares, with their store and build state.
        TemplateList = "template.list" {
            lab: String,
            root: std::path::PathBuf,
        },
        /// What the registry holds for one declared template.
        TemplateRemote = "template.remote" {
            lab: String,
            root: std::path::PathBuf,
            template: String,
            #[serde(default)] arch: Option<String>,
        },
        /// Start building one declared template.
        TemplateBuild = "template.build" {
            lab: String,
            root: std::path::PathBuf,
            template: String,
            #[serde(default)] arch: Option<String>,
        },
        /// Abort a running build.
        TemplateStopBuild = "template.stop_build" {
            lab: String,
            arch: String,
            template: String,
        },
        /// Start pushing one built template to its registry.
        TemplatePush = "template.push" {
            lab: String,
            root: std::path::PathBuf,
            template: String,
            #[serde(default)] arch: Option<String>,
            #[serde(default)] version: Option<String>,
        },
        /// Which template builds and pushes are in flight for one lab.
        TemplateOpStatus = "template.op_status" {
            lab: String,
        },
        /// The socket serving a running build's console, for the web viewer.
        TemplateConsolePath = "template.console_path" {
            lab: String,
            arch: String,
            template: String,
        },

        /// Tear the supervisor down; the reply is sent before it exits.
        Shutdown = "shutdown" {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_through_the_wire_pair() {
        let req = LabRequest::MachineStop {
            machine: "dc01".into(),
            force: true,
        };
        let (cmd, args) = req.to_wire();
        assert_eq!(cmd, "machine.stop");
        assert_eq!(args["machine"], "dc01");
        assert_eq!(args["force"], true);
        assert_eq!(LabRequest::from_wire(cmd, args).unwrap(), req);
    }

    #[test]
    fn an_argument_less_request_accepts_null_args() {
        let req = LabRequest::from_wire("status", Value::Null).unwrap();
        assert_eq!(req, LabRequest::Status {});
    }

    #[test]
    fn an_unknown_command_is_not_a_bad_argument() {
        use crate::proto::ErrorCode;
        let e = LabRequest::from_wire("machine.teleport", json!({})).unwrap_err();
        assert_eq!(e.code, ErrorCode::UnknownCommand);
        let e = LabRequest::from_wire("machine.stop", json!({})).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }

    /// The wire says `machine`, but a client that predates the collapse says
    /// `vm` or `container`. Both keep working.
    #[test]
    fn machine_arguments_accept_the_old_names() {
        for spelling in ["machine", "vm", "container"] {
            let req = LabRequest::from_wire("machine.start", json!({spelling: "dc01"})).unwrap();
            assert_eq!(
                req,
                LabRequest::MachineStart {
                    machine: "dc01".into()
                }
            );
        }
        for spelling in ["machines", "vms"] {
            let req = LabRequest::from_wire("up", json!({spelling: ["dc01"]})).unwrap();
            assert_eq!(
                req,
                LabRequest::Up {
                    machines: vec!["dc01".into()]
                }
            );
        }
    }

    /// Sending two spellings at once used to be harmless — `machine` won —
    /// and still is, rather than failing as a duplicate field.
    #[test]
    fn the_declared_spelling_wins_when_a_client_sends_two() {
        let req =
            LabRequest::from_wire("machine.start", json!({"machine": "a", "vm": "b"})).unwrap();
        assert_eq!(
            req,
            LabRequest::MachineStart {
                machine: "a".into()
            }
        );
        let req = LabRequest::from_wire("up", json!({"machines": ["a"], "vms": ["b"]})).unwrap();
        assert_eq!(
            req,
            LabRequest::Up {
                machines: vec!["a".into()]
            }
        );
    }

    #[test]
    fn omitted_arguments_take_the_documented_default() {
        let req = LabRequest::from_wire("machine.logs", json!({"machine": "dc01"})).unwrap();
        assert_eq!(
            req,
            LabRequest::MachineLogs {
                machine: "dc01".into(),
                lines: 100,
                follow: false,
            }
        );
        let req = LabRequest::from_wire("machine.mouse_click", json!({"machine": "dc01"})).unwrap();
        let LabRequest::MachineMouseClick { button, x, y, .. } = req else {
            panic!("wrong variant");
        };
        assert_eq!((button.as_str(), x, y), ("left", None, None));
    }

    #[test]
    fn a_region_is_four_numbers_and_clamps_negatives() {
        let req = LabRequest::from_wire(
            "machine.ocr",
            json!({"machine": "dc01", "region": [-5, 2, 3, 4]}),
        )
        .unwrap();
        let LabRequest::MachineOcr { region, .. } = req else {
            panic!("wrong variant");
        };
        assert_eq!(region.map(Region::as_tuple), Some((0, 2, 3, 4)));

        assert!(
            LabRequest::from_wire("machine.ocr", json!({"machine": "d", "region": [1, 2, 3]}))
                .is_err()
        );
        assert!(
            LabRequest::from_wire("machine.ocr", json!({"machine": "d", "region": "nope"}))
                .is_err()
        );
    }

    #[test]
    fn a_subnet_argument_is_a_parsed_cidr() {
        let req = SupRequest::from_wire(
            "global.attach",
            json!({"name": "corp", "subnet": "10.9.0.0/24"}),
        )
        .unwrap();
        let SupRequest::GlobalAttach { subnet, .. } = req else {
            panic!("wrong variant");
        };
        assert_eq!(
            subnet.map(|s| s.to_string()).as_deref(),
            Some("10.9.0.0/24")
        );
        assert!(
            SupRequest::from_wire("global.attach", json!({"name": "c", "subnet": "nope"})).is_err()
        );
    }

    /// Both vocabularies are enumerable, and every command in them is
    /// uniquely spelled — the property the protocol report leans on.
    #[test]
    fn commands_are_enumerable_and_unique() {
        for commands in [LabRequest::COMMANDS, SupRequest::COMMANDS] {
            assert!(!commands.is_empty());
            let mut seen = std::collections::HashSet::new();
            for spec in commands {
                assert!(seen.insert(spec.cmd), "duplicate command `{}`", spec.cmd);
                assert!(!spec.doc.is_empty(), "`{}` is undocumented", spec.cmd);
            }
        }
    }
}
