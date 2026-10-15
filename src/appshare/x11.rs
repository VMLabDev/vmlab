//! The host end of X11 forwarding (PRD §20.3, ADR-0016).
//!
//! Each X client the guest program opens arrives here as one unix stream. Its
//! connection setup carries the session's fake `MIT-MAGIC-COOKIE-1`, which the
//! CLI minted and the agent wrote into the guest's Xauthority. The proxy checks
//! it, swaps in the real cookie of the host's display, and splices the
//! connection to the host X server — the substitution OpenSSH's
//! `x11_open_helper` does, so the host cookie never enters the guest.

use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

/// The only authorization protocol a shared app's client may present.
const MIT_MAGIC_COOKIE: &[u8] = b"MIT-MAGIC-COOKIE-1";

/// The reason a setup failure reply gives a client with the wrong cookie.
const COOKIE_REFUSED: &str = "vmlab: invalid MIT-MAGIC-COOKIE-1 for this shared app";

/// Xauthority address families (libXau `Xauth.h`).
const FAMILY_INTERNET: u16 = 0;
const FAMILY_INTERNET6: u16 = 6;
const FAMILY_LOCAL: u16 = 256;
const FAMILY_WILD: u16 = 65535;

/// Where the host X server listens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Endpoint {
    /// A unix socket: the abstract name first when there is one, then the path.
    Local {
        abstract_name: Option<String>,
        path: PathBuf,
    },
    /// TCP to `host:6000+N`.
    Tcp { host: String, port: u16 },
}

/// A `DISPLAY` value taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Display {
    endpoint: Endpoint,
    /// The display number, which Xauthority entries are keyed by.
    number: u32,
}

/// Parse `DISPLAY`: `:N`, `:N.S`, `unix:N` and `/path/to/socket:N` are local,
/// `host:N` is TCP. The screen is irrelevant to a connection and dropped.
fn parse_display(display: &str) -> Result<Display> {
    let (host, rest) = display
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("DISPLAY {display:?} has no ':<number>'"))?;
    let digits = rest.split_once('.').map_or(rest, |(n, _)| n);
    let number: u32 = digits
        .parse()
        .with_context(|| format!("DISPLAY {display:?} has no display number"))?;
    let endpoint = if host.is_empty() || host == "unix" {
        let path = format!("/tmp/.X11-unix/X{number}");
        Endpoint::Local {
            abstract_name: Some(path.clone()),
            path: path.into(),
        }
    } else if host.starts_with('/') {
        // XQuartz's launchd socket is named for the whole display string.
        Endpoint::Local {
            abstract_name: None,
            path: format!("{host}:{digits}").into(),
        }
    } else {
        let port = u16::try_from(6000 + u64::from(number))
            .with_context(|| format!("DISPLAY {display:?} is past the last TCP port"))?;
        Endpoint::Tcp {
            host: host.to_string(),
            port,
        }
    };
    Ok(Display { endpoint, number })
}

/// One record of an Xauthority file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct XauthEntry {
    family: u16,
    address: Vec<u8>,
    number: Vec<u8>,
    name: Vec<u8>,
    data: Vec<u8>,
}

/// Parse an Xauthority file: big-endian records of a `u16` family and four
/// `u16`-length-prefixed fields. A truncated trailing record ends the list,
/// as it does for libXau's `XauReadAuth`.
fn parse_xauthority(mut bytes: &[u8]) -> Vec<XauthEntry> {
    fn u16_be(bytes: &mut &[u8]) -> Option<u16> {
        let (head, tail) = bytes.split_first_chunk::<2>()?;
        *bytes = tail;
        Some(u16::from_be_bytes(*head))
    }
    fn field(bytes: &mut &[u8]) -> Option<Vec<u8>> {
        let len = usize::from(u16_be(bytes)?);
        if bytes.len() < len {
            return None;
        }
        let (head, tail) = bytes.split_at(len);
        *bytes = tail;
        Some(head.to_vec())
    }
    let mut entries = Vec::new();
    while !bytes.is_empty() {
        let entry = (|| {
            Some(XauthEntry {
                family: u16_be(&mut bytes)?,
                address: field(&mut bytes)?,
                number: field(&mut bytes)?,
                name: field(&mut bytes)?,
                data: field(&mut bytes)?,
            })
        })();
        match entry {
            Some(entry) => entries.push(entry),
            None => break,
        }
    }
    entries
}

