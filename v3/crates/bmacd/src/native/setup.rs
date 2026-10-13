// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The host setup job (`DEPLOYMENT.md`, "Host setup job"): what v2's
//! `add_proxmox_host.sh` did after the install, run on the new host from the
//! bootstrap file the first boot left in `/var/lib/bmac/bootstrap/`. Each
//! phase is recorded as a checkpoint when it finishes, so a job resumed
//! after a reboot carries on from the first unfinished one.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use bmac_core::HostSlot;
use bmac_core::disk::DiskIdentity;
use bmac_core::job::{PhaseStatus, Severity};
use bmac_proto::api::Role;
use bmac_proto::bootstrap::{
    BOOTSTRAP_DIR, BOOTSTRAP_FILE, BOOTSTRAP_VERSION, BootstrapOrigin, HostBootstrap,
};
use bmac_proto::inventory::{ClearRequest, ClearTarget, Inventory, InventoryDisk, SystemInfo};
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use bmac_storage::{ById, PoolLayout};
use serde_json::json;

use super::{Checkpoints, Stop, Workflow};

pub const OPERATION: &str = "hosts.setup";
/// Written when the job finishes; `bmacd` leaves setup mode once it exists.
pub const SETUP_DONE: &str = "/var/lib/bmac/setup-complete";

/// The phases in order, as the job view lists them.
pub const PHASES: &[(&str, &str)] = &[
    ("identity", "Verify identity"),
    ("settings", "Cluster settings"),
    ("storage", "Storage"),
    ("drills", "Boot drills"),
    ("network", "Private network"),
    ("cluster", "Cluster"),
    ("retire", "Retire the setup key"),
    ("orchestration", "Routing and orchestration"),
    ("headers", "LUKS header backup"),
    ("verify", "Verify the end state"),
];

/// A joining host's checkpoints. The job runs over SSH from the host that
/// approved the join, not under this host's `bmacd`.
pub const DRIVEN_WORK: &str = "/var/lib/bmac/setup-work";
const AUTHORIZED_KEYS: &str = "/root/.ssh/authorized_keys";
const CLUSTER_AUTHORIZED_KEYS: &str = "/etc/pve/priv/authorized_keys";

/// The cluster member running a joining host's setup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Driver {
    pub host: HostSlot,
    /// Its private VLAN address, which `pvecm add` joins through.
    pub address: std::net::Ipv4Addr,
    /// The SHA-256 fingerprint of the certificate it serves on 8006.
    pub fingerprint: String,
}

/// `--driven-by HOST --join-address IP --join-fingerprint FP`, or nothing.
pub fn parse_args(args: &[String]) -> Result<Option<Driver>, String> {
    if args.is_empty() {
        return Ok(None);
    }
    let mut host = None;
    let mut address = None;
    let mut fingerprint = None;
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let slot = match flag.as_str() {
            "--driven-by" => &mut host,
            "--join-address" => &mut address,
            "--join-fingerprint" => &mut fingerprint,
            other => return Err(format!("hosts.setup doesn't take {other}")),
        };
        let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
        if slot.replace(value.clone()).is_some() {
            return Err(format!("{flag} is given twice"));
        }
    }
    match (host, address, fingerprint) {
        (Some(host), Some(address), Some(fingerprint)) => {
            let host = host
                .parse::<HostSlot>()
                .map_err(|_| format!("{host} is not a BMAC host name"))?;
            let address = address
                .parse()
                .map_err(|_| format!("{address} is not an IPv4 address"))?;
            let fingerprint = super::cluster::openssl_fingerprint(&format!("={fingerprint}"))
                .ok_or_else(|| format!("{fingerprint} is not a SHA-256 fingerprint"))?;
            Ok(Some(Driver {
                host,
                address,
                fingerprint,
            }))
        }
        _ => Err("--driven-by, --join-address, and --join-fingerprint go together".into()),
    }
}

/// The key's type and base64 body, which identify it whatever its comment.
fn key_body(line: &str) -> Option<(&str, &str)> {
    let mut words = line.split_whitespace();
    let (kind, body) = (words.next()?, words.next()?);
    kind.starts_with("ssh-").then_some((kind, body))
}

