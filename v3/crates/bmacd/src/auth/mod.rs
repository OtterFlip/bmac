// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Logging in (`SECURITY.md`, "Login"). Proxmox checks the password and the
//! second factor; BMAC decides the role from the Proxmox groups, issues a
//! memory-only token, and ends sessions when they go idle, reach their
//! limit, are revoked, or the account changes.

pub mod accounts;
pub mod limits;
pub mod proxmox;
pub mod revocations;
pub mod sessions;

use std::collections::{BTreeSet, HashMap};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use bmac_proto::api::{ApiError, ErrorCode, LoginRequest, LoginResponse, SecondFactor, Session};
use ring::rand::SystemRandom;
use tokio::sync::watch;

use self::accounts::{Accounts, Standing};
use self::limits::{Limited, Limiter, Window};
use self::proxmox::{ProxmoxError, ProxmoxTickets, TicketForm, TicketOutcome};
use self::revocations::Revocations;
use self::sessions::{EndReason, Lookup, SessionInfo, SessionPolicy, Sessions};
use crate::audit::{Audit, AuditKind};
use crate::db::{Database, DbError};
use crate::log;
use crate::store::Store;

/// A second-factor challenge must be answered within this long.
pub const CHALLENGE_TTL_MS: i64 = 2 * 60 * 1000;
pub const MAX_CHALLENGES: usize = 1000;
/// Wrong codes allowed per challenge before the password is needed again.
pub const MAX_SECOND_FACTOR_TRIES: u32 = 3;
/// How often accounts and groups are read again.
pub const ACCOUNT_RECHECK_MS: i64 = 5 * 60 * 1000;
const MAX_USER: usize = 128;
const MAX_PASSWORD: usize = 4096;

/// Milliseconds since the epoch.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as i64)
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginLimits {
    /// Every attempt from one address.
    pub per_address: Window,
    /// Failed attempts for one user name, from anywhere.
    pub per_user: Window,
}

impl Default for LoginLimits {
    fn default() -> LoginLimits {
        LoginLimits {
            per_address: Window {
                limit: 30,
                period_ms: 5 * 60 * 1000,
            },
            per_user: Window {
                limit: 10,
                period_ms: 15 * 60 * 1000,
            },
        }
    }
}

pub struct AuthConfig {
    pub proxmox: ProxmoxTickets,
    pub user_cfg: PathBuf,
    pub revocations: Revocations,
    pub policy: SessionPolicy,
    pub limits: LoginLimits,
    /// This host's node name, for audit events.
    pub host: String,
    pub clock: Clock,
}

impl AuthConfig {
    pub fn system(host: impl Into<String>) -> AuthConfig {
        AuthConfig {
            proxmox: ProxmoxTickets::system(),
            user_cfg: PathBuf::from(accounts::USER_CFG),
            revocations: Revocations::system(),
            policy: SessionPolicy::default(),
            limits: LoginLimits::default(),
            host: host.into(),
            clock: system_clock(),
        }
    }
}

struct Pending {
    user: String,
    partial_ticket: String,
    client: IpAddr,
    created_ms: i64,
    methods: Vec<SecondFactor>,
    tries: u32,
}

struct Limiters {
    address: Limiter,
    user: Limiter,
}

pub struct Auth {
    proxmox: ProxmoxTickets,
    user_cfg: PathBuf,
    revocations: Revocations,
    sessions: Sessions,
    audit: Audit,
    store: Store,
    clock: Clock,
    limiters: Mutex<Limiters>,
    pending: Mutex<HashMap<String, Pending>>,
    random: SystemRandom,
    last_recheck_ms: Mutex<i64>,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Auth")
            .field("sessions", &self.sessions)
            .finish()
    }
}

fn error(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

fn rate_limited(limited: Limited) -> ApiError {
    let mut e = error(
        ErrorCode::RateLimited,
        format!(
            "too many login attempts; wait {} seconds and try again",
            limited.retry_after_seconds
        ),
    );
    e.retry_after_seconds = Some(limited.retry_after_seconds);
    e
}

fn unavailable(e: &dyn std::fmt::Display) -> ApiError {
    log::error(&format!("login: {e}"));
    error(
        ErrorCode::Unavailable,
        "this host can't check logins right now; try again shortly or use another host",
    )
}

/// `name@realm`, as Proxmox spells user IDs. API tokens (`!`) can't log in.
fn valid_user(user: &str) -> bool {
    let Some((name, realm)) = user.rsplit_once('@') else {
        return false;
    };
    user.len() <= MAX_USER
        && !name.is_empty()
        && !name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, ':' | '/' | '!'))
        && realm.len() >= 2
        && realm.starts_with(|c: char| c.is_ascii_alphabetic())
        && realm
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

