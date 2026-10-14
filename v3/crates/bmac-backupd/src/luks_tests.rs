// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use bmac_exec::{FakeRunner, Reply};

use super::*;
use crate::terminal::Scripted;

const PASS: &str = "correct horse battery";

fn lsblk(device: &str, extra: &str) -> String {
    format!(
        r#"{{"blockdevices":[{{"name":"{device}","type":"disk","size":10737418240,"model":"QEMU HARDDISK ","serial":"S1","fstype":null,"mountpoints":[null]{extra}}}]}}"#
    )
}

fn expect_disk(fake: &FakeRunner, id: &str, device: &str, extra: &str) {
    fake.expect(
        LSBLK,
        [
            "-J",
            "-b",
            "-p",
            "-o",
            "NAME,TYPE,SIZE,MODEL,SERIAL,FSTYPE,MOUNTPOINTS",
            id,
        ],
        Reply::ok(lsblk(device, extra)),
    );
    fake.expect(ZPOOL, ["status", "-LP"], Reply::ok("no pools available\n"));
}

fn no_pool(fake: &FakeRunner) {
    fake.expect(
        ZPOOL,
        ["list", "-Hp", "-o", "name,guid"],
        Reply::ok("rpool\t42\n"),
    );
}

#[tokio::test]
async fn create_encrypts_every_disk_then_builds_the_pool_on_the_mappings() {
    let fake = FakeRunner::new();
    no_pool(&fake);
    let disks = ["/dev/disk/by-id/ata-A", "/dev/disk/by-id/ata-B"];
    expect_disk(&fake, disks[0], "/dev/sdb", "");
    expect_disk(
        &fake,
        disks[1],
        "/dev/sdc",
        r#","children":[{"name":"/dev/sdc1","type":"part","fstype":"zfs_member","mountpoints":[null]}]"#,
    );
    for (device, uuid) in [("/dev/sdb", "u1"), ("/dev/sdc", "u2")] {
        fake.expect(WIPEFS, ["--all", "--force", device], Reply::ok(""));
        fake.expect(
            CRYPTSETUP,
            [
                "luksFormat",
                "--batch-mode",
                "--type",
                "luks2",
                "--label",
                "bmac-backup:bk",
                "--key-file=-",
                device,
            ],
            Reply::ok(""),
        );
        fake.expect(
            CRYPTSETUP,
            ["luksUUID", device],
            Reply::ok(format!("{uuid}\n")),
        );
        fake.expect(
            CRYPTSETUP,
            ["open", "--key-file=-", device, &mapper(uuid)],
            Reply::ok(""),
        );
    }
    fake.expect(
        ZPOOL,
        [
            "create",
            "-o",
            "ashift=12",
            "-o",
            "cachefile=none",
            "-O",
            "compression=on",
            "-O",
            "atime=off",
            "-O",
            "mountpoint=none",
            "-O",
            "canmount=off",
            "bk",
            "mirror",
            "/dev/mapper/bmac-bk-u1",
            "/dev/mapper/bmac-bk-u2",
        ],
        Reply::ok(""),
    );
    fake.expect(
        ZPOOL,
        ["list", "-Hp", "-o", "name,guid"],
        Reply::ok("rpool\t42\nbk\t123\n"),
    );
    let mut prompter = Scripted::new(["bk", PASS, PASS]);
    let disks: Vec<String> = disks.map(String::from).to_vec();
    let guid = create(&fake, &mut prompter, "bk", Layout::Mirror, &disks)
        .await
        .unwrap();
    assert_eq!(guid, "123");
    fake.assert_done();
    for call in fake.calls() {
        if call.args.iter().any(|a| a == "--key-file=-") {
            assert_eq!(call.stdin.as_deref(), Some(PASS.as_bytes()), "no newline");
        } else {
            assert!(call.stdin.is_none());
        }
        assert!(!call.line().contains(PASS), "never on argv");
    }
    let shown = prompter.text();
    assert!(shown.contains("erases /dev/sdc1 (zfs_member)"), "{shown}");
    assert!(!shown.contains(PASS));
}

#[tokio::test]
async fn create_refuses_partitions_busy_disks_and_an_unconfirmed_erase() {
    let fake = FakeRunner::new();
    for bad in ["/dev/sdb", "/dev/disk/by-id/ata-A-part1"] {
        no_pool(&fake);
        let error = create(
            &fake,
            &mut Scripted::new(["bk"]),
            "bk",
            Layout::Stripe,
            &[bad.to_string()],
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("by-id") || error.contains("partition"),
            "{error}"
        );
    }
    no_pool(&fake);
    expect_disk(
        &fake,
        "/dev/disk/by-id/ata-A",
        "/dev/sda",
        r#","children":[{"name":"/dev/sda1","type":"part","fstype":"ext4","mountpoints":["/"]}]"#,
    );
    let error = create(
        &fake,
        &mut Scripted::new(["bk"]),
        "bk",
        Layout::Stripe,
        &["/dev/disk/by-id/ata-A".to_string()],
    )
    .await
    .unwrap_err();
    assert!(error.contains("mounted at /"), "{error}");
    no_pool(&fake);
    expect_disk(&fake, "/dev/disk/by-id/ata-B", "/dev/sdb", "");
    let error = create(
        &fake,
        &mut Scripted::new(["yes"]),
        "bk",
        Layout::Stripe,
        &["/dev/disk/by-id/ata-B".to_string()],
    )
    .await
    .unwrap_err();
    assert!(error.contains("not confirmed"), "{error}");
    let error = create(
        &fake,
        &mut Scripted::new(Vec::<String>::new()),
        "bk",
        Layout::Raidz2,
        &["/dev/disk/by-id/ata-B".to_string()],
    )
    .await
    .unwrap_err();
    assert!(error.contains("at least 4"), "{error}");
    fake.assert_done();
    assert!(
        !fake
            .call_lines()
            .iter()
            .any(|l| l.contains("wipefs") || l.contains("luksFormat")),
        "nothing was erased"
    );
}

