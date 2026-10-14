// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Every key of v2's `cluster.conf` and `moxN.conf`, with the checks
//! `scripts/lib/config.sh` makes on them (`_config_validate_cluster`,
//! `_config_validate_mox`), and the files' text. Shared by `bmacd`, which
//! stores the settings in pmxcfs, and the Deployer, which collects the
//! first host's settings for the new-cluster seed.
//!
//! Where `config.sh` is loose, this layer is stricter: addresses are always
//! full dotted quads, and empty values are refused (a key is either set or
//! absent). So anything accepted here loads in `config.sh` unchanged, and
//! anything `config.sh` rejects is refused here first. No secret is a
//! setting: `secrets.env` has no counterpart.

use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;

use bmac_core::HostSlot;
use bmac_pmxcfs::reject_secret_material;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::settings::SettingKey;
use serde::Serialize;

const MAX_VALUE: usize = 1024;
/// The VM disk block sizes BMAC defines a storage for (`STORAGE.md`, "VM
/// disk block size").
pub const BLOCK_SIZES: [&str; 4] = ["16K", "32K", "64K", "128K"];

/// What a key holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "choices")]
pub enum Kind {
    Text,
    PositiveInteger,
    /// `MAX_MOX_HOSTS`: 1 to [`HostSlot::MAX`].
    HostCount,
    /// `PROXMOX_PREFIX`: 1 to 32.
    PrefixLength,
    Ipv4,
    /// A usable address in `PRIVATE_SUBNET_CIDR`.
    PrivateAddress,
    /// A usable address in `PRIVATE_SUBNET_CIDR` with its prefix.
    PrivateInterface,
    /// `PRIVATE_SUBNET_CIDR`: an IPv4 network from /16 to /24.
    PrivateSubnet,
    /// An IPv4 network with no host bits set.
    Network,
    /// A private domain ending in `.internal`.
    InternalDomain,
    /// A Linux interface name, 1 to 15 characters.
    Interface,
    Sha256,
    /// A public HTTPS URL ending in `.iso`.
    IsoUrl,
    SshPublicKey,
    Mac,
    MacOrAuto,
    /// `VM_BLOCK_SIZES`: some of 16K, 32K, 64K, and 128K, comma-separated,
    /// each once.
    BlockSizes,
    Choice(&'static [&'static str]),
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Key {
    pub name: &'static str,
    #[serde(flatten)]
    pub kind: Kind,
    pub required: bool,
}

const fn key(name: &'static str, kind: Kind, required: bool) -> Key {
    Key {
        name,
        kind,
        required,
    }
}

use Kind::*;

/// The keys of `cluster.conf`, in the order the file is written. v2's
/// legacy aliases (`PROXMOX_MAX_HOSTS` and the like) aren't settings; their
/// canonical keys are.
pub const CLUSTER_KEYS: &[Key] = &[
    key("PROXMOX_CLUSTER_NAME", Text, false),
    key("PROXMOX_QDEVICE_HOST", Text, false),
    key("PROXMOX_INTERNAL_DOMAIN", InternalDomain, true),
    key("MAX_MOX_HOSTS", HostCount, true),
    key("PRIVATE_SUBNET_CIDR", PrivateSubnet, true),
    key("PROXMOX_PRIVATE_BRIDGE", Interface, false),
    key("PROXMOX_MIGRATION_NETWORK", Network, true),
    key("DATACENTER_PRIVATE_VLAN_GATEWAY", PrivateAddress, true),
    key("GUEST_EGRESS_VIP", PrivateInterface, true),
    key("MOX_IP_START", PrivateAddress, true),
    key("MOX_IP_END", PrivateAddress, true),
    key("MOX_REPLICATION_IP_START", PrivateAddress, true),
    key("MOX_REPLICATION_IP_END", PrivateAddress, true),
    key("HAPROXY_IP_START", PrivateAddress, true),
    key("HAPROXY_IP_END", PrivateAddress, true),
    key("PRODUCTION_IP_START", PrivateAddress, true),
    key("PRODUCTION_IP_END", PrivateAddress, true),
    key("STAGING_IP_START", PrivateAddress, true),
    key("STAGING_IP_END", PrivateAddress, true),
    key("HAPROXY_LXC_ROOTFS_GB", PositiveInteger, false),
    key("HAPROXY_LXC_MEMORY_MB", PositiveInteger, false),
    key("HAPROXY_LXC_CORES", PositiveInteger, false),
    key("HAPROXY_LXC_STORAGE", Text, false),
    key("HAPROXY_LXC_TEMPLATE", Text, false),
    key("PROD_VM_CORES", PositiveInteger, false),
    key("PROD_VM_MEMORY_GIB", PositiveInteger, false),
    key("PROD_VM_DISK_GB", PositiveInteger, false),
    key("PROD_VM_BRIDGE", Interface, false),
    key("PROD_VM_CPU_TYPE", Text, false),
    key("PROD_VM_REPLICATION_INTERVAL", Text, false),
    key("STAGING_VM_CORES", PositiveInteger, false),
    key("STAGING_VM_MEMORY_GIB", PositiveInteger, false),
    key("STAGING_VM_DISK_GB", PositiveInteger, false),
    key("STAGING_VM_BRIDGE", Interface, false),
    key("VM_BLOCK_SIZES", BlockSizes, false),
    key("ISO_STORAGE_ID", Text, false),
    key("SNIPPETS_STORAGE_ID", Text, false),
    key("PROXMOX_ISO_FILE_URL", IsoUrl, false),
    key("PROXMOX_ISO_FILE_SHA256", Sha256, false),
    key("PROD_GUEST_OS_ISO_URL", IsoUrl, false),
    key("PROD_GUEST_OS_ISO_SHA256", Sha256, false),
    key(
        "PROD_GUEST_OS_INSTALL_MODE",
        Choice(&["ubuntu-autoinstall", "manual"]),
        false,
    ),
    key("PROXMOX_DNS_SERVER", Text, false),
    key("GUEST_DNS_SERVERS", Text, false),
    key("DNS_SEARCH_DOMAIN", Text, false),
    key("PROXMOX_ADMIN_EMAIL", Text, false),
    key("PROXMOX_TIMEZONE", Text, false),
    key("PROXMOX_COUNTRY", Text, false),
    key("PROXMOX_KEYBOARD", Text, false),
    key("TAILSCALE_TAG", Text, false),
    key("MOX_SSH_CONNECT_TIMEOUT", PositiveInteger, false),
    key("ADMIN_1_PUBLIC_SSH_KEY", SshPublicKey, false),
    key("ADMIN_2_PUBLIC_SSH_KEY", SshPublicKey, false),
];

