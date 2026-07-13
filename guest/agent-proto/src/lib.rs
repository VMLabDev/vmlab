//! The wire contract between the vmlab host and `vmlab-agent`, the in-guest
//! agent that serves interactive terminals, streaming exec, file transfer,
//! tailing, metrics and clipboard over **one** virtio-serial port
//! (`vmlab.agent.0`) — no guest network involved.
//!
//! The stream is a sequence of length-prefixed frames multiplexing many
//! channels over the single port:
//!
//! ```text
//! magic "VMLB" | len u32 LE | kind u8 | channel u32 LE | payload (len bytes)
//! ```
//!
//! Channel 0 is the control channel; its payloads are JSON-encoded
//! [`HostMsg`] / [`AgentMsg`]. Every other channel is a byte stream opened by
//! a control message (`open_terminal`, `open_exec`, …) whose bytes ride
//! [`FrameKind::Data`] frames ([`FrameKind::DataErr`] for exec stderr).
//!
//! The magic prefix exists for resynchronisation: after an online snapshot
//! restore QEMU can replay or drop bytes on the virtio-serial stream, so a
//! receiver that desyncs rescans to the next magic ([`FrameDecoder`]) and the
//! host re-handshakes with a fresh [`HostMsg::Hello`] token, which the agent
//! echoes — everything before the echo is discarded.
//!
//! Data channels are flow-controlled with credit windows: a sender may have
//! at most the granted window of un-acknowledged payload bytes in flight per
//! channel; the receiver grants more with `window_adjust`. Control frames are
//! never subject to flow control.

use serde::{Deserialize, Serialize};

/// Version of this contract. The agent reports it in [`AgentMsg::Hello`]; the
/// host refuses to drive an agent speaking a different version.
pub const PROTO_VERSION: u32 = 1;

/// The virtio-serial port name the agent serves on (both full VMs and
/// container micro-VMs).
pub const PORT_NAME: &str = "vmlab.agent.0";

/// Hard cap on a single frame's payload.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// Initial per-channel receive window either side grants when a channel
/// opens. Generous enough that interactive terminals never stall on credit.
pub const INITIAL_WINDOW: u64 = 256 * 1024;

/// Replenish threshold: grant more credit once half the window is consumed.
pub const WINDOW_REPLENISH: u64 = INITIAL_WINDOW / 2;

/// Feature strings advertised in [`AgentMsg::Hello`]. Hosts must tolerate
/// absent features (e.g. `clipboard` never appears on a headless guest).
pub mod features {
    pub const TERMINAL: &str = "terminal";
    pub const EXEC: &str = "exec";
    pub const FILE: &str = "file";
    pub const TAIL: &str = "tail";
    pub const METRICS: &str = "metrics";
    pub const CLIPBOARD: &str = "clipboard";
    /// Windows event-log tailing.
    pub const EVENTLOG: &str = "eventlog";
}

/// How `vmlab-cinit` tells a container micro-VM's agent about the container
/// it serves (written as JSON, passed via `vmlab-agent --container <path>`).
/// Not host-visible — this is a guest-internal contract, but it lives here
/// so cinit and the agent share one definition.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ContainerConfig {
    /// The merged container rootfs (cinit's overlay mount).
    pub rootfs: String,
    /// A process inside the container's PID+mount namespaces to `setns`
    /// into for terminal/exec sessions. `None` in idle mode, where no
    /// namespaces exist and a plain chroot into `rootfs` is the container.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setns_pid: Option<u32>,
    /// Container environment (image env merged with lab overrides).
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Working directory inside the rootfs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
}

/// Initramfs layout shared by cinit and the agent: the static BusyBox that
/// cinit injects into every container rootfs so distroless images still get
/// a shell/toolbox (paths as seen *inside* the rootfs).
pub const BUSYBOX_FALLBACK: &str = "/.vmlab/busybox";
pub const BUSYBOX_BIN_DIR: &str = "/.vmlab/bin";

const MAGIC: [u8; 4] = *b"VMLB";
const HEADER_LEN: usize = 4 + 4 + 1 + 4;

/// What a frame's payload is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameKind {
    /// JSON [`HostMsg`] / [`AgentMsg`] on channel 0.
    Ctrl,
    /// Channel byte stream (PTY bytes, exec stdin/stdout, file bytes, tail
    /// output).
    Data,
    /// Exec stderr (same channel id as the exec's stdout).
    DataErr,
}

