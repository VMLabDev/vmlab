//! A client of the GDB remote serial protocol, as far as the lockstep needs
//! it: registers, memory, continue, interrupt, one hardware breakpoint.
//!
//! Written from the protocol as GDB documents it ("Remote Protocol" in the
//! GDB manual) and from the answers of QEMU's gdbstub (S6).

use super::Core;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

/// The bytes of QEMU's `g` answer for an aarch64 CPU: X0 to X30, SP and PC
/// of 8 bytes each, and CPSR of 4.
const CORE_BYTES: usize = 31 * 8 + 8 + 8 + 4;

/// The number of V0 for the `p` and `P` packets: QEMU's target description
/// puts V0 to V31, FPSR and FPCR after the 34 core registers.
const FIRST_VECTOR: usize = 34;

/// The most bytes of memory in one `M` packet. QEMU answers
/// `PacketSize=20020`, and a byte takes two characters.
const WRITE_CHUNK: usize = 0x4000;

pub struct Rsp {
    stream: UnixStream,
    /// Bytes read from the socket and not yet used.
    pending: Vec<u8>,
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

fn hex(bytes: &[u8]) -> Vec<u8> {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)]);
        out.push(DIGITS[usize::from(byte & 15)]);
    }
    out
}

fn unhex(text: &[u8]) -> io::Result<Vec<u8>> {
    let digit = |c: u8| match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(other(
            "the gdbstub sent a character that is not a hex digit",
        )),
    };
    if !text.len().is_multiple_of(2) {
        return Err(other("the gdbstub sent an odd count of hex digits"));
    }
    text.chunks(2)
        .map(|pair| Ok(digit(pair[0])? << 4 | digit(pair[1])?))
        .collect()
}

/// A packet around `payload`: `$payload#checksum`.
pub fn frame(payload: &[u8]) -> Vec<u8> {
    let checksum = payload
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    let mut packet = Vec::with_capacity(payload.len() + 4);
    packet.push(b'$');
    packet.extend_from_slice(payload);
    packet.push(b'#');
    packet.extend_from_slice(&hex(&[checksum]));
    packet
}

/// The payload of the first whole packet in `bytes`, and how many bytes it
/// took with what came before it (acknowledgements). `None`: not whole yet.
pub fn unframe(bytes: &[u8]) -> io::Result<Option<(Vec<u8>, usize)>> {
    let Some(start) = bytes.iter().position(|byte| *byte == b'$') else {
        return Ok(None);
    };
    let Some(end) = bytes[start..].iter().position(|byte| *byte == b'#') else {
        return Ok(None);
    };
    let end = start + end;
    if bytes.len() < end + 3 {
        return Ok(None);
    }
    let payload = &bytes[start + 1..end];
    let checksum = payload
        .iter()
        .fold(0u8, |sum, byte| sum.wrapping_add(*byte));
    if unhex(&bytes[end + 1..end + 3])? != [checksum] {
        return Err(other("a packet of the gdbstub has a wrong checksum"));
    }
    if payload.contains(&b'*') || payload.contains(&b'}') {
        return Err(other("the gdbstub sent a run-length or escaped packet"));
    }
    Ok(Some((payload.to_vec(), end + 3)))
}

impl Rsp {
    pub fn connect(path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(path)?;
        let mut rsp = Self {
            stream,
            pending: Vec::new(),
        };
        // The stop reason of the CPU that QEMU started with -S.
        let reason = rsp.command(b"?")?;
        if !reason.starts_with(b"T") && !reason.starts_with(b"S") {
            return Err(other("the gdbstub does not report a stopped CPU"));
        }
        let registers = rsp.command(b"g")?;
        if registers.len() != CORE_BYTES * 2 {
            return Err(other(format!(
                "the `g` answer has {} bytes, not the {CORE_BYTES} of an aarch64 core",
                registers.len() / 2
            )));
        }
        Ok(rsp)
    }

    pub fn send(&mut self, payload: &[u8]) -> io::Result<()> {
        self.stream.write_all(&frame(payload))
    }

