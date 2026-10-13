// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Workstation SSH set up after the fact (`DEPLOYMENT.md`, "Workstation
//! SSH"). [`host`] pins a host this workstation didn't install: its key is
//! read with `ssh-keyscan` and pinned only once the operator confirms it
//! against the fingerprint in Dashboard Settings, and the other hosts' keys
//! then come from Proxmox's own record over that pinned connection. It also
//! reports entries left behind by hosts that have left the cluster.
//! [`guest`] adds a guest reached through a host, pinned to the key the
//! cluster reads from inside the guest.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_core::job::{FieldType, PhaseStatus, RequestForm, Severity};
use serde_json::json;

use super::{Env, FieldExt, field, list, note, option, stopped, text};
use crate::jump_ssh::{self, ConfiguredHost, Guest, GuestList};
use crate::tailnet::{self, PeerSelection};
use crate::tasks::{Stop, TaskContext, TaskResult};
use crate::workstation_ssh::{self, HostPin, SshSetup, SshSetupError, pin_alias, valid_name};

type FieldErrors = (BTreeMap<String, String>, Option<String>);

fn one(id: &str, message: impl Into<String>) -> FieldErrors {
    (BTreeMap::from([(id.to_string(), message.into())]), None)
}

/// The workstation's `~/.ssh/config`, or nothing when there is none.
pub(crate) fn ssh_config(env: &Env) -> String {
    std::fs::read_to_string(env.ssh_dir.join("config")).unwrap_or_default()
}

pub(crate) fn setup<'a>(
    env: &'a Env,
    ssh: &'a Path,
    note: &'a (dyn Fn(String) + Send + Sync),
) -> SshSetup<'a> {
    SshSetup {
        ssh_dir: env.ssh_dir.clone(),
        state_dir: env.state_dir.join("ssh"),
        ssh,
        runner: &*env.runner,
        attempts: 3,
        wait: env.poll,
        lock_timeout: Duration::from_secs(30),
        note,
    }
}

