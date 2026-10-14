// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The storage parsers against the legacy `host_storage.py` (copied from
//! v2): the same inputs go through it and through this crate, and the JSON
//! must match. The layout cases are its fixtures from `test_host_storage.py`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use bmac_adapters::layout::{self, Texts};
use bmac_adapters::zpool::{self, ConfigNode, PoolStatus, VdevClass};
use bmac_core::PoolName;
use bmac_exec::{FakeRunner, Reply};
use serde_json::{Value, json};

fn v2_tests() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../legacy/tests")
}

/// Runs `program` with v2's storage test module importable, feeding it
/// `input` as JSON, and returns what it prints as JSON.
fn python(program: &str, input: &Value) -> Value {
    let mut child = Command::new("python3")
        .arg("-c")
        .arg(program)
        .arg(v2_tests())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("python3 runs");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

const LAYOUT_CASES: &str = r##"
import json, sys
sys.path.insert(0, sys.argv[1])
import test_host_storage as T
hs = T.host_storage
json.load(sys.stdin)

cases = {}

def case(name, realpaths=T.LUKS_REALPATHS, **overrides):
    args = {
        "status_text": T.LUKS_STATUS,
        "list_text": T.LUKS_LIST,
        "pool_properties_text": T.POOL_PROPERTIES,
        "dataset_properties_text": T.DATASET_PROPERTIES,
        "lsblk_text": T.LUKS_LSBLK,
        "boot_uuids_text": "# managed by proxmox-boot-tool\nAAAA-1111\nBBBB-2222\n",
        "volume_text": T.VOLUMES,
        "snapshot_text": T.SNAPSHOTS,
        "crypttab_text": "",
        "all_pools_status_text": None,
    }
    args.update(overrides)
    layout = hs.build_layout(
        realpath=lambda path: realpaths.get(path, path),
        hostname="mox1", collected_at=T.NOW, **args,
    )
    cases[name] = {
        "inputs": args,
        "realpaths": realpaths,
        "layout": layout,
        "attention": hs.attention_items(layout, ()),
    }

case("luks")
case("crypttab", crypttab_text=(
    "# comment\n"
    "crypt-rpool-a\tUUID=1\tbmac-rpool\tluks,initramfs,nofail,keyscript=decrypt_keyctl\n"
    "crypt-rpool-mirror3-1 UUID=2 none luks,initramfs,nofail\n"
    "cryptswap UUID=3 none swap\n"
))
case("prepared_pair_not_added", status_text=T.LUKS_STATUS.replace(
    "\t  mirror-2                             ONLINE       0     0     0\n"
    "\t    /dev/mapper/crypt-rpool-mirror2-1  ONLINE       0     0     0\n"
    "\t    /dev/mapper/crypt-rpool-mirror2-2  ONLINE       0     0     0\n",
    "",
))
case("healthy", status_text=T.LUKS_STATUS.replace(
    "remove: Evacuation of mirror-1 in progress since Wed Sep 24 10:00:00 2026\n"
    "\t21.5G copied out of 40.0G at 120M/s, 53.75% done, 0h2m to go\n"
    "\t4.21M memory used for removed device mappings\n",
    "",
))
case("unresolved_and_no_esp", realpaths={}, boot_uuids_text="")

devices = json.loads(T.LUKS_LSBLK)
devices["blockdevices"].append(T.boot_disk(8, "S8NEWBOOT", "CCCC-3333", "crypt-rpool-unused"))
case("esp_only", lsblk_text=json.dumps(devices),
     boot_uuids_text="AAAA-1111\nBBBB-2222\nCCCC-3333\n")

clear_status = """\
  pool: rpool
 state: ONLINE
config:

\tNAME                                  STATE     READ WRITE CKSUM
\trpool                                 ONLINE       0     0     0
\t  mirror-0                            ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-SAMSUNG_S0BOOTA-part3  ONLINE       0     0     0
\t    /dev/disk/by-id/nvme-SAMSUNG_S1BOOTB-part3  ONLINE       0     0     0

errors: No known data errors
"""
clear_lsblk = json.dumps({"blockdevices": [
    T.disk("/dev/nvme0n1", "S0BOOTA", 2000398934016, children=[
        T.part("/dev/nvme0n1p2", fstype="vfat", uuid="AAAA-1111"),
        T.part("/dev/nvme0n1p3", fstype="zfs_member"),
    ]),
    T.disk("/dev/nvme1n1", "S1BOOTB", 2000398934016, children=[
        T.part("/dev/nvme1n1p2", fstype="vfat", uuid="BBBB-2222"),
        T.part("/dev/nvme1n1p3", fstype="zfs_member"),
    ]),
]})
case("clear", status_text=clear_status, list_text="", lsblk_text=clear_lsblk,
     volume_text="", snapshot_text="", realpaths={
        "/dev/disk/by-id/nvme-SAMSUNG_S0BOOTA-part3": "/dev/nvme0n1p3",
        "/dev/disk/by-id/nvme-SAMSUNG_S1BOOTB-part3": "/dev/nvme1n1p3",
     })

tank = """\
  pool: tank
 state: ONLINE
config:

\tNAME            STATE     READ WRITE CKSUM
\ttank            ONLINE       0     0     0
\t  /dev/nvme6n1  ONLINE       0     0     0

errors: No known data errors
"""
case("second_pool", all_pools_status_text=T.LUKS_STATUS + "\n" + tank)

json.dump(cases, sys.stdout)
"##;

const PARSE_STATUS: &str = r#"
import json, sys
sys.path.insert(0, sys.argv[1])
import test_host_storage as T
json.dump([T.host_storage.parse_zpool_status(text) for text in json.load(sys.stdin)], sys.stdout)
"#;

/// The layout without what v3 adds for its storage workflows: each disk's
/// and member's identity, and a member's replacing or spare group.
fn without_v3_fields(mut value: Value) -> Value {
    match &mut value {
        Value::Object(map) => {
            map.remove("identity");
            map.remove("group");
            for v in map.values_mut() {
                *v = without_v3_fields(v.take());
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                *v = without_v3_fields(v.take());
            }
        }
        _ => {}
    }
    value
}

fn text<'a>(inputs: &'a Value, key: &str) -> &'a str {
    inputs[key].as_str().unwrap_or_default()
}

