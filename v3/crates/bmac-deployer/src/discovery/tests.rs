// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::process::Command as Std;

use bmac_exec::RealRunner;

use super::*;

const KEY: &str = "tskey-auth-kDiscover1CNTRL-abcdefghijklmnopqrstuvwxyz0123";

fn request() -> DiscoveryRequest {
    DiscoveryRequest {
        network: Some(StaticNetwork {
            address: "203.0.113.33".parse().unwrap(),
            prefix: 26,
            gateway: "203.0.113.1".parse().unwrap(),
        }),
        dns: vec!["1.1.1.1".parse().unwrap(), "9.9.9.9".parse().unwrap()],
        public_mac: Some("AA:BB:CC:DD:EE:FF".into()),
        tailscale_key: KEY.into(),
        carrier_attempts: 10,
        carrier_interval_s: 3,
    }
}

const ELF: &[u8] = b"\x7fELF fake inventory";

fn file<'a>(prepared: &'a PreparedDiscovery, path: &str) -> &'a [u8] {
    &prepared
        .files
        .iter()
        .find(|(p, _, _)| p == path)
        .unwrap_or_else(|| panic!("{path} missing"))
        .1
}

fn tool(name: &str) -> bool {
    Std::new(name).arg("--version").output().is_ok()
}

const CONF: &str = "etc/bmac-discovery";

fn conf(name: &str) -> String {
    format!("{CONF}/{name}")
}

const ALPINE_GRUB: &str = "set timeout=1\n\nmenuentry \"Linux lts\" {\nlinux\t/boot/vmlinuz-lts modules=loop,squashfs,sd-mod,usb-storage quiet \ninitrd\t/boot/intel-ucode.img /boot/amd-ucode.img /boot/initramfs-lts\n}\n";
const ALPINE_SYSLINUX: &str = "TIMEOUT 10\nPROMPT 1\nDEFAULT lts\n\nLABEL lts\nMENU LABEL Linux lts\nKERNEL /boot/vmlinuz-lts\nINITRD /boot/intel-ucode.img,/boot/amd-ucode.img,/boot/initramfs-lts\nFDTDIR /boot/dtbs-lts\nAPPEND modules=loop,squashfs,sd-mod,usb-storage quiet \n";

#[test]
fn the_boot_menus_start_discovery_at_once_with_every_message() {
    let grub = boot_menu(ALPINE_GRUB);
    assert_eq!(
        grub,
        "set timeout=0\n\nmenuentry \"BMAC hardware discovery\" {\nlinux\t/boot/vmlinuz-lts modules=loop,squashfs,sd-mod,usb-storage console=tty0 console=ttyS0,115200n8\ninitrd\t/boot/intel-ucode.img /boot/amd-ucode.img /boot/initramfs-lts\n}\n"
    );
    let syslinux = boot_menu(ALPINE_SYSLINUX);
    assert_eq!(
        syslinux,
        "TIMEOUT 1\nPROMPT 0\nDEFAULT lts\n\nLABEL lts\nMENU LABEL BMAC hardware discovery\nKERNEL /boot/vmlinuz-lts\nINITRD /boot/intel-ucode.img,/boot/amd-ucode.img,/boot/initramfs-lts\nFDTDIR /boot/dtbs-lts\nAPPEND modules=loop,squashfs,sd-mod,usb-storage console=tty0 console=ttyS0,115200n8\n"
    );
    let two = format!("{ALPINE_GRUB}menuentry \"Linux virt\" {{\nlinux\t/boot/vmlinuz-virt\n}}\n");
    let one = boot_menu(&two);
    assert_eq!(one.matches("menuentry").count(), 1);
    assert!(!one.contains("virt"));
}

