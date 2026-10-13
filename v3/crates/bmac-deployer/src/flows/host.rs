// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Installing a Proxmox host (`DEPLOYMENT.md`, "Proxmox host ISO" and
//! "Handoff"): the cluster and host settings (or a join bundle), the
//! hardware from a discovery session or a file, the pool layout, the root
//! password and Tailscale key, the ISO, the boot steps, the wait for the
//! host, the certificate check, this workstation's SSH, and the link to
//! the host's Dashboard. Setup continues there; the Deployer does nothing
//! more for the host.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use bmac_core::job::{FieldType, PhaseStatus, RequestForm, Severity};
use bmac_core::{HostSlot, PoolName};
use bmac_proto::bootstrap::{JoinBundle, SettingValues};
use bmac_proto::deployer::{BuiltIso, IsoKind};
use bmac_proto::inventory::{Inventory, InventoryDisk, InventoryNic, NicLink};
use bmac_storage::{ById, Encryption, PoolLayout, Vdev, VdevClass, VdevType};
use serde_json::json;

use super::discovery::{self, Preset};
use super::{Env, FieldExt, download_progress, field, list, note, option, stopped, text};
use crate::boot::{self, BootMethod, ErasedDisk, InstallTarget};
use crate::handoff::{self, CertCheck, NextStep};
use crate::hostiso::{self, HostIsoRequest, Origin};
use crate::tailnet;
use crate::tasks::{Stop, TaskContext, TaskResult, Values};
use crate::workstation_ssh::{self, HostPin, SshSetup, SshSetupError};

type FieldErrors = (BTreeMap<String, String>, Option<String>);

fn message(text: impl Into<String>) -> FieldErrors {
    (BTreeMap::new(), Some(text.into()))
}

fn api_errors(error: bmac_proto::api::ApiError) -> FieldErrors {
    let message = (error.field_errors.is_empty()).then_some(error.message);
    (error.field_errors, message)
}

fn read_text(path: &str) -> Result<String, String> {
    if !path.starts_with('/') {
        return Err("Enter the file's full path.".into());
    }
    let meta = std::fs::metadata(path).map_err(|e| format!("{path}: {e}"))?;
    if !meta.is_file() || meta.len() > 1 << 20 {
        return Err(format!("{path} isn't a settings file."));
    }
    std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))
}

/// How the host comes into a cluster, with the settings that go with it.
struct Start {
    origin: Origin,
    cluster: SettingValues,
}

fn load_origin(values: &Values, now_ms: i64) -> Result<Start, FieldErrors> {
    let path = text(values, "file").ok_or_else(|| message("Choose the file."))?;
    let content = read_text(&path).map_err(|e| (BTreeMap::from([("file".into(), e)]), None))?;
    if text(values, "origin").as_deref() == Some("join") {
        let bundle: JoinBundle = serde_json::from_str(&content).map_err(|e| {
            (
                BTreeMap::from([("file".into(), format!("This isn't a join bundle: {e}"))]),
                None,
            )
        })?;
        if bundle.expires_ms <= now_ms {
            return Err(message(
                "The join bundle has expired. Issue a new one from the Dashboard.",
            ));
        }
        let cluster = bundle.cluster_settings.clone();
        Ok(Start {
            origin: Origin::Join(Box::new(bundle)),
            cluster,
        })
    } else {
        let cluster = bmac_settings::parse_conf(&content)
            .map_err(|e| (BTreeMap::from([("file".into(), e)]), None))?;
        bmac_settings::validate_cluster(&cluster).map_err(|e| message(e.message))?;
        Ok(Start {
            origin: Origin::NewCluster {
                cluster_settings: cluster.clone(),
            },
            cluster,
        })
    }
}

const HOST_FIELDS: [(&str, FieldType, &str, bool); 3] = [
    (
        "MAX_PROD_VM_COUNT_ON_THIS_HOST",
        FieldType::Integer,
        "Production VMs this host may run",
        true,
    ),
    (
        "MAX_STAGING_VM_COUNT_ON_THIS_HOST",
        FieldType::Integer,
        "Staging VMs this host may run",
        true,
    ),
    ("IDRAC_IP", FieldType::IpAddress, "iDRAC address", false),
];

fn host_values(values: &Values) -> SettingValues {
    HOST_FIELDS
        .iter()
        .filter_map(|(key, ..)| text(values, key).map(|v| (key.to_string(), v)))
        .collect()
}

fn host_form(cluster: &SettingValues) -> RequestForm {
    let hosts: u8 = cluster
        .get("MAX_MOX_HOSTS")
        .and_then(|v| v.parse().ok())
        .unwrap_or(HostSlot::MAX);
    let mut fields = vec![
        field("slot", FieldType::Select, "Host")
            .required()
            .default_value(json!("mox1"))
            .choices(
                HostSlot::all()
                    .take(hosts as usize)
                    .map(|h| option(&h.to_string(), &h.to_string(), None))
                    .collect(),
            ),
    ];
    for (key, kind, label, required) in HOST_FIELDS {
        let mut f = field(key, kind, label);
        if required {
            f = f.required();
        }
        f = match key {
            "MAX_PROD_VM_COUNT_ON_THIS_HOST" | "MAX_STAGING_VM_COUNT_ON_THIS_HOST" => {
                f.default_value(json!(1))
            }
            "IDRAC_IP" => f.help("Optional. Only used to link to the iDRAC in the steps."),
            _ => f,
        };
        fields.push(f);
    }
    RequestForm::InputGroup {
        title: "Host settings".into(),
        description: Some(
            "They become the host's settings in the cluster. Its network ports and addresses \
             are chosen once the hardware has been read."
                .into(),
        ),
        context: None,
        layout: None,
        fields,
    }
}

