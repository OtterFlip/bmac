// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance administrator's credential and the appliance's name, kept
//! in the metadata dataset so they are encrypted with the pool and survive
//! an OS reinstall: `adopt` checks the password against them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::fsx;

pub const ROUNDS: u32 = 100_000;
pub const MIN_PASSWORD: usize = 12;
const SCHEMA: u32 = 1;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Admin {
    pub schema_version: u32,
    /// `$6$rounds=N$salt$hash`, as `crypt(3)` writes SHA-512 crypt.
    pub password_hash: String,
    pub changed_at_ms: i64,
}

impl std::fmt::Debug for Admin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admin")
            .field("changed_at_ms", &self.changed_at_ms)
            .finish_non_exhaustive()
    }
}

/// Which appliance wrote this pool, for `adopt`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Note {
    pub schema_version: u32,
    pub appliance_uuid: Uuid,
    pub name: String,
}

fn admin_path(metadata: &Path) -> PathBuf {
    metadata.join("admin.json")
}

fn note_path(metadata: &Path) -> PathBuf {
    metadata.join("appliance.json")
}

pub fn load(metadata: &Path) -> Result<Option<Admin>, String> {
    fsx::read_json(&admin_path(metadata)).map_err(|e| e.to_string())
}

pub fn note(metadata: &Path) -> Result<Option<Note>, String> {
    fsx::read_json(&note_path(metadata)).map_err(|e| e.to_string())
}

pub fn save_note(metadata: &Path, appliance_uuid: Uuid, name: &str) -> Result<(), String> {
    let note = Note {
        schema_version: SCHEMA,
        appliance_uuid,
        name: name.to_string(),
    };
    fsx::write_json(&note_path(metadata), &note, 0o644).map_err(|e| e.to_string())
}

fn encode(digest: &[u8; 64]) -> String {
    const ORDER: [usize; 64] = [
        42, 21, 0, 1, 43, 22, 23, 2, 44, 45, 24, 3, 4, 46, 25, 26, 5, 47, 48, 27, 6, 7, 49, 28, 29,
        8, 50, 51, 30, 9, 10, 52, 31, 32, 11, 53, 54, 33, 12, 13, 55, 34, 35, 14, 56, 57, 36, 15,
        16, 58, 37, 38, 17, 59, 60, 39, 18, 19, 61, 40, 41, 20, 62, 63,
    ];
    const ALPHABET: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let ordered: Vec<u8> = ORDER.iter().map(|&i| digest[i]).collect();
    let mut text = String::new();
    for chunk in ordered.chunks(3) {
        let mut value = 0u32;
        for (shift, byte) in chunk.iter().enumerate() {
            value |= u32::from(*byte) << (8 * shift);
        }
        for _ in 0..[0, 2, 3, 4][chunk.len()] {
            text.push(ALPHABET[(value & 0x3f) as usize] as char);
            value >>= 6;
        }
    }
    text
}

fn crypt(password: &str, salt: &str, rounds: u32) -> Result<String, String> {
    let params = sha_crypt::Params::new(rounds).map_err(|e| format!("{e:?}"))?;
    let digest = sha_crypt::sha512_crypt(password.as_bytes(), salt.as_bytes(), params);
    Ok(format!("$6$rounds={rounds}${salt}${}", encode(&digest)))
}

pub fn hash(password: &str, rounds: u32) -> Result<String, String> {
    const SALT: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let salt: String = crate::grants::random(16)?
        .iter()
        .map(|b| SALT[(b & 0x3f) as usize] as char)
        .collect();
    crypt(password, &salt, rounds)
}

/// Whether `password` matches `stored`, compared in constant time.
pub fn matches(stored: &str, password: &str) -> bool {
    let Some(rest) = stored.strip_prefix("$6$rounds=") else {
        return false;
    };
    let mut parts = rest.splitn(3, '$');
    let (Some(rounds), Some(salt), Some(_)) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let Ok(rounds) = rounds.parse::<u32>() else {
        return false;
    };
    let Ok(computed) = crypt(password, salt, rounds) else {
        return false;
    };
    computed.len() == stored.len()
        && computed
            .bytes()
            .zip(stored.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

pub fn check_strength(password: &str) -> Result<(), String> {
    if password.chars().count() < MIN_PASSWORD {
        return Err(format!(
            "the password must be at least {MIN_PASSWORD} characters"
        ));
    }
    if password.contains(['\n', '\r', '\0']) {
        return Err("the password must be one line".into());
    }
    Ok(())
}

pub fn set_password(
    metadata: &Path,
    password: &str,
    now_ms: i64,
    rounds: u32,
) -> Result<(), String> {
    check_strength(password)?;
    let admin = Admin {
        schema_version: SCHEMA,
        password_hash: hash(password, rounds)?,
        changed_at_ms: now_ms,
    };
    fsx::write_json(&admin_path(metadata), &admin, 0o600).map_err(|e| e.to_string())
}

/// `Ok(true)` when an administrator password is set and `password` is it.
pub fn verify(metadata: &Path, password: &str) -> Result<bool, String> {
    Ok(load(metadata)?.is_some_and(|a| matches(&a.password_hash, password)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_crypt3_sha512_and_verifies_only_the_password() {
        assert_eq!(
            crypt("Hello world!", "saltstring", 5000).unwrap(),
            "$6$rounds=5000$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIFNjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1"
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(!verify(dir.path(), "anything at all").unwrap());
        assert!(set_password(dir.path(), "short", 1, 1000).is_err());
        set_password(dir.path(), "a long enough password", 1, 1000).unwrap();
        assert!(verify(dir.path(), "a long enough password").unwrap());
        assert!(!verify(dir.path(), "a long enough passwore").unwrap());
        let text = std::fs::read_to_string(dir.path().join("admin.json")).unwrap();
        assert!(!text.contains("long enough"));
        assert!(!matches("$1$x$y", "a long enough password"));
    }
}