#[test]
fn the_overlay_unpacks_over_the_root_and_starts_discovery() {
    if !tool("tar") {
        return;
    }
    let prepared = prepare(&request(), ELF, b"tgz").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let archive = temp.path().join("o.apkovl.tar.gz");
    fs::write(&archive, prepared.overlay().unwrap()).unwrap();
    let root = temp.path().join("root");
    fs::create_dir(&root).unwrap();
    assert!(
        Std::new("tar")
            .args(["-xzf"])
            .arg(&archive)
            .arg("-C")
            .arg(&root)
            .arg("--no-same-owner")
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        fs::read_link(root.join("etc/runlevels/default/local")).unwrap(),
        Path::new("/etc/init.d/local")
    );
    assert!(root.join("etc/.default_boot_services").is_file());
    assert_eq!(
        fs::read_to_string(root.join("etc/apk/world")).unwrap(),
        "alpine-base\n"
    );
    assert_eq!(
        fs::read_to_string(root.join("etc/hostname"))
            .unwrap()
            .trim(),
        prepared.session.tailscale_name
    );
    let start = fs::read_to_string(root.join("etc/local.d/bmac-discovery.start")).unwrap();
    assert!(start.contains("/etc/bmac-discovery/discovery.sh"));
    let mode = |p: &str| fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode("etc/local.d/bmac-discovery.start"), 0o755);
    assert_eq!(mode(CONF), 0o700);
    for (path, bytes, file_mode) in &prepared.files {
        assert_eq!(&fs::read(root.join(path)).unwrap(), bytes, "{path}");
        assert_eq!(mode(path), *file_mode, "{path}");
    }
}

#[test]
fn the_session_has_fresh_keys_and_a_discovery_name() {
    let a = prepare(&request(), ELF, b"tgz").unwrap();
    let b = prepare(&request(), ELF, b"tgz").unwrap();
    assert_ne!(a.session.host_key, b.session.host_key);
    assert_ne!(a.session.client.public, b.session.client.public);
    assert_ne!(a.session.tailscale_name, b.session.tailscale_name);
    let prefix = if cfg!(feature = "lab") {
        "bmacnvme-discover-"
    } else {
        "bmac-discover-"
    };
    assert!(a.session.tailscale_name.starts_with(prefix));
    assert!(a.session.host_fingerprint.starts_with("SHA256:"));
    let authorized = String::from_utf8_lossy(file(&a, &conf("authorized_keys"))).to_string();
    assert_eq!(authorized.trim(), key_core(&a.session.client.public));
    assert!(!format!("{:?}{:?}", a.session, request()).contains("PRIVATE KEY"));
    assert!(!format!("{:?}", request()).contains(KEY));
}

#[test]
fn secrets_are_only_where_they_belong() {
    let prepared = prepare(&request(), ELF, b"tgz").unwrap();
    for (path, bytes, mode) in &prepared.files {
        let text = String::from_utf8_lossy(bytes);
        let holds_key = text.contains(KEY);
        let holds_private = text.contains("PRIVATE KEY");
        assert_eq!(holds_key, *path == conf("tailscale-auth-key"), "{path}");
        assert_eq!(
            holds_private,
            *path == conf("ssh_host_ed25519_key"),
            "{path}"
        );
        if holds_key || holds_private {
            assert_eq!(*mode, 0o600, "{path}");
        }
    }
    assert!(
        !String::from_utf8_lossy(file(&prepared, &conf("authorized_keys"))).contains("PRIVATE")
    );
}

