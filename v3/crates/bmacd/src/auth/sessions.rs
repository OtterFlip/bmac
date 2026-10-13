// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Login sessions. A token is 256 random bits, given to the browser once and
//! kept only in the page's memory; the engine stores its SHA-256 hash. The
//! table lives in memory and in the host-local database, so a restarted
//! engine keeps its sessions.

use std::collections::HashMap;
use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Mutex;

use bmac_proto::api::Role;
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::params;
use sha2::{Digest, Sha256};
use tokio::sync::watch;

use crate::db::{Database, DbError};

/// Base64url without padding of 32 bytes.
pub const TOKEN_LENGTH: usize = 43;
/// `last_used` is written to the database at most this often per session.
const TOUCH_WRITE_MS: i64 = 60_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionPolicy {
    /// Without a request, the session ends after this long.
    pub idle_ms: i64,
    /// The session ends this long after login regardless.
    pub absolute_ms: i64,
}

impl Default for SessionPolicy {
    fn default() -> SessionPolicy {
        SessionPolicy {
            idle_ms: 30 * 60 * 1000,
            absolute_ms: 12 * 60 * 60 * 1000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndReason {
    LoggedOut,
    Idle,
    Expired,
    /// "Log out everywhere", from this host or another.
    Revoked,
    /// Disabled, expired, removed from the BMAC groups, or given another
    /// role in Proxmox.
    AccountChanged(String),
}

impl EndReason {
    pub fn kind(&self) -> &'static str {
        match self {
            EndReason::LoggedOut => "logged_out",
            EndReason::Idle => "idle",
            EndReason::Expired => "expired",
            EndReason::Revoked => "revoked",
            EndReason::AccountChanged(_) => "account_changed",
        }
    }

    /// What the user is told.
    pub fn message(&self) -> String {
        match self {
            EndReason::LoggedOut => "you logged out".into(),
            EndReason::Idle => {
                "the session ended after a period without activity; log in again".into()
            }
            EndReason::Expired => "the session reached its time limit; log in again".into(),
            EndReason::Revoked => {
                "the session was ended by logging out everywhere; log in again".into()
            }
            EndReason::AccountChanged(why) => {
                format!("the session ended because {why}; log in again")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub token_hash: String,
    pub user: String,
    pub role: Role,
    pub client: IpAddr,
    pub created_ms: i64,
    pub last_used_ms: i64,
}

struct Live {
    info: SessionInfo,
    written_last_used_ms: i64,
    ended: watch::Sender<Option<EndReason>>,
}

impl Live {
    fn new(info: SessionInfo) -> Live {
        Live {
            written_last_used_ms: info.last_used_ms,
            info,
            ended: watch::channel(None).0,
        }
    }
}

/// The result of checking a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    Valid(SessionInfo),
    /// It was valid until now; this check ended it.
    Ended(EndReason),
    Unknown,
}

pub struct Sessions {
    db: Database,
    policy: SessionPolicy,
    live: Mutex<HashMap<String, Live>>,
    random: SystemRandom,
}

impl std::fmt::Debug for Sessions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sessions")
            .field("policy", &self.policy)
            .finish()
    }
}

pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `token` has the shape of one of ours, checked before any lookup.
pub fn is_token_shaped(token: &str) -> bool {
    token.len() == TOKEN_LENGTH
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
        }
    }
    out
}

/// A fresh 256-bit token, or `None` if the system random source fails.
pub fn new_token(random: &SystemRandom) -> Option<String> {
    let mut bytes = [0u8; 32];
    random.fill(&mut bytes).ok()?;
    Some(base64url(&bytes))
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::Viewer => "viewer",
        Role::Operator => "operator",
        Role::Admin => "admin",
    }
}

fn parse_role(name: &str) -> Option<Role> {
    match name {
        "viewer" => Some(Role::Viewer),
        "operator" => Some(Role::Operator),
        "admin" => Some(Role::Admin),
        _ => None,
    }
}

