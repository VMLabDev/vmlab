//! Which build this `vmlab` binary is, and the refusal to drive a daemon of
//! another one.
//!
//! The CLI, the supervisor and every lab daemon are one binary, and the
//! daemons outlive the `vmlab` process that started them. Install a new
//! vmlab while they run and the next `vmlab up` drives a daemon of the old
//! build from a CLI of the new one — which showed up as a `vmlab.wcl` digest
//! that "changed" between two builds that hashed it differently, and would
//! show up as anything two builds disagree on. The wire (ADR-0007) carries
//! no protocol version to say when two builds are compatible, so the build
//! itself is the unit: a daemon of another build is refused, by name, with
//! the one fix there is.

/// This binary's build: the crate version, plus the git revision it was built
/// from where there was one (`0.31.0+1a2b3c4`). What every daemon answers
/// `version` with, and what the CLI compares it against. Stamped by
/// `build.rs`.
pub const BUILD: &str = env!("VMLAB_BUILD");

/// Why `mine` must not drive a daemon reporting `theirs` — `None` when they
/// are the same build. `theirs` is `None` for a daemon that could not answer
/// `version` at all: one from before the check existed, which is not this
/// build either.
pub fn mismatch(what: &str, mine: &str, theirs: Option<&str>) -> Option<String> {
    let theirs = match theirs {
        Some(theirs) if theirs == mine => return None,
        Some(theirs) => format!("vmlab build {theirs}"),
        None => "an older vmlab build, from before daemons reported their build".to_string(),
    };
    Some(format!(
        "{what} is {theirs}, but this vmlab is build {mine} — one build cannot drive \
         another's daemons. Run `vmlab daemon stop` (which stops every lab daemon and the \
         machines they run; disks and snapshots stay), then this command again"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same build is no mismatch; a different one is refused with both
    /// builds named and the fix spelled out, and so is a daemon too old to
    /// say what it is.
    #[test]
    fn another_build_is_refused_by_name_with_the_fix() {
        assert_eq!(
            mismatch("vmlabd", "0.31.0+abc1234", Some("0.31.0+abc1234")),
            None
        );
        let msg = mismatch("lab \"t\"'s daemon", "0.31.0+abc1234", Some("0.30.0-alpha"))
            .expect("refused");
        assert!(
            msg.contains("lab \"t\"'s daemon is vmlab build 0.30.0-alpha"),
            "{msg}"
        );
        assert!(msg.contains("this vmlab is build 0.31.0+abc1234"), "{msg}");
        assert!(msg.contains("vmlab daemon stop"), "{msg}");
        let old = mismatch("vmlabd", "0.31.0+abc1234", None).expect("refused");
        assert!(old.contains("is an older vmlab build"), "{old}");
        assert!(old.contains("vmlab daemon stop"), "{old}");
    }

    /// Two developer builds are both `0.1.0`; the revision is what tells
    /// them apart, so a build string carries one where git could supply it.
    #[test]
    fn this_build_is_stamped() {
        assert!(BUILD.starts_with(env!("CARGO_PKG_VERSION")), "{BUILD}");
    }
}