const fn mirror(name: &'static str, kind: Kind) -> Key {
    key(name, kind, false)
}

/// The cluster's address plan: fixed once the cluster settings exist,
/// because hosts, guests, and proxies already hold addresses from it.
pub const LOCKED_CLUSTER_KEYS: &[&str] = &[
    "PRIVATE_SUBNET_CIDR",
    "MAX_MOX_HOSTS",
    "MOX_IP_START",
    "MOX_IP_END",
    "MOX_REPLICATION_IP_START",
    "MOX_REPLICATION_IP_END",
    "HAPROXY_IP_START",
    "HAPROXY_IP_END",
    "PRODUCTION_IP_START",
    "PRODUCTION_IP_END",
    "STAGING_IP_START",
    "STAGING_IP_END",
];

/// Refuses a change to any of [`LOCKED_CLUSTER_KEYS`].
pub fn check_locked(current: &Values, next: &Values) -> Result<(), ApiError> {
    for key in LOCKED_CLUSTER_KEYS {
        if current.get(*key) != next.get(*key) {
            return Err(field(
                key,
                format!(
                    "is part of the cluster's address plan and can't change once the cluster \
                     exists (it is {})",
                    current.get(*key).map_or("unset", String::as_str)
                ),
            ));
        }
    }
    Ok(())
}

