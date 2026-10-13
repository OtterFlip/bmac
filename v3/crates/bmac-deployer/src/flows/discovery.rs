// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A discovery session (`DEPLOYMENT.md`, "The discovery session"): the
//! network settings and ephemeral key, the ISO, the operator's boot steps,
//! the wait on the tailnet, the inventory over pinned SSH, clearing old
//! `rpool` labels if the operator agrees, and the power-off.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use bmac_core::job::{FieldType, Instruction, PhaseStatus, RequestForm, Severity};
use bmac_proto::deployer::{BuiltIso, IsoKind};
use bmac_proto::inventory::{ClearRequest, ClearTarget, Inventory, InventoryDisk, NicLink};
use serde_json::json;

use super::{Env, FieldExt, download_progress, field, list, note, option, stopped, text};
use crate::boot::{self, BootMethod};
use crate::discovery::{self as iso, DiscoveryRequest, DiscoverySession, SessionFiles};
use crate::tailnet::{self, PeerSelection};
use crate::tasks::{Stop, TaskContext, TaskResult, Values};

/// What the operator already gave, such as a host's network settings.
#[derive(Debug, Clone, Default)]
pub struct Preset {
    pub address: Option<String>,
    pub prefix: Option<u8>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
    pub public_mac: Option<String>,
    pub idrac: Option<String>,
}

/// What a session found, and how the machine was booted.
#[derive(Debug, Clone)]
pub struct Found {
    pub inventory: Inventory,
    /// How the discovery OS brought up the public port.
    pub network: Option<iso::SeenNetwork>,
    pub method: BootMethod,
    pub idrac: Option<String>,
}

pub async fn run(ctx: &TaskContext, env: &Env) -> TaskResult {
    discover(ctx, env, &Preset::default()).await.map(|_| ())
}

type FieldErrors = (BTreeMap<String, String>, Option<String>);

fn one(id: &str, message: impl Into<String>) -> FieldErrors {
    (BTreeMap::from([(id.to_string(), message.into())]), None)
}

pub(crate) fn method_field(default: BootMethod) -> bmac_core::job::Field {
    let value = match default {
        BootMethod::Idrac => "idrac",
        BootMethod::RemoteConsole => "remote_console",
        BootMethod::Usb => "usb",
    };
    field("method", FieldType::Select, "How you'll boot the machine")
        .required()
        .default_value(json!(value))
        .choices(vec![
            option(
                "idrac",
                "iDRAC Virtual Console",
                Some("Map the ISO in the Virtual Console's Virtual Media."),
            ),
            option(
                "remote_console",
                "Another remote console",
                Some("Another vendor's BMC, a KVM over IP, or a provider's console."),
            ),
            option(
                "usb",
                "USB stick",
                Some("Write the ISO to a stick and plug it in."),
            ),
        ])
}

pub(crate) fn idrac_field(default: Option<&str>) -> bmac_core::job::Field {
    field("idrac", FieldType::Hostname, "iDRAC address")
        .help("Optional. Only used to link to the iDRAC's web interface in the steps.")
        .default_value(default.map_or(serde_json::Value::Null, |d| json!(d)))
}

pub(crate) fn parse_method(values: &Values) -> BootMethod {
    match text(values, "method").as_deref() {
        Some("remote_console") => BootMethod::RemoteConsole,
        Some("usb") => BootMethod::Usb,
        _ => BootMethod::Idrac,
    }
}

pub(crate) fn parse_idrac(values: &Values) -> Result<Option<String>, FieldErrors> {
    match text(values, "idrac") {
        None => Ok(None),
        Some(address) if crate::plain_https(&boot::idrac_console_url(&address)) => {
            Ok(Some(address))
        }
        Some(_) => Err(one("idrac", "Enter a host name or an IP address.")),
    }
}

fn ipv4(values: &Values, id: &str) -> Result<Ipv4Addr, FieldErrors> {
    text(values, id)
        .and_then(|t| t.parse().ok())
        .ok_or_else(|| one(id, "Enter an IPv4 address."))
}

