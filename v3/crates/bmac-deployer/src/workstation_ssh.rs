// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `ssh mox1` from the workstation (`DEPLOYMENT.md`, "Workstation SSH"),
//! after v2's `write_workstation_ssh_config`. A managed block at the top of
//! `~/.ssh/config` sends the chosen names to the host's tailnet address and
//! accepts only the pinned Ed25519 key, stored in `~/.ssh/known_hosts`
//! under a `HostKeyAlias`. The block is prepended so OpenSSH's
//! first-value-wins rule overrides older stanzas for those names without
//! deleting anything of the operator's, and it ends with `Host *` so
//! top-level options further down stay global. Both files are proven with
//! `ssh -G` and `ssh <name> true`, and restored when the proof fails.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bmac_exec::{Command, CommandRunner, ExecError};
use bmac_proto::deployer::SshEntry;

/// One machine's pin: a cluster host reached directly over the tailnet,
/// or a guest reached through a host with `ProxyJump`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostPin {
    /// The `HostKeyAlias` and the block's marker, unique per cluster.
    pub alias: String,
    /// The tailnet address of a host, or the private address of a guest.
    pub address: String,
    /// `ssh-ed25519 AAAA...`, without a comment.
    pub host_key: String,
    /// For a guest, the workstation's SSH name for the jump host.
    pub jump: Option<String>,
}

impl HostPin {
    fn kind(&self) -> &'static str {
        if self.jump.is_some() { "guest" } else { "host" }
    }
}

/// The `HostKeyAlias` for a cluster host or guest, so two clusters' `mox1`
/// or `prod1` never share a pin.
pub fn pin_alias(cluster: &str, machine: &str) -> String {
    format!("bmac-{cluster}-{machine}")
}

/// Names the operator may give a host.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && name.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

fn markers(pin: &HostPin) -> (String, String) {
    let (kind, alias) = (pin.kind(), &pin.alias);
    (
        format!("# BEGIN bmac managed {kind} {alias}"),
        format!("# END bmac managed {kind} {alias}"),
    )
}

/// `~/.ssh/config` with this host's block replaced and put first.
pub fn merge_config(
    existing: &str,
    pin: &HostPin,
    names: &[String],
    known_hosts: &Path,
) -> Result<String, String> {
    let (begin, end) = markers(pin);
    let mut kept = Vec::new();
    let mut inside = false;
    for line in existing.lines() {
        if !inside && line == begin {
            inside = true;
        } else if inside && line == end {
            inside = false;
        } else if !inside {
            kept.push(line);
        }
    }
    if inside {
        return Err(format!(
            "The managed SSH block for {} in ~/.ssh/config has no end marker; fix or remove it by hand.",
            pin.alias
        ));
    }
    let block = [
        begin,
        format!("Host {}", names.join(" ")),
        format!("    HostName {}", pin.address),
        "    User root".into(),
        "    Port 22".into(),
        format!("    ProxyJump {}", pin.jump.as_deref().unwrap_or("none")),
        format!("    HostKeyAlias {}", pin.alias),
        "    HostKeyAlgorithms ssh-ed25519".into(),
        "    StrictHostKeyChecking yes".into(),
        "    UpdateHostKeys no".into(),
        format!("    UserKnownHostsFile \"{}\"", known_hosts.display()),
        "    PasswordAuthentication no".into(),
        "    KbdInteractiveAuthentication no".into(),
        "Host *".into(),
        end,
        String::new(),
    ];
    let mut lines: Vec<&str> = block.iter().map(String::as_str).collect();
    lines.extend(kept.into_iter().skip_while(|l| l.trim().is_empty()));
    Ok(format!("{}\n", lines.join("\n").trim_end()))
}

/// Whether a `known_hosts` host pattern field names `name` on port 22,
/// plainly or hashed.
fn names_host(field: &str, name: &str) -> bool {
    if let Some(hashed) = field.strip_prefix("|1|") {
        let Some((salt, digest)) = hashed.split_once('|') else {
            return false;
        };
        let (Ok(salt), Ok(digest)) = (STANDARD.decode(salt), STANDARD.decode(digest)) else {
            return false;
        };
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        return ring::hmac::verify(&key, name.as_bytes(), &digest).is_ok();
    }
    field.split(',').any(|pattern| pattern == name)
}

