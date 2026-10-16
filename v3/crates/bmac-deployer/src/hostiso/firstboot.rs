// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The first-boot executable the installer runs once on the new host:
//! `first-boot.sh` with a tar payload of the host's keys, certificate, and
//! settings appended. The installer takes at most 1 MiB, so the packages
//! go on the install media as their own tar, which the script finds by the
//! SHA-256 it carries; the operator takes the media out only once the
//! installed system's login prompt appears, after first boot.

use sha2::{Digest, Sha256};

use super::keys::{HttpsCertificate, SshHostKey};
use super::tar::TarWriter;

const TEMPLATE: &str = include_str!("first-boot.sh");
const OFFSET_WIDTH: usize = 10;
/// The largest first-boot executable the installer accepts.
pub const MAX_SIZE: usize = 1 << 20;
/// Where the packages' tar goes on the install media.
pub const MEDIA_PACKAGES: &str = "/bmac/packages.tar";

/// A package to install, by file name.
#[derive(Debug, Clone)]
pub struct Package {
    pub file_name: String,
    pub bytes: Vec<u8>,
}

pub struct FirstBoot<'a> {
    pub host: &'a str,
    pub public_ip: &'a str,
    pub tailscale_name: &'a str,
    pub authorized_keys: &'a [String],
    pub ssh_host_key: &'a SshHostKey,
    pub https: &'a HttpsCertificate,
    pub bootstrap_json: &'a [u8],
    pub tailscale_key: &'a str,
    /// The SHA-256 of [`packages_tar`]'s output, which is on the media.
    pub packages_sha256: &'a str,
    pub mtime_s: u64,
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The packages for the install media: `packages/<name>.deb`, the signed
/// bundle they came from as `bundles/<name>.bundle`, which first boot
/// leaves for bmacd to keep for a rollback, and the tar's SHA-256 for the
/// first-boot script.
pub fn packages_tar(packages: &[Package], mtime_s: u64) -> Result<(Vec<u8>, String), String> {
    if !packages.iter().any(|p| p.file_name.ends_with(".deb")) {
        return Err("No BMAC packages to put on the ISO.".into());
    }
    let mut tar = TarWriter::new(mtime_s);
    for package in packages {
        let folder = match package_name_ok(&package.file_name) {
            Some(folder) => folder,
            None => {
                return Err(format!(
                    "{:?} is not a package file name",
                    package.file_name
                ));
            }
        };
        tar.file(
            &format!("{folder}/{}", package.file_name),
            0o644,
            &package.bytes,
        );
    }
    let tar = tar.finish();
    let digest = sha256_hex(&tar);
    Ok((tar, digest))
}

fn plain(value: &str, what: &str, allowed: impl Fn(char) -> bool) -> Result<(), String> {
    if value.is_empty() || value.len() > 253 || !value.chars().all(allowed) {
        return Err(format!(
            "{what} {value:?} can't go into the first-boot script"
        ));
    }
    Ok(())
}

/// The media folder a package or bundle file goes in, if its name is one.
fn package_name_ok(name: &str) -> Option<&'static str> {
    let plain = !name.starts_with(['.', '-'])
        && name.len() <= 90
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-~".contains(c));
    if !plain {
        None
    } else if name.ends_with(".deb") {
        Some("packages")
    } else if name.ends_with(".bundle") {
        Some("bundles")
    } else {
        None
    }
}