#[test]
fn layouts_match_v2_for_its_fixtures() {
    let cases = python(LAYOUT_CASES, &json!(null));
    let cases = cases.as_object().unwrap();
    assert_eq!(cases.len(), 8);
    for (name, case) in cases {
        let inputs = &case["inputs"];
        let realpaths: BTreeMap<String, String> =
            serde_json::from_value(case["realpaths"].clone()).unwrap();
        let realpath = |path: &str| realpaths.get(path).cloned().unwrap_or(path.to_string());
        let texts = Texts {
            status: text(inputs, "status_text"),
            list: text(inputs, "list_text"),
            pool_properties: text(inputs, "pool_properties_text"),
            dataset_properties: text(inputs, "dataset_properties_text"),
            lsblk: text(inputs, "lsblk_text"),
            boot_uuids: text(inputs, "boot_uuids_text"),
            volumes: text(inputs, "volume_text"),
            snapshots: text(inputs, "snapshot_text"),
            crypttab: text(inputs, "crypttab_text"),
            all_pools: inputs["all_pools_status_text"].as_str(),
        };
        let ours = layout::build_from_texts(&texts, &realpath, "mox1", 1758704200).unwrap();
        assert_eq!(
            without_v3_fields(serde_json::to_value(&ours).unwrap()),
            case["layout"],
            "layout case {name}"
        );
        let v2_attention: Vec<String> = case["attention"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                item.as_str().unwrap().replace(
                    "scripts/user_callable/hosts/add_replacement_disk.sh",
                    "the Replace disk operation",
                )
            })
            .collect();
        assert_eq!(
            layout::attention_items(&ours),
            v2_attention,
            "attention case {name}"
        );
    }
}

/// `parse_zpool_status`'s shape, from this crate's tree.
fn v2_shape(status: &PoolStatus) -> Value {
    let removing = status.removing_vdev();
    let vdev = |node: &ConfigNode| {
        json!({
            "name": node.name,
            "type": node.vdev_type(),
            "state": node.state,
            "removing": removing.as_deref() == Some(node.name.as_str()),
            "replacing": node.replacing(),
            "members": node.leaves().iter()
                .map(|leaf| json!({"path": leaf.name, "state": leaf.state}))
                .collect::<Vec<_>>(),
        })
    };
    let auxiliary: serde_json::Map<String, Value> = status
        .sections
        .iter()
        .filter(|section| section.class != VdevClass::Data)
        .map(|section| {
            (
                section.class.as_str().to_string(),
                section.vdevs.iter().map(vdev).collect(),
            )
        })
        .collect();
    json!({
        "pool": status.pool,
        "state": status.state(),
        "status": status.status(),
        "scan": status.scan(),
        "remove": status.remove(),
        "removal_in_progress": removing.is_some(),
        "errors": status.errors(),
        "vdevs": status.vdevs(VdevClass::Data).iter().map(vdev).collect::<Vec<_>>(),
        "auxiliary": auxiliary,
    })
}