#[test]
fn the_settings_file_reads_back_in_sh() {
    if !tool("sh") {
        return;
    }
    let prepared = prepare(&request(), ELF, b"tgz").unwrap();
    let temp = tempfile::tempdir().unwrap();
    let env = temp.path().join("discovery.env");
    fs::write(&env, file(&prepared, &conf("discovery.env"))).unwrap();
    let out = Std::new("sh")
        .arg("-c")
        .arg(". \"$1\"; printf '%s|' \"$ADDRESS\" \"$PREFIX\" \"$GATEWAY\" \"$DNS\" \"$PUBLIC_MAC\" \"$CARRIER_ATTEMPTS\" \"$CARRIER_INTERVAL\" \"$TAILSCALE_NAME\" \"$TOOLS\"")
        .arg("sh")
        .arg(&env)
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        format!(
            "203.0.113.33|26|203.0.113.1|1.1.1.1 9.9.9.9|aa:bb:cc:dd:ee:ff|10|3|{}|{TOOLS}|",
            prepared.session.tailscale_name
        )
    );
    let automatic = prepare(
        &DiscoveryRequest {
            network: None,
            dns: vec![],
            public_mac: None,
            ..request()
        },
        ELF,
        b"tgz",
    )
    .unwrap();
    let env = String::from_utf8_lossy(file(&automatic, &conf("discovery.env"))).to_string();
    for empty in ["ADDRESS", "PREFIX", "GATEWAY", "DNS", "PUBLIC_MAC"] {
        assert!(env.contains(&format!("{empty}=''\n")), "{empty} in {env}");
    }
}

#[test]
fn bad_requests_are_refused() {
    let refuse = |r: DiscoveryRequest| prepare(&r, ELF, b"tgz").unwrap_err();
    assert!(
        refuse(DiscoveryRequest {
            network: Some(StaticNetwork {
                prefix: 0,
                ..request().network.unwrap()
            }),
            ..request()
        })
        .contains("prefix")
    );
    assert!(
        refuse(DiscoveryRequest {
            network: Some(StaticNetwork {
                gateway: "198.51.100.1".parse().unwrap(),
                ..request().network.unwrap()
            }),
            ..request()
        })
        .contains("not in")
    );
    assert!(
        refuse(DiscoveryRequest {
            dns: vec![],
            ..request()
        })
        .contains("DNS")
    );
    assert!(
        refuse(DiscoveryRequest {
            public_mac: Some("aa:bb".into()),
            ..request()
        })
        .contains("MAC")
    );
    assert!(
        refuse(DiscoveryRequest {
            tailscale_key: "tskey-api-x".into(),
            ..request()
        })
        .contains("auth key")
    );
    assert!(
        refuse(DiscoveryRequest {
            tailscale_key: "tskey-auth-a'b".into(),
            ..request()
        })
        .contains("auth key")
    );
    assert!(
        refuse(DiscoveryRequest {
            carrier_attempts: 0,
            ..request()
        })
        .contains("link wait")
    );
    assert!(
        prepare(&request(), b"#!/bin/sh", b"tgz")
            .unwrap_err()
            .contains("not a Linux program")
    );
}

#[test]
fn the_discovery_script_parses_as_posix_sh() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("discovery.sh");
    fs::write(&path, DISCOVERY_SH).unwrap();
    assert!(DISCOVERY_SH.starts_with("#!/bin/sh\n"));
    for shell in [["sh", "-n"], ["busybox", "sh"]] {
        if !tool(shell[0]) && shell[0] != "sh" {
            continue;
        }
        let mut command = Std::new(shell[0]);
        command.args(&shell[1..]);
        if shell[0] == "busybox" {
            command.arg("-n");
        }
        if let Ok(status) = command.arg(&path).status() {
            assert!(status.success(), "{shell:?}");
        }
    }
}

/// A small El Torito-less ISO laid out like Alpine's.
fn fixture_iso(dir: &Path) -> PathBuf {
    let tree = dir.join("tree");
    fs::create_dir_all(tree.join("boot/grub")).unwrap();
    fs::create_dir_all(tree.join("boot/syslinux")).unwrap();
    fs::create_dir_all(tree.join("apks")).unwrap();
    fs::write(tree.join("boot/grub/grub.cfg"), ALPINE_GRUB).unwrap();
    fs::write(tree.join("boot/syslinux/syslinux.cfg"), ALPINE_SYSLINUX).unwrap();
    fs::write(tree.join("boot/vmlinuz-lts"), "kernel").unwrap();
    fs::write(tree.join("apks/.boot_repository"), "").unwrap();
    let iso = dir.join("alpine.iso");
    let status = Std::new("xorriso")
        .args(["-as", "mkisofs", "-quiet", "-r", "-V", "alpine-ext", "-o"])
        .arg(&iso)
        .arg(&tree)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    iso
}

