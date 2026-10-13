// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Finding a new machine on the tailnet from this workstation's
//! `tailscale status --json`, as v2's `tailscale_peer_selection` does. Peer
//! host names are self-reported, so a name alone could pick a stale node
//! from an earlier install or any machine using that name: a peer counts
//! only with the required tag, enrolled after the ISO was built, and only
//! when exactly one matches.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use bmac_exec::{Command, CommandRunner};
use serde_json::Value;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// The tags a peer must carry. The lab uses the same ones, on a tailnet of
/// its own (`lab/README.md`).
pub const HOST_TAG: &str = "tag:bmac-proxmox-host";
pub const DISCOVERY_TAG: &str = "tag:bmac-discovery";
pub const APPLIANCE_TAG: &str = "tag:bmac-backup-appliance";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerSelection {
    /// The one matching peer, online, with its `100.x` address and
    /// MagicDNS name.
    Found { ip: String, dns_name: String },
    /// Not there yet.
    Absent,
    /// There, but offline.
    Offline,
    /// More than one match; the operator must remove the extra machines.
    Ambiguous(usize),
    /// A fresh machine with the name enrolled without the required tag, so
    /// its auth key was the wrong one.
    Untagged { tags: Vec<String> },
}

fn created_ms(peer: &Value) -> Option<i64> {
    let text = peer["Created"].as_str()?;
    jiff::Timestamp::from_str(text)
        .ok()
        .map(|t| t.as_millisecond())
}

