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

/// What a command reachable from one surface and no other has to say for
/// itself.
///
/// A one-way command is not automatically a gap — several only mean anything
/// from one place — but nothing distinguishes a decision from an oversight
/// unless the decision is written down. This is where it is written, and the
/// coverage report renders it beside the command. Every one-way command
/// carries one of these: `report::every_one_way_command_records_why` fails
/// when one carries neither, so a command with a single caller is a decision
/// made at declaration time rather than one a later audit discovers.
///
/// The two kinds are one enumeration rather than two independent fields
/// because a command is one or the other, never both and never a blend, and
/// three of the four combinations two fields allow would be nonsense.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneWay {
    /// A decision: this command belongs on this surface and no other, for
    /// this reason.
    Deliberate {
        /// The surface the command is reachable from, spelled as the coverage
        /// report spells it — one of `report::SURFACES`, which a test checks.
        surface: &'static str,
        /// Why that is the only surface it belongs on.
        why: &'static str,
    },
    /// No decision: the command reaches one surface because nobody wrote the
    /// other half, and `issue` tracks the question.
    ///
    /// This asserts nothing about whether the asymmetry is right, which is
    /// exactly why it is not a [`Deliberate`](Self::Deliberate) with a reason
    /// reading "unknown" — the report separates the two lists, and a reason
    /// field used loosely would collapse the distinction the whole exercise
    /// draws.
    ///
    /// The annotation self-cleans in one direction only. Close the *gap* by
    /// giving the command a second caller and
    /// `report::an_annotated_command_is_still_one_way` fails, so the report
    /// cannot advertise a gap already closed. Close the *issue* while the gap
    /// stays open and nothing notices: no test can reach GitHub, and building
    /// issue-state checking for this is deliberately not worth it.
    Gap {
        /// As [`Deliberate::surface`](Self::Deliberate).
        surface: &'static str,
        /// The issue tracking whether this asymmetry should close.
        issue: u32,
    },
}

impl OneWay {
    /// The surface the command is reachable from, whichever kind this is.
    /// Both checks the report makes are about the surface, and neither cares
    /// which kind made the claim.
    pub fn surface(&self) -> &'static str {
        match self {
            Self::Deliberate { surface, .. } | Self::Gap { surface, .. } => surface,
        }
    }
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
    /// What this command says about being reachable from one surface only:
    /// the reason, or the issue tracking the gap. `None` is legal in the type
    /// and illegal in the repo — it means a command reachable from more than
    /// one surface, and a test rejects it on one that is not.
    pub one_way: Option<OneWay>,
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

/// An optional `#[one_way(..)]` or `#[one_way_gap(..)]` as an
/// `Option<OneWay>`.
///
/// `vocabulary!` expands both of the variant's optional repetitions straight
/// into this call, so what comes out picks the arm: nothing for a variant that
/// carried neither annotation, and a `deliberate` or `gap` tag with its
/// literals for one that carried either. The alternative — a `let mut` fixed
/// up by the repetitions — also evaluates in a `const`, but it puts a mutable
/// binding in the middle of a table of constants to express something the arms
/// below say outright, and it would quietly let the second annotation win
/// instead of rejecting the pair.
macro_rules! one_way {
    () => {
        None
    };
    (deliberate $surface:literal, $why:literal) => {
        Some(OneWay::Deliberate {
            surface: $surface,
            why: $why,
        })
    };
    (gap $surface:literal, $issue:literal) => {
        Some(OneWay::Gap {
            surface: $surface,
            issue: $issue,
        })
    };
    // Both annotations at once. Saying a command is deliberate *and* an open
    // gap is a contradiction, not an override, so name it rather than leaving
    // the reader with "no rules expected the token `gap`".
    (deliberate $surface:literal, $why:literal gap $gap_surface:literal, $issue:literal) => {
        compile_error!(
            "a one-way command is either deliberate or a tracked gap, never both: \
             delete `#[one_way]` if nobody has decided, or `#[one_way_gap]` if somebody has"
        )
    };
}