impl FrameKind {
    fn to_u8(self) -> u8 {
        match self {
            FrameKind::Ctrl => 0,
            FrameKind::Data => 1,
            FrameKind::DataErr => 2,
        }
    }

    fn from_u8(b: u8) -> Option<Self> {
        match b {
            0 => Some(FrameKind::Ctrl),
            1 => Some(FrameKind::Data),
            2 => Some(FrameKind::DataErr),
            _ => None,
        }
    }
}

/// One decoded frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub kind: FrameKind,
    pub channel: u32,
    pub payload: Vec<u8>,
}

/// Encode one frame. Panics if `payload` exceeds [`MAX_PAYLOAD`] — callers
/// chunk their streams.
pub fn encode_frame(kind: FrameKind, channel: u32, payload: &[u8]) -> Vec<u8> {
    assert!(payload.len() <= MAX_PAYLOAD, "frame payload too large");
    let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.push(kind.to_u8());
    out.extend_from_slice(&channel.to_le_bytes());
    out.extend_from_slice(payload);
    out
}

/// Encode a control message (JSON payload on channel 0).
pub fn encode_ctrl<T: Serialize>(msg: &T) -> Vec<u8> {
    let payload = serde_json::to_vec(msg).expect("ctl message serializes");
    encode_frame(FrameKind::Ctrl, 0, &payload)
}

/// Incremental frame decoder with desync recovery: feed raw bytes with
/// [`push`](Self::push), drain complete frames with [`next_frame`]
/// (Self::next_frame). Garbage between frames (snapshot-restore replay,
/// mid-frame connect) is skipped by scanning to the next magic; a frame whose
/// header is implausible (bad kind, oversized payload) is treated as garbage
/// starting one byte in, so a magic embedded in stream data cannot wedge the
/// decoder.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop all buffered bytes (host re-handshake after snapshot restore).
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next_frame(&mut self) -> Option<Frame> {
        loop {
            // Scan to the next magic, discarding garbage.
            match find_magic(&self.buf) {
                Some(0) => {}
                Some(off) => {
                    self.buf.drain(..off);
                }
                None => {
                    // No magic: keep at most the last 3 bytes (a magic could
                    // straddle the boundary), drop the rest.
                    let keep = self.buf.len().min(MAGIC.len() - 1);
                    self.buf.drain(..self.buf.len() - keep);
                    return None;
                }
            }
            if self.buf.len() < HEADER_LEN {
                return None;
            }
            let len = u32::from_le_bytes(self.buf[4..8].try_into().unwrap()) as usize;
            let kind = FrameKind::from_u8(self.buf[8]);
            let (Some(kind), true) = (kind, len <= MAX_PAYLOAD) else {
                // Implausible header: this "magic" was stream data. Skip one
                // byte and rescan.
                self.buf.drain(..1);
                continue;
            };
            if self.buf.len() < HEADER_LEN + len {
                return None;
            }
            let channel = u32::from_le_bytes(self.buf[9..13].try_into().unwrap());
            let payload = self.buf[HEADER_LEN..HEADER_LEN + len].to_vec();
            self.buf.drain(..HEADER_LEN + len);
            return Some(Frame {
                kind,
                channel,
                payload,
            });
        }
    }
}

fn find_magic(buf: &[u8]) -> Option<usize> {
    buf.windows(MAGIC.len()).position(|w| w == MAGIC)
}

/// Per-channel send-side credit window. The receiver's grants add credit;
/// sending consumes it. A sender must not put more payload bytes on the wire
/// than it has credit for.
#[derive(Debug)]
pub struct SendWindow {
    credit: u64,
}

impl SendWindow {
    pub fn new(initial: u64) -> Self {
        Self { credit: initial }
    }

    pub fn credit(&self) -> u64 {
        self.credit
    }

    /// How many bytes may be sent right now (capped at one frame).
    pub fn sendable(&self) -> usize {
        self.credit.min(MAX_PAYLOAD as u64) as usize
    }

    /// Consume credit for a payload about to be sent.
    pub fn consume(&mut self, bytes: usize) {
        debug_assert!(bytes as u64 <= self.credit, "flow-control overrun");
        self.credit = self.credit.saturating_sub(bytes as u64);
    }

    /// A `window_adjust` arrived.
    pub fn grant(&mut self, bytes: u64) {
        self.credit = self.credit.saturating_add(bytes);
    }
}

/// Per-channel receive-side accounting: tracks consumed bytes and says when
/// (and how much) to replenish the sender.
#[derive(Debug, Default)]
pub struct RecvWindow {
    consumed: u64,
}