/// Who the host display's cookie is filed under, besides `FamilyWild`.
enum CookieKey<'a> {
    /// A local display: `FamilyLocal` with this host's name.
    Local { hostname: &'a str },
    /// A TCP display: an Internet entry carrying one of the host's addresses.
    Tcp { addresses: &'a [IpAddr] },
}

/// The first `MIT-MAGIC-COOKIE-1` entry for display `number` filed under
/// `key` or `FamilyWild` — the first match wins, as in libXau.
fn pick_cookie(entries: &[XauthEntry], number: u32, key: &CookieKey) -> Option<Vec<u8>> {
    let number = number.to_string();
    entries
        .iter()
        .find(|e| {
            e.name == MIT_MAGIC_COOKIE
                && e.number == number.as_bytes()
                && match (e.family, key) {
                    (FAMILY_WILD, _) => true,
                    (FAMILY_LOCAL, CookieKey::Local { hostname }) => {
                        e.address == hostname.as_bytes()
                    }
                    (FAMILY_INTERNET, CookieKey::Tcp { addresses }) => addresses
                        .iter()
                        .any(|a| matches!(a, IpAddr::V4(v4) if e.address == v4.octets())),
                    (FAMILY_INTERNET6, CookieKey::Tcp { addresses }) => addresses
                        .iter()
                        .any(|a| matches!(a, IpAddr::V6(v6) if e.address == v6.octets())),
                    _ => false,
                }
        })
        .map(|e| e.data.clone())
}

/// The host's X display: where to connect and the real cookie to present.
#[derive(Clone)]
pub struct HostDisplay {
    endpoint: Endpoint,
    /// The real `MIT-MAGIC-COOKIE-1`, or `None` to forward with empty auth and
    /// let the server's host-based access decide.
    cookie: Option<Vec<u8>>,
}

impl std::fmt::Debug for HostDisplay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The real cookie is a credential; say only whether there is one.
        f.debug_struct("HostDisplay")
            .field("endpoint", &self.endpoint)
            .field("cookie", &self.cookie.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl HostDisplay {
    /// From $DISPLAY and $XAUTHORITY (else ~/.Xauthority).
    pub fn from_env() -> Result<HostDisplay> {
        let display = std::env::var("DISPLAY")
            .ok()
            .filter(|d| !d.is_empty())
            .ok_or_else(|| anyhow!("DISPLAY is not set, so there is no host X server"))?;
        let xauthority = std::env::var_os("XAUTHORITY")
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join(".Xauthority")));
        Self::resolve(&display, xauthority.as_deref())
    }

    /// Resolve `display`, taking its cookie from the Xauthority file at
    /// `xauthority`. A missing file means no cookie, not an error.
    fn resolve(display: &str, xauthority: Option<&Path>) -> Result<HostDisplay> {
        let parsed = parse_display(display)?;
        let entries = match xauthority {
            Some(path) => match std::fs::read(path) {
                Ok(bytes) => parse_xauthority(&bytes),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
                Err(e) => {
                    return Err(e).with_context(|| format!("reading {}", path.display()));
                }
            },
            None => Vec::new(),
        };
        let cookie = match &parsed.endpoint {
            Endpoint::Local { .. } => {
                let hostname = hostname();
                pick_cookie(
                    &entries,
                    parsed.number,
                    &CookieKey::Local {
                        hostname: &hostname,
                    },
                )
            }
            Endpoint::Tcp { host, port } => {
                // An unresolvable host fails at connect; here it only means no
                // Internet entry can match, leaving a Wild one.
                let addresses: Vec<IpAddr> = (host.as_str(), *port)
                    .to_socket_addrs()
                    .map(|addrs| addrs.map(|a| a.ip()).collect())
                    .unwrap_or_default();
                pick_cookie(
                    &entries,
                    parsed.number,
                    &CookieKey::Tcp {
                        addresses: &addresses,
                    },
                )
            }
        };
        Ok(HostDisplay {
            endpoint: parsed.endpoint,
            cookie,
        })
    }
}

/// This host's name, as `FamilyLocal` Xauthority entries record it.
fn hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| std::fs::read_to_string("/etc/hostname"))
        .map(|h| h.trim().to_string())
        .unwrap_or_default()
}