    /// The next packet, waiting at most `timeout`. `None`: none came.
    pub fn receive(&mut self, timeout: Duration) -> io::Result<Option<Vec<u8>>> {
        self.stream.set_read_timeout(Some(timeout))?;
        loop {
            if let Some((payload, used)) = unframe(&self.pending)? {
                self.pending.drain(..used);
                self.stream.write_all(b"+")?;
                return Ok(Some(payload));
            }
            let mut buffer = [0u8; 4096];
            match self.stream.read(&mut buffer) {
                Ok(0) => return Err(other("the gdbstub closed the connection")),
                Ok(count) => self.pending.extend_from_slice(&buffer[..count]),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    return Ok(None);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }

    /// Send a packet and wait for its answer.
    pub fn command(&mut self, payload: &[u8]) -> io::Result<Vec<u8>> {
        self.send(payload)?;
        self.receive(Duration::from_secs(10))?
            .ok_or_else(|| other("the gdbstub did not answer in 10 seconds"))
    }

    fn expect_ok(&mut self, payload: &[u8], what: &str) -> io::Result<()> {
        let answer = self.command(payload)?;
        if answer != b"OK" {
            return Err(other(format!(
                "the gdbstub answered {:?} to {what}",
                String::from_utf8_lossy(&answer)
            )));
        }
        Ok(())
    }

    pub fn core(&mut self) -> io::Result<Core> {
        let bytes = unhex(&self.command(b"g")?)?;
        if bytes.len() != CORE_BYTES {
            return Err(other("the `g` answer has an unexpected length"));
        }
        let word = |at: usize| {
            let mut value = [0u8; 8];
            value.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(value)
        };
        let mut x = [0; 31];
        for (n, value) in x.iter_mut().enumerate() {
            *value = word(n * 8);
        }
        let mut cpsr = [0u8; 4];
        cpsr.copy_from_slice(&bytes[33 * 8..]);
        Ok(Core {
            x,
            sp: word(31 * 8),
            pc: word(32 * 8),
            cpsr: u32::from_le_bytes(cpsr),
        })
    }

    pub fn set_core(&mut self, core: &Core) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(CORE_BYTES);
        for value in core.x {
            bytes.extend(value.to_le_bytes());
        }
        bytes.extend(core.sp.to_le_bytes());
        bytes.extend(core.pc.to_le_bytes());
        bytes.extend(core.cpsr.to_le_bytes());
        let mut packet = vec![b'G'];
        packet.extend(hex(&bytes));
        self.expect_ok(&packet, "G")
    }

    pub fn write_memory(&mut self, address: u64, data: &[u8]) -> io::Result<()> {
        for (index, chunk) in data.chunks(WRITE_CHUNK).enumerate() {
            let at = address + (index * WRITE_CHUNK) as u64;
            let mut packet = format!("M{at:x},{:x}:", chunk.len()).into_bytes();
            packet.extend(hex(chunk));
            self.expect_ok(&packet, "M")?;
        }
        Ok(())
    }

    pub fn read_memory(&mut self, address: u64, len: usize) -> io::Result<Vec<u8>> {
        let answer = self.command(format!("m{address:x},{len:x}").as_bytes())?;
        if answer.starts_with(b"E") {
            return Err(other("the gdbstub cannot read that memory"));
        }
        unhex(&answer)
    }

    /// The vector register `n`. The gdbstub numbers V0 to V31 after the 34
    /// core registers.
    pub fn vector(&mut self, n: usize) -> io::Result<u128> {
        let bytes = unhex(&self.command(format!("p{:x}", FIRST_VECTOR + n).as_bytes())?)?;
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|_| other("the `p` answer for a vector register is not 16 bytes"))?;
        Ok(u128::from_le_bytes(bytes))
    }

    pub fn set_vector(&mut self, n: usize, value: u128) -> io::Result<()> {
        let mut packet = format!("P{:x}=", FIRST_VECTOR + n).into_bytes();
        packet.extend(hex(&value.to_le_bytes()));
        self.expect_ok(&packet, "P")
    }

    /// A hardware breakpoint on the instruction at `address`.
    pub fn break_at(&mut self, address: u64) -> io::Result<()> {
        self.expect_ok(format!("Z1,{address:x},4").as_bytes(), "Z1")
    }

    /// Let the CPU run. The stop reply comes later, through `receive`.
    pub fn resume(&mut self) -> io::Result<()> {
        self.send(b"c")
    }

