// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Rotating the Dashboard certificate of the host the browser is connected
//! to (`SECURITY.md`, "Rotation"): `GET /api/v1/certificate`, then
//! `POST /api/v1/certificate/rotate` makes a new key and certificate on the
//! host, which the Dashboard shows over the current, already verified
//! certificate. `POST /api/v1/certificate/switch` names the fingerprint the
//! operator saw and switches to it; `POST /api/v1/certificate/discard` throws
//! it away.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CertificateInfo {
    /// SHA-256 of the DER encoding, as colon-separated uppercase hex.
    pub fingerprint: String,
    /// The subject alternative names: DNS names and IP addresses.
    pub names: Vec<String>,
    pub not_before_ms: i64,
    pub not_after_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CertificateView {
    /// The host whose certificate this is: the one answering this request.
    pub host: String,
    /// Absent when the host has no readable certificate.
    pub current: Option<CertificateInfo>,
    /// A new certificate made but not switched to yet.
    pub pending: Option<CertificateInfo>,
    /// The caller may rotate (admins only).
    pub can_rotate: bool,
}

/// The body of `POST /api/v1/certificate/switch`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SwitchCertificate {
    /// The pending certificate's fingerprint, as shown to the operator; the
    /// switch is refused if the pending certificate has changed since.
    pub fingerprint: String,
}
