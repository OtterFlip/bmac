// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Appliance enrollment on the cluster (`DEPLOYMENT.md`, "Backup appliance
//! ISO"): the bundle an appliance enrolls with, the requests appliances make
//! with it, and the administrator's approval, which records the appliance's
//! certificate fingerprint for `bmac-source` to admit. Revoking an appliance
//! keeps its record, so its PITs stay in the catalog, and stops admitting it.

use std::path::PathBuf;
use std::sync::Arc;

use bmac_backup::records::{ApplianceRecord, Enrollment};
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::normalized_fingerprint;
use bmac_proto::appliance::{ENROLL_BUNDLE_VERSION, EnrollBundle, SourceHost};
use bmac_proto::backups::{
    ApplianceList, ApplianceView, ApproveAppliance, DEFAULT_TTL_HOURS, EnrollRequestView,
    IssueEnroll, IssuedEnroll, MAX_TTL_HOURS, display_fingerprint,
};
use uuid::Uuid;

use super::store::BackupStore;
use crate::identity::Identities;
use crate::joins::{JoinStore, ca_fingerprint};
use crate::mesh::tokens::{PendingRecord, TokenKind, Tokens};
use crate::settings::Settings;

pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Clone)]
pub struct Appliances {
    pub store: BackupStore,
    pub joins: JoinStore,
    pub tokens: Arc<Tokens>,
    pub settings: Settings,
    pub identities: Identities,
    pub clock: Clock,
    pub ca: PathBuf,
}

impl std::fmt::Debug for Appliances {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Appliances")
    }
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn not_found(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::NotFound, message)
}

/// The SHA-256 of a pending request's certificate, normalized.
fn request_fingerprint(pending: &PendingRecord) -> Option<String> {
    pending
        .certificate
        .as_deref()
        .and_then(|pem| ca_fingerprint(pem.as_bytes()).ok())
        .and_then(|f| normalized_fingerprint(&f))
}

fn enrollment_name(e: Enrollment) -> &'static str {
    match e {
        Enrollment::Pending => "pending",
        Enrollment::Approved => "approved",
        Enrollment::Revoked => "revoked",
    }
}

impl Appliances {
    /// The hosts an appliance can reach `bmac-source` on: its tailnet name,
    /// then its private address.
    fn source_hosts(&self) -> Result<Vec<SourceHost>, ApiError> {
        let cluster = self.settings.cluster()?;
        let identities = self.identities.all()?;
        let mut out = Vec::new();
        for facts in self.settings.hosts()? {
            let mut addresses = Vec::new();
            if let Some(name) = identities
                .iter()
                .find(|i| i.host == facts.host)
                .and_then(|i| i.tailnet_name.clone())
            {
                addresses.push(name);
            } else if let Some(name) = facts
                .values
                .get("TAILSCALE_HOSTNAME")
                .filter(|n| !n.is_empty())
            {
                addresses.push(name.clone());
            }
            if let Some(cluster) = &cluster
                && let Ok(ip) =
                    bmac_settings::host_private_ip(facts.host, &facts.values, &cluster.values)
            {
                addresses.push(ip);
            }
            if !addresses.is_empty() {
                out.push(SourceHost {
                    host: facts.host,
                    addresses,
                });
            }
        }
        Ok(out)
    }

