//! Parsing container-image references with Docker shorthand normalisation.
//!
//! Unlike template references ([`crate::oci::reference`]), which always
//! require an explicit registry host, container-image references follow the
//! Docker conventions users already know: `nginx` means
//! `registry-1.docker.io/library/nginx:latest`, `owner/app:v1` means
//! `registry-1.docker.io/owner/app:v1`, and only a first path segment that
//! looks like a host (contains `.` or `:`, or is exactly `localhost` — the
//! same detection rule as `parse_template_ref`) is treated as one. A
//! `@sha256:<hex>` digest pins the exact manifest and wins over any tag.

use std::fmt;

use anyhow::{Result, bail};

/// The registry host Docker shorthand references resolve to. `docker.io`
/// and `index.docker.io` normalise to this — it is the host that actually
/// answers the distribution API for Docker Hub.
pub const DOCKER_HUB_HOST: &str = "registry-1.docker.io";

const DEFAULT_TAG: &str = "latest";

/// A parsed, normalised container-image reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageReference {
    /// The registry host (e.g. `registry-1.docker.io`, `localhost:5000`).
    pub host: String,
    /// The repository path under the host (e.g. `library/nginx`).
    pub repository: String,
    /// What to pull: a tag or a pinned manifest digest.
    pub selector: Selector,
}

/// The tag-or-digest part of an [`ImageReference`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// A mutable tag (`latest` when the reference names neither).
    Tag(String),
    /// A pinned `sha256:<hex>` manifest digest — content-addressed, so a
    /// cached image satisfies it with no registry round trip.
    Digest(String),
}

impl fmt::Display for ImageReference {
    /// Renders the normalised form (`host/repository:tag` or
    /// `host/repository@sha256:…`).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.selector {
            Selector::Tag(t) => write!(f, "{}/{}:{}", self.host, self.repository, t),
            Selector::Digest(d) => write!(f, "{}/{}@{}", self.host, self.repository, d),
        }
    }
}

/// Parse `[host/]repository[:tag][@sha256:<hex>]` with Docker shorthand
/// normalisation. When both a tag and a digest are present the digest wins
/// (the tag is display-only, exactly as `docker pull` treats it).
pub fn parse_image_reference(reference: &str) -> Result<ImageReference> {
    let reference = reference.trim();
    if reference.is_empty() {
        bail!("empty container image reference");
    }

    // 1. Split off a digest selector — it pins the manifest regardless of tag.
    let (rest, digest) = match reference.rsplit_once('@') {
        Some((r, d)) => {
            validate_digest(reference, d)?;
            (r, Some(d.to_string()))
        }
        None => (reference, None),
    };
    if rest.is_empty() {
        bail!("container image reference `{reference}` has no name");
    }

    // 2. Split off the tag: a ':' in the last path segment (so a
    // `localhost:5000/…` port is never mistaken for one).
    let (path, tag) = match rest.rsplit_once('/') {
        Some((prefix, last)) => match last.split_once(':') {
            Some((name, tag)) => (format!("{prefix}/{name}"), Some(tag.to_string())),
            None => (rest.to_string(), None),
        },
        None => match rest.split_once(':') {
            Some((name, tag)) => (name.to_string(), Some(tag.to_string())),
            None => (rest.to_string(), None),
        },
    };

    // 3. Host detection — same rule as template references: the first path
    // segment is a registry host only when it contains '.' or ':' or is
    // exactly `localhost`. Anything else is Docker Hub shorthand.
    let (host, mut repository) = match path.split_once('/') {
        Some((first, remainder)) if looks_like_registry_host(first) => {
            (normalise_host(first), remainder.to_string())
        }
        _ => (DOCKER_HUB_HOST.to_string(), path),
    };
    // Official images live under `library/` on Docker Hub.
    if host == DOCKER_HUB_HOST && !repository.contains('/') {
        repository = format!("library/{repository}");
    }

    validate_repository(reference, &repository)?;
    if let Some(t) = &tag {
        validate_tag(reference, t)?;
    }

    let selector = match digest {
        Some(d) => Selector::Digest(d),
        None => Selector::Tag(tag.unwrap_or_else(|| DEFAULT_TAG.to_string())),
    };
    Ok(ImageReference {
        host,
        repository,
        selector,
    })
}

/// Whether `segment` looks like a registry host: contains `.` or `:`, or is
/// exactly `localhost` (mirrors `parse_template_ref` / template references).
fn looks_like_registry_host(segment: &str) -> bool {
    segment == "localhost" || segment.contains('.') || segment.contains(':')
}

/// Docker Hub's user-facing host aliases all resolve to the registry host
/// that actually serves the distribution API.
fn normalise_host(host: &str) -> String {
    match host {
        "docker.io" | "index.docker.io" => DOCKER_HUB_HOST.to_string(),
        other => other.to_string(),
    }
}

/// A digest selector must be `sha256:` + exactly 64 lowercase hex chars.
fn validate_digest(reference: &str, digest: &str) -> Result<()> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        bail!(
            "container image reference `{reference}` has an unsupported digest \
             `{digest}` — only `sha256:<hex>` digests are supported"
        );
    };
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        bail!(
            "container image reference `{reference}` has a malformed digest \
             `{digest}` — expected 64 lowercase hex chars after `sha256:`"
        );
    }
    Ok(())
}