/// 32 lowercase hex digits from the OS RNG.
pub fn mint_cookie() -> String {
    use rand::TryRng;
    let mut bytes = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes)
        .expect("the OS random number generator failed");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Decode a session cookie's hex digits into the bytes a client sends.
fn decode_cookie(cookie: &str) -> Result<Vec<u8>> {
    if !cookie.len().is_multiple_of(2) || !cookie.is_ascii() {
        bail!("session cookie {cookie:?} is not hex");
    }
    (0..cookie.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&cookie[i..i + 2], 16)
                .with_context(|| format!("session cookie {cookie:?} is not hex"))
        })
        .collect()
}

/// Equal in time independent of where the first difference lies.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// `n` rounded up to a multiple of four, as X pads every string.
fn pad4(n: usize) -> usize {
    (n + 3) & !3
}

/// The client's byte order, from the setup's first byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteOrder {
    Msb,
    Lsb,
}

impl ByteOrder {
    fn read(self, b: [u8; 2]) -> u16 {
        match self {
            ByteOrder::Msb => u16::from_be_bytes(b),
            ByteOrder::Lsb => u16::from_le_bytes(b),
        }
    }

    fn write(self, v: u16) -> [u8; 2] {
        match self {
            ByteOrder::Msb => v.to_be_bytes(),
            ByteOrder::Lsb => v.to_le_bytes(),
        }
    }
}

/// A client's connection setup prefix, up to the end of its auth data.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Setup {
    order: ByteOrder,
    major: u16,
    minor: u16,
    auth_name: Vec<u8>,
    auth_data: Vec<u8>,
}

/// Read exactly the setup prefix: the fixed 12 bytes, then the padded auth
/// name and data. Nothing past it is consumed, so requests a client pipelines
/// behind its setup stay in the stream for the splice.
async fn read_setup<R: AsyncRead + Unpin>(r: &mut R) -> Result<Setup> {
    let mut fixed = [0u8; 12];
    r.read_exact(&mut fixed)
        .await
        .context("reading the X11 connection setup")?;
    let order = match fixed[0] {
        b'B' => ByteOrder::Msb,
        b'l' => ByteOrder::Lsb,
        other => bail!("X11 connection setup has byte-order byte {other:#04x}"),
    };
    let major = order.read([fixed[2], fixed[3]]);
    let minor = order.read([fixed[4], fixed[5]]);
    let name_len = usize::from(order.read([fixed[6], fixed[7]]));
    let data_len = usize::from(order.read([fixed[8], fixed[9]]));
    let mut rest = vec![0u8; pad4(name_len) + pad4(data_len)];
    r.read_exact(&mut rest)
        .await
        .context("reading the X11 connection setup's authorization")?;
    let data_at = pad4(name_len);
    Ok(Setup {
        order,
        major,
        minor,
        auth_name: rest[..name_len].to_vec(),
        auth_data: rest[data_at..data_at + data_len].to_vec(),
    })
}

/// Encode a setup prefix carrying `auth` (name and data), or no
/// authorization at all, in the client's byte order.
fn encode_setup(setup: &Setup, auth: Option<(&[u8], &[u8])>) -> Vec<u8> {
    let (name, data) = auth.unwrap_or((&[], &[]));
    let o = setup.order;
    let mut out = Vec::with_capacity(12 + pad4(name.len()) + pad4(data.len()));
    out.push(match o {
        ByteOrder::Msb => b'B',
        ByteOrder::Lsb => b'l',
    });
    out.push(0);
    out.extend_from_slice(&o.write(setup.major));
    out.extend_from_slice(&o.write(setup.minor));
    // Both lengths came from a u16 field or are a fixed cookie's size.
    out.extend_from_slice(&o.write(name.len() as u16));
    out.extend_from_slice(&o.write(data.len() as u16));
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(name);
    out.resize(12 + pad4(name.len()), 0);
    out.extend_from_slice(data);
    out.resize(12 + pad4(name.len()) + pad4(data.len()), 0);
    out
}

