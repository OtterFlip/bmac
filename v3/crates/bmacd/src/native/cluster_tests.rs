// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::io::Write;
use std::sync::{Arc, Mutex};

use bmac_exec::{FakeRunner, Reply};

use super::*;

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn slot(name: &str) -> HostSlot {
    name.parse().unwrap()
}

fn ip(s: &str) -> Ipv4Addr {
    s.parse().unwrap()
}

const INSTALLED: &str = "127.0.0.1 localhost.localdomain localhost\n\
                         203.0.113.5 mox1.pve.internal mox1\n\
                         \n\
                         # The following lines are desirable for IPv6 capable hosts\n\
                         ::1     ip6-localhost ip6-loopback\n";

#[test]
fn hosts_are_pinned_in_one_block_and_the_public_pin_is_dropped() {
    let pins = [
        (slot("mox1"), ip("10.213.0.11")),
        (slot("mox2"), ip("10.213.0.12")),
    ];
    let pinned = pin_hosts(INSTALLED, &pins, "pve.internal").unwrap();
    assert_eq!(
        pinned,
        "127.0.0.1 localhost.localdomain localhost\n\
         \n\
         # The following lines are desirable for IPv6 capable hosts\n\
         ::1     ip6-localhost ip6-loopback\n\
         \n\
         # BEGIN bmac managed host private addresses\n\
         10.213.0.11 mox1.pve.internal mox1\n\
         10.213.0.12 mox2.pve.internal mox2\n\
         # END bmac managed host private addresses\n"
    );
    assert_eq!(pin_hosts(&pinned, &pins, "pve.internal").unwrap(), pinned);
    let fewer = pin_hosts(&pinned, &pins[..1], "pve.internal").unwrap();
    assert!(!fewer.contains("mox2"), "{fewer}");
    assert_eq!(fewer.matches(PINS_BEGIN).count(), 1);
}

#[test]
fn a_mixed_or_unbalanced_hosts_file_is_refused() {
    let pins = [(slot("mox1"), ip("10.213.0.11"))];
    let mixed = "10.0.0.5 mox1 backup.example.com\n";
    assert!(
        pin_hosts(mixed, &pins, "pve.internal")
            .unwrap_err()
            .contains("other names")
    );
    let commented = "10.0.0.5 backup.example.com # not mox1\n";
    assert!(pin_hosts(commented, &pins, "pve.internal").is_ok());
    let unbalanced = format!("{PINS_BEGIN}\n10.213.0.11 mox1\n");
    assert!(
        pin_hosts(&unbalanced, &pins, "pve.internal")
            .unwrap_err()
            .contains("1 start and 0 end")
    );
    let reversed = format!("{PINS_END}\n{PINS_BEGIN}\n");
    assert!(
        pin_hosts(&reversed, &pins, "pve.internal")
            .unwrap_err()
            .contains("out of order")
    );
}

#[test]
fn proxmox_files_are_read_and_written() {
    let conf = "totem {\n  cluster_name: bmaclab\n  config_version: 1\n}\n";
    assert_eq!(corosync_cluster_name(conf).as_deref(), Some("bmaclab"));
    assert_eq!(corosync_cluster_name("totem {\n}\n"), None);
    assert!(quorate("Votequorum information\nQuorate:          Yes\n"));
    assert!(!quorate("Quorate:          No\n"));
    assert_eq!(
        datacenter_with("keyboard: en-us\nmigration: insecure\n", "10.213.0.240/28"),
        "keyboard: en-us\n\
         migration: secure,network=10.213.0.240/28\n\
         replication: secure,network=10.213.0.240/28\n"
    );
    assert!(valid_cluster_name("MyAppCloud"));
    assert!(valid_cluster_name("bmac-lab-1"));
    assert!(!valid_cluster_name("-lab"));
    assert!(!valid_cluster_name("a-name-that-is-too-long"));
    assert!(!valid_cluster_name("it's"));
}

struct Rig {
    root: tempfile::TempDir,
    _work: tempfile::TempDir,
    _input: tokio::io::DuplexStream,
    runner: FakeRunner,
    flow: Workflow,
    out: Sink,
    cluster: Values,
}