/// `authorized_keys` without `key`.
pub fn without_key(text: &str, key: &str) -> String {
    let Some(retired) = key_body(key) else {
        return text.to_string();
    };
    text.lines()
        .filter(|line| key_body(line) != Some(retired))
        .map(|line| format!("{line}\n"))
        .collect()
}

pub fn operation() -> Operation {
    Operation {
        id: OPERATION.parse().expect("a valid operation id"),
        title: "Set up this host".into(),
        summary: "Finish a new host's setup after the Proxmox install.".into(),
        description: "Checks the hardware against what the host ISO was built for, stores the \
                      cluster settings, builds the storage, sets up the private network, creates \
                      or joins the cluster, and verifies the end state. It asks for input where \
                      it needs it and resumes from its last finished phase after a reboot."
            .into(),
        category: Category::Hosts,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: true,
        supports_dry_run: false,
        params: Vec::new(),
        locks: vec!["cluster-membership".into()],
        target: TargetRule::Any {},
        cancel: CancelRule::Phases {
            not_during: vec!["storage".into(), "cluster".into()],
        },
        resume: ResumeRule::Checkpoint,
        notes: Some(
            "Replaces steps 5 to 9 of v2's add_proxmox_host.sh, which ran on the workstation \
             over SSH."
                .into(),
        ),
    }
}

/// Where the job reads and writes on the host.
#[derive(Debug, Clone)]
pub struct Paths {
    pub bootstrap: PathBuf,
    pub done: PathBuf,
    /// The cluster's identity record ([`crate::joins::ClusterRecord`]).
    pub cluster_record: PathBuf,
    /// A joining host's request ([`crate::joiner`]).
    pub join_state: PathBuf,
}

impl Paths {
    pub fn system() -> Paths {
        Paths {
            bootstrap: Path::new(BOOTSTRAP_DIR).join(BOOTSTRAP_FILE),
            done: PathBuf::from(SETUP_DONE),
            cluster_record: PathBuf::from(crate::joins::CLUSTER_RECORD),
            join_state: PathBuf::from(crate::joiner::JOIN_STATE),
        }
    }
}

pub fn read_bootstrap(path: &Path) -> Result<HostBootstrap, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let bootstrap: HostBootstrap = serde_json::from_slice(&bytes)
        .map_err(|e| format!("{} is not a bootstrap file: {e}", path.display()))?;
    if bootstrap.version != BOOTSTRAP_VERSION {
        return Err(format!(
            "{} is version {}; this bmacd reads version {BOOTSTRAP_VERSION}",
            path.display(),
            bootstrap.version
        ));
    }
    Ok(bootstrap)
}

/// Whether this host still has its setup ahead of it.
pub fn in_setup_mode(paths: &Paths) -> bool {
    paths.bootstrap.is_file() && !paths.done.exists()
}

fn identity_text(identity: &DiskIdentity) -> String {
    match identity {
        DiskIdentity::Wwn(wwn) => format!("WWN {wwn}"),
        DiskIdentity::Serial(serial) => format!("serial {serial}"),
    }
}

fn describe(disk: &InventoryDisk) -> String {
    let identity = disk
        .identity
        .as_ref()
        .map_or_else(|| "no serial or WWN".to_string(), identity_text);
    format!(
        "{} ({}, {identity})",
        disk.name,
        disk.model.as_deref().unwrap_or("unknown model")
    )
}

/// Whether `found` is the machine discovery saw. Fields either side
/// doesn't report aren't compared.
fn same_machine(seen: &SystemInfo, found: &SystemInfo) -> Result<(), String> {
    let pairs = [
        ("vendor", &seen.vendor, &found.vendor),
        ("product", &seen.product, &found.product),
        ("serial number", &seen.serial, &found.serial),
    ];
    for (what, seen, found) in pairs {
        if let (Some(seen), Some(found)) = (seen, found)
            && seen.trim() != found.trim()
        {
            return Err(format!(
                "This machine's {what} is {found}, but the host ISO was built for one whose \
                 {what} is {seen}."
            ));
        }
    }
    Ok(())
}