/// The keys of `moxN.conf`, in the order the file is written. The ones
/// `config.sh` derives from the host's slot may be set only to the value it
/// derives.
pub const HOST_KEYS: &[Key] = &[
    key("IDRAC_IP", Ipv4, false),
    key("IDRAC_ASSET_ID", Text, false),
    key("PROXMOX_IP", Ipv4, true),
    key("PROXMOX_GATEWAY", Ipv4, true),
    key("PROXMOX_PREFIX", PrefixLength, true),
    key("PROXMOX_PUBLIC_MAC", Mac, true),
    key("PROXMOX_SECONDARY_MAC", MacOrAuto, true),
    key("PROXMOX_SECONDARY_IP", Text, false),
    key("PROXMOX_SECONDARY_GATEWAY", Ipv4, false),
    key(
        "PROXMOX_SECONDARY_GATEWAY_ROUTE",
        Choice(&["yes", "no"]),
        false,
    ),
    key("TAILSCALE_HOSTNAME", Text, false),
    key("HAPROXY_LXC_VMID", Text, false),
    key("HAPROXY_LXC_HOSTNAME", Text, false),
    key("HAPROXY_LXC_IP", Text, false),
    key("HAPROXY_LXC_GATEWAY", Text, false),
    key("VRRP_PRIORITY", Text, false),
    key("MAX_PROD_VM_COUNT_ON_THIS_HOST", PositiveInteger, true),
    key("MAX_STAGING_VM_COUNT_ON_THIS_HOST", PositiveInteger, true),
    mirror("NVME_MIRROR_0_SERIAL_1", Text),
    mirror("NVME_MIRROR_0_SERIAL_2", Text),
    mirror("NVME_MIRROR_0_CAPACITY_BYTES_1", PositiveInteger),
    mirror("NVME_MIRROR_0_CAPACITY_BYTES_2", PositiveInteger),
    mirror("NVME_MIRROR_1_SERIAL_1", Text),
    mirror("NVME_MIRROR_1_SERIAL_2", Text),
    mirror("NVME_MIRROR_1_CAPACITY_BYTES_1", PositiveInteger),
    mirror("NVME_MIRROR_1_CAPACITY_BYTES_2", PositiveInteger),
    mirror("NVME_MIRROR_2_SERIAL_1", Text),
    mirror("NVME_MIRROR_2_SERIAL_2", Text),
    mirror("NVME_MIRROR_2_CAPACITY_BYTES_1", PositiveInteger),
    mirror("NVME_MIRROR_2_CAPACITY_BYTES_2", PositiveInteger),
    mirror("NVME_MIRROR_3_SERIAL_1", Text),
    mirror("NVME_MIRROR_3_SERIAL_2", Text),
    mirror("NVME_MIRROR_3_CAPACITY_BYTES_1", PositiveInteger),
    mirror("NVME_MIRROR_3_CAPACITY_BYTES_2", PositiveInteger),
    mirror("NVME_MIRROR_4_SERIAL_1", Text),
    mirror("NVME_MIRROR_4_SERIAL_2", Text),
    mirror("NVME_MIRROR_4_CAPACITY_BYTES_1", PositiveInteger),
    mirror("NVME_MIRROR_4_CAPACITY_BYTES_2", PositiveInteger),
];

pub type Values = BTreeMap<String, String>;

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorCode::Invalid, message)
}

fn field(key: &str, message: impl Into<String>) -> ApiError {
    let message = message.into();
    let mut error = invalid(format!("{key}: {message}"));
    error.field_errors.insert(key.to_string(), message);
    error
}

fn is_positive_integer(value: &str) -> bool {
    value.starts_with(|c: char| ('1'..='9').contains(&c))
        && value.bytes().all(|b| b.is_ascii_digit())
}

