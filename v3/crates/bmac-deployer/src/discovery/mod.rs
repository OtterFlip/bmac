// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The discovery ISO and session (`DEPLOYMENT.md`, "Hardware discovery").
//!
//! The source is Alpine Extended (the pinned default, or the ISO the
//! operator chose in Settings), which boots diskless: everything runs from
//! RAM. The Deployer adds one file, `bmac-discovery.apkovl.tar.gz`, which
//! Alpine's init finds on the boot media and unpacks over the root. It holds
//! `discovery.sh` and its settings, the session's SSH host key and
//! authorized client key, the ephemeral Tailscale key, the pinned Tailscale
//! build, and a static `bmac-inventory`; OpenRC's `local` service starts the
//! script. Both boot menus are cut to one entry that starts at once.

use std::fs;
use std::net::Ipv4Addr;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use bmac_exec::{Command, CommandRunner};
use bmac_proto::inventory::{ClearReport, ClearRequest, Inventory};

use crate::download::Pinned;
use crate::hostiso::keys::SshHostKey;

const DISCOVERY_SH: &str = include_str!("discovery.sh");

/// Alpine Extended 3.24.2, checked against its published `.sha256` and
/// `latest-releases.yaml`.
pub fn alpine_iso() -> Pinned {
    Pinned {
        url: "https://dl-cdn.alpinelinux.org/alpine/v3.24/releases/x86_64/alpine-extended-3.24.2-x86_64.iso".into(),
        sha256: "a22115a46a1bcd876234a6cd43797fe19f4acfc4512fb41ea64e16cc2ffb4d90".into(),
    }
}

/// Where the overlay goes on the ISO; Alpine's init looks for
/// `*.apkovl.tar.gz` at the root of the boot media.
pub const OVERLAY_PATH: &str = "/bmac-discovery.apkovl.tar.gz";

/// What the discovery OS installs from the boot media, each on its own.
/// Not `zfs-lts`: the ZFS module is already in Alpine's module image, and
/// the package pulls in the whole kernel package and every firmware
/// package, which can't write over the read-only module mount.
const TOOLS: &str = "bash openssh-server openssh-keygen zfs lsblk blkid iproute2 ethtool \
                     pciutils util-linux";

/// The source's boot menus, rewritten by [`boot_menu`].
pub const BOOT_MENUS: [&str; 2] = ["/boot/grub/grub.cfg", "/boot/syslinux/syslinux.cfg"];

/// Tailscale's static amd64 build, checked against its published `.sha256`.
pub fn tailscale_tgz() -> Pinned {
    Pinned {
        url: "https://pkgs.tailscale.com/stable/tailscale_1.104.1_amd64.tgz".into(),
        sha256: "108d1d96ecf410d305571e173516f27038f870919a9b89c914a167c6d33a4528".into(),
    }
}

/// A static address for the public port, when the operator gives one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StaticNetwork {
    pub address: Ipv4Addr,
    pub prefix: u8,
    pub gateway: Ipv4Addr,
}

/// What the operator enters for one discovery boot.
#[derive(Clone)]
pub struct DiscoveryRequest {
    /// `None`: the port gets its address by DHCP.
    pub network: Option<StaticNetwork>,
    /// Needed with a static address; with DHCP, these replace the lease's.
    pub dns: Vec<Ipv4Addr>,
    /// The public port's MAC when known; otherwise every NIC is tried.
    pub public_mac: Option<String>,
    /// Single-use, ephemeral, tagged [`crate::tailnet::DISCOVERY_TAG`].
    pub tailscale_key: String,
    pub carrier_attempts: u32,
    pub carrier_interval_s: u32,
}

impl std::fmt::Debug for DiscoveryRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoveryRequest")
            .field("network", &self.network)
            .field("dns", &self.dns)
            .field("public_mac", &self.public_mac)
            .finish_non_exhaustive()
    }
}

/// The Deployer's side of one session, kept in memory and in a 0700
/// session directory that is deleted afterwards.
pub struct DiscoverySession {
    pub tailscale_name: String,
    /// `ssh-ed25519 AAAA...`, without a comment.
    pub host_key: String,
    pub host_fingerprint: String,
    pub client: SshHostKey,
}

impl std::fmt::Debug for DiscoverySession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscoverySession")
            .field("tailscale_name", &self.tailscale_name)
            .field("host_fingerprint", &self.host_fingerprint)
            .finish_non_exhaustive()
    }
}

