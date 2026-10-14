// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Cluster settings rendered for scripts and loaded back with the legacy
//! `config.sh` in a real shell: every value comes back identical, and every
//! setting `config.sh` rejects is rejected by the typed layer first.

mod harness;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use bmac_core::HostSlot;
use bmac_proto::api::ErrorCode;
use bmacd::settings::{
    BridgeAddresses, CLUSTER_KEYS, HOST_KEYS, INSTALLED, Settings, Values, bridge_addresses,
    file_text, parse_conf, render, ssh_config_text, validate_cluster, validate_host,
};
use harness::{Lab, host};

fn values(pairs: &[(&str, &str)]) -> Values {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// A full cluster that sets every cluster key.
fn template_cluster() -> Values {
    values(&[
        ("PROXMOX_CLUSTER_NAME", "MyAppCloud"),
        ("PROXMOX_QDEVICE_HOST", "qdevice"),
        ("PROXMOX_INTERNAL_DOMAIN", "pve.internal"),
        ("MAX_MOX_HOSTS", "10"),
        ("PRIVATE_SUBNET_CIDR", "10.213.0.0/24"),
        ("PROXMOX_PRIVATE_BRIDGE", "vmbr-private"),
        ("PROXMOX_MIGRATION_NETWORK", "10.213.0.240/28"),
        ("DATACENTER_PRIVATE_VLAN_GATEWAY", "10.213.0.1"),
        ("GUEST_EGRESS_VIP", "10.213.0.10/24"),
        ("MOX_IP_START", "10.213.0.11"),
        ("MOX_IP_END", "10.213.0.20"),
        ("MOX_REPLICATION_IP_START", "10.213.0.241"),
        ("MOX_REPLICATION_IP_END", "10.213.0.250"),
        ("HAPROXY_IP_START", "10.213.0.21"),
        ("HAPROXY_IP_END", "10.213.0.30"),
        ("PRODUCTION_IP_START", "10.213.0.31"),
        ("PRODUCTION_IP_END", "10.213.0.50"),
        ("STAGING_IP_START", "10.213.0.51"),
        ("STAGING_IP_END", "10.213.0.200"),
        ("HAPROXY_LXC_ROOTFS_GB", "8"),
        ("HAPROXY_LXC_MEMORY_MB", "18432"),
        ("HAPROXY_LXC_CORES", "8"),
        ("HAPROXY_LXC_STORAGE", "local-zfs"),
        ("PROD_VM_CORES", "64"),
        ("PROD_VM_MEMORY_GIB", "74"),
        ("PROD_VM_DISK_GB", "1024"),
        ("PROD_VM_BRIDGE", "vmbr-private"),
        ("PROD_VM_CPU_TYPE", "x86-64-v2-AES"),
        ("PROD_VM_REPLICATION_INTERVAL", "*/15"),
        ("STAGING_VM_CORES", "10"),
        ("STAGING_VM_MEMORY_GIB", "16"),
        ("STAGING_VM_BRIDGE", "vmbr-private"),
        ("VM_BLOCK_SIZES", "16K,64K"),
        ("ISO_STORAGE_ID", "local"),
        ("SNIPPETS_STORAGE_ID", "local"),
        (
            "PROXMOX_ISO_FILE_URL",
            "https://enterprise.proxmox.com/iso/proxmox-ve_9.2-1.iso",
        ),
        (
            "PROXMOX_ISO_FILE_SHA256",
            "4e88fe416df9b527624a175f24c9aa07c714d3332afb1ee3dbf3879573ef2c6c",
        ),
        (
            "PROD_GUEST_OS_ISO_URL",
            "https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-live-server-amd64.iso",
        ),
        (
            "PROD_GUEST_OS_ISO_SHA256",
            "cc8a95cde20f6ced61a322420de00f10cc3c90ced545daa46cb9c1a117f1d927",
        ),
        ("PROD_GUEST_OS_INSTALL_MODE", "ubuntu-autoinstall"),
        ("PROXMOX_DNS_SERVER", "1.1.1.1"),
        ("GUEST_DNS_SERVERS", "1.1.1.1,1.0.0.1"),
        ("PROXMOX_ADMIN_EMAIL", "admin@myapp.com"),
        ("PROXMOX_TIMEZONE", "America/Denver"),
        ("PROXMOX_COUNTRY", "us"),
        ("PROXMOX_KEYBOARD", "en-us"),
        ("TAILSCALE_TAG", "tag:bmac-proxmox-host"),
        ("MOX_SSH_CONNECT_TIMEOUT", "8"),
        (
            "ADMIN_1_PUBLIC_SSH_KEY",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4 admin1@myapp.com",
        ),
        (
            "ADMIN_2_PUBLIC_SSH_KEY",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPIdG/s65LGj6TCZMI9gFwrAKAClmhr4871Cgidd9RNR admin2@myapp.com",
        ),
    ])
}

/// A /22 private network holding 32 hosts, whose production range
/// crosses an octet boundary.
fn wide_cluster() -> Values {
    values(&[
        ("PROXMOX_INTERNAL_DOMAIN", "pve.internal"),
        ("MAX_MOX_HOSTS", "32"),
        ("PRIVATE_SUBNET_CIDR", "10.40.0.0/22"),
        ("PROXMOX_MIGRATION_NETWORK", "10.40.3.192/26"),
        ("DATACENTER_PRIVATE_VLAN_GATEWAY", "10.40.0.1"),
        ("GUEST_EGRESS_VIP", "10.40.0.10/22"),
        ("MOX_IP_START", "10.40.0.11"),
        ("MOX_IP_END", "10.40.0.60"),
        ("MOX_REPLICATION_IP_START", "10.40.3.193"),
        ("MOX_REPLICATION_IP_END", "10.40.3.240"),
        ("HAPROXY_IP_START", "10.40.0.71"),
        ("HAPROXY_IP_END", "10.40.0.120"),
        ("PRODUCTION_IP_START", "10.40.0.200"),
        ("PRODUCTION_IP_END", "10.40.1.254"),
        ("STAGING_IP_START", "10.40.2.0"),
        ("STAGING_IP_END", "10.40.3.150"),
    ])
}

/// mox32 in the wide cluster, with every value `config.sh` derives.
fn wide_mox32() -> Values {
    values(&[
        ("NVME_MIRROR_0_SERIAL_1", "W32A"),
        ("NVME_MIRROR_0_SERIAL_2", "W32B"),
        ("PROXMOX_IP", "203.0.113.32"),
        ("PROXMOX_GATEWAY", "203.0.113.1"),
        ("PROXMOX_PREFIX", "24"),
        ("PROXMOX_PUBLIC_MAC", "02:00:00:00:00:32"),
        ("PROXMOX_SECONDARY_MAC", "02:00:00:00:01:32"),
        ("TAILSCALE_HOSTNAME", "mox32"),
        ("PROXMOX_SECONDARY_IP", "10.40.0.42/22"),
        ("HAPROXY_LXC_VMID", "9142"),
        ("HAPROXY_LXC_HOSTNAME", "haproxy32"),
        ("HAPROXY_LXC_IP", "10.40.0.102/22"),
        ("HAPROXY_LXC_GATEWAY", "10.40.0.42"),
        ("VRRP_PRIORITY", "168"),
        ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", "4"),
        ("MAX_PROD_VM_COUNT_ON_THIS_HOST", "2"),
    ])
}

/// mox1's facts in the full cluster.
fn template_mox1() -> Values {
    values(&[
        ("NVME_MIRROR_0_SERIAL_1", "ABCD012345678900BGN"),
        ("NVME_MIRROR_0_SERIAL_2", "DEFD012345678900BGN"),
        ("NVME_MIRROR_0_CAPACITY_BYTES_1", "2000398934016"),
        ("NVME_MIRROR_0_CAPACITY_BYTES_2", "2000398934016"),
        ("PROXMOX_IP", "32.99.26.33"),
        ("PROXMOX_GATEWAY", "32.99.26.1"),
        ("PROXMOX_PREFIX", "24"),
        ("TAILSCALE_HOSTNAME", "mox1"),
        ("PROXMOX_PUBLIC_MAC", "AA:BB:CC:DD:EE:FF"),
        ("PROXMOX_SECONDARY_MAC", "BB:AA:BB:CC:DD:EE"),
        ("PROXMOX_SECONDARY_IP", "10.213.0.11/24"),
        ("HAPROXY_LXC_VMID", "9111"),
        ("HAPROXY_LXC_HOSTNAME", "haproxy1"),
        ("HAPROXY_LXC_IP", "10.213.0.21/24"),
        ("HAPROXY_LXC_GATEWAY", "10.213.0.11"),
        ("VRRP_PRIORITY", "199"),
        ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", "5"),
        ("MAX_PROD_VM_COUNT_ON_THIS_HOST", "1"),
    ])
}

/// A second mox: mirror pairs 0 and 2 (a gap is allowed), no derived keys.
fn template_mox2() -> Values {
    values(&[
        ("NVME_MIRROR_0_SERIAL_1", "S2A"),
        ("NVME_MIRROR_0_SERIAL_2", "S2B"),
        ("NVME_MIRROR_2_SERIAL_1", "S2C"),
        ("NVME_MIRROR_2_SERIAL_2", "S2D"),
        ("NVME_MIRROR_2_CAPACITY_BYTES_1", "3840755982336"),
        ("NVME_MIRROR_2_CAPACITY_BYTES_2", "3840755982336"),
        ("PROXMOX_IP", "32.99.26.34"),
        ("PROXMOX_GATEWAY", "32.99.26.1"),
        ("PROXMOX_PREFIX", "24"),
        ("PROXMOX_PUBLIC_MAC", "aa:bb:cc:dd:ee:01"),
        ("PROXMOX_SECONDARY_MAC", "auto"),
        ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", "3"),
        ("MAX_PROD_VM_COUNT_ON_THIS_HOST", "2"),
    ])
}

/// A small cluster with only the required keys and values that are hard
/// to quote.
fn small_cluster() -> Values {
    values(&[
        ("PROXMOX_INTERNAL_DOMAIN", "lab.pve.internal"),
        ("MAX_MOX_HOSTS", "3"),
        ("PRIVATE_SUBNET_CIDR", "192.168.7.0/24"),
        ("PROXMOX_MIGRATION_NETWORK", "192.168.7.248/29"),
        ("DATACENTER_PRIVATE_VLAN_GATEWAY", "192.168.7.254"),
        ("GUEST_EGRESS_VIP", "192.168.7.2/24"),
        ("MOX_IP_START", "192.168.7.3"),
        ("MOX_IP_END", "192.168.7.5"),
        ("MOX_REPLICATION_IP_START", "192.168.7.249"),
        ("MOX_REPLICATION_IP_END", "192.168.7.251"),
        ("HAPROXY_IP_START", "192.168.7.6"),
        ("HAPROXY_IP_END", "192.168.7.8"),
        ("PRODUCTION_IP_START", "192.168.7.9"),
        ("PRODUCTION_IP_END", "192.168.7.9"),
        ("STAGING_IP_START", "192.168.7.10"),
        ("STAGING_IP_END", "192.168.7.200"),
        ("PROXMOX_CLUSTER_NAME", "  it's \"quoted\" $HOME `x` \\n  "),
        (
            "ADMIN_1_PUBLIC_SSH_KEY",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIOmM o'brien's laptop'",
        ),
        ("GUEST_DNS_SERVERS", "'9.9.9.9'"),
    ])
}

fn small_mox3() -> Values {
    values(&[
        ("NVME_MIRROR_0_SERIAL_1", "PHAB 1234"),
        ("NVME_MIRROR_0_SERIAL_2", "PHAB 5678"),
        ("IDRAC_IP", "10.20.30.3"),
        ("IDRAC_ASSET_ID", "rack 4, slot 2"),
        ("PROXMOX_IP", "203.0.113.7"),
        ("PROXMOX_GATEWAY", "203.0.113.1"),
        ("PROXMOX_PREFIX", "28"),
        ("PROXMOX_PUBLIC_MAC", "02:00:00:00:00:07"),
        ("PROXMOX_SECONDARY_MAC", "02:00:00:00:01:07"),
        ("PROXMOX_SECONDARY_IP", "192.168.7.5/24"),
        ("HAPROXY_LXC_IP", "192.168.7.8/24"),
        ("VRRP_PRIORITY", "197"),
        ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", "1"),
        ("MAX_PROD_VM_COUNT_ON_THIS_HOST", "1"),
    ])
}

const DERIVED: &[&str] = &[
    "TAILSCALE_HOSTNAME",
    "PROXMOX_SECONDARY_IP",
    "HAPROXY_LXC_VMID",
    "HAPROXY_LXC_HOSTNAME",
    "HAPROXY_LXC_IP",
    "HAPROXY_LXC_GATEWAY",
    "VRRP_PRIORITY",
];

fn config_sh() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../legacy/scripts/lib/config.sh")
}

