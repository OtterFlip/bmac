// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `ssh prod1` from the workstation through a host (`DEPLOYMENT.md`,
//! "Workstation SSH"), after v2's `setup_jump_ssh_access.sh` and
//! `report_stale_jump_ssh.sh`. The guest list and the guest's host key come
//! from `bmac guests --json` and `bmac guest-host-key --json` run over the
//! workstation's pinned SSH to a host; the key is read inside the guest by
//! the guest agent, so it is attested by the cluster rather than trusted on
//! first use. The managed block is written by [`crate::workstation_ssh`].

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use bmac_exec::{Command, CommandRunner};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::workstation_ssh::{HostPin, pin_alias, valid_name};

/// One row of `bmac guests --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Guest {
    pub name: String,
    /// `production` or `staging`.
    pub kind: String,
    pub state: String,
    pub vmid: u32,
    pub ip: String,
    pub node: Option<String>,
    pub status: String,
    #[serde(default)]
    pub primary_domain: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub placement: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestList {
    pub guests: Vec<Guest>,
    #[serde(default)]
    pub staging_limits: BTreeMap<String, u32>,
}

/// `bmac guest-host-key --json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GuestKey {
    pub guest: String,
    pub vmid: u32,
    pub node: String,
    pub key: String,
    pub fingerprint: String,
}

fn ssh(ssh: &Path) -> Command {
    Command::new(ssh)
        .inherit_env("HOME")
        .inherit_env("USER")
        .inherit_env("SSH_AUTH_SOCK")
        .args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=15"])
        .stdin(Vec::new())
}

async fn bmac_json(
    runner: &dyn CommandRunner,
    ssh_program: &Path,
    host: &str,
    args: &[&str],
) -> Result<Vec<u8>, String> {
    let output = runner
        .run(
            ssh(ssh_program)
                .arg(host)
                .arg("bmac")
                .args(args.iter().copied())
                .arg("--json")
                .timeout(Duration::from_secs(90))
                .stdout_limit(4 << 20),
        )
        .await
        .map_err(|e| format!("ssh {host} bmac {}: {e}", args.join(" ")))?;
    Ok(output.stdout)
}

/// The registered guests, as a host sees them.
pub async fn guests(
    runner: &dyn CommandRunner,
    ssh_program: &Path,
    host: &str,
) -> Result<GuestList, String> {
    let bytes = bmac_json(runner, ssh_program, host, &["guests"]).await?;
    serde_json::from_slice(&bytes).map_err(|e| format!("bmac guests on {host}: {e}"))
}

/// The `SHA256:` fingerprint of an `ssh-ed25519 <base64>` key.
pub fn key_fingerprint(key: &str) -> Option<String> {
    let mut fields = key.split_whitespace();
    if fields.next()? != "ssh-ed25519" {
        return None;
    }
    let blob = STANDARD.decode(fields.next()?).ok()?;
    let name = b"ssh-ed25519";
    let ok = blob.len() == 4 + name.len() + 4 + 32
        && blob[..4] == (name.len() as u32).to_be_bytes()
        && &blob[4..4 + name.len()] == name
        && blob[4 + name.len()..8 + name.len()] == 32u32.to_be_bytes();
    ok.then(|| format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&blob))))
}

/// The guest's attested key. The fingerprint is recomputed here rather
/// than taken from the host's answer.
pub async fn guest_key(
    runner: &dyn CommandRunner,
    ssh_program: &Path,
    host: &str,
    guest: &str,
) -> Result<GuestKey, String> {
    if !valid_name(guest) {
        return Err(format!("{guest:?} is not a guest name."));
    }
    let bytes = bmac_json(runner, ssh_program, host, &["guest-host-key", guest]).await?;
    let key: GuestKey = serde_json::from_slice(&bytes)
        .map_err(|e| format!("bmac guest-host-key on {host}: {e}"))?;
    if key.guest != guest {
        return Err(format!(
            "{host} answered for {} instead of {guest}.",
            key.guest
        ));
    }
    match key_fingerprint(&key.key) {
        Some(fingerprint) if fingerprint == key.fingerprint => Ok(key),
        _ => Err(format!(
            "{host} returned an invalid Ed25519 host key for {guest}."
        )),
    }
}

