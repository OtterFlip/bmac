// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Audit events (`SECURITY.md`, "Audit"): who did what, from where, on which
//! host, and when. Stored as records in the host's record log.

use std::net::IpAddr;

use crate::log;
use crate::store::{Entry, Store};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditKind {
    Login,
    LoginFailed,
    LoginRefused,
    LoginRateLimited,
    Logout,
    LogoutEverywhere,
    SessionEnded,
    JobLaunched,
    JobCancelRequested,
    JobResumed,
    JobReconciled,
    JobAnswered,
    SettingsChanged,
    CertificateChanged,
    AlertAcknowledged,
    SilenceChanged,
    NotificationsChanged,
    JoinChanged,
    ApplianceChanged,
}

impl AuditKind {
    pub fn as_str(self) -> &'static str {
        match self {
            AuditKind::Login => "login",
            AuditKind::LoginFailed => "login_failed",
            AuditKind::LoginRefused => "login_refused",
            AuditKind::LoginRateLimited => "login_rate_limited",
            AuditKind::Logout => "logout",
            AuditKind::LogoutEverywhere => "logout_everywhere",
            AuditKind::SessionEnded => "session_ended",
            AuditKind::JobLaunched => "job_launched",
            AuditKind::JobCancelRequested => "job_cancel_requested",
            AuditKind::JobResumed => "job_resumed",
            AuditKind::JobReconciled => "job_reconciled",
            AuditKind::JobAnswered => "job_answered",
            AuditKind::SettingsChanged => "settings_changed",
            AuditKind::CertificateChanged => "certificate_changed",
            AuditKind::AlertAcknowledged => "alert_acknowledged",
            AuditKind::SilenceChanged => "silence_changed",
            AuditKind::NotificationsChanged => "notifications_changed",
            AuditKind::JoinChanged => "join_changed",
            AuditKind::ApplianceChanged => "appliance_changed",
        }
    }
}

pub use crate::store::AuditRecord as AuditEvent;

#[derive(Debug, Clone)]
pub struct Audit {
    store: Store,
}

impl Audit {
    pub fn new(store: Store) -> Audit {
        Audit { store }
    }

    /// Records an event. A failure to store it is logged, never fatal.
    pub async fn record(
        &self,
        time_ms: i64,
        kind: AuditKind,
        user: Option<&str>,
        client: Option<IpAddr>,
        message: impl Into<String>,
    ) {
        let client = client.map(|c| c.to_string());
        let entry = Entry::audit(
            time_ms,
            kind.as_str(),
            user,
            client.as_deref(),
            &message.into(),
        );
        if let Err(e) = self.store.append(vec![entry]).await {
            log::error(&format!(
                "cannot store an audit event ({}): {e}",
                kind.as_str()
            ));
        }
    }

    /// The newest `limit` events, newest first.
    pub async fn recent(&self, limit: u32) -> Vec<AuditEvent> {
        self.store.audit_recent(limit).await.unwrap_or_default()
    }
}