/// The layout with each member renamed to this host's own `/dev/disk/by-id`
/// name for the same disk, found by its serial number or WWN. udev on the
/// installed system can name a disk differently from udev on the discovery
/// OS, so the names in the bootstrap are never used to touch a disk.
pub fn resolve_layout(
    layout: &PoolLayout,
    approved: &Inventory,
    live: &[InventoryDisk],
) -> Result<PoolLayout, Vec<String>> {
    let mut problems = Vec::new();
    let mut resolved = layout.clone();
    for vdev in &mut resolved.vdevs {
        for member in &mut vdev.members {
            let Some(seen) = approved
                .disks
                .iter()
                .find(|d| d.by_id.iter().any(|n| n == member.as_str()))
            else {
                problems.push(format!(
                    "The layout names {}, which the discovery inventory doesn't list.",
                    member.as_str()
                ));
                continue;
            };
            let Some(identity) = &seen.identity else {
                problems.push(format!(
                    "{} reported neither a serial number nor a WWN at discovery, so it can't \
                     be found again safely.",
                    describe(seen)
                ));
                continue;
            };
            let found: Vec<&InventoryDisk> = live
                .iter()
                .filter(|d| d.identity.as_ref() == Some(identity))
                .collect();
            let disk = match found.as_slice() {
                [disk] => *disk,
                [] => {
                    problems.push(format!(
                        "{} isn't on this host: no disk reports its {}.",
                        describe(seen),
                        identity_text(identity)
                    ));
                    continue;
                }
                several => {
                    problems.push(format!(
                        "{} disks on this host report {}, so the right one can't be told apart.",
                        several.len(),
                        identity_text(identity)
                    ));
                    continue;
                }
            };
            if disk.size_bytes != seen.size_bytes {
                problems.push(format!(
                    "{} was {} bytes at discovery and is {} bytes now.",
                    describe(disk),
                    seen.size_bytes,
                    disk.size_bytes
                ));
                continue;
            }
            match disk.by_id.first().map(|n| n.parse::<ById>()) {
                Some(Ok(name)) => *member = name,
                _ => problems.push(format!(
                    "{} has no usable name under /dev/disk/by-id.",
                    describe(disk)
                )),
            }
        }
    }
    if problems.is_empty() {
        Ok(resolved)
    } else {
        Err(problems)
    }
}

/// Data disks in the layout that still carry ZFS labels, with the pools'
/// GUIDs. The boot vdev carries the installed `rpool`, so it is left out.
pub fn labelled_data_disks<'a>(
    layout: &PoolLayout,
    live: &'a [InventoryDisk],
) -> Vec<(&'a InventoryDisk, Vec<String>)> {
    layout
        .vdevs
        .iter()
        .filter(|v| !v.boot)
        .flat_map(|v| &v.members)
        .filter_map(|member| {
            let disk = live
                .iter()
                .find(|d| d.by_id.iter().any(|n| n == member.as_str()))?;
            let mut guids: Vec<String> = disk
                .zfs_labels
                .iter()
                .filter_map(|l| l.pool_guid.clone())
                .collect();
            guids.sort();
            guids.dedup();
            (!disk.zfs_labels.is_empty()).then_some((disk, guids))
        })
        .collect()
}

/// What the setup job needs from the host, so tests can stand in for it.
pub trait Host: Send + Sync {
    fn system(&self) -> SystemInfo;
    fn disks(&self) -> Vec<InventoryDisk>;
    fn clear_labels(&self, request: &ClearRequest) -> bmac_proto::inventory::ClearReport;
    fn now_ms(&self) -> i64;
}

pub struct RealHost;

impl Host for RealHost {
    fn system(&self) -> SystemInfo {
        bmac_inventory::system::read_system(&bmac_inventory::Root::system())
    }

    fn disks(&self) -> Vec<InventoryDisk> {
        bmac_inventory::disks::read_disks(&bmac_inventory::Root::system())
    }