/// The pin for a running guest, jumping through `jump`.
pub fn guest_pin(
    cluster: &str,
    guest: &Guest,
    key: &GuestKey,
    jump: &str,
) -> Result<HostPin, String> {
    if guest.status != "running" {
        return Err(format!(
            "{} is not running (live status: {}); start it and try again.",
            guest.name, guest.status
        ));
    }
    if guest.ip.parse::<std::net::Ipv4Addr>().is_err() {
        return Err(format!(
            "{} has an unusable private address: {}",
            guest.name, guest.ip
        ));
    }
    if !valid_name(jump) {
        return Err(format!("{jump:?} is not an SSH name."));
    }
    Ok(HostPin {
        alias: pin_alias(cluster, &guest.name),
        address: guest.ip.clone(),
        host_key: key.key.clone(),
        jump: Some(jump.to_string()),
    })
}

/// `ssh <jump> true` works: `ProxyJump` resolves the jump host through the
/// workstation's own config.
pub async fn check_jump(
    runner: &dyn CommandRunner,
    ssh_program: &Path,
    jump: &str,
) -> Result<(), String> {
    runner
        .run(
            ssh(ssh_program)
                .args([
                    "-o",
                    "ControlMaster=no",
                    "-o",
                    "ControlPath=none",
                    jump,
                    "true",
                ])
                .timeout(Duration::from_secs(30)),
        )
        .await
        .map(|_| ())
        .map_err(|_| {
            format!(
                "ssh {jump} does not work from this workstation. Set up SSH to that host first, \
                 then try again."
            )
        })
}

/// A host the workstation already reaches with a managed block: its slot
/// and the names in the block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredHost {
    pub host: String,
    pub names: Vec<String>,
}

/// The managed host blocks for `cluster` in `~/.ssh/config`.
pub fn configured_hosts(config: &str, cluster: &str) -> Vec<ConfiguredHost> {
    let prefix = format!("# BEGIN bmac managed host bmac-{cluster}-");
    let mut found = Vec::new();
    let mut current: Option<String> = None;
    for line in config.lines() {
        if let Some(host) = line.strip_prefix(&prefix) {
            current = Some(host.to_string());
            continue;
        }
        if line.starts_with("# END bmac managed host ") {
            current = None;
            continue;
        }
        if let (Some(host), Some(names)) = (&current, line.strip_prefix("Host "))
            && names != "*"
        {
            found.push(ConfiguredHost {
                host: host.clone(),
                names: names.split_whitespace().map(str::to_string).collect(),
            });
        }
    }
    found
}

/// The clusters with managed host entries in `config`, in the order first
/// seen.
pub fn managed_clusters(config: &str) -> Vec<String> {
    let mut clusters: Vec<String> = Vec::new();
    for line in config.lines() {
        let Some(alias) = line.strip_prefix("# BEGIN bmac managed host bmac-") else {
            continue;
        };
        let Some((cluster, host)) = alias.rsplit_once('-') else {
            continue;
        };
        if host.parse::<bmac_core::HostSlot>().is_ok()
            && !cluster.is_empty()
            && !clusters.iter().any(|c| c == cluster)
        {
            clusters.push(cluster.to_string());
        }
    }
    clusters
}

/// The default jump host: the workstation's first name for the node running
/// the guest, or else for any configured host.
pub fn default_jump(hosts: &[ConfiguredHost], guest_node: Option<&str>) -> Option<String> {
    let first = |h: &ConfiguredHost| h.names.first().cloned();
    guest_node
        .and_then(|node| hosts.iter().find(|h| h.host == node).and_then(first))
        .or_else(|| hosts.iter().find_map(first))
}

/// A guest alias whose `ProxyJump` goes through a host that left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleAlias {
    pub alias: String,
    /// `guest` for a managed block, or `hand-written entry`.
    pub kind: String,
    pub address: Option<String>,
    pub jump: String,
}

fn hop_host(hop: &str) -> String {
    let mut hop = hop.trim();
    if let Some((_, rest)) = hop.split_once("://") {
        hop = rest;
    }
    hop = hop.rsplit('@').next().unwrap_or(hop);
    let host = if let Some(rest) = hop.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        hop.split(':').next().unwrap_or("")
    };
    host.to_ascii_lowercase()
}

fn uses_removed(value: &str, removed: &[String]) -> bool {
    value.split(',').any(|hop| {
        let host = hop_host(hop);
        removed.iter().any(|r| {
            host == r.to_ascii_lowercase()
                || host.starts_with(&format!("{}.", r.to_ascii_lowercase()))
        })
    })
}