/// Stand-ins for the settings chosen later, so the first form's values can
/// be checked on their own.
fn placeholders(settings: &mut SettingValues) {
    for (key, value) in [
        ("PROXMOX_IP", "192.0.2.10"),
        ("PROXMOX_PREFIX", "24"),
        ("PROXMOX_GATEWAY", "192.0.2.1"),
        ("PROXMOX_PUBLIC_MAC", "02:00:00:00:00:01"),
        ("PROXMOX_SECONDARY_MAC", "02:00:00:00:00:02"),
        ("NVME_MIRROR_0_SERIAL_1", "BOOT-DISK-A"),
        ("NVME_MIRROR_0_SERIAL_2", "BOOT-DISK-B"),
    ] {
        settings
            .entry(key.to_string())
            .or_insert_with(|| value.to_string());
    }
}

fn nic_label(nic: &InventoryNic) -> String {
    let link = match &nic.link {
        NicLink::Up {
            speed_mbps: Some(speed),
            ..
        } if speed % 1000 == 0 => format!("link up at {} Gb/s", speed / 1000),
        NicLink::Up {
            speed_mbps: Some(speed),
            ..
        } => format!("link up at {speed} Mb/s"),
        NicLink::Up { .. } => "link up".into(),
        NicLink::NoCarrier { waited_seconds } => {
            format!("no link after {waited_seconds} seconds")
        }
        NicLink::Error { .. } => "couldn't be read".into(),
    };
    format!(
        "{} ({}{}), {link}",
        nic.name,
        nic.mac,
        nic.driver
            .as_deref()
            .map(|d| format!(", {d}"))
            .unwrap_or_default()
    )
}

/// The inventory's MAC for a setting's, matched without regard to case.
fn known_mac(inventory: &Inventory, mac: Option<&String>) -> Option<String> {
    let mac = mac?;
    inventory
        .nics
        .iter()
        .find(|n| n.mac.eq_ignore_ascii_case(mac))
        .map(|n| n.mac.clone())
}

pub(crate) fn network_form(
    inventory: &Inventory,
    settings: &SettingValues,
    seen: Option<&crate::discovery::SeenNetwork>,
    host: HostSlot,
    cluster: &SettingValues,
) -> RequestForm {
    let choices: Vec<_> = inventory
        .nics
        .iter()
        .map(|n| option(&n.mac, &nic_label(n), None))
        .collect();
    let seen = seen.filter(|s| known_mac(inventory, Some(&s.mac)).is_some());
    let public = known_mac(inventory, settings.get("PROXMOX_PUBLIC_MAC"))
        .or_else(|| seen.map(|s| s.mac.clone()));
    let private = known_mac(inventory, settings.get("PROXMOX_SECONDARY_MAC")).or_else(|| {
        let others: Vec<_> = inventory
            .nics
            .iter()
            .filter(|n| Some(&n.mac) != public.as_ref() && matches!(n.link, NicLink::Up { .. }))
            .collect();
        match others.as_slice() {
            [one] => Some(one.mac.clone()),
            _ => None,
        }
    });
    let given = |key: &str| settings.get(key).cloned();
    let address =
        given("PROXMOX_IP").or_else(|| seen.and_then(|s| s.address.map(|a| a.to_string())));
    let prefix =
        given("PROXMOX_PREFIX").or_else(|| seen.and_then(|s| s.prefix.map(|p| p.to_string())));
    let gateway =
        given("PROXMOX_GATEWAY").or_else(|| seen.and_then(|s| s.gateway.map(|g| g.to_string())));
    let or_null = |v: Option<String>| v.map_or(serde_json::Value::Null, |v| json!(v));
    let given_private = given("PROXMOX_SECONDARY_IP");
    let private_address = given_private
        .as_deref()
        .and_then(|v| v.split('/').next())
        .map(str::to_string)
        .or_else(|| bmac_settings::default_private_ip(host, cluster).ok());
    let subnet = bmac_settings::private_subnet(cluster).ok();
    let subnet_len = subnet.map_or(24, |s| s.len);
    let private_prefix = given_private
        .as_deref()
        .and_then(|v| v.split_once('/'))
        .map_or_else(|| subnet_len.to_string(), |(_, p)| p.to_string());
    let private_gateway = given("PROXMOX_SECONDARY_GATEWAY")
        .or_else(|| cluster.get("DATACENTER_PRIVATE_VLAN_GATEWAY").cloned());
    let route = given("PROXMOX_SECONDARY_GATEWAY_ROUTE").as_deref() == Some("yes");
    let (first, last) = (
        cluster.get("MOX_IP_START").cloned().unwrap_or_default(),
        cluster.get("MOX_IP_END").cloned().unwrap_or_default(),
    );
    let mut description = "Choose the port that reaches the internet and the static address your \
         provider assigned to this server, and the port cabled to the private VLAN between the \
         hosts."
        .to_string();
    match seen {
        Some(s) if s.dhcp && given("PROXMOX_IP").is_none() => description.push_str(
            " The address below is the one DHCP gave the discovery OS. A Proxmox host keeps a \
             static address: check that it is the one your provider assigned.",
        ),
        Some(_) if given("PROXMOX_IP").is_none() => {
            description.push_str(" The address below is the one the discovery session used.")
        }
        _ => {}
    }
    RequestForm::InputGroup {
        title: "Network ports".into(),
        description: Some(description),
        context: None,
        layout: None,
        fields: vec![
            field("public_nic", FieldType::Select, "Public port")
                .required()
                .default_value(or_null(public))
                .choices(choices.clone()),
            field("PROXMOX_IP", FieldType::IpAddress, "Public address")
                .required()
                .default_value(or_null(address)),
            field("PROXMOX_PREFIX", FieldType::Integer, "Prefix length")
                .required()
                .default_value(or_null(prefix)),
            field("PROXMOX_GATEWAY", FieldType::IpAddress, "Gateway")
                .required()
                .default_value(or_null(gateway)),
            field("private_nic", FieldType::Select, "Private VLAN port")
                .required()
                .default_value(or_null(private))
                .help("The port cabled to the private VLAN between the hosts.")
                .choices(choices),
            field("private_address", FieldType::IpAddress, "Private address")
                .required()
                .default_value(or_null(private_address))
                .help(&format!(
                    "One of the cluster's host addresses, {first} to {last}, not used by \
                     another host."
                )),
            field("private_prefix", FieldType::Integer, "Private prefix length")
                .required()
                .default_value(json!(private_prefix))
                .help(&match subnet {
                    Some(subnet) => format!(
                        "{subnet_len}, or shorter so the network still holds the cluster's \
                         private subnet, {subnet}."
                    ),
                    None => "The cluster's private subnet prefix, or shorter.".to_string(),
                }),
            field("PROXMOX_SECONDARY_GATEWAY", FieldType::IpAddress, "Private VLAN gateway")
                .default_value(or_null(private_gateway))
                .help("Optional. Recorded for the host; no traffic uses it unless the next box is ticked."),
            field(
                "route_private_gateway",
                FieldType::Boolean,
                "Also route to the internet through the private VLAN gateway",
            )
            .default_value(json!(route))
            .help(
                "Tick this only if the VLAN's gateway gives internet access. Host setup then \
                 adds a second default route through it, with a higher metric than the public \
                 gateway's, so the public port stays the preferred way out.",
            ),
        ],
    }
}

