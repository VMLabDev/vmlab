//! Artefact cache (PRD §6.1): URL sources are downloaded to a cache and
//! verified against a required sha256 before use; local paths pass through.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::config::model::ArtefactSource;

/// `~/.local/share/vmlab/cache/artefacts`
pub fn cache_dir() -> PathBuf {
    crate::paths::data_dir().join("cache").join("artefacts")
}

/// Resolve an artefact source to a local file path, downloading + verifying
/// URL sources. Reports progress via `log`.
pub async fn resolve(source: &ArtefactSource, log: impl Fn(String)) -> Result<PathBuf> {
    resolve_in(&cache_dir(), source, log, DOWNLOAD_LIMITS).await
}

/// [`resolve`] against a cache directory and limits the caller names.
async fn resolve_in(
    dir: &Path,
    source: &ArtefactSource,
    log: impl Fn(String),
    limits: DownloadLimits,
) -> Result<PathBuf> {
    match source {
        ArtefactSource::Path { path, .. } => {
            if !path.is_file() {
                bail!("source file {} does not exist", path.display());
            }
            Ok(path.clone())
        }
        ArtefactSource::Url { url, sha256, .. } => {
            // The digest names the cache files, so it is checked here too,
            // for a source that did not come through the lab file.
            let sha256 = &digest_name(sha256)?;
            std::fs::create_dir_all(dir)?;
            // Cache key = the expected digest, so a re-download is skipped
            // when the verified artefact is already present.
            let cached = dir.join(format!("{sha256}.artefact"));
            let comp = Compression::from_url(url);
            match comp {
                // Uncompressed: the cached artefact's own hash is the key, so
                // we can re-verify it directly on a cache hit.
                None => {
                    if cached.is_file() && verify_file(&cached, sha256).await.is_ok() {
                        log(format!("using cached artefact {sha256}"));
                        return Ok(cached);
                    }
                    log(format!("downloading {url}"));
                    let dl = Scratch::new(dir, sha256);
                    download(url, dl.path(), limits).await?;
                    verify_file(dl.path(), sha256)
                        .await
                        .with_context(|| format!("hash mismatch for {url}"))?;
                    dl.publish(&cached)?;
                    log(format!("verified sha256 {sha256}"));
                }
                // Compressed: sha256 verifies the *download*, then we
                // decompress. The cached artefact is the decompressed image,
                // so its hash differs from the key — an `.ok` marker records
                // that a verified decompression already happened.
                Some(c) => {
                    let marker = dir.join(format!("{sha256}.ok"));
                    if cached.is_file() && marker.is_file() {
                        log(format!("using cached artefact {sha256}"));
                        return Ok(cached);
                    }
                    log(format!("downloading {url}"));
                    let dl = Scratch::new(dir, sha256);
                    download(url, dl.path(), limits).await?;
                    verify_file(dl.path(), sha256)
                        .await
                        .with_context(|| format!("hash mismatch for {url}"))?;
                    log(format!(
                        "verified sha256 {sha256}; decompressing ({})",
                        c.name()
                    ));
                    let image = Scratch::new(dir, sha256);
                    c.decompress(dl.path(), image.path()).await?;
                    image.publish(&cached)?;
                    std::fs::write(&marker, [])?;
                }
            }
            Ok(cached)
        }
    }
}

/// A `sha256` as the cache names files by it: 64 hex digits, in lower case.
/// Anything else is refused, because the value becomes a file name.
pub fn digest_name(sha256: &str) -> Result<String> {
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("`sha256` must be 64 hex digits, got `{sha256}`");
    }
    Ok(sha256.to_ascii_lowercase())
}

/// A file of the cache directory that one resolve writes and no other
/// resolve names: a download, or an image being decompressed. It gets its
/// digest-named place with [`Scratch::publish`], after it is verified, and
/// is removed when it does not get there.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(dir: &Path, sha256: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // The process id and a counter separate the resolves of this host.
        // The clock separates this process from an earlier one with its id.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        Scratch {
            path: dir.join(format!(".{sha256}.{pid}-{n}-{nanos:09}.part")),
        }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// Rename the file to `dest`. A rename replaces whole files, so a
    /// concurrent resolve that publishes the same artefact does no harm.
    fn publish(self, dest: &Path) -> Result<()> {
        std::fs::rename(&self.path, dest)
            .with_context(|| format!("moving the artefact to {}", dest.display()))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // After `publish` the name is gone and this is a no-op.
        std::fs::remove_file(&self.path).ok();
    }
}