fn valid_response(method: SecondFactor, response: &str) -> bool {
    match method {
        SecondFactor::Totp => {
            (6..=8).contains(&response.len()) && response.bytes().all(|b| b.is_ascii_digit())
        }
        SecondFactor::Recovery => {
            (1..=64).contains(&response.len())
                && response
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }
        SecondFactor::Webauthn => false,
    }
}

fn method_name(method: SecondFactor) -> &'static str {
    match method {
        SecondFactor::Totp => "totp",
        SecondFactor::Recovery => "recovery",
        SecondFactor::Webauthn => "webauthn",
    }
}

impl Auth {
    pub async fn new(config: AuthConfig, db: Database) -> Result<Auth, DbError> {
        let sessions = Sessions::load(db.clone(), config.policy).await?;
        let now = (config.clock)();
        let store = Store::open(db, config.host, now).await?;
        Ok(Auth {
            proxmox: config.proxmox,
            user_cfg: config.user_cfg,
            revocations: config.revocations,
            sessions,
            audit: Audit::new(store.clone()),
            store,
            limiters: Mutex::new(Limiters {
                address: Limiter::new(config.limits.per_address),
                user: Limiter::new(config.limits.per_user),
            }),
            clock: config.clock,
            pending: Mutex::new(HashMap::new()),
            random: SystemRandom::new(),
            last_recheck_ms: Mutex::new(now),
        })
    }

    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    pub fn audit(&self) -> &Audit {
        &self.audit
    }

    /// This incarnation's record log, opened with the sessions.
    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn now(&self) -> i64 {
        (self.clock)()
    }

    fn limiters(&self) -> std::sync::MutexGuard<'_, Limiters> {
        self.limiters.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn pending(&self) -> std::sync::MutexGuard<'_, HashMap<String, Pending>> {
        self.pending.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Counts an attempt from `client` and refuses if the address or the
    /// user name is over its limit.
    async fn admit(&self, client: IpAddr, user: Option<&str>, now: i64) -> Result<(), ApiError> {
        let address = client.to_string();
        let refused = {
            let mut limiters = self.limiters();
            match limiters.address.check(&address, now) {
                Err(limited) => Some((limited, None)),
                Ok(()) => {
                    limiters.address.record(&address, now);
                    match user.map(|u| (u, limiters.user.check(&u.to_lowercase(), now))) {
                        Some((u, Err(limited))) => Some((limited, Some(u.to_string()))),
                        _ => None,
                    }
                }
            }
        };
        let Some((limited, limited_user)) = refused else {
            return Ok(());
        };
        if limited.first {
            let what = match &limited_user {
                Some(_) => "too many failed logins for this user name",
                None => "too many login attempts from this address",
            };
            self.audit
                .record(
                    now,
                    AuditKind::LoginRateLimited,
                    limited_user.as_deref().or(user),
                    Some(client),
                    what,
                )
                .await;
        }
        Err(rate_limited(limited))
    }

    fn failed(&self, user: &str, now: i64) {
        self.limiters().user.record(&user.to_lowercase(), now);
    }

    pub async fn login(
        &self,
        request: LoginRequest,
        client: IpAddr,
    ) -> Result<LoginResponse, ApiError> {
        match request {
            LoginRequest::Password { user, password } => {
                self.password(user, password, client).await
            }
            LoginRequest::SecondFactor {
                challenge,
                method,
                response,
            } => {
                self.second_factor(challenge, method, response, client)
                    .await
            }
        }
    }

