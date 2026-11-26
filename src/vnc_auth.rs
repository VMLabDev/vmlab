//! VNC authentication for the console TCP bridge (PRD §11).
//!
//! `vnc.sock` offers RFB security type None, which is sound for a socket
//! only its owner can reach. A loopback TCP port is reachable by every
//! user of the host, so the bridge ends the viewer's handshake itself: it
//! offers VNC authentication (RFB security type 2) alone, under a password
//! made for that one bridge, and connects to `vnc.sock` only for a viewer
//! that answered the challenge. The handshake with `vnc.sock` is then the
//! bridge's own, with type None.
//!
//! VNC authentication is a DES challenge and response, and no crate vmlab
//! depends on has DES, so the cipher is here. It is used for this
//! handshake and for the password file `vncviewer` reads, nothing else.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// VNC authentication reads eight bytes of a password and no more.
pub const PASSWORD_LEN: usize = 8;

/// Six bits a character, so eight of them are 48 bits, and a byte maps to
/// a character without bias.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A fresh password for one bridge.
pub fn new_password() -> String {
    rand::random::<[u8; PASSWORD_LEN]>()
        .iter()
        .map(|byte| ALPHABET[(byte & 63) as usize] as char)
        .collect()
}

/// Whether `password` is one [`new_password`] could have made. The bridge
/// helper takes its password from its environment and checks it with this.
pub fn is_password(password: &str) -> bool {
    password.len() == PASSWORD_LEN && password.bytes().all(|byte| ALPHABET.contains(&byte))
}

/// The answer to a VNC authentication challenge: both halves of it
/// encrypted under the password.
pub fn response(password: &str, challenge: &[u8; 16]) -> [u8; 16] {
    let key = vnc_key(password.as_bytes());
    let mut out = [0u8; 16];
    for half in [0, 8] {
        let plain: [u8; 8] = std::array::from_fn(|i| challenge[half + i]);
        out[half..half + 8].copy_from_slice(&des_encrypt(key, plain));
    }
    out
}

/// The content of a `vncviewer -passwd` file: the password encrypted under
/// the key every VNC implementation shares. It hides the password from a
/// glance and from nothing else, so the file is kept owner-only.
pub fn password_file(password: &str) -> [u8; 8] {
    let mut plain = [0u8; 8];
    let bytes = password.as_bytes();
    let len = bytes.len().min(8);
    plain[..len].copy_from_slice(&bytes[..len]);
    des_encrypt(vnc_key(&[23, 82, 107, 6, 35, 78, 88, 7]), plain)
}

/// A viewer that answered the challenge and waits for the security result.
#[derive(Debug)]
pub struct Admitted {
    minor: u8,
}

/// The server half of the RFB handshake with a TCP viewer, up to the
/// checked response. A viewer that asks for another security type or
/// answers wrongly is refused here and gets an error.
pub async fn challenge_viewer<S>(viewer: &mut S, password: &str) -> io::Result<Admitted>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    viewer.write_all(b"RFB 003.008\n").await?;
    let admitted = Admitted {
        minor: read_version(viewer).await?,
    };
    if admitted.minor == 3 {
        // RFB 3.3: the server names the one type.
        viewer.write_u32(2).await?;
    } else {
        viewer.write_all(&[1, 2]).await?;
        if viewer.read_u8().await? != 2 {
            refuse(
                viewer,
                &admitted,
                "this console takes VNC authentication only",
            )
            .await?;
            return Err(denied("the viewer asked for another security type"));
        }
    }
    let challenge: [u8; 16] = rand::random();
    viewer.write_all(&challenge).await?;
    let mut answer = [0u8; 16];
    viewer.read_exact(&mut answer).await?;
    if answer != response(password, &challenge) {
        refuse(viewer, &admitted, "wrong password").await?;
        return Err(denied("the viewer gave a wrong password"));
    }
    Ok(admitted)
}