impl Sessions {
    /// Loads the stored sessions.
    pub async fn load(db: Database, policy: SessionPolicy) -> Result<Sessions, DbError> {
        let stored = db
            .run(|c| {
                let mut statement = c.prepare(
                    "SELECT token_hash, user, role, client, created_ms, last_used_ms FROM sessions",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
            })
            .await?;
        let mut live = HashMap::new();
        let mut unreadable = Vec::new();
        for (token_hash, user, role, client, created_ms, last_used_ms) in stored {
            match (parse_role(&role), IpAddr::from_str(&client)) {
                (Some(role), Ok(client)) => {
                    let info = SessionInfo {
                        token_hash: token_hash.clone(),
                        user,
                        role,
                        client,
                        created_ms,
                        last_used_ms,
                    };
                    live.insert(token_hash, Live::new(info));
                }
                _ => unreadable.push(token_hash),
            }
        }
        if !unreadable.is_empty() {
            db.run(move |c| {
                for hash in &unreadable {
                    c.execute("DELETE FROM sessions WHERE token_hash = ?1", [hash])?;
                }
                Ok(())
            })
            .await?;
        }
        Ok(Sessions {
            db,
            policy,
            live: Mutex::new(live),
            random: SystemRandom::new(),
        })
    }

    pub fn policy(&self) -> SessionPolicy {
        self.policy
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Live>> {
        self.live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts a session; the token is returned once and never stored.
    pub async fn create(
        &self,
        user: &str,
        role: Role,
        client: IpAddr,
        now: i64,
    ) -> Result<(String, SessionInfo), DbError> {
        let token = new_token(&self.random)
            .ok_or_else(|| DbError::Task("the system random source failed".into()))?;
        let info = SessionInfo {
            token_hash: hash_token(&token),
            user: user.to_string(),
            role,
            client,
            created_ms: now,
            last_used_ms: now,
        };
        let row = info.clone();
        self.db
            .run(move |c| {
                c.execute(
                    "INSERT INTO sessions (token_hash, user, role, client, created_ms, last_used_ms)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        row.token_hash,
                        row.user,
                        role_name(row.role),
                        row.client.to_string(),
                        row.created_ms,
                        row.last_used_ms
                    ],
                )
            })
            .await?;
        self.live()
            .insert(info.token_hash.clone(), Live::new(info.clone()));
        Ok((token, info))
    }

    fn expiry(&self, info: &SessionInfo, now: i64) -> Option<EndReason> {
        if now - info.created_ms >= self.policy.absolute_ms {
            Some(EndReason::Expired)
        } else if now - info.last_used_ms >= self.policy.idle_ms {
            Some(EndReason::Idle)
        } else {
            None
        }
    }

    /// Checks a token. `activity` marks a user request, which keeps the
    /// session from going idle; an open event stream is not activity.
    pub async fn lookup(&self, token: &str, now: i64, activity: bool) -> Result<Lookup, DbError> {
        if !is_token_shaped(token) {
            return Ok(Lookup::Unknown);
        }
        let hash = hash_token(token);
        let checked = {
            let mut live = self.live();
            let Some(session) = live.get_mut(&hash) else {
                return Ok(Lookup::Unknown);
            };
            match self.expiry(&session.info, now) {
                Some(reason) => Err(reason),
                None => {
                    let mut write = false;
                    if activity && now > session.info.last_used_ms {
                        session.info.last_used_ms = now;
                        if now - session.written_last_used_ms >= TOUCH_WRITE_MS {
                            session.written_last_used_ms = now;
                            write = true;
                        }
                    }
                    Ok((session.info.clone(), write))
                }
            }
        };
        let (info, write) = match checked {
            Ok(checked) => checked,
            Err(reason) => {
                self.end(&hash, reason.clone()).await?;
                return Ok(Lookup::Ended(reason));
            }
        };
        if write {
            let (hash, used) = (info.token_hash.clone(), info.last_used_ms);
            self.db
                .run(move |c| {
                    c.execute(
                        "UPDATE sessions SET last_used_ms = ?2 WHERE token_hash = ?1",
                        params![hash, used],
                    )
                })
                .await?;
        }
        Ok(Lookup::Valid(info))
    }

    /// Ends a session and tells anything waiting on it. Returns the session
    /// if it was live.
    pub async fn end(
        &self,
        token_hash: &str,
        reason: EndReason,
    ) -> Result<Option<SessionInfo>, DbError> {
        let removed = self.live().remove(token_hash);
        let hash = token_hash.to_string();
        self.db
            .run(move |c| c.execute("DELETE FROM sessions WHERE token_hash = ?1", [hash]))
            .await?;
        Ok(removed.map(|live| {
            live.ended.send_replace(Some(reason));
            live.info
        }))
    }

    /// A receiver that changes to `Some(reason)` when the session ends;
    /// `None` if it already has.
    pub fn watch_end(&self, token_hash: &str) -> Option<watch::Receiver<Option<EndReason>>> {
        self.live()
            .get(token_hash)
            .map(|live| live.ended.subscribe())
    }

    pub fn all(&self) -> Vec<SessionInfo> {
        self.live().values().map(|live| live.info.clone()).collect()
    }

    /// The sessions past their idle or absolute limit at `now`.
    pub fn expired(&self, now: i64) -> Vec<(SessionInfo, EndReason)> {
        self.live()
            .values()
            .filter_map(|live| Some((live.info.clone(), self.expiry(&live.info, now)?)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64url_matches_the_standard_alphabet() {
        assert_eq!(base64url(b""), "");
        assert_eq!(base64url(b"f"), "Zg");
        assert_eq!(base64url(b"fo"), "Zm8");
        assert_eq!(base64url(b"foo"), "Zm9v");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(base64url(&[0u8; 32]).len(), TOKEN_LENGTH);
    }

    #[tokio::test]
    async fn tokens_are_random_hashed_and_survive_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bmac.db");
        let client: IpAddr = "100.64.0.9".parse().unwrap();
        let sessions = Sessions::load(Database::open(&path).unwrap(), SessionPolicy::default())
            .await
            .unwrap();
        let (a, info) = sessions
            .create("alice@pve", Role::Operator, client, 1000)
            .await
            .unwrap();
        let (b, _) = sessions
            .create("alice@pve", Role::Operator, client, 1000)
            .await
            .unwrap();
        assert_ne!(a, b);
        assert!(is_token_shaped(&a), "{a}");
        drop(sessions);

        let stored: Vec<String> = rusqlite::Connection::open(&path)
            .unwrap()
            .prepare("SELECT token_hash FROM sessions")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(stored.len(), 2);
        assert!(
            stored.contains(&hash_token(&a)) && !stored.contains(&a),
            "only hashes are stored"
        );
        let file = std::fs::read(&path).unwrap();
        assert!(!file.windows(a.len()).any(|w| w == a.as_bytes()));

        let sessions = Sessions::load(Database::open(&path).unwrap(), SessionPolicy::default())
            .await
            .unwrap();
        assert_eq!(
            sessions.lookup(&a, 2000, true).await.unwrap(),
            Lookup::Valid(SessionInfo {
                last_used_ms: 2000,
                ..info
            })
        );
        assert_eq!(
            sessions
                .lookup("x".repeat(TOKEN_LENGTH).as_str(), 2000, true)
                .await
                .unwrap(),
            Lookup::Unknown
        );
        assert_eq!(
            sessions.lookup("short", 2000, true).await.unwrap(),
            Lookup::Unknown
        );
    }

    #[tokio::test]
    async fn idle_and_absolute_limits() {
        let dir = tempfile::tempdir().unwrap();
        let policy = SessionPolicy {
            idle_ms: 100,
            absolute_ms: 250,
        };
        let sessions = Sessions::load(Database::open(&dir.path().join("db")).unwrap(), policy)
            .await
            .unwrap();
        let client: IpAddr = "10.0.0.5".parse().unwrap();

        let (idle, _) = sessions
            .create("a@pve", Role::Viewer, client, 0)
            .await
            .unwrap();
        assert!(matches!(
            sessions.lookup(&idle, 60, false).await.unwrap(),
            Lookup::Valid(_)
        ));
        assert_eq!(
            sessions.lookup(&idle, 100, false).await.unwrap(),
            Lookup::Ended(EndReason::Idle),
            "a stream doesn't count as activity"
        );
        assert_eq!(
            sessions.lookup(&idle, 101, true).await.unwrap(),
            Lookup::Unknown
        );

        let (busy, info) = sessions
            .create("a@pve", Role::Viewer, client, 0)
            .await
            .unwrap();
        let mut ended = sessions.watch_end(&info.token_hash).unwrap();
        for t in [90, 180] {
            assert!(matches!(
                sessions.lookup(&busy, t, true).await.unwrap(),
                Lookup::Valid(_)
            ));
        }
        assert_eq!(sessions.expired(249), Vec::new());
        assert_eq!(sessions.expired(250).len(), 1);
        assert_eq!(
            sessions.lookup(&busy, 250, true).await.unwrap(),
            Lookup::Ended(EndReason::Expired)
        );
        ended.changed().await.unwrap();
        assert_eq!(*ended.borrow(), Some(EndReason::Expired));
        assert!(sessions.watch_end(&info.token_hash).is_none());
    }
}
