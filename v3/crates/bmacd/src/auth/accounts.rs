// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Proxmox accounts and BMAC roles, from `/etc/pve/user.cfg`. Proxmox checks
//! the password and second factor; this decides whether an authenticated
//! user may use BMAC, and is read again every few minutes so a disabled
//! account or a removed group ends the user's sessions.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use bmac_proto::api::Role;

pub const USER_CFG: &str = "/etc/pve/user.cfg";
pub const ROOT: &str = "root@pam";

/// The Proxmox group for each role, most privileged first.
pub const GROUPS: [(&str, Role); 3] = [
    ("bmac-admin", Role::Admin),
    ("bmac-operator", Role::Operator),
    ("bmac-viewer", Role::Viewer),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Allowed(Role),
    /// In none of the BMAC groups.
    NoGroup,
    Disabled,
    Expired,
    /// Not in `user.cfg`.
    Unknown,
}

impl Standing {
    pub fn describe(self) -> &'static str {
        match self {
            Standing::Allowed(_) => "allowed",
            Standing::NoGroup => "the account is in no BMAC group",
            Standing::Disabled => "the account is disabled",
            Standing::Expired => "the account has expired",
            Standing::Unknown => "the account no longer exists",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Account {
    enabled: bool,
    /// Seconds since the epoch; 0 means never.
    expire: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Accounts {
    users: HashMap<String, Account>,
    groups: HashMap<String, HashSet<String>>,
}

impl Accounts {
    /// Reads the `user:` and `group:` lines; everything else is ignored.
    ///
    /// `user:<userid>:<enable>:<expire>:<first>:<last>:<email>:<comment>:<keys>:`
    /// `group:<groupid>:<member>,<member>:<comment>:`
    pub fn parse(text: &str) -> Accounts {
        let mut accounts = Accounts::default();
        for line in text.lines().map(str::trim) {
            let fields: Vec<&str> = line.split(':').collect();
            match fields.as_slice() {
                ["user", id, enable, expire, ..] if !id.is_empty() => {
                    let account = Account {
                        enabled: *enable != "0",
                        expire: expire.parse().unwrap_or(0),
                    };
                    accounts.users.insert(id.to_string(), account);
                }
                ["group", id, members, ..] if !id.is_empty() => {
                    let members = members
                        .split(',')
                        .map(str::trim)
                        .filter(|m| !m.is_empty())
                        .map(String::from)
                        .collect();
                    accounts.groups.insert(id.to_string(), members);
                }
                _ => {}
            }
        }
        accounts
    }

    pub fn load(path: &Path) -> std::io::Result<Accounts> {
        Ok(Accounts::parse(&std::fs::read_to_string(path)?))
    }

    /// What `user` may do at `now` (seconds since the epoch).
    pub fn standing(&self, user: &str, now: i64) -> Standing {
        let Some(account) = self.users.get(user) else {
            return Standing::Unknown;
        };
        if !account.enabled {
            return Standing::Disabled;
        }
        if account.expire > 0 && account.expire <= now {
            return Standing::Expired;
        }
        if user == ROOT {
            return Standing::Allowed(Role::Admin);
        }
        GROUPS
            .iter()
            .find(|(group, _)| self.groups.get(*group).is_some_and(|m| m.contains(user)))
            .map_or(Standing::NoGroup, |(_, role)| Standing::Allowed(*role))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER_CFG: &str = "\
user:root@pam:1:0:::root@example.com:::
user:alice@pve:1:0:Alice:Smith:alice@example.com:::
user:bob@pve:1:0::::::
user:carol@pve:0:0::::::
user:dave@pve:1:1700000000::::::
user:erin@pve:1:0::::::
user:frank@pam:1::::::
token:alice@pve!ci:0:1::
group:bmac-operator:alice@pve,bob@pve::
group:bmac-admin: alice@pve ::
group:bmac-viewer:carol@pve,dave@pve,frank@pam:read only:
group:admins:erin@pve::
acl:1:/:@admins:Administrator:
";

    #[test]
    fn roles_come_from_the_most_privileged_group() {
        let accounts = Accounts::parse(USER_CFG);
        let now = 1_800_000_000;
        assert_eq!(
            accounts.standing("root@pam", now),
            Standing::Allowed(Role::Admin)
        );
        assert_eq!(
            accounts.standing("alice@pve", now),
            Standing::Allowed(Role::Admin)
        );
        assert_eq!(
            accounts.standing("bob@pve", now),
            Standing::Allowed(Role::Operator)
        );
        assert_eq!(
            accounts.standing("frank@pam", now),
            Standing::Allowed(Role::Viewer)
        );
        assert_eq!(accounts.standing("carol@pve", now), Standing::Disabled);
        assert_eq!(accounts.standing("dave@pve", now), Standing::Expired);
        assert_eq!(
            accounts.standing("dave@pve", 1_600_000_000),
            Standing::Allowed(Role::Viewer)
        );
        assert_eq!(
            accounts.standing("erin@pve", now),
            Standing::NoGroup,
            "Proxmox admins are not BMAC admins"
        );
        assert_eq!(accounts.standing("zoe@pve", now), Standing::Unknown);
        assert_eq!(
            accounts.standing("alice@pve!ci", now),
            Standing::Unknown,
            "API tokens can't log in"
        );
    }

    #[test]
    fn a_disabled_root_is_disabled() {
        let accounts = Accounts::parse("user:root@pam:0:0::::::\n");
        assert_eq!(accounts.standing(ROOT, 0), Standing::Disabled);
    }
}