/// Every vdev type and allocation class, in OpenZFS's `zpool status -P -p`
/// layout.
const RAIDZ_WITH_CLASSES: &str = "  pool: tank
 state: DEGRADED
status: One or more devices could not be used because the label is missing or
\tinvalid.  Sufficient replicas exist for the pool to continue
\tfunctioning in a degraded state.
action: Replace the device using 'zpool replace'.
   see: https://openzfs.github.io/openzfs-docs/msg/ZFS-8000-4J
  scan: resilvered 1.21G in 00:00:09 with 0 errors on Sun Oct 11 09:00:00 2026
config:

\tNAME                         STATE     READ WRITE CKSUM
\ttank                         DEGRADED     0     0     0
\t  raidz2-0                   DEGRADED     0     0     0
\t    /dev/disk/by-id/ata-A-part1  ONLINE       0     0     0
\t    /dev/disk/by-id/ata-B-part1  ONLINE       0     0     0
\t    spare-2                  DEGRADED     0     0     0
\t      /dev/disk/by-id/ata-C-part1  UNAVAIL      0     0     0  corrupted data
\t      /dev/disk/by-id/ata-F-part1  ONLINE       0     0     0
\t    /dev/disk/by-id/ata-D-part1  ONLINE       0     0     3
\t  draid1:2d:4c:0s-1          ONLINE       0     0     0
\t    /dev/sdg1                ONLINE       0     0     0
\t    /dev/sdh1                ONLINE       0     0     0
\t    /dev/sdi1                ONLINE       0     0     0
\t    /dev/sdj1                ONLINE       0     0     0
\tdedup\t
\t  mirror-4                   ONLINE       0     0     0
\t    /dev/nvme4n1p1           ONLINE       0     0     0
\t    /dev/nvme5n1p1           ONLINE       0     0     0
\tspecial\t
\t  mirror-2                   ONLINE       0     0     0
\t    /dev/nvme0n1p1           ONLINE       0     0     0
\t    /dev/nvme1n1p1           ONLINE       0     0     0
\tlogs\t
\t  mirror-3                   ONLINE       0     0     0
\t    /dev/nvme2n1p1           ONLINE       0     0     0
\t    /dev/nvme3n1p1           ONLINE       0     0     0
\tcache
\t  /dev/nvme6n1p1             ONLINE       0     0     0
\tspares
\t  /dev/disk/by-id/ata-F-part1  INUSE     currently in use
\t  /dev/disk/by-id/ata-G-part1  AVAIL

errors: No known data errors
";

const RESILVERING: &str = "  pool: rpool
 state: DEGRADED
status: One or more devices is currently being resilvered.  The pool will
\tcontinue to function, possibly in a degraded state.
action: Wait for the resilver to complete.
  scan: resilver in progress since Wed Sep 24 09:00:00 2026
\t12.0G / 40.0G scanned, 3.00G / 40.0G issued at 300M/s
\t3.00G resilvered, 7.50% done, 00:02:06 to go
config:

\tNAME                                           STATE     READ WRITE CKSUM
\trpool                                          DEGRADED     0     0     0
\t  mirror-0                                     DEGRADED     0     0     0
\t    /dev/disk/by-id/nvme-A-part3               ONLINE       0     0     0
\t    replacing-1                                DEGRADED     0     0     0
\t      /dev/disk/by-id/nvme-B-part3             FAULTED      7     0     0  too many errors
\t      /dev/disk/by-id/nvme-C-part3             ONLINE       0     0     0  (resilvering)
\t  /dev/disk/by-id/nvme-D                       ONLINE       0     0     0
\tlogs
\t  /dev/disk/by-id/nvme-E                       ONLINE       0     0     0

errors: No known data errors
";