fn is_mac(value: &str) -> bool {
    let parts: Vec<&str> = value.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_label(label: &str) -> bool {
    !label.is_empty()
        && label.len() <= 63
        && !label.starts_with('-')
        && !label.ends_with('-')
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn is_iso_url(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("https://") else {
        return false;
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = path.split('?').next().unwrap_or("");
    !authority.is_empty()
        && !authority.contains('@')
        && !value.contains('#')
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
        && path.to_ascii_lowercase().ends_with(".iso")
}

fn check_kind(key: &Key, value: &str, subnet: Option<Subnet>) -> Result<(), String> {
    match key.kind {
        Text => Ok(()),
        PositiveInteger => is_positive_integer(value)
            .then_some(())
            .ok_or_else(|| "must be a positive integer".into()),
        HostCount => matches!(value.parse::<u8>(), Ok(n) if (1..=HostSlot::MAX).contains(&n) && is_positive_integer(value))
            .then_some(())
            .ok_or_else(|| format!("must be between 1 and {}", HostSlot::MAX)),
        PrefixLength => matches!(value.parse::<u8>(), Ok(1..=32) if is_positive_integer(value))
            .then_some(())
            .ok_or_else(|| "must be between 1 and 32".into()),
        Ipv4 => value
            .parse::<Ipv4Addr>()
            .map(|_| ())
            .map_err(|_| "is not valid IPv4".into()),
        PrivateAddress | PrivateInterface => {
            let (address, length) = match value.split_once('/') {
                Some((address, length)) if key.kind == PrivateInterface => (address, Some(length)),
                None if key.kind == PrivateAddress => (value, None),
                _ if key.kind == PrivateInterface => {
                    return Err("must be an address with its prefix, such as /24".into());
                }
                _ => return Err("is not valid IPv4".into()),
            };
            let ip: Ipv4Addr = address.parse().map_err(|_| "is not valid IPv4")?;
            match subnet {
                Some(subnet) if !subnet.usable(ip) => {
                    Err(format!("must be a usable address in {subnet}"))
                }
                Some(subnet) if length.is_some_and(|l| l != subnet.len.to_string()) => {
                    Err(format!("must have the private subnet's prefix, /{}", subnet.len))
                }
                _ => Ok(()),
            }
        }
        PrivateSubnet => Subnet::parse(value).map(|_| ()),
        Network => parse_network(value).map(|_| ()),
        InternalDomain => {
            let labels: Vec<&str> = value.split('.').collect();
            (labels.len() >= 2
                && labels.last() == Some(&"internal")
                && labels.iter().all(|l| is_label(l)))
            .then_some(())
            .ok_or_else(|| "must be a lowercase private domain ending in .internal".into())
        }
        Interface => (value.len() <= 15
            && value.starts_with(|c: char| c.is_ascii_alphanumeric())
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-')))
        .then_some(())
        .ok_or_else(|| "must be a safe Linux interface name (1-15 characters)".into()),
        Sha256 => (value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
            .then_some(())
            .ok_or_else(|| "must contain exactly 64 hexadecimal characters".into()),
        IsoUrl => is_iso_url(value)
            .then_some(())
            .ok_or_else(|| "must be a public HTTPS URL ending in .iso".into()),
        SshPublicKey => [
            "ssh-ed25519",
            "ssh-rsa",
            "ecdsa-sha2-",
            "sk-ssh-",
            "sk-ecdsa-",
        ]
        .iter()
        .any(|p| value.starts_with(p))
        .then_some(())
        .ok_or_else(|| "must contain an OpenSSH public key".into()),
        Mac => is_mac(value)
            .then_some(())
            .ok_or_else(|| "is not a MAC address".into()),
        MacOrAuto => (value == "auto" || is_mac(value))
            .then_some(())
            .ok_or_else(|| "must be a MAC address or auto".into()),
        BlockSizes => {
            let mut seen = Vec::new();
            for size in value.split(',') {
                if !BLOCK_SIZES.contains(&size) {
                    return Err(format!("{size} is not one of {}", BLOCK_SIZES.join(", ")));
                }
                if seen.contains(&size) {
                    return Err(format!("lists {size} twice"));
                }
                seen.push(size);
            }
            Ok(())
        }
        Choice(choices) => choices
            .contains(&value)
            .then_some(())
            .ok_or_else(|| format!("must be one of {}", choices.join(", "))),
    }
}

/// The cluster's private VLAN network (`PRIVATE_SUBNET_CIDR`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub len: u8,
}

impl Subnet {
    /// The prefix lengths a cluster's private network may have.
    pub const LENGTHS: std::ops::RangeInclusive<u8> = 16..=24;

    pub fn parse(value: &str) -> Result<Subnet, String> {
        let (network, len) = parse_network(value)?;
        if !Self::LENGTHS.contains(&len) {
            return Err(format!(
                "must be an IPv4 network from /{} to /{}",
                Self::LENGTHS.start(),
                Self::LENGTHS.end()
            ));
        }
        Ok(Subnet {
            network: Ipv4Addr::from(network),
            len,
        })
    }

    fn mask(self) -> u32 {
        u32::MAX << (32 - self.len)
    }

    pub fn contains(self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask() == u32::from(self.network)
    }

    /// In the network, and neither its network nor its broadcast address.
    pub fn usable(self, ip: Ipv4Addr) -> bool {
        let bits = u32::from(ip);
        self.contains(ip)
            && bits != u32::from(self.network)
            && bits != u32::from(self.network) | !self.mask()
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.network, self.len)
    }
}

/// `start` plus `offset`, for the address of a host's slot in a range.
fn nth(start: Ipv4Addr, offset: u8) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(start).saturating_add(u32::from(offset)))
}

/// An IPv4 network with no host bits set: its address and prefix length.
fn parse_network(value: &str) -> Result<(u32, u8), String> {
    let bad = || "must be an IPv4 network such as 10.213.0.240/28".to_string();
    let (address, length) = value.split_once('/').ok_or_else(bad)?;
    let address: Ipv4Addr = address.parse().map_err(|_| bad())?;
    let length: u8 = length
        .parse()
        .ok()
        .filter(|l: &u8| *l <= 32 && l.to_string() == length)
        .ok_or_else(bad)?;
    let mask = if length == 0 {
        0
    } else {
        u32::MAX << (32 - length)
    };
    let bits = u32::from(address);
    if bits & !mask != 0 {
        return Err("has host bits set".into());
    }
    Ok((bits, length))
}

fn in_network(ip: Ipv4Addr, (net, length): (u32, u8)) -> bool {
    let mask = if length == 0 {
        0
    } else {
        u32::MAX << (32 - length)
    };
    u32::from(ip) & mask == net
}

/// Checks each value against its key: known, non-empty, single-line, and of
/// its kind, with every required key present.
fn check_values(
    keys: &[Key],
    values: &Values,
    what: &str,
    subnet: Option<Subnet>,
) -> Result<(), ApiError> {
    for (name, value) in values {
        let Some(key) = keys.iter().find(|k| k.name == name) else {
            return Err(field(name, format!("is not a {what} key")));
        };
        if value.is_empty() {
            return Err(field(name, "may not be empty; leave it out instead"));
        }
        if value.len() > MAX_VALUE || value.chars().any(char::is_control) {
            return Err(field(
                name,
                format!("must be one line of at most {MAX_VALUE} bytes"),
            ));
        }
        check_kind(key, value, subnet).map_err(|m| field(name, m))?;
    }
    for key in keys.iter().filter(|k| k.required) {
        if !values.contains_key(key.name) {
            return Err(field(key.name, "is required"));
        }
    }
    let value = serde_json::to_value(values).expect("values serialize");
    reject_secret_material(&value, what).map_err(|e| invalid(e.to_string()))
}

/// What the host checks need from valid cluster settings.
#[derive(Debug, Clone)]
struct Plan {
    subnet: Subnet,
    max_hosts: u8,
    mox_start: Ipv4Addr,
    mox_end: Ipv4Addr,
    haproxy_start: Ipv4Addr,
    replication_start: Ipv4Addr,
    migration: (u32, u8),
}

fn address(values: &Values, key: &str) -> Ipv4Addr {
    let value = &values[key];
    value
        .split('/')
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(Ipv4Addr::UNSPECIFIED)
}

/// Checks cluster settings as `config.sh` does.
pub fn validate_cluster(values: &Values) -> Result<(), ApiError> {
    plan(values).map(|_| ())
}

/// The cluster's private network, from valid cluster settings.
pub fn private_subnet(cluster: &Values) -> Result<Subnet, ApiError> {
    Ok(plan(cluster)?.subnet)
}

fn plan(values: &Values) -> Result<Plan, ApiError> {
    check_values(CLUSTER_KEYS, values, "cluster", None)?;
    let subnet = Subnet::parse(&values["PRIVATE_SUBNET_CIDR"])
        .map_err(|m| field("PRIVATE_SUBNET_CIDR", m))?;
    check_values(CLUSTER_KEYS, values, "cluster", Some(subnet))?;
    let max_hosts: u8 = values["MAX_MOX_HOSTS"].parse().unwrap_or(0);
    let ip = |key: &str| address(values, key);
    let n = |key: &str| u32::from(ip(key));
    let mut ranges = Vec::new();
    for (name, start, end, at_least) in [
        ("mox", "MOX_IP_START", "MOX_IP_END", Some(max_hosts)),
        (
            "mox replication",
            "MOX_REPLICATION_IP_START",
            "MOX_REPLICATION_IP_END",
            Some(max_hosts),
        ),
        (
            "HAProxy",
            "HAPROXY_IP_START",
            "HAPROXY_IP_END",
            Some(max_hosts),
        ),
        (
            "production",
            "PRODUCTION_IP_START",
            "PRODUCTION_IP_END",
            None,
        ),
        ("staging", "STAGING_IP_START", "STAGING_IP_END", None),
    ] {
        let (s, e) = (n(start), n(end));
        if s > e {
            return Err(field(
                start,
                format!("{start} through {end} must be a usable ascending range"),
            ));
        }
        if let Some(count) = at_least
            && e - s + 1 < u32::from(count)
        {
            return Err(field(
                start,
                format!("the {name} range must contain at least MAX_MOX_HOSTS addresses"),
            ));
        }
        ranges.push((name, start, s..=e));
    }
    for (what, key) in [
        ("data-center gateway", "DATACENTER_PRIVATE_VLAN_GATEWAY"),
        ("guest-egress VIP", "GUEST_EGRESS_VIP"),
    ] {
        if let Some((name, ..)) = ranges.iter().find(|(_, _, r)| r.contains(&n(key))) {
            return Err(field(key, format!("the {what} overlaps the {name} range")));
        }
    }
    if n("DATACENTER_PRIVATE_VLAN_GATEWAY") == n("GUEST_EGRESS_VIP") {
        return Err(field(
            "GUEST_EGRESS_VIP",
            "the data-center gateway and guest-egress VIP overlap",
        ));
    }
    for (i, (name, start, range)) in ranges.iter().enumerate() {
        for (other, _, other_range) in &ranges[i + 1..] {
            if range.start() <= other_range.end() && other_range.start() <= range.end() {
                return Err(field(
                    start,
                    format!("the {name} and {other} ranges overlap"),
                ));
            }
        }
    }
    let network = parse_network(&values["PROXMOX_MIGRATION_NETWORK"])
        .map_err(|m| field("PROXMOX_MIGRATION_NETWORK", m))?;
    for key in ["MOX_REPLICATION_IP_START", "MOX_REPLICATION_IP_END"] {
        if !in_network(ip(key), network) {
            return Err(field(
                key,
                "the replication address range is outside PROXMOX_MIGRATION_NETWORK",
            ));
        }
    }
    let mox = n("MOX_IP_START")..=n("MOX_IP_END");
    let haproxy = n("HAPROXY_IP_START")..=n("HAPROXY_IP_END");
    if mox
        .chain(haproxy)
        .any(|bits| in_network(Ipv4Addr::from(bits), network))
        || in_network(ip("GUEST_EGRESS_VIP"), network)
    {
        return Err(field(
            "PROXMOX_MIGRATION_NETWORK",
            "the migration network also matches a host, HAProxy, or floating VIP address",
        ));
    }
    Ok(Plan {
        subnet,
        max_hosts,
        mox_start: ip("MOX_IP_START"),
        mox_end: ip("MOX_IP_END"),
        haproxy_start: ip("HAPROXY_IP_START"),
        replication_start: ip("MOX_REPLICATION_IP_START"),
        migration: network,
    })
}

fn private_ip(plan: &Plan, host: HostSlot) -> Ipv4Addr {
    nth(plan.mox_start, host.number() - 1)
}

/// The private VLAN address the cluster's plan gives a host by default:
/// `MOX_IP_START` plus the slot.
pub fn default_private_ip(host: HostSlot, cluster: &Values) -> Result<String, ApiError> {
    Ok(private_ip(&plan(cluster)?, host).to_string())
}

/// A host's private VLAN address and prefix: its `PROXMOX_SECONDARY_IP`,
/// or the plan's default with the private subnet's prefix.
fn private_interface(
    plan: &Plan,
    host: HostSlot,
    values: &Values,
) -> Result<(Ipv4Addr, u8), ApiError> {
    let Some(value) = values.get("PROXMOX_SECONDARY_IP") else {
        return Ok((private_ip(plan, host), plan.subnet.len));
    };
    let bad = || {
        field(
            "PROXMOX_SECONDARY_IP",
            format!(
                "must be an address from {} to {} with a prefix of /{} or shorter, such as \
                 {}/{}",
                plan.mox_start,
                plan.mox_end,
                plan.subnet.len,
                private_ip(plan, host),
                plan.subnet.len
            ),
        )
    };
    let (address, length) = value.split_once('/').ok_or_else(bad)?;
    let ip: Ipv4Addr = address.parse().map_err(|_| bad())?;
    let length: u8 = length
        .parse()
        .ok()
        .filter(|l: &u8| (8..=plan.subnet.len).contains(l) && l.to_string() == length)
        .ok_or_else(bad)?;
    if !(plan.mox_start..=plan.mox_end).contains(&ip) {
        return Err(bad());
    }
    Ok((ip, length))
}

/// A host's address on the private VLAN, without the prefix.
pub fn host_private_ip(
    host: HostSlot,
    values: &Values,
    cluster: &Values,
) -> Result<String, ApiError> {
    Ok(private_interface(&plan(cluster)?, host, values)?
        .0
        .to_string())
}

/// What a host's private bridge carries (v2 `configure_private_network`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeAddresses {
    /// The private VLAN address and prefix.
    pub private: (Ipv4Addr, u8),
    /// The migration address: `MOX_REPLICATION_IP_START` plus the slot,
    /// with `PROXMOX_MIGRATION_NETWORK`'s prefix.
    pub replication: (Ipv4Addr, u8),
    pub gateway: Option<Ipv4Addr>,
    /// A second default route goes through `gateway`.
    pub gateway_route: bool,
}