    pub fn issue(&self, request: &IssueEnroll, by: &str) -> Result<IssuedEnroll, ApiError> {
        let ttl_hours = request.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS);
        if ttl_hours == 0 || ttl_hours > MAX_TTL_HOURS {
            return Err(invalid(format!("ttl_hours must be 1 to {MAX_TTL_HOURS}")));
        }
        let identity = self.joins.cluster()?.ok_or_else(|| {
            ApiError::new(
                ErrorCode::Unavailable,
                "this cluster has no identity record yet; finish the first host's setup",
            )
        })?;
        let cluster_name = self
            .settings
            .cluster()?
            .and_then(|c| c.values.get("PROXMOX_CLUSTER_NAME").cloned())
            .unwrap_or_default();
        let ca_pem = std::fs::read(&self.ca).map_err(|e| {
            ApiError::new(
                ErrorCode::Unavailable,
                format!("cannot read {}: {e}", self.ca.display()),
            )
        })?;
        let ca = ca_fingerprint(&ca_pem)
            .ok()
            .and_then(|f| normalized_fingerprint(&f))
            .ok_or_else(|| {
                ApiError::new(ErrorCode::Internal, "the cluster CA has no fingerprint")
            })?;
        let hosts = self.source_hosts()?;
        if hosts.is_empty() {
            return Err(ApiError::new(
                ErrorCode::Unavailable,
                "no host has an address an appliance could reach",
            ));
        }
        let ttl_ms = i64::from(ttl_hours) * 3_600_000;
        let token = self.tokens.issue(TokenKind::Enroll, None, ttl_ms, by)?;
        let bundle = EnrollBundle {
            version: ENROLL_BUNDLE_VERSION,
            cluster_uuid: identity.cluster_uuid,
            cluster_name: cluster_name.clone(),
            enroll_token: token,
            expires_ms: (self.clock)() + ttl_ms,
            proxmox_ca_fingerprint: ca,
            hosts,
        };
        Ok(IssuedEnroll {
            file_name: format!("{cluster_name}-appliance-enroll.json"),
            content: serde_json::to_string_pretty(&bundle).expect("bundles serialize"),
        })
    }

    fn requests(&self) -> Result<Vec<PendingRecord>, ApiError> {
        Ok(self
            .tokens
            .pending()?
            .into_iter()
            .filter(|p| p.kind == TokenKind::Enroll)
            .collect())
    }

    pub fn list(&self, can_manage: bool) -> Result<ApplianceList, ApiError> {
        let appliances = self
            .store
            .appliances()?
            .into_iter()
            .map(|a| ApplianceView {
                appliance_uuid: a.appliance_uuid.to_string(),
                name: a.name,
                fingerprint: display_fingerprint(&a.fingerprint),
                address: a.address.map(|a| a.to_string()),
                enrollment: enrollment_name(a.enrollment).into(),
                enrolled_at_ms: a.enrolled_at_ms,
                last_checkin_ms: a.last_checkin_ms,
                version: a.version,
                locked: a.locked,
                paused: a.paused,
                retired: a.retired,
            })
            .collect();
        let requests = self
            .requests()?
            .into_iter()
            .map(|p| EnrollRequestView {
                fingerprint: request_fingerprint(&p).map(|f| display_fingerprint(&f)),
                request_id: p.id,
                appliance_uuid: p.appliance_uuid.map(|u| u.to_string()),
                name: p.name,
                from: p.address,
                suggested_address: p.appliance_address.map(|a| a.to_string()),
                received_by: p.received_by.to_string(),
                created_at_ms: p.created_at_ms,
            })
            .collect();
        Ok(ApplianceList {
            appliances,
            requests,
            can_manage,
        })
    }

    /// Approves request `id` if its certificate is the one `confirmed`
    /// names. An appliance enrolling again (a new certificate after
    /// `adopt`) keeps its record.
    pub fn approve(
        &self,
        id: &str,
        confirmed: &ApproveAppliance,
    ) -> Result<ApplianceRecord, ApiError> {
        let pending = self
            .requests()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| not_found("no such enrollment request"))?;
        let fingerprint = request_fingerprint(&pending)
            .ok_or_else(|| invalid("the request has no usable certificate"))?;
        if normalized_fingerprint(&confirmed.fingerprint).as_deref() != Some(fingerprint.as_str()) {
            return Err(invalid(
                "the fingerprint doesn't match the request's certificate; compare it with the appliance's console",
            ));
        }
        let appliance_uuid = pending
            .appliance_uuid
            .ok_or_else(|| invalid("the request doesn't name its appliance"))?;
        let name = pending
            .name
            .clone()
            .ok_or_else(|| invalid("the request has no name"))?;
        let now = (self.clock)();
        let saved = self.store.locked(|l| {
            let others = l.store().appliances()?;
            if let Some(clash) = others
                .iter()
                .find(|a| a.appliance_uuid != appliance_uuid && !a.retired && a.name == name)
            {
                return Err(invalid(format!(
                    "appliance {} ({}) already has that name",
                    clash.name, clash.appliance_uuid
                )));
            }
            if let Some(clash) = others.iter().find(|a| {
                a.appliance_uuid != appliance_uuid
                    && normalized_fingerprint(&a.fingerprint).as_deref()
                        == Some(fingerprint.as_str())
            }) {
                return Err(invalid(format!(
                    "appliance {} already has that certificate",
                    clash.name
                )));
            }
            let record = match l.store().appliance(&appliance_uuid)? {
                Some(mut existing) => {
                    existing.name = name.clone();
                    existing.fingerprint = fingerprint.clone();
                    existing.enrollment = Enrollment::Approved;
                    existing.retired = false;
                    existing.enrolled_at_ms = now;
                    if pending.appliance_address.is_some() {
                        existing.address = pending.appliance_address.clone();
                    }
                    existing
                }
                None => ApplianceRecord {
                    schema_version: 0,
                    record_type: String::new(),
                    revision: 0,
                    appliance_uuid,
                    name: name.clone(),
                    fingerprint: fingerprint.clone(),
                    address: pending.appliance_address.clone(),
                    enrollment: Enrollment::Approved,
                    enrolled_at_ms: now,
                    paused: false,
                    retired: false,
                    last_checkin_ms: None,
                    version: None,
                    locked: false,
                    progress: Default::default(),
                    heads: Default::default(),
                    index_revision: 0,
                    scrub: None,
                },
            };
            l.save_appliance(record)
        })?;
        self.tokens.remove_pending(id)?;
        Ok(saved)
    }

    pub fn reject(&self, id: &str) -> Result<(), ApiError> {
        if !self.requests()?.iter().any(|p| p.id == id) {
            return Err(not_found("no such enrollment request"));
        }
        self.tokens.remove_pending(id)
    }

    pub fn revoke(&self, appliance: &Uuid) -> Result<ApplianceRecord, ApiError> {
        self.store.locked(|l| {
            let mut record = l
                .store()
                .appliance(appliance)?
                .ok_or_else(|| not_found("no such appliance"))?;
            record.enrollment = Enrollment::Revoked;
            l.save_appliance(record)
        })
    }
}

#[cfg(test)]
#[path = "enrollment_tests.rs"]
mod tests;
