// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Drives a discovery session for the lab (`lab/scenarios/m6.py`) through
//! the Deployer's own code. The session's client key and pinned
//! `known_hosts` live in `--session`; results are printed as JSON.
//!
//! lab_discovery pins
//! lab_discovery build --source A.iso --tailscale T.tgz --inventory BIN --out D.iso \
//!   --session DIR [--address A --prefix N --gateway G] [--dns D] [--mac M] < tailscale-key
//! lab_discovery wait --session DIR --since-ms MS
//! lab_discovery inventory --session DIR --address IP
//! lab_discovery clear --session DIR --address IP < clear-request.json
//! lab_discovery poweroff --session DIR --address IP

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_deployer::discovery::{
    self, DiscoveryRequest, DiscoverySession, SessionFiles, StaticNetwork,
};
use bmac_deployer::hostiso::keys::SshHostKey;
use bmac_deployer::tailnet;
use bmac_exec::{CancellationToken, RealRunner};
use bmac_proto::inventory::ClearRequest;

const SSH: &str = "/usr/bin/ssh";

fn stdin() -> Result<String, String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    Ok(text)
}

/// The session as `build` left it in the directory.
fn load(dir: &Path) -> Result<DiscoverySession, String> {
    let known = std::fs::read_to_string(dir.join("known_hosts")).map_err(|e| e.to_string())?;
    let (name, key) = known.trim().split_once(' ').ok_or("bad known_hosts")?;
    let private = std::fs::read_to_string(dir.join("client_key")).map_err(|e| e.to_string())?;
    Ok(DiscoverySession {
        tailscale_name: name.into(),
        host_key: key.into(),
        host_fingerprint: String::new(),
        client: SshHostKey {
            public: String::new(),
            private,
        },
    })
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut raw = std::env::args().skip(1);
    let action = raw.next().ok_or("an action is required")?;
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    while let Some(flag) = raw.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or(format!("unexpected {flag}"))?;
        args.insert(
            name.into(),
            raw.next().ok_or(format!("{flag} needs a value"))?,
        );
    }
    let get = |name: &str| {
        args.get(name)
            .cloned()
            .ok_or(format!("--{name} is required"))
    };
    if action == "pins" {
        let pin = |p: bmac_deployer::download::Pinned| serde_json::json!({"url": p.url, "sha256": p.sha256});
        println!(
            "{}",
            serde_json::json!({"alpine": pin(discovery::alpine_iso()), "tailscale": pin(discovery::tailscale_tgz())})
        );
        return Ok(());
    }
    let session_dir = PathBuf::from(get("session")?);
    let ssh = Path::new(SSH);
    match action.as_str() {
        "build" => {
            let network = match args.get("address") {
                Some(address) => Some(StaticNetwork {
                    address: address.parse().map_err(|_| "--address")?,
                    prefix: get("prefix")?.parse().map_err(|_| "--prefix")?,
                    gateway: get("gateway")?.parse().map_err(|_| "--gateway")?,
                }),
                None => None,
            };
            let request = DiscoveryRequest {
                network,
                dns: args
                    .get("dns")
                    .map(String::as_str)
                    .unwrap_or_default()
                    .split(',')
                    .filter(|d| !d.is_empty())
                    .map(|d| d.parse().map_err(|_| "--dns".to_string()))
                    .collect::<Result<_, _>>()?,
                public_mac: args.get("mac").cloned(),
                tailscale_key: stdin()?.trim().to_string(),
                carrier_attempts: 10,
                carrier_interval_s: 3,
            };
            let inventory = std::fs::read(get("inventory")?).map_err(|e| e.to_string())?;
            let tailscale = std::fs::read(get("tailscale")?).map_err(|e| e.to_string())?;
            let prepared = discovery::prepare(&request, &inventory, &tailscale)?;
            discovery::build(
                &prepared,
                Path::new(&get("source")?),
                Path::new(&get("out")?),
                Path::new("/usr/bin/xorriso"),
                &RealRunner,
            )
            .await?;
            std::fs::create_dir_all(&session_dir).map_err(|e| e.to_string())?;
            std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|e| e.to_string())?;
            let write = |name: &str, text: &str| -> Result<(), String> {
                let path = session_dir.join(name);
                std::fs::write(&path, text).map_err(|e| e.to_string())?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|e| e.to_string())
            };
            let s = &prepared.session;
            write("client_key", &s.client.private)?;
            write(
                "known_hosts",
                &format!("{} {}\n", s.tailscale_name, s.host_key),
            )?;
            println!(
                "{}",
                serde_json::json!({"name": s.tailscale_name, "fingerprint": s.host_fingerprint})
            );
        }
        "wait" => {
            let session = load(&session_dir)?;
            let since: i64 = get("since-ms")?.parse().map_err(|_| "--since-ms")?;
            let cancel = CancellationToken::new();
            let timer = cancel.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(600)).await;
                timer.cancel();
            });
            let (ip, dns) = tailnet::wait_for_peer(
                &RealRunner,
                Path::new("/usr/bin/tailscale"),
                &session.tailscale_name,
                tailnet::DISCOVERY_TAG,
                since,
                Duration::from_secs(5),
                Arc::new(tokio::sync::Notify::new()),
                &cancel,
                &|s| eprintln!("{s:?}"),
            )
            .await?;
            println!("{}", serde_json::json!({"ip": ip, "dns_name": dns}));
        }
        "inventory" | "clear" | "poweroff" => {
            let session = load(&session_dir)?;
            let address = get("address")?;
            let base = session_dir.join("live");
            let files = SessionFiles::create(&base, &session)?;
            match action.as_str() {
                "inventory" => {
                    let inventory =
                        discovery::read_inventory(&RealRunner, ssh, &files, &session, &address)
                            .await?;
                    println!(
                        "{}",
                        serde_json::to_string(&inventory).map_err(|e| e.to_string())?
                    );
                }
                "clear" => {
                    let request: ClearRequest =
                        serde_json::from_str(&stdin()?).map_err(|e| e.to_string())?;
                    let report = discovery::clear_labels(
                        &RealRunner,
                        ssh,
                        &files,
                        &session,
                        &address,
                        &request,
                    )
                    .await?;
                    println!(
                        "{}",
                        serde_json::to_string(&report).map_err(|e| e.to_string())?
                    );
                }
                _ => discovery::power_off(&RealRunner, ssh, &files, &session, &address).await,
            }
        }
        other => return Err(format!("unknown action {other}")),
    }
    Ok(())
}