/// `known_hosts` with every entry for `names` and the alias removed, as
/// `ssh-keygen -R` does, and the pin appended.
pub fn merge_known_hosts(existing: &str, pin: &HostPin, names: &[String]) -> String {
    let mut out = String::new();
    for line in existing.lines() {
        let field = line.split_whitespace().next().unwrap_or("");
        let stale = !line.trim_start().starts_with('#')
            && !field.starts_with('@')
            && names
                .iter()
                .chain(std::iter::once(&pin.alias))
                .any(|name| names_host(field, name));
        if !stale {
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push_str(&format!("{} {}\n", pin.alias, pin.host_key));
    out
}

/// Whether `ssh -G` output resolves to the managed settings.
pub fn settings_match(effective: &str, pin: &HostPin, known_hosts: &Path) -> bool {
    let lines: Vec<&str> = effective.lines().collect();
    let has = |want: String| lines.contains(&want.as_str());
    has(format!("hostname {}", pin.address))
        && has("user root".into())
        && has("port 22".into())
        && has(format!("hostkeyalias {}", pin.alias))
        && has("hostkeyalgorithms ssh-ed25519".into())
        && has("stricthostkeychecking true".into())
        && has(format!("userknownhostsfile {}", known_hosts.display()))
        && !lines.iter().any(|l| l.starts_with("proxycommand "))
        && match &pin.jump {
            Some(jump) => has(format!("proxyjump {jump}")),
            None => !lines.iter().any(|l| l.starts_with("proxyjump ")),
        }
}

/// What to do by hand when the operator declines or the proof fails.
pub fn manual_instructions(name: &str, pin: &HostPin) -> Vec<String> {
    vec![
        format!(
            "To set up \"ssh {name}\" yourself without trusting a first-use prompt, put this \
             entry at the top of ~/.ssh/config:"
        ),
        format!(
            "Host {name}\n    HostName {}\n    User root\n    ProxyJump {}\n    HostKeyAlias {}\n    \
             HostKeyAlgorithms ssh-ed25519\n    StrictHostKeyChecking yes\nHost *",
            pin.address,
            pin.jump.as_deref().unwrap_or("none"),
            pin.alias
        ),
        "and add this line to ~/.ssh/known_hosts:".into(),
        format!("{} {}", pin.alias, pin.host_key),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SshSetupError {
    /// The host presented a different key: a hard stop.
    Mismatch(String),
    /// Anything else; both files were restored.
    Failed(String),
}

impl std::fmt::Display for SshSetupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SshSetupError::Mismatch(m) | SshSetupError::Failed(m) => f.write_str(m),
        }
    }
}

/// Where the files are and how hard to try.
pub struct SshSetup<'a> {
    /// `~/.ssh`.
    pub ssh_dir: PathBuf,
    /// The Deployer's state directory: the lock and the backups.
    pub state_dir: PathBuf,
    pub ssh: &'a Path,
    pub runner: &'a dyn CommandRunner,
    pub attempts: u32,
    pub wait: Duration,
    pub lock_timeout: Duration,
    pub note: &'a (dyn Fn(String) + Send + Sync),
}

const BACKUPS_KEPT: usize = 20;

