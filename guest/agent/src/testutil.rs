//! Shared helpers for the session tests: a Mux whose port writes land in a
//! channel, and a collector that decodes them back into frames.

#![cfg(test)]

use std::io::Write;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use vmlab_agent_proto::fileops::{self, Request, Response};
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
    /// Reassembles a fileops channel's replies out of its byte stream.
    ops: fileops::RecordDecoder,
    /// Replies already decoded but not yet handed out — a pipelining test
    /// gets several out of one frame.
    pending: std::collections::VecDeque<(Response, Vec<u8>)>,
}

/// Put one fileops request on `channel`, framed the way the host frames it.
pub fn ask(mux: &Mux, channel: u32, id: u64, op: fileops::Op) {
    ask_with(mux, channel, id, op, b"");
}

/// The same, for the one request that carries bytes: a write.
pub fn ask_with(mux: &Mux, channel: u32, id: u64, op: fileops::Op, payload: &[u8]) {
    mux.route_input(
        channel,
        crate::mux::Input::Bytes(fileops::encode_record(&Request { id, op }, payload)),
    );
}

pub fn capture_mux() -> (Mux, Capture) {
    let (tx, rx) = channel();
    (
        Mux::new(CapturePort(tx)),
        Capture {
            rx,
            dec: FrameDecoder::new(),
            ops: fileops::RecordDecoder::new(),
            pending: std::collections::VecDeque::new(),
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

    /// Next fileops reply on `channel`, with its raw payload. Replies are
    /// free to arrive out of order, so a pipelining test matches on the id
    /// rather than assuming the order it asked in.
    pub fn fileops(&mut self, channel: u32) -> (Response, Vec<u8>) {
        loop {
            if let Some(reply) = self.pending.pop_front() {
                return reply;
            }
            let f = self.frame();
            match f.kind {
                // Only a failed *channel* arrives as a control message; a
                // failed operation is a reply on the stream.
                FrameKind::Ctrl => {
                    if let AgentMsg::Error { msg, .. } =
                        serde_json::from_slice::<AgentMsg>(&f.payload).unwrap()
                    {
                        panic!("agent failed the channel: {msg}");
                    }
                }
                _ => {
                    assert_eq!(f.channel, channel);
                    self.ops.push(&f.payload);
                    while let Some(reply) = self.ops.next_record::<Response>().unwrap() {
                        self.pending.push_back(reply);
                    }
                }
            }
        }
    }
}