/// The host settings with the chosen ports and public address.
fn network_from(
    values: &Values,
    inventory: &Inventory,
    host: HostSlot,
    settings: &SettingValues,
    cluster: &SettingValues,
    taken: &[(HostSlot, String)],
) -> Result<SettingValues, FieldErrors> {
    let one = |id: &str, text: &str| (BTreeMap::from([(id.to_string(), text.to_string())]), None);
    let port = |id: &str, what: &str| {
        let mac = text(values, id).ok_or_else(|| one(id, &format!("Choose the {what}.")))?;
        known_mac(inventory, Some(&mac))
            .ok_or_else(|| one(id, "That port isn't in the hardware inventory."))
    };
    let public = port("public_nic", "public port")?;
    let private = port("private_nic", "private VLAN port")?;
    if public == private {
        return Err(one(
            "private_nic",
            "The private VLAN needs a port of its own, not the public one.",
        ));
    }
    let mut next = settings.clone();
    for key in ["PROXMOX_IP", "PROXMOX_PREFIX", "PROXMOX_GATEWAY"] {
        match text(values, key) {
            Some(value) => next.insert(key.to_string(), value),
            None => next.remove(key),
        };
    }
    next.insert("PROXMOX_PUBLIC_MAC".into(), public.to_ascii_uppercase());
    next.insert("PROXMOX_SECONDARY_MAC".into(), private.to_ascii_uppercase());
    let address = text(values, "private_address")
        .ok_or_else(|| one("private_address", "Enter the private address."))?;
    let prefix = text(values, "private_prefix")
        .ok_or_else(|| one("private_prefix", "Enter the prefix length."))?;
    if let Some((other, _)) = taken.iter().find(|(h, ip)| *h != host && *ip == address) {
        return Err(one(
            "private_address",
            &format!("{address} is already {other}'s private address."),
        ));
    }
    next.insert("PROXMOX_SECONDARY_IP".into(), format!("{address}/{prefix}"));
    let route = text(values, "route_private_gateway").as_deref() == Some("true");
    match text(values, "PROXMOX_SECONDARY_GATEWAY") {
        Some(gateway) => {
            next.insert("PROXMOX_SECONDARY_GATEWAY".into(), gateway);
            next.insert(
                "PROXMOX_SECONDARY_GATEWAY_ROUTE".into(),
                if route { "yes" } else { "no" }.into(),
            );
        }
        None if route => {
            return Err(one(
                "PROXMOX_SECONDARY_GATEWAY",
                "Enter the gateway to route through it.",
            ));
        }
        None => {
            next.remove("PROXMOX_SECONDARY_GATEWAY");
            next.remove("PROXMOX_SECONDARY_GATEWAY_ROUTE");
        }
    }
    let mut checked = next.clone();
    placeholders(&mut checked);
    bmac_settings::validate_host(host, &checked, cluster).map_err(|e| {
        let (mut errors, message) = api_errors(e);
        for (key, id) in [
            ("PROXMOX_PUBLIC_MAC", "public_nic"),
            ("PROXMOX_SECONDARY_MAC", "private_nic"),
            ("PROXMOX_SECONDARY_IP", "private_address"),
            ("PROXMOX_SECONDARY_GATEWAY_ROUTE", "route_private_gateway"),
        ] {
            if let Some(error) = errors.remove(key) {
                errors.insert(id.to_string(), error);
            }
        }
        (errors, message)
    })?;
    Ok(next)
}

fn slot_of(values: &Values) -> Result<HostSlot, FieldErrors> {
    text(values, "slot")
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| {
            (
                BTreeMap::from([("slot".into(), "Choose a host.".into())]),
                None,
            )
        })
}

