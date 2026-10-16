// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Release bundles (`DEPLOYMENT.md`, "Upgrades"): one tar file holding a
//! manifest, its Ed25519 signature, and the `.deb` packages the manifest
//! lists with their SHA-256. Only the manifest is signed, so a bundle is
//! trusted only once the signature checks against a compiled-in key and
//! every package matches its hash.

pub mod build;
mod bundle;
mod keys;
mod manifest;
mod version;

pub use bundle::{Bundle, BundleError, MANIFEST_NAME, MAX_BUNDLE, MAX_MANIFEST, SIGNATURE_NAME};
pub use keys::{DEV_KEY, RELEASE_KEY, TrustedKey, trusted_keys};
pub use manifest::{BUNDLE_FORMAT, Manifest, Package};
pub use version::{compare_versions, debian_version};