const SCRUB_FOUND_ERRORS: &str = "  pool: rpool
 state: ONLINE
status: One or more devices has experienced an unrecoverable error.
  scan: scrub repaired 0B in 00:04:12 with 2 errors on Sun Sep 14 00:28:13 2026
config:

\tNAME            STATE     READ WRITE CKSUM
\trpool           ONLINE       0     0     0
\t  /dev/nvme0n1  ONLINE       0     0     0

errors: 2 data errors, use '-v' for a list
";

#[test]
fn status_parse_matches_v2_for_every_vdev_class() {
    let texts = [RAIDZ_WITH_CLASSES, RESILVERING, SCRUB_FOUND_ERRORS];
    let expected = python(PARSE_STATUS, &json!(texts));
    for (text, expected) in texts.iter().zip(expected.as_array().unwrap()) {
        let ours = zpool::parse_status(text).unwrap();
        assert_eq!(&v2_shape(&ours), expected, "{text}");
    }
    let raidz = zpool::parse_status(RAIDZ_WITH_CLASSES).unwrap();
    let types: Vec<String> = raidz
        .vdevs(VdevClass::Data)
        .iter()
        .map(ConfigNode::vdev_type)
        .collect();
    assert_eq!(types, ["raidz2", "draid1:2d:4c:0s"]);
    assert_eq!(
        raidz.sections.iter().map(|s| s.class).collect::<Vec<_>>(),
        [
            VdevClass::Data,
            VdevClass::Dedup,
            VdevClass::Special,
            VdevClass::Logs,
            VdevClass::Cache,
            VdevClass::Spares
        ]
    );
}

/// Captured from Proxmox's OpenZFS by `lab/lab scenario m2_cli` with
/// `BMAC_LAB_CAPTURE_FIXTURES=1`: a raidz2 of four with every other vdev
/// class, healthy, and with a member faulted and the event daemon's hot
/// spare resilvered in.
const LAB_STATUS_HEALTHY: &str = include_str!("fixtures/lab/zpool_status_healthy.txt");
const LAB_STATUS_DEGRADED: &str = include_str!("fixtures/lab/zpool_status_degraded.txt");
const LAB_LIST_HEALTHY: &str = include_str!("fixtures/lab/zpool_list_healthy.txt");
const LAB_LIST_DEGRADED: &str = include_str!("fixtures/lab/zpool_list_degraded.txt");

/// The command's output, after the license header the capture puts first.
fn captured(file: &str) -> &str {
    file.split_once("\n\n")
        .expect("a header, then the output")
        .1
}

#[test]
fn lab_captured_pools_parse_as_v2_parses_them() {
    let texts = [captured(LAB_STATUS_HEALTHY), captured(LAB_STATUS_DEGRADED)];
    let expected = python(PARSE_STATUS, &json!(texts));
    for (text, expected) in texts.iter().zip(expected.as_array().unwrap()) {
        let ours = zpool::parse_status(text).unwrap();
        assert_eq!(&v2_shape(&ours), expected, "{text}");
        assert_eq!(
            ours.sections.iter().map(|s| s.class).collect::<Vec<_>>(),
            [
                VdevClass::Data,
                VdevClass::Dedup,
                VdevClass::Special,
                VdevClass::Logs,
                VdevClass::Cache,
                VdevClass::Spares
            ]
        );
        let data = ours.vdevs(VdevClass::Data);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].vdev_type(), "raidz2");
        assert_eq!(
            data[0].children.len(),
            4,
            "a spare group stands for one member"
        );
    }

    let healthy = zpool::parse_status(texts[0]).unwrap();
    assert!(healthy.findings(&no_disk, None).is_empty());
    let degraded = zpool::parse_status(texts[1]).unwrap();
    assert_eq!(
        degraded.findings(&no_disk, None),
        [
            "pool fixture is DEGRADED",
            "fixture member /dev/loop2 (?) is FAULTED (external device fault)",
        ],
        "the spare standing in and the INUSE spare are fine"
    );

    let rows = zpool::parse_list(captured(LAB_LIST_HEALTHY));
    assert_eq!(rows["fixture"].health.as_deref(), Some("ONLINE"));
    assert!(rows["fixture"].size.is_some());
    assert_eq!(rows["/dev/loop10"].size, Some(1 << 30), "the cache device");
    assert!(!rows.contains_key("dedup"), "class headings are not rows");
    let rows = zpool::parse_list(captured(LAB_LIST_DEGRADED));
    assert_eq!(rows["/dev/loop2"].health.as_deref(), Some("FAULTED"));
    assert_eq!(rows["spare-2"].health.as_deref(), Some("DEGRADED"));
    assert_eq!(
        rows["/dev/loop11"].health.as_deref(),
        Some("INUSE"),
        "the spare's row under `spare` comes last"
    );
    assert_eq!(rows["/dev/loop11"].size, Some(1 << 30));
    assert_eq!(rows["/dev/loop12"].health.as_deref(), Some("AVAIL"));
}