/// All three static values, or none for DHCP.
pub(crate) fn static_network(values: &Values) -> Result<Option<iso::StaticNetwork>, FieldErrors> {
    let given = ["address", "prefix", "gateway"].map(|id| text(values, id).is_some());
    if given.iter().all(|g| !g) {
        return Ok(None);
    }
    let prefix = text(values, "prefix")
        .and_then(|t| t.parse::<u8>().ok())
        .filter(|p| (1..=32).contains(p))
        .ok_or_else(|| {
            one(
                "prefix",
                "Enter a prefix length from 1 to 32, or leave the address, prefix, and gateway \
                 all empty for DHCP.",
            )
        })?;
    let need = |id: &str| {
        ipv4(values, id).map_err(|_| {
            one(
                id,
                "Enter an IPv4 address, or leave the address, prefix, and gateway all empty \
                 for DHCP.",
            )
        })
    };
    Ok(Some(iso::StaticNetwork {
        address: need("address")?,
        prefix,
        gateway: need("gateway")?,
    }))
}

fn request_from(values: &Values, env: &Env) -> Result<DiscoveryRequest, FieldErrors> {
    let network = static_network(values)?;
    let mut dns = Vec::new();
    for part in text(values, "dns")
        .unwrap_or_default()
        .split([',', ' '])
        .filter(|p| !p.is_empty())
    {
        dns.push(
            part.parse()
                .map_err(|_| one("dns", format!("{part} is not an IPv4 address.")))?,
        );
    }
    if network.is_some() && dns.is_empty() {
        return Err(one(
            "dns",
            "With a static address, enter at least one DNS server.",
        ));
    }
    let request = DiscoveryRequest {
        network,
        dns,
        public_mac: text(values, "public_mac"),
        tailscale_key: text(values, "tailscale_key").unwrap_or_default(),
        carrier_attempts: env.link_wait.attempts,
        carrier_interval_s: env.link_wait.interval_seconds,
    };
    iso::check_request(&request).map_err(|message| (BTreeMap::new(), Some(message)))?;
    Ok(request)
}

fn settings_form(preset: &Preset) -> RequestForm {
    let text_default =
        |v: &Option<String>| v.as_ref().map_or(serde_json::Value::Null, |v| json!(v));
    RequestForm::InputGroup {
        title: "Discovery settings".into(),
        description: Some(
            "The discovery OS gets an internet connection, joins your tailnet, and reports \
             the machine's disks and network ports. Leave the network settings empty to use \
             DHCP on whichever port gets a lease; give them when the public port needs a \
             static address. Many data centers have no DHCP: if a machine booted without \
             them never joins the tailnet, run this again with them. It installs nothing and writes to no disk unless you agree to \
             clear old pool labels."
                .into(),
        ),
        context: None,
        layout: None,
        fields: vec![
            field("address", FieldType::IpAddress, "Static public address")
                .help("Optional. Empty: DHCP.")
                .default_value(text_default(&preset.address)),
            field("prefix", FieldType::Integer, "Prefix length")
                .help("With a static address.")
                .range(1.0, 32.0)
                .default_value(preset.prefix.map_or(serde_json::Value::Null, |p| json!(p))),
            field("gateway", FieldType::IpAddress, "Gateway")
                .help("With a static address.")
                .default_value(text_default(&preset.gateway)),
            field("dns", FieldType::String, "DNS servers")
                .help("Separated by commas. Needed with a static address; with DHCP, they replace the lease's.")
                .default_value(if preset.dns.is_empty() {
                    serde_json::Value::Null
                } else {
                    json!(preset.dns.join(", "))
                }),
            field(
                "public_mac",
                FieldType::MacAddress,
                "Public port's MAC address",
            )
            .help("Optional. Empty: each port is tried in turn until one reaches the internet.")
            .default_value(text_default(&preset.public_mac)),
            field("tailscale_key", FieldType::Password, "Tailscale auth key")
                .required()
                .help(&format!(
                    "Single-use and ephemeral, tagged {}. The machine leaves the tailnet by \
                     itself once it is powered off.",
                    tailnet::DISCOVERY_TAG
                )),
            method_field(if preset.idrac.is_some() {
                BootMethod::Idrac
            } else {
                BootMethod::RemoteConsole
            }),
            idrac_field(preset.idrac.as_deref()),
        ],
    }
}

/// The steps as instructions, with the ISO's path to copy.
pub(crate) fn instructions(steps: Vec<String>, iso: &str) -> Vec<Instruction> {
    let mut list = vec![Instruction::WithCopy {
        text: format!("The ISO: {iso}"),
        copy: iso.to_string(),
        run_on: None,
    }];
    list.extend(steps.into_iter().map(Instruction::Text));
    list
}