    fn clear_labels(&self, request: &ClearRequest) -> bmac_proto::inventory::ClearReport {
        bmac_inventory::clear::clear_zfs_labels(
            &bmac_inventory::Root::system(),
            request,
            &bmac_inventory::clear::ZpoolLabelClear::installed(),
        )
    }

    fn now_ms(&self) -> i64 {
        crate::auth::system_clock()()
    }
}

async fn verify_identity(
    flow: &Workflow,
    host: &dyn Host,
    bootstrap: &HostBootstrap,
) -> Result<PoolLayout, Stop> {
    same_machine(&bootstrap.inventory.system, &host.system()).map_err(Stop::failed)?;
    let live = host.disks();
    let layout =
        resolve_layout(&bootstrap.layout, &bootstrap.inventory, &live).map_err(|problems| {
            Stop::Failed {
                message: "The disks don't match the layout the host ISO was built for.".into(),
                details: Some(problems.join("\n")),
            }
        })?;
    for (vdev, approved) in layout.vdevs.iter().zip(&bootstrap.layout.vdevs) {
        for (member, seen) in vdev.members.iter().zip(&approved.members) {
            if member != seen {
                flow.info(format!(
                    "{} is {} on this host.",
                    seen.as_str(),
                    member.as_str()
                ));
            }
        }
    }
    let labelled = labelled_data_disks(&layout, &live);
    if labelled.is_empty() {
        return Ok(layout);
    }
    let unnamed: Vec<String> = labelled
        .iter()
        .filter(|(d, guids)| d.serial.is_none() || guids.is_empty())
        .map(|(d, _)| describe(d))
        .collect();
    if !unnamed.is_empty() {
        return Err(Stop::Failed {
            message: "Disks in the layout carry ZFS labels that can't be cleared safely here; \
                      clear them by hand (zpool labelclear) and resume."
                .into(),
            details: Some(unnamed.join("\n")),
        });
    }
    let details: Vec<String> = labelled
        .iter()
        .map(|(d, guids)| format!("{}: old pool GUID {}", describe(d), guids.join(", ")))
        .collect();
    let confirmed = flow
        .confirm(
            "Clear old ZFS labels on the data disks?",
            "These disks in the layout still carry labels of an earlier pool. Building the \
             storage needs them cleared; only those labels are removed, after each disk is \
             checked by serial, WWN, and pool GUID.",
            Severity::Destructive,
            Some("clear"),
            details,
        )
        .await?;
    if !confirmed {
        return Err(Stop::failed(
            "The labels were kept, so the storage can't be built. Clear them, or resume and \
             agree to clearing them.",
        ));
    }
    let request = ClearRequest {
        disks: labelled
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
    let report = host.clear_labels(&request);
    for device in &report.cleared {
        flow.info(format!("Cleared the old label on {device}."));
    }
    let problems: Vec<String> = report
        .refused
        .iter()
        .chain(&report.survived)
        .chain(&report.errors)
        .cloned()
        .collect();
    if !problems.is_empty() {
        return Err(Stop::Failed {
            message: "The old labels weren't all cleared.".into(),
            details: Some(problems.join("\n")),
        });
    }
    Ok(layout)
}

/// Stores the bootstrap's cluster settings (first host) and this host's
/// facts. A join gets the cluster's settings when it joins instead.
fn store_settings(
    flow: &Workflow,
    settings: &crate::settings::Settings,
    bootstrap: &HostBootstrap,
    now_ms: i64,
) -> Result<(), Stop> {
    let BootstrapOrigin::NewCluster(seed) = &bootstrap.origin else {
        flow.info("A joining host gets the cluster settings from the cluster.");
        return Ok(());
    };
    let current = settings.cluster().map_err(|e| e.message)?;
    match &current {
        Some(stored) if stored.values == seed.cluster_settings => {}
        Some(stored) => {
            settings
                .save_cluster(
                    seed.cluster_settings.clone(),
                    Some(stored.revision),
                    "setup",
                    now_ms,
                )
                .map_err(|e| e.message)?;
        }
        None => {
            settings
                .save_cluster(seed.cluster_settings.clone(), None, "setup", now_ms)
                .map_err(|e| e.message)?;
        }
    }
    let facts = settings.host(bootstrap.host).map_err(|e| e.message)?;
    if facts.as_ref().map(|f| &f.values) != Some(&bootstrap.host_settings) {
        settings
            .save_host(
                bootstrap.host,
                bootstrap.host_settings.clone(),
                facts.map(|f| f.revision),
                "setup",
                now_ms,
            )
            .map_err(|e| e.message)?;
    }
    flow.info(format!(
        "Stored the cluster settings and {}'s facts.",
        bootstrap.host
    ));
    Ok(())
}

async fn cluster_phase(
    flow: &Workflow,
    settings: &crate::settings::Settings,
    bootstrap: &HostBootstrap,
    me: HostSlot,
    driver: Option<&Driver>,
) -> Result<serde_json::Value, Stop> {
    let cluster = bootstrap.cluster_settings();
    let private_ip = |host: HostSlot, values: &bmac_settings::Values| {
        bmac_settings::bridge_addresses(host, values, cluster)
            .map(|a| a.private.0)
            .map_err(|e| Stop::failed(format!("{host}'s settings: {}", e.message)))
    };
    let mine = private_ip(me, &bootstrap.host_settings)?;
    let mut pins = vec![(me, mine)];
    match &bootstrap.origin {
        BootstrapOrigin::NewCluster(_) => {
            for facts in settings.hosts().map_err(|e| e.message)? {
                if facts.host != me {
                    pins.push((facts.host, private_ip(facts.host, &facts.values)?));
                }
            }
        }
        BootstrapOrigin::Join(bundle) => {
            for host in &bundle.hosts {
                let ip = host.private_ip.parse().map_err(|_| {
                    Stop::failed(format!(
                        "The bundle's private address for {} isn't IPv4.",
                        host.host
                    ))
                })?;
                if host.host != me {
                    pins.push((host.host, ip));
                }
            }
        }
    }
    pins.sort();
    let runner = bmac_exec::RealRunner;
    let cluster = super::cluster::Cluster {
        flow,
        runner: &runner,
        root: PathBuf::from("/"),
        me,
        cluster_settings: cluster,
        private_ip: mine,
        pins,
        poll: std::time::Duration::from_secs(3),
    };
    match (&bootstrap.origin, driver) {
        (BootstrapOrigin::NewCluster(_), _) => cluster.create().await,
        (BootstrapOrigin::Join(bundle), Some(driver)) => {
            cluster
                .join(&super::cluster::JoinVia {
                    host: driver.host,
                    address: driver.address,
                    fingerprint: driver.fingerprint.clone(),
                    cluster_name: bundle.cluster_name.clone(),
                })
                .await
        }
        (BootstrapOrigin::Join(_), None) => Err(Stop::failed(
            "A joining host is set up from the cluster; approve its join in a member's \
             Dashboard.",
        )),
    }
}

/// Removes `key` from the `authorized_keys` file at `path`. Proxmox makes
/// root's file a link to the cluster's, which is changed in place.
pub fn remove_authorized_key(path: &Path, key: &str) -> Result<bool, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let kept = without_key(&text, key);
    if kept == text {
        return Ok(false);
    }
    let real = std::fs::canonicalize(path)
        .map_err(|e| format!("cannot resolve {}: {e}", path.display()))?;
    let written = if real.starts_with("/etc/pve") {
        std::fs::write(&real, kept.as_bytes())
    } else {
        super::write_atomic(&real, kept.as_bytes()).and_then(|()| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600))
        })
    };
    written.map_err(|e| format!("cannot write {}: {e}", real.display()))?;
    Ok(true)
}