/// Everything that goes into the overlay, plus the session.
pub struct PreparedDiscovery {
    pub session: DiscoverySession,
    /// Path under the root, contents, mode.
    pub files: Vec<(String, Vec<u8>, u32)>,
}

impl PreparedDiscovery {
    /// The overlay as Alpine's init unpacks it: `tar -xzf` over the root.
    pub fn overlay(&self) -> Result<Vec<u8>, String> {
        let mut tar = crate::hostiso::tar::TarWriter::new(0);
        let mut dirs = std::collections::BTreeSet::new();
        for (path, _, _) in &self.files {
            let mut parent = Path::new(path).parent();
            while let Some(dir) = parent.filter(|d| !d.as_os_str().is_empty()) {
                dirs.insert(dir.display().to_string());
                parent = dir.parent();
            }
        }
        dirs.insert("etc/runlevels/default".into());
        for dir in &dirs {
            let private = dir.starts_with("etc/bmac-discovery");
            tar.dir(dir, if private { 0o700 } else { 0o755 });
        }
        tar.symlink("etc/runlevels/default/local", "/etc/init.d/local");
        for (path, bytes, mode) in &self.files {
            tar.file(path, *mode, bytes);
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gz, &tar.finish()).map_err(|e| e.to_string())?;
        gz.finish().map_err(|e| e.to_string())
    }
}

impl std::fmt::Debug for PreparedDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let paths: Vec<&str> = self.files.iter().map(|(p, _, _)| p.as_str()).collect();
        f.debug_struct("PreparedDiscovery")
            .field("session", &self.session)
            .field("files", &paths)
            .finish()
    }
}

fn random_hex(bytes: usize) -> Result<String, String> {
    use ring::rand::SecureRandom;
    let mut buffer = vec![0u8; bytes];
    ring::rand::SystemRandom::new()
        .fill(&mut buffer)
        .map_err(|_| "no randomness available".to_string())?;
    Ok(buffer.iter().map(|b| format!("{b:02x}")).collect())
}

/// `bmac-discover-<random>`; in a lab build, under the lab prefix.
pub fn discovery_name(suffix: &str) -> String {
    if cfg!(feature = "lab") {
        format!(
            "{}discover-{suffix}",
            crate::hostiso::lab_tailscale_prefix()
        )
    } else {
        format!("bmac-discover-{suffix}")
    }
}

fn valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn key_core(public: &str) -> String {
    public
        .split_whitespace()
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The operator's values, checked before anything is downloaded or built.
pub fn check_request(request: &DiscoveryRequest) -> Result<(), String> {
    if let Some(net) = &request.network {
        if !(1..=32).contains(&net.prefix) {
            return Err("The prefix length must be from 1 to 32.".into());
        }
        let network = |ip: Ipv4Addr| u32::from(ip) >> (32 - u32::from(net.prefix));
        if net.prefix < 32 && network(net.address) != network(net.gateway) {
            return Err(format!(
                "The gateway {} is not in {}/{}.",
                net.gateway, net.address, net.prefix
            ));
        }
        if request.dns.is_empty() {
            return Err(
                "With a static address, at least one DNS server is needed to reach \
                        Tailscale."
                    .into(),
            );
        }
    }
    if let Some(mac) = &request.public_mac
        && !valid_mac(mac)
    {
        return Err(format!("{mac:?} is not a MAC address."));
    }
    let key = request.tailscale_key.trim();
    if !key.starts_with("tskey-auth-")
        || !key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err("The Tailscale key must be an auth key (tskey-auth-...).".into());
    }
    if !(1..=100).contains(&request.carrier_attempts)
        || !(1..=60).contains(&request.carrier_interval_s)
    {
        return Err("The link wait must be 1-100 checks, 1-60 seconds apart.".into());
    }
    Ok(())
}

