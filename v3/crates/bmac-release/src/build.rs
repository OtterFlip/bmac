// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Writing a bundle's archive, for tests. Releases are packed by
//! `dev/release.py`, which signs with `openssl`, so the private keys never
//! pass through this code.

use crate::bundle::{MANIFEST_NAME, SIGNATURE_NAME};

const BLOCK: usize = 512;

fn octal(field: &mut [u8], value: u64) {
    let width = field.len() - 1;
    let text = format!("{value:0width$o}");
    field[..width].copy_from_slice(text.as_bytes());
}

fn member(out: &mut Vec<u8>, name: &str, data: &[u8]) {
    assert!(name.len() < 100 && !name.contains('/'), "{name}");
    let mut header = [0u8; BLOCK];
    header[..name.len()].copy_from_slice(name.as_bytes());
    octal(&mut header[100..108], 0o644);
    octal(&mut header[108..116], 0);
    octal(&mut header[116..124], 0);
    octal(&mut header[124..136], data.len() as u64);
    octal(&mut header[136..148], 0);
    header[148..156].fill(b' ');
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");
    let sum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(data);
    out.extend(std::iter::repeat_n(0, (BLOCK - data.len() % BLOCK) % BLOCK));
}

/// The archive of a bundle: the manifest, its signature, then `files` in
/// order.
pub fn archive(manifest: &[u8], signature: &[u8], files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    member(&mut out, MANIFEST_NAME, manifest);
    member(&mut out, SIGNATURE_NAME, signature);
    for (name, data) in files {
        member(&mut out, name, data);
    }
    out.extend(std::iter::repeat_n(0, 2 * BLOCK));
    out
}

/// A key pair for signing test bundles.
pub struct TestKey {
    pair: ring::signature::Ed25519KeyPair,
}

impl TestKey {
    pub fn generate() -> TestKey {
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).expect("a key");
        TestKey {
            pair: ring::signature::Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("a key"),
        }
    }

    pub fn trusted(&self, name: &'static str) -> crate::TrustedKey {
        use ring::signature::KeyPair;
        let mut public = [0u8; 32];
        public.copy_from_slice(self.pair.public_key().as_ref());
        crate::TrustedKey { name, public }
    }

    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.pair.sign(message).as_ref().to_vec()
    }

    /// A signed bundle of `packages` (name, file, version, contents).
    pub fn bundle(&self, version: &str, packages: &[(&str, &str, &str, &[u8])]) -> Vec<u8> {
        use sha2::{Digest, Sha256};
        let manifest = crate::Manifest {
            format: crate::BUNDLE_FORMAT,
            version: version.into(),
            built_at_s: 1_760_000_000,
            record_schema: 1,
            reads_record_schema_from: 1,
            packages: packages
                .iter()
                .map(|(name, file, debian, data)| crate::Package {
                    name: (*name).into(),
                    file: (*file).into(),
                    version: (*debian).into(),
                    architecture: "amd64".into(),
                    sha256: Sha256::digest(data)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                    size: data.len() as u64,
                })
                .collect(),
        };
        let text = serde_json::to_vec_pretty(&manifest).expect("a manifest");
        let files: Vec<(&str, &[u8])> = packages.iter().map(|p| (p.1, p.3)).collect();
        archive(&text, &self.sign(&text), &files)
    }
}
