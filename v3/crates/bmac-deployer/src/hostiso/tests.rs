// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::sync::Mutex;

use bmac_core::PoolName;
use bmac_exec::{BoxFuture, ExecError, Exit, Output};
use bmac_proto::inventory::{
    CpuInfo, Firmware, FirmwareMode, INVENTORY_VERSION, InventoryNic, NicLink, SecureBoot,
    SystemInfo,
};
use bmac_storage::{ById, Encryption, Vdev, VdevClass, VdevType};

use super::answer::tests::disk;
use super::*;

const GIB: u64 = 1 << 30;
pub(crate) const PASSWORD: &str = "root password 123";
pub(crate) const TS_KEY: &str = "tskey-auth-kLAB0001CNTRL-secretsecretsecret";

fn values(pairs: &[(&str, &str)]) -> SettingValues {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

pub(crate) fn cluster() -> SettingValues {
    values(&[
        ("PROXMOX_CLUSTER_NAME", "MyAppCloud"),
        ("PROXMOX_INTERNAL_DOMAIN", "pve.internal"),
        ("MAX_MOX_HOSTS", "10"),
        ("PRIVATE_SUBNET_CIDR", "10.213.0.0/24"),
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
        ("PROXMOX_DNS_SERVER", "1.1.1.1"),
        ("PROXMOX_ADMIN_EMAIL", "admin@myapp.com"),
        ("PROXMOX_TIMEZONE", "America/Denver"),
        ("PROXMOX_COUNTRY", "us"),
        ("PROXMOX_KEYBOARD", "en-us"),
        (
            "ADMIN_1_PUBLIC_SSH_KEY",
            "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4 admin1@myapp.com",
        ),
    ])
}

pub(crate) fn host_settings() -> SettingValues {
    values(&[
        ("PROXMOX_IP", "203.0.113.33"),
        ("PROXMOX_GATEWAY", "203.0.113.1"),
        ("PROXMOX_PREFIX", "24"),
        ("PROXMOX_PUBLIC_MAC", "AA:BB:CC:DD:EE:FF"),
        ("PROXMOX_SECONDARY_MAC", "auto"),
        ("MAX_PROD_VM_COUNT_ON_THIS_HOST", "1"),
        ("MAX_STAGING_VM_COUNT_ON_THIS_HOST", "4"),
    ])
}

pub(crate) fn inventory() -> Inventory {
    let mut a = disk(
        "nvme0n1",
        500 * GIB,
        &[
            ("ID_SERIAL_SHORT", "S1BOOT"),
            ("ID_SERIAL", "Samsung_S1BOOT"),
            ("ID_WWN", "eui.0001"),
        ],
    );
    a.by_id = vec!["nvme-Samsung_S1BOOT".into()];
    a.serial = Some("S1BOOT".into());
    let mut b = disk(
        "nvme1n1",
        500 * GIB,
        &[
            ("ID_SERIAL_SHORT", "S2BOOT"),
            ("ID_SERIAL", "Samsung_S2BOOT"),
            ("ID_WWN", "eui.0002"),
        ],
    );
    b.by_id = vec!["nvme-Samsung_S2BOOT".into()];
    b.serial = Some("S2BOOT".into());
    let mut c = disk("sda", 4000 * GIB, &[("ID_SERIAL_SHORT", "DATA1")]);
    c.by_id = vec!["ata-Disk_DATA1".into()];
    let mut d = disk("sdb", 4000 * GIB, &[("ID_SERIAL_SHORT", "DATA2")]);
    d.by_id = vec!["ata-Disk_DATA2".into()];
    Inventory {
        version: INVENTORY_VERSION,
        taken_ms: 1,
        system: SystemInfo::default(),
        cpu: CpuInfo::default(),
        memory_bytes: None,
        firmware: Firmware {
            mode: FirmwareMode::Uefi,
            secure_boot: SecureBoot::Disabled,
        },
        disks: vec![a, b, c, d],
        nics: vec![
            InventoryNic {
                name: "eno1".into(),
                mac: "aa:bb:cc:dd:ee:ff".into(),
                driver: None,
                bus: None,
                link: NicLink::NoCarrier { waited_seconds: 0 },
            },
            InventoryNic {
                name: "eno2".into(),
                mac: "aa:bb:cc:dd:ee:01".into(),
                driver: Some("ixgbe".into()),
                bus: None,
                link: NicLink::Up {
                    speed_mbps: Some(10_000),
                    duplex: None,
                    waited_seconds: 9,
                },
            },
        ],
        link_wait: None,
    }
}

fn id(name: &str) -> ById {
    name.parse().unwrap()
}

pub(crate) fn layout() -> PoolLayout {
    PoolLayout::new(
        "rpool".parse::<PoolName>().unwrap(),
        Encryption::None,
        vec![
            Vdev::new(
                VdevClass::Data,
                VdevType::Mirror,
                vec![id("nvme-Samsung_S1BOOT"), id("nvme-Samsung_S2BOOT")],
            )
            .boot(),
            Vdev::new(
                VdevClass::Data,
                VdevType::Mirror,
                vec![id("ata-Disk_DATA1"), id("ata-Disk_DATA2")],
            ),
        ],
    )
}

fn request() -> HostIsoRequest {
    HostIsoRequest {
        host: HostSlot::new(1).unwrap(),
        host_settings: host_settings(),
        origin: Origin::NewCluster {
            cluster_settings: cluster(),
        },
        inventory: inventory(),
        layout: layout(),
        root_password: PASSWORD.into(),
        tailscale_key: TS_KEY.into(),
        magic_dns_suffix: "tail1234.ts.net".into(),
    }
}

pub(crate) fn packages() -> Vec<Package> {
    vec![Package {
        file_name: "bmacd_3.0.0_amd64.deb".into(),
        bytes: b"!<arch>\ndebian-binary".to_vec(),
    }]
}

pub(crate) const NOW: i64 = 1_760_000_000_000;

fn payload_files(first_boot: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let text = String::from_utf8_lossy(first_boot);
    let offset: usize = text
        .lines()
        .find_map(|l| l.strip_prefix("PAYLOAD_OFFSET="))
        .unwrap()
        .parse()
        .unwrap();
    let mut files = BTreeMap::new();
    let mut at = offset;
    while at + 512 <= first_boot.len() {
        let header = &first_boot[at..at + 512];
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let name_end = header[..100].iter().position(|&b| b == 0).unwrap_or(100);
        let name = String::from_utf8(header[..name_end].to_vec()).unwrap();
        let size_text = String::from_utf8(header[124..135].to_vec()).unwrap();
        let size = usize::from_str_radix(size_text.trim_matches('\0'), 8).unwrap();
        let start = at + 512;
        files.insert(name, first_boot[start..start + size].to_vec());
        at = start + size.div_ceil(512) * 512;
    }
    files
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn the_answer_file_says_what_v2_s_does() {
    let prepared = prepare(&request(), &packages(), NOW, false).unwrap();
    let answer = &prepared.answer_toml;
    let salt = answer::root_password_salt("MyAppCloud", "mox1");
    for line in [
        "keyboard = \"en-us\"",
        "country = \"us\"",
        "fqdn = \"mox1.pve.internal\"",
        "mailto = \"admin@myapp.com\"",
        "timezone = \"America/Denver\"",
        &format!("root-password-hashed = \"$6${salt}$"),
        "root-ssh-keys = [\"ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBhE1+iRU5AP8b2hmfLfAHJQ/BVRAR0n4ddx0DFCnkg4 admin1@myapp.com\"]",
        "reboot-on-error = false",
        "source = \"from-answer\"",
        "cidr = \"203.0.113.33/24\"",
        "dns = \"1.1.1.1\"",
        "gateway = \"203.0.113.1\"",
        "filter.ID_NET_NAME_MAC = \"*aabbccddeeff\"",
        "filesystem = \"zfs\"",
        "filter-match = \"any\"",
        "zfs.raid = \"raid1\"",
        "zfs.ashift = 12",
        "zfs.hdsize = 496",
        "[first-boot]",
        "source = \"from-iso\"",
        "ordering = \"before-network\"",
    ] {
        assert!(answer.contains(line), "missing {line:?} in\n{answer}");
    }
    assert!(!answer.contains("root-password ="));
    assert!(!answer.contains(PASSWORD));
    // Each boot disk by its own property; the data disks are not selected.
    let filters: Vec<&str> = answer
        .lines()
        .filter(|l| l.starts_with("filter.ID_SERIAL") || l.starts_with("filter.ID_WWN"))
        .collect();
    assert_eq!(
        filters,
        [
            "filter.ID_SERIAL_SHORT = \"S1BOOT\"",
            "filter.ID_WWN = \"eui.0002\""
        ]
    );
    assert!(!answer.contains("DATA1") && !answer.contains("DATA2"));
}

#[test]
fn the_certificate_in_the_iso_is_the_one_on_the_host_card() {
    let prepared = prepare(&request(), &packages(), NOW, false).unwrap();
    let files = payload_files(&prepared.first_boot);
    let pem = String::from_utf8(files["tls/https.pem"].clone()).unwrap();
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    use base64::Engine;
    let der = base64::engine::general_purpose::STANDARD
        .decode(body)
        .unwrap();
    assert_eq!(keys::der_fingerprint(&der), prepared.card.https_fingerprint);
    assert_eq!(
        prepared.bootstrap.https_fingerprint,
        prepared.card.https_fingerprint
    );
    assert_eq!(
        prepared.card.dashboard_url,
        "https://mox1.tail1234.ts.net:8443/"
            .replace("mox1", &tailscale_name(HostSlot::new(1).unwrap()))
    );
    let public = String::from_utf8(files["ssh/ssh_host_ed25519_key.pub"].clone()).unwrap();
    assert_eq!(public.trim(), prepared.card.ssh_host_key);
    assert_eq!(
        keys::ssh_fingerprint(&prepared.card.ssh_host_key).unwrap(),
        prepared.card.ssh_fingerprint
    );
}

#[test]
fn every_image_gets_fresh_keys() {
    let one = prepare(&request(), &packages(), NOW, false).unwrap();
    let two = prepare(&request(), &packages(), NOW, false).unwrap();
    assert_ne!(one.card.ssh_host_key, two.card.ssh_host_key);
    assert_ne!(one.card.https_fingerprint, two.card.https_fingerprint);
}

#[test]
fn the_image_holds_only_the_bootstrap_s_own_secrets() {
    let prepared = prepare(&request(), &packages(), NOW, false).unwrap();
    let files = payload_files(&prepared.first_boot);
    let names: Vec<&str> = files.keys().map(String::as_str).collect();
    assert_eq!(
        names,
        [
            "SHA256SUMS",
            "authorized_keys",
            "bootstrap/bootstrap.json",
            "ssh/ssh_host_ed25519_key",
            "ssh/ssh_host_ed25519_key.pub",
            "tailscale/auth-key",
            "tls/https.key",
            "tls/https.pem",
        ]
    );
    let everything = [prepared.answer_toml.as_bytes(), &prepared.first_boot].concat();
    // Never in the image: the plaintext password.
    assert!(!contains(&everything, PASSWORD));
    // The Tailscale key exactly once, in its own file.
    let key_files: Vec<&String> = files
        .iter()
        .filter(|(_, data)| contains(data, TS_KEY))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(key_files, ["tailscale/auth-key"]);
    assert!(!contains(prepared.answer_toml.as_bytes(), TS_KEY));
    // Private keys only in their own files.
    for (name, data) in &files {
        let private = contains(data, "PRIVATE KEY");
        let expected = matches!(name.as_str(), "ssh/ssh_host_ed25519_key" | "tls/https.key");
        assert_eq!(private, expected, "{name}");
    }
    // The bootstrap file is readable by bmacd's setup mode and secret-free.
    let bootstrap: HostBootstrap =
        serde_json::from_slice(&files["bootstrap/bootstrap.json"]).unwrap();
    assert_eq!(bootstrap, prepared.bootstrap);
    match &bootstrap.origin {
        BootstrapOrigin::NewCluster(seed) => {
            assert_eq!(seed.used_tailscale_key_sha256, [tskey::key_digest(TS_KEY)]);
            assert!(uuid::Uuid::parse_str(&seed.cluster_uuid).is_ok());
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_used_tailscale_key_is_refused_unless_the_build_accepts_it() {
    let mut req = request();
    let bundle = JoinBundle {
        version: bmac_proto::bootstrap::JOIN_BUNDLE_VERSION,
        cluster_uuid: "11111111-2222-3333-4444-555555555555".into(),
        cluster_name: "MyAppCloud".into(),
        cluster_settings: cluster(),
        slot: HostSlot::new(1).unwrap(),
        host_settings: host_settings(),
        join_token: "join-token-secret".into(),
        expires_ms: NOW + 1000,
        hosts: Vec::new(),
        proxmox_ca_fingerprint: "AA:BB".into(),
        setup_ssh_public_key: "ssh-ed25519 AAAAsetup bmac-setup".into(),
        used_tailscale_key_sha256: vec![tskey::key_digest(TS_KEY)],
    };
    req.origin = Origin::Join(Box::new(bundle.clone()));
    let error = prepare(&req, &packages(), NOW, false).unwrap_err();
    assert!(error.contains("already used"), "{error}");
    let accepted = prepare(&req, &packages(), NOW, true).unwrap();
    assert_eq!(accepted.warnings.len(), 1);

    // A fresh key joins, and the setup key goes in next to the admin keys.
    req.tailscale_key = "tskey-auth-kFRESH-0000".into();
    let joined = prepare(&req, &packages(), NOW, false).unwrap();
    let files = payload_files(&joined.first_boot);
    let keys = String::from_utf8(files["authorized_keys"].clone()).unwrap();
    assert!(keys.contains("admin1@myapp.com") && keys.contains("bmac-setup"));
    assert!(!joined.answer_toml.contains("bmac-setup"));
    match &joined.bootstrap.origin {
        BootstrapOrigin::Join(b) => assert_eq!(b.used_tailscale_key_sha256.len(), 2),
        other => panic!("{other:?}"),
    }

    let mut expired = bundle;
    expired.expires_ms = NOW;
    req.origin = Origin::Join(Box::new(expired));
    assert!(
        prepare(&req, &packages(), NOW, false)
            .unwrap_err()
            .contains("expired")
    );
}

#[test]
fn inputs_that_do_not_fit_the_machine_are_refused() {
    let mut req = request();
    req.host_settings
        .insert("PROXMOX_PUBLIC_MAC".into(), "AA:BB:CC:DD:EE:00".into());
    assert!(
        prepare(&req, &packages(), NOW, false)
            .unwrap_err()
            .contains("public MAC")
    );

    let mut req = request();
    req.layout.vdevs[1].members[0] = id("ata-Missing_DISK");
    assert!(
        prepare(&req, &packages(), NOW, false)
            .unwrap_err()
            .contains("not in the hardware inventory")
    );

    let mut req = request();
    req.layout.vdevs[0].boot = false;
    assert!(
        prepare(&req, &packages(), NOW, false)
            .unwrap_err()
            .contains("boot vdev")
    );

    let mut req = request();
    req.root_password = "short".into();
    assert!(prepare(&req, &packages(), NOW, false).is_err());

    let mut req = request();
    req.inventory.disks[0].problems = vec!["serial shared with sdb".into()];
    assert!(
        prepare(&req, &packages(), NOW, false)
            .unwrap_err()
            .contains("serial shared")
    );

    let mut req = request();
    if let Origin::NewCluster { cluster_settings } = &mut req.origin {
        cluster_settings.remove("ADMIN_1_PUBLIC_SSH_KEY");
    }
    assert!(
        prepare(&req, &packages(), NOW, false)
            .unwrap_err()
            .contains("administrator SSH key")
    );
}

#[test]
fn the_request_never_prints_its_secrets() {
    let shown = format!("{:?}", request());
    assert!(!shown.contains(PASSWORD) && !shown.contains(TS_KEY));
    let prepared = prepare(&request(), &packages(), NOW, false).unwrap();
    let shown = format!("{prepared:?}");
    assert!(!shown.contains("PRIVATE") && !shown.contains(TS_KEY));
}

fn after<'a>(args: &'a [String], flag: &str) -> &'a str {
    let at = args.iter().position(|a| a == flag).unwrap();
    &args[at + 1]
}

/// The assistant and xorriso: records their arguments, checks the working
/// files, and writes what each would. `xorriso` appends the mapped file to
/// the image, so the test can see both.
struct FakeAssistant {
    calls: Mutex<Vec<Vec<String>>>,
    /// Whether prepare-iso builds anything.
    builds: bool,
}

impl CommandRunner for FakeAssistant {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
        let mut args = command.arguments().to_vec();
        let xorriso = command.program().ends_with("xorriso");
        Box::pin(async move {
            let mut stdout = Vec::new();
            if xorriso {
                let mapped = fs::read(after(&args, "-map")).unwrap();
                let mut iso = fs::read(after(&args, "-dev")).unwrap();
                iso.extend_from_slice(&mapped);
                fs::write(after(&args, "-dev"), iso).unwrap();
                args.insert(0, "xorriso".into());
            } else if args.first().map(String::as_str) == Some("prepare-iso") {
                let answer = after(&args, "--answer-file");
                let mode = fs::metadata(answer).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600);
                let first_boot = fs::read(after(&args, "--on-first-boot")).unwrap();
                assert!(first_boot.starts_with(b"#!/usr/bin/env bash"));
                if self.builds {
                    fs::write(after(&args, "--output"), b"built iso;").unwrap();
                } else {
                    stdout = b"Checking provided answer file...\nError: too big\n".to_vec();
                }
            }
            self.calls.lock().unwrap().push(args);
            Ok(Output {
                exit: Exit::Code(0),
                stdout,
                stderr: Vec::new(),
                stdout_truncated: false,
                stderr_truncated: false,
                elapsed: std::time::Duration::ZERO,
            })
        })
    }
}

pub(crate) struct NoFetch;

impl Fetch for NoFetch {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        _dest: &'a Path,
        _progress: Progress<'a>,
    ) -> bmac_exec::BoxFuture<'a, Result<(), String>> {
        Box::pin(async move { Err(format!("no network in tests: {url}")) })
    }
}