/// Reports what the tailnet shows while waiting for `name`.
pub(crate) fn peer_report(
    ctx: &TaskContext,
    name: String,
    tag: String,
) -> impl Fn(&PeerSelection) + Send + Sync + use<> {
    let ctx = ctx.clone();
    move |selection| match selection {
        PeerSelection::Absent => ctx.output(format!("Waiting for {name} to join the tailnet.")),
        PeerSelection::Offline => ctx.output(format!("{name} is on the tailnet but offline.")),
        PeerSelection::Found { ip, .. } => ctx.output(format!("{name} is on the tailnet at {ip}.")),
        PeerSelection::Untagged { tags } => ctx.warning(format!(
            "{name} joined the tailnet without {tag} (its tags: {}), so its auth key was the \
             wrong one. Remove it in the Tailscale admin console, then build a new ISO with a \
             key tagged {tag}.",
            if tags.is_empty() {
                "none".to_string()
            } else {
                tags.join(", ")
            }
        )),
        PeerSelection::Ambiguous(_) => {}
    }
}

fn size(bytes: u64) -> String {
    bmac_core::Bytes(bytes).to_string()
}

fn link_text(link: &NicLink) -> String {
    match link {
        NicLink::Up {
            speed_mbps: Some(speed),
            ..
        } => format!("link up at {speed} Mb/s"),
        NicLink::Up { .. } => "link up".into(),
        NicLink::NoCarrier { waited_seconds } => {
            format!("no carrier after {waited_seconds} seconds")
        }
        NicLink::Error { message } => format!("couldn't be read: {message}"),
    }
}

fn describe(disk: &InventoryDisk) -> String {
    format!(
        "{} ({}, {}{})",
        disk.name,
        disk.model.as_deref().unwrap_or("unknown model"),
        size(disk.size_bytes),
        disk.serial
            .as_deref()
            .map(|s| format!(", serial {s}"))
            .unwrap_or_default()
    )
}

fn summarize(ctx: &TaskContext, inventory: &Inventory) {
    for disk in &inventory.disks {
        ctx.output(format!("Disk {}", describe(disk)));
        for problem in &disk.problems {
            ctx.warning(format!("{}: {problem}", disk.name));
        }
    }
    for nic in &inventory.nics {
        ctx.output(format!(
            "Port {} ({}): {}",
            nic.name,
            nic.mac,
            link_text(&nic.link)
        ));
    }
}

/// Disks carrying an `rpool` label, with the label GUIDs to clear.
fn rpool_disks(inventory: &Inventory) -> Vec<(&InventoryDisk, Vec<String>)> {
    inventory
        .disks
        .iter()
        .filter_map(|disk| {
            let mut guids: Vec<String> = disk
                .zfs_labels
                .iter()
                .filter(|l| l.pool.as_deref() == Some("rpool"))
                .filter_map(|l| l.pool_guid.clone())
                .collect();
            guids.sort();
            guids.dedup();
            (!guids.is_empty()).then_some((disk, guids))
        })
        .collect()
}

async fn read(
    ctx: &TaskContext,
    env: &Env,
    files: &SessionFiles,
    session: &DiscoverySession,
    address: &str,
) -> Result<Inventory, Stop> {
    let ssh = env.tool(&env.tools.ssh, "ssh")?;
    let mut last = String::new();
    for attempt in 0..5 {
        if attempt > 0 {
            tokio::select! {
                _ = ctx.token().cancelled() => return Err(Stop::Cancelled),
                _ = tokio::time::sleep(env.poll) => {}
            }
        }
        match iso::read_inventory(&*env.runner, ssh, files, session, address).await {
            Ok(inventory) => return Ok(inventory),
            Err(e) => {
                ctx.output(format!("{e}; trying again."));
                last = e;
            }
        }
    }
    Err(Stop::failed(last))
}

