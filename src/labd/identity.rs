//! Who a person-invoked command runs as (PRD §19.2).
//!
//! **Precedence: CLI flag → wscript → `login {}` → agent identity.** The
//! first two arrive here as `user`/`password`; the third is the machine's
//! own declaration; the fourth is `None`, which the agent reads as its own
//! identity.
//!
//! The host resolves label → triple and the guest only ever sees the triple
//! (§19.5), so no vmlab label crosses the wire and `DOMAIN\user` never has
//! to survive an SSH username. Everything vmlab does on its own behalf —
//! provisioning, share mounting, readiness, metrics, shutdown — never calls
//! this and keeps the agent identity.

use anyhow::{Result, bail};
use vmlab_agent_proto::Logon;

use crate::config::model::{Login, default_login};
use crate::labd::guest_os::GuestOs;

/// The selector that means "the agent's own identity" on each guest family.
/// It needs no new spelling: `SYSTEM` and `root` *are* what the agent
/// already runs as, so the surfaces treat them as "spawn directly, no
/// logon" rather than inventing a pseudo-principal.
pub const WINDOWS_FLOOR: &str = "SYSTEM";
pub const LINUX_FLOOR: &str = "root";

/// Resolve who a command on `machine` runs as.
///
/// `user`/`password` are the CLI flags (or a wscript override); absent means
/// the machine's default `login {}`. `Ok(None)` is the agent identity.
///
/// Failure is loud and names the account **and** the machine: falling back
/// to the agent identity would leave commands mysteriously running as
/// SYSTEM and writing into `systemprofile` (§19.2).
pub fn resolve(
    machine: &str,
    logins: &[Login],
    guest_os: GuestOs,
    user: Option<&str>,
    password: Option<&str>,
) -> Result<Option<Logon>> {
    let Some(selector) = user else {
        // No flag: the machine's declared default, or the floor.
        return match default_login(logins) {
            Some(login) => Ok(Some(as_logon(machine, login, guest_os)?)),
            None => Ok(None),
        };
    };

    if is_floor(selector, guest_os) {
        return Ok(None);
    }

    // The label wins over the raw account name, so one account declared
    // twice at different elevation stays selectable (§19.2).
    let declared = logins
        .iter()
        .find(|l| l.label == selector)
        .or_else(|| logins.iter().find(|l| l.user == selector));
    if let Some(login) = declared {
        // A password on the flag overrides the declared secret without
        // needing a second declaration — the rotated-password case.
        return match password {
            Some(secret) => Ok(Some(Logon {
                user: login.user.clone(),
                secret: secret.to_string(),
                elevated: elevated(login.elevated, guest_os),
            })),
            None => Ok(Some(as_logon(machine, login, guest_os)?)),
        };
    }

    // An account the lab file never declared: a flag, not a schema addition.
    match password {
        Some(secret) => Ok(Some(Logon {
            user: selector.to_string(),
            secret: secret.to_string(),
            elevated: elevated(None, guest_os),
        })),
        None => bail!(
            "`{selector}` is not a login declared on machine `{machine}` — \
             pass --password to use an account the lab file does not declare, \
             or --user {} to run as the agent identity",
            floor(guest_os)
        ),
    }
}

/// Whether a selector names the agent's own identity on this guest family.
fn is_floor(selector: &str, guest_os: GuestOs) -> bool {
    selector.eq_ignore_ascii_case(floor(guest_os))
}

fn floor(guest_os: GuestOs) -> &'static str {
    match guest_os {
        GuestOs::Windows => WINDOWS_FLOOR,
        GuestOs::Linux => LINUX_FLOOR,
    }
}

fn as_logon(machine: &str, login: &Login, guest_os: GuestOs) -> Result<Logon> {
    let Some(secret) = login.password.clone() else {
        // §5.1 rejects this at validation on a Windows-family profile, so
        // reaching it means a Linux machine whose login has no password —
        // which the Linux session (#83) resolves without one.
        bail!(
            "login `{}` on machine `{machine}` declares no password, and account `{}` \
             cannot be logged on without one",
            login.label,
            login.user
        );
    };
    Ok(Logon {
        user: login.user.clone(),
        secret,
        elevated: elevated(login.elevated, guest_os),
    })
}

