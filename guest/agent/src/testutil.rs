//! Shared helpers for the session tests: a Mux whose port writes land in a
//! channel, and a collector that decodes them back into frames.

#![cfg(test)]

use std::io::Write;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use vmlab_agent_proto::{AgentMsg, Frame, FrameDecoder, FrameKind};

use crate::mux::Mux;

struct CapturePort(Sender<Vec<u8>>);

impl Write for CapturePort {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = self.0.send(buf.to_vec());
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub struct Capture {
    rx: Receiver<Vec<u8>>,
    dec: FrameDecoder,
}

pub fn capture_mux() -> (Mux, Capture) {
    let (tx, rx) = channel();
    (
        Mux::new(CapturePort(tx)),
        Capture {
            rx,
            dec: FrameDecoder::new(),
        },
    )
}

impl Capture {
    /// Next frame off the wire (10s timeout).
    pub fn frame(&mut self) -> Frame {
        loop {
            if let Some(f) = self.dec.next_frame() {
                return f;
            }
            let bytes = self
                .rx
                .recv_timeout(Duration::from_secs(10))
                .expect("frame within 10s");
            self.dec.push(&bytes);
        }
    }

    /// Next control message, skipping data frames.
    pub fn ctrl(&mut self) -> AgentMsg {
        loop {
            let f = self.frame();
            if f.kind == FrameKind::Ctrl {
                return serde_json::from_slice(&f.payload).unwrap();
            }
        }
    }

    /// Accumulate DATA payloads on `channel` until `needle` appears in them
    /// (or panic after 10s). Control frames are collected and returned too.
    pub fn data_until(&mut self, channel: u32, needle: &[u8]) -> (Vec<u8>, Vec<AgentMsg>) {
        let mut data = Vec::new();
        let mut msgs = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !data.windows(needle.len().max(1)).any(|w| w == needle) {
            assert!(Instant::now() < deadline, "timed out; got {data:?}");
            let f = self.frame();
            match f.kind {
                FrameKind::Ctrl => msgs.push(serde_json::from_slice(&f.payload).unwrap()),
                _ => {
                    assert_eq!(f.channel, channel);
                    data.extend(f.payload);
                }
            }
        }
        (data, msgs)
    }

    /// Collect frames until the channel reports `exited`; returns
    /// (accumulated stdout-kind data, accumulated stderr-kind data, code).
    pub fn until_exited(&mut self, channel: u32) -> (Vec<u8>, Vec<u8>, i32) {
        let mut out = Vec::new();
        let mut err = Vec::new();
        loop {
            let f = self.frame();
            match f.kind {
                FrameKind::Ctrl => match serde_json::from_slice::<AgentMsg>(&f.payload).unwrap() {
                    AgentMsg::Exited { id, code } if id == channel => {
                        return (out, err, code);
                    }
                    _ => {}
                },
                FrameKind::Data => {
                    assert_eq!(f.channel, channel);
                    out.extend(f.payload);
                }
                FrameKind::DataErr => {
                    assert_eq!(f.channel, channel);
                    err.extend(f.payload);
                }
            }
        }
    }

    /// Collect frames until `file_done`; returns (data, sha256, len).
    pub fn until_file_done(&mut self, channel: u32) -> (Vec<u8>, String, u64) {
        let mut data = Vec::new();
        loop {
            let f = self.frame();
            match f.kind {
                FrameKind::Ctrl => match serde_json::from_slice::<AgentMsg>(&f.payload).unwrap() {
                    AgentMsg::FileDone { id, sha256, len } if id == channel => {
                        return (data, sha256, len);
                    }
                    AgentMsg::Error { msg, .. } => panic!("agent error: {msg}"),
                    _ => {}
                },
                _ => {
                    assert_eq!(f.channel, channel);
                    data.extend(f.payload);
                }
            }
        }
    }
}