fn refuse_symlink(path: &Path) -> Result<(), SshSetupError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(SshSetupError::Failed(format!(
            "Refusing to manage {} because it is a symbolic link.",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn read_optional(path: &Path) -> Result<Option<String>, SshSetupError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(SshSetupError::Failed(format!("{}: {e}", path.display()))),
    }
}

/// Replaces `path` with `text`, mode 0600, through a file in the same
/// directory.
fn replace(path: &Path, text: &str) -> Result<(), SshSetupError> {
    let fail = |e: std::io::Error| SshSetupError::Failed(format!("{}: {e}", path.display()));
    let partial = path.with_extension(format!("bmac-{}", std::process::id()));
    let _ = fs::remove_file(&partial);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&partial)
        .map_err(fail)?;
    file.write_all(text.as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    drop(file);
    fs::set_permissions(&partial, fs::Permissions::from_mode(0o600)).map_err(fail)?;
    fs::rename(&partial, path).map_err(fail)
}

fn restore(path: &Path, before: &Option<String>) {
    match before {
        Some(text) => {
            let _ = replace(path, text);
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

/// Keeps a timestamped copy of both files and prunes old copies.
fn back_up(
    state_dir: &Path,
    config: &Option<String>,
    known: &Option<String>,
) -> Result<(), SshSetupError> {
    let fail = |e: std::io::Error| SshSetupError::Failed(format!("backing up ~/.ssh: {e}"));
    let root = state_dir.join("ssh-backups");
    fs::create_dir_all(&root).map_err(fail)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).map_err(fail)?;
    let stamp = jiff::Timestamp::now()
        .strftime("%Y%m%dT%H%M%S%.3fZ")
        .to_string();
    let dir = root.join(stamp);
    fs::create_dir(&dir).map_err(fail)?;
    for (name, text) in [("config", config), ("known_hosts", known)] {
        if let Some(text) = text {
            replace(&dir.join(name), text)?;
        }
    }
    let mut existing: Vec<PathBuf> = fs::read_dir(&root)
        .map_err(fail)?
        .flatten()
        .map(|e| e.path())
        .collect();
    existing.sort();
    let extra = existing.len().saturating_sub(BACKUPS_KEPT);
    for old in &existing[..extra] {
        let _ = fs::remove_dir_all(old);
    }
    Ok(())
}

async fn lock(state_dir: &Path, timeout: Duration) -> Result<File, SshSetupError> {
    fs::create_dir_all(state_dir)
        .map_err(|e| SshSetupError::Failed(format!("{}: {e}", state_dir.display())))?;
    let path = state_dir.join("workstation-ssh.lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|e| SshSetupError::Failed(format!("{}: {e}", path.display())))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(SshSetupError::Failed(
                    "Timed out waiting for another Deployer task to finish editing ~/.ssh/config."
                        .into(),
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(SshSetupError::Failed(format!("{}: {e}", path.display())));
            }
        }
    }
}

enum Probe {
    Ok,
    Retry(String),
    Stop(SshSetupError),
}

fn classify(result: Result<bmac_exec::Output, ExecError>, name: &str, pin: &HostPin) -> Probe {
    let text = match result {
        Ok(_) => return Probe::Ok,
        Err(e) => match e.output() {
            Some(output) => output.stderr_text(),
            None => e.to_string(),
        },
    };
    if text.contains("Host key verification failed")
        || text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED")
    {
        return Probe::Stop(SshSetupError::Mismatch(format!(
            "ssh {name} ({}) presented a host key other than the one pinned for {}. This could \
             be a man-in-the-middle attack or a different machine at that address. Nothing was \
             changed in ~/.ssh.",
            pin.address, pin.alias
        )));
    }
    if text.contains("Permission denied") {
        return Probe::Stop(SshSetupError::Failed(format!(
            "this workstation's SSH key is not authorized for root on {} (it must be one of the \
             cluster's administrator keys)",
            pin.alias
        )));
    }
    Probe::Retry(text.lines().last().unwrap_or("").to_string())
}

impl SshSetup<'_> {
    fn ssh(&self) -> Command {
        let command = Command::new(self.ssh)
            .inherit_env("HOME")
            .inherit_env("USER")
            .inherit_env("SSH_AUTH_SOCK");
        // ssh reads the passwd home's ~/.ssh/config, whatever HOME says, so a
        // lab run with its own ssh_dir names the file.
        if cfg!(feature = "lab") {
            command
                .arg("-F")
                .arg(self.ssh_dir.join("config").display().to_string())
        } else {
            command
        }
    }

    async fn prove(
        &self,
        pin: &HostPin,
        names: &[String],
        known_hosts: &Path,
    ) -> Result<(), SshSetupError> {
        for name in names {
            let effective = self
                .runner
                .run(
                    self.ssh()
                        .args(["-G", name.as_str()])
                        .timeout(Duration::from_secs(10)),
                )
                .await
                .map(|o| o.stdout_text())
                .unwrap_or_default();
            if !settings_match(&effective, pin, known_hosts) {
                return Err(SshSetupError::Failed(format!(
                    "ssh -G {name} does not resolve to the managed settings (a Match block or \
                     Include may be overriding them)"
                )));
            }
        }
        let name = &names[0];
        let mut last = String::new();
        for attempt in 1..=self.attempts {
            let probe = self
                .ssh()
                .args([
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=15",
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "ControlPath=none",
                    name.as_str(),
                    "true",
                ])
                .stdin(Vec::new())
                .timeout(Duration::from_secs(30));
            match classify(self.runner.run(probe).await, name, pin) {
                Probe::Ok => return Ok(()),
                Probe::Stop(error) => return Err(error),
                Probe::Retry(text) => last = text,
            }
            if attempt < self.attempts {
                (self.note)(format!(
                    "Waiting for SSH on {} ({}), attempt {attempt}/{}...",
                    pin.alias, pin.address, self.attempts
                ));
                tokio::time::sleep(self.wait).await;
            }
        }
        Err(SshSetupError::Failed(format!(
            "ssh {name} did not connect after {} attempts: {last}",
            self.attempts
        )))
    }

    /// Writes the block and the pin for `names`, then proves them. Returns
    /// whether anything changed.
    pub async fn apply(&self, pin: &HostPin, names: &[String]) -> Result<bool, SshSetupError> {
        if names.is_empty() || !names.iter().all(|n| valid_name(n)) {
            return Err(SshSetupError::Failed(
                "SSH names use 1-64 letters, digits, dots, underscores, or hyphens.".into(),
            ));
        }
        let dir_text = self.ssh_dir.display().to_string();
        if !self.ssh_dir.is_absolute() || dir_text.contains(['"', '\n']) {
            return Err(SshSetupError::Failed(
                "The home directory must be an absolute path without quotes or newlines to manage ~/.ssh/config.".into(),
            ));
        }
        let config = self.ssh_dir.join("config");
        let known_hosts = self.ssh_dir.join("known_hosts");
        for path in [&self.ssh_dir, &config, &known_hosts] {
            refuse_symlink(path)?;
        }
        let _lock = lock(&self.state_dir, self.lock_timeout).await?;
        fs::create_dir_all(&self.ssh_dir)
            .and_then(|_| fs::set_permissions(&self.ssh_dir, fs::Permissions::from_mode(0o700)))
            .map_err(|e| SshSetupError::Failed(format!("{dir_text}: {e}")))?;
        let config_before = read_optional(&config)?;
        let known_before = read_optional(&known_hosts)?;
        let config_after = merge_config(
            config_before.as_deref().unwrap_or(""),
            pin,
            names,
            &known_hosts,
        )
        .map_err(SshSetupError::Failed)?;
        let known_after = merge_known_hosts(known_before.as_deref().unwrap_or(""), pin, names);
        let changed = config_before.as_deref() != Some(config_after.as_str())
            || known_before.as_deref() != Some(known_after.as_str());
        if changed {
            back_up(&self.state_dir, &config_before, &known_before)?;
            replace(&known_hosts, &known_after)?;
            if let Err(e) = replace(&config, &config_after) {
                restore(&known_hosts, &known_before);
                return Err(e);
            }
        }
        match self.prove(pin, names, &known_hosts).await {
            Ok(()) => Ok(changed),
            Err(error) => {
                if changed {
                    (self.note)(
                        "Restoring the previous ~/.ssh/config and ~/.ssh/known_hosts.".into(),
                    );
                    restore(&known_hosts, &known_before);
                    restore(&config, &config_before);
                }
                Err(error)
            }
        }
    }
}