/// Compression of a downloaded artefact, inferred from the URL extension.
/// Decompressed by shelling out (like qemu-img elsewhere) to avoid pulling
/// in codec crates.
#[derive(Clone, Copy)]
enum Compression {
    Xz,
    Gzip,
}

impl Compression {
    fn from_url(url: &str) -> Option<Self> {
        let path = url.split(['?', '#']).next().unwrap_or(url);
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".xz") {
            Some(Compression::Xz)
        } else if lower.ends_with(".gz") {
            Some(Compression::Gzip)
        } else {
            None
        }
    }

    fn name(self) -> &'static str {
        match self {
            Compression::Xz => "xz",
            Compression::Gzip => "gzip",
        }
    }

    /// Decompress `src` into `dest`, which the caller owns and publishes.
    async fn decompress(self, src: &Path, dest: &Path) -> Result<()> {
        let (prog, args): (&str, &[&str]) = match self {
            Compression::Xz => ("xz", &["--decompress", "--stdout"]),
            Compression::Gzip => ("gzip", &["--decompress", "--stdout"]),
        };
        let out =
            std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
        let status = tokio::process::Command::new(prog)
            .args(args)
            .arg(src)
            .stdout(std::process::Stdio::from(out))
            .status()
            .await
            .with_context(|| format!("running `{prog}` (is it installed?)"))?;
        if !status.success() {
            bail!("`{prog}` failed to decompress {}", src.display());
        }
        Ok(())
    }
}

/// How long an artefact download waits on a server that has stopped
/// talking: to connect, and then for the response head and between two
/// reads of the body. Neither bounds the whole download.
#[derive(Clone, Copy)]
struct DownloadLimits {
    connect: std::time::Duration,
    read: std::time::Duration,
    max_bytes: u64,
}

const DOWNLOAD_LIMITS: DownloadLimits = DownloadLimits {
    connect: std::time::Duration::from_secs(30),
    read: std::time::Duration::from_secs(60),
    max_bytes: 32 << 30,
};