impl FirstBoot<'_> {
    /// The payload's files, in order, with `SHA256SUMS` first.
    pub fn files(&self) -> Result<Vec<(String, u32, Vec<u8>)>, String> {
        let mut keys = self.authorized_keys.join("\n");
        keys.push('\n');
        let mut files: Vec<(String, u32, Vec<u8>)> = vec![
            (
                "ssh/ssh_host_ed25519_key".into(),
                0o600,
                self.ssh_host_key.private.clone().into_bytes(),
            ),
            (
                "ssh/ssh_host_ed25519_key.pub".into(),
                0o644,
                format!("{}\n", self.ssh_host_key.public).into_bytes(),
            ),
            ("authorized_keys".into(), 0o600, keys.into_bytes()),
            (
                "tls/https.key".into(),
                0o600,
                self.https.key_pem.clone().into_bytes(),
            ),
            (
                "tls/https.pem".into(),
                0o644,
                self.https.cert_pem.clone().into_bytes(),
            ),
            (
                "bootstrap/bootstrap.json".into(),
                0o600,
                self.bootstrap_json.to_vec(),
            ),
            (
                "tailscale/auth-key".into(),
                0o600,
                format!("{}\n", self.tailscale_key).into_bytes(),
            ),
        ];
        let sums: String = files
            .iter()
            .map(|(name, _, data)| format!("{}  {name}\n", sha256_hex(data)))
            .collect();
        files.insert(0, ("SHA256SUMS".into(), 0o600, sums.into_bytes()));
        Ok(files)
    }

    /// The executable: the script, then the payload.
    pub fn build(&self) -> Result<Vec<u8>, String> {
        plain(self.host, "The host name", |c| c.is_ascii_alphanumeric())?;
        plain(self.public_ip, "The public address", |c| {
            c.is_ascii_digit() || c == '.'
        })?;
        self.public_ip
            .parse::<std::net::Ipv4Addr>()
            .map_err(|_| format!("{:?} is not an IPv4 address", self.public_ip))?;
        plain(self.tailscale_name, "The Tailscale name", |c| {
            c.is_ascii_alphanumeric() || c == '-'
        })?;
        if self.packages_sha256.len() != 64
            || !self.packages_sha256.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err("The packages' SHA-256 isn't a digest.".into());
        }
        let mut tar = TarWriter::new(self.mtime_s);
        for (name, mode, data) in self.files()? {
            tar.file(&name, mode, &data);
        }
        let payload = tar.finish();
        let script = TEMPLATE
            .replace("__HOST__", self.host)
            .replace("__PUBLIC_IP__", self.public_ip)
            .replace("__TAILSCALE_NAME__", self.tailscale_name)
            .replace("__PACKAGES_SHA256__", self.packages_sha256)
            .replace("__MEDIA_PACKAGES__", MEDIA_PACKAGES);
        let placeholder = "__PAYLOAD_OFFSET__";
        let width_change = OFFSET_WIDTH as isize - placeholder.len() as isize;
        let offset = (script.len() as isize + width_change) as usize;
        let script = script.replace(placeholder, &format!("{offset:0OFFSET_WIDTH$}"));
        debug_assert_eq!(script.len(), offset);
        let mut out = script.into_bytes();
        out.extend_from_slice(&payload);
        if out.len() > MAX_SIZE {
            return Err(format!(
                "The first-boot file is {} bytes; the installer takes at most {MAX_SIZE}.",
                out.len()
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::process::Command;

    use super::*;

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    pub(crate) fn sample<'a>(
        ssh: &'a SshHostKey,
        https: &'a HttpsCertificate,
        keys: &'a [String],
    ) -> FirstBoot<'a> {
        FirstBoot {
            host: "mox1",
            public_ip: "203.0.113.10",
            tailscale_name: "mox1",
            authorized_keys: keys,
            ssh_host_key: ssh,
            https,
            bootstrap_json: b"{\"version\":1}",
            tailscale_key: "tskey-auth-kTEST-abcdef",
            packages_sha256: DIGEST,
            mtime_s: 1_760_000_000,
        }
    }

    #[test]
    fn the_payload_starts_where_the_script_says_and_checks_out() {
        let ssh = SshHostKey::generate("root@mox1.example.internal").unwrap();
        let https = HttpsCertificate::generate(&["mox1".into()], 1_760_000_000_000).unwrap();
        let keys = vec!["ssh-ed25519 AAAAadmin admin@example".to_string()];
        let built = sample(&ssh, &https, &keys).build().unwrap();
        let text = String::from_utf8_lossy(&built);
        let offset: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("PAYLOAD_OFFSET="))
            .unwrap()
            .parse()
            .unwrap();
        let script = &built[..offset];
        assert!(script.ends_with(b"exit 0\n"));
        assert!(!String::from_utf8_lossy(script).contains("__"));

        if Command::new("tar").arg("--version").output().is_err() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("p.tar"), &built[offset..]).unwrap();
        let out = dir.path().join("out");
        std::fs::create_dir(&out).unwrap();
        assert!(
            Command::new("tar")
                .args(["-x", "--no-same-owner", "-f"])
                .arg(dir.path().join("p.tar"))
                .arg("-C")
                .arg(&out)
                .status()
                .unwrap()
                .success()
        );
        let check = Command::new("sha256sum")
            .args(["--check", "--strict", "--quiet", "SHA256SUMS"])
            .current_dir(&out)
            .output()
            .unwrap();
        assert!(
            check.status.success(),
            "{}",
            String::from_utf8_lossy(&check.stderr)
        );
        assert_eq!(
            std::fs::read_to_string(out.join("tls/https.pem")).unwrap(),
            https.cert_pem
        );
        assert_eq!(
            std::fs::read_to_string(out.join("authorized_keys")).unwrap(),
            "ssh-ed25519 AAAAadmin admin@example\n"
        );
    }

    #[test]
    fn the_packages_tar_holds_each_package_and_its_digest_goes_in_the_script() {
        let packages = vec![Package {
            file_name: "bmacd_3.0.0~dev.0_amd64.deb".into(),
            bytes: vec![7u8; 3000],
        }];
        let (tar, digest) = packages_tar(&packages, 1_760_000_000).unwrap();
        assert_eq!(digest, sha256_hex(&tar));
        assert_eq!(&tar[..33], b"packages/bmacd_3.0.0~dev.0_amd64.");
        let ssh = SshHostKey::generate("c").unwrap();
        let https = HttpsCertificate::generate(&["mox1".into()], 0).unwrap();
        let keys = vec!["ssh-ed25519 AAAA a".to_string()];
        let mut first = sample(&ssh, &https, &keys);
        first.packages_sha256 = &digest;
        let built = String::from_utf8_lossy(&first.build().unwrap()).into_owned();
        assert!(built.contains(&format!("PACKAGES_SHA256='{digest}'")));
        assert!(built.contains(&format!("MEDIA_PACKAGES='{MEDIA_PACKAGES}'")));

        let bad = vec![Package {
            file_name: "../x.deb".into(),
            bytes: vec![1],
        }];
        assert!(packages_tar(&bad, 0).is_err());
        assert!(packages_tar(&[], 0).is_err());
    }

    #[test]
    fn the_bundle_the_packages_came_from_goes_beside_them() {
        let deb = Package {
            file_name: "bmacd_3.0.0_amd64.deb".into(),
            bytes: vec![7u8; 100],
        };
        let bundle = Package {
            file_name: "bmac-host-3.0.0.bundle".into(),
            bytes: vec![9u8; 100],
        };
        let (tar, _) = packages_tar(&[deb.clone(), bundle.clone()], 0).unwrap();
        assert_eq!(&tar[..30], b"packages/bmacd_3.0.0_amd64.deb");
        assert_eq!(&tar[1024..1054], b"bundles/bmac-host-3.0.0.bundle");
        // A bundle alone installs nothing, and other files don't go.
        assert!(packages_tar(std::slice::from_ref(&bundle), 0).is_err());
        let other = Package {
            file_name: "notes.txt".into(),
            bytes: vec![1],
        };
        assert!(packages_tar(&[deb, other], 0).is_err());
    }

    #[test]
    fn a_first_boot_file_the_installer_would_refuse_is_not_built() {
        let ssh = SshHostKey::generate("c").unwrap();
        let https = HttpsCertificate::generate(&["mox1".into()], 0).unwrap();
        let keys = vec!["ssh-ed25519 AAAA a".to_string()];
        let big = vec![b'x'; MAX_SIZE];
        let mut first = sample(&ssh, &https, &keys);
        first.bootstrap_json = &big;
        let error = first.build().unwrap_err();
        assert!(error.contains("at most 1048576"), "{error}");
    }

    #[test]
    fn the_script_is_valid_bash() {
        if Command::new("bash").arg("--version").output().is_err() {
            return;
        }
        let ssh = SshHostKey::generate("c").unwrap();
        let https = HttpsCertificate::generate(&["mox1".into()], 0).unwrap();
        let keys = vec!["ssh-ed25519 AAAA a".to_string()];
        let built = sample(&ssh, &https, &keys).build().unwrap();
        let text = String::from_utf8_lossy(&built);
        let offset: usize = text
            .lines()
            .find_map(|l| l.strip_prefix("PAYLOAD_OFFSET="))
            .unwrap()
            .parse()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("first-boot");
        std::fs::write(&path, &built[..offset]).unwrap();
        let checked = Command::new("bash").arg("-n").arg(&path).output().unwrap();
        assert!(
            checked.status.success(),
            "{}",
            String::from_utf8_lossy(&checked.stderr)
        );
    }

    #[test]
    fn values_that_could_break_the_script_are_refused() {
        let ssh = SshHostKey::generate("c").unwrap();
        let https = HttpsCertificate::generate(&["mox1".into()], 0).unwrap();
        let keys = vec![];
        let mut first = sample(&ssh, &https, &keys);
        first.tailscale_name = "mox1'; rm -rf /";
        assert!(first.build().is_err());
        let mut first = sample(&ssh, &https, &keys);
        first.public_ip = "203.0.113.300";
        assert!(first.build().is_err());
        let mut first = sample(&ssh, &https, &keys);
        first.packages_sha256 = "'; reboot; '";
        assert!(first.build().is_err());
    }
}