pub fn bridge_addresses(
    host: HostSlot,
    values: &Values,
    cluster: &Values,
) -> Result<BridgeAddresses, ApiError> {
    validate_host(host, values, cluster)?;
    let plan = plan(cluster)?;
    let gateway = values
        .get("PROXMOX_SECONDARY_GATEWAY")
        .and_then(|g| g.parse().ok());
    Ok(BridgeAddresses {
        private: private_interface(&plan, host, values)?,
        replication: (
            nth(plan.replication_start, host.number() - 1),
            plan.migration.1,
        ),
        gateway,
        gateway_route: gateway.is_some()
            && values
                .get("PROXMOX_SECONDARY_GATEWAY_ROUTE")
                .map(String::as_str)
                == Some("yes"),
    })
}

/// Refuses two hosts with the same private VLAN address.
pub fn check_private_ips<'a>(
    hosts: impl IntoIterator<Item = (HostSlot, &'a Values)>,
    cluster: &Values,
) -> Result<(), ApiError> {
    let plan = plan(cluster)?;
    let mut seen: BTreeMap<Ipv4Addr, HostSlot> = BTreeMap::new();
    for (host, values) in hosts {
        let (ip, _) = private_interface(&plan, host, values)?;
        if let Some(other) = seen.insert(ip, host)
            && other != host
        {
            return Err(field(
                "PROXMOX_SECONDARY_IP",
                format!("{ip} is already {other}'s private address"),
            ));
        }
    }
    Ok(())
}