/// The disk's name for pool layouts.
fn by_id(disk: &InventoryDisk) -> Option<&str> {
    disk.by_id.first().map(String::as_str)
}

fn describe(disk: &InventoryDisk) -> String {
    format!(
        "{} ({}, {}{})",
        disk.name,
        disk.model.as_deref().unwrap_or("unknown model"),
        bmac_core::Bytes(disk.size_bytes),
        disk.serial
            .as_deref()
            .map(|s| format!(", serial {s}"))
            .unwrap_or_default()
    )
}

fn usable(inventory: &Inventory) -> Vec<&InventoryDisk> {
    inventory
        .disks
        .iter()
        .filter(|d| d.problems.is_empty() && by_id(d).is_some() && !d.removable)
        .collect()
}

fn layout_form(inventory: &Inventory) -> RequestForm {
    let choices: Vec<_> = usable(inventory)
        .into_iter()
        .map(|d| option(by_id(d).unwrap_or_default(), &describe(d), None))
        .collect();
    RequestForm::InputGroup {
        title: "Pool layout".into(),
        description: Some(
            "The installer puts Proxmox VE on the boot disks; host setup adds the other vdevs to \
             the same pool afterwards. Disks are named by their identity, never by device name."
                .into(),
        ),
        context: None,
        layout: None,
        fields: vec![
            {
                let mut boot = field("boot", FieldType::Multiselect, "Boot disks")
                    .required()
                    .help(
                        "One or two disks. Two make a mirror; one has no redundancy. The \
                         Proxmox installer can be pointed at no more than two disks, so a boot \
                         vdev of three or more (such as raidz) isn't possible; put the other \
                         disks in data vdevs.",
                    )
                    .choices(choices.clone());
                boot.min_selected = Some(1);
                boot.max_selected = Some(MAX_BOOT_DISKS as u32);
                boot
            },
            field("data_kind", FieldType::Select, "Other data disks")
                .required()
                .default_value(json!("none"))
                .choices(vec![
                    option("none", "None", None),
                    option("mirror", "Mirrored pairs, in the order chosen", None),
                    option("raidz1", "One raidz1 vdev", None),
                    option("raidz2", "One raidz2 vdev", None),
                    option("raidz3", "One raidz3 vdev", None),
                ]),
            field("data", FieldType::Multiselect, "Data disks").choices(choices),
            field("encryption", FieldType::Select, "Encryption")
                .required()
                .default_value(json!("none"))
                .choices(vec![
                    option("none", "None", None),
                    option(
                        "luks2",
                        "LUKS2",
                        Some(
                            "Setup encrypts every disk and asks for the passphrase at the host's \
                             console; after each reboot it must be entered there again.",
                        ),
                    ),
                ]),
        ],
    }
}

/// The installer selects boot disks by udev property, each property once,
/// and only two properties agree between the discovery OS and the installer
/// (`hostiso::answer`).
pub(crate) const MAX_BOOT_DISKS: usize = 2;

/// Whether the installer can be told to use exactly these boot disks.
fn check_boot_disks(layout: &PoolLayout, inventory: &Inventory) -> Result<(), FieldErrors> {
    let boot: Vec<&InventoryDisk> = layout
        .vdevs
        .iter()
        .filter(|v| v.boot)
        .flat_map(|v| &v.members)
        .filter_map(|m| {
            inventory
                .disks
                .iter()
                .find(|d| d.by_id.iter().any(|n| n == m.as_str()))
        })
        .collect();
    hostiso::answer::disk_filters(&boot, &inventory.disks)
        .map(|_| ())
        .map_err(|e| (BTreeMap::from([("boot".to_string(), e)]), None))
}

pub(crate) fn layout_from(values: &Values) -> Result<PoolLayout, FieldErrors> {
    let field_error =
        |id: &str, text: &str| (BTreeMap::from([(id.to_string(), text.to_string())]), None);
    let ids = |id: &str| -> Result<Vec<ById>, FieldErrors> {
        list(values, id)
            .iter()
            .map(|name| {
                name.parse::<ById>()
                    .map_err(|e| field_error(id, &e.to_string()))
            })
            .collect()
    };
    let boot = ids("boot")?;
    let data = ids("data")?;
    if boot.is_empty() {
        return Err(field_error("boot", "Choose at least one boot disk."));
    }
    if boot.len() > MAX_BOOT_DISKS {
        return Err(field_error(
            "boot",
            "Choose one or two boot disks. The Proxmox installer can be pointed at no more \
             than two; put the other disks in data vdevs.",
        ));
    }
    if data.iter().any(|d| boot.contains(d)) {
        return Err(field_error(
            "data",
            "A boot disk can't also be a data disk.",
        ));
    }
    let boot_kind = if boot.len() == 1 {
        VdevType::Single
    } else {
        VdevType::Mirror
    };
    let mut vdevs = vec![Vdev::new(VdevClass::Data, boot_kind, boot).boot()];
    match text(values, "data_kind").as_deref().unwrap_or("none") {
        "none" if data.is_empty() => {}
        "none" => {
            return Err(field_error(
                "data_kind",
                "Choose how the data disks are used.",
            ));
        }
        "mirror" => {
            if data.len() % 2 != 0 {
                return Err(field_error(
                    "data",
                    "Mirrored pairs need an even number of disks.",
                ));
            }
            for pair in data.chunks(2) {
                vdevs.push(Vdev::new(VdevClass::Data, VdevType::Mirror, pair.to_vec()));
            }
        }
        kind => {
            let kind = match kind {
                "raidz1" => VdevType::Raidz1,
                "raidz2" => VdevType::Raidz2,
                _ => VdevType::Raidz3,
            };
            vdevs.push(Vdev::new(VdevClass::Data, kind, data));
        }
    }
    let encryption = match text(values, "encryption").as_deref() {
        Some("luks2") => Encryption::Luks2,
        _ => Encryption::None,
    };
    let pool: PoolName = "rpool".parse().map_err(|_| message("bad pool name"))?;
    let layout = PoolLayout::new(pool, encryption, vdevs);
    layout.validate().map_err(|e| message(e.to_string()))?;
    Ok(layout)
}