/// Fold a login's declared `elevated` into a value the guest can act on.
///
/// It defaults to true on Windows — the parity bar is devcontainers and a
/// devcontainer gives you root — and is nothing at all on Linux, where
/// declaring it is a §5.1 validation error, so only a caller that knows the
/// guest family can fold it in. `None` is both "not declared" and the
/// ad-hoc account a flag names.
fn elevated(declared: Option<bool>, guest_os: GuestOs) -> bool {
    match guest_os {
        GuestOs::Windows => declared.unwrap_or(true),
        GuestOs::Linux => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::Span;

    const NO_SPAN: Span = (0, 0);

    fn login(label: &str, user: &str, password: Option<&str>) -> Login {
        Login {
            label: label.into(),
            user: user.into(),
            password: password.map(str::to_string),
            elevated: None,
            default: None,
            span: NO_SPAN,
        }
    }

    fn declared() -> Vec<Login> {
        let mut dev = login("dev", r"PROBE\dev", Some("vmlab123!"));
        dev.default = Some(true);
        let mut standard = login("standard", r"PROBE\dev", Some("vmlab123!"));
        standard.elevated = Some(false);
        vec![
            dev,
            login("admin", r"PROBE\administrator", Some("adm1n!")),
            standard,
        ]
    }

    /// With nothing on the command line, a person-invoked verb is the
    /// machine's default login — that is the headline of §19.2.
    #[test]
    fn no_flag_takes_the_machines_default_login() {
        let got = resolve("dc01", &declared(), GuestOs::Windows, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(got.user, r"PROBE\dev");
        assert_eq!(got.secret, "vmlab123!");
        assert!(got.elevated, "elevation defaults to true on Windows");
    }

    /// A machine that declares nothing falls to the agent identity, and the
    /// tree it writes is SYSTEM-owned — still correct, because the attached
    /// session is SYSTEM too.
    #[test]
    fn a_machine_with_no_login_falls_to_the_agent_identity() {
        assert!(
            resolve("web01", &[], GuestOs::Windows, None, None)
                .unwrap()
                .is_none()
        );
    }

    /// The selector carries the *label*, and the label wins — which is what
    /// lets one account be declared twice at different elevation.
    #[test]
    fn a_label_selects_its_declaration_including_its_elevation() {
        let logins = declared();
        let admin = resolve("dc01", &logins, GuestOs::Windows, Some("admin"), None)
            .unwrap()
            .unwrap();
        assert_eq!(admin.user, r"PROBE\administrator");
        let standard = resolve("dc01", &logins, GuestOs::Windows, Some("standard"), None)
            .unwrap()
            .unwrap();
        assert_eq!(standard.user, r"PROBE\dev");
        assert!(!standard.elevated, "`elevated = false` must survive");
    }

    /// The raw account name is accepted as an alias for its label. Two
    /// labels name `PROBE\dev`; the alias resolves to the first declared,
    /// which is the one whose label it would have matched anyway.
    #[test]
    fn the_raw_account_name_is_an_alias_for_its_label() {
        let got = resolve(
            "dc01",
            &declared(),
            GuestOs::Windows,
            Some(r"PROBE\administrator"),
            None,
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.user, r"PROBE\administrator");
    }

    /// §19.2: `--user SYSTEM` and `--user root` *are* the agent's identity,
    /// so they need no new spelling and mint no logon.
    #[test]
    fn the_floor_is_the_agent_identity_on_each_family() {
        for selector in ["SYSTEM", "system", "SyStEm"] {
            assert!(
                resolve("dc01", &declared(), GuestOs::Windows, Some(selector), None)
                    .unwrap()
                    .is_none(),
                "{selector} must be the agent identity"
            );
        }
        assert!(
            resolve("web01", &declared(), GuestOs::Linux, Some("root"), None)
                .unwrap()
                .is_none()
        );
        // And each family's floor is only its own: `root` on Windows is an
        // ordinary account name, not the floor.
        assert!(
            resolve("dc01", &[], GuestOs::Windows, Some("root"), Some("pw"))
                .unwrap()
                .is_some_and(|l| l.user == "root")
        );
    }

    /// A second ad-hoc identity is a flag, not a schema addition: an
    /// account the lab file never declared needs its password with it.
    #[test]
    fn an_undeclared_account_needs_a_password_on_the_flag() {
        let got = resolve(
            "dc01",
            &declared(),
            GuestOs::Windows,
            Some(r"PROBE\audit"),
            Some("s3cret"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.user, r"PROBE\audit");
        assert_eq!(got.secret, "s3cret");
    }

    /// §19.2: a declared account that does not exist, or a selector nothing
    /// matches, fails naming the account **and** the machine — never a
    /// silent fallback to the agent identity.
    #[test]
    fn an_unknown_selector_fails_naming_the_account_and_the_machine() {
        let err = resolve("dc01", &declared(), GuestOs::Windows, Some("qa"), None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`qa`"), "{msg}");
        assert!(msg.contains("`dc01`"), "{msg}");
    }

    /// A password on the flag overrides the declared secret, so a rotated
    /// password does not need the lab file edited to attach once.
    #[test]
    fn a_password_flag_overrides_the_declared_secret() {
        let got = resolve(
            "dc01",
            &declared(),
            GuestOs::Windows,
            Some("dev"),
            Some("rotated"),
        )
        .unwrap()
        .unwrap();
        assert_eq!(got.user, r"PROBE\dev");
        assert_eq!(got.secret, "rotated");
    }

    /// A Linux login carries no elevation: root is root, and a non-root user
    /// is not elevatable without sudo — declaring it is a §5.1 error.
    #[test]
    fn a_linux_login_is_never_elevated() {
        let logins = vec![login("dev", "dev", Some("pw"))];
        let got = resolve("web01", &logins, GuestOs::Linux, None, None)
            .unwrap()
            .unwrap();
        assert!(!got.elevated);
    }

    /// A Linux login may declare no password at all; nothing can log it on
    /// until #83, so the refusal says which login and which machine rather
    /// than sending a secretless triple the guest cannot use.
    #[test]
    fn a_login_with_no_password_fails_naming_the_login_and_the_machine() {
        let logins = vec![login("dev", "dev", None)];
        let err = resolve("web01", &logins, GuestOs::Linux, None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("`dev`"), "{msg}");
        assert!(msg.contains("`web01`"), "{msg}");
    }
}
