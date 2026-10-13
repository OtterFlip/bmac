// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The per-host Proxmox ISO (`DEPLOYMENT.md`, "Proxmox host ISO").
//! [`prepare`] checks the inputs and makes everything that goes into the
//! image: fresh keys, the answer file, the first-boot executable, and the
//! host card. [`build`] then has Proxmox's auto-install assistant put them
//! into a copy of the standard ISO. Only the bootstrap's own secrets go in:
//! the root password as a hash, the Tailscale key, the SSH host key, the
//! HTTPS key, and on joining hosts the join token.

pub mod answer;
pub mod assistant;
pub mod firstboot;
pub mod keys;
pub mod tar;
pub mod tskey;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use bmac_core::HostSlot;
use bmac_exec::{Command, CommandRunner};
use bmac_proto::bootstrap::{
    BOOTSTRAP_VERSION, BootstrapOrigin, HostBootstrap, JoinBundle, NewClusterSeed, SettingValues,
};
use bmac_proto::inventory::{Inventory, InventoryDisk};
use bmac_storage::PoolLayout;
use serde::Serialize;

use crate::download::{self, Fetch, Pinned, Progress};
use firstboot::{FirstBoot, Package};
use keys::{HttpsCertificate, SshHostKey};

/// The standard ISO when the cluster settings don't name one
/// (`config/cluster_dot_conf` in v2).
pub const DEFAULT_SOURCE_ISO: (&str, &str) = (
    "https://enterprise.proxmox.com/iso/proxmox-ve_9.2-1.iso",
    "4e88fe416df9b527624a175f24c9aa07c714d3332afb1ee3dbf3879573ef2c6c",
);
/// The Dashboard's port.
pub const DASHBOARD_PORT: u16 = 8443;
/// Lab machines join the tailnet under a prefix, which is how a lab tells
/// its own devices apart when it removes them (`lab/README.md`). Each lab
/// has its own, passed in this variable, so two labs never share a name.
pub const LAB_TAILSCALE_PREFIX_VAR: &str = "BMAC_LAB_TAILSCALE_PREFIX";
const LAB_TAILSCALE_PREFIX: &str = "bmaclab-";

/// The lab's tailnet prefix.
///
/// # Panics
///
/// If the variable is not lowercase letters, digits and hyphens ending in
/// a hyphen; only lab builds call this.
pub fn lab_tailscale_prefix() -> String {
    let prefix = std::env::var(LAB_TAILSCALE_PREFIX_VAR)
        .unwrap_or_else(|_| LAB_TAILSCALE_PREFIX.to_string());
    let valid = prefix.ends_with('-')
        && prefix.len() > 1
        && prefix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    assert!(
        valid,
        "{LAB_TAILSCALE_PREFIX_VAR} must be lowercase letters, digits and hyphens ending in a hyphen"
    );
    prefix
}

/// The host's name on the tailnet.
pub fn tailscale_name(host: HostSlot) -> String {
    if cfg!(feature = "lab") {
        format!("{}{host}", lab_tailscale_prefix())
    } else {
        host.to_string()
    }
}

/// How the host comes into a cluster, as the operator chose.
#[derive(Debug, Clone)]
pub enum Origin {
    /// The first host; the Deployer collected the cluster settings.
    NewCluster { cluster_settings: SettingValues },
    /// Another host, from a bundle the cluster issued.
    Join(Box<JoinBundle>),
}

/// What the operator gave for one host.
#[derive(Clone)]
pub struct HostIsoRequest {
    pub host: HostSlot,
    /// The content of `moxN.conf`; for a join, the bundle's.
    pub host_settings: SettingValues,
    pub origin: Origin,
    pub inventory: Inventory,
    pub layout: PoolLayout,
    pub root_password: String,
    pub tailscale_key: String,
    /// The tailnet's MagicDNS suffix, such as `tail1234.ts.net`.
    pub magic_dns_suffix: String,
}

impl std::fmt::Debug for HostIsoRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostIsoRequest")
            .field("host", &self.host)
            .field("magic_dns_suffix", &self.magic_dns_suffix)
            .finish_non_exhaustive()
    }
}