fn names_list(text: Option<String>) -> Vec<String> {
    text.map(|t| t.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

/// The host's tailnet address, or why there is none.
async fn host_address(ctx: &TaskContext, env: &Env, host: &str) -> Result<String, Stop> {
    let tailscale = env.tool(&env.tools.tailscale, "tailscale")?;
    let status = tailnet::status(&*env.runner, tailscale)
        .await
        .map_err(|e| stopped(ctx, e))?;
    match tailnet::select_peer(&status, host, tailnet::HOST_TAG, 0) {
        PeerSelection::Found { ip, .. } => Ok(ip),
        PeerSelection::Absent => Err(Stop::failed(format!(
            "{host} isn't on this workstation's tailnet with the tag {}. Check that this \
             workstation is signed in to the cluster's tailnet.",
            tailnet::HOST_TAG
        ))),
        PeerSelection::Offline => Err(Stop::failed(format!(
            "{host} is on the tailnet but offline. Try again once it is running."
        ))),
        PeerSelection::Ambiguous(n) => Err(Stop::failed(format!(
            "{n} machines on the tailnet are named {host}. Remove the stale ones in the \
             Tailscale admin console, then try again."
        ))),
        PeerSelection::Untagged { tags } => Err(Stop::failed(format!(
            "The machine named {host} on the tailnet doesn't carry {} (it has {}), so it isn't \
             one of the cluster's hosts.",
            tailnet::HOST_TAG,
            if tags.is_empty() {
                "no tags".to_string()
            } else {
                tags.join(", ")
            }
        ))),
    }
}

/// Writes one pin and reports the outcome; `Err` only for a key mismatch,
/// which ends the task.
async fn apply(
    ctx: &TaskContext,
    setup: &SshSetup<'_>,
    pin: &HostPin,
    names: &[String],
) -> Result<bool, Stop> {
    match setup.apply(pin, names).await {
        Ok(changed) => {
            ctx.output(if changed {
                format!("ssh {} now works.", names[0])
            } else {
                format!("ssh {} was already set up.", names[0])
            });
            Ok(true)
        }
        Err(SshSetupError::Mismatch(m)) => Err(Stop::failed(m)),
        Err(SshSetupError::Failed(m)) => {
            ctx.warning(format!("ssh {} wasn't set up: {m}", names[0]));
            for line in workstation_ssh::manual_instructions(&names[0], pin) {
                ctx.output(line);
            }
            Ok(false)
        }
    }
}

fn cluster_field(known: &[String]) -> bmac_core::job::Field {
    let f = field("cluster", FieldType::String, "Cluster name")
        .required()
        .help(
            "PROXMOX_CLUSTER_NAME, as the Dashboard's Settings show it. It keeps this cluster's \
             pins apart from another cluster's hosts of the same name.",
        );
    match known {
        [only] => f.default_value(json!(only)),
        _ => f,
    }
}

/// Sets up SSH to a host this workstation didn't install, then to the
/// cluster's other hosts.
pub async fn host(ctx: &TaskContext, env: &Env) -> TaskResult {
    let ssh = env.tool(&env.tools.ssh, "ssh")?;
    let keyscan = env.tool(&env.tools.ssh_keyscan, "ssh-keyscan")?;
    let note = note(ctx);

    ctx.phase("host", "Host", PhaseStatus::Running);
    let known = jump_ssh::managed_clusters(&ssh_config(env));
    let values = ctx
        .values(
            RequestForm::InputGroup {
                title: "Which host?".into(),
                description: Some(
                    "For a host this workstation didn't install. Hosts installed from this \
                     workstation were set up at handoff."
                        .into(),
                ),
                context: None,
                layout: None,
                fields: vec![
                    cluster_field(&known),
                    field("host", FieldType::Select, "Host")
                        .required()
                        .default_value(json!("mox1"))
                        .choices(
                            HostSlot::all()
                                .map(|h| option(&h.to_string(), &h.to_string(), None))
                                .collect(),
                        ),
                    field("names", FieldType::String, "Other names").help(
                        "Optional: more names for ssh to accept for this host, separated by \
                         spaces. The slot name always works.",
                    ),
                ],
            },
            |values| {
                let cluster = text(values, "cluster").unwrap_or_default();
                if !valid_name(&cluster) {
                    return Err(one(
                        "cluster",
                        "Use the cluster's name: letters, digits, dots, underscores, or hyphens.",
                    ));
                }
                if !names_list(text(values, "names"))
                    .iter()
                    .all(|n| valid_name(n))
                {
                    return Err(one(
                        "names",
                        "Names use 1-64 letters, digits, dots, underscores, or hyphens.",
                    ));
                }
                Ok(())
            },
        )
        .await?;
    let cluster = text(&values, "cluster").unwrap_or_default();
    let host = text(&values, "host")
        .and_then(|h| h.parse::<HostSlot>().ok())
        .ok_or_else(|| Stop::failed("No host was chosen."))?
        .to_string();
    let mut names = vec![host.clone()];
    for name in names_list(text(&values, "names")) {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let address = host_address(ctx, env, &host).await?;
    ctx.output(format!("{host} is at {address} on the tailnet."));
    ctx.phase("host", "Host", PhaseStatus::Complete);

    ctx.phase("key", "Confirm the host key", PhaseStatus::Running);
    let key = workstation_ssh::scan(&*env.runner, keyscan, &address)
        .await
        .map_err(|e| stopped(ctx, e))?;
    let fingerprint = jump_ssh::key_fingerprint(&key)
        .ok_or_else(|| Stop::failed(format!("{host} offered an invalid Ed25519 key.")))?;
    let confirmed = ctx
        .confirm(RequestForm::Confirm {
            title: format!("Is this {host}'s SSH key?"),
            message: format!(
                "{host} at {address} offered the SSH host key {fingerprint}. Open the \
                 Dashboard through a link whose certificate fingerprint you have checked, go to \
                 Settings, then Certificates and SSH host keys, and compare {host}'s SSH \
                 fingerprint. Confirm only if they are identical."
            ),
            severity: Severity::Warning,
            confirm_label: Some("They're identical".into()),
            cancel_label: Some("They differ".into()),
            confirmation_text: None,
            context: None,
            details: Vec::new(),
        })
        .await?;
    if !confirmed {
        return Err(Stop::failed(
            "Nothing was written. If the fingerprints differ, something between this \
             workstation and the host may be intercepting SSH; don't connect to it until you \
             know why.",
        ));
    }
    ctx.phase("key", "Confirm the host key", PhaseStatus::Complete);

    ctx.phase("ssh", "Set up SSH", PhaseStatus::Running);
    let setup = setup(env, ssh, &note);
    let pin = HostPin {
        alias: pin_alias(&cluster, &host),
        address,
        host_key: key,
        jump: None,
    };
    if !apply(ctx, &setup, &pin, &names).await? {
        return Err(Stop::failed(format!("SSH to {host} wasn't set up.")));
    }
    ctx.phase("ssh", "Set up SSH", PhaseStatus::Complete);

    ctx.phase("others", "The other hosts", PhaseStatus::Running);
    let nodes = workstation_ssh::node_keys(&*env.runner, ssh, &host)
        .await
        .map_err(|e| stopped(ctx, e))?;
    let others: Vec<(String, String)> = nodes
        .into_iter()
        .filter(|(node, _)| *node != host && node.parse::<HostSlot>().is_ok())
        .collect();
    if !others.is_empty() {
        let chosen = ctx
            .values(
                RequestForm::InputGroup {
                    title: "Set up the other hosts too?".into(),
                    description: Some(format!(
                        "Their keys come from the cluster's own record, read through the \
                         connection to {host} you just confirmed, so they need no separate check."
                    )),
                    context: None,
                    layout: None,
                    fields: vec![
                        field("hosts", FieldType::Multiselect, "Hosts")
                            .default_value(json!(
                                others.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>()
                            ))
                            .choices(others.iter().map(|(n, _)| option(n, n, None)).collect()),
                    ],
                },
                |_| Ok(()),
            )
            .await?;
        for node in list(&chosen, "hosts") {
            let Some((_, key)) = others.iter().find(|(n, _)| *n == node) else {
                continue;
            };
            let address = match host_address(ctx, env, &node).await {
                Ok(address) => address,
                Err(Stop::Failed { message, .. }) => {
                    ctx.warning(format!("{node} skipped: {message}"));
                    continue;
                }
                Err(stop) => return Err(stop),
            };
            let pin = HostPin {
                alias: pin_alias(&cluster, &node),
                address,
                host_key: key.clone(),
                jump: None,
            };
            if let Err(Stop::Failed { message, .. }) =
                apply(ctx, &setup, &pin, std::slice::from_ref(&node)).await
            {
                ctx.warning(format!(
                    "{node} presented a key other than the one the cluster records for it, so it \
                     wasn't set up: {message}"
                ));
            }
        }
    }
    let members: Vec<String> = std::iter::once(host.clone())
        .chain(others.iter().map(|(n, _)| n.clone()))
        .collect();
    report_departed(ctx, &ssh_config(env), &cluster, &members);
    ctx.phase("others", "The other hosts", PhaseStatus::Complete);
    ctx.result(json!({ "host": host, "names": names }));
    Ok(())
}

/// Entries for hosts no longer in the cluster, and guest entries that jump
/// through them.
fn report_departed(ctx: &TaskContext, config: &str, cluster: &str, members: &[String]) {
    for gone in jump_ssh::configured_hosts(config, cluster)
        .into_iter()
        .filter(|h| !members.contains(&h.host))
    {
        ctx.warning(format!(
            "{} is no longer in the cluster, but ~/.ssh/config still has an entry for it ({}). \
             Remove it, or set it up again after reinstalling that slot.",
            gone.host,
            gone.names.join(" ")
        ));
        let mut removed = gone.names.clone();
        if !removed.contains(&gone.host) {
            removed.push(gone.host.clone());
        }
        let stale = jump_ssh::stale_aliases(config, &removed);
        if !stale.is_empty() {
            for line in jump_ssh::stale_report(&gone.host, &stale) {
                ctx.output(line);
            }
        }
    }
}

/// The cluster and a working host to ask, from the managed entries.
pub(crate) async fn cluster_and_list(
    ctx: &TaskContext,
    env: &Env,
    ssh: &Path,
    title: &str,
) -> Result<(String, Vec<ConfiguredHost>, GuestList), Stop> {
    let config = ssh_config(env);
    let clusters = jump_ssh::managed_clusters(&config);
    let cluster = match clusters.as_slice() {
        [] => {
            return Err(Stop::failed(
                "This workstation has no SSH entry for any cluster host. Set up SSH to a host \
                 first.",
            ));
        }
        [only] => only.clone(),
        many => {
            let values = ctx
                .values(
                    RequestForm::Input {
                        title: Some(title.into()),
                        description: None,
                        context: None,
                        field: field("cluster", FieldType::Select, "Cluster")
                            .required()
                            .choices(many.iter().map(|c| option(c, c, None)).collect()),
                    },
                    |_| Ok(()),
                )
                .await?;
            text(&values, "cluster").unwrap_or_default()
        }
    };
    let hosts = jump_ssh::configured_hosts(&config, &cluster);
    let mut last = String::new();
    for host in &hosts {
        let Some(name) = host.names.first() else {
            continue;
        };
        match jump_ssh::guests(&*env.runner, ssh, name).await {
            Ok(list) => {
                ctx.output(format!("Read the guest list through {name}."));
                return Ok((cluster, hosts, list));
            }
            Err(e) => {
                ctx.check()?;
                ctx.output(format!("{name} didn't answer: {e}"));
                last = e;
            }
        }
    }
    Err(Stop::failed(format!(
        "No host of {cluster} that this workstation reaches answered. {last}"
    )))
}

fn guest_label(guest: &Guest) -> String {
    format!(
        "{} ({}, {}, {}, {})",
        guest.name,
        guest.kind,
        guest.ip,
        guest
            .node
            .as_deref()
            .map_or("not placed".to_string(), |n| format!("on {n}")),
        guest.status
    )
}

pub(crate) async fn choose_guest(
    ctx: &TaskContext,
    guests: &[Guest],
    title: &str,
    description: &str,
) -> Result<Guest, Stop> {
    if guests.is_empty() {
        return Err(Stop::failed(
            "The cluster has no guest that can be chosen here.",
        ));
    }
    let values = ctx
        .values(
            RequestForm::Input {
                title: Some(title.into()),
                description: Some(description.into()),
                context: None,
                field: field("guest", FieldType::Select, "Guest")
                    .required()
                    .choices(
                        guests
                            .iter()
                            .map(|g| option(&g.name, &guest_label(g), None))
                            .collect(),
                    ),
            },
            |values| match guests
                .iter()
                .find(|g| Some(&g.name) == text(values, "guest").as_ref())
            {
                Some(g) if g.status != "running" => Err(one(
                    "guest",
                    format!("{} is {}; start it first.", g.name, g.status),
                )),
                Some(_) => Ok(()),
                None => Err(one("guest", "Choose a guest.")),
            },
        )
        .await?;
    let name = text(&values, "guest").unwrap_or_default();
    guests
        .iter()
        .find(|g| g.name == name)
        .cloned()
        .ok_or_else(|| Stop::failed("No guest was chosen."))
}

/// Sets up `ssh <guest>` through a host, pinned to the attested key.
pub async fn guest(ctx: &TaskContext, env: &Env) -> TaskResult {
    let ssh = env.tool(&env.tools.ssh, "ssh")?;
    let note = note(ctx);

    ctx.phase("guest", "Guest", PhaseStatus::Running);
    let (cluster, hosts, list) = cluster_and_list(ctx, env, ssh, "Which cluster's guest?").await?;
    let guest = choose_guest(
        ctx,
        &list.guests,
        "Which guest?",
        "The cluster's registered production and staging guests. The guest must be running.",
    )
    .await?;
    let jumps: Vec<String> = hosts
        .iter()
        .filter_map(|h| h.names.first().cloned())
        .collect();
    let values = ctx
        .values(
            RequestForm::InputGroup {
                title: format!("SSH to {}", guest.name),
                description: None,
                context: None,
                layout: None,
                fields: vec![
                    field("jump", FieldType::Select, "Through host")
                        .required()
                        .default_value(
                            jump_ssh::default_jump(&hosts, guest.node.as_deref())
                                .map_or(serde_json::Value::Null, |j| json!(j)),
                        )
                        .help("The host ssh goes through; the default is the host running the guest.")
                        .choices(jumps.iter().map(|j| option(j, j, None)).collect()),
                    field("alias", FieldType::String, "SSH name")
                        .required()
                        .default_value(json!(guest.name))
                        .help(&format!(
                            "The name for ssh. Tools that expect `ssh {}` won't use another one.",
                            guest.name
                        )),
                ],
            },
            |values| {
                let alias = text(values, "alias").unwrap_or_default();
                if !valid_name(&alias) || alias.parse::<HostSlot>().is_ok() {
                    return Err(one(
                        "alias",
                        "Use 1-64 letters, digits, dots, underscores, or hyphens, and not a host's name.",
                    ));
                }
                if !jumps.contains(&text(values, "jump").unwrap_or_default()) {
                    return Err(one("jump", "Choose a host this workstation reaches."));
                }
                Ok(())
            },
        )
        .await?;
    let jump = text(&values, "jump").unwrap_or_default();
    let alias = text(&values, "alias").unwrap_or_default();
    if alias != guest.name {
        ctx.warning(format!(
            "{alias} is not the guest's name, so tools that run `ssh {}` won't use this entry.",
            guest.name
        ));
    }
    ctx.phase("guest", "Guest", PhaseStatus::Complete);

    ctx.phase("key", "The guest's host key", PhaseStatus::Running);
    jump_ssh::check_jump(&*env.runner, ssh, &jump)
        .await
        .map_err(|e| stopped(ctx, e))?;
    let key = jump_ssh::guest_key(&*env.runner, ssh, &jump, &guest.name)
        .await
        .map_err(|e| stopped(ctx, e))?;
    ctx.output(format!(
        "The cluster read {}'s SSH host key from inside the guest, through the guest agent on {}: \
         {}",
        guest.name, key.node, key.fingerprint
    ));
    let pin = jump_ssh::guest_pin(&cluster, &guest, &key, &jump).map_err(Stop::failed)?;
    ctx.phase("key", "The guest's host key", PhaseStatus::Complete);

    ctx.phase("ssh", "Set up SSH", PhaseStatus::Running);
    let setup = setup(env, ssh, &note);
    if !apply(ctx, &setup, &pin, std::slice::from_ref(&alias)).await? {
        return Err(Stop::failed(format!(
            "ssh {alias} wasn't set up. If the reason is a refused login, this workstation's key \
             isn't in the guest's /root/.ssh/authorized_keys."
        )));
    }
    ctx.phase("ssh", "Set up SSH", PhaseStatus::Complete);
    ctx.result(json!({ "guest": guest.name, "alias": alias, "jump": jump }));
    Ok(())
}