/// Checks the request and makes the session keys and files.
pub fn prepare(
    request: &DiscoveryRequest,
    inventory: &[u8],
    tailscale: &[u8],
) -> Result<PreparedDiscovery, String> {
    check_request(request)?;
    let mac = request
        .public_mac
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let key = request.tailscale_key.trim();
    if inventory.len() < 4 || &inventory[..4] != b"\x7fELF" {
        return Err("The bundled bmac-inventory is not a Linux program.".into());
    }
    let name = discovery_name(&random_hex(4)?);
    let host = SshHostKey::generate(&format!("root@{name}"))?;
    let client = SshHostKey::generate(&format!("bmac-deployer@{name}"))?;
    let dns: Vec<String> = request.dns.iter().map(Ipv4Addr::to_string).collect();
    let env = format!(
        "ADDRESS='{}'\nPREFIX='{}'\nGATEWAY='{}'\nDNS='{}'\nPUBLIC_MAC='{mac}'\n\
         TAILSCALE_NAME='{name}'\nCARRIER_ATTEMPTS='{}'\nCARRIER_INTERVAL='{}'\n\
         TOOLS='{TOOLS}'\n",
        request
            .network
            .map_or(String::new(), |n| n.address.to_string()),
        request
            .network
            .map_or(String::new(), |n| n.prefix.to_string()),
        request
            .network
            .map_or(String::new(), |n| n.gateway.to_string()),
        dns.join(" "),
        request.carrier_attempts,
        request.carrier_interval_s,
    );
    let host_fingerprint = host.fingerprint();
    let conf = "etc/bmac-discovery";
    let files =
        vec![
        (format!("{conf}/discovery.sh"), DISCOVERY_SH.as_bytes().to_vec(), 0o755),
        (format!("{conf}/discovery.env"), env.into_bytes(), 0o600),
        (
            format!("{conf}/ssh_host_ed25519_key"),
            host.private.clone().into_bytes(),
            0o600,
        ),
        (
            format!("{conf}/ssh_host_ed25519_key.pub"),
            format!("{}\n", host.public).into_bytes(),
            0o644,
        ),
        (
            format!("{conf}/authorized_keys"),
            format!("{}\n", key_core(&client.public)).into_bytes(),
            0o600,
        ),
        (
            format!("{conf}/tailscale-auth-key"),
            format!("{key}\n").into_bytes(),
            0o600,
        ),
        (format!("{conf}/tailscale.tgz"), tailscale.to_vec(), 0o600),
        (format!("{conf}/bmac-inventory"), inventory.to_vec(), 0o755),
        ("etc/hostname".into(), format!("{name}\n").into_bytes(), 0o644),
        (
            "etc/hosts".into(),
            format!("127.0.0.1\tlocalhost\n127.0.1.1\t{name}\n").into_bytes(),
            0o644,
        ),
        ("etc/apk/world".into(), b"alpine-base\n".to_vec(), 0o644),
        // Asks init for the usual boot services, as without an overlay.
        ("etc/.default_boot_services".into(), Vec::new(), 0o644),
        (
            "etc/local.d/bmac-discovery.start".into(),
            b"#!/bin/sh\n/etc/bmac-discovery/discovery.sh >/var/log/bmac-discovery.log 2>&1 &\n"
                .to_vec(),
            0o755,
        ),
    ];
    Ok(PreparedDiscovery {
        session: DiscoverySession {
            tailscale_name: name,
            host_key: key_core(&host.public),
            host_fingerprint,
            client,
        },
        files,
    })
}