pub fn validate_host(host: HostSlot, values: &Values, cluster: &Values) -> Result<(), ApiError> {
    let plan = plan(cluster)?;
    check_values(HOST_KEYS, values, "host", None)?;
    let index = host.number();
    if index > plan.max_hosts {
        return Err(invalid(format!(
            "{host} exceeds configured MAX_MOX_HOSTS={}",
            plan.max_hosts
        )));
    }
    if let (Some(public), Some(private)) = (
        values.get("PROXMOX_PUBLIC_MAC"),
        values.get("PROXMOX_SECONDARY_MAC"),
    ) && private != "auto"
        && public.eq_ignore_ascii_case(private)
    {
        return Err(field(
            "PROXMOX_SECONDARY_MAC",
            "public and private NIC MAC addresses must differ",
        ));
    }
    let (private, length) = private_interface(&plan, host, values)?;
    let mox_ip = private.to_string();
    if let Some(gateway) = values.get("PROXMOX_SECONDARY_GATEWAY") {
        let gateway: Ipv4Addr = gateway.parse().unwrap_or(Ipv4Addr::UNSPECIFIED);
        let mask = u32::MAX << (32 - length);
        let network = u32::from(private) & mask;
        if !in_network(gateway, (network, length))
            || gateway == private
            || u32::from(gateway) == network
            || u32::from(gateway) == network | !mask
        {
            return Err(field(
                "PROXMOX_SECONDARY_GATEWAY",
                format!("must be another usable address on {mox_ip}/{length}"),
            ));
        }
    } else if values
        .get("PROXMOX_SECONDARY_GATEWAY_ROUTE")
        .map(String::as_str)
        == Some("yes")
    {
        return Err(field(
            "PROXMOX_SECONDARY_GATEWAY_ROUTE",
            "needs PROXMOX_SECONDARY_GATEWAY",
        ));
    }
    let haproxy_ip = nth(plan.haproxy_start, index - 1);
    for (key, expected) in [
        ("TAILSCALE_HOSTNAME", host.to_string()),
        ("HAPROXY_LXC_HOSTNAME", format!("haproxy{index}")),
        (
            "HAPROXY_LXC_IP",
            format!("{haproxy_ip}/{}", plan.subnet.len),
        ),
        ("HAPROXY_LXC_GATEWAY", mox_ip.clone()),
        ("HAPROXY_LXC_VMID", (9110 + u16::from(index)).to_string()),
        ("VRRP_PRIORITY", (200 - u16::from(index)).to_string()),
    ] {
        if values.get(key).is_some_and(|v| *v != expected) {
            return Err(field(key, format!("for {host} must be {expected}")));
        }
    }
    let mut serials = BTreeSet::new();
    for pair in 0..5 {
        let get =
            |suffix: &str, member: u8| values.get(&format!("NVME_MIRROR_{pair}_{suffix}_{member}"));
        let name = |suffix: &str, member: u8| format!("NVME_MIRROR_{pair}_{suffix}_{member}");
        match (get("SERIAL", 1), get("SERIAL", 2)) {
            (Some(a), Some(b)) => {
                if a == b {
                    return Err(field(
                        &name("SERIAL", 2),
                        format!("NVMe mirror {pair} contains the same serial twice"),
                    ));
                }
                for serial in [a, b] {
                    if !serials.insert(serial.clone()) {
                        return Err(invalid(format!(
                            "NVMe serial is reused across mirror pairs: {serial}"
                        )));
                    }
                }
                if get("CAPACITY_BYTES", 1).is_some() != get("CAPACITY_BYTES", 2).is_some() {
                    return Err(invalid(format!(
                        "NVMe mirror {pair} must define both {} and {}",
                        name("CAPACITY_BYTES", 1),
                        name("CAPACITY_BYTES", 2)
                    )));
                }
            }
            (None, None) => {
                if pair == 0 {
                    return Err(invalid("NVMe mirror 0 is mandatory"));
                }
                if get("CAPACITY_BYTES", 1).is_some() || get("CAPACITY_BYTES", 2).is_some() {
                    return Err(invalid(format!(
                        "NVMe mirror {pair} capacities were provided without serials"
                    )));
                }
            }
            _ => {
                return Err(invalid(format!(
                    "NVMe mirror {pair} must define both {} and {}",
                    name("SERIAL", 1),
                    name("SERIAL", 2)
                )));
            }
        }
    }
    Ok(())
}