fn guest_like(name: &str) -> bool {
    let digits =
        |s: &str| !s.is_empty() && !s.starts_with('0') && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(rest) = name.strip_prefix("prod") {
        return digits(rest);
    }
    if let Some(rest) = name.strip_prefix("stage")
        && let Some((n, m)) = rest.split_once("prod")
    {
        return digits(n) && digits(m);
    }
    false
}

#[derive(Default)]
struct Seen {
    kind: Option<String>,
    address: Option<String>,
    jump: Option<String>,
}

/// Guest aliases in `config` jumping through any of `removed` (the host's
/// slot and the workstation's names for it).
pub fn stale_aliases(config: &str, removed: &[String]) -> Vec<StaleAlias> {
    let mut found: BTreeMap<String, Seen> = BTreeMap::new();
    let mut managed: Option<String> = None;
    let mut patterns: Vec<String> = Vec::new();
    for raw in config.lines() {
        let stripped = raw.trim();
        if let Some(alias) = stripped
            .strip_prefix("# BEGIN bmac managed guest ")
            .filter(|a| !a.contains(char::is_whitespace))
        {
            managed = Some(alias.to_string());
            continue;
        }
        if managed.is_some() && stripped.starts_with("# END bmac managed guest ") {
            managed = None;
            continue;
        }
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let (keyword, value) = match stripped.split_once(|c: char| c == '=' || c.is_whitespace()) {
            Some((k, v)) => (
                k.to_ascii_lowercase(),
                v.trim_start_matches(|c: char| c == '=' || c.is_whitespace())
                    .trim(),
            ),
            None => continue,
        };
        match keyword.as_str() {
            "host" => {
                patterns = value.split_whitespace().map(str::to_string).collect();
            }
            "match" => patterns.clear(),
            "hostname" => {
                let targets: Vec<String> = match &managed {
                    Some(_) => patterns.clone(),
                    None => patterns.iter().filter(|p| guest_like(p)).cloned().collect(),
                };
                for alias in targets {
                    let entry = found.entry(alias).or_default();
                    entry.address.get_or_insert_with(|| value.to_string());
                }
            }
            "proxyjump" if uses_removed(value, removed) => {
                let (aliases, kind) = match &managed {
                    Some(_) => (patterns.clone(), "guest"),
                    None => (
                        patterns.iter().filter(|p| guest_like(p)).cloned().collect(),
                        "hand-written entry",
                    ),
                };
                for alias in aliases {
                    let entry = found.entry(alias).or_default();
                    entry.kind.get_or_insert_with(|| kind.to_string());
                    entry.jump.get_or_insert_with(|| value.to_string());
                }
            }
            _ => {}
        }
    }
    found
        .into_iter()
        .filter_map(
            |(
                alias,
                Seen {
                    kind,
                    address,
                    jump,
                },
            )| {
                Some(StaleAlias {
                    alias,
                    kind: kind?,
                    address,
                    jump: jump?,
                })
            },
        )
        .collect()
}