/// Picks the peer named `host` with `tag`, created at or after `since_ms`.
pub fn select_peer(status: &Value, host: &str, tag: &str, since_ms: i64) -> PeerSelection {
    let peers: Vec<&Value> = status["Peer"]
        .as_object()
        .map(|peers| peers.values().collect())
        .unwrap_or_default();
    let prefix = format!("{host}.");
    let fresh_named: Vec<&Value> = peers
        .into_iter()
        .filter(|p| {
            p["HostName"].as_str() == Some(host)
                || p["DNSName"]
                    .as_str()
                    .is_some_and(|d| d.starts_with(&prefix))
        })
        .filter(|p| created_ms(p).is_some_and(|c| c >= since_ms))
        .collect();
    let tags = |p: &Value| -> Vec<String> {
        p["Tags"]
            .as_array()
            .map(|t| {
                t.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let candidates: Vec<&&Value> = fresh_named
        .iter()
        .filter(|p| tags(p).iter().any(|t| t == tag))
        .collect();
    match candidates.as_slice() {
        [] => match fresh_named.as_slice() {
            [one] => PeerSelection::Untagged { tags: tags(one) },
            _ => PeerSelection::Absent,
        },
        [one] => {
            if one["Online"].as_bool() != Some(true) {
                return PeerSelection::Offline;
            }
            let ip = one["TailscaleIPs"]
                .as_array()
                .and_then(|ips| {
                    ips.iter()
                        .filter_map(|ip| ip.as_str())
                        .find(|ip| ip.starts_with("100."))
                })
                .map(str::to_string);
            match ip {
                Some(ip) => PeerSelection::Found {
                    ip,
                    dns_name: one["DNSName"]
                        .as_str()
                        .unwrap_or("")
                        .trim_end_matches('.')
                        .to_string(),
                },
                None => PeerSelection::Absent,
            }
        }
        many => PeerSelection::Ambiguous(many.len()),
    }
}

/// The tailnet's MagicDNS suffix, such as `tail1234.ts.net`.
pub fn magic_dns_suffix(status: &Value) -> Option<String> {
    let from_field = status["MagicDNSSuffix"]
        .as_str()
        .or_else(|| status["CurrentTailnet"]["MagicDNSSuffix"].as_str());
    let suffix = match from_field {
        Some(s) => s.to_string(),
        None => {
            let name = status["Self"]["DNSName"].as_str()?.trim_end_matches('.');
            name.split_once('.')?.1.to_string()
        }
    };
    let suffix = suffix.trim_matches('.').to_string();
    (!suffix.is_empty()).then_some(suffix)
}

/// This workstation's `tailscale status --json`.
pub async fn status(runner: &dyn CommandRunner, tailscale: &Path) -> Result<Value, String> {
    let output = runner
        .run(
            Command::new(tailscale)
                .args(["status", "--json"])
                .timeout(Duration::from_secs(20))
                .stdout_limit(16 << 20),
        )
        .await
        .map_err(|e| format!("tailscale status: {e}"))?;
    if output.stdout_truncated {
        return Err("tailscale status printed more than expected".into());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("tailscale status: {e}"))
}

/// How the wait reports what it sees.
pub type Report<'a> = &'a (dyn Fn(&PeerSelection) + Send + Sync);

/// Waits until exactly one fresh, tagged, online peer named `host` is
/// there. `look_now` makes it look again at once, as when the operator
/// says the machine has booted. Ambiguity stops the wait; the other states
/// keep it going.
#[allow(clippy::too_many_arguments)]
pub async fn wait_for_peer(
    runner: &dyn CommandRunner,
    tailscale: &Path,
    host: &str,
    tag: &str,
    since_ms: i64,
    every: Duration,
    look_now: Arc<Notify>,
    cancel: &CancellationToken,
    report: Report<'_>,
) -> Result<(String, String), String> {
    let mut last: Option<PeerSelection> = None;
    loop {
        let selection = match status(runner, tailscale).await {
            Ok(status) => select_peer(&status, host, tag, since_ms),
            Err(e) => return Err(e),
        };
        if last.as_ref() != Some(&selection) {
            report(&selection);
            last = Some(selection.clone());
        }
        match selection {
            PeerSelection::Found { ip, dns_name } => return Ok((ip, dns_name)),
            PeerSelection::Ambiguous(n) => {
                return Err(format!(
                    "{n} machines named {host} with {tag} enrolled after this ISO was built; \
                     expected exactly one. Remove the unexpected machines in the Tailscale admin \
                     console, then look again."
                ));
            }
            _ => {}
        }
        tokio::select! {
            _ = cancel.cancelled() => return Err("Cancelled.".into()),
            _ = look_now.notified() => {}
            _ = tokio::time::sleep(every) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn peer(host: &str, ip: &str, created: &str, tags: Option<&[&str]>, online: bool) -> Value {
        let mut peer = json!({
            "HostName": host,
            "DNSName": format!("{host}.example.ts.net."),
            "Online": online,
            "Created": created,
            "TailscaleIPs": [ip, "fd7a:115c:a1e0::1"],
        });
        if let Some(tags) = tags {
            peer["Tags"] = json!(tags);
        }
        peer
    }

    fn select(peers: &[Value]) -> PeerSelection {
        let status = json!({"Peer": peers.iter().enumerate()
            .map(|(i, p)| (i.to_string(), p.clone())).collect::<serde_json::Map<_, _>>()});
        let since = jiff::Timestamp::from_str("2026-10-06T16:56:07Z")
            .unwrap()
            .as_millisecond();
        select_peer(&status, "mox1", "tag:bmac-proxmox-host", since)
    }

    const TAG: Option<&[&str]> = Some(&["tag:bmac-proxmox-host"]);

    #[test]
    fn exactly_one_fresh_tagged_online_peer_is_chosen() {
        let fresh = peer(
            "mox1",
            "100.64.0.10",
            "2026-10-06T17:12:28.407677017Z",
            TAG,
            true,
        );
        let stale = peer("mox1", "100.64.0.9", "2026-10-05T01:00:00.1Z", TAG, true);
        let other_tag = peer(
            "mox1",
            "100.64.0.7",
            "2026-10-06T17:13:00Z",
            Some(&["tag:laptop"]),
            true,
        );
        let similar = peer("mox10", "100.64.0.6", "2026-10-06T17:14:00Z", TAG, true);
        assert_eq!(
            select(&[
                stale.clone(),
                other_tag.clone(),
                similar.clone(),
                fresh.clone()
            ]),
            PeerSelection::Found {
                ip: "100.64.0.10".into(),
                dns_name: "mox1.example.ts.net".into()
            }
        );
        assert_eq!(
            select(&[stale.clone(), similar.clone()]),
            PeerSelection::Absent
        );
        let mut offline = fresh.clone();
        offline["Online"] = json!(false);
        assert_eq!(select(&[offline, stale]), PeerSelection::Offline);
        let second = peer("mox1", "100.64.0.11", "2026-10-06T17:20:00Z", TAG, true);
        assert_eq!(select(&[fresh, second]), PeerSelection::Ambiguous(2));
        assert_eq!(
            select_peer(&json!({"Peer": null}), "mox1", "tag:bmac-proxmox-host", 0),
            PeerSelection::Absent
        );
    }

    #[test]
    fn a_fresh_peer_without_the_tag_is_named_as_such() {
        let untagged = peer("mox1", "100.64.0.8", "2026-10-06T17:13:00Z", None, true);
        assert_eq!(
            select(&[untagged]),
            PeerSelection::Untagged { tags: vec![] }
        );
        let laptop = peer(
            "mox1",
            "100.64.0.7",
            "2026-10-06T17:13:00Z",
            Some(&["tag:laptop"]),
            true,
        );
        assert_eq!(
            select(&[laptop]),
            PeerSelection::Untagged {
                tags: vec!["tag:laptop".into()]
            }
        );
    }

    #[test]
    fn the_suffix_comes_from_the_status() {
        assert_eq!(
            magic_dns_suffix(&json!({"MagicDNSSuffix": "tail1234.ts.net"})).as_deref(),
            Some("tail1234.ts.net")
        );
        assert_eq!(
            magic_dns_suffix(&json!({"Self": {"DNSName": "laptop.tail1234.ts.net."}})).as_deref(),
            Some("tail1234.ts.net")
        );
        assert_eq!(magic_dns_suffix(&json!({})), None);
    }

    struct Statuses(std::sync::Mutex<Vec<Value>>);

    impl CommandRunner for Statuses {
        fn run(
            &self,
            _command: Command,
        ) -> bmac_exec::BoxFuture<'_, Result<bmac_exec::Output, bmac_exec::ExecError>> {
            let mut list = self.0.lock().unwrap();
            let next = if list.len() > 1 {
                list.remove(0)
            } else {
                list[0].clone()
            };
            Box::pin(async move {
                Ok(bmac_exec::Output {
                    exit: bmac_exec::Exit::Code(0),
                    stdout: serde_json::to_vec(&next).unwrap(),
                    stderr: Vec::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    elapsed: Duration::ZERO,
                })
            })
        }
    }

    #[tokio::test]
    async fn the_wait_keeps_going_until_the_host_is_online() {
        let offline = peer("mox1", "100.64.0.10", "2026-10-06T17:12:28Z", TAG, false);
        let online = peer("mox1", "100.64.0.10", "2026-10-06T17:12:28Z", TAG, true);
        let runner = Statuses(std::sync::Mutex::new(vec![
            json!({"Peer": {}}),
            json!({"Peer": {"a": offline}}),
            json!({"Peer": {"a": online}}),
        ]));
        let seen = std::sync::Mutex::new(Vec::new());
        let found = wait_for_peer(
            &runner,
            Path::new("/usr/bin/tailscale"),
            "mox1",
            "tag:bmac-proxmox-host",
            0,
            Duration::from_millis(1),
            Arc::new(Notify::new()),
            &CancellationToken::new(),
            &|s| seen.lock().unwrap().push(s.clone()),
        )
        .await
        .unwrap();
        assert_eq!(found.0, "100.64.0.10");
        assert_eq!(
            *seen.lock().unwrap(),
            [
                PeerSelection::Absent,
                PeerSelection::Offline,
                PeerSelection::Found {
                    ip: "100.64.0.10".into(),
                    dns_name: "mox1.example.ts.net".into()
                }
            ]
        );
    }

    #[tokio::test]
    async fn ambiguity_stops_the_wait() {
        let a = peer("mox1", "100.64.0.10", "2026-10-06T17:12:28Z", TAG, true);
        let b = peer("mox1", "100.64.0.11", "2026-10-06T17:12:29Z", TAG, true);
        let runner = Statuses(std::sync::Mutex::new(vec![
            json!({"Peer": {"a": a, "b": b}}),
        ]));
        let error = wait_for_peer(
            &runner,
            Path::new("/usr/bin/tailscale"),
            "mox1",
            "tag:bmac-proxmox-host",
            0,
            Duration::from_millis(1),
            Arc::new(Notify::new()),
            &CancellationToken::new(),
            &|_| {},
        )
        .await
        .unwrap_err();
        assert!(error.contains("expected exactly one"), "{error}");
    }
}