impl RecvWindow {
    /// Record received payload bytes; returns `Some(grant)` when a
    /// `window_adjust` should be sent back.
    pub fn recv(&mut self, bytes: usize) -> Option<u64> {
        self.consumed += bytes as u64;
        if self.consumed >= WINDOW_REPLENISH {
            let grant = self.consumed;
            self.consumed = 0;
            Some(grant)
        } else {
            None
        }
    }
}

/// Messages the host sends to the agent (channel 0, JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum HostMsg {
    /// Handshake. Sent on (re)connect; the agent replies with its own
    /// [`AgentMsg::Hello`] echoing `token`, and both sides discard channel
    /// state from before the exchange.
    Hello {
        proto_version: u32,
        token: String,
    },
    /// Open an interactive shell on channel `id`. `command` overrides the
    /// agent's default shell (absolute argv).
    OpenTerminal {
        id: u32,
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<Vec<String>>,
    },
    /// Resize a terminal's PTY.
    Resize {
        id: u32,
        cols: u16,
        rows: u16,
    },
    /// Run `argv` (no PTY) on channel `id`: stdin = host DATA frames,
    /// stdout = guest DATA frames, stderr = guest DATA_ERR frames, exit via
    /// [`AgentMsg::Exited`].
    OpenExec {
        id: u32,
        argv: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// No more host->guest bytes on this channel (exec stdin EOF; end of a
    /// pushed file's bytes).
    Eof {
        id: u32,
    },
    /// Receive a file: host streams DATA frames, then [`HostMsg::Eof`]; the
    /// agent writes `path` (mode is Unix permission bits, ignored on
    /// Windows) and replies [`AgentMsg::FileDone`].
    OpenFilePush {
        id: u32,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<u32>,
    },
    /// Send a file: the agent streams `path` as DATA frames and finishes
    /// with [`AgentMsg::FileDone`].
    OpenFilePull {
        id: u32,
        path: String,
    },
    /// Follow a file (like `tail -F`): existing tail then live appends as
    /// DATA frames until closed. Survives rotation/truncation.
    OpenTail {
        id: u32,
        path: String,
    },
    /// Follow the Windows event log (XML-rendered events as DATA frames).
    /// `filter` is an XPath query, default `*` on the System channel.
    OpenEventLog {
        id: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        filter: Option<String>,
    },
    /// Set the guest clipboard.
    SetClipboard {
        text: String,
    },
    /// Ask for the guest clipboard; the agent replies [`AgentMsg::Clipboard`].
    GetClipboard,
    /// Start periodic [`AgentMsg::Metrics`].
    SubscribeMetrics {
        interval_secs: u64,
    },
    UnsubscribeMetrics,
    /// Grant the guest more send credit on a channel.
    WindowAdjust {
        id: u32,
        bytes: u64,
    },
    /// Tear down a channel (kills the terminal/exec process, stops a
    /// tail/transfer). The agent must not send further frames for `id`.
    Close {
        id: u32,
    },
    Ping,
}

/// Messages the agent sends to the host (channel 0, JSON).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentMsg {
    /// Handshake reply; `token` echoes [`HostMsg::Hello`]. `features` lists
    /// what this agent supports on this guest (see [`features`]).
    Hello {
        proto_version: u32,
        agent_version: String,
        os: String,
        features: Vec<String>,
        token: String,
    },
    /// A channel opened by the host is live (terminal spawned, exec started,
    /// file opened, tail/eventlog following).
    Opened {
        id: u32,
    },
    /// The channel's process ended (terminal shell exit, exec exit).
    /// `code` is the exit code, or `128 + signal` on Unix signal death.
    Exited {
        id: u32,
        code: i32,
    },
    /// A file transfer completed (both directions). `sha256` is the hex
    /// digest of the bytes written/read so the host can verify.
    FileDone {
        id: u32,
        sha256: String,
        len: u64,
    },
    /// Periodic sample after [`HostMsg::SubscribeMetrics`].
    Metrics {
        cpu_pct: f32,
        mem_used: u64,
        mem_total: u64,
        disks: Vec<DiskUsage>,
    },
    /// Guest clipboard contents (reply to `get_clipboard`, or spontaneous on
    /// guest-side clipboard change).
    Clipboard {
        text: String,
    },
    /// Grant the host more send credit on a channel.
    WindowAdjust {
        id: u32,
        bytes: u64,
    },
    /// A channel failed (`id` set) or the agent hit a channel-less error.
    Error {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        id: Option<u32>,
        msg: String,
    },
    Pong,
}