/// Download `url` into `dest`. The caller verifies the file before it gives
/// it a name that another resolve reads.
async fn download(url: &str, dest: &Path, limits: DownloadLimits) -> Result<()> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    // A User-Agent is required: some CDNs (e.g. Fastly fronting
    // cloud.centos.org) answer 403 to requests with an empty UA.
    let client = reqwest::Client::builder()
        .user_agent(concat!("vmlab/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(limits.connect)
        .read_timeout(limits.read)
        .build()
        .context("building HTTP client")?;
    let resp = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GET {url} returned {}", resp.status());
    }
    let mut file = tokio::fs::File::create(dest).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

async fn verify_file(path: &Path, expected_hex: &str) -> Result<()> {
    let path = path.to_path_buf();
    let expected = expected_hex.to_ascii_lowercase();
    let actual = tokio::task::spawn_blocking(move || -> Result<String> {
        let f = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        crate::hashing::feed(f, &mut hasher)?;
        Ok(hex::encode(hasher.finalize()))
    })
    .await??;
    if actual != expected {
        bail!("sha256 mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review F10: a mirror that accepts the connection and sends nothing
    /// must fail the download, not hold it for ever.
    #[tokio::test]
    async fn a_mirror_that_never_answers_times_out() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (sock, _) = listener.accept().await.unwrap();
                held.push(sock);
            }
        });
        let tmp = tempfile::tempdir().unwrap();
        let limits = DownloadLimits {
            connect: std::time::Duration::from_secs(5),
            read: std::time::Duration::from_millis(300),
            ..DOWNLOAD_LIMITS
        };
        let done = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            download(
                &format!("http://{addr}/x.iso"),
                &tmp.path().join("x.iso"),
                limits,
            ),
        )
        .await
        .expect("the download must give up by itself");
        assert!(done.is_err(), "a silent mirror is an error");
    }

    /// What the loopback mirror below holds back, and for whom.
    struct Mirror {
        addr: std::net::SocketAddr,
        /// Sent to release the second half of the first connection's body.
        release_first: Option<tokio::sync::oneshot::Sender<()>>,
    }

    /// A loopback HTTP mirror that serves `body` to every connection. The
    /// first connection gets the head and the first half of the body, and the
    /// rest only when `release_first` fires. Later connections get it all.
    async fn mirror(body: Vec<u8>) -> Mirror {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (release_first, gate) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut gate = Some(gate);
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                let gate = gate.take();
                let body = body.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let (first, second) = body.split_at(body.len() / 2);
                    sock.write_all(head.as_bytes()).await.ok();
                    sock.write_all(first).await.ok();
                    sock.flush().await.ok();
                    if let Some(gate) = gate {
                        gate.await.ok();
                    }
                    sock.write_all(second).await.ok();
                    sock.shutdown().await.ok();
                });
            }
        });
        Mirror {
            addr,
            release_first: Some(release_first),
        }
    }

    /// A loopback HTTP mirror for review F2. It answers every connection
    /// with a head that declares `declared` bytes, or no length at all, and
    /// then sends 64 KiB blocks until the client closes or `sent_at_most`
    /// bytes are out. The bound keeps a client with no limit from filling
    /// the disk of the test.
    async fn flood_mirror(declared: Option<u64>, sent_at_most: u64) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let length = declared
                        .map(|n| format!("Content-Length: {n}\r\n"))
                        .unwrap_or_default();
                    let head = format!("HTTP/1.1 200 OK\r\n{length}Connection: close\r\n\r\n");
                    if sock.write_all(head.as_bytes()).await.is_err() {
                        return;
                    }
                    let block = [0x5au8; 64 * 1024];
                    let mut sent = 0u64;
                    while sent < sent_at_most {
                        if sock.write_all(&block).await.is_err() {
                            return;
                        }
                        sent += block.len() as u64;
                    }
                    sock.shutdown().await.ok();
                });
            }
        });
        addr
    }

    /// Review template-build F2: a mirror that sends data without end must
    /// not fill the filesystem of the cache. The download stops at its
    /// limit, and the cache keeps nothing of it.
    #[tokio::test]
    async fn a_download_that_never_ends_is_stopped_at_a_limit() {
        const LIMIT: u64 = 2 << 20;
        let limits = DownloadLimits {
            max_bytes: LIMIT,
            ..DOWNLOAD_LIMITS
        };
        let addr = flood_mirror(None, 16 * LIMIT).await;
        let tmp = tempfile::tempdir().unwrap();

        let part = tmp.path().join("x.part");
        let err = download(&format!("http://{addr}/x.iso"), &part, limits)
            .await
            .expect_err("a body past the limit is an error");
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        let written = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        assert!(written <= LIMIT, "{written} bytes written, limit {LIMIT}");

        let dir = tmp.path().join("artefacts");
        let source = url_source(format!("http://{addr}/x.iso"), &"0".repeat(64));
        let err = resolve_in(&dir, &source, |_| {}, limits)
            .await
            .expect_err("a body past the limit is an error");
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        assert_eq!(entries(&dir), Vec::<String>::new());
    }

    /// Review template-build F2: a declared length past the limit is
    /// refused from the head, before a byte of the body is written.
    #[tokio::test]
    async fn a_declared_length_past_the_limit_writes_nothing() {
        const LIMIT: u64 = 2 << 20;
        let limits = DownloadLimits {
            max_bytes: LIMIT,
            ..DOWNLOAD_LIMITS
        };
        let addr = flood_mirror(Some(LIMIT + 1), LIMIT + 1).await;
        let tmp = tempfile::tempdir().unwrap();
        let part = tmp.path().join("x.part");
        let err = download(&format!("http://{addr}/x.iso"), &part, limits)
            .await
            .expect_err("a declared length past the limit is an error");
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        let written = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
        assert_eq!(written, 0);
    }

    fn url_source(url: String, sha256: &str) -> ArtefactSource {
        ArtefactSource::Url {
            url,
            sha256: sha256.to_string(),
            span: (0, 0),
        }
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        hex::encode(Sha256::digest(bytes))
    }

    /// Every entry of `dir`, by file name.
    fn entries(dir: &Path) -> Vec<String> {
        let Ok(read) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let mut names: Vec<String> = read
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    /// Review F15, part 1: two builds that use one URL at the same time each
    /// download it. Neither may write into, or rename away, the file the
    /// other is still downloading.
    #[tokio::test]
    async fn two_resolves_of_one_url_both_succeed() {
        let body: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let half = (body.len() / 2) as u64;
        let mut mirror = mirror(body.clone()).await;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("artefacts");
        let source = url_source(format!("http://{}/x.iso", mirror.addr), &sha256_hex(&body));

        // The first resolve stops half way through its body.
        let first = {
            let (dir, source) = (dir.clone(), source.clone());
            tokio::spawn(async move { resolve_in(&dir, &source, |_| {}, DOWNLOAD_LIMITS).await })
        };
        // Let it put that half on disk. The bound is not a pass condition:
        // when it runs out the test goes on, and only proves less.
        for _ in 0..3000 {
            let written = std::fs::read_dir(&dir).into_iter().flatten().any(|e| {
                let e = e.unwrap();
                e.file_name().to_string_lossy().ends_with(".part")
                    && e.metadata().map(|m| m.len() >= half).unwrap_or(false)
            });
            if written {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        // The second runs from start to end while the first is held.
        let second = resolve_in(&dir, &source, |_| {}, DOWNLOAD_LIMITS).await;
        mirror.release_first.take().unwrap().send(()).unwrap();
        let first = first.await.unwrap();

        let second = second.expect("the second resolve");
        let first = first.expect("the first resolve");
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), body);
        assert_eq!(
            entries(&dir),
            vec![format!("{}.artefact", sha256_hex(&body))],
            "the cache holds the artefact and no leftover"
        );
    }

    /// Review F15, part 2: a download is checked before it gets its
    /// digest-named place, so a failed check leaves nothing behind.
    #[tokio::test]
    async fn a_download_that_fails_its_hash_leaves_no_cache_entry() {
        let mut mirror = mirror(b"not what the definition names".to_vec()).await;
        mirror.release_first.take().unwrap().send(()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("artefacts");
        for name in ["x.iso", "x.qcow2.gz"] {
            let source = url_source(format!("http://{}/{name}", mirror.addr), &"0".repeat(64));
            let err = resolve_in(&dir, &source, |_| {}, DOWNLOAD_LIMITS)
                .await
                .expect_err("a wrong hash is an error");
            assert!(format!("{err:#}").contains("hash mismatch"), "{err:#}");
            assert_eq!(entries(&dir), Vec::<String>::new(), "after {name}");
        }
    }

    /// Review F15, part 3: the cache names its files by the digest, so a
    /// `sha256` that is not one is refused before anything is fetched.
    #[tokio::test]
    async fn a_sha256_that_is_not_a_digest_writes_nothing() {
        let mut mirror = mirror(b"body".to_vec()).await;
        mirror.release_first.take().unwrap().send(()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cache").join("artefacts");
        let source = url_source(format!("http://{}/x.iso", mirror.addr), "../x");
        let err = resolve_in(&dir, &source, |_| {}, DOWNLOAD_LIMITS)
            .await
            .expect_err("`../x` is not a digest");
        assert!(format!("{err:#}").contains("64 hex digits"), "{err:#}");
        assert_eq!(entries(&tmp.path().join("cache")), Vec::<String>::new());
    }

    #[tokio::test]
    async fn local_path_passthrough() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x.iso");
        std::fs::write(&f, b"hello").unwrap();
        let src = ArtefactSource::Path {
            path: f.clone(),
            span: (0, 0),
        };
        let resolved = resolve(&src, |_| {}).await.unwrap();
        assert_eq!(resolved, f);
    }

    #[tokio::test]
    async fn missing_local_path_errors() {
        let src = ArtefactSource::Path {
            path: PathBuf::from("/no/such.iso"),
            span: (0, 0),
        };
        assert!(resolve(&src, |_| {}).await.is_err());
    }

    #[test]
    fn compression_from_url() {
        assert!(matches!(
            Compression::from_url("https://x/y/FreeBSD-15.0-amd64.qcow2.xz"),
            Some(Compression::Xz)
        ));
        assert!(matches!(
            Compression::from_url("https://x/img.qcow2.gz?a=1"),
            Some(Compression::Gzip)
        ));
        assert!(Compression::from_url("https://x/img.qcow2").is_none());
    }

    #[tokio::test]
    async fn verify_detects_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("x");
        std::fs::write(&f, b"hello").unwrap();
        // sha256("hello")
        let good = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";
        verify_file(&f, good).await.unwrap();
        assert!(verify_file(&f, &"0".repeat(64)).await.is_err());
    }
}
