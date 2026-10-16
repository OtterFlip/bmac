// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use super::*;
use crate::build::{TestKey, archive};

const DEB: &[u8] = b"!<arch>\nnot really a package, but bytes all the same";

fn write(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join("release.bundle");
    std::fs::write(&path, bytes).unwrap();
    path
}

fn one_package(key: &TestKey) -> Vec<u8> {
    key.bundle("3.0.1", &[("bmacd", "bmacd_3.0.1_amd64.deb", "3.0.1", DEB)])
}

fn untrusted(result: Result<Bundle, BundleError>) -> String {
    match result {
        Err(BundleError::Untrusted(message)) => message,
        other => panic!("expected an untrusted bundle, got {other:?}"),
    }
}

#[test]
fn a_signed_bundle_opens_and_its_package_extracts() {
    let dir = tempfile::tempdir().unwrap();
    let key = TestKey::generate();
    let bytes = one_package(&key);
    let path = write(dir.path(), &bytes);
    let other = TestKey::generate();
    let bundle = Bundle::open(&path, &[other.trusted("other"), key.trusted("test")]).unwrap();
    assert_eq!(bundle.signed_by, "test");
    assert_eq!(bundle.manifest.version, "3.0.1");
    assert_eq!(bundle.size, bytes.len() as u64);
    assert_eq!(bundle.sha256, hex(&Sha256::digest(&bytes)));

    let out = dir.path().join("bmacd.deb");
    bundle.extract("bmacd", &out).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), DEB);
    assert!(bundle.extract("bmac-backupd", &out).is_err());
}

#[test]
fn a_bundle_from_an_untrusted_key_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = TestKey::generate();
    let path = write(dir.path(), &one_package(&key));
    let message = untrusted(Bundle::open(&path, &[TestKey::generate().trusted("other")]));
    assert!(message.contains("isn't signed by a key"), "{message}");
    assert!(untrusted(Bundle::open(&path, &[])).contains("isn't signed"));
}

#[test]
fn a_changed_package_or_manifest_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = TestKey::generate();
    let trusted = [key.trusted("test")];

    let mut bytes = one_package(&key);
    let at = bytes.windows(DEB.len()).position(|w| w == DEB).unwrap();
    bytes[at + 4] ^= 1;
    let message = untrusted(Bundle::open(&write(dir.path(), &bytes), &trusted));
    assert!(
        message.contains("doesn't match the signed manifest"),
        "{message}"
    );

    let text = br#"{"format":1}"#;
    let forged = archive(text, &[0u8; 64], &[]);
    untrusted(Bundle::open(&write(dir.path(), &forged), &trusted));
}

#[test]
fn an_extra_or_missing_file_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let key = TestKey::generate();
    let trusted = [key.trusted("test")];
    let manifest = Manifest {
        format: BUNDLE_FORMAT,
        version: "3.0.1".into(),
        built_at_s: 0,
        record_schema: 1,
        reads_record_schema_from: 1,
        packages: vec![crate::Package {
            name: "bmacd".into(),
            file: "bmacd_3.0.1_amd64.deb".into(),
            version: "3.0.1".into(),
            architecture: "amd64".into(),
            sha256: hex(&Sha256::digest(DEB)),
            size: DEB.len() as u64,
        }],
    };
    let text = serde_json::to_vec(&manifest).unwrap();
    let signature = key.sign(&text);

    let missing = archive(&text, &signature, &[]);
    let message = untrusted(Bundle::open(&write(dir.path(), &missing), &trusted));
    assert!(message.contains("listed but missing"), "{message}");

    let extra = archive(
        &text,
        &signature,
        &[("bmacd_3.0.1_amd64.deb", DEB), ("evil.deb", b"x")],
    );
    let message = untrusted(Bundle::open(&write(dir.path(), &extra), &trusted));
    assert!(message.contains("evil.deb"), "{message}");
}

#[test]
fn damaged_archives_are_invalid() {
    let dir = tempfile::tempdir().unwrap();
    let key = TestKey::generate();
    let trusted = [key.trusted("test")];
    let good = one_package(&key);

    let truncated = &good[..good.len() / 2];
    assert!(matches!(
        Bundle::open(&write(dir.path(), truncated), &trusted),
        Err(BundleError::Invalid(_))
    ));

    let mut bad_sum = good.clone();
    bad_sum[0] = b'X';
    assert!(matches!(
        Bundle::open(&write(dir.path(), &bad_sum), &trusted),
        Err(BundleError::Invalid(m)) if m.contains("checksum")
    ));

    let text = br#"{}"#;
    let mut renamed = archive(text, &key.sign(text), &[]);
    renamed[..13].copy_from_slice(b"notes.json\0\0\0");
    let sum: u32 = renamed[..512]
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u32::from(b)
            }
        })
        .sum();
    renamed[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    assert!(matches!(
        Bundle::open(&write(dir.path(), &renamed), &trusted),
        Err(BundleError::Invalid(m)) if m.contains("must start with")
    ));
}