/// Loads `dir` with `config.sh` for `host`, and returns every settings key
/// it set, or its error output.
fn load(dir: &Path, host: HostSlot) -> Result<BTreeMap<String, String>, String> {
    let script = r#"
        source "$1" || exit 2
        load_proxmox_config --host "$2" || exit 3
        shift 2
        for key in "$@"; do
            if [[ -n "${!key+x}" ]]; then printf '%s=%s\0' "$key" "${!key}"; fi
        done
    "#;
    let output = Command::new("bash")
        .arg("-c")
        .arg(script)
        .arg("bash")
        .arg(config_sh())
        .arg(host.to_string())
        .args(CLUSTER_KEYS.iter().chain(HOST_KEYS).map(|k| k.name))
        .args(INSTALLED.iter().map(|(name, _)| name))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("BMAC_CONFIG_TEST_MODE", "1")
        .env("BMAC_CONFIG_DIR", dir)
        .output()
        .unwrap();
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|item| !item.is_empty())
        .map(|item| {
            let text = String::from_utf8(item.to_vec()).unwrap();
            let (k, v) = text.split_once('=').unwrap();
            (k.to_string(), v.to_string())
        })
        .collect())
}

fn render_dir(lab: &Lab) -> PathBuf {
    let dir = lab.dir.path().canonicalize().unwrap().join("run-config");
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Every key set comes back identical, and nothing else but the
/// `INSTALLED` values and the keys `config.sh` derives from the slot.
fn assert_round_trip(lab: &Lab, cluster: &Values, hosts: &[(HostSlot, Values)]) {
    validate_cluster(cluster).unwrap();
    let dir = render_dir(lab);
    let refs: Vec<(HostSlot, &Values)> = hosts.iter().map(|(h, v)| (*h, v)).collect();
    render(&dir, Some(cluster), &refs).unwrap();
    for (host, facts) in hosts {
        validate_host(*host, facts, cluster).unwrap();
        let loaded = load(&dir, *host).unwrap_or_else(|e| panic!("{host}: {e}"));
        for (key, value) in cluster.iter().chain(facts) {
            assert_eq!(loaded.get(key), Some(value), "{host} {key}");
        }
        for (key, value) in INSTALLED {
            assert_eq!(
                loaded.get(*key).map(String::as_str),
                Some(*value),
                "{host} {key}"
            );
        }
        for key in loaded.keys() {
            assert!(
                cluster.contains_key(key)
                    || facts.contains_key(key)
                    || INSTALLED.iter().any(|(name, _)| name == key)
                    || DERIVED.contains(&key.as_str()),
                "{host}: config.sh set {key}, which wasn't rendered"
            );
        }
    }
}

#[test]
fn rendered_settings_load_back_identically_in_config_sh() {
    let lab = Lab::new(&[1]);
    assert_round_trip(
        &lab,
        &template_cluster(),
        &[(host(1), template_mox1()), (host(2), template_mox2())],
    );
    assert_round_trip(&lab, &small_cluster(), &[(host(3), small_mox3())]);
}

#[test]
fn a_wide_private_network_with_32_hosts_loads_back() {
    let lab = Lab::new(&[1]);
    let mut mox20 = wide_mox32();
    for key in [
        "TAILSCALE_HOSTNAME",
        "HAPROXY_LXC_VMID",
        "HAPROXY_LXC_HOSTNAME",
        "HAPROXY_LXC_IP",
        "HAPROXY_LXC_GATEWAY",
        "VRRP_PRIORITY",
    ] {
        mox20.remove(key);
    }
    mox20.insert("NVME_MIRROR_0_SERIAL_1".into(), "W20A".into());
    mox20.insert("NVME_MIRROR_0_SERIAL_2".into(), "W20B".into());
    mox20.insert("PROXMOX_SECONDARY_IP".into(), "10.40.0.55/21".into());
    mox20.insert("PROXMOX_SECONDARY_GATEWAY".into(), "10.40.7.254".into());
    assert_round_trip(
        &lab,
        &wide_cluster(),
        &[(host(32), wide_mox32()), (host(20), mox20.clone())],
    );
    let dir = render_dir(&lab);
    render(&dir, Some(&wide_cluster()), &[(host(20), &mox20)]).unwrap();
    let loaded = load(&dir, host(20)).unwrap();
    assert_eq!(loaded["HAPROXY_LXC_IP"], "10.40.0.90/22");
    assert_eq!(loaded["HAPROXY_LXC_VMID"], "9130");

    for (key, value) in [
        ("PROXMOX_SECONDARY_IP", "10.40.0.42/23"),
        ("PROXMOX_SECONDARY_IP", "10.40.0.61/22"),
        ("HAPROXY_LXC_IP", "10.40.0.102/24"),
    ] {
        let mox32 = changed(wide_mox32(), (key, Some(value)));
        assert!(
            validate_host(host(32), &mox32, &wide_cluster()).is_err(),
            "{key}={value} was accepted"
        );
        render(&dir, Some(&wide_cluster()), &[(host(32), &mox32)]).unwrap();
        assert!(
            load(&dir, host(32)).is_err(),
            "config.sh accepted {key}={value}"
        );
    }
    let text = ssh_config_text(&wide_cluster(), [(host(20), &mox20)]).unwrap();
    assert_eq!(text.matches("\nHost mox").count(), 32);
    assert!(text.contains("Host mox20\n    HostName 10.40.0.55\n"));
    assert!(text.contains("Host mox32\n    HostName 10.40.0.42\n"));
}

#[test]
fn the_address_plan_is_locked_once_the_cluster_exists() {
    let lab = Lab::new(&[1]);
    let settings = Settings::new(
        lab.pmxcfs.clone(),
        lab.dir.path().join("pve/priv/bmac/config"),
    );
    settings
        .save_cluster(wide_cluster(), None, "ada@pve", 1)
        .unwrap();
    for change in [
        &[("MAX_MOX_HOSTS", "31")][..],
        &[
            ("PRIVATE_SUBNET_CIDR", "10.40.0.0/21"),
            ("GUEST_EGRESS_VIP", "10.40.0.10/21"),
        ],
        &[("MOX_IP_START", "10.40.0.12")],
        &[("MOX_IP_END", "10.40.0.59")],
        &[("MOX_REPLICATION_IP_START", "10.40.3.194")],
        &[("MOX_REPLICATION_IP_END", "10.40.3.239")],
        &[("HAPROXY_IP_START", "10.40.0.72")],
        &[("HAPROXY_IP_END", "10.40.0.119")],
        &[("PRODUCTION_IP_START", "10.40.0.201")],
        &[("PRODUCTION_IP_END", "10.40.1.253")],
        &[("STAGING_IP_START", "10.40.2.1")],
        &[("STAGING_IP_END", "10.40.3.149")],
    ] {
        let mut next = wide_cluster();
        for (key, value) in change {
            next.insert(key.to_string(), value.to_string());
        }
        validate_cluster(&next).unwrap_or_else(|e| panic!("{change:?}: {e:?}"));
        let refused = settings
            .save_cluster(next, Some(1), "ada@pve", 2)
            .unwrap_err();
        assert_eq!(refused.code, ErrorCode::Invalid, "{change:?}");
        assert!(
            refused.field_errors[change[0].0].contains("can't change once the cluster exists"),
            "{change:?}: {refused:?}"
        );
    }
    let mut next = wide_cluster();
    next.insert("GUEST_EGRESS_VIP".into(), "10.40.0.9/22".into());
    next.insert("PROXMOX_CLUSTER_NAME".into(), "Wide".into());
    settings.save_cluster(next, Some(1), "ada@pve", 2).unwrap();
}

#[test]
fn a_chosen_private_address_and_routed_gateway_load_back() {
    let lab = Lab::new(&[1]);
    let mut mox1 = template_mox1();
    for (key, value) in [
        ("PROXMOX_SECONDARY_IP", "10.213.0.15/23"),
        ("HAPROXY_LXC_GATEWAY", "10.213.0.15"),
        ("PROXMOX_SECONDARY_GATEWAY", "10.213.1.1"),
        ("PROXMOX_SECONDARY_GATEWAY_ROUTE", "yes"),
    ] {
        mox1.insert(key.into(), value.into());
    }
    assert_round_trip(&lab, &template_cluster(), &[(host(1), mox1)]);
}

#[test]
fn the_private_bridge_carries_the_private_and_migration_addresses() {
    let ip = |s: &str| s.parse().unwrap();
    assert_eq!(
        bridge_addresses(host(1), &template_mox1(), &template_cluster()).unwrap(),
        BridgeAddresses {
            private: (ip("10.213.0.11"), 24),
            replication: (ip("10.213.0.241"), 28),
            gateway: None,
            gateway_route: false,
        }
    );
    let mut mox1 = template_mox1();
    for (key, value) in [
        ("PROXMOX_SECONDARY_IP", "10.213.0.15/23"),
        ("HAPROXY_LXC_GATEWAY", "10.213.0.15"),
        ("PROXMOX_SECONDARY_GATEWAY", "10.213.1.1"),
        ("PROXMOX_SECONDARY_GATEWAY_ROUTE", "yes"),
    ] {
        mox1.insert(key.into(), value.into());
    }
    let routed = bridge_addresses(host(1), &mox1, &template_cluster()).unwrap();
    assert_eq!(routed.private, (ip("10.213.0.15"), 23));
    assert_eq!(routed.gateway, Some(ip("10.213.1.1")));
    assert!(routed.gateway_route);
    let third = bridge_addresses(host(3), &small_mox3(), &small_cluster()).unwrap();
    assert_eq!(third.replication, (ip("192.168.7.251"), 29));
}

#[test]
fn two_hosts_may_not_share_a_private_address() {
    let cluster = template_cluster();
    let mut mox2 = template_mox2();
    mox2.insert("PROXMOX_SECONDARY_IP".into(), "10.213.0.11/24".into());
    let mox1 = template_mox1();
    let error = bmac_settings::check_private_ips([(host(1), &mox1), (host(2), &mox2)], &cluster)
        .unwrap_err();
    assert!(error.message.contains("mox1"), "{error:?}");
    mox2.insert("PROXMOX_SECONDARY_IP".into(), "10.213.0.13/24".into());
    bmac_settings::check_private_ips([(host(1), &mox1), (host(2), &mox2)], &cluster).unwrap();
}

#[test]
fn conf_files_are_read_as_config_sh_reads_them() {
    for (cluster, facts) in [
        (template_cluster(), template_mox1()),
        (small_cluster(), small_mox3()),
    ] {
        assert_eq!(
            parse_conf(&file_text(CLUSTER_KEYS, &cluster)).unwrap(),
            cluster
        );
        assert_eq!(parse_conf(&file_text(HOST_KEYS, &facts)).unwrap(), facts);
    }
    assert_eq!(
        parse_conf("  # note\n\tMAX_MOX_HOSTS = \"3\" \nEMPTY=\nX=''\n").unwrap(),
        values(&[("MAX_MOX_HOSTS", "3"), ("EMPTY", ""), ("X", "")])
    );
    for (text, why) in [
        ("export A=1", "literal KEY=VALUE"),
        ("a=1", "literal KEY=VALUE"),
        ("1A=1", "literal KEY=VALUE"),
        ("A", "literal KEY=VALUE"),
        ("A='1", "unterminated"),
        ("A=\"1'", "unterminated"),
        ("A='", "unterminated"),
        ("A=1\r\n", "CRLF"),
        ("A=1\nA=2", "line 2: duplicate key A"),
    ] {
        let error = parse_conf(text).unwrap_err();
        assert!(error.contains(why), "{text:?}: {error}");
    }
}

#[test]
fn conf_files_import_into_the_records() {
    let lab = Lab::new(&[1]);
    let settings = Settings::new(
        lab.pmxcfs.clone(),
        lab.dir.path().join("pve/priv/bmac/config"),
    );
    let dir = lab.dir.path().join("import");
    std::fs::create_dir(&dir).unwrap();
    let error = settings.import(&dir, "root@pam", 1).unwrap_err();
    assert!(
        error.message.contains("has no cluster.conf"),
        "{}",
        error.message
    );

    std::fs::write(
        dir.join("cluster.conf"),
        file_text(CLUSTER_KEYS, &template_cluster()),
    )
    .unwrap();
    std::fs::write(
        dir.join("mox1.conf"),
        file_text(HOST_KEYS, &template_mox1()),
    )
    .unwrap();
    assert_eq!(settings.import(&dir, "root@pam", 1).unwrap(), [host(1)]);
    assert_eq!(
        settings.cluster().unwrap().unwrap().values,
        template_cluster()
    );
    assert_eq!(
        settings.host(host(1)).unwrap().unwrap().values,
        template_mox1()
    );

    let mut renamed = template_cluster();
    renamed.insert("PROXMOX_CLUSTER_NAME".into(), "bmaclab".into());
    std::fs::write(dir.join("cluster.conf"), file_text(CLUSTER_KEYS, &renamed)).unwrap();
    std::fs::remove_file(dir.join("mox1.conf")).unwrap();
    std::fs::write(
        dir.join("mox2.conf"),
        file_text(HOST_KEYS, &template_mox2()),
    )
    .unwrap();
    assert_eq!(settings.import(&dir, "root@pam", 2).unwrap(), [host(2)]);
    let cluster = settings.cluster().unwrap().unwrap();
    assert_eq!((cluster.values, cluster.revision), (renamed, 2));
    assert_eq!(settings.hosts().unwrap().len(), 2, "mox1 keeps its facts");

    std::fs::write(dir.join("mox2.conf"), "PROXMOX_IP=nope\n").unwrap();
    assert_eq!(
        settings.import(&dir, "root@pam", 3).unwrap_err().code,
        ErrorCode::Invalid
    );
}

#[test]
fn rendered_files_are_private_and_follow_the_records() {
    let lab = Lab::new(&[1]);
    let settings = Settings::new(
        lab.pmxcfs.clone(),
        lab.dir.path().join("pve/priv/bmac/config"),
    );
    let dir = render_dir(&lab);
    settings.render_to(&dir).unwrap();
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "nothing to render yet"
    );
    assert_eq!(
        settings
            .save_host(host(1), template_mox1(), None, "ada@pve", 1)
            .unwrap_err()
            .code,
        ErrorCode::Conflict,
        "host facts need cluster settings first"
    );

    let cluster = settings
        .save_cluster(template_cluster(), None, "ada@pve", 1)
        .unwrap();
    assert_eq!(cluster.revision, 1);
    settings
        .save_host(host(1), template_mox1(), None, "ada@pve", 2)
        .unwrap();
    settings
        .save_host(host(2), template_mox2(), None, "ada@pve", 2)
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("mox9.conf"), "stale").unwrap();
    settings.render_to(&dir).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&dir), 0o700);
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, ["cluster.conf", "mox1.conf", "mox2.conf"]);
    for name in &names {
        assert_eq!(mode(&dir.join(name)), 0o600, "{name}");
    }
    assert_eq!(load(&dir, host(2)).unwrap()["PROXMOX_IP"], "32.99.26.34");

    // Rendering unchanged settings again leaves the files alone, and a file
    // with the wrong mode is replaced.
    use std::os::unix::fs::MetadataExt;
    let inode = |p: &Path| std::fs::metadata(p).unwrap().ino();
    let before = inode(&dir.join("cluster.conf"));
    std::fs::set_permissions(
        dir.join("mox1.conf"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    settings.render_to(&dir).unwrap();
    assert_eq!(inode(&dir.join("cluster.conf")), before);
    assert_eq!(mode(&dir.join("mox1.conf")), 0o600);

    // The address plan can't change, and a stale revision is refused.
    let mut shrink = template_cluster();
    shrink.insert("MAX_MOX_HOSTS".into(), "1".into());
    let refused = settings
        .save_cluster(shrink, Some(1), "ada@pve", 3)
        .unwrap_err();
    assert!(
        refused
            .message
            .contains("can't change once the cluster exists"),
        "{}",
        refused.message
    );
    let mut renamed = template_cluster();
    renamed.insert("PROXMOX_CLUSTER_NAME".into(), "Renamed".into());
    settings
        .save_cluster(renamed.clone(), Some(1), "ada@pve", 3)
        .unwrap();
    assert_eq!(
        settings
            .save_cluster(renamed, Some(1), "ada@pve", 4)
            .unwrap_err()
            .code,
        ErrorCode::Conflict
    );
    settings.render_to(&dir).unwrap();
    assert_eq!(
        load(&dir, host(1)).unwrap()["PROXMOX_CLUSTER_NAME"],
        "Renamed"
    );

    let mut secret = template_mox1();
    secret.insert("IDRAC_ASSET_ID".into(), "tskey-auth-abc123".into());
    assert_eq!(
        settings
            .save_host(host(1), secret, Some(1), "ada@pve", 5)
            .unwrap_err()
            .code,
        ErrorCode::Invalid,
        "secret material is never a setting"
    );
}

type Change = (&'static str, Option<&'static str>);

/// Settings `config.sh` rejects: each must be refused by the typed layer
/// too.
const CLUSTER_REJECTED: &[Change] = &[
    ("MAX_MOX_HOSTS", Some("33")),
    ("MAX_MOX_HOSTS", Some("0")),
    ("PRIVATE_SUBNET_CIDR", Some("10.213.0.0/25")),
    ("PRIVATE_SUBNET_CIDR", Some("10.212.0.0/15")),
    ("PRIVATE_SUBNET_CIDR", Some("10.213.0.1/24")),
    ("PROXMOX_INTERNAL_DOMAIN", Some("pve.local")),
    ("GUEST_EGRESS_VIP", Some("10.213.0.15/24")),
    ("DATACENTER_PRIVATE_VLAN_GATEWAY", Some("10.213.0.0")),
    ("DATACENTER_PRIVATE_VLAN_GATEWAY", Some("10.213.1.1")),
    ("MOX_IP_END", Some("10.213.0.15")),
    ("HAPROXY_IP_START", Some("10.213.0.20")),
    ("STAGING_IP_END", Some("10.213.0.50")),
    ("PRODUCTION_IP_END", Some("10.213.0.30")),
    ("PROXMOX_MIGRATION_NETWORK", Some("10.213.0.0/28")),
    ("PROXMOX_MIGRATION_NETWORK", Some("10.213.0.241/28")),
    ("MOX_REPLICATION_IP_END", Some("10.213.0.200")),
    ("PROXMOX_ISO_FILE_URL", Some("http://example.com/pve.iso")),
    (
        "PROD_GUEST_OS_ISO_URL",
        Some("https://example.com/ubuntu.img"),
    ),
    ("PROXMOX_ISO_FILE_SHA256", Some("abc123")),
    ("PROD_GUEST_OS_INSTALL_MODE", Some("kickstart")),
    ("ADMIN_1_PUBLIC_SSH_KEY", Some("not a key")),
    ("PROD_VM_CORES", Some("0")),
    ("MOX_SSH_CONNECT_TIMEOUT", Some("08")),
    ("PROD_VM_BRIDGE", Some("vmbr private")),
    ("STAGING_VM_BRIDGE", Some("a-very-long-bridge-name")),
    ("VM_BLOCK_SIZES", Some("8K")),
    ("VM_BLOCK_SIZES", Some("16K,16K")),
    ("VM_BLOCK_SIZES", Some("16K,")),
    ("VM_BLOCK_SIZES", Some("16k")),
    ("MOX_IP_START", None),
    ("PROXMOX_MIGRATION_NETWORK", None),
];

const HOST_REJECTED: &[Change] = &[
    ("PROXMOX_PREFIX", Some("33")),
    ("PROXMOX_PUBLIC_MAC", Some("AA:BB:CC:DD:EE")),
    ("PROXMOX_SECONDARY_MAC", Some("aa:bb:cc:dd:ee:ff")),
    ("PROXMOX_SECONDARY_MAC", Some("none")),
    ("MAX_PROD_VM_COUNT_ON_THIS_HOST", Some("0")),
    ("IDRAC_IP", Some("999.1.1.1")),
    ("PROXMOX_GATEWAY", Some("32.99.26")),
    ("TAILSCALE_HOSTNAME", Some("mox9")),
    ("PROXMOX_SECONDARY_IP", Some("10.213.0.12/24")),
    ("PROXMOX_SECONDARY_IP", Some("10.213.0.200/24")),
    ("PROXMOX_SECONDARY_IP", Some("10.213.0.11/25")),
    ("PROXMOX_SECONDARY_IP", Some("10.99.0.11/24")),
    ("PROXMOX_SECONDARY_IP", Some("10.213.0.11")),
    ("PROXMOX_SECONDARY_IP", Some("10.213.0.011/24")),
    ("PROXMOX_SECONDARY_GATEWAY", Some("10.214.0.1")),
    ("PROXMOX_SECONDARY_GATEWAY", Some("10.213.0.11")),
    ("PROXMOX_SECONDARY_GATEWAY", Some("10.213.0.255")),
    ("PROXMOX_SECONDARY_GATEWAY_ROUTE", Some("yes")),
    ("PROXMOX_SECONDARY_GATEWAY_ROUTE", Some("maybe")),
    ("HAPROXY_LXC_IP", Some("10.213.0.22/24")),
    ("HAPROXY_LXC_GATEWAY", Some("10.213.0.1")),
    ("HAPROXY_LXC_VMID", Some("9112")),
    ("HAPROXY_LXC_HOSTNAME", Some("haproxy2")),
    ("VRRP_PRIORITY", Some("150")),
    ("NVME_MIRROR_0_SERIAL_2", None),
    ("NVME_MIRROR_0_SERIAL_2", Some("ABCD012345678900BGN")),
    ("NVME_MIRROR_0_CAPACITY_BYTES_2", None),
    ("NVME_MIRROR_0_CAPACITY_BYTES_1", Some("0")),
    ("NVME_MIRROR_1_SERIAL_1", Some("ONLY-ONE")),
    ("NVME_MIRROR_3_CAPACITY_BYTES_1", Some("100")),
    ("PROXMOX_IP", None),
    ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", None),
];

fn changed(mut values: Values, (key, value): Change) -> Values {
    match value {
        Some(value) => values.insert(key.into(), value.into()),
        None => values.remove(key),
    };
    values
}

#[test]
fn what_config_sh_rejects_the_typed_layer_rejects() {
    let lab = Lab::new(&[1]);
    let root = lab.dir.path().canonicalize().unwrap();
    let cases: Vec<(Values, Values, String)> = CLUSTER_REJECTED
        .iter()
        .map(|change| {
            (
                changed(template_cluster(), *change),
                template_mox1(),
                format!("cluster {change:?}"),
            )
        })
        .chain(HOST_REJECTED.iter().map(|change| {
            (
                template_cluster(),
                changed(template_mox1(), *change),
                format!("mox1 {change:?}"),
            )
        }))
        .collect();
    // Each case starts its own shell, so they run side by side.
    std::thread::scope(|scope| {
        for (n, (cluster, mox1, what)) in cases.iter().enumerate() {
            let dir = root.join(format!("run-config-{n}"));
            scope.spawn(move || {
                let typed =
                    validate_cluster(cluster).and_then(|_| validate_host(host(1), mox1, cluster));
                assert!(typed.is_err(), "the typed layer accepted {what}");
                render(&dir, Some(cluster), &[(host(1), mox1)]).unwrap();
                assert!(load(&dir, host(1)).is_err(), "config.sh accepted {what}");
            });
        }
    });
    let mut reused = template_mox2();
    reused.insert("NVME_MIRROR_2_SERIAL_1".into(), "S2A".into());
    let dir = render_dir(&lab);
    render(&dir, Some(&template_cluster()), &[(host(2), &reused)]).unwrap();
    assert!(
        load(&dir, host(2))
            .unwrap_err()
            .contains("reused across mirror pairs")
    );
    assert!(validate_host(host(2), &reused, &template_cluster()).is_err());
}

#[test]
fn the_typed_layer_is_stricter_where_config_sh_is_loose() {
    let mox1 = template_mox1();
    for (change, why) in [
        (
            ("PROD_VM_CPU_TYPE", Some("")),
            "an empty value is left out instead",
        ),
        (
            ("HAPROXY_IP_START", Some("21")),
            "addresses are full dotted quads",
        ),
        (("MOX_IP_START", Some("10.213.0.011")), "no leading zeros"),
        (
            ("GUEST_EGRESS_VIP", Some("10.213.0.10/25")),
            "the VIP has the private subnet's prefix",
        ),
        (("PROD_VM_CPU_TYPE", Some("x86\tv2")), "one plain line"),
        (("NOT_A_SETTING", Some("x")), "unknown keys"),
        (
            ("CLUSTER_STATE_DIR", Some("/tmp/registry")),
            "the package fixes the registry's place",
        ),
        (
            (
                "PROXMOX_SSH_KNOWN_HOSTS_FILE",
                Some("/root/.ssh/known_hosts"),
            ),
            "host keys are always pinned from pmxcfs",
        ),
        (
            ("PROXMOX_ROOT_PASSWORD", Some("x")),
            "secrets aren't settings",
        ),
    ] {
        let cluster = changed(template_cluster(), change);
        assert!(
            validate_cluster(&cluster).is_err(),
            "{why}: {change:?} was accepted"
        );
    }
    let error = validate_cluster(&changed(
        template_cluster(),
        ("MOX_IP_START", Some("10.213.0.011")),
    ))
    .unwrap_err();
    assert!(error.field_errors.contains_key("MOX_IP_START"), "{error:?}");
    assert!(
        validate_host(
            host(1),
            &changed(mox1, ("IDRAC_ASSET_ID", Some(""))),
            &template_cluster()
        )
        .is_err()
    );
}

#[test]
fn the_ssh_config_pins_every_slot_to_its_private_address_and_cluster_host_key() {
    let text = ssh_config_text(&small_cluster(), []).unwrap();
    let blocks: Vec<&str> = text.split("\nHost ").skip(1).collect();
    assert_eq!(blocks.len(), 3, "{text}");
    assert_eq!(
        blocks[2],
        "mox3\n\
         \x20   HostName 192.168.7.5\n\
         \x20   User root\n\
         \x20   HostKeyAlias mox3\n\
         \x20   UserKnownHostsFile /etc/pve/nodes/mox3/ssh_known_hosts\n\
         \x20   GlobalKnownHostsFile none\n\
         \x20   StrictHostKeyChecking yes\n\
         \x20   CheckHostIP no\n\
         \x20   BatchMode yes\n\
         \x20   ClearAllForwardings yes\n\
         \x20   ConnectTimeout 8\n\
         \x20   ServerAliveInterval 15\n\
         \x20   ServerAliveCountMax 2\n"
    );
    assert!(blocks[0].starts_with("mox1\n    HostName 192.168.7.3\n"));

    let lab = Lab::new(&[1]);
    let settings = Settings::new(
        lab.pmxcfs.clone(),
        lab.dir.path().join("pve/priv/bmac/config"),
    );
    let path = lab.dir.path().join("etc/bmac/ssh_config");
    settings.render_ssh_config(&path).unwrap();
    assert!(
        !std::fs::read_to_string(&path).unwrap().contains("Host "),
        "no hosts before there are settings"
    );
    settings
        .save_cluster(template_cluster(), None, "ada@pve", 1)
        .unwrap();
    settings.render_ssh_config(&path).unwrap();
    let rendered = std::fs::read_to_string(&path).unwrap();
    assert_eq!(rendered.matches("\nHost mox").count(), 10);
    assert!(rendered.contains("Host mox10\n    HostName 10.213.0.20\n"));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}