/// A cache with the assistant and a pinned source ISO, and a request for it.
fn cached(dir: &Path) -> (PathBuf, HostIsoRequest) {
    let cache = dir.join("cache");
    // A cached assistant and source ISO, so nothing is downloaded.
    let binary = cache.join(format!(
        "tools/proxmox-auto-install-assistant-{}/usr/bin/proxmox-auto-install-assistant",
        assistant::VERSION
    ));
    fs::create_dir_all(binary.parent().unwrap()).unwrap();
    fs::write(&binary, b"").unwrap();
    let mut req = request();
    if let Origin::NewCluster { cluster_settings } = &mut req.origin {
        let iso = b"standard iso";
        let digest: String = sha2::Sha256::digest(iso)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        cluster_settings.insert(
            "PROXMOX_ISO_FILE_URL".into(),
            "https://example.test/proxmox-ve_9.2-1.iso".into(),
        );
        cluster_settings.insert("PROXMOX_ISO_FILE_SHA256".into(), digest);
        fs::create_dir_all(cache.join("source-iso")).unwrap();
        fs::write(cache.join("source-iso/proxmox-ve_9.2-1.iso"), iso).unwrap();
    }
    (cache, req)
}

fn build_env<'a>(cache: &'a Path, isos: &'a Path, runner: &'a FakeAssistant) -> BuildEnv<'a> {
    BuildEnv {
        cache_dir: cache,
        iso_dir: isos,
        runner,
        dpkg_deb: Path::new("/usr/bin/dpkg-deb"),
        xorriso: Path::new("/usr/bin/xorriso"),
        fetch: &NoFetch,
        progress: &|_, _| {},
        note: &|_| {},
    }
}