/// Repository paths are non-empty `/`-separated segments of lowercase
/// alphanumerics with interior `.`/`_`/`-` separators (the Docker rules,
/// slightly relaxed on separator runs).
fn validate_repository(reference: &str, repository: &str) -> Result<()> {
    if repository.is_empty() {
        bail!("container image reference `{reference}` has an empty repository");
    }
    for segment in repository.split('/') {
        let ok = !segment.is_empty()
            && segment.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
            })
            && segment.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
            && segment.ends_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit());
        if !ok {
            bail!(
                "container image reference `{reference}` has an invalid repository \
                 segment `{segment}` — segments are lowercase alphanumerics with \
                 interior `.`/`_`/`-`"
            );
        }
    }
    Ok(())
}

/// Tags are 1–128 chars of `[A-Za-z0-9_.-]`, not starting with `.` or `-`.
fn validate_tag(reference: &str, tag: &str) -> Result<()> {
    let ok = !tag.is_empty()
        && tag.len() <= 128
        && !tag.starts_with(['.', '-'])
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !ok {
        bail!("container image reference `{reference}` has an invalid tag `{tag}`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(s: &str) -> Selector {
        Selector::Tag(s.to_string())
    }

    #[test]
    fn bare_name_is_docker_hub_library_latest() {
        let r = parse_image_reference("nginx").unwrap();
        assert_eq!(r.host, DOCKER_HUB_HOST);
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.selector, tag("latest"));
        assert_eq!(r.to_string(), "registry-1.docker.io/library/nginx:latest");
    }

    #[test]
    fn owner_name_is_docker_hub() {
        let r = parse_image_reference("owner/app:v1").unwrap();
        assert_eq!(r.host, DOCKER_HUB_HOST);
        assert_eq!(r.repository, "owner/app");
        assert_eq!(r.selector, tag("v1"));
        assert_eq!(r.to_string(), "registry-1.docker.io/owner/app:v1");
    }

    #[test]
    fn explicit_hosts_pass_through() {
        let r = parse_image_reference("ghcr.io/owner/app:v2").unwrap();
        assert_eq!(r.host, "ghcr.io");
        assert_eq!(r.repository, "owner/app");
        assert_eq!(r.selector, tag("v2"));

        let r = parse_image_reference("localhost:5000/x/y:dev").unwrap();
        assert_eq!(r.host, "localhost:5000");
        assert_eq!(r.repository, "x/y");
        assert_eq!(r.selector, tag("dev"));

        let r = parse_image_reference("localhost/x").unwrap();
        assert_eq!(r.host, "localhost");
        assert_eq!(r.repository, "x", "no library/ outside Docker Hub");
        assert_eq!(r.selector, tag("latest"));

        let r = parse_image_reference("harbor.example.com/team/project/app").unwrap();
        assert_eq!(r.host, "harbor.example.com");
        assert_eq!(r.repository, "team/project/app");
    }

    #[test]
    fn docker_io_aliases_normalise() {
        for host in ["docker.io", "index.docker.io"] {
            let r = parse_image_reference(&format!("{host}/nginx:1.25")).unwrap();
            assert_eq!(r.host, DOCKER_HUB_HOST, "{host} should normalise");
            assert_eq!(r.repository, "library/nginx", "shorthand still expands");
            assert_eq!(r.selector, tag("1.25"));

            let r = parse_image_reference(&format!("{host}/owner/app")).unwrap();
            assert_eq!(r.host, DOCKER_HUB_HOST);
            assert_eq!(r.repository, "owner/app");
        }
    }

    #[test]
    fn digest_selector_parses_and_wins_over_tag() {
        let hex = "a".repeat(64);
        let digest = format!("sha256:{hex}");

        let r = parse_image_reference(&format!("nginx@{digest}")).unwrap();
        assert_eq!(r.repository, "library/nginx");
        assert_eq!(r.selector, Selector::Digest(digest.clone()));
        assert_eq!(
            r.to_string(),
            format!("registry-1.docker.io/library/nginx@{digest}")
        );

        // host + tag + digest: the digest wins, the tag is discarded.
        let r = parse_image_reference(&format!("ghcr.io/owner/app:v1@{digest}")).unwrap();
        assert_eq!(r.host, "ghcr.io");
        assert_eq!(r.repository, "owner/app");
        assert_eq!(r.selector, Selector::Digest(digest));
    }

    #[test]
    fn bad_digests_rejected() {
        let short = format!("nginx@sha256:{}", "a".repeat(63));
        assert!(parse_image_reference(&short).is_err());
        let upper = format!("nginx@sha256:{}", "A".repeat(64));
        assert!(parse_image_reference(&upper).is_err(), "uppercase hex");
        let nonhex = format!("nginx@sha256:{}", "g".repeat(64));
        assert!(parse_image_reference(&nonhex).is_err());
        let algo = format!("nginx@md5:{}", "a".repeat(64));
        assert!(parse_image_reference(&algo).is_err(), "unsupported algo");
        assert!(parse_image_reference("@sha256:").is_err());
    }

    #[test]
    fn invalid_names_rejected() {
        assert!(parse_image_reference("").is_err());
        assert!(parse_image_reference("   ").is_err());
        assert!(parse_image_reference("Nginx").is_err(), "uppercase repo");
        assert!(parse_image_reference("owner/App:v1").is_err());
        assert!(
            parse_image_reference("ghcr.io//app").is_err(),
            "empty segment"
        );
        assert!(parse_image_reference("ghcr.io/owner/-app").is_err());
        assert!(parse_image_reference("nginx:").is_err(), "empty tag");
        assert!(parse_image_reference("nginx:.bad").is_err());
        assert!(parse_image_reference("nginx:has space").is_err());
    }
}