/// Removes the key the cluster set this host up with, now that the
/// cluster's own SSH trust covers it. Joining merged root's keys into the
/// cluster's, so it comes out of both.
fn retire_setup_key(flow: &Workflow, bootstrap: &HostBootstrap) -> Result<(), Stop> {
    let BootstrapOrigin::Join(bundle) = &bootstrap.origin else {
        flow.info("The first host was set up with no setup key.");
        return Ok(());
    };
    for path in [AUTHORIZED_KEYS, CLUSTER_AUTHORIZED_KEYS] {
        if remove_authorized_key(Path::new(path), &bundle.setup_ssh_public_key)
            .map_err(Stop::failed)?
        {
            flow.info(format!("Removed the setup key from {path}."));
        }
    }
    Ok(())
}

async fn phases(
    flow: &Workflow,
    host: &dyn Host,
    paths: &Paths,
    settings: &crate::settings::Settings,
    me: HostSlot,
    driver: Option<&Driver>,
) -> Result<(), Stop> {
    let bootstrap = read_bootstrap(&paths.bootstrap)?;
    if bootstrap.host != me {
        return Err(Stop::failed(format!(
            "The bootstrap file is for {}, but this host is {me}.",
            bootstrap.host
        )));
    }
    match (&bootstrap.origin, driver) {
        (BootstrapOrigin::Join(_), None) => {
            return Err(Stop::failed(
                "A joining host is set up from the cluster; approve its join in a member's \
                 Dashboard.",
            ));
        }
        (BootstrapOrigin::NewCluster(_), Some(_)) => {
            return Err(Stop::failed(
                "The first host of a cluster sets itself up; it isn't driven from another.",
            ));
        }
        (_, Some(driver)) if driver.host == me => {
            return Err(Stop::failed(format!("{me} can't join through itself.")));
        }
        (_, Some(driver)) => {
            let message = format!("Being set up from {}.", driver.host);
            if let Some(mut state) = crate::joiner::read_state(&paths.join_state) {
                state.message = Some(message);
                crate::joiner::write_state(&paths.join_state, &state);
            }
        }
        _ => {}
    }
    let mut done: Checkpoints = flow.checkpoints();
    for (id, label) in PHASES {
        if done.is_done(id) {
            flow.phase(id, label, PhaseStatus::Complete);
        } else {
            flow.phase(id, label, PhaseStatus::Pending);
        }
    }
    let mut layout: Option<PoolLayout> = done
        .get("identity")
        .and_then(|d| serde_json::from_value(d["layout"].clone()).ok());
    for (id, label) in PHASES {
        if done.is_done(id) {
            continue;
        }
        flow.check()?;
        flow.phase_with(id, label, PhaseStatus::Running, *id != "storage");
        let data = match *id {
            "identity" => {
                let resolved = verify_identity(flow, host, &bootstrap).await?;
                let data = json!({ "layout": resolved });
                layout = Some(resolved);
                data
            }
            "settings" => {
                store_settings(flow, settings, &bootstrap, host.now_ms())?;
                if let BootstrapOrigin::NewCluster(seed) = &bootstrap.origin {
                    crate::joins::JoinStore::new(
                        bmac_pmxcfs::Pmxcfs::system(),
                        crate::joins::JOIN_DIR,
                        &paths.cluster_record,
                    )
                    .record_cluster(&seed.cluster_uuid, &seed.used_tailscale_key_sha256)
                    .map_err(|e| e.message)?;
                }
                json!({})
            }
            "storage" | "drills" | "headers" => {
                let layout = layout.as_ref().ok_or_else(|| {
                    Stop::failed("the identity checkpoint holds no layout; start the job again")
                })?;
                let runner = bmac_exec::RealRunner;
                let storage = super::storage::Storage {
                    flow,
                    host,
                    runner: &runner,
                    root: PathBuf::from("/"),
                    layout,
                    define_storages: matches!(bootstrap.origin, BootstrapOrigin::NewCluster(_)),
                    poll: std::time::Duration::from_secs(15),
                };
                match *id {
                    "storage" => storage.run_phase().await?,
                    "drills" => storage.drills(&super::boot_id()?).await?,
                    _ => storage.headers().await?,
                }
            }
            "network" => {
                let cluster = match &bootstrap.origin {
                    BootstrapOrigin::NewCluster(seed) => &seed.cluster_settings,
                    BootstrapOrigin::Join(bundle) => &bundle.cluster_settings,
                };
                let plan = super::network::NetworkPlan::from_settings(
                    me,
                    &bootstrap.host_settings,
                    cluster,
                )?;
                let runner = bmac_exec::RealRunner;
                super::network::Network {
                    flow,
                    runner: &runner,
                    root: PathBuf::from("/"),
                    plan: &plan,
                    link_wait: bootstrap.inventory.link_wait.unwrap_or_default(),
                    links: std::sync::Arc::new(bmac_inventory::nics::IpLink),
                }
                .run_phase()
                .await?
            }
            "cluster" => cluster_phase(flow, settings, &bootstrap, me, driver).await?,
            "retire" => {
                retire_setup_key(flow, &bootstrap)?;
                json!({})
            }
            "orchestration" => {
                let runner = bmac_exec::RealRunner;
                let render = || settings.render_for_scripts();
                super::orchestration::Orchestration {
                    flow,
                    runner: &runner,
                    root: PathBuf::from("/"),
                    me,
                    render: &render,
                }
                .run_phase()
                .await?
            }
            "verify" => {
                let layout = layout.as_ref().ok_or_else(|| {
                    Stop::failed("the identity checkpoint holds no layout; start the job again")
                })?;
                let encryption = if layout.encryption == bmac_storage::Encryption::None {
                    "none"
                } else {
                    "luks"
                };
                let runner = bmac_exec::RealRunner;
                let render = || settings.render_for_scripts();
                super::orchestration::HostScript {
                    flow,
                    runner: &runner,
                    me,
                    render: &render,
                }
                .run(
                    super::orchestration::VERIFY_SCRIPT,
                    &[encryption],
                    "final verification",
                )
                .await?;
                json!({ "encryption": encryption })
            }
            other => {
                flow.phase(id, label, PhaseStatus::Failed);
                return Err(Stop::failed(format!(
                    "The {other} phase isn't built yet (IMPLEMENTATION_PLAN.md, M7.3); \
                     resume this job once a bmacd that has it is installed."
                )));
            }
        };
        done.record(id, data)?;
        flow.phase(id, label, PhaseStatus::Complete);
    }
    std::fs::write(&paths.done, b"")
        .map_err(|e| Stop::failed(format!("cannot write {}: {e}", paths.done.display())))?;
    flow.result(json!({ "host": me.to_string() }));
    Ok(())
}