    async fn password(
        &self,
        user: String,
        password: String,
        client: IpAddr,
    ) -> Result<LoginResponse, ApiError> {
        let now = self.now();
        if !valid_user(&user) {
            let mut e = error(
                ErrorCode::Invalid,
                "enter the user name with its realm, such as alice@pve",
            );
            e.field_errors
                .insert("user".into(), "a Proxmox user such as alice@pve".into());
            return Err(e);
        }
        if password.is_empty() || password.len() > MAX_PASSWORD {
            let mut e = error(ErrorCode::Invalid, "enter the password");
            e.field_errors.insert("password".into(), "required".into());
            return Err(e);
        }
        self.admit(client, Some(&user), now).await?;
        let outcome = self
            .proxmox
            .ticket(&TicketForm::Password {
                user: user.clone(),
                password,
            })
            .await;
        match outcome {
            Ok(TicketOutcome::Granted { user }) => self.finish(user, client).await,
            Ok(TicketOutcome::SecondFactor {
                user,
                partial_ticket,
                challenge,
            }) => {
                let mut methods = Vec::new();
                if challenge.totp {
                    methods.push(SecondFactor::Totp);
                }
                if challenge.recovery {
                    methods.push(SecondFactor::Recovery);
                }
                if methods.is_empty() {
                    let message = "the account's only second factor is one BMAC can't use yet (WebAuthn, Yubico, or U2F); add TOTP or recovery keys to it in Proxmox";
                    self.audit
                        .record(
                            now,
                            AuditKind::LoginRefused,
                            Some(&user),
                            Some(client),
                            message,
                        )
                        .await;
                    return Err(error(ErrorCode::Forbidden, message));
                }
                let id = sessions::new_token(&self.random)
                    .ok_or_else(|| unavailable(&"no random bytes"))?;
                {
                    let mut pending = self.pending();
                    pending.retain(|_, p| now - p.created_ms < CHALLENGE_TTL_MS);
                    if pending.len() >= MAX_CHALLENGES {
                        return Err(rate_limited(Limited {
                            retry_after_seconds: 30,
                            first: false,
                        }));
                    }
                    pending.insert(
                        id.clone(),
                        Pending {
                            user,
                            partial_ticket,
                            client,
                            created_ms: now,
                            methods: methods.clone(),
                            tries: 0,
                        },
                    );
                }
                Ok(LoginResponse::SecondFactor {
                    challenge: id,
                    methods,
                })
            }
            Err(ProxmoxError::Refused) => {
                self.failed(&user, now);
                self.audit
                    .record(
                        now,
                        AuditKind::LoginFailed,
                        Some(&user),
                        Some(client),
                        "wrong password, or the account is disabled",
                    )
                    .await;
                Err(error(
                    ErrorCode::Unauthenticated,
                    "the user name or password is wrong, or the account is disabled",
                ))
            }
            Err(e) => Err(unavailable(&e)),
        }
    }

    async fn second_factor(
        &self,
        challenge: String,
        method: SecondFactor,
        response: String,
        client: IpAddr,
    ) -> Result<LoginResponse, ApiError> {
        let now = self.now();
        let found = {
            let pending = self.pending();
            pending
                .get(&challenge)
                .filter(|p| p.client == client && now - p.created_ms < CHALLENGE_TTL_MS)
                .map(|p| (p.user.clone(), p.partial_ticket.clone(), p.methods.clone()))
        };
        let Some((user, partial_ticket, methods)) = found else {
            self.pending()
                .retain(|_, p| now - p.created_ms < CHALLENGE_TTL_MS);
            self.admit(client, None, now).await?;
            return Err(error(
                ErrorCode::Unauthenticated,
                "the login took too long; start again with your password",
            ));
        };
        let response: String = response.chars().filter(|c| !c.is_whitespace()).collect();
        if !methods.contains(&method) || !valid_response(method, &response) {
            let mut e = error(
                ErrorCode::Invalid,
                "enter the code from your authenticator app or a recovery key",
            );
            e.field_errors
                .insert("response".into(), "not a valid code".into());
            return Err(e);
        }
        self.admit(client, Some(&user), now).await?;
        let form = TicketForm::SecondFactor {
            user: user.clone(),
            partial_ticket,
            response: format!("{}:{response}", method_name(method)),
        };
        match self.proxmox.ticket(&form).await {
            Ok(TicketOutcome::Granted { user }) => {
                self.pending().remove(&challenge);
                self.finish(user, client).await
            }
            Ok(TicketOutcome::SecondFactor { .. }) => {
                self.pending().remove(&challenge);
                Err(unavailable(&"Proxmox asked for a second factor twice"))
            }
            Err(ProxmoxError::Refused) => {
                self.failed(&user, now);
                let exhausted = {
                    let mut pending = self.pending();
                    let tries = pending
                        .get_mut(&challenge)
                        .map_or(MAX_SECOND_FACTOR_TRIES, |p| {
                            p.tries += 1;
                            p.tries
                        });
                    if tries >= MAX_SECOND_FACTOR_TRIES {
                        pending.remove(&challenge);
                    }
                    tries >= MAX_SECOND_FACTOR_TRIES
                };
                self.audit
                    .record(
                        now,
                        AuditKind::LoginFailed,
                        Some(&user),
                        Some(client),
                        format!("wrong {} code", method_name(method)),
                    )
                    .await;
                Err(error(
                    ErrorCode::Unauthenticated,
                    if exhausted {
                        "that code is wrong; start again with your password"
                    } else {
                        "that code is wrong; try again"
                    },
                ))
            }
            Err(e) => Err(unavailable(&e)),
        }
    }

