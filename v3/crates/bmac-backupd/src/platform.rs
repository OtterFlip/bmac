// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance's own settings, in the metadata dataset: scrubs, the I/O
//! policy scrubs and pulls share, the watchdog, and where its email goes.
//! The Postmark server token is apart, in a 0600 file, and is never shown.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fsx;
use crate::journal::Metadata;

const SCHEMA: u32 = 1;
pub const MAX_URLS: usize = 20;

/// How scrubs and pulls share the disks.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IoPolicy {
    /// A pull pauses a running scrub, and no scrub starts during a pull.
    #[default]
    PullsFirst,
    /// No pull starts while a scrub runs.
    ScrubFirst,
    /// Both run at once.
    Together,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Watchdog {
    /// Minutes with no host answering before the down email.
    pub down_after_minutes: u32,
    /// Public production URLs checked from outside the cluster.
    pub urls: Vec<String>,
}

impl Default for Watchdog {
    fn default() -> Watchdog {
        Watchdog {
            down_after_minutes: 5,
            urls: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Notify {
    /// A confirmed Postmark sender signature.
    pub from: String,
    pub to: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Platform {
    pub schema_version: u32,
    /// 0 turns scheduled scrubs off.
    pub scrub_every_days: u32,
    pub io_policy: IoPolicy,
    pub watchdog: Watchdog,
    pub notify: Notify,
}

impl Default for Platform {
    fn default() -> Platform {
        Platform {
            schema_version: SCHEMA,
            scrub_every_days: 30,
            io_policy: IoPolicy::default(),
            watchdog: Watchdog::default(),
            notify: Notify::default(),
        }
    }
}

fn is_email(text: &str) -> bool {
    let Some((user, domain)) = text.split_once('@') else {
        return false;
    };
    !user.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == ',')
}

/// An `http` or `https` URL with a host and no credentials.
pub fn check_url(url: &str) -> Result<(), String> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .ok_or_else(|| format!("{url}: only http and https URLs are checked"))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err(format!("{url}: give a host, without credentials"));
    }
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{url}: no spaces"));
    }
    Ok(())
}

impl Platform {
    fn path(metadata: &Metadata) -> PathBuf {
        metadata.root().join("platform.json")
    }

    fn token_path(metadata: &Metadata) -> PathBuf {
        metadata.root().join("postmark-token")
    }

    pub fn load(metadata: &Metadata) -> std::io::Result<Platform> {
        Ok(fsx::read_json(&Self::path(metadata))?.unwrap_or_default())
    }

    pub fn check(&self) -> Result<(), String> {
        if self.scrub_every_days > 366 {
            return Err("scrub at least once a year".into());
        }
        if !(1..=1440).contains(&self.watchdog.down_after_minutes) {
            return Err("the watchdog waits 1 to 1440 minutes".into());
        }
        if self.watchdog.urls.len() > MAX_URLS {
            return Err(format!("at most {MAX_URLS} URLs"));
        }
        for url in &self.watchdog.urls {
            check_url(url)?;
        }
        if !self.notify.from.is_empty() && !is_email(&self.notify.from) {
            return Err(format!("{} isn't an email address", self.notify.from));
        }
        if let Some(bad) = self.notify.to.iter().find(|t| !is_email(t)) {
            return Err(format!("{bad} isn't an email address"));
        }
        Ok(())
    }

    pub fn save(&self, metadata: &Metadata) -> Result<(), String> {
        self.check()?;
        fsx::write_json(&Self::path(metadata), self, 0o644).map_err(|e| e.to_string())
    }

    pub fn token(metadata: &Metadata) -> Option<String> {
        std::fs::read_to_string(Self::token_path(metadata))
            .ok()
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    }

    pub fn set_token(metadata: &Metadata, token: &str) -> Result<(), String> {
        let token = token.trim();
        if token.is_empty() || token.len() > 200 || token.chars().any(|c| c.is_whitespace()) {
            return Err("that doesn't look like a Postmark server token".into());
        }
        fsx::write_atomic(&Self::token_path(metadata), token.as_bytes(), 0o600)
            .map_err(|e| e.to_string())
    }

    pub fn clear_token(metadata: &Metadata) -> Result<(), String> {
        match std::fs::remove_file(Self::token_path(metadata)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Whether the watchdog can send email.
    pub fn can_notify(&self, metadata: &Metadata) -> bool {
        !self.notify.from.is_empty()
            && !self.notify.to.is_empty()
            && Self::token(metadata).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn settings_are_checked_and_the_token_is_kept_apart() {
        let dir = tempfile::tempdir().unwrap();
        let metadata = Metadata::new(dir.path());
        let mut p = Platform::load(&metadata).unwrap();
        assert_eq!((p.scrub_every_days, p.watchdog.down_after_minutes), (30, 5));
        p.watchdog.urls = vec!["https://shop.example.com/health".into()];
        p.notify = Notify {
            from: "bmac@example.com".into(),
            to: vec!["ops@example.com".into()],
        };
        p.save(&metadata).unwrap();
        assert!(!p.can_notify(&metadata));
        Platform::set_token(&metadata, " tok-123 \n").unwrap();
        assert!(p.can_notify(&metadata));
        assert_eq!(Platform::token(&metadata).as_deref(), Some("tok-123"));
        let mode = std::fs::metadata(dir.path().join("postmark-token"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(
            !std::fs::read_to_string(dir.path().join("platform.json"))
                .unwrap()
                .contains("tok-123")
        );
        assert_eq!(Platform::load(&metadata).unwrap(), p);

        for bad in ["ftp://x", "https://user:pw@x/", "https://", "https://a b"] {
            p.watchdog.urls = vec![bad.into()];
            assert!(p.save(&metadata).is_err(), "{bad}");
        }
        p.watchdog.urls.clear();
        p.notify.to = vec!["not an address".into()];
        assert!(p.save(&metadata).is_err());
        Platform::clear_token(&metadata).unwrap();
        assert!(Platform::token(&metadata).is_none());
    }
}
