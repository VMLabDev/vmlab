//! What preparing a Windows guest to serve a shared app over RDP decides
//! (PRD §20.1, §20.3, ADR-0016).
//!
//! The decisions are here, portable and testable with no guest: which
//! editions have no Remote Desktop host, the registry values a RemoteApp
//! session needs, whether one already holds what it should, and how an
//! account name reaches the account lookup. The Win32 calls that read and
//! write them stay with `windows/rdp.rs`.

/// One registry value the guest must hold before it can serve a RemoteApp
/// session, with the step an error names when writing it fails.
pub struct DwordSetting {
    pub step: &'static str,
    /// Under `HKEY_LOCAL_MACHINE`.
    pub key: &'static str,
    pub value: &'static str,
    pub wanted: u32,
}

/// Every DWORD `prepare` sets, in the order it sets them: Remote Desktop
/// on, then unlisted programs allowed — the policy value and the allow-list
/// switch both, since which one a given build honours is not documented.
pub const SETTINGS: &[DwordSetting] = &[
    DwordSetting {
        step: "turning Remote Desktop on",
        key: r"SYSTEM\CurrentControlSet\Control\Terminal Server",
        value: "fDenyTSConnections",
        wanted: 0,
    },
    DwordSetting {
        step: "allowing unlisted remote programs",
        key: r"SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services",
        value: "fAllowUnlistedRemotePrograms",
        wanted: 1,
    },
    DwordSetting {
        step: "disabling the RemoteApp allow list",
        key: r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Terminal Server\TSAppAllowList",
        value: "fDisabledAllowList",
        wanted: 1,
    },
];

/// Refuse an edition with no Remote Desktop host, by name.
///
/// Every Home edition's `EditionID` begins with `Core` (`Core`, `CoreN`,
/// `CoreSingleLanguage`, `CoreCountrySpecific`); Pro, Enterprise, Education
/// and every Server edition can host.
pub fn check_edition(edition_id: &str) -> Result<(), String> {
    if edition_id.starts_with("Core") {
        return Err(format!(
            "Windows `{edition_id}` has no Remote Desktop host; shared apps need Pro, \
             Enterprise, Education or Server"
        ));
    }
    Ok(())
}

/// Whether a DWORD must be written: it is absent, or holds something else.
pub fn differs(current: Option<u32>, wanted: u32) -> bool {
    current != Some(wanted)
}

/// Split `DOMAIN\user`, `user@domain` or a bare account into its domain and
/// user. `.` names this machine, so `.\user` is the bare local account.
///
/// The UPN form is left whole, as the logon mint leaves it: the account
/// lookup takes a UPN as it is, and its DNS suffix is not a NetBIOS domain.
pub fn split_account(account: &str) -> (Option<&str>, &str) {
    match account.split_once('\\') {
        Some((".", user)) => (None, user),
        Some((domain, user)) => (Some(domain), user),
        None => (None, account),
    }
}

/// The name the account lookup resolves to a SID: the account as written,
/// less a `.\` the lookup does not understand.
pub fn lookup_name(account: &str) -> String {
    match split_account(account) {
        (Some(domain), user) => format!(r"{domain}\{user}"),
        (None, user) => user.to_string(),
    }
}