/// Tell an admitted viewer that it is in. The bytes after this are the
/// viewer's and the server's own.
pub async fn admit<S>(viewer: &mut S, _admitted: Admitted) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    viewer.write_u32(0).await
}

/// The failed security result, with the reason RFB 3.8 carries.
pub async fn refuse<S>(viewer: &mut S, admitted: &Admitted, reason: &str) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    viewer.write_u32(1).await?;
    if admitted.minor == 8 {
        viewer.write_u32(reason.len() as u32).await?;
        viewer.write_all(reason.as_bytes()).await?;
    }
    viewer.flush().await
}

/// The client half of the RFB handshake with `vnc.sock`, with security
/// type None, up to the security result. What follows on `server` is what
/// follows the result on the viewer's side: the client's init message.
pub async fn open_server<S>(server: &mut S) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let minor = read_version(server).await?;
    server
        .write_all(format!("RFB 003.00{minor}\n").as_bytes())
        .await?;
    let wants_password = || denied("the console's own socket asks for authentication");
    if minor == 3 {
        if server.read_u32().await? != 1 {
            return Err(wants_password());
        }
        return Ok(());
    }
    let mut types = vec![0u8; server.read_u8().await? as usize];
    server.read_exact(&mut types).await?;
    if !types.contains(&1) {
        return Err(wants_password());
    }
    server.write_all(&[1]).await?;
    // RFB 3.7 sends no result for type None.
    if minor == 8 && server.read_u32().await? != 0 {
        return Err(denied("the console's own socket refused the bridge"));
    }
    Ok(())
}

fn denied(why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, why.to_string())
}

/// Read the peer's `RFB 003.00x\n` and give the minor version both ends
/// then speak: 3, 7 or 8. An unknown minor is 3.3, as the protocol says.
async fn read_version<S>(peer: &mut S) -> io::Result<u8>
where
    S: AsyncRead + Unpin,
{
    let mut version = [0u8; 12];
    peer.read_exact(&mut version).await?;
    if &version[..4] != b"RFB " || version[11] != b'\n' {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not an RFB version",
        ));
    }
    Ok(match &version[4..11] {
        b"003.008" => 8,
        b"003.007" => 7,
        _ => 3,
    })
}

/// VNC turns each byte of a password round before it uses it as a DES key.
fn vnc_key(password: &[u8]) -> u64 {
    let mut key = [0u8; 8];
    for (slot, byte) in key.iter_mut().zip(password) {
        *slot = byte.reverse_bits();
    }
    u64::from_be_bytes(key)
}

// DES (FIPS 46-3). The tables count bits from 1 at the most significant.