fn secrets_form(host: HostSlot) -> RequestForm {
    RequestForm::InputGroup {
        title: "Root password and Tailscale key".into(),
        description: Some(
            "Only the password's hash goes into the ISO. The key is used once, when the host \
             first joins the tailnet."
                .into(),
        ),
        context: None,
        layout: None,
        fields: vec![
            field(
                "root_password",
                FieldType::Password,
                "Proxmox root password",
            )
            .required(),
            field(
                "root_password_again",
                FieldType::Password,
                "The password again",
            )
            .required(),
            field("tailscale_key", FieldType::Password, "Tailscale auth key")
                .required()
                .help(&format!(
                    "A fresh single-use key tagged {}, not ephemeral; {host} keeps it.",
                    tailnet::HOST_TAG.to_string()
                )),
        ],
    }
}

fn check_secrets(values: &Values) -> Result<(), FieldErrors> {
    let password = text(values, "root_password").unwrap_or_default();
    if password.chars().count() < hostiso::answer::MIN_ROOT_PASSWORD {
        return Err((
            BTreeMap::from([(
                "root_password".into(),
                format!(
                    "Use at least {} characters.",
                    hostiso::answer::MIN_ROOT_PASSWORD
                ),
            )]),
            None,
        ));
    }
    if text(values, "root_password_again").as_deref() != Some(password.as_str()) {
        return Err((
            BTreeMap::from([("root_password_again".into(), "The passwords differ.".into())]),
            None,
        ));
    }
    if !text(values, "tailscale_key").is_some_and(|k| k.starts_with("tskey-auth-")) {
        return Err((
            BTreeMap::from([(
                "tailscale_key".into(),
                "Enter an auth key (tskey-auth-...).".into(),
            )]),
            None,
        ));
    }
    Ok(())
}

/// `host` and `port` of an `https://host:port/` URL.
fn host_port(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("https://")?;
    let authority = rest.split('/').next()?;
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host.to_string(), port.parse().ok()?)),
        None => Some((authority.to_string(), 443)),
    }
}

fn card_text(card: &hostiso::HostCard) -> String {
    format!(
        "BMAC host card\n\nHost: {}\nCluster: {}\nTailscale name: {}\nPublic address: {}\n\
         Dashboard: {}\nHTTPS certificate SHA-256: {}\nSSH host key: {}\nSSH fingerprint: {}\n\n\
         Keep this somewhere other than the machine it describes. Before logging in, check that \
         the browser shows the same certificate fingerprint.\n",
        card.host,
        card.cluster_name,
        card.tailscale_name,
        card.public_ip,
        card.dashboard_url,
        card.https_fingerprint,
        card.ssh_host_key,
        card.ssh_fingerprint,
    )
}

/// Checks an existing host's Dashboard for a joining host's instructions.
async fn existing_dashboard(ctx: &TaskContext, bundle: &JoinBundle) -> Result<NextStep, Stop> {
    let first = bundle.hosts.first().ok_or_else(|| {
        Stop::failed("The join bundle names no existing host to approve the new one.")
    })?;
    let (host, port) = host_port(&first.dashboard_url)
        .ok_or_else(|| Stop::failed("The join bundle's Dashboard URL isn't valid."))?;
    match handoff::check_certificate(
        &host,
        port,
        &first.https_fingerprint,
        Duration::from_secs(15),
    )
    .await
    {
        CertCheck::Mismatch { seen } => Err(Stop::Failed {
            message: format!(
                "{} presents a different certificate than the join bundle names, so it isn't \
                 linked. Check it before approving the new host there.",
                first.dashboard_url
            ),
            details: Some(format!("expected {}, seen {seen}", first.https_fingerprint)),
        }),
        check => {
            if let CertCheck::Waiting(reason) = check {
                ctx.warning(format!(
                    "{} didn't answer ({reason}); check its fingerprint in the browser.",
                    first.dashboard_url
                ));
            }
            Ok(NextStep::Join {
                existing_url: first.dashboard_url.clone(),
                existing_fingerprint: first.https_fingerprint.clone(),
            })
        }
    }
}