const BLKID_OUT: &str = "DEVNAME=/dev/sdb\nUUID=u1\nLABEL=bmac-backup:bk\nTYPE=crypto_LUKS\n\n\
DEVNAME=/dev/sdc\nUUID=u2\nLABEL=bmac-backup:bk\nTYPE=crypto_LUKS\n\n\
DEVNAME=/dev/sdd\nUUID=u3\nTYPE=crypto_LUKS\n";

fn expect_blkid(fake: &FakeRunner, out: &str) {
    fake.expect(
        BLKID,
        ["-c", "/dev/null", "-o", "export", "-t", "TYPE=crypto_LUKS"],
        Reply::ok(out),
    );
}

#[tokio::test]
async fn unlock_asks_once_opens_every_member_and_imports_by_guid() {
    let fake = FakeRunner::new();
    expect_blkid(&fake, BLKID_OUT);
    for uuid in ["u1", "u2"] {
        fake.expect(
            CRYPTSETUP,
            ["status", &mapper(uuid)],
            Reply::fail(4, "inactive"),
        );
    }
    let test = ["open", "--test-passphrase", "--key-file=-", "/dev/sdb"];
    fake.expect(CRYPTSETUP, test, Reply::fail(2, "No key available"));
    fake.expect(CRYPTSETUP, test, Reply::ok(""));
    for (device, uuid) in [("/dev/sdb", "u1"), ("/dev/sdc", "u2")] {
        fake.expect(
            CRYPTSETUP,
            ["open", "--key-file=-", device, &mapper(uuid)],
            Reply::ok(""),
        );
    }
    no_pool(&fake);
    fake.expect(
        ZPOOL,
        [
            "import",
            "-d",
            "/dev/mapper",
            "-N",
            "-o",
            "cachefile=none",
            "777",
        ],
        Reply::ok(""),
    );
    fake.expect(
        ZPOOL,
        ["list", "-Hp", "-o", "name,guid"],
        Reply::ok("rpool\t42\nbk\t777\n"),
    );
    let mut prompter = Scripted::new(["wrong passphrase", PASS]);
    let done = unlock(&fake, &mut prompter, Some(("bk", "777")))
        .await
        .unwrap();
    assert_eq!(
        done,
        Unlocked {
            pool: "bk".into(),
            pool_guid: "777".into(),
            opened: 2,
            imported: true
        }
    );
    fake.assert_done();
    assert!(prompter.text().contains("doesn't open the pool; try again"));
    let opens: Vec<_> = fake
        .calls()
        .into_iter()
        .filter(|c| c.args[0] == "open" && c.args[1] == "--key-file=-")
        .collect();
    assert!(
        opens
            .iter()
            .all(|c| c.stdin.as_deref() == Some(PASS.as_bytes()))
    );
}

#[tokio::test]
async fn unlock_refuses_a_foreign_guid_and_ambiguous_disks() {
    let fake = FakeRunner::new();
    expect_blkid(&fake, BLKID_OUT);
    for uuid in ["u1", "u2"] {
        fake.expect(CRYPTSETUP, ["status", &mapper(uuid)], Reply::ok("active"));
    }
    fake.expect(
        ZPOOL,
        ["list", "-Hp", "-o", "name,guid"],
        Reply::ok("rpool\t42\nbk\t999\n"),
    );
    let error = unlock(&fake, &mut Scripted::default(), Some(("bk", "777")))
        .await
        .unwrap_err();
    assert!(error.contains("not this appliance's 777"), "{error}");

    expect_blkid(
        &fake,
        "DEVNAME=/dev/sdb\nUUID=u1\nLABEL=bmac-backup:bk\n\nDEVNAME=/dev/sdc\nUUID=u2\nLABEL=bmac-backup:old\n",
    );
    let error = unlock(&fake, &mut Scripted::default(), None)
        .await
        .unwrap_err();
    assert!(error.contains("several backup pools (bk, old)"), "{error}");

    expect_blkid(&fake, "");
    let error = unlock(&fake, &mut Scripted::default(), Some(("bk", "777")))
        .await
        .unwrap_err();
    assert!(error.contains("no disk labelled bmac-backup:bk"), "{error}");
    fake.assert_done();
}
