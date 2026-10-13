// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The setup job's cluster phase on the first host (`DEPLOYMENT.md`, "Host
//! setup job" phase 5; v2's `configure_cluster_host_resolution`,
//! `cluster_setup`, and `reconcile_private_cluster_ssh_trust`): pin the
//! hosts' names to their private VLAN addresses, create the Proxmox
//! cluster with corosync's first link on the private VLAN and its second on
//! the tailnet, send migration and replication over the migration network,
//! and publish the host's SSH pin.

use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_exec::{Command, CommandRunner};
use bmac_settings::Values;
use serde_json::{Value, json};

use super::{Stop, Workflow};

pub const PVECM: &str = "/usr/bin/pvecm";
pub const TAILSCALE: &str = "/usr/bin/tailscale";
pub const GETENT: &str = "/usr/bin/getent";
pub const HOSTNAME: &str = "/usr/bin/hostname";
const HOSTS: &str = "/etc/hosts";
const ETC_HOSTNAME: &str = "/etc/hostname";
const COROSYNC_CONF: &str = "/etc/pve/corosync.conf";
const DATACENTER_CFG: &str = "/etc/pve/datacenter.cfg";
pub const PINS_BEGIN: &str = "# BEGIN bmac managed host private addresses";
pub const PINS_END: &str = "# END bmac managed host private addresses";
const SLOW: Duration = Duration::from_secs(5 * 60);
/// `pvecm status` polls before the new cluster must be quorate.
const QUORUM_POLLS: u32 = 40;

fn fail(what: &str, e: impl std::fmt::Display) -> Stop {
    Stop::Failed {
        message: format!("{what} failed"),
        details: Some(e.to_string()),
    }
}

/// Whether a name in `/etc/hosts` is a BMAC host's: `moxN` or `moxN.domain`.
fn names_host(name: &str) -> bool {
    let short = name.split_once('.').map_or(name, |(short, _)| short);
    short.parse::<HostSlot>().is_ok()
}

/// `/etc/hosts` with the hosts pinned to their private addresses in one
/// managed block. Lines naming only hosts (the installer pins its own name
/// to the public address) are dropped; a line naming a host and anything
/// else is refused rather than guessed at.
pub fn pin_hosts(
    text: &str,
    pins: &[(HostSlot, Ipv4Addr)],
    domain: &str,
) -> Result<String, String> {
    let begins = text.lines().filter(|l| *l == PINS_BEGIN).count();
    let ends = text.lines().filter(|l| *l == PINS_END).count();
    match (begins, ends) {
        (0, 0) => {}
        (1, 1) => {
            let at = |marker| text.lines().position(|l| l == marker);
            if at(PINS_BEGIN) > at(PINS_END) {
                return Err(format!("{HOSTS}'s managed block markers are out of order."));
            }
        }
        _ => {
            return Err(format!(
                "{HOSTS} has {begins} start and {ends} end markers for the managed block."
            ));
        }
    }
    let mut kept = Vec::new();
    let mut managed = false;
    for line in text.lines() {
        if line == PINS_BEGIN {
            managed = true;
            continue;
        }
        if line == PINS_END {
            managed = false;
            continue;
        }
        if managed {
            continue;
        }
        let names: Vec<&str> = line
            .split_whitespace()
            .skip(1)
            .take_while(|w| !w.starts_with('#'))
            .collect();
        let hosts = names.iter().filter(|n| names_host(n)).count();
        if line.trim_start().starts_with('#') || hosts == 0 {
            kept.push(line);
        } else if hosts != names.len() {
            return Err(format!(
                "{HOSTS} has a line naming a BMAC host and other names, which can't be \
                 replaced safely: {line}"
            ));
        }
    }
    while kept.last().is_some_and(|l| l.trim().is_empty()) {
        kept.pop();
    }
    let mut out = kept.join("\n");
    out.push_str(&format!("\n\n{PINS_BEGIN}\n"));
    for (host, ip) in pins {
        out.push_str(&format!("{ip} {host}.{domain} {host}\n"));
    }
    out.push_str(PINS_END);
    out.push('\n');
    Ok(out)
}