/// What the Deployer shows after a host is removed.
pub fn stale_report(removed: &str, stale: &[StaleAlias]) -> Vec<String> {
    if stale.is_empty() {
        return vec![format!(
            "No ~/.ssh/config guest entries on this workstation jump through {removed}."
        )];
    }
    let mut lines = vec![format!(
        "These ~/.ssh/config entries jump through {removed}, which is no longer in the cluster, \
         so ssh to them will hang or fail:"
    )];
    for entry in stale {
        let mut details = vec![entry.kind.clone()];
        details.extend(entry.address.clone());
        details.push(format!("ProxyJump {}", entry.jump));
        lines.push(format!("{} ({})", entry.alias, details.join(", ")));
    }
    lines.push(
        "Set up guest SSH again for each one in the Deployer, choosing another host to jump \
         through. Other administrators' workstations need the same repair."
            .into(),
    );
    lines
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bmac_exec::{BoxFuture, ExecError, Exit, Output};
    use serde_json::json;

    use super::*;

    /// v2 test_setup_jump_ssh_access.py's fixture key and its fingerprint.
    const KEY: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4";

    struct Answers {
        replies: Mutex<Vec<serde_json::Value>>,
        calls: Mutex<Vec<Vec<String>>>,
    }

    impl CommandRunner for Answers {
        fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push(command.arguments().to_vec());
                let reply = self.replies.lock().unwrap().remove(0);
                Ok(Output {
                    exit: Exit::Code(0),
                    stdout: serde_json::to_vec(&reply).unwrap(),
                    stderr: Vec::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    elapsed: Duration::ZERO,
                })
            })
        }
    }

    fn answers(replies: Vec<serde_json::Value>) -> Answers {
        Answers {
            replies: Mutex::new(replies),
            calls: Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn the_fingerprint_is_ssh_keygens() {
        let fingerprint = key_fingerprint(KEY).unwrap();
        assert!(
            fingerprint.starts_with("SHA256:") && fingerprint.len() == 50,
            "{fingerprint}"
        );
        assert_eq!(key_fingerprint("ssh-rsa AAAAB3NzaC1yc2E="), None);
        assert_eq!(key_fingerprint("ssh-ed25519 AAAAC3NzaC1lZDI1NTE5"), None);
    }

    #[tokio::test]
    async fn the_guest_list_and_key_come_from_a_host() {
        let fingerprint = key_fingerprint(KEY).unwrap();
        let runner = answers(vec![
            json!({"guests": [{"name": "prod1", "kind": "production", "state": "active", "vmid": 100,
                               "ip": "10.213.0.31", "node": "mox3", "status": "running",
                               "primary_domain": "example.com", "aliases": [], "placement": ["mox1", "mox3"]}],
                   "staging_limits": {"mox1": 2, "mox3": 4}}),
            json!({"guest": "prod1", "vmid": 100, "node": "mox3", "key": KEY, "fingerprint": fingerprint}),
            json!({"guest": "prod1", "vmid": 100, "node": "mox3", "key": KEY, "fingerprint": "SHA256:other"}),
            json!({"guest": "prod2", "vmid": 101, "node": "mox3", "key": KEY, "fingerprint": fingerprint}),
        ]);
        let ssh = Path::new("/usr/bin/ssh");
        let list = guests(&runner, ssh, "mox1").await.unwrap();
        assert_eq!(list.guests[0].node.as_deref(), Some("mox3"));
        assert_eq!(list.staging_limits["mox3"], 4);
        assert_eq!(
            guest_key(&runner, ssh, "mox1", "prod1").await.unwrap().key,
            KEY
        );
        assert!(
            guest_key(&runner, ssh, "mox1", "prod1")
                .await
                .unwrap_err()
                .contains("invalid")
        );
        assert!(
            guest_key(&runner, ssh, "mox1", "prod1")
                .await
                .unwrap_err()
                .contains("instead of")
        );
        assert!(guest_key(&runner, ssh, "mox1", "prod1; rm").await.is_err());
        let calls = runner.calls.lock().unwrap();
        assert_eq!(
            calls[0][calls[0].len() - 4..],
            ["mox1", "bmac", "guests", "--json"]
        );
        assert!(calls[0].contains(&"BatchMode=yes".to_string()));
        assert_eq!(
            calls[1][calls[1].len() - 5..],
            ["mox1", "bmac", "guest-host-key", "prod1", "--json"]
        );
    }

    const CONFIG: &str = "\
# BEGIN bmac managed host bmac-prod-mox1
Host mox1 m1
    HostName 100.64.0.10
Host *
# END bmac managed host bmac-prod-mox1
# BEGIN bmac managed host bmac-prod-mox3
Host mox3
    HostName 100.64.0.12
Host *
# END bmac managed host bmac-prod-mox3
# BEGIN bmac managed host bmac-other-mox2
Host other-mox2
Host *
# END bmac managed host bmac-other-mox2

# BEGIN bmac managed guest bmac-prod-prod1
Host prod1
    HostName 10.213.0.31
    User root
    ProxyJump mox1
Host *
# END bmac managed guest bmac-prod-prod1

# BEGIN bmac managed guest bmac-prod-stage1prod1
Host stage1prod1
    HostName 10.213.0.61
    ProxyJump mox2
Host *
# END bmac managed guest bmac-prod-stage1prod1

# BEGIN bmac managed guest bmac-prod-prod2
Host p2
    HostName 10.213.0.32
    ProxyJump root@mox1.internal:22
Host *
# END bmac managed guest bmac-prod-prod2

# BEGIN bmac managed guest bmac-prod-prod4
Host prod4
    HostName 10.213.0.34
    ProxyJump mox10
Host *
# END bmac managed guest bmac-prod-prod4

Host prod3
    HostName 10.213.0.33
    ProxyJump=qdevice,m1

Host web
    ProxyJump mox1
";

    #[test]
    fn configured_hosts_are_read_per_cluster() {
        let hosts = configured_hosts(CONFIG, "prod");
        assert_eq!(
            hosts,
            [
                ConfiguredHost {
                    host: "mox1".into(),
                    names: vec!["mox1".into(), "m1".into()]
                },
                ConfiguredHost {
                    host: "mox3".into(),
                    names: vec!["mox3".into()]
                },
            ]
        );
        assert_eq!(default_jump(&hosts, Some("mox3")).as_deref(), Some("mox3"));
        assert_eq!(default_jump(&hosts, Some("mox2")).as_deref(), Some("mox1"));
        assert_eq!(default_jump(&[], Some("mox2")), None);
    }

    #[test]
    fn only_guest_aliases_jumping_through_the_removed_host_are_listed() {
        let removed = ["mox1".to_string(), "m1".to_string()];
        let stale = stale_aliases(CONFIG, &removed);
        let aliases: Vec<&str> = stale.iter().map(|s| s.alias.as_str()).collect();
        assert_eq!(aliases, ["p2", "prod1", "prod3"]);
        assert_eq!(stale[0].kind, "guest");
        assert_eq!(stale[0].address.as_deref(), Some("10.213.0.32"));
        assert_eq!(stale[2].kind, "hand-written entry");
        assert_eq!(stale[2].jump, "qdevice,m1");
        let report = stale_report("mox1", &stale).join("\n");
        assert!(
            report.contains("prod1 (guest, 10.213.0.31, ProxyJump mox1)"),
            "{report}"
        );
        assert!(
            !report.contains("stage1prod1") && !report.contains("prod4") && !report.contains("web")
        );

        let clean = stale_report("mox3", &stale_aliases(CONFIG, &["mox3".to_string()]));
        assert_eq!(
            clean,
            ["No ~/.ssh/config guest entries on this workstation jump through mox3."]
        );
        assert!(stale_aliases("", &removed).is_empty());
    }

    #[test]
    fn a_guest_pin_jumps_through_the_chosen_host() {
        let guest = Guest {
            name: "prod1".into(),
            kind: "production".into(),
            state: "active".into(),
            vmid: 100,
            ip: "10.213.0.31".into(),
            node: Some("mox3".into()),
            status: "running".into(),
            primary_domain: "example.com".into(),
            aliases: vec![],
            placement: vec![],
        };
        let key = GuestKey {
            guest: "prod1".into(),
            vmid: 100,
            node: "mox3".into(),
            key: KEY.into(),
            fingerprint: key_fingerprint(KEY).unwrap(),
        };
        let pin = guest_pin("prod", &guest, &key, "mox3").unwrap();
        assert_eq!(pin.alias, "bmac-prod-prod1");
        assert_eq!(pin.jump.as_deref(), Some("mox3"));
        let config = crate::workstation_ssh::merge_config(
            "",
            &pin,
            &["prod1".into()],
            Path::new("/h/.ssh/known_hosts"),
        )
        .unwrap();
        assert!(config.starts_with(
            "# BEGIN bmac managed guest bmac-prod-prod1\nHost prod1\n    HostName 10.213.0.31\n"
        ));
        assert!(config.contains("    ProxyJump mox3\n"));
        let stopped = Guest {
            status: "stopped".into(),
            ..guest.clone()
        };
        assert!(
            guest_pin("prod", &stopped, &key, "mox3")
                .unwrap_err()
                .contains("not running")
        );
        assert!(guest_pin("prod", &guest, &key, "mox3 -oProxyCommand=x").is_err());
    }

    #[test]
    fn hops_are_parsed_like_ssh_does() {
        assert_eq!(hop_host("ssh://root@[mox1]:22"), "mox1");
        assert_eq!(hop_host(" root@MOX1.internal:22"), "mox1.internal");
        assert!(guest_like("prod12") && guest_like("stage2prod1"));
        assert!(
            !guest_like("prod0")
                && !guest_like("prod")
                && !guest_like("stageprod1")
                && !guest_like("web")
        );
    }
}
