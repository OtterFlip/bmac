// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Ed25519 SSH keys made in memory: a host ISO's SSH host key (the
//! Deployer) and a join's setup key (`bmacd`).

use std::fmt;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{Ed25519KeyPair, KeyPair as _};
use sha2::{Digest, Sha256};

/// An Ed25519 SSH host key.
pub struct SshHostKey {
    /// `ssh-ed25519 AAAA... root@<fqdn>`.
    pub public: String,
    /// OpenSSH's unencrypted private key file.
    pub private: String,
}

impl fmt::Debug for SshHostKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SshHostKey")
            .field("public", &self.public)
            .finish_non_exhaustive()
    }
}

fn ssh_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

impl SshHostKey {
    pub fn generate(comment: &str) -> Result<SshHostKey, String> {
        let random = SystemRandom::new();
        let mut seed = [0u8; 32];
        random
            .fill(&mut seed)
            .map_err(|_| "the system random source failed")?;
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| "could not make an Ed25519 key")?;
        let public_key = pair.public_key().as_ref();

        let mut blob = Vec::new();
        ssh_string(&mut blob, b"ssh-ed25519");
        ssh_string(&mut blob, public_key);

        let mut check = [0u8; 4];
        random
            .fill(&mut check)
            .map_err(|_| "the system random source failed")?;
        let mut private = Vec::new();
        private.extend_from_slice(&check);
        private.extend_from_slice(&check);
        ssh_string(&mut private, b"ssh-ed25519");
        ssh_string(&mut private, public_key);
        let mut secret = seed.to_vec();
        secret.extend_from_slice(public_key);
        ssh_string(&mut private, &secret);
        ssh_string(&mut private, comment.as_bytes());
        let mut pad = 1u8;
        while private.len() % 8 != 0 {
            private.push(pad);
            pad += 1;
        }

        let mut file = b"openssh-key-v1\0".to_vec();
        ssh_string(&mut file, b"none");
        ssh_string(&mut file, b"none");
        ssh_string(&mut file, b"");
        file.extend_from_slice(&1u32.to_be_bytes());
        ssh_string(&mut file, &blob);
        ssh_string(&mut file, &private);

        let encoded = STANDARD.encode(&file);
        let mut pem = String::from("-----BEGIN OPENSSH PRIVATE KEY-----\n");
        for line in encoded.as_bytes().chunks(70) {
            pem.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
            pem.push('\n');
        }
        pem.push_str("-----END OPENSSH PRIVATE KEY-----\n");
        Ok(SshHostKey {
            public: format!("ssh-ed25519 {} {comment}", STANDARD.encode(&blob)),
            private: pem,
        })
    }

    /// `SHA256:...`, as `ssh-keygen -lf` and SSH's prompts show it.
    pub fn fingerprint(&self) -> String {
        ssh_fingerprint(&self.public).unwrap_or_default()
    }
}

/// The SHA-256 fingerprint of an OpenSSH public key line.
pub fn ssh_fingerprint(public: &str) -> Option<String> {
    let blob = STANDARD.decode(public.split_whitespace().nth(1)?).ok()?;
    Some(format!(
        "SHA256:{}",
        STANDARD_NO_PAD.encode(Sha256::digest(blob))
    ))
}