/// Pins every host in the stored settings to its private address in
/// `/etc/hosts` under `root`, so each member reaches a host that joined
/// after it by name. Whether the file changed.
pub fn pin_from_settings(
    root: &std::path::Path,
    settings: &crate::settings::Settings,
) -> Result<bool, String> {
    let Some(cluster) = settings.cluster().map_err(|e| e.message)? else {
        return Ok(false);
    };
    let domain = cluster
        .values
        .get("PROXMOX_INTERNAL_DOMAIN")
        .ok_or("the cluster settings have no PROXMOX_INTERNAL_DOMAIN")?;
    let mut pins = Vec::new();
    for facts in settings.hosts().map_err(|e| e.message)? {
        let ip = bmac_settings::host_private_ip(facts.host, &facts.values, &cluster.values)
            .map_err(|e| format!("{}: {}", facts.host, e.message))?;
        let ip = ip
            .parse()
            .map_err(|_| format!("{}'s private address {ip} isn't IPv4", facts.host))?;
        pins.push((facts.host, ip));
    }
    if pins.is_empty() {
        return Ok(false);
    }
    pins.sort();
    let path = root.join(HOSTS.trim_start_matches('/'));
    let text = std::fs::read_to_string(&path).map_err(|e| format!("reading {HOSTS}: {e}"))?;
    let pinned = pin_hosts(&text, &pins, domain)?;
    if pinned == text {
        return Ok(false);
    }
    super::write_atomic(&path, pinned.as_bytes())
        .and_then(|()| std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)))
        .map_err(|e| format!("writing {HOSTS}: {e}"))?;
    Ok(true)
}

/// The cluster name in `corosync.conf`.
pub fn corosync_cluster_name(text: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key.trim() == "cluster_name").then(|| value.trim().to_string())
    })
}

pub fn quorate(pvecm_status: &str) -> bool {
    pvecm_status.lines().any(|line| {
        line.split_once(':')
            .is_some_and(|(k, v)| k.trim() == "Quorate" && v.trim() == "Yes")
    })
}

/// `datacenter.cfg` with migration and replication on `network`.
pub fn datacenter_with(text: &str, network: &str) -> String {
    let mut out: String = text
        .lines()
        .filter(|l| !l.starts_with("migration:") && !l.starts_with("replication:"))
        .map(|l| format!("{l}\n"))
        .collect();
    out.push_str(&format!("migration: secure,network={network}\n"));
    out.push_str(&format!("replication: secure,network={network}\n"));
    out
}

/// Proxmox's rule for cluster names.
pub fn valid_cluster_name(name: &str) -> bool {
    (1..=15).contains(&name.len())
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The colon-separated SHA-256 in `openssl x509 -fingerprint -sha256`'s
/// output, upper case.
pub fn openssl_fingerprint(text: &str) -> Option<String> {
    let (_, value) = text.trim().split_once('=')?;
    let value = value.trim().to_ascii_uppercase();
    let hex = value.split(':').collect::<Vec<_>>();
    (hex.len() == 32
        && hex
            .iter()
            .all(|b| b.len() == 2 && b.chars().all(|c| c.is_ascii_hexdigit())))
    .then_some(value)
}

/// The cluster member a joining host joins through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinVia {
    pub host: HostSlot,
    /// Its private VLAN address.
    pub address: Ipv4Addr,
    /// The SHA-256 fingerprint of the certificate it serves on 8006.
    pub fingerprint: String,
    pub cluster_name: String,
}

/// Prints the fingerprint of the certificate served at `$1:8006`.
const SERVED_FINGERPRINT: &str = "openssl s_client -connect \"$1:8006\" -servername \"$2\" \
     </dev/null 2>/dev/null | openssl x509 -noout -fingerprint -sha256";
pub const BASH: &str = "/usr/bin/bash";
pub const SYSTEMCTL: &str = "/usr/bin/systemctl";