/// One mounted filesystem in [`AgentMsg::Metrics`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskUsage {
    /// Mount point (Unix) or drive root like `C:\` (Windows).
    pub mount: String,
    pub used: u64,
    pub total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<T>(v: &T) -> T
    where
        T: serde::Serialize + serde::de::DeserializeOwned,
    {
        serde_json::from_str(&serde_json::to_string(v).unwrap()).unwrap()
    }

    #[test]
    fn frame_roundtrips() {
        let wire = encode_frame(FrameKind::Data, 7, b"hello");
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        let f = dec.next_frame().unwrap();
        assert_eq!(f.kind, FrameKind::Data);
        assert_eq!(f.channel, 7);
        assert_eq!(f.payload, b"hello");
        assert!(dec.next_frame().is_none());
    }

    #[test]
    fn decoder_handles_split_delivery() {
        let wire = encode_frame(FrameKind::DataErr, 3, &[9u8; 1000]);
        let mut dec = FrameDecoder::new();
        for chunk in wire.chunks(13) {
            dec.push(chunk);
        }
        let f = dec.next_frame().unwrap();
        assert_eq!(f.kind, FrameKind::DataErr);
        assert_eq!(f.payload.len(), 1000);
    }

    #[test]
    fn decoder_rescans_past_garbage() {
        let mut wire = b"replayed junk from a snapshot \xff\x00".to_vec();
        wire.extend(encode_frame(FrameKind::Ctrl, 0, b"{}"));
        // And garbage *between* frames too.
        wire.extend(b"more noise");
        wire.extend(encode_frame(FrameKind::Data, 1, b"x"));
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        assert_eq!(dec.next_frame().unwrap().channel, 0);
        assert_eq!(dec.next_frame().unwrap().payload, b"x");
        assert!(dec.next_frame().is_none());
    }

    #[test]
    fn decoder_skips_false_magic_with_bad_header() {
        // A magic followed by an implausible kind byte must not wedge the
        // decoder: it skips a byte and finds the real frame behind it.
        let mut wire = Vec::new();
        wire.extend_from_slice(b"VMLB");
        wire.extend_from_slice(&(10u32).to_le_bytes());
        wire.push(0xEE); // invalid kind
        wire.extend_from_slice(&[0; 4]);
        wire.extend(encode_frame(FrameKind::Data, 2, b"real"));
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        let f = dec.next_frame().unwrap();
        assert_eq!(f.channel, 2);
        assert_eq!(f.payload, b"real");
    }

    #[test]
    fn decoder_skips_false_magic_with_oversize_len() {
        let mut wire = Vec::new();
        wire.extend_from_slice(b"VMLB");
        wire.extend_from_slice(&(u32::MAX).to_le_bytes());
        wire.push(1);
        wire.extend_from_slice(&[0; 4]);
        wire.extend(encode_frame(FrameKind::Ctrl, 0, b"[]"));
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        assert_eq!(dec.next_frame().unwrap().payload, b"[]");
    }

    #[test]
    fn decoder_keeps_partial_magic_tail() {
        let mut dec = FrameDecoder::new();
        dec.push(b"garbage garbage VM");
        assert!(dec.next_frame().is_none());
        // The trailing "VM" must survive the garbage drop so a magic split
        // across pushes still decodes.
        let rest = &encode_frame(FrameKind::Data, 5, b"ok")[4..];
        dec.push(b"LB"); // completes the magic...
        dec.push(rest); // ...followed by the rest of the frame
        let f = dec.next_frame().unwrap();
        assert_eq!(f.channel, 5);
        assert_eq!(f.payload, b"ok");
    }

    #[test]
    fn empty_payload_frames_work() {
        let wire = encode_frame(FrameKind::Data, 9, b"");
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        let f = dec.next_frame().unwrap();
        assert_eq!(f.channel, 9);
        assert!(f.payload.is_empty());
    }

    #[test]
    fn send_window_accounts_credit() {
        let mut w = SendWindow::new(10);
        assert_eq!(w.sendable(), 10);
        w.consume(10);
        assert_eq!(w.sendable(), 0);
        w.grant(INITIAL_WINDOW);
        assert_eq!(w.sendable(), MAX_PAYLOAD); // capped at one frame
    }

    #[test]
    fn recv_window_replenishes_at_threshold() {
        let mut w = RecvWindow::default();
        assert_eq!(w.recv(1000), None);
        let grant = w.recv(WINDOW_REPLENISH as usize).unwrap();
        assert_eq!(grant, 1000 + WINDOW_REPLENISH);
        assert_eq!(w.recv(8), None); // counter reset
    }

    #[test]
    fn host_msgs_roundtrip() {
        for m in [
            HostMsg::Hello {
                proto_version: PROTO_VERSION,
                token: "t0".into(),
            },
            HostMsg::OpenTerminal {
                id: 1,
                cols: 120,
                rows: 32,
                command: None,
            },
            HostMsg::OpenTerminal {
                id: 2,
                cols: 80,
                rows: 24,
                command: Some(vec!["/bin/zsh".into(), "-l".into()]),
            },
            HostMsg::Resize {
                id: 1,
                cols: 132,
                rows: 43,
            },
            HostMsg::OpenExec {
                id: 3,
                argv: vec!["ls".into(), "-l".into()],
                env: vec![("K".into(), "v".into())],
                cwd: Some("/tmp".into()),
            },
            HostMsg::Eof { id: 3 },
            HostMsg::OpenFilePush {
                id: 4,
                path: "/etc/motd".into(),
                mode: Some(0o644),
            },
            HostMsg::OpenFilePull {
                id: 5,
                path: "C:\\log.txt".into(),
            },
            HostMsg::OpenTail {
                id: 6,
                path: "/var/log/syslog".into(),
            },
            HostMsg::OpenEventLog {
                id: 7,
                filter: None,
            },
            HostMsg::SetClipboard { text: "hi".into() },
            HostMsg::GetClipboard,
            HostMsg::SubscribeMetrics { interval_secs: 5 },
            HostMsg::UnsubscribeMetrics,
            HostMsg::WindowAdjust {
                id: 4,
                bytes: 65536,
            },
            HostMsg::Close { id: 1 },
            HostMsg::Ping,
        ] {
            assert_eq!(roundtrip(&m), m);
        }
    }

    #[test]
    fn agent_msgs_roundtrip() {
        for m in [
            AgentMsg::Hello {
                proto_version: PROTO_VERSION,
                agent_version: "0.1.0".into(),
                os: "linux".into(),
                features: vec![features::TERMINAL.into(), features::EXEC.into()],
                token: "t0".into(),
            },
            AgentMsg::Opened { id: 1 },
            AgentMsg::Exited { id: 1, code: 130 },
            AgentMsg::FileDone {
                id: 4,
                sha256: "ab".repeat(32),
                len: 1 << 30,
            },
            AgentMsg::Metrics {
                cpu_pct: 12.5,
                mem_used: 1 << 30,
                mem_total: 4 << 30,
                disks: vec![DiskUsage {
                    mount: "/".into(),
                    used: 10,
                    total: 100,
                }],
            },
            AgentMsg::Clipboard {
                text: "clip".into(),
            },
            AgentMsg::WindowAdjust { id: 2, bytes: 1 },
            AgentMsg::Error {
                id: Some(9),
                msg: "no such file".into(),
            },
            AgentMsg::Error {
                id: None,
                msg: "bad frame".into(),
            },
            AgentMsg::Pong,
        ] {
            assert_eq!(roundtrip(&m), m);
        }
    }

    #[test]
    fn ctl_msgs_use_snake_case_tags() {
        assert_eq!(
            serde_json::to_string(&HostMsg::Ping).unwrap(),
            r#"{"cmd":"ping"}"#
        );
        assert_eq!(
            serde_json::to_string(&HostMsg::Resize {
                id: 1,
                cols: 80,
                rows: 24
            })
            .unwrap(),
            r#"{"cmd":"resize","id":1,"cols":80,"rows":24}"#
        );
        assert_eq!(
            serde_json::to_string(&AgentMsg::Opened { id: 3 }).unwrap(),
            r#"{"event":"opened","id":3}"#
        );
    }

    #[test]
    fn encode_ctrl_produces_a_channel0_ctrl_frame() {
        let wire = encode_ctrl(&HostMsg::Ping);
        let mut dec = FrameDecoder::new();
        dec.push(&wire);
        let f = dec.next_frame().unwrap();
        assert_eq!(f.kind, FrameKind::Ctrl);
        assert_eq!(f.channel, 0);
        let m: HostMsg = serde_json::from_slice(&f.payload).unwrap();
        assert_eq!(m, HostMsg::Ping);
    }
}
