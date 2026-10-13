// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::io::Write;
use std::sync::Mutex;

use bmac_exec::{FakeRunner, Reply};
use bmac_inventory::nics::LeaveAsIs;
use tokio::io::AsyncWriteExt;

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

fn ip(s: &str) -> Ipv4Addr {
    s.parse().unwrap()
}

fn plan(route: bool) -> NetworkPlan {
    NetworkPlan {
        bridge: "vmbr-private".into(),
        addresses: BridgeAddresses {
            private: (ip("10.213.0.11"), 24),
            replication: (ip("10.213.0.241"), 28),
            gateway: route.then(|| ip("10.213.0.1")),
            gateway_route: route,
        },
        public_mac: "AA:00:00:00:00:01".into(),
        public_ip: ip("203.0.113.5"),
        private_mac: "aa:00:00:00:00:02".into(),
    }
}

/// A host with a public NIC in vmbr0 and `spare` more NICs, the first
/// being the private one; each spare's link is `(carrier, speed)`.
fn host(spare: &[(bool, &str)]) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let net = root.path().join("sys/class/net");
    let nic = |name: &str, mac: &str| {
        let dir = net.join(name);
        std::fs::create_dir_all(dir.join("device")).unwrap();
        std::fs::write(dir.join("type"), "1\n").unwrap();
        std::fs::write(dir.join("address"), format!("{mac}\n")).unwrap();
        dir
    };
    std::fs::create_dir_all(net.join("vmbr0/bridge")).unwrap();
    let public = nic("eno1", "aa:00:00:00:00:01");
    std::os::unix::fs::symlink("../vmbr0", public.join("master")).unwrap();
    std::fs::write(public.join("carrier"), "1\n").unwrap();
    for (i, (carrier, speed)) in spare.iter().enumerate() {
        let dir = nic(
            &format!("enp{}s0", i + 2),
            &format!("aa:00:00:00:00:0{}", i + 2),
        );
        std::fs::write(dir.join("carrier"), if *carrier { "1\n" } else { "0\n" }).unwrap();
        std::fs::write(dir.join("speed"), format!("{speed}\n")).unwrap();
        std::fs::write(dir.join("duplex"), "full\n").unwrap();
    }
    std::fs::create_dir_all(root.path().join("etc/network")).unwrap();
    std::fs::write(
        root.path().join("etc/network/interfaces"),
        "auto lo\niface lo inet loopback\n",
    )
    .unwrap();
    root
}

fn addresses_json(rows: &[(&str, &str, u8)]) -> String {
    let mut by: Vec<(String, Vec<Value>)> = Vec::new();
    for (name, address, prefix) in rows {
        let info = json!({ "family": "inet", "local": address, "prefixlen": prefix });
        match by.iter_mut().find(|(n, _)| n == name) {
            Some((_, infos)) => infos.push(info),
            None => by.push((name.to_string(), vec![info])),
        }
    }
    let rows: Vec<Value> = by
        .into_iter()
        .map(|(name, infos)| json!({ "ifname": name, "addr_info": infos }))
        .collect();
    serde_json::to_string(&rows).unwrap()
}

fn expect_addresses(runner: &FakeRunner, rows: &[(&str, &str, u8)]) {
    runner.expect(
        IP,
        ["-j", "-4", "address", "show"],
        Reply::ok(addresses_json(rows)),
    );
}

struct Rig {
    root: tempfile::TempDir,
    _work: tempfile::TempDir,
    runner: FakeRunner,
    flow: Workflow,
    out: Sink,
    input: tokio::io::DuplexStream,
}

fn rig(spare: &[(bool, &str)]) -> Rig {
    let work = tempfile::tempdir().unwrap();
    let out = Sink::default();
    let (input, theirs) = tokio::io::duplex(4096);
    let flow = Workflow::new(
        Box::new(out.clone()),
        tokio::io::BufReader::new(theirs),
        work.path().to_path_buf(),
    );
    Rig {
        root: host(spare),
        _work: work,
        runner: FakeRunner::new(),
        flow,
        out,
        input,
    }
}

impl Rig {
    fn network<'a>(&'a self, plan: &'a NetworkPlan) -> Network<'a> {
        Network {
            flow: &self.flow,
            runner: &self.runner,
            root: self.root.path().to_path_buf(),
            plan,
            link_wait: LinkWait {
                attempts: 1,
                interval_seconds: 0,
            },
            links: Arc::new(LeaveAsIs),
        }
    }

