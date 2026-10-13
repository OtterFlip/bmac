// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Who may connect to the engine socket, decided by the peer's UID from
//! `SO_PEERCRED`: the front end's user and root (the `bmac` CLI).

use std::collections::BTreeSet;
use std::path::Path;

pub const WEB_USER: &str = "bmac-web";

/// A user from `/etc/passwd`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
}

/// Finds `name` in the text of a passwd file.
pub fn find_account(passwd: &str, name: &str) -> Option<Account> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        if fields.next()? != name {
            return None;
        }
        let _password = fields.next()?;
        let uid = fields.next()?.parse().ok()?;
        let gid = fields.next()?.parse().ok()?;
        Some(Account { uid, gid })
    })
}

pub fn lookup_account(passwd: &Path, name: &str) -> std::io::Result<Account> {
    let text = std::fs::read_to_string(passwd)?;
    find_account(&text, name).ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("user {name} is not in {}", passwd.display()),
        )
    })
}

/// The UIDs the engine answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllowedPeers(BTreeSet<u32>);

impl AllowedPeers {
    pub fn new(uids: impl IntoIterator<Item = u32>) -> AllowedPeers {
        AllowedPeers(uids.into_iter().collect())
    }

    /// The front end's user and root.
    pub fn front_end_and_root(web: Account) -> AllowedPeers {
        AllowedPeers::new([0, web.uid])
    }

    pub fn allows(&self, uid: u32) -> bool {
        self.0.contains(&uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/bash\n\
        bmac-web-old:x:997:997::/:/usr/sbin/nologin\n\
        bmac-web:x:998:996:BMAC HTTPS front end:/:/usr/sbin/nologin\n\
        broken:x:abc:1::/:/bin/false\n";

    #[test]
    fn finds_accounts_by_exact_name() {
        assert_eq!(
            find_account(PASSWD, "bmac-web"),
            Some(Account { uid: 998, gid: 996 })
        );
        assert_eq!(
            find_account(PASSWD, "root"),
            Some(Account { uid: 0, gid: 0 })
        );
        assert_eq!(find_account(PASSWD, "bmac"), None);
        assert_eq!(find_account(PASSWD, "broken"), None);
    }

    #[test]
    fn allows_only_listed_uids() {
        let allowed = AllowedPeers::front_end_and_root(Account { uid: 998, gid: 996 });
        assert!(allowed.allows(0));
        assert!(allowed.allows(998));
        assert!(!allowed.allows(997));
        assert!(!allowed.allows(1000));
    }

    #[test]
    fn lookup_reports_a_missing_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("passwd");
        std::fs::write(&path, PASSWD).unwrap();
        assert_eq!(lookup_account(&path, "bmac-web").unwrap().uid, 998);
        let err = lookup_account(&path, "nobody-here").unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }
}