const IP: [u8; 64] = [
    58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14, 6,
    64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11, 3, 61,
    53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
];
const FP: [u8; 64] = [
    40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62, 30,
    37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19, 59, 27,
    34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
];
const E: [u8; 48] = [
    32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17, 18,
    19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
];
const P: [u8; 32] = [
    16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9, 19,
    13, 30, 6, 22, 11, 4, 25,
];
const PC1: [u8; 56] = [
    57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3, 60,
    52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45, 37, 29,
    21, 13, 5, 28, 20, 12, 4,
];
const PC2: [u8; 48] = [
    14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41, 52,
    31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
];
const SHIFTS: [u32; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
const SBOX: [[u8; 64]; 8] = [
    [
        14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7, 0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12,
        11, 9, 5, 3, 8, 4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0, 15, 12, 8, 2, 4, 9,
        1, 7, 5, 11, 3, 14, 10, 0, 6, 13,
    ],
    [
        15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10, 3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1,
        10, 6, 9, 11, 5, 0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15, 13, 8, 10, 1, 3, 15,
        4, 2, 11, 6, 7, 12, 0, 5, 14, 9,
    ],
    [
        10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8, 13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5,
        14, 12, 11, 15, 1, 13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7, 1, 10, 13, 0, 6,
        9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12,
    ],
    [
        7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15, 13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2,
        12, 1, 10, 14, 9, 10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4, 3, 15, 0, 6, 10, 1,
        13, 8, 9, 4, 5, 11, 12, 7, 2, 14,
    ],
    [
        2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9, 14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15,
        10, 3, 9, 8, 6, 4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14, 11, 8, 12, 7, 1, 14,
        2, 13, 6, 15, 0, 9, 10, 4, 5, 3,
    ],
    [
        12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11, 10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13,
        14, 0, 11, 3, 8, 9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6, 4, 3, 2, 12, 9, 5,
        15, 10, 11, 14, 1, 7, 6, 0, 8, 13,
    ],
    [
        4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1, 13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5,
        12, 2, 15, 8, 6, 1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2, 6, 11, 13, 8, 1, 4,
        10, 7, 9, 5, 0, 15, 14, 2, 3, 12,
    ],
    [
        13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7, 1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6,
        11, 0, 14, 9, 2, 7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8, 2, 1, 14, 7, 4, 10,
        8, 13, 15, 12, 9, 0, 3, 5, 6, 11,
    ],
];

/// Pick the bits of a `width`-bit `value` that `table` names, in order.
fn permute(value: u64, table: &[u8], width: u32) -> u64 {
    table.iter().fold(0, |out, &bit| {
        (out << 1) | ((value >> (width - u32::from(bit))) & 1)
    })
}

fn des_encrypt(key: u64, block: [u8; 8]) -> [u8; 8] {
    const HALF: u64 = 0x0fff_ffff;
    let key = permute(key, &PC1, 64);
    let (mut c, mut d) = (key >> 28, key & HALF);
    let block = permute(u64::from_be_bytes(block), &IP, 64);
    let (mut left, mut right) = (block >> 32, block & 0xffff_ffff);
    for shift in SHIFTS {
        c = ((c << shift) | (c >> (28 - shift))) & HALF;
        d = ((d << shift) | (d >> (28 - shift))) & HALF;
        let mixed = permute(right, &E, 32) ^ permute((c << 28) | d, &PC2, 56);
        let mut substituted = 0u64;
        for (i, sbox) in SBOX.iter().enumerate() {
            let six = (mixed >> (42 - 6 * i)) & 0x3f;
            let row = ((six >> 4) & 2) | (six & 1);
            let column = (six >> 1) & 0xf;
            substituted = (substituted << 4) | u64::from(sbox[(row * 16 + column) as usize]);
        }
        (left, right) = (right, left ^ permute(substituted, &P, 32));
    }
    permute((right << 32) | left, &FP, 64).to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn des_matches_the_standard_worked_example() {
        let cipher = des_encrypt(
            0x1334_5779_9bbc_dff1,
            0x0123_4567_89ab_cdef_u64.to_be_bytes(),
        );
        assert_eq!(hex(&cipher), "85e813540f0ab405");
    }

    // Both values are from an independent DES (pycryptodome) under the
    // bit-reversed password.
    #[test]
    fn a_response_is_both_challenge_halves_under_the_reversed_password() {
        let challenge: [u8; 16] = std::array::from_fn(|i| i as u8);
        assert_eq!(
            hex(&response("Ab3dEf7h", &challenge)),
            "4bac91fed2322a73783d686a4e9d49f5"
        );
        // A short password is padded with zero bytes.
        let challenge: [u8; 16] = std::array::from_fn(|i| 100 + i as u8);
        assert_eq!(
            hex(&response("abc", &challenge)),
            "632e6a4b187c61329bc77a4648bddf40"
        );
    }

    // The file every `vncpasswd` writes for the password "password".
    #[test]
    fn a_password_file_is_what_vncpasswd_writes() {
        assert_eq!(hex(&password_file("password")), "dbd83cfd727a1458");
    }

    #[test]
    fn a_new_password_is_eight_characters_a_viewer_can_be_given() {
        let password = new_password();
        assert!(is_password(&password), "{password}");
        assert!(!is_password("short"));
        assert!(!is_password("has spac"));
    }
}
