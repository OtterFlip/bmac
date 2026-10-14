// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Shared types: identifiers, units, disk identity, job events, ports, and SSH keys.

pub mod address;
pub mod disk;
pub mod ids;
pub mod job;
pub mod ports;
pub mod sshkey;
pub mod units;

pub use address::{AddressHost, ApplianceAddress};
pub use disk::DiskIdentity;
pub use ids::{
    DatasetName, DomainLabel, GuestName, HostSlot, JobId, MacAddress, OperationId, PoolName,
    Purpose, SafeId, Sha256Hex, SnapshotGuid, SnapshotName, Vmid,
};
pub use units::Bytes;

/// A value that failed validation. The message names the kind of value and
/// what was wrong, and is safe to show to an operator.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct InvalidValue(pub String);

impl InvalidValue {
    pub(crate) fn new(kind: &str, text: &str, why: &str) -> Self {
        let shown: String = text.chars().take(80).collect();
        InvalidValue(format!("{shown:?} is not a valid {kind}: {why}"))
    }
}