    /// Ask a running CPU to stop. The stop reply comes through `receive`.
    pub fn interrupt(&mut self) -> io::Result<()> {
        self.stream.write_all(&[3])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_frame_and_unframe() {
        assert_eq!(frame(b"g"), b"$g#67");
        assert_eq!(frame(b""), b"$#00");
        assert_eq!(unframe(b"+$OK#9a").unwrap(), Some((b"OK".to_vec(), 7)));
        // Not whole yet.
        assert_eq!(unframe(b"+").unwrap(), None);
        assert_eq!(unframe(b"+$OK#9").unwrap(), None);
        assert_eq!(unframe(b"$T05thread:p01.01;").unwrap(), None);
        // Two packets: the first one only.
        let (payload, used) = unframe(b"$OK#9a$E01#a6").unwrap().unwrap();
        assert_eq!((payload.as_slice(), used), (b"OK".as_slice(), 6));
        assert!(unframe(b"$OK#00").is_err());
        assert!(unframe(b"$0*\"#00").is_err());
    }

    #[test]
    fn hex_goes_both_ways() {
        assert_eq!(hex(&[0x00, 0xab, 0x7f]), b"00ab7f");
        assert_eq!(unhex(b"00AB7f").unwrap(), vec![0x00, 0xab, 0x7f]);
        assert!(unhex(b"0").is_err());
        assert!(unhex(b"zz").is_err());
    }

    /// A gdbstub that answers from a script, on the other end of a socket pair.
    fn scripted(answers: Vec<(&'static [u8], Vec<u8>)>) -> (Rsp, std::thread::JoinHandle<()>) {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            let mut pending = Vec::new();
            for (expected, answer) in answers {
                let payload = loop {
                    if let Some((payload, used)) = unframe(&pending).unwrap() {
                        pending.drain(..used);
                        break payload;
                    }
                    let mut buffer = [0u8; 4096];
                    let count = theirs.read(&mut buffer).unwrap();
                    assert_ne!(count, 0, "the client closed before {expected:?}");
                    pending.extend_from_slice(&buffer[..count]);
                };
                assert_eq!(
                    String::from_utf8_lossy(&payload),
                    String::from_utf8_lossy(expected)
                );
                theirs.write_all(b"+").unwrap();
                theirs.write_all(&frame(&answer)).unwrap();
            }
        });
        (
            Rsp {
                stream: ours,
                pending: Vec::new(),
            },
            server,
        )
    }

    #[test]
    fn registers_and_memory_go_through_a_scripted_stub() {
        let mut core = Core {
            x: [0; 31],
            sp: 0x1122_3344_5566_7788,
            pc: 0x4200_0000,
            cpsr: 0x6000_03c5,
        };
        core.x[1] = 0x0102_0304_0506_0708;
        let mut image = Vec::new();
        for value in core.x {
            image.extend(value.to_le_bytes());
        }
        image.extend(core.sp.to_le_bytes());
        image.extend(core.pc.to_le_bytes());
        image.extend(core.cpsr.to_le_bytes());
        let mut set = vec![b'G'];
        set.extend(hex(&image));
        let set: &'static [u8] = Box::leak(set.into_boxed_slice());
        let (mut rsp, server) = scripted(vec![
            (&b"g"[..], hex(&image)),
            (set, b"OK".to_vec()),
            (&b"M42000000,4:00040091"[..], b"OK".to_vec()),
            (&b"m42000000,4"[..], b"00040091".to_vec()),
            (&b"Z1,200,4"[..], b"OK".to_vec()),
            (&b"M0,1:00"[..], b"E14".to_vec()),
            (&b"p23"[..], b"0102030405060708090a0b0c0d0e0f10".to_vec()),
            (
                &b"P41=100f0e0d0c0b0a090807060504030201"[..],
                b"OK".to_vec(),
            ),
        ]);
        assert_eq!(rsp.core().unwrap(), core);
        rsp.set_core(&core).unwrap();
        rsp.write_memory(0x4200_0000, &0x9100_0400u32.to_le_bytes())
            .unwrap();
        assert_eq!(
            rsp.read_memory(0x4200_0000, 4).unwrap(),
            [0x00, 0x04, 0x00, 0x91]
        );
        rsp.break_at(0x200).unwrap();
        assert!(rsp.write_memory(0, &[0]).is_err());
        assert_eq!(rsp.vector(1).unwrap(), 0x100f_0e0d_0c0b_0a09_0807_0605_0403_0201);
        rsp.set_vector(31, 0x0102_0304_0506_0708_090a_0b0c_0d0e_0f10)
            .unwrap();
        server.join().unwrap();
    }
}