impl Key {
    /// How the Settings page describes the key.
    pub fn view(&self) -> SettingKey {
        let kind = serde_json::to_value(self.kind).expect("kinds serialize");
        SettingKey {
            name: self.name.to_string(),
            kind: kind["kind"].as_str().unwrap_or_default().to_string(),
            choices: match self.kind {
                Choice(choices) => choices.iter().map(|c| c.to_string()).collect(),
                _ => Vec::new(),
            },
            required: self.required,
            locked: LOCKED_CLUSTER_KEYS.contains(&self.name),
        }
    }
}

pub const HEADER: &str = "# Written by bmacd from the cluster settings in pmxcfs. It is rewritten\n\
                      # whenever they change, so changes made here are lost.\n";

/// The text of a `cluster.conf` or `moxN.conf`. Each value is wrapped in
/// single quotes, which `config.sh` strips without interpreting anything
/// between them, so any single-line value comes back exactly.
pub fn file_text(keys: &[Key], values: &Values) -> String {
    let mut text = String::from(HEADER);
    for key in keys {
        if let Some(value) = values.get(key.name) {
            text.push_str(&format!("{}='{value}'\n", key.name));
        }
    }
    text
}

/// An SSH client config naming every host slot the settings allow, for
/// `ssh -F`: `moxN` is root at its private address (from `hosts` when its
/// facts are there, else the plan's default), its host key pinned from the
/// cluster's `/etc/pve/nodes/moxN/ssh_known_hosts` under the alias `moxN`,
/// so the address isn't checked against any other file.
pub fn ssh_config_text<'a>(
    cluster: &Values,
    hosts: impl IntoIterator<Item = (HostSlot, &'a Values)>,
) -> Result<String, ApiError> {
    let plan = plan(cluster)?;
    let facts: BTreeMap<HostSlot, &Values> = hosts.into_iter().collect();
    let timeout = cluster
        .get("MOX_SSH_CONNECT_TIMEOUT")
        .map_or("8", String::as_str);
    let mut text = String::from(HEADER);
    for slot in HostSlot::all().take(usize::from(plan.max_hosts)) {
        let host = slot.to_string();
        let address = match facts.get(&slot) {
            Some(values) => private_interface(&plan, slot, values)?.0,
            None => private_ip(&plan, slot),
        };
        text.push_str(&format!(
            "\nHost {host}\n\
             \x20   HostName {address}\n\
             \x20   User root\n\
             \x20   HostKeyAlias {host}\n\
             \x20   UserKnownHostsFile /etc/pve/nodes/{host}/ssh_known_hosts\n\
             \x20   GlobalKnownHostsFile none\n\
             \x20   StrictHostKeyChecking yes\n\
             \x20   CheckHostIP no\n\
             \x20   BatchMode yes\n\
             \x20   ClearAllForwardings yes\n\
             \x20   ConnectTimeout {timeout}\n\
             \x20   ServerAliveInterval 15\n\
             \x20   ServerAliveCountMax 2\n"
        ));
    }
    Ok(text)
}