/// What the operator keeps: how to reach the host and how to know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HostCard {
    pub host: String,
    pub cluster_name: String,
    pub tailscale_name: String,
    pub dashboard_url: String,
    pub https_fingerprint: String,
    pub ssh_host_key: String,
    pub ssh_fingerprint: String,
    /// The public address, for the operator's notes.
    pub public_ip: String,
}

/// Everything that goes into the image.
pub struct Prepared {
    pub answer_toml: String,
    pub first_boot: Vec<u8>,
    /// The packages, for the install media at [`firstboot::MEDIA_PACKAGES`].
    pub packages_tar: Vec<u8>,
    pub card: HostCard,
    pub bootstrap: HostBootstrap,
    pub tailscale_key_digest: String,
    pub source_iso: Pinned,
    pub warnings: Vec<String>,
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("card", &self.card)
            .field("warnings", &self.warnings)
            .finish_non_exhaustive()
    }
}

fn setting<'a>(values: &'a SettingValues, key: &str, what: &str) -> Result<&'a str, String> {
    values
        .get(key)
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("The {what} setting {key} is needed to build a host ISO."))
}

fn api_text(e: bmac_proto::api::ApiError) -> String {
    e.message
}

const BOOT_MIRROR_PREFIX: &str = "NVME_MIRROR_0_";

fn without_boot_mirror(values: &SettingValues) -> SettingValues {
    values
        .iter()
        .filter(|(k, _)| !k.starts_with(BOOT_MIRROR_PREFIX))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// v2's runtime reads the boot mirror from `moxN.conf` as mirror 0, so a
/// two-disk boot mirror is written there from the layout.
fn with_boot_mirror(
    values: &SettingValues,
    boot: &bmac_storage::Vdev,
    disks: &[&InventoryDisk],
) -> SettingValues {
    let mut values = without_boot_mirror(values);
    if boot.kind == bmac_storage::VdevType::Mirror && disks.len() == 2 {
        for (i, disk) in disks.iter().enumerate() {
            if let Some(serial) = &disk.serial {
                values.insert(
                    format!("{BOOT_MIRROR_PREFIX}SERIAL_{}", i + 1),
                    serial.clone(),
                );
            }
            values.insert(
                format!("{BOOT_MIRROR_PREFIX}CAPACITY_BYTES_{}", i + 1),
                disk.size_bytes.to_string(),
            );
        }
    }
    values
}

/// The inventory disk a layout member names.
fn disk_for<'a>(inventory: &'a Inventory, by_id: &str) -> Option<&'a InventoryDisk> {
    inventory
        .disks
        .iter()
        .find(|d| d.by_id.iter().any(|name| name == by_id))
}

