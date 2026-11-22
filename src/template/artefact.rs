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
    match source {
        ArtefactSource::Path { path, .. } => {
            if !path.is_file() {
                bail!("source file {} does not exist", path.display());
            }
            Ok(path.clone())
        }
        ArtefactSource::Url { url, sha256, .. } => {
            let dir = cache_dir();
            std::fs::create_dir_all(&dir)?;
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
                    download(url, &cached, DOWNLOAD_LIMITS).await?;
                    verify_file(&cached, sha256)
                        .await
                        .with_context(|| format!("hash mismatch for {url}"))?;
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
                    let dl = dir.join(format!("{sha256}.download"));
                    log(format!("downloading {url}"));
                    download(url, &dl, DOWNLOAD_LIMITS).await?;
                    verify_file(&dl, sha256)
                        .await
                        .with_context(|| format!("hash mismatch for {url}"))?;
                    log(format!(
                        "verified sha256 {sha256}; decompressing ({})",
                        c.name()
                    ));
                    c.decompress(&dl, &cached).await?;
                    std::fs::remove_file(&dl).ok();
                    std::fs::write(&marker, [])?;
                }
            }
            Ok(cached)
        }
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

    async fn decompress(self, src: &Path, dest: &Path) -> Result<()> {
        let (prog, args): (&str, &[&str]) = match self {
            Compression::Xz => ("xz", &["--decompress", "--stdout"]),
            Compression::Gzip => ("gzip", &["--decompress", "--stdout"]),
        };
        let tmp = dest.with_extension("decomp");
        let out =
            std::fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
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
        tokio::fs::rename(&tmp, dest).await?;
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
}

const DOWNLOAD_LIMITS: DownloadLimits = DownloadLimits {
    connect: std::time::Duration::from_secs(30),
    read: std::time::Duration::from_secs(60),
};

async fn download(url: &str, dest: &Path, limits: DownloadLimits) -> Result<()> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let tmp = dest.with_extension("part");
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
    let mut file = tokio::fs::File::create(&tmp).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, dest).await?;
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