/// Reads a `cluster.conf` or `moxN.conf` the way `config.sh` does
/// (`_config_parse_file`): comments, blank lines, and literal
/// `KEY=VALUE` lines, where one pair of matching outer quotes is removed
/// and nothing is interpreted. The values aren't checked here.
pub fn parse_conf(text: &str) -> Result<Values, String> {
    let trim = |s: &str| s.trim_matches([' ', '\t']).to_string();
    let mut values = Values::new();
    for (number, line) in text.split('\n').enumerate() {
        let at = |why: &str| format!("line {}: {why}", number + 1);
        if line.ends_with('\r') {
            return Err(at("CRLF line endings are not accepted"));
        }
        let line = trim(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, raw) = line
            .split_once('=')
            .ok_or_else(|| at("expected a literal KEY=VALUE assignment"))?;
        let name = name.trim_end_matches([' ', '\t']);
        if !name.starts_with(|c: char| c.is_ascii_uppercase())
            || !name
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(at("expected a literal KEY=VALUE assignment"));
        }
        let raw = trim(raw);
        let value = match raw.chars().next() {
            Some(quote @ ('\'' | '"')) => {
                if raw.len() < 2 || !raw.ends_with(quote) {
                    return Err(at("unterminated quoted value"));
                }
                raw[1..raw.len() - 1].to_string()
            }
            _ => raw,
        };
        if values.insert(name.to_string(), value).is_some() {
            return Err(at(&format!("duplicate key {name}")));
        }
    }
    Ok(values)
}