    async fn answer(&mut self, confirmed: bool) {
        self.input
            .write_all(
                format!(
                    "{{\"type\":\"response\",\"request_id\":\"r1\",\"confirmed\":{confirmed}}}\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
    }

    fn file(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.root.path().join(path.trim_start_matches('/'))).ok()
    }

    fn output(&self) -> String {
        String::from_utf8(self.out.0.lock().unwrap().clone()).unwrap()
    }
}

const PUBLIC: (&str, &str, u8) = ("vmbr0", "203.0.113.5", 24);
const UP: [(&str, &str, u8); 3] = [
    PUBLIC,
    ("vmbr-private", "10.213.0.11", 24),
    ("vmbr-private", "10.213.0.241", 28),
];

#[test]
fn addresses_are_read_from_ip_json() {
    let json = r#"[{"ifindex":1,"ifname":"lo","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8}]},
                   {"ifname":"vmbr0","addr_info":[{"family":"inet6","local":"fe80::1","prefixlen":64},
                                                  {"family":"inet","local":"203.0.113.5","prefixlen":24}]}]"#;
    assert_eq!(
        parse_addresses(json).unwrap(),
        vec![
            Address {
                interface: "lo".into(),
                ip: ip("127.0.0.1"),
                prefix: 8
            },
            Address {
                interface: "vmbr0".into(),
                ip: ip("203.0.113.5"),
                prefix: 24
            },
        ]
    );
}

#[test]
fn the_bridge_must_hold_both_addresses_and_be_alone_in_the_replication_network() {
    let expected = plan(false).addresses;
    let rows = |rows: &[(&str, &str, u8)]| parse_addresses(&addresses_json(rows)).unwrap();
    assert!(address_problems(&rows(&UP), "vmbr-private", &expected).is_empty());

    let missing = address_problems(&rows(&UP[..2]), "vmbr-private", &expected);
    assert_eq!(
        missing,
        vec!["vmbr-private doesn't have its replication address 10.213.0.241/28."]
    );

    let mut crowded = UP.to_vec();
    crowded.push(("vmbr1", "10.213.0.242", 28));
    let problems = address_problems(&rows(&crowded), "vmbr-private", &expected);
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].starts_with("vmbr1 has 10.213.0.242/28"));
}

#[test]
fn the_stanza_adds_the_gateway_route_only_when_asked() {
    let plain = bridge_stanza("vmbr-private", "enp2s0", &plan(false).addresses);
    assert!(plain.contains("auto vmbr-private\niface vmbr-private inet static\n"));
    assert!(plain.contains("\taddress 10.213.0.11/24\n\taddress 10.213.0.241/28\n"));
    assert!(plain.contains("\tbridge-ports enp2s0\n\tbridge-stp off\n\tbridge-fd 0\n"));
    assert!(!plain.contains("route"));
    let routed = bridge_stanza("vmbr-private", "enp2s0", &plan(true).addresses);
    assert!(routed.contains(
        "\tpost-up /usr/sbin/ip route replace default via 10.213.0.1 dev vmbr-private metric 1000\n"
    ));
    assert!(routed.contains("\tpre-down /usr/sbin/ip route del default via 10.213.0.1"));
}

#[test]
fn interfaces_must_source_the_directory() {
    assert!(sources_directory(
        "auto lo\nsource /etc/network/interfaces.d/*\n"
    ));
    assert!(sources_directory("source interfaces.d/*"));
    assert!(!sources_directory("# source /etc/network/interfaces.d/*\n"));
    assert!(!sources_directory("source /etc/network/other\n"));
}

#[tokio::test]
async fn the_private_nic_is_bridged_with_its_addresses_after_confirmation() {
    let mut rig = rig(&[(true, "10000")]);
    let plan = plan(false);
    expect_addresses(&rig.runner, &[PUBLIC]);
    rig.runner.expect(IFRELOAD, ["-a"], Reply::ok(""));
    expect_addresses(&rig.runner, &UP);
    rig.answer(true).await;
    let data = rig.network(&plan).run_phase().await.unwrap();
    rig.runner.assert_done();
    assert_eq!(
        data,
        json!({
            "public_nic": "eno1",
            "public_bridge": "vmbr0",
            "private_nic": "enp2s0",
            "bridge": "vmbr-private",
        })
    );
    assert_eq!(
        rig.file(BRIDGE_FILE).unwrap(),
        bridge_stanza("vmbr-private", "enp2s0", &plan.addresses)
    );
    let interfaces = rig.file(INTERFACES).unwrap();
    assert!(interfaces.starts_with("auto lo\niface lo inet loopback\n"));
    assert!(sources_directory(&interfaces));
    let mode = std::fs::metadata(rig.root.path().join(BRIDGE_FILE.trim_start_matches('/')))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o644);
    let text = rig.output();
    assert!(
        text.contains("enp2s0 (aa:00:00:00:00:02) has a 10 Gb/s link"),
        "{text}"
    );
    assert!(text.contains(CONFIRM_TEXT), "{text}");
}