fn rig() -> Rig {
    let root = tempfile::tempdir().unwrap();
    let work = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("etc/pve/nodes/mox1")).unwrap();
    std::fs::write(root.path().join("etc/hostname"), "mox1\n").unwrap();
    std::fs::write(root.path().join("etc/hosts"), INSTALLED).unwrap();
    std::fs::write(
        root.path().join("etc/pve/nodes/mox1/ssh_known_hosts"),
        "mox1 ssh-ed25519 AAAA\n",
    )
    .unwrap();
    let out = Sink::default();
    let (input, theirs) = tokio::io::duplex(4096);
    let flow = Workflow::new(
        Box::new(out.clone()),
        tokio::io::BufReader::new(theirs),
        work.path().to_path_buf(),
    );
    let cluster = [
        ("PROXMOX_CLUSTER_NAME", "bmaclab"),
        ("PROXMOX_INTERNAL_DOMAIN", "pve.internal"),
        ("PROXMOX_MIGRATION_NETWORK", "10.213.0.240/28"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    Rig {
        root,
        _work: work,
        _input: input,
        runner: FakeRunner::new(),
        flow,
        out,
        cluster,
    }
}

impl Rig {
    fn cluster(&self) -> Cluster<'_> {
        Cluster {
            flow: &self.flow,
            runner: &self.runner,
            root: self.root.path().to_path_buf(),
            me: slot("mox1"),
            cluster_settings: &self.cluster,
            private_ip: ip("10.213.0.11"),
            pins: vec![(slot("mox1"), ip("10.213.0.11"))],
            poll: Duration::ZERO,
        }
    }

    fn file(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.path().join(path)).unwrap_or_default()
    }

    fn resolves(&self) {
        for name in ["mox1", "mox1.pve.internal"] {
            self.runner.expect(
                GETENT,
                ["ahostsv4", name],
                Reply::ok(format!(
                    "10.213.0.11     STREAM {name}\n10.213.0.11     DGRAM\n"
                )),
            );
        }
        self.runner
            .expect(HOSTNAME, ["-f"], Reply::ok("mox1.pve.internal\n"));
        self.runner
            .expect(TAILSCALE, ["ip", "-4"], Reply::ok("100.64.0.11\n"));
    }

    fn settles(&self) {
        self.runner.expect(
            PVECM,
            ["status"],
            Reply::fail(2, "").stdout("Quorate: No\n"),
        );
        self.runner
            .expect(PVECM, ["status"], Reply::ok("Quorate:          Yes\n"));
        self.runner.expect(
            PVECM,
            ["updatecerts", "--unmerge-known-hosts"],
            Reply::ok(""),
        );
    }
}

#[tokio::test]
async fn the_first_host_creates_the_cluster_on_the_private_vlan_and_the_tailnet() {
    let rig = rig();
    rig.resolves();
    rig.runner.expect(
        PVECM,
        [
            "create",
            "bmaclab",
            "--link0",
            "address=10.213.0.11,priority=100",
            "--link1",
            "address=100.64.0.11,priority=10",
        ],
        Reply::ok(""),
    );
    rig.settles();
    let data = rig.cluster().create().await.unwrap();
    rig.runner.assert_done();
    assert_eq!(data["cluster_name"], "bmaclab");
    assert_eq!(data["link1"], "100.64.0.11");
    let hosts = rig.file("etc/hosts");
    assert!(
        hosts.contains("10.213.0.11 mox1.pve.internal mox1\n"),
        "{hosts}"
    );
    assert!(!hosts.contains("203.0.113.5"), "{hosts}");
    assert_eq!(
        rig.file("etc/pve/datacenter.cfg"),
        "migration: secure,network=10.213.0.240/28\nreplication: secure,network=10.213.0.240/28\n"
    );
    let text = String::from_utf8(rig.out.0.lock().unwrap().clone()).unwrap();
    assert!(text.contains("Created the cluster bmaclab"), "{text}");
}

#[tokio::test]
async fn datacenter_cfg_waits_for_pmxcfs_to_take_writes() {
    use std::os::unix::fs::PermissionsExt;
    let rig = rig();
    rig.resolves();
    rig.runner.expect(
        PVECM,
        [
            "create",
            "bmaclab",
            "--link0",
            "address=10.213.0.11,priority=100",
            "--link1",
            "address=100.64.0.11,priority=10",
        ],
        Reply::ok(""),
    );
    rig.settles();
    let cfg = rig.root.path().join("etc/pve/datacenter.cfg");
    std::fs::write(&cfg, "").unwrap();
    std::fs::set_permissions(&cfg, std::fs::Permissions::from_mode(0o444)).unwrap();
    let opening = cfg.clone();
    let opener = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::set_permissions(&opening, std::fs::Permissions::from_mode(0o644)).unwrap();
    });
    let mut cluster = rig.cluster();
    cluster.poll = std::time::Duration::from_millis(5);
    cluster.create().await.unwrap();
    opener.join().unwrap();
    assert!(
        rig.file("etc/pve/datacenter.cfg")
            .starts_with("migration: secure")
    );
}

#[tokio::test]
async fn a_resumed_phase_finds_the_cluster_already_made() {
    let rig = rig();
    std::fs::write(
        rig.root.path().join("etc/pve/corosync.conf"),
        "totem {\n  cluster_name: bmaclab\n}\n",
    )
    .unwrap();
    rig.resolves();
    rig.settles();
    rig.cluster().create().await.unwrap();
    rig.runner.assert_done();
}