    async fn accounts(&self) -> std::io::Result<Accounts> {
        let path = self.user_cfg.clone();
        tokio::task::spawn_blocking(move || Accounts::load(&path))
            .await
            .map_err(std::io::Error::other)?
    }

    /// Proxmox accepted the user: decide the role and start the session.
    async fn finish(&self, user: String, client: IpAddr) -> Result<LoginResponse, ApiError> {
        let now = self.now();
        let accounts = self
            .accounts()
            .await
            .map_err(|e| unavailable(&format!("cannot read the account list: {e}")))?;
        let role = match accounts.standing(&user, now / 1000) {
            Standing::Allowed(role) => role,
            Standing::NoGroup => {
                self.audit
                    .record(
                        now,
                        AuditKind::LoginRefused,
                        Some(&user),
                        Some(client),
                        "in no BMAC group",
                    )
                    .await;
                return Err(error(
                    ErrorCode::Forbidden,
                    "this account may not use BMAC; an administrator can add it to the bmac-viewer, bmac-operator, or bmac-admin group in Proxmox",
                ));
            }
            other => {
                self.audit
                    .record(
                        now,
                        AuditKind::LoginRefused,
                        Some(&user),
                        Some(client),
                        other.describe(),
                    )
                    .await;
                return Err(error(ErrorCode::Forbidden, other.describe()));
            }
        };
        let (token, info) = self
            .sessions
            .create(&user, role, client, now)
            .await
            .map_err(|e| unavailable(&e))?;
        self.audit
            .record(
                now,
                AuditKind::Login,
                Some(&user),
                Some(client),
                format!("logged in as {role:?}").to_lowercase(),
            )
            .await;
        let policy = self.sessions.policy();
        Ok(LoginResponse::Session(Session {
            token,
            user,
            role,
            idle_timeout_seconds: (policy.idle_ms / 1000) as u64,
            expires_at_ms: info.created_ms + policy.absolute_ms,
        }))
    }