fn no_disk(_: &str) -> Option<String> {
    None
}

#[test]
fn health_findings_follow_v2s_pool_rules() {
    let raidz = zpool::parse_status(RAIDZ_WITH_CLASSES).unwrap();
    let describe =
        |path: &str| (path == "/dev/disk/by-id/ata-D-part1").then(|| "sdd SERIALD".to_string());
    assert_eq!(
        raidz.findings(&describe, None),
        [
            "pool tank is DEGRADED",
            "tank member /dev/disk/by-id/ata-C-part1 (?) is UNAVAIL (corrupted data)",
            "tank member /dev/disk/by-id/ata-D-part1 (sdd SERIALD) has read/write/checksum errors 0/0/3",
        ],
        "DEGRADED vdevs are explained by their members; INUSE and AVAIL spares are fine"
    );

    let resilvering = zpool::parse_status(RESILVERING).unwrap();
    assert_eq!(
        resilvering.findings(&no_disk, None),
        [
            "pool rpool is DEGRADED",
            "rpool member /dev/disk/by-id/nvme-B-part3 (?) is FAULTED (too many errors)",
            "rpool member /dev/disk/by-id/nvme-B-part3 (?) has read/write/checksum errors 7/0/0",
            "pool rpool is resilvering; redundancy is reduced until it finishes",
        ]
    );

    let scrubbed = zpool::parse_status(SCRUB_FOUND_ERRORS).unwrap();
    assert_eq!(
        scrubbed.findings(&no_disk, Some("pool 'rpool' is healthy")),
        [
            "pool rpool reports data errors: 2 data errors, use '-v' for a list",
            "the last scan of pool rpool found 2 errors",
        ]
    );

    let quiet = zpool::parse_status(
        &SCRUB_FOUND_ERRORS
            .replace("with 2 errors", "with 0 errors")
            .replace("2 data errors, use '-v' for a list", "No known data errors"),
    )
    .unwrap();
    assert!(
        quiet
            .findings(&no_disk, Some("pool 'rpool' is healthy"))
            .is_empty()
    );
    assert_eq!(
        quiet.findings(
            &no_disk,
            Some(
                "  pool: rpool\n state: ONLINE\nstatus: Some supported features are not enabled.\n"
            )
        ),
        [
            "zpool status -x reports pool rpool needs attention: One or more devices has experienced an unrecoverable error."
        ]
    );
}

const ZPOOL: &str = "/usr/sbin/zpool";
const ZFS: &str = "/usr/sbin/zfs";
const LSBLK_ARGS: [&str; 5] = [
    "-J",
    "-b",
    "-p",
    "-o",
    "NAME,KNAME,TYPE,SIZE,SERIAL,MODEL,WWN,TRAN,FSTYPE,UUID,MOUNTPOINTS",
];

struct Files;

impl layout::HostFiles for Files {
    fn read(&self, path: &std::path::Path) -> Option<String> {
        (path == std::path::Path::new(layout::BOOT_UUIDS_FILE)).then(|| "AAAA-1111\n".to_string())
    }
    fn realpath(&self, path: &str) -> String {
        path.replace("/dev/disk/by-id/nvme-S0-part3", "/dev/nvme0n1p3")
    }
    fn hostname(&self) -> String {
        "mox1".into()
    }
    fn now(&self) -> i64 {
        1758704200
    }
}

const ONE_DISK_LSBLK: &str = r#"{"blockdevices":[{"name":"/dev/nvme0n1","kname":"/dev/nvme0n1",
    "type":"disk","size":1000204886016,"serial":"S0","children":[
      {"name":"/dev/nvme0n1p2","kname":"/dev/nvme0n1p2","type":"part","uuid":"AAAA-1111"},
      {"name":"/dev/nvme0n1p3","kname":"/dev/nvme0n1p3","type":"part"}]}]}"#;