/// Checks the request and makes the image's contents.
pub fn prepare(
    request: &HostIsoRequest,
    packages: &[Package],
    now_ms: i64,
    accept_reused_key: bool,
) -> Result<Prepared, String> {
    let host = request.host;
    let host_name = host.to_string();
    let mut warnings = Vec::new();

    let (cluster, used_digests, setup_key) = match &request.origin {
        Origin::NewCluster { cluster_settings } => (cluster_settings, Vec::new(), None),
        Origin::Join(bundle) => {
            if bundle.slot != host {
                return Err(format!(
                    "The join bundle is for {}, not {host}.",
                    bundle.slot
                ));
            }
            let changed: Vec<String> = without_boot_mirror(&bundle.host_settings)
                .into_iter()
                .filter(|(key, value)| request.host_settings.get(key) != Some(value))
                .map(|(key, _)| key)
                .collect();
            if !changed.is_empty() {
                return Err(format!(
                    "The host settings change what the join bundle set: {}.",
                    changed.join(", ")
                ));
            }
            if bundle.expires_ms <= now_ms {
                return Err(
                    "The join bundle has expired. Issue a new one from the Dashboard.".into(),
                );
            }
            (
                &bundle.cluster_settings,
                bundle.used_tailscale_key_sha256.clone(),
                Some(bundle.setup_ssh_public_key.clone()),
            )
        }
    };
    bmac_settings::validate_cluster(cluster).map_err(api_text)?;
    let cluster_name = setting(cluster, "PROXMOX_CLUSTER_NAME", "cluster")?;
    let domain = setting(cluster, "PROXMOX_INTERNAL_DOMAIN", "cluster")?;
    let fqdn = format!("{host_name}.{domain}");
    let hs = &request.host_settings;
    let public_ip = setting(hs, "PROXMOX_IP", "host")?;
    let public_mac = setting(hs, "PROXMOX_PUBLIC_MAC", "host")?;
    let private_ip = bmac_settings::host_private_ip(host, hs, cluster).map_err(api_text)?;

    if !request
        .inventory
        .nics
        .iter()
        .any(|nic| nic.mac.eq_ignore_ascii_case(public_mac))
    {
        return Err(format!(
            "No network interface in the hardware inventory has the public MAC address \
             {public_mac}. Check the host settings against the discovery session."
        ));
    }

    request.layout.validate().map_err(|e| e.to_string())?;
    let boots: Vec<_> = request.layout.vdevs.iter().filter(|v| v.boot).collect();
    let [boot] = boots.as_slice() else {
        return Err("The pool layout needs exactly one boot vdev.".into());
    };
    let mut boot_disks = Vec::new();
    for vdev in &request.layout.vdevs {
        for member in &vdev.members {
            let disk = disk_for(&request.inventory, member.as_str()).ok_or_else(|| {
                format!("{member} is in the pool layout but not in the hardware inventory.")
            })?;
            if !disk.problems.is_empty() {
                return Err(format!("{}: {}", disk.name, disk.problems.join("; ")));
            }
            if vdev.boot {
                boot_disks.push(disk);
            }
        }
    }
    let host_settings = with_boot_mirror(&request.host_settings, boot, &boot_disks);
    bmac_settings::validate_host(host, &host_settings, cluster).map_err(api_text)?;
    let hdsize = answer::boot_hdsize_gib(&boot_disks)?;
    let filters = answer::disk_filters(&boot_disks, &request.inventory.disks)?;
    let password_hash =
        answer::root_password_hash(&request.root_password, cluster_name, &host_name)?;

    let key = tskey::check_key(&request.tailscale_key, &used_digests, accept_reused_key)?;
    if key.reused {
        warnings
            .push("This Tailscale auth key was used before; a lab build accepts it.".to_string());
    }
    let mut digests = used_digests;
    if !digests.contains(&key.digest) {
        digests.push(key.digest.clone());
    }

    let mut admin_keys: Vec<String> = ["ADMIN_1_PUBLIC_SSH_KEY", "ADMIN_2_PUBLIC_SSH_KEY"]
        .iter()
        .filter_map(|k| cluster.get(*k))
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
        .collect();
    if admin_keys.is_empty() {
        return Err(
            "At least one administrator SSH key (ADMIN_1_PUBLIC_SSH_KEY) is needed, or nobody \
             could log in to the host."
                .into(),
        );
    }
    let mut authorized = admin_keys.clone();
    if let Some(setup) = setup_key {
        authorized.push(setup);
    }
    admin_keys.dedup();

    let suffix = request.magic_dns_suffix.trim().trim_matches('.');
    if suffix.is_empty()
        || !suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
    {
        return Err(format!("{suffix:?} is not a MagicDNS suffix."));
    }
    let ts_name = tailscale_name(host);
    let magic_name = format!("{ts_name}.{suffix}");
    let ssh_host_key = SshHostKey::generate(&format!("root@{fqdn}"))?;
    let https = HttpsCertificate::generate(
        &[
            host_name.clone(),
            fqdn.clone(),
            magic_name.clone(),
            private_ip.clone(),
        ],
        now_ms,
    )?;

    let origin = match &request.origin {
        Origin::NewCluster { cluster_settings } => BootstrapOrigin::NewCluster(NewClusterSeed {
            cluster_uuid: uuid::Uuid::new_v4().to_string(),
            cluster_settings: cluster_settings.clone(),
            used_tailscale_key_sha256: digests,
        }),
        Origin::Join(bundle) => {
            let mut bundle = (**bundle).clone();
            bundle.used_tailscale_key_sha256 = digests;
            BootstrapOrigin::Join(bundle)
        }
    };
    let bootstrap = HostBootstrap {
        version: BOOTSTRAP_VERSION,
        host,
        host_settings: host_settings.clone(),
        origin,
        layout: request.layout.clone(),
        inventory: request.inventory.clone(),
        https_fingerprint: https.fingerprint.clone(),
        ssh_host_key: ssh_host_key.public.clone(),
        tailscale_name: ts_name.clone(),
        built_ms: now_ms,
        deployer_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let bootstrap_json =
        serde_json::to_vec_pretty(&bootstrap).map_err(|e| format!("the bootstrap file: {e}"))?;

    let cidr = format!("{public_ip}/{}", setting(hs, "PROXMOX_PREFIX", "host")?);
    let answer_toml = answer::Answer {
        keyboard: setting(cluster, "PROXMOX_KEYBOARD", "cluster")?,
        country: setting(cluster, "PROXMOX_COUNTRY", "cluster")?,
        fqdn: &fqdn,
        mailto: setting(cluster, "PROXMOX_ADMIN_EMAIL", "cluster")?,
        timezone: setting(cluster, "PROXMOX_TIMEZONE", "cluster")?,
        root_password_hash: &password_hash,
        root_ssh_keys: &admin_keys,
        cidr: &cidr,
        dns: setting(cluster, "PROXMOX_DNS_SERVER", "cluster")?,
        gateway: setting(hs, "PROXMOX_GATEWAY", "host")?,
        public_mac,
        boot,
        filters: &filters,
        ashift: request.layout.ashift,
        hdsize_gib: hdsize,
    }
    .toml();

    let mtime_s = (now_ms / 1000).max(0) as u64;
    let (packages_tar, packages_sha256) = firstboot::packages_tar(packages, mtime_s)?;
    let first_boot = FirstBoot {
        host: &host_name,
        public_ip,
        tailscale_name: &ts_name,
        authorized_keys: &authorized,
        ssh_host_key: &ssh_host_key,
        https: &https,
        bootstrap_json: &bootstrap_json,
        tailscale_key: &key.key,
        packages_sha256: &packages_sha256,
        mtime_s,
    }
    .build()?;

    let source_iso = match (
        cluster.get("PROXMOX_ISO_FILE_URL"),
        cluster.get("PROXMOX_ISO_FILE_SHA256"),
    ) {
        (Some(url), Some(sha)) => Pinned {
            url: url.clone(),
            sha256: sha.to_ascii_lowercase(),
        },
        (None, None) => Pinned {
            url: DEFAULT_SOURCE_ISO.0.into(),
            sha256: DEFAULT_SOURCE_ISO.1.into(),
        },
        _ => {
            return Err(
                "PROXMOX_ISO_FILE_URL and PROXMOX_ISO_FILE_SHA256 go together; set both or \
                 neither."
                    .into(),
            );
        }
    };

    Ok(Prepared {
        answer_toml,
        first_boot,
        packages_tar,
        card: HostCard {
            host: host_name,
            cluster_name: cluster_name.to_string(),
            tailscale_name: ts_name,
            dashboard_url: format!("https://{magic_name}:{DASHBOARD_PORT}/"),
            https_fingerprint: https.fingerprint.clone(),
            ssh_fingerprint: ssh_host_key.fingerprint(),
            ssh_host_key: ssh_host_key.public.clone(),
            public_ip: public_ip.to_string(),
        },
        bootstrap,
        tailscale_key_digest: key.digest,
        source_iso,
        warnings,
    })
}

/// The packages in `dir` (`*.deb`), sorted by name.
pub fn read_packages(dir: &Path) -> Result<Vec<Package>, String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".deb"))
        .collect();
    names.sort();
    if names.is_empty() {
        return Err(format!("{} holds no BMAC packages", dir.display()));
    }
    names
        .into_iter()
        .map(|name| {
            let bytes = fs::read(dir.join(&name)).map_err(|e| format!("{name}: {e}"))?;
            Ok(Package {
                file_name: name,
                bytes,
            })
        })
        .collect()
}