pub async fn run(ctx: &TaskContext, env: &Env) -> TaskResult {
    let tailscale = env.tool(&env.tools.tailscale, "tailscale")?;
    let ssh = env.tool(&env.tools.ssh, "ssh")?;
    let dpkg_deb = env.tool(&env.tools.dpkg_deb, "dpkg-deb")?;
    let xorriso = env.tool(&env.tools.xorriso, "xorriso")?;
    let packages_dir = env.packages_dir.clone().ok_or_else(|| {
        Stop::failed(
            "This Deployer has no BMAC packages to put on the host ISO. Reinstall the \
             Deployer's package.",
        )
    })?;

    ctx.phase(
        "settings",
        "Cluster and host settings",
        PhaseStatus::Running,
    );
    let now = env.now();
    let values = ctx
        .values(
            RequestForm::InputGroup {
                title: "New cluster or another host?".into(),
                description: None,
                context: None,
                layout: None,
                fields: vec![
                    field("origin", FieldType::Select, "This host")
                        .required()
                        .default_value(json!("new"))
                        .choices(vec![
                            option(
                                "new",
                                "Starts a new cluster",
                                Some("Choose the cluster settings file (cluster.conf)."),
                            ),
                            option(
                                "join",
                                "Joins an existing cluster",
                                Some(
                                    "Choose the join bundle from the Dashboard's Hosts, Add host.",
                                ),
                            ),
                        ]),
                    field("file", FieldType::File, "Settings file or join bundle").required(),
                ],
            },
            |values| load_origin(values, now).map(|_| ()),
        )
        .await?;
    let start = load_origin(&values, now)
        .map_err(|(_, m)| Stop::failed(m.unwrap_or_else(|| "The file isn't valid.".into())))?;
    let (host, host_settings) = match &start.origin {
        Origin::Join(bundle) => {
            ctx.output(format!(
                "Joining {} as {}.",
                bundle.cluster_name, bundle.slot
            ));
            (bundle.slot, bundle.host_settings.clone())
        }
        Origin::NewCluster { .. } => {
            let cluster = start.cluster.clone();
            let values = ctx
                .values(host_form(&start.cluster), move |values| {
                    let host = slot_of(values)?;
                    let mut settings = host_values(values);
                    placeholders(&mut settings);
                    bmac_settings::validate_host(host, &settings, &cluster).map_err(api_errors)
                })
                .await?;
            let host = slot_of(&values).map_err(|_| Stop::failed("No host was chosen."))?;
            (host, host_values(&values))
        }
    };
    let cluster_name = start
        .cluster
        .get("PROXMOX_CLUSTER_NAME")
        .cloned()
        .unwrap_or_default();
    ctx.phase(
        "settings",
        "Cluster and host settings",
        PhaseStatus::Complete,
    );

    ctx.phase("hardware", "Hardware", PhaseStatus::Running);
    let idrac = host_settings.get("IDRAC_IP").cloned();
    let how = ctx
        .values(
            RequestForm::InputGroup {
                title: format!("{host}'s hardware"),
                description: Some(
                    "The pool layout is chosen from the disks the machine itself reports.".into(),
                ),
                context: None,
                layout: None,
                fields: vec![
                    field("source", FieldType::Select, "Read it")
                        .required()
                        .default_value(json!("discover"))
                        .choices(vec![
                            option("discover", "With a discovery session now", None),
                            option(
                                "file",
                                "From an inventory file",
                                Some("One a discovery session saved, or bmac-inventory printed."),
                            ),
                        ]),
                    field("inventory_file", FieldType::File, "Inventory file"),
                ],
            },
            |values| {
                if text(values, "source").as_deref() == Some("file") {
                    let path = text(values, "inventory_file")
                        .ok_or_else(|| message("Choose the inventory file."))?;
                    let content = read_text(&path).map_err(message)?;
                    serde_json::from_str::<Inventory>(&content)
                        .map_err(|e| message(format!("This isn't an inventory: {e}")))?;
                }
                Ok(())
            },
        )
        .await?;
    let mut seen = None;
    let (inventory, method_hint) = if text(&how, "source").as_deref() == Some("file") {
        let path = text(&how, "inventory_file").unwrap_or_default();
        let content = read_text(&path).map_err(Stop::failed)?;
        let inventory: Inventory = serde_json::from_str(&content).map_err(|e| e.to_string())?;
        (inventory, None)
    } else {
        let preset = Preset {
            address: host_settings.get("PROXMOX_IP").cloned(),
            prefix: host_settings
                .get("PROXMOX_PREFIX")
                .and_then(|p| p.parse().ok()),
            gateway: host_settings.get("PROXMOX_GATEWAY").cloned(),
            dns: start
                .cluster
                .get("PROXMOX_DNS_SERVER")
                .map(|d| {
                    d.split([',', ' '])
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
            public_mac: host_settings.get("PROXMOX_PUBLIC_MAC").cloned(),
            idrac: idrac.clone(),
        };
        let found = discovery::discover(ctx, env, &preset).await?;
        seen = found.network;
        (found.inventory, Some(found.method))
    };
    ctx.phase("hardware", "Hardware", PhaseStatus::Complete);

    ctx.phase("network", "Network ports", PhaseStatus::Running);
    let form = network_form(
        &inventory,
        &host_settings,
        seen.as_ref(),
        host,
        &start.cluster,
    );
    let taken: Vec<(HostSlot, String)> = match &start.origin {
        Origin::Join(bundle) => bundle
            .hosts
            .iter()
            .map(|h| (h.host, h.private_ip.clone()))
            .collect(),
        Origin::NewCluster { .. } => Vec::new(),
    };
    let values = {
        let (inventory, settings, cluster, taken) = (
            inventory.clone(),
            host_settings.clone(),
            start.cluster.clone(),
            taken.clone(),
        );
        ctx.values(form, move |values| {
            network_from(values, &inventory, host, &settings, &cluster, &taken).map(|_| ())
        })
        .await?
    };
    let host_settings = network_from(
        &values,
        &inventory,
        host,
        &host_settings,
        &start.cluster,
        &taken,
    )
    .map_err(|(_, m)| Stop::failed(m.unwrap_or_else(|| "The network isn't valid.".into())))?;
    for nic in &inventory.nics {
        let chosen = |key: &str| {
            host_settings
                .get(key)
                .is_some_and(|m| m.eq_ignore_ascii_case(&nic.mac))
        };
        if chosen("PROXMOX_SECONDARY_MAC") && !matches!(nic.link, NicLink::Up { .. }) {
            ctx.warning(format!(
                "{} had no link during discovery. Host setup waits for carrier on it; check \
                 its cable and the private VLAN's switch port before installing.",
                nic.name
            ));
        }
        if chosen("PROXMOX_PUBLIC_MAC") && !matches!(nic.link, NicLink::Up { .. }) {
            ctx.warning(format!(
                "{} had no link during discovery, so the installed host may not reach the \
                 internet.",
                nic.name
            ));
        }
    }
    ctx.phase("network", "Network ports", PhaseStatus::Complete);

    ctx.phase("layout", "Pool layout", PhaseStatus::Running);
    let values = ctx
        .values(layout_form(&inventory), {
            let inventory = inventory.clone();
            move |values| check_boot_disks(&layout_from(values)?, &inventory)
        })
        .await?;
    let layout = layout_from(&values)
        .map_err(|(_, m)| Stop::failed(m.unwrap_or_else(|| "The layout isn't valid.".into())))?;
    ctx.phase("layout", "Pool layout", PhaseStatus::Complete);

    ctx.phase(
        "secrets",
        "Root password and Tailscale key",
        PhaseStatus::Running,
    );
    let secrets = ctx.values(secrets_form(host), check_secrets).await?;
    let status = tailnet::status(&*env.runner, tailscale).await?;
    let suffix = tailnet::magic_dns_suffix(&status).ok_or_else(|| {
        Stop::failed("This workstation's Tailscale doesn't report the tailnet's MagicDNS name.")
    })?;
    ctx.phase(
        "secrets",
        "Root password and Tailscale key",
        PhaseStatus::Complete,
    );

    ctx.phase("build", "Host ISO", PhaseStatus::Running);
    let request = HostIsoRequest {
        host,
        host_settings: host_settings.clone(),
        origin: start.origin.clone(),
        inventory: inventory.clone(),
        layout: layout.clone(),
        root_password: text(&secrets, "root_password").unwrap_or_default(),
        tailscale_key: text(&secrets, "tailscale_key").unwrap_or_default(),
        magic_dns_suffix: suffix,
    };
    drop(secrets);
    let packages = hostiso::read_packages(&packages_dir)?;
    let prepared = hostiso::prepare(
        &request,
        &packages,
        env.now(),
        hostiso::tskey::ACCEPT_REUSED_KEYS,
    )?;
    drop(request);
    for warning in &prepared.warnings {
        ctx.warning(warning.clone());
    }
    let note = note(ctx);
    let progress = download_progress(ctx, "Proxmox VE".into());
    let built = hostiso::build(
        &prepared,
        &hostiso::BuildEnv {
            cache_dir: &env.cache_dir,
            iso_dir: &env.iso_dir,
            runner: &*env.runner,
            dpkg_deb,
            xorriso,
            fetch: &*env.fetch,
            progress: &progress,
            note: &note,
        },
    )
    .await?;
    let built_ms = env.now();
    let path = built.path.display().to_string();
    env.isos.record(BuiltIso {
        path: path.clone(),
        kind: IsoKind::Host,
        target: host.to_string(),
        built_ms,
    });
    let card = prepared.card.clone();
    let card_file = env.iso_dir.join(format!("{host}-card.txt"));
    std::fs::write(&card_file, card_text(&card))
        .map_err(|e| format!("{}: {e}", card_file.display()))?;
    ctx.output(format!("Built {path} (SHA-256 {}).", built.sha256));
    ctx.output(format!(
        "The host card is in {}. Keep it somewhere other than the machine.",
        card_file.display()
    ));
    ctx.phase("build", "Host ISO", PhaseStatus::Complete);

    // Once the installed host is on the tailnet nothing reads the ISO again.
    let mut on_tailnet = false;
    let outcome = async {
        ctx.phase("install", "Install", PhaseStatus::Running);
        let default_method = method_hint.unwrap_or(if idrac.is_some() {
            BootMethod::Idrac
        } else {
            BootMethod::RemoteConsole
        });
        let how = ctx
            .values(
                RequestForm::InputGroup {
                    title: format!("How will you boot {host}?"),
                    description: None,
                    context: None,
                    layout: None,
                    fields: vec![
                        discovery::method_field(default_method),
                        discovery::idrac_field(idrac.as_deref()),
                    ],
                },
                |values| discovery::parse_idrac(values).map(|_| ()),
            )
            .await?;
        let method = discovery::parse_method(&how);
        let idrac = discovery::parse_idrac(&how).unwrap_or(None);
        let erased: Vec<ErasedDisk> = layout
            .vdevs
            .iter()
            .filter(|v| v.boot)
            .flat_map(|v| &v.members)
            .filter_map(|member| {
                inventory
                    .disks
                    .iter()
                    .find(|d| d.by_id.iter().any(|n| n == member.as_str()))
            })
            .map(|d| ErasedDisk {
                name: d.name.clone(),
                model: d.model.clone(),
                identity: d
                    .identity
                    .as_ref()
                    .map(|i| i.to_string())
                    .or_else(|| d.serial.as_ref().map(|s| format!("serial {s}")))
                    .unwrap_or_else(|| "no identity".into()),
                size_bytes: d.size_bytes,
            })
            .collect();
        let target = InstallTarget {
            host: host.to_string(),
            fqdn: card.host.clone(),
            public_ip: card.public_ip.clone(),
            iso_path: path.clone(),
            erased,
            public_mac: host_settings
                .get("PROXMOX_PUBLIC_MAC")
                .cloned()
                .unwrap_or_default(),
            tailscale_name: card.tailscale_name.clone(),
            tag: tailnet::HOST_TAG.to_string(),
            idrac,
        };
        let warning = boot::destructive_warning(&target);
        let confirmed = ctx
            .confirm(RequestForm::Confirm {
                title: format!("Install Proxmox VE on {host}?"),
                message: warning[0].clone(),
                severity: Severity::Destructive,
                confirm_label: Some("Show the install steps".into()),
                cancel_label: None,
                confirmation_text: Some(host.to_string()),
                context: None,
                details: warning[1..].to_vec(),
            })
            .await?;
        if !confirmed {
            return Err(Stop::failed("Not installed."));
        }
        let wait_from = env.now();
        ctx.acknowledge(RequestForm::ManualAction {
            title: format!("Install {host}"),
            instructions: discovery::instructions(boot::steps(method, &target), &path),
            acknowledge_label: Some("It's installing".into()),
            context: None,
        })
        .await?;
        ctx.phase("install", "Install", PhaseStatus::Complete);

        ctx.phase("wait", "Wait for the tailnet", PhaseStatus::Running);
        let tag = tailnet::HOST_TAG.to_string();
        let report = discovery::peer_report(ctx, card.tailscale_name.clone(), tag.clone());
        let (ip, dns_name) = tailnet::wait_for_peer(
            &*env.runner,
            tailscale,
            &card.tailscale_name,
            &tag,
            built_ms.min(wait_from),
            env.poll,
            Arc::new(tokio::sync::Notify::new()),
            ctx.token(),
            &report,
        )
        .await
        .map_err(|e| stopped(ctx, e))?;
        on_tailnet = true;
        ctx.phase("wait", "Wait for the tailnet", PhaseStatus::Complete);

        ctx.phase("handoff", "Hand over", PhaseStatus::Running);
        let checked_host = if dns_name.is_empty() {
            ip.clone()
        } else {
            dns_name
        };
        let cert_report = {
            let ctx = ctx.clone();
            move |check: &CertCheck| {
                if let CertCheck::Waiting(reason) = check {
                    ctx.output(format!("Waiting for the Dashboard to start ({reason})."));
                }
            }
        };
        let check = handoff::wait_for_certificate(
            &checked_host,
            env.dashboard_port,
            &card.https_fingerprint,
            env.poll,
            ctx.token(),
            &cert_report,
        )
        .await
        .map_err(|e| stopped(ctx, e))?;
        let next = match &start.origin {
            Origin::NewCluster { .. } => NextStep::FirstHost,
            Origin::Join(bundle) => existing_dashboard(ctx, bundle).await?,
        };
        let luks = layout.encryption == Encryption::Luks2;
        let view = handoff::view(
            &check,
            &card.dashboard_url,
            &card.https_fingerprint,
            &next,
            luks,
            target.idrac.as_deref(),
        );
        if let Some(warning) = view.warning {
            return Err(Stop::Failed {
                message: "The host's certificate doesn't match. There is no Dashboard link.".into(),
                details: Some(warning),
            });
        }
        ctx.output(format!(
            "{} presents the certificate this Deployer made.",
            card.dashboard_url
        ));

        let set_up = ctx
            .confirm(RequestForm::Confirm {
                title: format!("Set up SSH from this workstation to {host}?"),
                message: format!(
                    "Adds {host} to ~/.ssh/config, pinned to the host key in the ISO ({}).",
                    card.ssh_fingerprint
                ),
                severity: Severity::Normal,
                confirm_label: Some("Set it up".into()),
                cancel_label: Some("Skip".into()),
                confirmation_text: None,
                context: None,
                details: Vec::new(),
            })
            .await?;
        let pin = HostPin {
            alias: workstation_ssh::pin_alias(&cluster_name, &host.to_string()),
            address: ip.clone(),
            host_key: card
                .ssh_host_key
                .split_whitespace()
                .take(2)
                .collect::<Vec<_>>()
                .join(" "),
            jump: None,
        };
        if set_up {
            let setup = SshSetup {
                ssh_dir: env.ssh_dir.clone(),
                state_dir: env.state_dir.join("ssh"),
                ssh,
                runner: &*env.runner,
                attempts: 10,
                wait: env.poll,
                lock_timeout: Duration::from_secs(30),
                note: &note,
            };
            match setup.apply(&pin, &[host.to_string()]).await {
                Ok(changed) => ctx.output(if changed {
                    format!("ssh {host} now reaches the host.")
                } else {
                    format!("ssh {host} was already set up.")
                }),
                Err(SshSetupError::Mismatch(m)) => ctx.warning(format!(
                    "SSH wasn't set up: {m} The Dashboard link is still safe to use, because its \
                     certificate matched."
                )),
                Err(SshSetupError::Failed(m)) => {
                    ctx.warning(format!("SSH wasn't set up: {m}"));
                    for line in workstation_ssh::manual_instructions(&host.to_string(), &pin) {
                        ctx.output(line);
                    }
                }
            }
        } else {
            for line in workstation_ssh::manual_instructions(&host.to_string(), &pin) {
                ctx.output(line);
            }
        }

        for message in &view.messages {
            ctx.next_step(message.clone());
        }
        ctx.result(json!({
            "open_url": card.dashboard_url,
            "dashboard_url": card.dashboard_url,
            "fingerprint": card.https_fingerprint,
            "card_file": card_file.display().to_string(),
        }));
        ctx.phase("handoff", "Hand over", PhaseStatus::Complete);
        Ok::<(), Stop>(())
    }
    .await;
    discovery::dispose(ctx, env, &path, on_tailnet).await;
    outcome
}