    /// The session for a call's token. `activity` is false for an event
    /// stream, which doesn't keep a session alive.
    pub async fn authenticate(
        &self,
        token: Option<&str>,
        activity: bool,
    ) -> Result<SessionInfo, ApiError> {
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            return Err(error(ErrorCode::Unauthenticated, "log in first"));
        };
        let now = self.now();
        match self.sessions.lookup(token, now, activity).await {
            Ok(Lookup::Valid(info)) => Ok(info),
            Ok(Lookup::Ended(reason)) => {
                self.ended_audit(None, &reason, now).await;
                Err(error(ErrorCode::Unauthenticated, reason.message()))
            }
            Ok(Lookup::Unknown) => Err(error(
                ErrorCode::Unauthenticated,
                "the session has ended; log in again",
            )),
            Err(e) => Err(unavailable(&e)),
        }
    }

    async fn ended_audit(&self, info: Option<&SessionInfo>, reason: &EndReason, now: i64) {
        self.audit
            .record(
                now,
                AuditKind::SessionEnded,
                info.map(|i| i.user.as_str()),
                info.map(|i| i.client),
                format!("{}: {}", reason.kind(), reason.message()),
            )
            .await;
    }

    async fn end(&self, info: &SessionInfo, reason: EndReason, now: i64) {
        match self.sessions.end(&info.token_hash, reason.clone()).await {
            Ok(Some(info)) => self.ended_audit(Some(&info), &reason, now).await,
            Ok(None) => {}
            Err(e) => log::error(&format!("cannot end a session of {}: {e}", info.user)),
        }
    }

    pub async fn logout(
        &self,
        session: &SessionInfo,
        client: IpAddr,
        everywhere: bool,
    ) -> Result<(), ApiError> {
        let now = self.now();
        if !everywhere {
            self.sessions
                .end(&session.token_hash, EndReason::LoggedOut)
                .await
                .map_err(|e| unavailable(&e))?;
            self.audit
                .record(
                    now,
                    AuditKind::Logout,
                    Some(&session.user),
                    Some(client),
                    "logged out",
                )
                .await;
            return Ok(());
        }
        let revocations = self.revocations.clone();
        let user = session.user.clone();
        tokio::task::spawn_blocking(move || revocations.revoke(&user, now))
            .await
            .map_err(|e| unavailable(&e))?
            .map_err(|e| unavailable(&e))?;
        self.audit
            .record(
                now,
                AuditKind::LogoutEverywhere,
                Some(&session.user),
                Some(client),
                "logged out everywhere",
            )
            .await;
        for info in self
            .sessions
            .all()
            .into_iter()
            .filter(|i| i.user == session.user)
        {
            self.end(&info, EndReason::Revoked, now).await;
        }
        Ok(())
    }

    /// Changes to `Some(reason)` when the session ends.
    pub fn watch_end(&self, session: &SessionInfo) -> Option<watch::Receiver<Option<EndReason>>> {
        self.sessions.watch_end(&session.token_hash)
    }

    /// Ends expired and revoked sessions, and rechecks accounts when due.
    /// Called every few seconds.
    pub async fn maintain(&self) {
        let now = self.now();
        for (info, reason) in self.sessions.expired(now) {
            self.end(&info, reason, now).await;
        }
        let users: BTreeSet<String> = self.sessions.all().into_iter().map(|i| i.user).collect();
        for user in users {
            let revocations = self.revocations.clone();
            let name = user.clone();
            let revoked = tokio::task::spawn_blocking(move || revocations.revoked_at(&name)).await;
            match revoked {
                Ok(Ok(Some(at))) => {
                    for info in self.sessions.all() {
                        if info.user == user && info.created_ms <= at {
                            self.end(&info, EndReason::Revoked, now).await;
                        }
                    }
                }
                Ok(Ok(None)) => {}
                Ok(Err(e)) => log::warning(&format!("cannot read session revocations: {e}")),
                Err(e) => log::warning(&format!("cannot read session revocations: {e}")),
            }
        }
        let due = {
            let mut last = self
                .last_recheck_ms
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let due = now - *last >= ACCOUNT_RECHECK_MS;
            if due {
                *last = now;
            }
            due
        };
        if due {
            self.recheck_accounts().await;
        }
    }

    /// Ends the sessions of users who were disabled, expired, removed, taken
    /// out of the BMAC groups, or moved to another role.
    pub async fn recheck_accounts(&self) {
        let now = self.now();
        let accounts = match self.accounts().await {
            Ok(accounts) => accounts,
            Err(e) => {
                log::warning(&format!(
                    "cannot read the account list; sessions stay as they are: {e}"
                ));
                return;
            }
        };
        for info in self.sessions.all() {
            let reason = match accounts.standing(&info.user, now / 1000) {
                Standing::Allowed(role) if role == info.role => continue,
                Standing::Allowed(_) => "your BMAC role changed".to_string(),
                other => other.describe().to_string(),
            };
            self.end(&info, EndReason::AccountChanged(reason), now)
                .await;
        }
    }
}

/// Starts the engine's periodic session upkeep.
pub fn spawn_upkeep(auth: Arc<Auth>, every: std::time::Duration) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(every).await;
            auth.maintain().await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_names_need_a_realm() {
        for good in [
            "alice@pve",
            "root@pam",
            "j.doe@ad-corp",
            "a@b.c",
            "alice@example@pve",
        ] {
            assert!(valid_user(good), "{good}");
        }
        for bad in [
            "alice",
            "@pve",
            "alice@",
            "alice@p",
            "al ice@pve",
            "a:b@pve",
            "a/b@pve",
            "alice@pve!tok",
            "alice@1pve",
        ] {
            assert!(!valid_user(bad), "{bad}");
        }
    }

    #[test]
    fn codes_are_checked_before_proxmox_sees_them() {
        assert!(valid_response(SecondFactor::Totp, "123456"));
        assert!(!valid_response(SecondFactor::Totp, "12345"));
        assert!(!valid_response(SecondFactor::Totp, "12345a"));
        assert!(valid_response(
            SecondFactor::Recovery,
            "abcd-ef12-3456-7890"
        ));
        assert!(!valid_response(SecondFactor::Recovery, "abc:def"));
        assert!(!valid_response(SecondFactor::Webauthn, "{}"));
    }
}