/// Offers to clear old `rpool` labels; returns the inventory as it then is.
async fn offer_clearing(
    ctx: &TaskContext,
    env: &Env,
    files: &SessionFiles,
    session: &DiscoverySession,
    address: &str,
    inventory: Inventory,
) -> Result<Inventory, Stop> {
    let labelled = rpool_disks(&inventory);
    for disk in inventory.disks.iter().filter(|d| {
        d.zfs_labels
            .iter()
            .any(|l| l.pool.as_deref() != Some("rpool"))
    }) {
        ctx.output(format!(
            "{} carries labels of another pool; it doesn't stop the boot, and setup won't use \
             the disk until it is cleared.",
            describe(disk)
        ));
    }
    if labelled.is_empty() {
        return Ok(inventory);
    }
    ctx.phase("labels", "Old rpool labels", PhaseStatus::Running);
    let (offered, unnamed): (Vec<_>, Vec<_>) =
        labelled.iter().partition(|(disk, _)| disk.serial.is_some());
    for (disk, _) in &unnamed {
        ctx.warning(format!(
            "{} carries an old rpool label but reports no serial number, so the Deployer can't \
             safely clear it. Clear it by hand (zpool labelclear) before installing.",
            describe(disk)
        ));
    }
    if offered.is_empty() {
        ctx.phase("labels", "Old rpool labels", PhaseStatus::Complete);
        return Ok(inventory);
    }
    let serials: Vec<String> = offered
        .iter()
        .filter_map(|(d, _)| d.serial.clone())
        .collect();
    let chosen = ctx
        .values(
            RequestForm::InputGroup {
                title: "Clear old rpool labels?".into(),
                description: Some(
                    "These disks still carry labels of an earlier pool named rpool. The \
                     installer erases only the disks it installs to, so the installed host \
                     would find two pools named rpool and stop at the (initramfs) prompt. \
                     Clearing removes only those labels."
                        .into(),
                ),
                context: None,
                layout: None,
                fields: vec![
                    field("clear", FieldType::Multiselect, "Disks to clear")
                        .default_value(json!(serials))
                        .choices(
                            offered
                                .iter()
                                .map(|(d, guids)| {
                                    option(
                                        d.serial.as_deref().unwrap_or_default(),
                                        &describe(d),
                                        Some(&format!("old pool GUID {}", guids.join(", "))),
                                    )
                                })
                                .collect(),
                        ),
                ],
            },
            |_| Ok(()),
        )
        .await?;
    let chosen = list(&chosen, "clear");
    if chosen.is_empty() {
        ctx.warning(
            "The old rpool labels were kept. The installed host may stop at the (initramfs) \
             prompt; there, `zpool import` lists the pools, and `zpool labelclear -f` on the old \
             member's device clears it.",
        );
        ctx.phase("labels", "Old rpool labels", PhaseStatus::Skipped);
        return Ok(inventory);
    }
    let picked: Vec<_> = offered
        .iter()
        .filter(|(d, _)| d.serial.as_ref().is_some_and(|s| chosen.contains(s)))
        .collect();
    let confirmed = ctx
        .confirm(RequestForm::Confirm {
            title: format!("Clear the old labels on {} disks?", picked.len()),
            message: "Only the rpool labels are cleared; each disk is checked by serial, WWN, \
                      and pool GUID first, and nothing is touched on a mismatch."
                .into(),
            severity: Severity::Destructive,
            confirm_label: Some("Clear the labels".into()),
            cancel_label: Some("Keep them".into()),
            confirmation_text: Some("clear".into()),
            context: None,
            details: picked.iter().map(|(d, _)| describe(d)).collect(),
        })
        .await?;
    if !confirmed {
        ctx.warning("The old rpool labels were kept.");
        ctx.phase("labels", "Old rpool labels", PhaseStatus::Skipped);
        return Ok(inventory);
    }
    let request = ClearRequest {
        disks: picked
            .iter()
            .flat_map(|(d, guids)| {
                guids.iter().map(|guid| ClearTarget {
                    serial: d.serial.clone().unwrap_or_default(),
                    wwn: d.wwn.clone(),
                    pool_guid: guid.clone(),
                })
            })
            .collect(),
    };
    let ssh = env.tool(&env.tools.ssh, "ssh")?;
    let report = iso::clear_labels(&*env.runner, ssh, files, session, address, &request).await?;
    for device in &report.cleared {
        ctx.output(format!("Cleared the label on {device}."));
    }
    if !report.succeeded() {
        let problems: Vec<String> = report
            .refused
            .iter()
            .chain(&report.survived)
            .chain(&report.errors)
            .cloned()
            .collect();
        return Err(Stop::Failed {
            message: "The labels weren't all cleared.".into(),
            details: Some(problems.join("\n")),
        });
    }
    let inventory = read(ctx, env, files, session, address).await?;
    if !rpool_disks(&inventory)
        .iter()
        .all(|(d, _)| d.serial.as_ref().is_none_or(|s| !chosen.contains(s)))
    {
        return Err(Stop::failed(
            "A cleared disk still shows an rpool label on a fresh read.",
        ));
    }
    ctx.phase("labels", "Old rpool labels", PhaseStatus::Complete);
    Ok(inventory)
}

