// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Reading and checking a bundle: a flat ustar archive whose first two
//! members are `manifest.json` and `manifest.sig` (the raw 64-byte Ed25519
//! signature over the manifest's bytes), followed by exactly the packages
//! the manifest lists.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use ring::signature::{ED25519, UnparsedPublicKey};
use sha2::{Digest, Sha256};

use crate::keys::TrustedKey;
use crate::manifest::{BUNDLE_FORMAT, Manifest, digest_ok, file_name_ok, package_name_ok};

pub const MANIFEST_NAME: &str = "manifest.json";
pub const SIGNATURE_NAME: &str = "manifest.sig";
pub const MAX_MANIFEST: u64 = 64 * 1024;
/// The largest bundle accepted.
pub const MAX_BUNDLE: u64 = 2 << 30;
const BLOCK: usize = 512;

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("{0}")]
    Io(String),
    /// Not a bundle, or a damaged one.
    #[error("not a valid release bundle: {0}")]
    Invalid(String),
    /// Well formed, but not signed by a key this build trusts, or a package
    /// doesn't match the signed manifest.
    #[error("{0}")]
    Untrusted(String),
}

fn io(path: &Path) -> impl Fn(std::io::Error) -> BundleError + '_ {
    move |e| BundleError::Io(format!("{}: {e}", path.display()))
}

fn invalid(message: impl Into<String>) -> BundleError {
    BundleError::Invalid(message.into())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Debug, Clone)]
struct Entry {
    offset: u64,
    size: u64,
    sha256: String,
}

/// A bundle whose signature and package hashes have been checked.
#[derive(Debug, Clone)]
pub struct Bundle {
    path: PathBuf,
    pub manifest: Manifest,
    /// The name of the key that signed it.
    pub signed_by: &'static str,
    /// The SHA-256 of the whole file, lowercase hex: the bundle's identity.
    pub sha256: String,
    pub size: u64,
    entries: BTreeMap<String, Entry>,
}

/// Feeds every byte read into a whole-file hash.
struct Hashing<R> {
    inner: R,
    hash: Sha256,
    read: u64,
}

impl<R: Read> Read for Hashing<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hash.update(&buf[..n]);
        self.read += n as u64;
        Ok(n)
    }
}

fn octal(field: &[u8], what: &str) -> Result<u64, BundleError> {
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    let text = text.trim();
    if text.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(text, 8).map_err(|_| invalid(format!("bad {what} field")))
}

fn text(field: &[u8]) -> Result<String, BundleError> {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|_| invalid("a member name isn't UTF-8"))
}

/// One member's header: its name and size; `None` at the end marker.
fn header(block: &[u8; BLOCK]) -> Result<Option<(String, u64)>, BundleError> {
    if block.iter().all(|&b| b == 0) {
        return Ok(None);
    }
    if &block[257..262] != b"ustar" {
        return Err(invalid("not a ustar archive"));
    }
    let stored = octal(&block[148..156], "checksum")?;
    let sum: u64 = block
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u64::from(b)
            }
        })
        .sum();
    if sum != stored {
        return Err(invalid("a member header's checksum doesn't match"));
    }
    if !matches!(block[156], b'0' | 0) {
        return Err(invalid("only regular files belong in a bundle"));
    }
    let prefix = text(&block[345..500])?;
    let name = text(&block[..100])?;
    if !prefix.is_empty() || name.contains('/') || name.is_empty() {
        return Err(invalid(format!("{prefix}{name:?} isn't a plain file name")));
    }
    if block[124] & 0x80 != 0 {
        return Err(invalid("a member is too large"));
    }
    Ok(Some((name, octal(&block[124..136], "size")?)))
}

fn skip_padding(reader: &mut impl Read, size: u64) -> std::io::Result<()> {
    let pad = (BLOCK as u64 - size % BLOCK as u64) % BLOCK as u64;
    std::io::copy(&mut reader.take(pad), &mut std::io::sink()).map(drop)
}