#[tokio::test]
async fn a_slow_private_link_stops_the_phase_before_anything_is_written() {
    let rig = rig(&[(true, "1000")]);
    let plan = plan(false);
    expect_addresses(&rig.runner, &[PUBLIC]);
    let error = rig.network(&plan).run_phase().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("enp2s0 has a link at 1000 Mb/s, not the expected 10000"),
        "{error:?}"
    );
    rig.runner.assert_done();
    assert!(rig.file(BRIDGE_FILE).is_none());
}

#[tokio::test]
async fn the_public_bridge_must_own_the_public_address() {
    let rig = rig(&[(true, "10000")]);
    let plan = plan(false);
    expect_addresses(&rig.runner, &[("vmbr0", "203.0.113.9", 24)]);
    let error = rig.network(&plan).run_phase().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("doesn't have PROXMOX_IP 203.0.113.5"),
        "{error:?}"
    );
}

#[tokio::test]
async fn auto_picks_the_only_spare_nic_with_carrier() {
    let mut rig = rig(&[(false, "-1"), (true, "10000")]);
    let plan = NetworkPlan {
        private_mac: "auto".into(),
        ..plan(false)
    };
    expect_addresses(&rig.runner, &[PUBLIC]);
    rig.runner.expect(IFRELOAD, ["-a"], Reply::ok(""));
    expect_addresses(&rig.runner, &UP);
    rig.answer(true).await;
    let data = rig.network(&plan).run_phase().await.unwrap();
    assert_eq!(data["private_nic"], "enp3s0");

    let rig = self::rig(&[(true, "10000"), (true, "10000")]);
    expect_addresses(&rig.runner, &[PUBLIC]);
    let error = rig.network(&plan).run_phase().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("2 spare NICs have carrier"),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_gateway_route_must_lose_to_the_public_one() {
    let mut rig = rig(&[(true, "10000")]);
    let plan = plan(true);
    let defaults = json!([
        { "dst": "default", "gateway": "203.0.113.1", "dev": "vmbr0" },
        { "dst": "default", "gateway": "10.213.0.1", "dev": "vmbr-private", "metric": 1000 },
    ]);
    expect_addresses(&rig.runner, &[PUBLIC]);
    rig.runner.expect(IFRELOAD, ["-a"], Reply::ok(""));
    expect_addresses(&rig.runner, &UP);
    rig.runner.expect(
        IP,
        ["-j", "route", "show", "default"],
        Reply::ok(defaults.to_string()),
    );
    rig.runner.expect(
        IP,
        ["-j", "route", "get", PROBE],
        Reply::ok(r#"[{"dst":"1.1.1.1","gateway":"203.0.113.1","dev":"vmbr0"}]"#),
    );
    rig.answer(true).await;
    rig.network(&plan).run_phase().await.unwrap();
    rig.runner.assert_done();
    assert!(rig.output().contains("behind the public one"));

    let rig = self::rig(&[(true, "10000")]);
    expect_addresses(&rig.runner, &[PUBLIC]);
    rig.runner.expect(IFRELOAD, ["-a"], Reply::ok(""));
    expect_addresses(&rig.runner, &UP);
    rig.runner.expect(
        IP,
        ["-j", "route", "show", "default"],
        Reply::ok(json!([defaults[0]]).to_string()),
    );
    let error = rig.network(&plan).run_phase().await.unwrap_err();
    assert!(format!("{error:?}").contains("isn't in place"), "{error:?}");
}

#[tokio::test]
async fn an_unconfirmed_vlan_fails_the_phase() {
    let mut rig = rig(&[(true, "10000")]);
    let plan = plan(false);
    expect_addresses(&rig.runner, &[PUBLIC]);
    rig.runner.expect(IFRELOAD, ["-a"], Reply::ok(""));
    expect_addresses(&rig.runner, &UP);
    rig.answer(false).await;
    let error = rig.network(&plan).run_phase().await.unwrap_err();
    assert!(
        format!("{error:?}").contains("wasn't confirmed"),
        "{error:?}"
    );
}