#[tokio::test]
async fn the_iso_gets_the_overlay_and_one_quick_menu_and_keeps_the_rest() {
    let Some(xorriso) = ["/usr/bin/xorriso", "/usr/local/bin/xorriso"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
    else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let source = fixture_iso(temp.path());
    let prepared = prepare(&request(), ELF, b"tgz bytes").unwrap();
    let output = temp.path().join("out/discovery.iso");
    build(&prepared, &source, &output, xorriso, &RealRunner)
        .await
        .unwrap();
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let leftovers: Vec<_> = fs::read_dir(output.parent().unwrap())
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    assert_eq!(leftovers, [std::ffi::OsString::from("discovery.iso")]);

    let extracted = temp.path().join("x");
    fs::create_dir_all(&extracted).unwrap();
    let status = Std::new(xorriso)
        .args(["-osirrox", "on", "-indev"])
        .arg(&output)
        .args(["-extract", "/"])
        .arg(&extracted)
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let overlay = extracted.join(OVERLAY_PATH.trim_start_matches('/'));
    assert!(fs::read(&overlay).unwrap().starts_with(&[0x1f, 0x8b]));
    assert_eq!(
        fs::read_to_string(extracted.join("boot/grub/grub.cfg")).unwrap(),
        boot_menu(ALPINE_GRUB)
    );
    assert_eq!(
        fs::read_to_string(extracted.join("boot/syslinux/syslinux.cfg")).unwrap(),
        boot_menu(ALPINE_SYSLINUX)
    );
    assert_eq!(
        fs::read_to_string(extracted.join("boot/vmlinuz-lts")).unwrap(),
        "kernel"
    );
    assert!(extracted.join("apks/.boot_repository").exists());
}

#[tokio::test]
async fn an_iso_without_a_boot_menu_is_refused() {
    let Some(xorriso) = ["/usr/bin/xorriso"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
    else {
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    fs::create_dir_all(&tree).unwrap();
    fs::write(tree.join("readme"), "x").unwrap();
    let source = temp.path().join("plain.iso");
    assert!(
        Std::new(xorriso)
            .args(["-as", "mkisofs", "-quiet", "-r", "-o"])
            .arg(&source)
            .arg(&tree)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    let prepared = prepare(&request(), ELF, b"tgz").unwrap();
    let err = build(
        &prepared,
        &source,
        &temp.path().join("o/d.iso"),
        xorriso,
        &RealRunner,
    )
    .await
    .unwrap_err();
    assert!(err.contains("no boot menu"), "{err}");
}

#[test]
fn the_session_ssh_pins_the_embedded_key() {
    let prepared = prepare(&request(), ELF, b"tgz").unwrap();
    let base = tempfile::tempdir().unwrap();
    let dir;
    {
        let files = SessionFiles::create(base.path(), &prepared.session).unwrap();
        dir = files.dir.clone();
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::read_to_string(dir.join("known_hosts")).unwrap(),
            format!(
                "{} {}\n",
                prepared.session.tailscale_name, prepared.session.host_key
            )
        );
        let command = files.ssh(Path::new("/usr/bin/ssh"), &prepared.session, "100.64.0.9");
        let args = command.arguments().join(" ");
        for want in [
            "StrictHostKeyChecking=yes",
            "IdentitiesOnly=yes",
            "GlobalKnownHostsFile=none",
            &format!("HostKeyAlias={}", prepared.session.tailscale_name),
            "root@100.64.0.9",
        ] {
            assert!(args.contains(want), "{want} in {args}");
        }
    }
    assert!(!dir.exists(), "the session files go when the session ends");
}