/// The managed entries in `config`, with what the workstation's own files
/// show is wrong with each: no pin in `known_hosts`, or a jump host with no
/// entry of its own.
pub fn managed_entries(config: &str, known_hosts: &str) -> Vec<SshEntry> {
    let mut entries: Vec<SshEntry> = Vec::new();
    let mut current: Option<SshEntry> = None;
    for line in config.lines() {
        if let Some(rest) = line.strip_prefix("# BEGIN bmac managed ") {
            current = rest.split_once(' ').map(|(kind, alias)| SshEntry {
                kind: kind.into(),
                alias: alias.into(),
                names: Vec::new(),
                address: String::new(),
                jump: None,
                problem: None,
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if line.starts_with("# END bmac managed ") {
            entries.extend(current.take());
        } else if let Some(names) = line.strip_prefix("Host ")
            && names != "*"
        {
            entry.names = names.split_whitespace().map(str::to_string).collect();
        } else if let Some(address) = line.trim().strip_prefix("HostName ") {
            entry.address = address.into();
        } else if let Some(jump) = line.trim().strip_prefix("ProxyJump ")
            && jump != "none"
        {
            entry.jump = Some(jump.into());
        }
    }
    let host_names: Vec<String> = entries
        .iter()
        .filter(|e| e.kind == "host")
        .flat_map(|e| e.names.clone())
        .collect();
    for entry in &mut entries {
        let pinned = known_hosts
            .lines()
            .any(|l| l.split_whitespace().next() == Some(entry.alias.as_str()));
        entry.problem = if !pinned {
            Some("Its key isn't pinned in ~/.ssh/known_hosts; set it up again.".into())
        } else {
            entry
                .jump
                .as_ref()
                .filter(|jump| !host_names.contains(jump))
                .map(|jump| {
                    format!(
                        "It jumps through {jump}, which has no entry on this workstation. Set up \
                         SSH to that host, or set this guest up again through another host."
                    )
                })
        };
    }
    entries
}

/// Removes the managed entry pinned under `alias` from `~/.ssh/config` and
/// its pin from `~/.ssh/known_hosts`, under the same lock and with the same
/// backup as setting one up. Returns whether there was one.
pub async fn remove_entry(
    ssh_dir: &Path,
    state_dir: &Path,
    lock_timeout: Duration,
    alias: &str,
) -> Result<bool, String> {
    let text = |e: SshSetupError| match e {
        SshSetupError::Mismatch(m) | SshSetupError::Failed(m) => m,
    };
    if !valid_name(alias) {
        return Err(format!("{alias:?} is not a managed entry."));
    }
    let config = ssh_dir.join("config");
    let known_hosts = ssh_dir.join("known_hosts");
    for path in [ssh_dir, &config, &known_hosts] {
        refuse_symlink(path).map_err(text)?;
    }
    let _lock = lock(state_dir, lock_timeout).await.map_err(text)?;
    let config_before = read_optional(&config).map_err(text)?;
    let known_before = read_optional(&known_hosts).map_err(text)?;
    let ends_with_alias = |line: &str, prefix: &str| {
        line.strip_prefix(prefix)
            .and_then(|rest| rest.split_once(' '))
            .is_some_and(|(_, a)| a == alias)
    };
    let mut kept = Vec::new();
    let mut inside = false;
    let mut found = false;
    for line in config_before.as_deref().unwrap_or("").lines() {
        if !inside && ends_with_alias(line, "# BEGIN bmac managed ") {
            inside = true;
            found = true;
        } else if inside && ends_with_alias(line, "# END bmac managed ") {
            inside = false;
        } else if !inside {
            kept.push(line);
        }
    }
    if inside {
        return Err(format!(
            "The managed SSH block for {alias} in ~/.ssh/config has no end marker; fix or remove \
             it by hand."
        ));
    }
    if !found {
        return Ok(false);
    }
    let config_after = format!(
        "{}\n",
        kept.into_iter()
            .skip_while(|l| l.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
            .trim_end()
    );
    let known_after: String = known_before
        .as_deref()
        .unwrap_or("")
        .lines()
        .filter(|l| l.split_whitespace().next() != Some(alias))
        .map(|l| format!("{l}\n"))
        .collect();
    back_up(state_dir, &config_before, &known_before).map_err(text)?;
    replace(&config, &config_after).map_err(text)?;
    if let Err(e) = replace(&known_hosts, &known_after) {
        restore(&config, &config_before);
        return Err(text(e));
    }
    Ok(true)
}

/// The Ed25519 key from `ssh-keyscan` output, for a host this workstation
/// didn't install; the operator confirms its fingerprint against the one
/// Dashboard Settings shows before it is pinned.
pub fn scanned_ed25519(output: &str) -> Option<String> {
    output
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            let _host = fields.next()?;
            let kind = fields.next()?;
            let key = fields.next()?;
            (kind == "ssh-ed25519").then(|| format!("{kind} {key}"))
        })
        .next()
}

/// Reads a host's Ed25519 key with `ssh-keyscan`.
pub async fn scan(
    runner: &dyn CommandRunner,
    ssh_keyscan: &Path,
    address: &str,
) -> Result<String, String> {
    let output = runner
        .run(
            Command::new(ssh_keyscan)
                .args(["-T", "10", "-t", "ed25519", address])
                .timeout(Duration::from_secs(20)),
        )
        .await
        .map_err(|e| format!("ssh-keyscan {address}: {e}"))?;
    scanned_ed25519(&output.stdout_text())
        .ok_or_else(|| format!("{address} offered no Ed25519 host key."))
}

/// The cluster nodes' Ed25519 keys as Proxmox records them, read over an
/// already pinned connection to one host, so the other hosts need no
/// separate confirmation.
pub fn parse_node_keys(text: &str) -> Vec<(String, String)> {
    let mut keys = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(node), Some(kind), Some(key)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if kind == "ssh-ed25519" && valid_name(node) {
            let entry = (node.to_string(), format!("{kind} {key}"));
            if !keys.contains(&entry) {
                keys.push(entry);
            }
        }
    }
    keys
}

/// Reads `/etc/pve/nodes/*/ssh_known_hosts` through `ssh <name>`.
pub async fn node_keys(
    runner: &dyn CommandRunner,
    ssh: &Path,
    name: &str,
) -> Result<Vec<(String, String)>, String> {
    let script = r#"for f in /etc/pve/nodes/*/ssh_known_hosts; do n=${f#/etc/pve/nodes/}; n=${n%%/*}; awk -v n="$n" '$2 == "ssh-ed25519" { print n, $2, $3 }' "$f"; done"#;
    let output = runner
        .run(
            Command::new(ssh)
                .inherit_env("HOME")
                .inherit_env("USER")
                .inherit_env("SSH_AUTH_SOCK")
                .args(["-o", "BatchMode=yes", name, script])
                .stdin(Vec::new())
                .timeout(Duration::from_secs(30)),
        )
        .await
        .map_err(|e| format!("reading the cluster's host keys through ssh {name}: {e}"))?;
    Ok(parse_node_keys(&output.stdout_text()))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bmac_exec::{BoxFuture, Exit, Output};

    use super::*;

    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ1nYfCK3pinpinpinpinpinpinpinpinpinpinpinpinpin";
    const OLD: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOLDoldoldoldoldoldoldoldoldoldoldoldoldold";

    fn pin() -> HostPin {
        HostPin {
            alias: "bmac-prod-mox1".into(),
            address: "100.64.0.10".into(),
            host_key: KEY.into(),
            jump: None,
        }
    }

    #[derive(Clone, Copy)]
    enum Answer {
        Ok,
        Denied,
        Mismatch,
        Down,
    }

    /// `ssh -G` runs the real `ssh` against the test's config; a login
    /// probe answers as told.
    struct FakeSsh {
        config: PathBuf,
        answer: Answer,
        /// Replaces the real `ssh -G` output.
        effective: Option<String>,
        probes: Mutex<Vec<Vec<String>>>,
    }

    fn failed(command: &Command, stderr: &str) -> ExecError {
        ExecError::UnexpectedExit {
            command: command.program().display().to_string(),
            output: Box::new(Output {
                exit: Exit::Code(255),
                stdout: Vec::new(),
                stderr: stderr.as_bytes().to_vec(),
                stdout_truncated: false,
                stderr_truncated: false,
                elapsed: Duration::ZERO,
            }),
        }
    }

    impl CommandRunner for FakeSsh {
        fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
            Box::pin(async move {
                let mut args = command.arguments().to_vec();
                if args.first().is_some_and(|a| a == "-F") {
                    args.drain(..2);
                }
                if args[0] == "-G" {
                    if let Some(text) = &self.effective {
                        return Ok(Output {
                            exit: Exit::Code(0),
                            stdout: text.clone().into_bytes(),
                            stderr: Vec::new(),
                            stdout_truncated: false,
                            stderr_truncated: false,
                            elapsed: Duration::ZERO,
                        });
                    }
                    let out = std::process::Command::new("ssh")
                        .arg("-F")
                        .arg(&self.config)
                        .args(&args)
                        .output()
                        .unwrap();
                    return Ok(Output {
                        exit: Exit::Code(0),
                        stdout: out.stdout,
                        stderr: Vec::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                        elapsed: Duration::ZERO,
                    });
                }
                self.probes.lock().unwrap().push(args);
                match self.answer {
                    Answer::Ok => Ok(Output {
                        exit: Exit::Code(0),
                        stdout: Vec::new(),
                        stderr: Vec::new(),
                        stdout_truncated: false,
                        stderr_truncated: false,
                        elapsed: Duration::ZERO,
                    }),
                    Answer::Denied => Err(failed(
                        &command,
                        "root@100.64.0.10: Permission denied (publickey).\n",
                    )),
                    Answer::Mismatch => Err(failed(&command, "Host key verification failed.\n")),
                    Answer::Down => Err(failed(
                        &command,
                        "ssh: connect to host 100.64.0.10 port 22: Connection refused\n",
                    )),
                }
            })
        }
    }

    fn ssh_available() -> bool {
        std::process::Command::new("ssh").arg("-V").output().is_ok()
    }

    struct Layout {
        _temp: tempfile::TempDir,
        home: PathBuf,
        state: PathBuf,
    }

    fn layout() -> Layout {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home/.ssh");
        let state = temp.path().join("state");
        fs::create_dir_all(&home).unwrap();
        fs::write(
            home.join("config"),
            "User operator\nServerAliveInterval 30\n\nHost mox1\n    HostName 192.0.2.63\n    ProxyJump bastion\n",
        )
        .unwrap();
        fs::write(
            home.join("known_hosts"),
            format!("mox1 {OLD}\nunrelated {OLD}\n"),
        )
        .unwrap();
        Layout {
            home,
            state,
            _temp: temp,
        }
    }

    fn setup<'a>(
        l: &Layout,
        runner: &'a FakeSsh,
        note: &'a (dyn Fn(String) + Send + Sync),
    ) -> SshSetup<'a> {
        SshSetup {
            ssh_dir: l.home.clone(),
            state_dir: l.state.clone(),
            ssh: Path::new("/usr/bin/ssh"),
            runner,
            attempts: 3,
            wait: Duration::from_millis(1),
            lock_timeout: Duration::from_secs(5),
            note,
        }
    }

    fn fake(l: &Layout, answer: Answer) -> FakeSsh {
        FakeSsh {
            config: l.home.join("config"),
            answer,
            effective: None,
            probes: Mutex::new(Vec::new()),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[tokio::test]
    async fn the_block_pins_the_installed_host_key() {
        if !ssh_available() {
            return;
        }
        let l = layout();
        let runner = fake(&l, Answer::Ok);
        let names = vec!["mox1-test".to_string(), "mox1".to_string()];
        let s = setup(&l, &runner, &|_| {});
        assert!(s.apply(&pin(), &names).await.unwrap());
        assert!(
            !s.apply(&pin(), &names).await.unwrap(),
            "nothing to rewrite"
        );
        assert!(runner.probes.lock().unwrap()[0].contains(&"ControlMaster=no".to_string()));
        let config = fs::read_to_string(l.home.join("config")).unwrap();
        let known = fs::read_to_string(l.home.join("known_hosts")).unwrap();
        assert_eq!(mode(&l.home.join("config")), 0o600);
        assert_eq!(mode(&l.home.join("known_hosts")), 0o600);
        assert!(
            config.starts_with("# BEGIN bmac managed host bmac-prod-mox1\nHost mox1-test mox1\n")
        );
        assert_eq!(config.matches("# BEGIN bmac managed host").count(), 1);
        assert!(config.contains("Host *\n# END bmac managed host bmac-prod-mox1\n"));
        assert!(config.contains("User operator\nServerAliveInterval 30\n"));
        assert!(config.contains("    HostName 192.0.2.63\n"));
        assert_eq!(
            known
                .lines()
                .filter(|l| *l == format!("bmac-prod-mox1 {KEY}"))
                .count(),
            1
        );
        assert!(!known.contains(&format!("mox1 {OLD}")), "{known}");
        assert!(known.contains(&format!("unrelated {OLD}")));

        let effective = |name: &str| {
            String::from_utf8(
                std::process::Command::new("ssh")
                    .arg("-F")
                    .arg(l.home.join("config"))
                    .args(["-G", name])
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
        };
        let other = effective("otherhost");
        assert!(other.contains("user operator\n") && other.contains("serveraliveinterval 30\n"));
        let mox1 = effective("mox1");
        for line in [
            "hostname 100.64.0.10",
            "user root",
            "hostkeyalias bmac-prod-mox1",
            "hostkeyalgorithms ssh-ed25519",
            "stricthostkeychecking true",
            "updatehostkeys false",
        ] {
            assert!(mox1.contains(&format!("{line}\n")), "{line}");
        }
        assert!(!mox1.contains("proxyjump"));
        assert_eq!(
            fs::read_dir(l.state.join("ssh-backups")).unwrap().count(),
            1
        );
    }

    #[tokio::test]
    async fn a_refused_login_restores_both_files() {
        if !ssh_available() {
            return;
        }
        for answer in [Answer::Denied, Answer::Mismatch, Answer::Down] {
            let l = layout();
            let config = fs::read(l.home.join("config")).unwrap();
            let known = fs::read(l.home.join("known_hosts")).unwrap();
            let runner = fake(&l, answer);
            let notes = Mutex::new(Vec::new());
            let note = |n: String| notes.lock().unwrap().push(n);
            let error = setup(&l, &runner, &note)
                .apply(&pin(), &["mox1".into()])
                .await
                .unwrap_err();
            match answer {
                Answer::Denied => assert!(
                    matches!(&error, SshSetupError::Failed(m) if m.contains("not authorized for root"))
                ),
                Answer::Mismatch => assert!(
                    matches!(&error, SshSetupError::Mismatch(m) if m.contains("man-in-the-middle"))
                ),
                Answer::Down => {
                    assert!(
                        matches!(&error, SshSetupError::Failed(m) if m.contains("after 3 attempts") && m.contains("Connection refused"))
                    );
                    assert_eq!(runner.probes.lock().unwrap().len(), 3);
                }
                Answer::Ok => unreachable!(),
            }
            assert_eq!(fs::read(l.home.join("config")).unwrap(), config);
            assert_eq!(fs::read(l.home.join("known_hosts")).unwrap(), known);
            assert_eq!(
                notes
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|n| n.starts_with("Restoring the previous"))
                    .count(),
                1
            );
        }
    }

    #[tokio::test]
    async fn effective_settings_that_differ_are_caught_and_rolled_back() {
        let l = layout();
        let config = fs::read(l.home.join("config")).unwrap();
        let mut runner = fake(&l, Answer::Ok);
        runner.effective = Some("hostname 192.0.2.1\nuser root\nproxyjump bastion\n".into());
        let error = setup(&l, &runner, &|_| {})
            .apply(&pin(), &["mox1".into()])
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("ssh -G mox1 does not resolve"),
            "{error}"
        );
        assert_eq!(fs::read(l.home.join("config")).unwrap(), config);
        assert!(
            runner.probes.lock().unwrap().is_empty(),
            "no login after a bad -G"
        );
    }

    #[test]
    fn settings_match_needs_every_pinned_setting() {
        let known = Path::new("/h/.ssh/known_hosts");
        let good = "hostname 100.64.0.10\nuser root\nport 22\nhostkeyalias bmac-prod-mox1\nhostkeyalgorithms ssh-ed25519\nstricthostkeychecking true\nuserknownhostsfile /h/.ssh/known_hosts\n";
        assert!(settings_match(good, &pin(), known));
        assert!(!settings_match(
            &format!("{good}proxycommand nc %h %p\n"),
            &pin(),
            known
        ));
        assert!(!settings_match(&good.replace("true", "ask"), &pin(), known));
        assert!(!settings_match(good, &pin(), Path::new("/other")));
    }

    #[tokio::test]
    async fn symlinks_and_bad_names_are_refused() {
        let l = layout();
        let target = l.home.join("real-config");
        fs::rename(l.home.join("config"), &target).unwrap();
        std::os::unix::fs::symlink(&target, l.home.join("config")).unwrap();
        let runner = fake(&l, Answer::Ok);
        let s = setup(&l, &runner, &|_| {});
        let error = s.apply(&pin(), &["mox1".into()]).await.unwrap_err();
        assert!(error.to_string().contains("symbolic link"));
        for bad in ["", "-mox1", "mox 1", "mox1\nHost *", &"m".repeat(65)] {
            assert!(
                s.apply(&pin(), &[bad.to_string()]).await.is_err(),
                "{bad:?}"
            );
        }
        assert!(runner.probes.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unterminated_block_is_refused() {
        let error = merge_config(
            "# BEGIN bmac managed host bmac-prod-mox1\nHost mox1\n",
            &pin(),
            &["mox1".into()],
            Path::new("/h/.ssh/known_hosts"),
        )
        .unwrap_err();
        assert!(error.contains("no end marker"));
    }

    #[test]
    fn hashed_and_listed_entries_are_removed() {
        let salt = [7u8; 20];
        let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &salt);
        let digest = ring::hmac::sign(&key, b"mox1");
        let hashed = format!(
            "|1|{}|{}",
            STANDARD.encode(salt),
            STANDARD.encode(digest.as_ref())
        );
        let existing = format!(
            "{hashed} {OLD}\nmox2,mox1 {OLD}\n# mox1 comment\n@cert-authority mox1 {OLD}\nmox10 {OLD}\n[mox1]:2222 {OLD}\n"
        );
        let merged = merge_known_hosts(&existing, &pin(), &["mox1".into()]);
        assert_eq!(
            merged,
            format!(
                "# mox1 comment\n@cert-authority mox1 {OLD}\nmox10 {OLD}\n[mox1]:2222 {OLD}\nbmac-prod-mox1 {KEY}\n"
            )
        );
    }

    #[test]
    fn manual_instructions_carry_the_pin() {
        let text = manual_instructions("mox1", &pin()).join("\n");
        assert!(text.contains("HostKeyAlias bmac-prod-mox1"));
        assert!(text.contains("HostName 100.64.0.10"));
        assert!(text.contains(&format!("bmac-prod-mox1 {KEY}")));
    }

    #[test]
    fn scanned_and_cluster_keys_are_parsed() {
        assert_eq!(
            scanned_ed25519(&format!(
                "# 100.64.0.10:22 SSH-2.0-OpenSSH_9.2\n100.64.0.10 ssh-rsa AAAAB3\n100.64.0.10 {KEY}\n"
            )),
            Some(KEY.to_string())
        );
        assert_eq!(scanned_ed25519("100.64.0.10 ssh-rsa AAAAB3\n"), None);
        assert_eq!(
            parse_node_keys(&format!(
                "mox1 {KEY}\nmox2 ssh-rsa AAAAB3\nmox1 {KEY}\nbad;name {KEY}\nmox3 {OLD}\n"
            )),
            [
                ("mox1".to_string(), KEY.to_string()),
                ("mox3".to_string(), OLD.to_string())
            ]
        );
    }

    #[test]
    fn names_and_aliases() {
        assert!(valid_name("mox1") && valid_name("a.b_c-d") && valid_name(&"m".repeat(64)));
        assert!(!valid_name(".mox1") && !valid_name("mox1/x"));
        assert_eq!(pin_alias("prod", "mox1"), "bmac-prod-mox1");
    }
    #[tokio::test]
    async fn entries_are_listed_and_one_is_removed_with_its_pin() {
        let dir = tempfile::tempdir().unwrap();
        let ssh_dir = dir.path().join("ssh");
        fs::create_dir_all(&ssh_dir).unwrap();
        let known = ssh_dir.join("known_hosts");
        let guest = HostPin {
            alias: "bmac-prod-prod1".into(),
            address: "10.213.0.31".into(),
            host_key: OLD.into(),
            jump: Some("mox4".into()),
        };
        let mut config = "Host old\n    User me\n".to_string();
        let mut hosts = "github.com ssh-ed25519 AAAA\n".to_string();
        for (pin, names) in [
            (&pin(), vec!["mox1".to_string(), "pve1".into()]),
            (&guest, vec!["prod1".into()]),
        ] {
            config = merge_config(&config, pin, &names, &known).unwrap();
            hosts = merge_known_hosts(&hosts, pin, &names);
        }
        fs::write(ssh_dir.join("config"), &config).unwrap();
        fs::write(&known, &hosts).unwrap();

        let entries = managed_entries(&config, &hosts);
        assert_eq!(entries.len(), 2);
        let host = entries.iter().find(|e| e.kind == "host").unwrap();
        assert_eq!(host.alias, "bmac-prod-mox1");
        assert_eq!(host.names, ["mox1", "pve1"]);
        assert_eq!(host.address, "100.64.0.10");
        assert_eq!(
            (host.jump.as_deref(), host.problem.as_deref()),
            (None, None)
        );
        let listed = entries.iter().find(|e| e.kind == "guest").unwrap();
        assert_eq!(listed.jump.as_deref(), Some("mox4"));
        assert!(
            listed
                .problem
                .as_deref()
                .unwrap()
                .contains("mox4, which has no entry")
        );
        assert!(
            managed_entries(&config, "")[0]
                .problem
                .as_deref()
                .unwrap()
                .contains("isn't pinned")
        );

        let state = dir.path().join("state");
        let removed = remove_entry(&ssh_dir, &state, Duration::from_secs(5), "bmac-prod-mox1");
        assert!(removed.await.unwrap());
        let config = fs::read_to_string(ssh_dir.join("config")).unwrap();
        let hosts = fs::read_to_string(&known).unwrap();
        assert!(!config.contains("bmac-prod-mox1") && !config.contains("Host mox1"));
        assert!(config.contains("Host prod1\n") && config.contains("Host old\n    User me\n"));
        assert!(!hosts.contains("bmac-prod-mox1"));
        assert!(
            hosts.contains("github.com ssh-ed25519 AAAA\n") && hosts.contains("bmac-prod-prod1")
        );
        assert_eq!(fs::read_dir(state.join("ssh-backups")).unwrap().count(), 1);
        let again = remove_entry(&ssh_dir, &state, Duration::from_secs(5), "bmac-prod-mox1");
        assert!(!again.await.unwrap(), "nothing left to remove");
    }
}