/// A setup failure reply (`Failed`, protocol 11.0) giving `reason`, in the
/// client's byte order.
fn failure_reply(order: ByteOrder, reason: &str) -> Vec<u8> {
    let reason = &reason.as_bytes()[..reason.len().min(255)];
    let padded = pad4(reason.len());
    let mut out = Vec::with_capacity(8 + padded);
    out.push(0);
    out.push(reason.len() as u8);
    out.extend_from_slice(&order.write(11));
    out.extend_from_slice(&order.write(0));
    out.extend_from_slice(&order.write((padded / 4) as u16));
    out.extend_from_slice(reason);
    out.resize(8 + padded, 0);
    out
}

/// Check a client's setup against the session cookie. `Ok` is the prefix to
/// send the host server, carrying `host_cookie` or empty auth; `Err` is the
/// failure reply to send the client instead.
fn rewrite_setup(
    setup: &Setup,
    session_cookie: &[u8],
    host_cookie: Option<&[u8]>,
) -> std::result::Result<Vec<u8>, Vec<u8>> {
    let name_ok = setup.auth_name == MIT_MAGIC_COOKIE;
    // Evaluate both so a wrong name costs the same as a wrong cookie.
    let data_ok = constant_time_eq(&setup.auth_data, session_cookie);
    if !(name_ok & data_ok) {
        return Err(failure_reply(setup.order, COOKIE_REFUSED));
    }
    Ok(encode_setup(
        setup,
        host_cookie.map(|cookie| (MIT_MAGIC_COOKIE, cookie)),
    ))
}

/// Connect to the host X server.
async fn connect(endpoint: &Endpoint) -> Result<HostStream> {
    match endpoint {
        Endpoint::Local {
            abstract_name,
            path,
        } => {
            if let Some(name) = abstract_name
                && let Ok(stream) = connect_abstract(name)
            {
                return Ok(HostStream::Unix(stream));
            }
            let stream = UnixStream::connect(path).await.with_context(|| {
                format!("connecting to the host X server at {}", path.display())
            })?;
            Ok(HostStream::Unix(stream))
        }
        Endpoint::Tcp { host, port } => {
            let stream = tokio::net::TcpStream::connect((host.as_str(), *port))
                .await
                .with_context(|| format!("connecting to the host X server at {host}:{port}"))?;
            stream.set_nodelay(true).ok();
            Ok(HostStream::Tcp(stream))
        }
    }
}

/// Connect to the abstract socket `\0<name>`. A local connect does not block
/// on the peer, so the std call is safe on the runtime thread.
#[cfg(target_os = "linux")]
fn connect_abstract(name: &str) -> std::io::Result<UnixStream> {
    use std::os::linux::net::SocketAddrExt;
    let addr = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes())?;
    let stream = std::os::unix::net::UnixStream::connect_addr(&addr)?;
    stream.set_nonblocking(true)?;
    UnixStream::from_std(stream)
}

#[cfg(not(target_os = "linux"))]
fn connect_abstract(_name: &str) -> std::io::Result<UnixStream> {
    Err(std::io::ErrorKind::Unsupported.into())
}

/// The host server's end of a forwarded connection.
enum HostStream {
    Unix(UnixStream),
    Tcp(tokio::net::TcpStream),
}

/// Write `prefix` to `host`, then copy both ways until each side has closed,
/// passing each side's end-of-stream on as a half-close.
async fn splice<S: AsyncRead + AsyncWrite + Unpin>(
    guest: &mut UnixStream,
    host: &mut S,
    prefix: &[u8],
) -> Result<()> {
    host.write_all(prefix).await?;
    tokio::io::copy_bidirectional(guest, host).await?;
    Ok(())
}