/// Declare a request vocabulary: one enumeration, its wire spellings, its
/// argument shapes and the metadata the protocol report reads.
///
/// Each variant is `Name = "wire.command" { field: Type, ... }`. Serde
/// attributes on a field carry through, which is where an argument's default
/// and its legacy aliases live. An optional `=> path::to::fn` names a pre-pass
/// over the raw arguments, for legacy spellings serde alone cannot express.
///
/// A variant reachable from one surface only carries, directly after its doc
/// comments, either `#[one_way("surface", "why")]` — deliberately there and
/// nowhere else — or `#[one_way_gap("surface", 38)]` — nobody wrote the other
/// half, and issue 38 tracks it (see [`OneWay`]). Both are annotations, not
/// real attributes: they never reach the generated enumeration, only the
/// [`CommandSpec`]. Carrying both is a compile error, and carrying neither
/// fails a test.
macro_rules! vocabulary {
    (
        $(#[$enum_meta:meta])*
        $name:ident $(=> $normalise:path)? {
            $(
                $(#[doc = $doc:literal])*
                $(#[one_way($surface:literal, $why:literal)])?
                $(#[one_way_gap($gap_surface:literal, $issue:literal)])?
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
                        one_way: one_way!(
                            $( deliberate $surface, $why )?
                            $( gap $gap_surface, $issue )?
                        ),
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
fn default_keep() -> usize {
    1
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
        #[one_way(
            "cli",
            "A scratch script is a shell verb: it comes from a file the caller \
             already has and streams its output back to the terminal that ran \
             it. What the console runs is declared playbooks."
        )]
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
        #[one_way(
            "cli",
            "A scripting shortcut over data the console already holds: the lab \
             status projection carries every machine's address, so the console \
             reads it there rather than asking a second time."
        )]
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
        #[one_way(
            "cli",
            "The console drives a machine through a live VNC canvas, where a \
             human moves the pointer themselves. Scripted pointer input is for \
             callers that have no canvas."
        )]
        MachineMouseMove = "machine.mouse_move" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            x: i64,
            y: i64,
        },
        /// Click a mouse button, optionally moving there first (both `x` and
        /// `y`, or neither).
        #[one_way(
            "cli",
            "Scripted input, for the same reason as `machine.mouse_move`: a \
             console user clicks the VNC canvas directly."
        )]
        MachineMouseClick = "machine.mouse_click" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default = "default_button")] button: String,
            #[serde(default)] x: Option<i64>,
            #[serde(default)] y: Option<i64>,
        },
        /// Press at one point, drag, release at another.
        #[one_way(
            "cli",
            "Scripted input, for the same reason as `machine.mouse_move`: a \
             console user drags on the VNC canvas directly."
        )]
        MachineMouseDrag = "machine.mouse_drag" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            x1: i64,
            y1: i64,
            x2: i64,
            y2: i64,
        },
        /// Read text off the machine's display, whole screen or one region.
        #[one_way(
            "cli",
            "Reading text off the framebuffer is a script's substitute for \
             looking at it. The console shows the framebuffer to somebody who \
             can already read it."
        )]
        MachineOcr = "machine.ocr" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] region: Option<Region>,
        },
        /// Find a template image on the machine's display; null when no match
        /// scores above `threshold`.
        #[one_way(
            "cli",
            "How a script finds a control it cannot see. A console user clicks \
             the one they can, on the VNC canvas."
        )]
        MachineFindImage = "machine.find_image" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            image: String,
            #[serde(default = "default_threshold")] threshold: f64,
            #[serde(default)] region: Option<Region>,
        },

        /// Run a command in the guest through the agent and collect its
        /// output.
        #[one_way(
            "cli",
            "The scripted counterpart to the console's interactive terminals: \
             one command, its output collected, an exit code to branch on. The \
             console opens a shell and lets a human type instead."
        )]
        MachineExec = "machine.exec" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            cmd: String,
            #[serde(default)] args: Vec<String>,
            /// Seconds before the guest command is given up on.
            #[serde(default = "default_exec_timeout")] timeout: u64,
            /// Which of the machine's declared logins to run as — its label,
            /// or the account name as an alias (PRD §19.2). Absent is the
            /// machine's default login; `SYSTEM`/`root` is the agent
            /// identity, which is what a machine declaring no login gets.
            #[serde(default)] user: Option<String>,
            /// The secret for an account the lab file does not declare, or
            /// one whose declared password has been rotated.
            #[serde(default)] password: Option<String>,
        },
        /// What the guest OS says it is.
        #[one_way(
            "cli",
            "A live guest probe with a timeout, so it does not belong on a \
             panel that refreshes; the status projection already carries what \
             the console shows about a machine. Its CLI help calls it fit for \
             scripting, which is what it is."
        )]
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
            /// Which of the machine's declared logins the shell runs as —
            /// see [`LabRequest::MachineExec`].
            #[serde(default)] user: Option<String>,
            #[serde(default)] password: Option<String>,
        },
        /// Open an SSH facade connection for this machine, re-exposed as a
        /// unix socket the caller pipes stdin/stdout onto (PRD §19.3). One
        /// socket per connection, unlinked when it ends.
        #[one_way(
            "cli",
            "The endpoint is a stdio `ProxyCommand` and nothing else \
             (ADR-0012): the socket exists to be handed to an `ssh` process's \
             stdin and stdout, and a browser has nothing to connect a stdio \
             pipe to. Nothing listens on the host, so there is also no \
             address a console could offer."
        )]
        MachineSshOpen = "machine.ssh_open" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// Push the host's shipped vmlab-agent into a running machine and
        /// mark it **diverged** (PRD §19.4). Never fires by itself: an
        /// automatic refresh would make the template's sealed
        /// `agent_version` a lie.
        #[one_way(
            "cli",
            "A deliberate, machine-changing act with a rebuild as its \
             alternative — the console offering it as a button would invite \
             exactly the reflex the verb exists to keep manual, and its \
             audience is whoever is iterating on the agent itself, at a \
             terminal."
        )]
        MachineRepairAgent = "machine.repair_agent" {
            #[serde(alias = "vm", alias = "container")] machine: String,
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
        /// Copy a file out of the guest: to `to`, a host path the daemon can
        /// write, or — with `to` omitted — back inline as base64, for a caller
        /// that wants the bytes rather than a file on the daemon's host.
        MachinePullFile = "machine.pull_file" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            from: String,
            #[serde(default)] to: Option<String>,
        },
        /// Follow a guest file (`tail -F` semantics), streamed as chunks
        /// until the caller hangs up or the machine stops.
        #[one_way(
            "cli",
            "An open-ended stream of an arbitrary guest path, which is what a \
             terminal is for. The console follows a machine's console log \
             through `machine.logs`."
        )]
        MachineTail = "machine.tail" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            path: String,
        },
        /// Follow the Windows event log, streamed as chunks.
        #[one_way(
            "cli",
            "A stream into a terminal, for the same reason as `machine.tail`."
        )]
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
        #[one_way(
            "web",
            "A loopback forward for a guest's web page exists to be dialled by \
             a browser, and the console is the only surface with one."
        )]
        WebForward = "web.forward" {
            machine: String,
            page: String,
        },

        /// Every playbook assignment in the lab, one row per (machine, block).
        #[one_way(
            "cli",
            "One flat table is the shape a shell wants. The console builds its \
             playbook list from the lab's declarations directly and asks the \
             daemon only which runs are in flight."
        )]
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
        #[one_way(
            "web",
            "A poller's question. A CLI `check` or `apply` streams its own run \
             and holds the terminal until it ends, so it never has to ask what \
             is happening."
        )]
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

        /// Run a workspace sync pass now and answer with what it decided
        /// (PRD §19.6). What `vmlab dev sync flush` and `status --wait` are.
        #[one_way(
            "cli",
            "The console already has the answer: a syncer's report is part of \
             the machine's status projection, which the console polls. What \
             this adds is *waiting for a pass*, which is a terminal's idiom — \
             a page that blocks for up to two minutes on a guest that has \
             stopped answering is a page nobody wants."
        )]
        WorkspaceFlush = "workspace.flush" {
            #[serde(alias = "vm", alias = "container")] machine: String,
        },
        /// Say which side wins at halted paths, and carry it out (§19.6).
        ///
        /// `paths` empty with `all` set takes the whole batch — the 30 000-file
        /// case is one `.vmlabignore` edit away and nobody is going to type it.
        #[one_way(
            "cli",
            "§19.6 states it outright: the console reads the halt and does not \
             act on it. Resolution is a per-path judgement about a developer's \
             own working copy, made beside the two directories in question, and \
             the copy that loses is not recoverable from vmlab — which is a \
             decision for a terminal in the lab directory, not a button."
        )]
        WorkspaceResolve = "workspace.resolve" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            #[serde(default)] paths: Vec<String>,
            #[serde(default)] all: bool,
            /// `host` or `guest`.
            winner: String,
        },
        /// Bring the guest's copy of one workspace path to the host (§19.6).
        ///
        /// The host copy is a plain directory on the developer's own
        /// workstation, so only the *guest* side is behind the seam — which is
        /// the whole reason this verb exists rather than "attach and look".
        #[one_way(
            "cli",
            "It answers with the guest's bytes for a host-side `diff`, whose \
             audience is a terminal. A console showing two versions of a source \
             file is a diff viewer, which is the editor's job — and the editor \
             is already attached into the guest."
        )]
        WorkspaceDiff = "workspace.diff" {
            #[serde(alias = "vm", alias = "container")] machine: String,
            /// Workspace-relative paths; empty means every halted path.
            #[serde(default)] paths: Vec<String>,
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
        #[one_way(
            "cli",
            "What `vmlab daemon status` prints: which build of the supervisor \
             is running on this host, asked by whoever is standing in front of \
             it."
        )]
        Version = "version" {},
        /// Which network fast-path tier this host selected (PRD §9.1), and
        /// why the skipped tiers were unavailable.
        FastPath = "fastpath" {},
        /// Every lab in the registry.
        Status = "status" {},

        /// Spawn (or find) a lab's daemon; answers with its socket path.
        #[one_way(
            "cli",
            "Spawning-or-finding a lab daemon belongs in one place, and that \
             place is the helper in `src/cli/daemon.rs` — the web layer calls \
             it rather than asking the supervisor itself. One call site is the \
             decision; the scan reports it as the CLI because that is where \
             the helper lives."
        )]
        LabEnsure = "lab.ensure" {
            name: String,
            root: std::path::PathBuf,
        },
        /// Stop a lab's daemon, after `down` or `destroy`.
        #[one_way(
            "cli",
            "The other half of `lab.ensure`, and a shell's alone: a command \
             finishes and gives the daemon back. The console does not finish, \
             and leaves it up for the next request."
        )]
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
        #[one_way(
            "daemon",
            "Daemon-internal: a lab daemon joins a global segment because a \
             lab declared one, so there is nothing for a person to ask for."
        )]
        GlobalAttach = "global.attach" {
            name: String,
            #[serde(default, with = "opt_subnet")] subnet: Option<Ipv4Net>,
            #[serde(default)] peer: Option<String>,
        },
        /// Leave a global segment.
        #[one_way(
            "daemon",
            "The other half of `global.attach`, and daemon-internal for the \
             same reason."
        )]
        GlobalDetach = "global.detach" {
            name: String,
        },
        /// Every global segment this host knows.
        #[one_way(
            "daemon",
            "A lab daemon reads it to fold each segment's peer state into the \
             lab status projection, which is how both other surfaces already \
             see it."
        )]
        GlobalList = "global.list" {},

        /// The templates a file declares, with their store and build state.
        TemplateList = "template.list" {
            lab: String,
            root: std::path::PathBuf,
            /// The file declaring them; `root`'s `vmlab.wcl` when omitted.
            #[serde(default)] file: Option<std::path::PathBuf>,
        },
        /// What the registry holds for one declared template.
        #[one_way(
            "web",
            "Which versions a template's own registry publishes, so the \
             console can offer them beside the local ones. A shell asks a \
             namespace what is in it (`registry.search`) or the store what it \
             holds (`store.list --remote`); neither is a lab declaration's \
             view of its own registry."
        )]
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
            /// Pin this exact version instead of auto-incrementing.
            #[serde(default)] version: Option<String>,
            /// The file declaring it; `root`'s `vmlab.wcl` when omitted.
            #[serde(default)] file: Option<std::path::PathBuf>,
        },
        /// Abort a running build.
        TemplateStopBuild = "template.stop_build" {
            lab: String,
            arch: String,
            template: String,
        },
        /// Start pushing one built template to its registry.
        #[one_way(
            "web",
            "The console pushes a template a lab declares to the registry that \
             declaration names. A shell pushes a store reference wherever it \
             is told, and annotates the package with the git origin of the \
             directory it was run in — neither of which a lab declaration has, \
             so `store.push` carries them."
        )]
        TemplatePush = "template.push" {
            lab: String,
            root: std::path::PathBuf,
            template: String,
            #[serde(default)] arch: Option<String>,
            #[serde(default)] version: Option<String>,
        },
        /// Which template builds and pushes are in flight for one lab.
        #[one_way(
            "web",
            "A poller's question. A CLI build or push follows its own \
             operation's events and holds the terminal until it ends, so it \
             never has to ask what is happening."
        )]
        TemplateOpStatus = "template.op_status" {
            lab: String,
        },
        /// The socket serving a running build's console, for the web viewer.
        #[one_way(
            "web",
            "A raw VNC socket exists to be bridged into a browser canvas, and \
             the console is the only surface with one."
        )]
        TemplateConsolePath = "template.console_path" {
            lab: String,
            arch: String,
            template: String,
        },

        /// Every template in the store, with its size and, on request,
        /// whether that exact version is published.
        #[one_way(
            "cli",
            "The store is host-wide; every template command the console has \
             is scoped to the lab it has open, and it has no view of the \
             store as a whole to hang this on. Giving it one is a separate \
             decision from putting the operations on the protocol, which is \
             what this namespace does."
        )]
        StoreList = "store.list" {
            /// Also ask each template's registry whether its exact version and
            /// architecture is published.
            #[serde(default)] remote: bool,
        },
        /// Remove one exact store version `<arch>/<name>@<version>`.
        #[one_way("cli", "Store management, for the reason on `store.list`.")]
        StoreRemove = "store.remove" {
            reference: String,
            /// Remove even when the build still backs a clone.
            #[serde(default)] force: bool,
        },
        /// Plan a prune of superseded builds, and carry it out when `apply`.
        #[one_way("cli", "Store management, for the reason on `store.list`.")]
        StorePrune = "store.prune" {
            /// `<arch>/<name>`, `<arch>/`, or a bare name; every family when
            /// omitted.
            #[serde(default)] filter: Option<String>,
            /// Most-recent builds to keep per template.
            #[serde(default = "default_keep")] keep: usize,
            /// Actually remove; otherwise the answer is the plan alone.
            #[serde(default)] apply: bool,
            /// Also remove builds that still back a clone.
            #[serde(default)] force: bool,
        },
        /// Write one store version to a portable archive.
        #[one_way("cli", "Store management, for the reason on `store.list`.")]
        StoreExport = "store.export" {
            reference: String,
            out: std::path::PathBuf,
        },
        /// Read a template back out of an archive.
        #[one_way("cli", "Store management, for the reason on `store.list`.")]
        StoreImport = "store.import" {
            archive: std::path::PathBuf,
            #[serde(default)] overwrite: bool,
        },
        /// Download a published template into the store.
        #[one_way("cli", "Store management, for the reason on `store.list`.")]
        StorePull = "store.pull" {
            target: String,
            #[serde(default)] arch: Option<String>,
            #[serde(default)] overwrite: bool,
        },
        /// Start uploading one store version to an OCI registry.
        #[one_way("cli", "Store management, for the reason on `store.list`.")]
        StorePush = "store.push" {
            reference: String,
            /// Registry repo; the template's own `registry` field when
            /// omitted.
            #[serde(default)] target: Option<String>,
            /// Source repository URL to link the package to.
            #[serde(default)] source: Option<String>,
            /// Move `latest-prerelease` rather than `latest`.
            #[serde(default)] prerelease: bool,
            /// Lab to file the operation under, so a console watching that lab
            /// sees it. Empty files it under the store itself.
            #[serde(default)] lab: String,
        },
        /// Abort a running store push.
        #[one_way("cli", "The other half of `store.push`.")]
        StoreStopPush = "store.stop_push" {
            #[serde(default)] lab: String,
            arch: String,
            template: String,
        },

        /// Search one OCI namespace, or every configured one, for published
        /// templates or container images.
        #[one_way(
            "cli",
            "The console searches a namespace through its own REST endpoint, \
             which runs in the web process rather than over this socket — \
             `GET /api/catalog/oci`. Routing it here is #37's business, not \
             this command's."
        )]
        RegistrySearch = "registry.search" {
            #[serde(default)] query: Option<String>,
            /// The namespace to search; every configured one when omitted.
            #[serde(default)] namespace: Option<String>,
            #[serde(default)] arch: Option<String>,
            /// Search container images rather than VM templates.
            #[serde(default)] containers: bool,
        },
        /// Store credentials for an OCI registry host.
        #[one_way(
            "cli",
            "The console has its own login endpoint in the web process \
             (`POST /api/registries/login`), for the same reason as \
             `registry.search`."
        )]
        RegistryLogin = "registry.login" {
            registry: String,
            username: String,
            password: String,
        },
        /// The searchable OCI namespaces this host is configured with.
        #[one_way(
            "cli",
            "Namespace settings reach the console through the web process's \
             own `/api/registries` endpoints, for the same reason as \
             `registry.search`."
        )]
        RegistryNamespaces = "registry.namespaces" {},
        /// Add or update a searchable namespace.
        #[one_way("cli", "Namespace settings, for the reason on `registry.namespaces`.")]
        RegistryNamespaceAdd = "registry.namespace_add" {
            namespace: String,
            use_for: crate::template::registries::RegistryUse,
        },
        /// Remove a searchable namespace.
        #[one_way("cli", "Namespace settings, for the reason on `registry.namespaces`.")]
        RegistryNamespaceRemove = "registry.namespace_remove" {
            namespace: String,
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

    /// A pull's host path is what tells the daemon where to put the file, and
    /// omitting it is how a caller asks for the bytes instead. A client that
    /// predates the inline form still sends `to`, and must still mean what it
    /// always did.
    #[test]
    fn a_pull_without_a_host_path_asks_for_the_bytes() {
        let inline = LabRequest::from_wire(
            "machine.pull_file",
            json!({"machine": "dc01", "from": "/var/log/syslog"}),
        )
        .unwrap();
        assert_eq!(
            inline,
            LabRequest::MachinePullFile {
                machine: "dc01".into(),
                from: "/var/log/syslog".into(),
                to: None,
            }
        );
        let to_host = LabRequest::from_wire(
            "machine.pull_file",
            json!({"vm": "dc01", "from": "/var/log/syslog", "to": "/tmp/syslog"}),
        )
        .unwrap();
        assert_eq!(
            to_host,
            LabRequest::MachinePullFile {
                machine: "dc01".into(),
                from: "/var/log/syslog".into(),
                to: Some("/tmp/syslog".into()),
            }
        );
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

    /// A command that is deliberately reachable from one surface carries the
    /// reason on its spec, next to the doc comment it was written beside.
    #[test]
    fn an_annotated_command_carries_its_reason() {
        let one_way = LabRequest::spec("run")
            .unwrap()
            .one_way
            .expect("`run` is annotated");
        let OneWay::Deliberate { surface, why } = one_way else {
            panic!("`run` is one-way by decision, not a tracked gap");
        };
        assert_eq!(surface, "cli");
        assert!(!why.is_empty());
    }

    /// A gap carries the issue instead of a reason, because there is no reason
    /// to carry: nobody has decided whether the asymmetry should close.
    ///
    /// This declares its own vocabulary rather than naming a real command,
    /// because there are no open gaps left — #37, #38 and #39 closed the
    /// sixteen that #36 recorded. Naming one would make this test fail every
    /// time somebody closed the last gap, which is the opposite of what it is
    /// for; the annotation has to keep working for the next one regardless.
    #[test]
    fn a_gap_carries_the_issue_tracking_it_rather_than_a_reason() {
        vocabulary! {
            /// A stand-in vocabulary, one command of each kind.
            Undecided {
                /// Nobody has decided about this one.
                #[one_way_gap("web", 1234)]
                Wondering = "wondering" {},
                /// This one is settled.
                #[one_way("cli", "because")]
                Settled = "settled" {},
            }
        }
        assert_eq!(
            Undecided::spec("wondering").unwrap().one_way,
            Some(OneWay::Gap {
                surface: "web",
                issue: 1234
            }),
        );
        assert_eq!(
            Undecided::spec("settled").unwrap().one_way,
            Some(OneWay::Deliberate {
                surface: "cli",
                why: "because"
            }),
        );
        // Whichever kind it is, the report asks it the same question.
        assert_eq!(
            Undecided::spec("wondering")
                .unwrap()
                .one_way
                .unwrap()
                .surface(),
            "web"
        );
    }

    /// Only a command more than one surface reaches says nothing: it has no
    /// asymmetry to explain. A one-way command that says nothing fails
    /// `report::every_one_way_command_records_why`.
    #[test]
    fn a_command_more_than_one_surface_reaches_asserts_nothing() {
        assert!(LabRequest::spec("up").unwrap().one_way.is_none());
    }

    /// `template.list` and `template.build` grew a `file` (and `build` a
    /// `version`) so a shell can point at any template file and pin a
    /// version. The console sends neither, and must keep working untouched.
    #[test]
    fn the_consoles_template_payloads_still_decode() {
        let req = SupRequest::from_wire(
            "template.list",
            json!({"lab": "demo", "root": "/labs/demo"}),
        )
        .unwrap();
        assert_eq!(
            req,
            SupRequest::TemplateList {
                lab: "demo".into(),
                root: "/labs/demo".into(),
                file: None,
            }
        );
        let req = SupRequest::from_wire(
            "template.build",
            json!({"lab": "demo", "root": "/labs/demo", "template": "base"}),
        )
        .unwrap();
        assert_eq!(
            req,
            SupRequest::TemplateBuild {
                lab: "demo".into(),
                root: "/labs/demo".into(),
                template: "base".into(),
                arch: None,
                version: None,
                file: None,
            }
        );
    }

    /// The store is addressed by reference, not by lab: a shell says which
    /// version it means and how far it will go, and everything else defaults.
    #[test]
    fn store_commands_default_to_the_safe_shape() {
        let req = SupRequest::from_wire("store.list", json!({})).unwrap();
        assert_eq!(req, SupRequest::StoreList { remote: false });

        let req = SupRequest::from_wire("store.prune", json!({})).unwrap();
        assert_eq!(
            req,
            SupRequest::StorePrune {
                filter: None,
                // Keeping one build and not touching the disk is what a bare
                // prune means; anything else has to be asked for.
                keep: 1,
                apply: false,
                force: false,
            }
        );

        let req = SupRequest::from_wire("store.push", json!({"reference": "x86_64/base"})).unwrap();
        assert_eq!(
            req,
            SupRequest::StorePush {
                reference: "x86_64/base".into(),
                target: None,
                source: None,
                prerelease: false,
                lab: String::new(),
            }
        );
    }

    /// A namespace's use is a closed set on the wire, so a typo is a bad
    /// argument rather than a namespace nothing will ever search.
    #[test]
    fn a_namespace_use_is_one_of_three_spellings() {
        use crate::template::registries::RegistryUse;
        let req = SupRequest::from_wire(
            "registry.namespace_add",
            json!({"namespace": "ghcr.io/acme", "use_for": "containers"}),
        )
        .unwrap();
        assert_eq!(
            req,
            SupRequest::RegistryNamespaceAdd {
                namespace: "ghcr.io/acme".into(),
                use_for: RegistryUse::Containers,
            }
        );
        let e = SupRequest::from_wire(
            "registry.namespace_add",
            json!({"namespace": "ghcr.io/acme", "use_for": "everything"}),
        )
        .unwrap_err();
        assert_eq!(e.code, crate::proto::ErrorCode::InvalidArgument);
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