/// Where things go and what runs.
pub struct BuildEnv<'a> {
    /// The download cache: the standard ISO and the assistant.
    pub cache_dir: &'a Path,
    /// Where finished ISOs go.
    pub iso_dir: &'a Path,
    pub runner: &'a dyn CommandRunner,
    /// `dpkg-deb`, by absolute path.
    pub dpkg_deb: &'a Path,
    /// `xorriso`, by absolute path, to add the packages.
    pub xorriso: &'a Path,
    pub fetch: &'a dyn Fetch,
    pub progress: Progress<'a>,
    pub note: &'a (dyn Fn(String) + Send + Sync),
}

/// A finished ISO.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuiltHostIso {
    pub path: PathBuf,
    pub sha256: String,
}

fn source_stem(url: &str) -> String {
    download::cache_name(url)
        .ok()
        .and_then(|n| n.strip_suffix(".iso").map(str::to_string))
        .unwrap_or_else(|| "proxmox-ve".into())
}

/// Puts the prepared contents into a copy of the standard ISO.
pub async fn build(prepared: &Prepared, env: &BuildEnv<'_>) -> Result<BuiltHostIso, String> {
    let assistant =
        assistant::prepare(env.cache_dir, env.fetch, env.runner, env.dpkg_deb, env.note).await?;
    fs::create_dir_all(env.iso_dir).map_err(|e| format!("{}: {e}", env.iso_dir.display()))?;
    let work = crate::shred::Scratch::new_in(env.iso_dir, ".bmac-host-iso-")?;
    let answer_path = work.path().join("answer.toml");
    let first_boot_path = work.path().join("first-boot");
    write_private(&answer_path, prepared.answer_toml.as_bytes(), 0o600)?;
    write_private(&first_boot_path, &prepared.first_boot, 0o700)?;

    let run =
        |command: Command| async move { env.runner.run(command).await.map_err(|e| e.to_string()) };
    (env.note)("Checking the answer file with the auto-install assistant.".into());
    run(Command::new(&assistant)
        .arg("validate-answer")
        .arg(answer_path.display().to_string()))
    .await?;

    let source = download::ensure(
        &env.cache_dir.join("source-iso"),
        &prepared.source_iso,
        env.fetch,
        env.progress,
        env.note,
    )
    .await?;
    let output = env.iso_dir.join(format!(
        "{}-{}-auto.iso",
        source_stem(&prepared.source_iso.url),
        prepared.card.host
    ));
    crate::shred::file(&output).map_err(|e| format!("{}: {e}", output.display()))?;
    let pending = crate::shred::Pending::new(output.clone());
    (env.note)(format!("Building {}.", output.display()));
    let auto = work.path().join("auto.iso");
    let said = run(Command::new(&assistant)
        .arg("prepare-iso")
        .arg(source.display().to_string())
        .args(["--fetch-from", "iso", "--answer-file"])
        .arg(answer_path.display().to_string())
        .arg("--on-first-boot")
        .arg(first_boot_path.display().to_string())
        .arg("--output")
        .arg(auto.display().to_string())
        .timeout(std::time::Duration::from_secs(1800)))
    .await?;
    // The assistant reports some refusals on stdout and still exits 0.
    if !auto.is_file() {
        let printed = format!("{}{}", said.stdout_text(), said.stderr_text());
        let tail: Vec<&str> = printed.lines().rev().take(5).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        return Err(format!(
            "The auto-install assistant didn't build the ISO. It said: {}",
            tail.join(" / ")
        ));
    }
    let packages = work.path().join("packages.tar");
    write_private(&packages, &prepared.packages_tar, 0o644)?;
    // Added in place as a new session, as the assistant adds its own files:
    // replaying the boot setup into a fresh image fails on this ISO's MBR.
    run(Command::new(env.xorriso)
        .args(["-abort_on", "FAILURE", "-boot_image", "any", "keep", "-dev"])
        .arg(auto.display().to_string())
        .arg("-map")
        .arg(packages.display().to_string())
        .arg(firstboot::MEDIA_PACKAGES)
        .args(["-chmod", "0444", firstboot::MEDIA_PACKAGES, "--"])
        .timeout(std::time::Duration::from_secs(900)))
    .await
    .map_err(|e| format!("xorriso: {e}"))?;
    fs::rename(&auto, &output).map_err(|e| format!("{}: {e}", output.display()))?;
    fs::set_permissions(&output, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    run(Command::new(&assistant)
        .arg("inspect-iso")
        .arg(output.display().to_string()))
    .await?;
    let path = output.clone();
    let sha256 = tokio::task::spawn_blocking(move || download::file_sha256(&path))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{}: {e}", output.display()))?;
    Ok(BuiltHostIso {
        path: pending.keep(),
        sha256,
    })
}

fn write_private(path: &Path, data: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(mode)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    file.write_all(data)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests;