/// Runs a whole session and returns the machine's inventory.
pub async fn discover(ctx: &TaskContext, env: &Env, preset: &Preset) -> Result<Found, Stop> {
    let xorriso = env.tool(&env.tools.xorriso, "xorriso")?;
    let tailscale = env.tool(&env.tools.tailscale, "tailscale")?;
    env.tool(&env.tools.ssh, "ssh")?;
    let inventory_binary = env.inventory_binary.clone().ok_or_else(|| {
        Stop::failed(
            "This Deployer has no bmac-inventory program to put on the discovery ISO. Reinstall \
             the Deployer's package.",
        )
    })?;

    ctx.phase("settings", "Discovery settings", PhaseStatus::Running);
    let values = ctx
        .values(settings_form(preset), |values| {
            request_from(values, env)?;
            parse_idrac(values)?;
            Ok(())
        })
        .await?;
    let request = request_from(&values, env)
        .map_err(|(_, m)| Stop::failed(m.unwrap_or_else(|| "The settings aren't valid.".into())))?;
    let method = parse_method(&values);
    let idrac = parse_idrac(&values).map_err(|_| Stop::failed("The iDRAC address isn't valid."))?;
    ctx.phase("settings", "Discovery settings", PhaseStatus::Complete);

    ctx.phase("download", "Downloads", PhaseStatus::Running);
    let note = note(ctx);
    let (source, tailscale_build) = &env.discovery_sources;
    let source = crate::download::ensure(
        &env.cache_dir.join("discovery-iso"),
        source,
        &*env.fetch,
        &download_progress(ctx, "the Alpine ISO".into()),
        &note,
    )
    .await?;
    let tgz = crate::download::ensure(
        &env.cache_dir.join("tailscale"),
        tailscale_build,
        &*env.fetch,
        &download_progress(ctx, "Tailscale".into()),
        &note,
    )
    .await?;
    ctx.phase("download", "Downloads", PhaseStatus::Complete);
    ctx.check()?;

    ctx.phase("build", "Discovery ISO", PhaseStatus::Running);
    let read_file = |path: &std::path::Path| {
        std::fs::read(path).map_err(|e| Stop::failed(format!("{}: {e}", path.display())))
    };
    let prepared = iso::prepare(&request, &read_file(&inventory_binary)?, &read_file(&tgz)?)?;
    let name = prepared.session.tailscale_name.clone();
    let output = env.iso_dir.join(format!("{name}.iso"));
    iso::build(&prepared, &source, &output, xorriso, &*env.runner).await?;
    let built_ms = env.now();
    let path = output.display().to_string();
    env.isos.record(BuiltIso {
        path: path.clone(),
        kind: IsoKind::Discovery,
        target: name.clone(),
        built_ms,
    });
    ctx.output(format!("Built {path}."));
    ctx.output(format!(
        "The discovery OS's SSH host key: {}",
        prepared.session.host_fingerprint
    ));
    ctx.phase("build", "Discovery ISO", PhaseStatus::Complete);
    let session = prepared.session;

    let mut on_tailnet = false;
    let found = boot_and_read(
        ctx,
        env,
        Booting {
            tailscale,
            path: &path,
            name,
            session: &session,
            method,
            idrac,
            dhcp: request.network.is_none(),
        },
        &mut on_tailnet,
    )
    .await;
    dispose(ctx, env, &path, on_tailnet).await;
    found
}

/// What the boot and the session need from the build.
struct Booting<'a> {
    tailscale: &'a std::path::Path,
    path: &'a str,
    name: String,
    session: &'a DiscoverySession,
    method: BootMethod,
    idrac: Option<String>,
    dhcp: bool,
}