pub struct Cluster<'a> {
    pub flow: &'a Workflow,
    pub runner: &'a dyn CommandRunner,
    /// `/` on a host; `/etc` is read and written under it.
    pub root: PathBuf,
    pub me: HostSlot,
    pub cluster_settings: &'a Values,
    pub private_ip: Ipv4Addr,
    /// Every host whose private address is known, this one included.
    pub pins: Vec<(HostSlot, Ipv4Addr)>,
    pub poll: Duration,
}

impl Cluster<'_> {
    fn at(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn setting(&self, key: &str) -> Result<&str, Stop> {
        self.cluster_settings
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| Stop::failed(format!("The cluster settings have no {key}.")))
    }

    async fn run(&self, command: Command) -> Result<String, Stop> {
        let shown = command.to_string();
        let output = self
            .runner
            .run(command)
            .await
            .map_err(|e| fail(&shown, e))?;
        Ok(output.stdout_text())
    }

    fn read(&self, path: &str) -> Result<Option<String>, Stop> {
        match std::fs::read_to_string(self.at(path)) {
            Ok(text) => Ok(Some(text)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(fail(&format!("reading {path}"), e)),
        }
    }

    async fn pin_names(&self, domain: &str) -> Result<(), Stop> {
        let installed = self.read(ETC_HOSTNAME)?.unwrap_or_default();
        if installed.trim() != self.me.to_string() {
            return Err(Stop::failed(format!(
                "This host was installed as {}, not {}. Reinstall it with the right name; a \
                 Proxmox host isn't renamed in place.",
                installed.trim(),
                self.me
            )));
        }
        let text = self.read(HOSTS)?.unwrap_or_default();
        let pinned = pin_hosts(&text, &self.pins, domain).map_err(Stop::failed)?;
        if pinned != text {
            let path = self.at(HOSTS);
            super::write_atomic(&path, pinned.as_bytes())
                .and_then(|()| {
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                })
                .map_err(|e| fail(&format!("writing {HOSTS}"), e))?;
            let hosts: Vec<String> = self.pins.iter().map(|(h, _)| h.to_string()).collect();
            self.flow.info(format!(
                "Pinned {} to the private VLAN in {HOSTS}.",
                hosts.join(", ")
            ));
        }
        let fqdn = format!("{}.{domain}", self.me);
        for name in [self.me.to_string(), fqdn.clone()] {
            let found = self
                .run(Command::new(GETENT).args(["ahostsv4", name.as_str()]))
                .await?;
            let first = found.split_whitespace().next().unwrap_or("nothing");
            if first != self.private_ip.to_string() {
                return Err(Stop::failed(format!(
                    "{name} resolves to {first}, not this host's private address {}.",
                    self.private_ip
                )));
            }
        }
        let resolved = self.run(Command::new(HOSTNAME).arg("-f")).await?;
        if resolved.trim() != fqdn {
            return Err(Stop::failed(format!(
                "This host's full name is {}, not {fqdn}.",
                resolved.trim()
            )));
        }
        Ok(())
    }

    async fn tailscale_ip(&self) -> Result<Ipv4Addr, Stop> {
        let text = self.run(Command::new(TAILSCALE).args(["ip", "-4"])).await?;
        text.lines()
            .filter_map(|l| l.trim().parse::<Ipv4Addr>().ok())
            .find(|ip| ip.octets()[0] == 100)
            .ok_or_else(|| Stop::failed("This host has no tailnet address yet."))
    }

    async fn wait_quorate(&self) -> Result<(), Stop> {
        let mut last = String::new();
        for _ in 0..QUORUM_POLLS {
            self.flow.check()?;
            let output = self
                .runner
                .run(Command::new(PVECM).arg("status").expect_codes(&[0, 1, 2]))
                .await
                .map_err(|e| fail("pvecm status", e))?;
            last = output.stdout_text();
            if quorate(&last) {
                return Ok(());
            }
            tokio::time::sleep(self.poll).await;
        }
        Err(Stop::Failed {
            message: "The new cluster didn't become quorate.".into(),
            details: Some(last),
        })
    }

    /// Writes a file under `/etc/pve`. `pvecm status` reports quorum a
    /// moment before pmxcfs takes writes, which it refuses meanwhile.
    async fn write_pve(&self, path: &str, contents: &str) -> Result<(), Stop> {
        let mut polls = 0;
        loop {
            match std::fs::write(self.at(path), contents) {
                Ok(()) => return Ok(()),
                Err(e)
                    if polls < QUORUM_POLLS
                        && matches!(
                            e.kind(),
                            std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::ReadOnlyFilesystem
                        ) =>
                {
                    polls += 1;
                    self.flow.check()?;
                    tokio::time::sleep(self.poll).await;
                }
                Err(e) => return Err(fail(&format!("writing {path}"), e)),
            }
        }
    }

    pub async fn create(&self) -> Result<Value, Stop> {
        let name = self.setting("PROXMOX_CLUSTER_NAME")?;
        if !valid_cluster_name(name) {
            return Err(Stop::failed(format!(
                "PROXMOX_CLUSTER_NAME {name:?} isn't a Proxmox cluster name: 1 to 15 letters, \
                 digits, and hyphens, starting with a letter or digit."
            )));
        }
        let domain = self.setting("PROXMOX_INTERNAL_DOMAIN")?;
        let network = self.setting("PROXMOX_MIGRATION_NETWORK")?;
        self.pin_names(domain).await?;

        let tailnet = self.tailscale_ip().await?;
        match self
            .read(COROSYNC_CONF)?
            .as_deref()
            .map(corosync_cluster_name)
        {
            Some(Some(existing)) if existing == name => {
                self.flow
                    .info(format!("{} is already in the cluster {name}.", self.me));
            }
            Some(existing) => {
                return Err(Stop::failed(format!(
                    "{} already belongs to the cluster {}.",
                    self.me,
                    existing.unwrap_or_else(|| "with no name".into())
                )));
            }
            None => {
                self.run(
                    Command::new(PVECM)
                        .args(["create", name])
                        .arg("--link0")
                        .arg(format!("address={},priority=100", self.private_ip))
                        .arg("--link1")
                        .arg(format!("address={tailnet},priority=10"))
                        .timeout(SLOW),
                )
                .await?;
                self.flow.info(format!(
                    "Created the cluster {name}: link 0 on {} (private VLAN), link 1 on {tailnet} \
                     (tailnet).",
                    self.private_ip
                ));
            }
        }
        self.wait_quorate().await?;

        let current = self.read(DATACENTER_CFG)?.unwrap_or_default();
        let wanted = datacenter_with(&current, network);
        if wanted != current {
            self.write_pve(DATACENTER_CFG, &wanted).await?;
            self.flow
                .info(format!("Migration and replication use {network}."));
        }

        self.run(Command::new(PVECM).args(["updatecerts", "--unmerge-known-hosts"]))
            .await?;
        let pin = format!("/etc/pve/nodes/{}/ssh_known_hosts", self.me);
        if self.read(&pin)?.is_none_or(|t| t.trim().is_empty()) {
            return Err(Stop::failed(format!(
                "Proxmox didn't publish {}'s SSH pin in {pin}.",
                self.me
            )));
        }
        Ok(json!({
            "cluster_name": name,
            "link0": self.private_ip.to_string(),
            "link1": tailnet.to_string(),
        }))
    }

    /// Joins the cluster through `via` with `pvecm add`, as v2's
    /// `cluster_join` did: the certificate `via` serves must be the one the
    /// cluster vouched for, and the `root@pam` password is asked for here,
    /// handed to `pvecm` on stdin, and kept nowhere.
    pub async fn join(&self, via: &JoinVia) -> Result<Value, Stop> {
        let domain = self.setting("PROXMOX_INTERNAL_DOMAIN")?;
        self.pin_names(domain).await?;
        let tailnet = self.tailscale_ip().await?;
        match self
            .read(COROSYNC_CONF)?
            .as_deref()
            .map(corosync_cluster_name)
        {
            Some(Some(existing)) if existing == via.cluster_name => {
                self.flow
                    .info(format!("{} is already in the cluster {existing}.", self.me));
            }
            Some(existing) => {
                return Err(Stop::failed(format!(
                    "{} already belongs to the cluster {}.",
                    self.me,
                    existing.unwrap_or_else(|| "with no name".into())
                )));
            }
            None => {
                let served = self
                    .run(Command::new(BASH).args([
                        "-c",
                        SERVED_FINGERPRINT,
                        "bmac",
                        &via.address.to_string(),
                        &format!("{}.{domain}", via.host),
                    ]))
                    .await?;
                let served = openssl_fingerprint(&served).ok_or_else(|| {
                    Stop::failed(format!(
                        "{} at {} served no certificate on port 8006.",
                        via.host, via.address
                    ))
                })?;
                if served != via.fingerprint.to_ascii_uppercase() {
                    return Err(Stop::Failed {
                        message: format!(
                            "The certificate at {}:8006 isn't {}'s, so the join was stopped \
                             before sending the password.",
                            via.address, via.host
                        ),
                        details: Some(format!("expected {}\nserved   {served}", via.fingerprint)),
                    });
                }
                let password = self.password(via).await?;
                self.run(
                    Command::new(PVECM)
                        .args(["add", &via.address.to_string()])
                        .args(["--fingerprint", &served])
                        .arg("--link0")
                        .arg(format!("address={},priority=100", self.private_ip))
                        .arg("--link1")
                        .arg(format!("address={tailnet},priority=10"))
                        .secret_stdin(format!("{password}\n"))
                        .timeout(SLOW),
                )
                .await?;
                self.flow.info(format!(
                    "Joined the cluster {} through {}: link 0 on {} (private VLAN), link 1 on \
                     {tailnet} (tailnet).",
                    via.cluster_name, via.host, self.private_ip
                ));
            }
        }
        self.wait_quorate().await?;
        self.run(Command::new(PVECM).args(["updatecerts", "--unmerge-known-hosts"]))
            .await?;
        let pin = format!("/etc/pve/nodes/{}/ssh_known_hosts", self.me);
        if self.read(&pin)?.is_none_or(|t| t.trim().is_empty()) {
            return Err(Stop::failed(format!(
                "Proxmox didn't publish {}'s SSH pin in {pin}.",
                self.me
            )));
        }
        // The node's certificates are the cluster's now; bmacd's mesh and
        // Dashboard load them when they start.
        self.run(Command::new(SYSTEMCTL).args(["restart", "bmacd.service", "bmacd-web.service"]))
            .await?;
        Ok(json!({
            "cluster_name": via.cluster_name,
            "joined_through": via.host.to_string(),
            "link0": self.private_ip.to_string(),
            "link1": tailnet.to_string(),
        }))
    }

    async fn password(&self, via: &JoinVia) -> Result<String, Stop> {
        let field: bmac_core::job::Field = serde_json::from_value(json!({
            "id": "password",
            "type": "password",
            "label": format!("root@pam password on {}", via.host),
            "help": "Proxmox asks for it once to add this host; BMAC keeps it nowhere.",
            "required": true,
        }))
        .expect("a valid field");
        let values = self
            .flow
            .input_group(
                &format!("Join the cluster {}", via.cluster_name),
                Some(&format!(
                    "{} serves the certificate the cluster vouched for. Enter its root@pam \
                     password to add {} to the cluster.",
                    via.host, self.me
                )),
                vec![field],
                |values| match values.get("password").and_then(Value::as_str) {
                    Some(p) if !p.is_empty() && !p.contains('\n') => Ok(()),
                    _ => Err([("password".to_string(), "Enter the password.".to_string())].into()),
                },
            )
            .await?;
        Ok(values["password"].as_str().unwrap_or_default().to_string())
    }
}

#[cfg(test)]
#[path = "cluster_tests.rs"]
mod tests;