impl Bundle {
    /// Reads `path` once, checks its signature against `keys` and every
    /// package against the manifest, and returns the checked bundle.
    pub fn open(path: &Path, keys: &[TrustedKey]) -> Result<Bundle, BundleError> {
        let file = File::open(path).map_err(io(path))?;
        let size = file.metadata().map_err(io(path))?.len();
        if size > MAX_BUNDLE {
            return Err(invalid(format!(
                "it is {size} bytes, more than the {MAX_BUNDLE} allowed"
            )));
        }
        let mut reader = Hashing {
            inner: BufReader::with_capacity(1 << 20, file),
            hash: Sha256::new(),
            read: 0,
        };
        let mut entries: BTreeMap<String, Entry> = BTreeMap::new();
        let mut order: Vec<String> = Vec::new();
        let (mut manifest_text, mut signature) = (Vec::new(), Vec::new());
        loop {
            let mut block = [0u8; BLOCK];
            reader.read_exact(&mut block).map_err(|e| match e.kind() {
                std::io::ErrorKind::UnexpectedEof => invalid("it ends inside the archive"),
                _ => io(path)(e),
            })?;
            let Some((name, member_size)) = header(&block)? else {
                break;
            };
            if entries.contains_key(&name) {
                return Err(invalid(format!("{name} appears twice")));
            }
            if member_size > size {
                return Err(invalid(format!("{name} is larger than the file")));
            }
            let offset = reader.read;
            let mut member = (&mut reader).take(member_size);
            let mut hash = Sha256::new();
            let small = name == MANIFEST_NAME || name == SIGNATURE_NAME;
            if small && member_size > MAX_MANIFEST {
                return Err(invalid(format!("{name} is too large")));
            }
            let mut kept = Vec::new();
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = member.read(&mut buf).map_err(io(path))?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
                if small {
                    kept.extend_from_slice(&buf[..n]);
                }
            }
            if reader.read - offset != member_size {
                return Err(invalid(format!("it ends inside {name}")));
            }
            skip_padding(&mut reader, member_size).map_err(io(path))?;
            match name.as_str() {
                MANIFEST_NAME => manifest_text = kept,
                SIGNATURE_NAME => signature = kept,
                _ => {}
            }
            order.push(name.clone());
            entries.insert(
                name,
                Entry {
                    offset,
                    size: member_size,
                    sha256: hex(&hash.finalize()),
                },
            );
        }
        std::io::copy(&mut reader, &mut std::io::sink()).map_err(io(path))?;
        let sha256 = hex(&reader.hash.finalize());

        if order.first().map(String::as_str) != Some(MANIFEST_NAME)
            || order.get(1).map(String::as_str) != Some(SIGNATURE_NAME)
        {
            return Err(invalid(format!(
                "it must start with {MANIFEST_NAME} and {SIGNATURE_NAME}"
            )));
        }
        let signed_by = keys
            .iter()
            .find(|key| {
                UnparsedPublicKey::new(&ED25519, key.public)
                    .verify(&manifest_text, &signature)
                    .is_ok()
            })
            .map(|key| key.name)
            .ok_or_else(|| {
                BundleError::Untrusted(
                    "the bundle isn't signed by a key this BMAC build trusts".into(),
                )
            })?;
        let manifest: Manifest = serde_json::from_slice(&manifest_text)
            .map_err(|e| invalid(format!("{MANIFEST_NAME}: {e}")))?;
        if manifest.format != BUNDLE_FORMAT {
            return Err(invalid(format!(
                "it has format {}, and this BMAC reads format {BUNDLE_FORMAT}",
                manifest.format
            )));
        }
        let mut listed = vec![MANIFEST_NAME.to_string(), SIGNATURE_NAME.to_string()];
        for package in &manifest.packages {
            if !package_name_ok(&package.name)
                || !file_name_ok(&package.file)
                || !digest_ok(&package.sha256)
            {
                return Err(invalid(format!(
                    "the manifest's entry for {:?} is malformed",
                    package.name
                )));
            }
            if manifest
                .packages
                .iter()
                .filter(|p| p.name == package.name)
                .count()
                > 1
            {
                return Err(invalid(format!("{} is listed twice", package.name)));
            }
            let entry = entries.get(&package.file).ok_or_else(|| {
                BundleError::Untrusted(format!("{} is listed but missing", package.file))
            })?;
            if entry.size != package.size || entry.sha256 != package.sha256 {
                return Err(BundleError::Untrusted(format!(
                    "{} doesn't match the signed manifest",
                    package.file
                )));
            }
            listed.push(package.file.clone());
        }
        if let Some(extra) = order.iter().find(|name| !listed.contains(name)) {
            return Err(BundleError::Untrusted(format!(
                "{extra} is in the bundle but not in its manifest"
            )));
        }
        Ok(Bundle {
            path: path.to_path_buf(),
            manifest,
            signed_by,
            sha256,
            size,
            entries,
        })
    }

    /// Copies the package named `package` to `to` (mode 0644), checking its
    /// hash again as it goes, since the file could have changed since
    /// [`Bundle::open`].
    pub fn extract(&self, package: &str, to: &Path) -> Result<(), BundleError> {
        let listed = self
            .manifest
            .package(package)
            .ok_or_else(|| invalid(format!("the bundle has no {package} package")))?;
        let entry = &self.entries[&listed.file];
        let mut file = File::open(&self.path).map_err(io(&self.path))?;
        file.seek(SeekFrom::Start(entry.offset))
            .map_err(io(&self.path))?;
        let partial = to.with_extension("partial");
        let result = (|| {
            let mut out = {
                use std::os::unix::fs::OpenOptionsExt;
                std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .mode(0o644)
                    .open(&partial)
                    .map_err(io(&partial))?
            };
            let mut hash = Sha256::new();
            let mut member = file.take(entry.size);
            let mut buf = vec![0u8; 1 << 16];
            loop {
                let n = member.read(&mut buf).map_err(io(&self.path))?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
                out.write_all(&buf[..n]).map_err(io(&partial))?;
            }
            out.sync_all().map_err(io(&partial))?;
            if hex(&hash.finalize()) != listed.sha256 {
                return Err(BundleError::Untrusted(format!(
                    "{} changed since the bundle was checked",
                    listed.file
                )));
            }
            std::fs::rename(&partial, to).map_err(io(to))
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result
    }
}

#[cfg(test)]
#[path = "bundle_tests.rs"]
mod tests;
