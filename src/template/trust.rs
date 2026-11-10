//! Checks at the untrusted template boundary, before constructing store paths.
use anyhow::{Result, ensure};

use super::TemplateMeta;

pub(crate) fn component(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && (value.as_bytes()[0].is_ascii_lowercase() || value.as_bytes()[0].is_ascii_digit())
            && value
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
            && !value.contains(".."),
        "invalid template path component `{value}`: expected [a-z0-9][a-z0-9._-]*, at most 128 bytes, without .."
    );
    Ok(())
}

pub(crate) fn names(meta: &TemplateMeta) -> Result<()> {
    for value in [&meta.arch, &meta.name, &meta.version] {
        component(value)?;
    }
    for variant in &meta.variants {
        component(&variant.name)?;
    }
    Ok(())
}

pub(crate) fn scripts(meta: &TemplateMeta, trusted: bool) -> Result<()> {
    ensure!(
        trusted
            || (meta.first_boot_script.is_none()
                && meta.variants.iter().all(|v| v.first_boot_script.is_none())),
        "template first_boot_script refused: allow its exact registry repository in host.trusted_template_repositories; archives cannot establish registry trust"
    );
    Ok(())
}

pub(crate) fn repository_trusted(allowed: &[String], host: &str, repository: &str) -> bool {
    allowed
        .iter()
        .any(|name| name == &format!("{host}/{repository}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repository_trust_is_exact_and_default_denies() {
        assert!(!repository_trusted(&[], "ghcr.io", "org/template"));
        let allowed = vec!["ghcr.io/org/template".to_string()];
        assert!(repository_trusted(&allowed, "ghcr.io", "org/template"));
        for repo in [
            "org/template-evil",
            "org/template/sub",
            "org",
            "org/template:tag",
        ] {
            assert!(!repository_trusted(&allowed, "ghcr.io", repo));
        }
        assert!(!repository_trusted(
            &allowed,
            "evil.ghcr.io",
            "org/template"
        ));
    }

    #[test]
    fn path_components_refuse_escape_and_ambiguous_names() {
        for bad in [
            "", ".", "..", "a..b", "../x", "/tmp/x", "a/b", "a\\b", "A", "-x", "é", "a\0b",
        ] {
            assert!(component(bad).is_err(), "{bad:?}");
        }
        assert!(component(&"a".repeat(129)).is_err());
        for good in ["x86_64", "win-11", "1.2.3", "a_b.c-d"] {
            component(good).unwrap();
        }
        component(&"a".repeat(128)).unwrap();
    }
}