/// Whether the account must be added to Remote Desktop Users: it is in
/// neither the local Administrators group nor that one. `groups` are the
/// account's local groups as the guest names them, and the two group names
/// are looked up from their SIDs, because they are localised.
pub fn needs_remote_desktop_users(groups: &[String], admins: &str, rdp_users: &str) -> bool {
    !groups.iter().any(|g| {
        // A group may come back qualified (`BUILTIN\Administrators`).
        let bare = g.rsplit_once('\\').map_or(g.as_str(), |(_, n)| n);
        bare.eq_ignore_ascii_case(admins) || bare.eq_ignore_ascii_case(rdp_users)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSAL_TAIL: &str =
        "has no Remote Desktop host; shared apps need Pro, Enterprise, Education or Server";

    /// §20.1: every Home edition is refused by its own name, in the one
    /// message a developer sees, before anything on the guest is touched.
    #[test]
    fn every_home_edition_is_refused_by_name() {
        for edition in ["Core", "CoreN", "CoreSingleLanguage", "CoreCountrySpecific"] {
            assert_eq!(
                check_edition(edition),
                Err(format!("Windows `{edition}` {REFUSAL_TAIL}")),
            );
        }
    }

    /// Pro, Enterprise, Education and Server editions — evaluation builds
    /// included — all host Remote Desktop and pass.
    #[test]
    fn editions_with_a_remote_desktop_host_pass() {
        for edition in [
            "Professional",
            "Enterprise",
            "EnterpriseEval",
            "Education",
            "ServerStandard",
            "ServerDatacenterEval",
        ] {
            assert_eq!(check_edition(edition), Ok(()), "{edition}");
        }
    }

    /// §20.3's three values, in order: Remote Desktop on before unlisted
    /// programs are allowed, both switches for the latter.
    #[test]
    fn the_settings_are_rdp_on_then_unlisted_programs_allowed() {
        let got: Vec<_> = SETTINGS
            .iter()
            .map(|s| (s.key.rsplit('\\').next().unwrap(), s.value, s.wanted))
            .collect();
        assert_eq!(
            got,
            [
                ("Terminal Server", "fDenyTSConnections", 0),
                ("Terminal Services", "fAllowUnlistedRemotePrograms", 1),
                ("TSAppAllowList", "fDisabledAllowList", 1),
            ]
        );
        assert!(SETTINGS.iter().all(|s| !s.step.is_empty()));
    }

    /// A value is written only when it differs: absent counts as differing,
    /// so a fresh guest gets it and a prepared one is left alone.
    #[test]
    fn a_value_is_written_only_when_it_differs() {
        assert!(differs(None, 0));
        assert!(differs(Some(1), 0));
        assert!(!differs(Some(0), 0));
        assert!(!differs(Some(1), 1));
    }

    /// The three account forms §19.2 accepts split as the logon mint splits
    /// them, and `.\user` is the bare local account.
    #[test]
    fn account_names_split_into_domain_and_user() {
        assert_eq!(split_account(r"PROBE\dev"), (Some("PROBE"), "dev"));
        assert_eq!(split_account("dev@probe.local"), (None, "dev@probe.local"));
        assert_eq!(split_account("dev"), (None, "dev"));
        assert_eq!(split_account(r".\dev"), (None, "dev"));
    }

    /// The lookup takes `DOMAIN\user`, a UPN and a bare name as written, and
    /// a `.\` prefix is dropped because the lookup would not resolve it.
    #[test]
    fn the_lookup_name_drops_only_a_local_dot() {
        assert_eq!(lookup_name(r"PROBE\dev"), r"PROBE\dev");
        assert_eq!(lookup_name("dev@probe.local"), "dev@probe.local");
        assert_eq!(lookup_name("dev"), "dev");
        assert_eq!(lookup_name(r".\dev"), "dev");
    }

    /// An administrator already may log on remotely, as may a member of
    /// Remote Desktop Users; anyone else is added. Group names compare
    /// case-insensitively and qualified or not.
    #[test]
    fn only_an_account_in_neither_group_needs_adding() {
        let (admins, rdp) = ("Administrators", "Remote Desktop Users");
        let groups = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(!needs_remote_desktop_users(
            &groups(&["Users", "Administrators"]),
            admins,
            rdp
        ));
        assert!(!needs_remote_desktop_users(
            &groups(&[r"BUILTIN\remote desktop users"]),
            admins,
            rdp
        ));
        assert!(needs_remote_desktop_users(&groups(&["Users"]), admins, rdp));
        assert!(needs_remote_desktop_users(&[], admins, rdp));
    }
}