#[tokio::test]
async fn collect_runs_only_read_only_queries() {
    let status = "  pool: rpool\n state: ONLINE\nconfig:\n\n\
        \tNAME                              STATE     READ WRITE CKSUM\n\
        \trpool                             ONLINE       0     0     0\n\
        \t  /dev/disk/by-id/nvme-S0-part3   ONLINE       0     0     0\n\n\
        errors: No known data errors\n";
    let fake = FakeRunner::new();
    fake.expect("/usr/bin/lsblk", LSBLK_ARGS, Reply::ok(ONE_DISK_LSBLK))
        .expect(ZPOOL, ["status", "-P", "-p", "rpool"], Reply::ok(status))
        .expect(ZPOOL, ["status", "-L", "-P", "-p"], Reply::ok(status))
        .expect(
            ZPOOL,
            [
                "list",
                "-v",
                "-H",
                "-p",
                "-P",
                "-o",
                "name,size,allocated,free,health",
                "rpool",
            ],
            Reply::ok("rpool\t990000000000\t1000\t989999999000\tONLINE\n"),
        )
        .expect(
            ZPOOL,
            [
                "get",
                "-H",
                "-p",
                "-o",
                "property,value",
                "size,allocated,free,capacity,fragmentation,health",
                "rpool",
            ],
            Reply::ok("size\t990000000000\ncapacity\t0\nhealth\tONLINE\n"),
        )
        .expect(
            ZFS,
            [
                "get",
                "-H",
                "-p",
                "-o",
                "property,value",
                "available,used",
                "rpool",
            ],
            Reply::ok("available\t950000000000\nused\t1000\n"),
        )
        .expect(
            ZFS,
            [
                "list",
                "-H",
                "-p",
                "-t",
                "volume",
                "-r",
                "-o",
                "name,volsize,used,refreservation,usedbysnapshots",
                "rpool",
            ],
            Reply::ok(""),
        )
        .expect(
            ZFS,
            [
                "list",
                "-H",
                "-p",
                "-t",
                "snapshot",
                "-r",
                "-o",
                "name,used,creation",
                "rpool",
            ],
            Reply::ok(""),
        );
    let pool: PoolName = "rpool".parse().unwrap();
    let ours = layout::collect(&fake, &Files, &pool, false).await.unwrap();
    fake.assert_done();
    assert_eq!(ours.vdevs[0].members[0].serial.as_deref(), Some("S0"));
    assert!(ours.vdevs[0].holds_esp);
    assert_eq!(ours.pool.dataset_available, Some(950000000000));
    assert!(layout::attention_items(&ours).is_empty());
}

#[tokio::test]
async fn collect_inventories_disks_before_the_pool_exists() {
    let fake = FakeRunner::new();
    fake.expect("/usr/bin/lsblk", LSBLK_ARGS, Reply::ok(ONE_DISK_LSBLK))
        .expect(
            ZPOOL,
            ["status", "-P", "-p", "rpool"],
            Reply::fail(1, "cannot open 'rpool': no such pool\n"),
        );
    let pool: PoolName = "rpool".parse().unwrap();
    assert!(layout::collect(&fake, &Files, &pool, false).await.is_err());

    let fake = FakeRunner::new();
    fake.expect("/usr/bin/lsblk", LSBLK_ARGS, Reply::ok(ONE_DISK_LSBLK))
        .expect(
            ZPOOL,
            ["status", "-P", "-p", "rpool"],
            Reply::fail(1, "cannot open 'rpool': no such pool\n"),
        );
    let ours = layout::collect(&fake, &Files, &pool, true).await.unwrap();
    fake.assert_done();
    assert!(ours.vdevs.is_empty());
    assert_eq!(ours.pool.state.as_deref(), Some("UNAVAILABLE"));
    assert_eq!(ours.pool.errors.as_deref(), Some("no pool is imported"));
    assert_eq!(ours.esp_only_disks.len(), 1, "its ESP is registered");
    assert_eq!(
        ours.physically_removable_disks
            .iter()
            .map(|d| d.serial.as_deref())
            .collect::<Vec<_>>(),
        Vec::<Option<&str>>::new(),
        "a registered ESP keeps the disk in use"
    );
}