#[tokio::test]
async fn the_assistant_runs_with_files_and_never_with_secrets_in_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let isos = dir.path().join("isos");
    let (cache, req) = cached(dir.path());
    let prepared = prepare(&req, &packages(), NOW, false).unwrap();
    let output = isos.join("proxmox-ve_9.2-1-mox1-auto.iso");
    let runner = FakeAssistant {
        calls: Mutex::new(Vec::new()),
        builds: true,
    };
    let built = build(&prepared, &build_env(&cache, &isos, &runner))
        .await
        .unwrap();
    assert_eq!(built.path, output);
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // The packages go on the media, outside the first-boot file, under the
    // digest the script checks.
    assert_eq!(
        fs::read(&output).unwrap(),
        [b"built iso;".as_slice(), &prepared.packages_tar].concat()
    );
    let digest: String = sha2::Sha256::digest(&prepared.packages_tar)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert!(contains(
        &prepared.first_boot,
        &format!("PACKAGES_SHA256='{digest}'")
    ));
    assert!(!contains(&prepared.first_boot, "debian-binary"));
    let calls = runner.calls.lock().unwrap().clone();
    let verbs: Vec<&str> = calls.iter().map(|c| c[0].as_str()).collect();
    assert_eq!(
        verbs,
        ["validate-answer", "prepare-iso", "xorriso", "inspect-iso"]
    );
    assert!(
        calls[2]
            .join(" ")
            .contains(&format!("{} -chmod 0444", firstboot::MEDIA_PACKAGES))
    );
    for call in &calls {
        let args = call.join(" ");
        assert!(!args.contains(PASSWORD) && !args.contains(TS_KEY) && !args.contains("$6$"));
    }
    // The working files are gone afterwards.
    let left: Vec<_> = fs::read_dir(&isos)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["proxmox-ve_9.2-1-mox1-auto.iso"]);
}

#[tokio::test]
async fn an_assistant_that_builds_nothing_is_reported_with_what_it_said() {
    let dir = tempfile::tempdir().unwrap();
    let isos = dir.path().join("isos");
    let (cache, req) = cached(dir.path());
    let prepared = prepare(&req, &packages(), NOW, false).unwrap();
    let runner = FakeAssistant {
        calls: Mutex::new(Vec::new()),
        builds: false,
    };
    let error = build(&prepared, &build_env(&cache, &isos, &runner))
        .await
        .unwrap_err();
    assert!(error.contains("Error: too big"), "{error}");
    let left: Vec<_> = fs::read_dir(&isos).unwrap().collect();
    assert!(left.is_empty());
}

use sha2::Digest as _;