pub async fn run(flow: &Workflow, args: &[String]) -> ExitCode {
    flow.begin(
        OPERATION,
        "Set up this host",
        "Finishes the host's setup after the Proxmox install.",
    );
    let outcome = async {
        let driver = parse_args(args)?;
        let _lock = match &driver {
            Some(_) => Some(lock_work(flow).await?),
            None => None,
        };
        let me = crate::engine::listen::node_name()
            .parse::<HostSlot>()
            .map_err(|_| {
                Stop::failed(format!(
                    "{} is not a BMAC host name",
                    crate::engine::listen::node_name()
                ))
            })?;
        phases(
            flow,
            &RealHost,
            &Paths::system(),
            &crate::settings::Settings::system(),
            me,
            driver.as_ref(),
        )
        .await
    }
    .await;
    flow.finish(&outcome)
}

/// How long a driven run waits for one that lost its connection to stop.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Holds the work directory for this run. A run whose driver's connection
/// dropped stops at its next safe point; the driver's next run waits here
/// for it.
async fn lock_work(flow: &Workflow) -> Result<std::fs::File, Stop> {
    use rustix::fs::{FlockOperation, flock};
    let path = flow.work_dir().join("lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .map_err(|e| Stop::failed(format!("cannot open {}: {e}", path.display())))?;
    let started = std::time::Instant::now();
    let mut told = false;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => return Ok(file),
            Err(rustix::io::Errno::WOULDBLOCK) if started.elapsed() < LOCK_WAIT => {
                if !told {
                    flow.info("Waiting for the previous run of this setup to stop.");
                    told = true;
                }
                flow.check()?;
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => {
                return Err(Stop::failed(format!(
                    "Another run of this setup still holds {}: {e}",
                    path.display()
                )));
            }
        }
    }
}

#[cfg(test)]
#[path = "setup_tests.rs"]
mod tests;
