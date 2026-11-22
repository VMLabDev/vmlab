//! Parsing OCI references of the form `registry/owner/name[:tag]`
//! (PRD §6.4 addressing).
//!
//! vmlab **requires an explicit registry host** — there is no implicit
//! `docker.io` default. The first path segment is treated as the registry
//! host only when it looks like one: it contains a `.` (DNS name), a `:`
//! (host:port), or is exactly `localhost`. A bare `owner/name` is rejected
//! with a message asking for an explicit registry, so a tool that silently
//! reaches Docker Hub can never happen by accident.

use anyhow::{Result, bail};

/// A parsed registry reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reference {
    /// The registry host (e.g. `ghcr.io`, `localhost:5000`).
    pub host: String,
    /// The repository path under the host (e.g. `owner/name`). Always at
    /// least one segment; usually `owner/name`.
    pub repository: String,
    /// The tag (defaults to `latest` when omitted).
    pub tag: String,
}

const DEFAULT_TAG: &str = "latest";

impl Reference {
    /// Parse `[registry/]owner/name[:tag]`, requiring an explicit registry.
    pub fn parse(reference: &str) -> Result<Self> {
        let reference = reference.trim();
        if reference.is_empty() {
            bail!("empty registry reference");
        }

        // Split off the tag: the last ':' that is in the final path
        // segment (so `localhost:5000/x/y` keeps its port, but
        // `localhost:5000/x/y:1.2` splits the tag).
        let (path, tag) = match reference.rsplit_once('/') {
            Some((prefix, last)) => match last.split_once(':') {
                Some((name, tag)) => (format!("{prefix}/{name}"), tag.to_string()),
                None => (reference.to_string(), DEFAULT_TAG.to_string()),
            },
            None => {
                // No '/', so it cannot carry a registry + repository.
                bail!(
                    "reference `{reference}` has no registry — use an explicit host like \
                     `ghcr.io/owner/name[:tag]`"
                );
            }
        };

        let mut segments = path.splitn(2, '/');
        let host = segments.next().unwrap_or_default().to_string();
        let repository = match segments.next() {
            Some(rest) if !rest.is_empty() => rest.to_string(),
            _ => bail!("reference `{reference}` is missing a repository path"),
        };

        if !looks_like_registry_host(&host) {
            bail!(
                "reference `{reference}` has no explicit registry host (`{host}` does not look \
                 like one) — use e.g. `ghcr.io/{host}/{repository}` and never rely on a default \
                 registry"
            );
        }
        if tag.is_empty() {
            bail!("reference `{reference}` has an empty tag");
        }

        Ok(Reference {
            host,
            repository,
            tag,
        })
    }

    /// The repository with its host and without a tag
    /// (`host/owner/[group/]name`): who publishes what this reference names.
    pub fn source(&self) -> String {
        format!("{}/{}", self.host, self.repository)
    }

    /// The reference rendered back to canonical text.
    pub fn canonical(&self) -> String {
        format!("{}/{}:{}", self.host, self.repository, self.tag)
    }
}

/// Whether `segment` looks like a registry host: contains `.` or `:`, or is
/// exactly `localhost`.
fn looks_like_registry_host(segment: &str) -> bool {
    segment == "localhost" || segment.contains('.') || segment.contains(':')
}

/// Render `reference` with `version` as its tag (`host/repository:version`),
/// replacing any tag already present. vmlab publishes each template version as
/// a tag under a per-template repository (`host/owner/[group/]name:VERSION`),
/// so a single package holds every version of a template (PRD §6.4).
pub fn with_version_tag(reference: &str, version: &str) -> Result<String> {
    let r = Reference::parse(reference)?;
    Ok(format!("{}/{}:{}", r.host, r.repository, version))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_becomes_the_tag() {
        // the resolved version is set as the tag (any tag on the target ignored).
        assert_eq!(
            with_version_tag("ghcr.io/o/vmlab-templates/n", "1.2.3").unwrap(),
            "ghcr.io/o/vmlab-templates/n:1.2.3"
        );
        assert_eq!(
            with_version_tag("ghcr.io/o/n:ignored", "1.2.3").unwrap(),
            "ghcr.io/o/n:1.2.3"
        );
        // a host:port is not mistaken for a tag.
        assert_eq!(
            with_version_tag("localhost:5000/o/n", "1.2.3").unwrap(),
            "localhost:5000/o/n:1.2.3"
        );
    }

    #[test]
    fn parses_ghcr_with_tag() {
        let r = Reference::parse("ghcr.io/owner/name:1.2").unwrap();
        assert_eq!(r.host, "ghcr.io");
        assert_eq!(r.repository, "owner/name");
        assert_eq!(r.tag, "1.2");
        assert_eq!(r.canonical(), "ghcr.io/owner/name:1.2");
    }

    #[test]
    fn defaults_tag_to_latest() {
        let r = Reference::parse("ghcr.io/owner/name").unwrap();
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn localhost_with_port_and_tag() {
        let r = Reference::parse("localhost:5000/x/y:dev").unwrap();
        assert_eq!(r.host, "localhost:5000");
        assert_eq!(r.repository, "x/y");
        assert_eq!(r.tag, "dev");
    }

    #[test]
    fn localhost_with_port_no_tag() {
        let r = Reference::parse("localhost:5000/x/y").unwrap();
        assert_eq!(r.host, "localhost:5000");
        assert_eq!(r.repository, "x/y");
        assert_eq!(r.tag, "latest");
    }

    #[test]
    fn bare_localhost_ok() {
        let r = Reference::parse("localhost/x/y").unwrap();
        assert_eq!(r.host, "localhost");
        assert_eq!(r.repository, "x/y");
    }

    #[test]
    fn deep_repository_path() {
        let r = Reference::parse("harbor.example.com/team/project/image:v3").unwrap();
        assert_eq!(r.host, "harbor.example.com");
        assert_eq!(r.repository, "team/project/image");
        assert_eq!(r.tag, "v3");
    }

    /// The repository goes into the request path as written, and the URL
    /// parser removes dot segments from it, so a reference with one would
    /// name another repository than its text says (config review F2).
    #[test]
    fn a_repository_with_a_dot_segment_is_rejected() {
        for bad in [
            "ghcr.io/a/../b:1",
            "ghcr.io/a/./b:1",
            "ghcr.io/../b:1",
            "ghcr.io/a/..:1",
            "ghcr.io/a//b:1",
            "ghcr.io/a/b/:1",
            "ghcr.io/a/%2e%2e/b:1",
            "ghcr.io/a\\..\\b/c:1",
            "ghcr.io/a/b?x:1",
            "ghcr.io/a/b#x:1",
            "ghcr.io/a/b c:1",
        ] {
            let err = Reference::parse(bad).expect_err(bad);
            assert!(err.to_string().contains("repository"), "{bad}: {err}");
        }
        for good in [
            "ghcr.io/a/b:1",
            "ghcr.io/VMLabDev/vmlab-templates/windows-11:1",
            "ghcr.io/a/b.c_d-e/f..g:1",
        ] {
            Reference::parse(good).expect(good);
        }
    }

    #[test]
    fn bare_owner_name_rejected() {
        let err = Reference::parse("owner/name:1.2").unwrap_err();
        assert!(err.to_string().contains("explicit registry"), "{err}");
    }

    #[test]
    fn single_segment_rejected() {
        let err = Reference::parse("name").unwrap_err();
        assert!(err.to_string().contains("no registry"), "{err}");
    }

    #[test]
    fn empty_rejected() {
        assert!(Reference::parse("").is_err());
        assert!(Reference::parse("   ").is_err());
    }
}