/// Forward one guest X11 connection to the host display.
///
/// A connection whose setup does not carry `cookie` (the session's 32 hex
/// digits) as its `MIT-MAGIC-COOKIE-1` is answered with an X setup failure,
/// closed, and reported as an error; it never reaches the host server.
pub async fn proxy(mut guest: UnixStream, display: &HostDisplay, cookie: &str) -> Result<()> {
    let session_cookie = decode_cookie(cookie)?;
    let setup = read_setup(&mut guest).await?;
    let prefix = match rewrite_setup(&setup, &session_cookie, display.cookie.as_deref()) {
        Ok(prefix) => prefix,
        Err(reply) => {
            guest.write_all(&reply).await?;
            guest.shutdown().await.ok();
            bail!("refused an X11 connection: wrong MIT-MAGIC-COOKIE-1 for this shared app");
        }
    };
    match connect(&display.endpoint).await? {
        HostStream::Unix(mut host) => splice(&mut guest, &mut host, &prefix).await,
        HostStream::Tcp(mut host) => splice(&mut guest, &mut host, &prefix).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode one Xauthority record.
    fn xauth_record(
        family: u16,
        address: &[u8],
        number: &str,
        name: &[u8],
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = family.to_be_bytes().to_vec();
        for field in [address, number.as_bytes(), name, data] {
            out.extend_from_slice(&(field.len() as u16).to_be_bytes());
            out.extend_from_slice(field);
        }
        out
    }

    /// A client setup prefix as a real client sends it.
    fn client_setup(order: ByteOrder, name: &[u8], data: &[u8]) -> Vec<u8> {
        encode_setup(
            &Setup {
                order,
                major: 11,
                minor: 0,
                auth_name: Vec::new(),
                auth_data: Vec::new(),
            },
            Some((name, data)),
        )
    }

    const SESSION: [u8; 16] = [0x5a; 16];
    const HOST: [u8; 16] = [0xc3; 16];

    /// `:N` and `:N.S` name the local server, reached at its abstract socket
    /// and then its path socket; the screen does not change the connection.
    #[test]
    fn a_bare_display_number_is_the_local_server() {
        let local = |n: u32| Display {
            endpoint: Endpoint::Local {
                abstract_name: Some(format!("/tmp/.X11-unix/X{n}")),
                path: format!("/tmp/.X11-unix/X{n}").into(),
            },
            number: n,
        };
        assert_eq!(parse_display(":0").unwrap(), local(0));
        assert_eq!(parse_display(":12.1").unwrap(), local(12));
        assert_eq!(parse_display("unix:3").unwrap(), local(3));
        assert_eq!(parse_display("unix:3.0").unwrap(), local(3));
    }

    /// An XQuartz-style display is a socket path named for the whole string,
    /// with no abstract name to try.
    #[test]
    fn a_path_display_is_that_socket() {
        let d = parse_display("/private/tmp/com.apple.launchd.x/org.xquartz:0").unwrap();
        assert_eq!(
            d,
            Display {
                endpoint: Endpoint::Local {
                    abstract_name: None,
                    path: "/private/tmp/com.apple.launchd.x/org.xquartz:0".into(),
                },
                number: 0,
            }
        );
    }

    /// `host:N` is TCP to port 6000+N on that host.
    #[test]
    fn a_hostname_display_is_tcp_to_6000_plus_n() {
        let d = parse_display("example.test:10.0").unwrap();
        assert_eq!(
            d,
            Display {
                endpoint: Endpoint::Tcp {
                    host: "example.test".into(),
                    port: 6010,
                },
                number: 10,
            }
        );
    }

    /// A value with no display number is refused rather than guessed at.
    #[test]
    fn a_display_without_a_number_is_an_error() {
        assert!(parse_display("").is_err());
        assert!(parse_display("host").is_err());
        assert!(parse_display(":x").is_err());
    }

    /// Fixed bytes, written out by hand from libXau's record layout, parse to
    /// the entry they spell.
    #[test]
    fn an_xauthority_record_parses_from_its_big_endian_bytes() {
        #[rustfmt::skip]
        let bytes = [
            0x01, 0x00,                         // family 256, Local
            0x00, 0x04, b'h', b'o', b's', b't', // address
            0x00, 0x01, b'0',                   // number
            0x00, 0x02, b'N', b'M',             // name
            0x00, 0x03, 0xaa, 0xbb, 0xcc,       // data
        ];
        assert_eq!(
            parse_xauthority(&bytes),
            vec![XauthEntry {
                family: FAMILY_LOCAL,
                address: b"host".to_vec(),
                number: b"0".to_vec(),
                name: b"NM".to_vec(),
                data: vec![0xaa, 0xbb, 0xcc],
            }]
        );
    }

    /// A truncated trailing record ends the list without losing the records
    /// before it.
    #[test]
    fn a_truncated_xauthority_keeps_its_whole_records() {
        let mut bytes = xauth_record(FAMILY_WILD, b"", "0", MIT_MAGIC_COOKIE, &HOST);
        bytes.extend_from_slice(&[0x01, 0x00, 0x00, 0x09, b'h']);
        assert_eq!(parse_xauthority(&bytes).len(), 1);
    }

    /// For a local display, a `FamilyLocal` entry with this host's name and
    /// the display's number is the cookie; another host's entry and another
    /// display's entry are not.
    #[test]
    fn a_local_display_takes_the_local_entry_for_this_host() {
        let mut file = xauth_record(FAMILY_LOCAL, b"other", "0", MIT_MAGIC_COOKIE, &[1; 16]);
        file.extend(xauth_record(
            FAMILY_LOCAL,
            b"me",
            "1",
            MIT_MAGIC_COOKIE,
            &[2; 16],
        ));
        file.extend(xauth_record(
            FAMILY_LOCAL,
            b"me",
            "0",
            b"XDM-AUTHORIZATION-1",
            &[3; 16],
        ));
        file.extend(xauth_record(
            FAMILY_LOCAL,
            b"me",
            "0",
            MIT_MAGIC_COOKIE,
            &HOST,
        ));
        let entries = parse_xauthority(&file);
        let key = CookieKey::Local { hostname: "me" };
        assert_eq!(pick_cookie(&entries, 0, &key), Some(HOST.to_vec()));
        assert_eq!(pick_cookie(&entries, 1, &key), Some(vec![2; 16]));
        assert_eq!(pick_cookie(&entries, 5, &key), None);
    }

    /// A `FamilyWild` entry matches whatever the host is called and however
    /// the display is reached.
    #[test]
    fn a_wild_entry_matches_any_address() {
        let file = xauth_record(FAMILY_WILD, b"", "7", MIT_MAGIC_COOKIE, &HOST);
        let entries = parse_xauthority(&file);
        assert_eq!(
            pick_cookie(&entries, 7, &CookieKey::Local { hostname: "me" }),
            Some(HOST.to_vec())
        );
        assert_eq!(
            pick_cookie(&entries, 7, &CookieKey::Tcp { addresses: &[] }),
            Some(HOST.to_vec())
        );
    }

    /// A TCP display takes the Internet entry carrying one of the host's
    /// addresses, and never a `FamilyLocal` one.
    #[test]
    fn a_tcp_display_takes_the_internet_entry_for_its_address() {
        let mut file = xauth_record(FAMILY_LOCAL, b"me", "2", MIT_MAGIC_COOKIE, &[1; 16]);
        file.extend(xauth_record(
            FAMILY_INTERNET,
            &[10, 0, 0, 9],
            "2",
            MIT_MAGIC_COOKIE,
            &[2; 16],
        ));
        file.extend(xauth_record(
            FAMILY_INTERNET,
            &[10, 0, 0, 1],
            "2",
            MIT_MAGIC_COOKIE,
            &HOST,
        ));
        let entries = parse_xauthority(&file);
        let addresses = ["10.0.0.1".parse().unwrap()];
        assert_eq!(
            pick_cookie(
                &entries,
                2,
                &CookieKey::Tcp {
                    addresses: &addresses
                }
            ),
            Some(HOST.to_vec())
        );
    }

    /// A missing Xauthority file is no cookie, so the connection is forwarded
    /// with empty auth for the server's host access to judge.
    #[test]
    fn a_missing_xauthority_file_is_no_cookie() {
        let d = HostDisplay::resolve(":0", Some(Path::new("/nonexistent/.Xauthority"))).unwrap();
        assert_eq!(d.cookie, None);
    }

    /// A minted cookie is 32 lowercase hex digits, and two are not the same.
    #[test]
    fn a_minted_cookie_is_32_lowercase_hex_digits() {
        let a = mint_cookie();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')), "{a}");
        assert_ne!(a, mint_cookie());
        assert_eq!(decode_cookie(&a).unwrap().len(), 16);
    }

    /// In both byte orders the session cookie is swapped for the host's with
    /// lengths and padding in the client's order, and the client's protocol
    /// version is kept.
    #[tokio::test]
    async fn the_setup_is_rewritten_with_the_host_cookie_in_either_byte_order() {
        for order in [ByteOrder::Msb, ByteOrder::Lsb] {
            let wire = client_setup(order, MIT_MAGIC_COOKIE, &SESSION);
            let setup = read_setup(&mut wire.as_slice()).await.unwrap();
            assert_eq!(setup.auth_data, SESSION);
            let out = rewrite_setup(&setup, &SESSION, Some(&HOST)).unwrap();
            let mut expected = vec![if order == ByteOrder::Msb { b'B' } else { b'l' }, 0];
            for v in [11u16, 0, 18, 16] {
                expected.extend_from_slice(&order.write(v));
            }
            expected.extend_from_slice(&[0, 0]);
            expected.extend_from_slice(MIT_MAGIC_COOKIE);
            expected.extend_from_slice(&[0, 0]);
            expected.extend_from_slice(&HOST);
            assert_eq!(out, expected, "{order:?}");
        }
    }

    /// With no host cookie the setup goes on with both auth lengths zero.
    #[tokio::test]
    async fn with_no_host_cookie_the_setup_carries_empty_auth() {
        let wire = client_setup(ByteOrder::Lsb, MIT_MAGIC_COOKIE, &SESSION);
        let setup = read_setup(&mut wire.as_slice()).await.unwrap();
        let out = rewrite_setup(&setup, &SESSION, None).unwrap();
        assert_eq!(out, [b'l', 0, 11, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    /// Reading the setup stops at its end, leaving a pipelined request for
    /// the splice.
    #[tokio::test]
    async fn reading_the_setup_leaves_what_follows_it() {
        let mut wire = client_setup(ByteOrder::Msb, MIT_MAGIC_COOKIE, &SESSION);
        wire.extend_from_slice(b"next");
        let mut r = wire.as_slice();
        read_setup(&mut r).await.unwrap();
        assert_eq!(r, b"next");
    }

    /// A wrong cookie, a wrong-length cookie or another auth protocol each
    /// get a well-formed setup failure naming vmlab, in the client's order.
    #[tokio::test]
    async fn a_wrong_cookie_gets_the_setup_failure_reply() {
        for (order, name, data) in [
            (ByteOrder::Msb, MIT_MAGIC_COOKIE, &[0u8; 16][..]),
            (ByteOrder::Lsb, MIT_MAGIC_COOKIE, &SESSION[..8]),
            (ByteOrder::Lsb, &b"XDM-AUTHORIZATION-1"[..], &SESSION[..]),
        ] {
            let wire = client_setup(order, name, data);
            let setup = read_setup(&mut wire.as_slice()).await.unwrap();
            let reply = rewrite_setup(&setup, &SESSION, Some(&HOST)).unwrap_err();
            assert_failure_reply(&reply, order);
        }
    }

    /// Check `reply` is a setup failure carrying vmlab's reason.
    fn assert_failure_reply(reply: &[u8], order: ByteOrder) {
        let n = usize::from(reply[1]);
        assert_eq!(reply[0], 0, "Failed");
        assert_eq!(n, COOKIE_REFUSED.len());
        assert_eq!(order.read([reply[2], reply[3]]), 11);
        assert_eq!(usize::from(order.read([reply[6], reply[7]])), pad4(n) / 4);
        assert_eq!(reply.len(), 8 + pad4(n));
        assert_eq!(&reply[8..8 + n], COOKIE_REFUSED.as_bytes());
    }

    /// Through `proxy`, a wrong cookie is answered and refused before any
    /// host server is contacted — this display does not exist.
    #[tokio::test]
    async fn proxy_refuses_a_wrong_cookie_without_touching_the_host() {
        let display = HostDisplay {
            endpoint: Endpoint::Local {
                abstract_name: None,
                path: "/nonexistent/X0".into(),
            },
            cookie: None,
        };
        let (guest, mut client) = UnixStream::pair().unwrap();
        let task = tokio::spawn(async move { proxy(guest, &display, &"5a".repeat(16)).await });
        client
            .write_all(&client_setup(ByteOrder::Lsb, MIT_MAGIC_COOKIE, &[0; 16]))
            .await
            .unwrap();
        let mut reply = Vec::new();
        client.read_to_end(&mut reply).await.unwrap();
        assert_failure_reply(&reply, ByteOrder::Lsb);
        let err = task.await.unwrap().unwrap_err();
        assert!(
            err.to_string().contains("wrong MIT-MAGIC-COOKIE-1"),
            "{err}"
        );
    }

    /// Send a setup through `proxy` to `display` and return the server's
    /// setup reply, whatever its status.
    async fn live_setup(display: HostDisplay, session: &str, presented: &[u8]) -> Vec<u8> {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            live_setup_inner(display, session, presented),
        )
        .await
        .expect("no setup reply within 10s")
    }

    async fn live_setup_inner(display: HostDisplay, session: &str, presented: &[u8]) -> Vec<u8> {
        let (guest, mut client) = UnixStream::pair().unwrap();
        let session = session.to_string();
        tokio::spawn(async move { proxy(guest, &display, &session).await });
        client
            .write_all(&client_setup(ByteOrder::Lsb, MIT_MAGIC_COOKIE, presented))
            .await
            .unwrap();
        let mut head = [0u8; 8];
        client.read_exact(&mut head).await.unwrap();
        let extra = usize::from(u16::from_le_bytes([head[6], head[7]])) * 4;
        let mut body = vec![0u8; extra];
        client.read_exact(&mut body).await.unwrap();
        let mut reply = head.to_vec();
        reply.extend(body);
        reply
    }

    /// A real Xvfb holding only its own cookie accepts a client presenting the
    /// session cookie through the proxy, and the proxy refuses a wrong one.
    #[tokio::test]
    #[ignore = "needs Xvfb and xauth — run with `cargo test --lib appshare::x11 -- --ignored`"]
    async fn xvfb_accepts_the_session_cookie_through_the_proxy() {
        let dir = std::env::temp_dir().join(format!("vmlab-x11-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let auth = dir.join("Xauthority");
        let host_hex = mint_cookie();
        let status = std::process::Command::new("xauth")
            .arg("-f")
            .arg(&auth)
            .args(["add", ":97", "MIT-MAGIC-COOKIE-1", &host_hex])
            .status()
            .unwrap();
        assert!(status.success());
        let mut xvfb = std::process::Command::new("Xvfb")
            .arg(":97")
            .arg("-auth")
            .arg(&auth)
            // No reset when a client leaves: the next one would race it.
            .args(["-nolisten", "tcp", "-noreset"])
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..100 {
            if Path::new("/tmp/.X11-unix/X97").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let display = HostDisplay::resolve(":97", Some(&auth)).unwrap();
        assert!(display.cookie.is_some(), "the xauth entry was not picked");

        let session = mint_cookie();
        let ok = live_setup(display.clone(), &session, &decode_cookie(&session).unwrap()).await;
        let refused = live_setup(display.clone(), &session, &[0; 16]).await;
        // Without the substituted host cookie Xvfb itself says no, so the
        // success above is the swap's doing.
        let no_host_cookie = HostDisplay {
            cookie: None,
            ..display
        };
        let unauthorized =
            live_setup(no_host_cookie, &session, &decode_cookie(&session).unwrap()).await;
        xvfb.kill().ok();
        xvfb.wait().ok();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(ok[0], 1, "Xvfb answered {:?}", String::from_utf8_lossy(&ok));
        println!("xvfb: correct cookie -> first reply byte {}", ok[0]);
        assert_eq!(
            unauthorized[0], 0,
            "Xvfb accepted a connection with no cookie"
        );
        println!(
            "xvfb: empty host auth -> first reply byte 0, {:?}",
            String::from_utf8_lossy(&unauthorized[8..8 + usize::from(unauthorized[1])])
        );
        assert_failure_reply(&refused, ByteOrder::Lsb);
        println!(
            "xvfb: wrong cookie -> {:?}",
            String::from_utf8_lossy(&refused[8..8 + usize::from(refused[1])])
        );
    }

    /// This host's own display, from `DISPLAY` and `XAUTHORITY`, accepts a
    /// client presenting the session cookie through the proxy.
    #[tokio::test]
    #[ignore = "needs a host X server — run with DISPLAY and XAUTHORITY set"]
    async fn the_host_display_accepts_the_session_cookie_through_the_proxy() {
        let display = HostDisplay::from_env().unwrap();
        println!("host display: {display:?}");
        let session = mint_cookie();
        let ok = live_setup(display.clone(), &session, &decode_cookie(&session).unwrap()).await;
        assert_eq!(
            ok[0],
            1,
            "the server answered {:?}",
            String::from_utf8_lossy(&ok)
        );
        println!("host: correct cookie -> first reply byte {}", ok[0]);
        let refused = live_setup(display, &session, &[0; 16]).await;
        assert_failure_reply(&refused, ByteOrder::Lsb);
        println!(
            "host: wrong cookie -> {:?}",
            String::from_utf8_lossy(&refused[8..8 + usize::from(refused[1])])
        );
    }
}
