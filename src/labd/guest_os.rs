//! Guest OS family: what kind of operating system a machine runs.
//!
//! Its own module because [`super::machine::Machine`] reports it. Guest OS is
//! a property of the machine, and the interface is upstream of everything that
//! consumes one — it cannot take its vocabulary from [`super::playbook`],
//! which is one of those consumers (it picks a config-weave binary and path
//! scheme by it), any more than it could take it from the SMB mount planner
//! (which asks a related but different question under the name
//! [`crate::smb::OsHint`]).

/// Guest OS family, for callers that must shape a command line, a path or a
/// mount for one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOs {
    Linux,
    Windows,
}

/// Guest OS family from the resolved profile name, for selecting the
/// config-weave binary — where XP-versus-modern Windows is irrelevant, so
/// `windows-legacy` answers `Windows` here. Deliberately not the same
/// question as [`crate::smb::guest_os_hint`], which selects a share mount
/// command and for which the distinction matters a great deal. Containers are
/// always Linux; VM answers are confirmed against the agent handshake
/// (`AgentInfo.os`) once connected.
pub fn guest_os_of(profile: Option<&str>) -> GuestOs {
    match profile {
        Some(p) if p.starts_with("windows") => GuestOs::Windows,
        _ => GuestOs::Linux,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_profiles_are_windows() {
        assert_eq!(guest_os_of(Some("windows-server")), GuestOs::Windows);
        assert_eq!(guest_os_of(Some("windows-legacy")), GuestOs::Windows);
        assert_eq!(guest_os_of(Some("linux-modern")), GuestOs::Linux);
        assert_eq!(guest_os_of(None), GuestOs::Linux);
    }
}
