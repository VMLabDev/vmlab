//! Resolve the spec's `user` field (`name[:group]` or `uid[:gid]`) against
//! the container's /etc/passwd and /etc/group. Pure string parsing, so it is
//! fully unit-testable on the host.

use std::fs;

use crate::util::Result;

/// A resolved container identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedUser {
    pub uid: u32,
    pub gid: u32,
    /// Home directory from passwd, when the user was found there (drives the
    /// HOME default in the container env).
    pub home: Option<String>,
}

/// One /etc/passwd row (the fields we care about).
struct PasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
}

fn parse_passwd(content: &str) -> Vec<PasswdEntry> {
    content
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split(':').collect();
            // name:passwd:uid:gid:gecos:home:shell
            if f.len() < 7 {
                return None;
            }
            Some(PasswdEntry {
                name: f[0].to_string(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().ok()?,
                home: f[5].to_string(),
            })
        })
        .collect()
}

/// Look up a group by name in /etc/group content → gid.
fn find_group(content: &str, name: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        let f: Vec<&str> = line.split(':').collect();
        // name:passwd:gid:members
        if f.len() >= 3 && f[0] == name {
            f[2].parse().ok()
        } else {
            None
        }
    })
}

/// Resolve `spec` (`name[:group]` / `uid[:gid]`, numeric parts accepted
/// directly) against passwd/group file *content*.
///
/// Like the OCI runtimes: a numeric uid absent from passwd is accepted as-is
/// with gid 0 (unless a group part says otherwise) and no home.
pub fn resolve_user(spec: &str, passwd: &str, group: &str) -> Result<ResolvedUser> {
    let (user_part, group_part) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    if user_part.is_empty() {
        return Err(format!("bad user spec {spec:?}: empty user"));
    }

    let entries = parse_passwd(passwd);
    let mut resolved = if let Ok(uid) = user_part.parse::<u32>() {
        match entries.iter().find(|e| e.uid == uid) {
            Some(e) => ResolvedUser {
                uid,
                gid: e.gid,
                home: Some(e.home.clone()),
            },
            None => ResolvedUser {
                uid,
                gid: 0,
                home: None,
            },
        }
    } else {
        let e = entries
            .iter()
            .find(|e| e.name == user_part)
            .ok_or(format!("user {user_part:?} not found in /etc/passwd"))?;
        ResolvedUser {
            uid: e.uid,
            gid: e.gid,
            home: Some(e.home.clone()),
        }
    };

    if let Some(g) = group_part {
        if g.is_empty() {
            return Err(format!("bad user spec {spec:?}: empty group"));
        }
        resolved.gid = if let Ok(gid) = g.parse::<u32>() {
            gid
        } else {
            find_group(group, g).ok_or(format!("group {g:?} not found in /etc/group"))?
        };
    }
    Ok(resolved)
}

/// [`resolve_user`] against the container rootfs's passwd/group files.
/// Missing files are treated as empty (numeric specs still work).
pub fn resolve_user_in_rootfs(spec: &str, rootfs: &str) -> Result<ResolvedUser> {
    let passwd = fs::read_to_string(format!("{rootfs}/etc/passwd")).unwrap_or_default();
    let group = fs::read_to_string(format!("{rootfs}/etc/group")).unwrap_or_default();
    resolve_user(spec, &passwd, &group)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          nginx:x:101:102:nginx:/var/lib/nginx:/sbin/nologin\n\
                          malformed-line\n";
    const GROUP: &str = "root:x:0:\nnginx:x:102:\nwww:x:33:nginx\n";

    #[test]
    fn resolves_by_name() {
        let u = resolve_user("nginx", PASSWD, GROUP).unwrap();
        assert_eq!(
            u,
            ResolvedUser {
                uid: 101,
                gid: 102,
                home: Some("/var/lib/nginx".into())
            }
        );
    }

    #[test]
    fn resolves_name_with_group_name() {
        let u = resolve_user("nginx:www", PASSWD, GROUP).unwrap();
        assert_eq!(u.uid, 101);
        assert_eq!(u.gid, 33);
    }

    #[test]
    fn resolves_numeric_uid_in_passwd() {
        let u = resolve_user("101", PASSWD, GROUP).unwrap();
        assert_eq!(u.gid, 102);
        assert_eq!(u.home.as_deref(), Some("/var/lib/nginx"));
    }

    #[test]
    fn numeric_uid_not_in_passwd_defaults_gid_0() {
        let u = resolve_user("4242", PASSWD, GROUP).unwrap();
        assert_eq!(
            u,
            ResolvedUser {
                uid: 4242,
                gid: 0,
                home: None
            }
        );
    }

    #[test]
    fn numeric_pair_bypasses_files() {
        let u = resolve_user("1000:1000", "", "").unwrap();
        assert_eq!(u.uid, 1000);
        assert_eq!(u.gid, 1000);
    }

    #[test]
    fn unknown_name_is_an_error() {
        assert!(resolve_user("ghost", PASSWD, GROUP).is_err());
        assert!(resolve_user("nginx:ghosts", PASSWD, GROUP).is_err());
        assert!(resolve_user("", PASSWD, GROUP).is_err());
        assert!(resolve_user("nginx:", PASSWD, GROUP).is_err());
    }
}