/// A source boot menu with one entry that starts at once, both consoles,
/// and every message shown. GRUB (UEFI) and syslinux (BIOS) menus are
/// rewritten alike; the kernel and its options stay the source's own.
pub fn boot_menu(source: &str) -> String {
    const TITLE: &str = "BMAC hardware discovery";
    const CONSOLES: &str = "console=tty0 console=ttyS0,115200n8";
    let mut out = String::new();
    let mut kept_entry = false;
    let mut skipping = false;
    for line in source.lines() {
        let trimmed = line.trim_start();
        let word = trimmed.split_whitespace().next().unwrap_or("");
        let entry_start = word == "menuentry" || word.eq_ignore_ascii_case("LABEL");
        if entry_start {
            skipping = kept_entry;
            kept_entry = true;
        }
        if skipping {
            continue;
        }
        let options = |rest: &str| {
            let kept: Vec<&str> = rest.split_whitespace().filter(|w| *w != "quiet").collect();
            format!("{} {CONSOLES}", kept.join(" "))
        };
        let line = if word == "set" && trimmed.starts_with("set timeout=") {
            "set timeout=0".to_string()
        } else if word.eq_ignore_ascii_case("TIMEOUT") {
            "TIMEOUT 1".to_string()
        } else if word.eq_ignore_ascii_case("PROMPT") {
            "PROMPT 0".to_string()
        } else if word == "menuentry" {
            format!("menuentry \"{TITLE}\" {{")
        } else if word.eq_ignore_ascii_case("MENU")
            && trimmed[4..].trim_start().starts_with("LABEL")
        {
            format!("MENU LABEL {TITLE}")
        } else if word == "linux" || word == "linuxefi" {
            let indent = &line[..line.len() - trimmed.len()];
            format!("{indent}{word}\t{}", options(&trimmed[word.len()..]))
        } else if word.eq_ignore_ascii_case("APPEND") {
            format!("APPEND {}", options(&trimmed[word.len()..]))
        } else {
            line.to_string()
        };
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Makes `output` from `source` under a 0700 temporary directory: reads the
/// source's boot menus, then adds the overlay and the rewritten menus with
/// `xorriso`, keeping the source's boot setup.
pub async fn build(
    prepared: &PreparedDiscovery,
    source: &Path,
    output: &Path,
    xorriso: &Path,
    runner: &dyn CommandRunner,
) -> Result<(), String> {
    let parent = output
        .parent()
        .ok_or_else(|| format!("{} has no directory", output.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let work = crate::shred::Scratch::new_in(parent, ".bmac-discovery-")?;

    let menus: Vec<(&str, PathBuf)> = BOOT_MENUS
        .iter()
        .enumerate()
        .map(|(i, menu)| (*menu, work.path().join(format!("menu{i}"))))
        .collect();
    let mut extract = Command::new(xorriso)
        .args(["-osirrox", "on", "-indev"])
        .arg(source.display().to_string());
    for (menu, local) in &menus {
        extract = extract
            .arg("-extract")
            .arg(*menu)
            .arg(local.display().to_string());
    }
    // A menu the source doesn't have is skipped below.
    let _ = runner
        .run(
            extract
                .expect_codes(&[0, 1, 32])
                .timeout(Duration::from_secs(120)),
        )
        .await;

    let overlay = work.path().join("overlay");
    fs::write(&overlay, prepared.overlay()?).map_err(|e| format!("{}: {e}", overlay.display()))?;
    fs::set_permissions(&overlay, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    let mut command = Command::new(xorriso)
        .args(["-abort_on", "FAILURE"])
        .arg("-indev")
        .arg(source.display().to_string())
        .arg("-outdev")
        .arg(work.path().join("out.iso").display().to_string())
        .arg("-map")
        .arg(overlay.display().to_string())
        .arg(OVERLAY_PATH);
    let mut found = 0;
    for (menu, local) in &menus {
        let Ok(text) = fs::read_to_string(local) else {
            continue;
        };
        found += 1;
        let edited = local.with_extension("new");
        fs::write(&edited, boot_menu(&text)).map_err(|e| e.to_string())?;
        command = command
            .arg("-map")
            .arg(edited.display().to_string())
            .arg(*menu);
    }
    if found == 0 {
        return Err(format!(
            "{} has no boot menu at {}; is it an Alpine ISO?",
            source.display(),
            BOOT_MENUS.join(" or ")
        ));
    }
    command = command
        .args(["-chmod", "0644", OVERLAY_PATH, "--"])
        .args(["-boot_image", "any", "replay"])
        .timeout(Duration::from_secs(900));
    runner
        .run(command)
        .await
        .map_err(|e| format!("xorriso: {e}"))?;
    let built = work.path().join("out.iso");
    fs::set_permissions(&built, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    fs::rename(&built, output).map_err(|e| format!("{}: {e}", output.display()))
}

/// The session directory: the client key and the pinned `known_hosts`.
pub struct SessionFiles {
    pub dir: PathBuf,
}

impl SessionFiles {
    pub fn create(base: &Path, session: &DiscoverySession) -> Result<SessionFiles, String> {
        let dir = base.join(format!("discovery-{}", session.tailscale_name));
        let _ = crate::shred::tree(&dir);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?;
        let write = |name: &str, text: &str| -> Result<(), String> {
            let path = dir.join(name);
            fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())
        };
        write("client_key", &session.client.private)?;
        write(
            "known_hosts",
            &format!("{} {}\n", session.tailscale_name, session.host_key),
        )?;
        Ok(SessionFiles { dir })
    }

    /// `ssh` to the discovery OS, accepting only the embedded host key.
    pub fn ssh(&self, ssh: &Path, session: &DiscoverySession, address: &str) -> Command {
        Command::new(ssh)
            .args([
                "-F",
                "/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "IdentitiesOnly=yes",
            ])
            .arg("-i")
            .arg(self.dir.join("client_key").display().to_string())
            .arg("-o")
            .arg(format!(
                "UserKnownHostsFile={}",
                self.dir.join("known_hosts").display()
            ))
            .args([
                "-o",
                "GlobalKnownHostsFile=none",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "HostKeyAlgorithms=ssh-ed25519",
                "-o",
                "CheckHostIP=no",
                "-o",
                "ConnectTimeout=15",
            ])
            .arg("-o")
            .arg(format!("HostKeyAlias={}", session.tailscale_name))
            .arg(format!("root@{address}"))
    }
}

impl Drop for SessionFiles {
    fn drop(&mut self) {
        let _ = crate::shred::tree(&self.dir);
    }
}

/// `bmac-inventory --json` on the discovery OS.
pub async fn read_inventory(
    runner: &dyn CommandRunner,
    ssh: &Path,
    files: &SessionFiles,
    session: &DiscoverySession,
    address: &str,
) -> Result<Inventory, String> {
    let output = runner
        .run(
            files
                .ssh(ssh, session, address)
                .arg("bmac-inventory")
                .stdin(Vec::new())
                .timeout(Duration::from_secs(600))
                .stdout_limit(16 << 20),
        )
        .await
        .map_err(|e| format!("reading the inventory: {e}"))?;
    serde_json::from_slice(&output.stdout).map_err(|e| format!("the inventory isn't valid: {e}"))
}

/// Where the discovery OS records how it brought up the public port.
pub const PUBLIC_NETWORK_FILE: &str = "/run/bmac-discovery/public-network";

/// The public port as the discovery OS brought it up, offered as the
/// host's public network.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeenNetwork {
    /// Lowercase, as the inventory has it.
    pub mac: String,
    pub address: Option<Ipv4Addr>,
    pub prefix: Option<u8>,
    pub gateway: Option<Ipv4Addr>,
    pub dhcp: bool,
}

/// Reads [`PUBLIC_NETWORK_FILE`]; anything unexpected is left out.
pub fn parse_seen_network(text: &str) -> Option<SeenNetwork> {
    let mut seen = SeenNetwork::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key {
            "MAC" => seen.mac = value.to_ascii_lowercase(),
            "ADDRESS" => {
                if let Some((ip, prefix)) = value.split_once('/') {
                    seen.address = ip.parse().ok();
                    seen.prefix = prefix.parse().ok().filter(|p| (1..=32).contains(p));
                }
            }
            "GATEWAY" => seen.gateway = value.parse().ok(),
            "METHOD" => seen.dhcp = value == "dhcp",
            _ => {}
        }
    }
    let mac_ok = seen.mac.len() == 17
        && seen
            .mac
            .split(':')
            .all(|b| b.len() == 2 && b.bytes().all(|c| c.is_ascii_hexdigit()));
    mac_ok.then_some(seen)
}

/// How the public port came up, if the discovery OS says.
pub async fn read_public_network(
    runner: &dyn CommandRunner,
    ssh: &Path,
    files: &SessionFiles,
    session: &DiscoverySession,
    address: &str,
) -> Option<SeenNetwork> {
    let output = runner
        .run(
            files
                .ssh(ssh, session, address)
                .args(["cat", PUBLIC_NETWORK_FILE])
                .stdin(Vec::new())
                .timeout(Duration::from_secs(60))
                .stdout_limit(4096),
        )
        .await
        .ok()?;
    parse_seen_network(&String::from_utf8_lossy(&output.stdout))
}

/// `bmac-inventory clear-zfs-labels` with the confirmed disks on stdin.
pub async fn clear_labels(
    runner: &dyn CommandRunner,
    ssh: &Path,
    files: &SessionFiles,
    session: &DiscoverySession,
    address: &str,
    request: &ClearRequest,
) -> Result<ClearReport, String> {
    let body = serde_json::to_vec(request).map_err(|e| e.to_string())?;
    let result = runner
        .run(
            files
                .ssh(ssh, session, address)
                .args(["bmac-inventory", "clear-zfs-labels"])
                .stdin(body)
                .expect_codes(&[0, 1])
                .timeout(Duration::from_secs(300)),
        )
        .await
        .map_err(|e| format!("clearing the labels: {e}"))?;
    serde_json::from_slice(&result.stdout)
        .map_err(|e| format!("the clearing report isn't valid: {e}"))
}

/// Shuts the discovery OS down; the connection dropping is expected.
pub async fn power_off(
    runner: &dyn CommandRunner,
    ssh: &Path,
    files: &SessionFiles,
    session: &DiscoverySession,
    address: &str,
) {
    let _ = runner
        .run(
            files
                .ssh(ssh, session, address)
                .args(["sh", "-c", "'(sleep 1; poweroff) >/dev/null 2>&1 &'"])
                .stdin(Vec::new())
                .timeout(Duration::from_secs(30)),
        )
        .await;
}

#[cfg(test)]
mod tests;