#[tokio::test]
async fn another_cluster_or_another_name_stops_the_phase() {
    let rig = rig();
    std::fs::write(
        rig.root.path().join("etc/pve/corosync.conf"),
        "totem {\n  cluster_name: other\n}\n",
    )
    .unwrap();
    rig.resolves();
    let error = rig.cluster().create().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("already belongs to the cluster other"),
        "{error:?}"
    );

    let rig = self::rig();
    std::fs::write(rig.root.path().join("etc/hostname"), "pve\n").unwrap();
    let error = rig.cluster().create().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("installed as pve, not mox1"),
        "{error:?}"
    );
    assert_eq!(rig.file("etc/hosts"), INSTALLED);
}

#[tokio::test]
async fn the_hosts_name_must_resolve_to_the_private_address() {
    let rig = rig();
    rig.runner.expect(
        GETENT,
        ["ahostsv4", "mox1"],
        Reply::ok("203.0.113.5     STREAM mox1\n"),
    );
    let error = rig.cluster().create().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("mox1 resolves to 203.0.113.5"),
        "{error:?}"
    );
}

fn stored_settings(root: &std::path::Path, hosts: &[&str]) -> crate::settings::Settings {
    use bmac_pmxcfs::{Pmxcfs, RealFs};
    let pmxcfs = Pmxcfs::new(root, Arc::new(RealFs));
    let config = root.join("config");
    let record = |kind: &str, extra: Value| {
        let mut value = json!({"schema_version": 1, "record_type": kind, "revision": 1,
            "updated_by": "setup", "updated_at_ms": 1});
        for (k, v) in extra.as_object().unwrap() {
            value[k] = v.clone();
        }
        value
    };
    let cluster = json!({
        "PROXMOX_CLUSTER_NAME": "lab", "PROXMOX_INTERNAL_DOMAIN": "pve.internal",
        "MAX_MOX_HOSTS": "3", "PRIVATE_SUBNET_CIDR": "10.213.0.0/24",
        "MOX_IP_START": "10.213.0.11", "MOX_IP_END": "10.213.0.20",
        "MOX_REPLICATION_IP_START": "10.213.0.241", "MOX_REPLICATION_IP_END": "10.213.0.250",
        "PROXMOX_MIGRATION_NETWORK": "10.213.0.240/28",
        "DATACENTER_PRIVATE_VLAN_GATEWAY": "10.213.0.1", "GUEST_EGRESS_VIP": "10.213.0.10/24",
        "HAPROXY_IP_START": "10.213.0.21", "HAPROXY_IP_END": "10.213.0.30",
        "PRODUCTION_IP_START": "10.213.0.31", "PRODUCTION_IP_END": "10.213.0.50",
        "STAGING_IP_START": "10.213.0.51", "STAGING_IP_END": "10.213.0.200",
    });
    pmxcfs
        .write_json(
            &config.join("cluster.json"),
            &record("cluster_settings", json!({ "values": cluster })),
        )
        .unwrap();
    for host in hosts {
        pmxcfs
            .write_json(
                &config.join(format!("hosts/{host}.json")),
                &record("host_facts", json!({"host": host, "values": {}})),
            )
            .unwrap();
    }
    crate::settings::Settings::new(pmxcfs, &config)
}

#[test]
fn every_stored_host_is_pinned_and_a_second_pass_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let settings = stored_settings(&dir.path().join("pve"), &["mox1", "mox2"]);
    let root = dir.path().join("root");
    std::fs::create_dir_all(root.join("etc")).unwrap();
    std::fs::write(
        root.join("etc/hosts"),
        "127.0.0.1 localhost\n203.0.113.5 mox1.pve.internal mox1\n",
    )
    .unwrap();
    assert!(pin_from_settings(&root, &settings).unwrap());
    let text = std::fs::read_to_string(root.join("etc/hosts")).unwrap();
    assert!(
        text.contains("10.213.0.11 mox1.pve.internal mox1\n"),
        "{text}"
    );
    assert!(
        text.contains("10.213.0.12 mox2.pve.internal mox2\n"),
        "{text}"
    );
    assert!(!text.contains("203.0.113.5"));
    assert!(!pin_from_settings(&root, &settings).unwrap());
}

#[test]
fn nothing_is_pinned_before_there_are_settings() {
    let dir = tempfile::tempdir().unwrap();
    let pmxcfs = bmac_pmxcfs::Pmxcfs::new(dir.path(), Arc::new(bmac_pmxcfs::RealFs));
    let settings = crate::settings::Settings::new(pmxcfs, dir.path().join("config"));
    assert!(!pin_from_settings(dir.path(), &settings).unwrap());
}