/// Boots the machine from the ISO and reads it. `on_tailnet` is set once
/// the machine is online, after which it no longer reads the ISO.
async fn boot_and_read(
    ctx: &TaskContext,
    env: &Env,
    booting: Booting<'_>,
    on_tailnet: &mut bool,
) -> Result<Found, Stop> {
    let Booting {
        tailscale,
        path,
        name,
        session,
        method,
        idrac,
        dhcp,
    } = booting;
    ctx.phase("boot", "Boot the machine", PhaseStatus::Running);
    ctx.acknowledge(RequestForm::ManualAction {
        title: "Boot the machine from the discovery ISO".into(),
        instructions: instructions(
            boot::discovery_steps(method, path, idrac.as_deref(), dhcp),
            path,
        ),
        acknowledge_label: Some("It's booting".into()),
        context: None,
    })
    .await?;
    ctx.phase("boot", "Boot the machine", PhaseStatus::Complete);

    ctx.phase("wait", "Wait for the tailnet", PhaseStatus::Running);
    let tag = tailnet::DISCOVERY_TAG.to_string();
    let report = peer_report(ctx, name.clone(), tag.clone());
    let (ip, _) = tailnet::wait_for_peer(
        &*env.runner,
        tailscale,
        &name,
        &tag,
        0,
        env.poll,
        Arc::new(tokio::sync::Notify::new()),
        ctx.token(),
        &report,
    )
    .await
    .map_err(|e| stopped(ctx, e))?;
    *on_tailnet = true;
    ctx.phase("wait", "Wait for the tailnet", PhaseStatus::Complete);

    ctx.phase("inventory", "Read the hardware", PhaseStatus::Running);
    let files = SessionFiles::create(&env.state_dir, session)?;
    let inventory = read(ctx, env, &files, session, &ip).await?;
    summarize(ctx, &inventory);
    ctx.phase("inventory", "Read the hardware", PhaseStatus::Complete);

    let inventory = offer_clearing(ctx, env, &files, session, &ip, inventory).await?;

    ctx.phase("finish", "Power off", PhaseStatus::Running);
    let ssh = env.tool(&env.tools.ssh, "ssh")?;
    let network = iso::read_public_network(&*env.runner, ssh, &files, session, &ip).await;
    iso::power_off(&*env.runner, ssh, &files, session, &ip).await;
    drop(files);
    let file = env.iso_dir.join(format!("inventory-{name}.json"));
    let text = serde_json::to_vec_pretty(&inventory).map_err(|e| e.to_string())?;
    std::fs::write(&file, text).map_err(|e| format!("{}: {e}", file.display()))?;
    ctx.result(json!({
        "inventory_file": file.display().to_string(),
        "system": [inventory.system.vendor, inventory.system.product, inventory.system.serial],
        "disks": inventory.disks.len(),
        "ports": inventory.nics.len(),
    }));
    if let Some(removal) = boot::discovery_steps(method, path, idrac.as_deref(), dhcp).pop() {
        ctx.next_step(removal);
    }
    ctx.phase("finish", "Power off", PhaseStatus::Complete);
    Ok(Found {
        inventory,
        network,
        method,
        idrac,
    })
}

/// Shreds an ISO once its machine has stopped reading it (it reached the
/// tailnet). Before that the machine may still be booting from it, so it is
/// kept and stays listed in Settings.
pub(crate) async fn dispose(ctx: &TaskContext, env: &Env, path: &str, spent: bool) {
    if !spent {
        ctx.output(format!(
            "{path} is kept, because the machine may still be booting from it. It holds keys \
             made for this machine: delete it in Settings once you're done with it."
        ));
        return;
    }
    let isos = env.isos.clone();
    let file = path.to_string();
    match tokio::task::spawn_blocking(move || isos.delete(&file)).await {
        Ok(Ok(())) => ctx.output(format!(
            "Shredded {path}: the machine no longer reads it, and it held keys made for it."
        )),
        Ok(Err(e)) => ctx.warning(format!(
            "Couldn't shred {path}: {e}. It holds keys made for this machine: delete it in \
             Settings."
        )),
        Err(e) => ctx.warning(format!(
            "Couldn't shred {path}: {e}. It holds keys made for this machine: delete it in \
             Settings."
        )),
    }
}

/// How long the tailnet wait and the certificate wait look between tries
/// by default.
pub const POLL: Duration = Duration::from_secs(5);
