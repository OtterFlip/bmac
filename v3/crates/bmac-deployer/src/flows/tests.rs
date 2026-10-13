// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::job::{Answer, EventBody, Instruction, RequestForm};
use bmac_exec::{BoxFuture, Command, CommandRunner, ExecError, Exit, Output};
use bmac_proto::deployer::{AnswerTask, BuiltIso, LinkWait, TaskKind};
use bmac_proto::inventory::{ClearReport, Inventory, ZfsLabel, ZfsPoolState};
use serde_json::{Value, json};
use sha2::Digest;

use super::*;
use crate::download::{Fetch, Pinned, Progress};
use crate::hostiso::keys::HttpsCertificate;
use crate::hostiso::tests::{PASSWORD, TS_KEY, cluster, inventory};
use crate::tailnet;
use crate::tasks::{NoSink, Tasks};

/// The machine and the workstation's programs, as the workflows see them.
struct Machine {
    /// The name and tag the tailnet shows once the machine has booted.
    peer: Mutex<Option<(String, String)>>,
    dns_name: String,
    inventory: Inventory,
    cleared: AtomicBool,
    calls: Mutex<Vec<String>>,
}

fn output(stdout: impl Into<Vec<u8>>) -> Output {
    Output {
        exit: Exit::Code(0),
        stdout: stdout.into(),
        stderr: Vec::new(),
        stdout_truncated: false,
        stderr_truncated: false,
        elapsed: Duration::ZERO,
    }
}

fn after<'a>(args: &'a [String], flag: &str) -> Option<&'a String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
}

impl CommandRunner for Machine {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
        let program = command
            .program()
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let args = command.arguments().to_vec();
        self.calls
            .lock()
            .unwrap()
            .push(format!("{program} {}", args.join(" ")));
        Box::pin(async move {
            Ok(match program.as_str() {
                "xorriso" => {
                    if let Some(out) = after(&args, "-outdev") {
                        std::fs::write(out, b"discovery iso").unwrap();
                    } else if after(&args, "-dev").is_some() {
                        // The host ISO's packages, added in place.
                    } else {
                        let menu = after(&args, "-extract").unwrap();
                        let local = &args[args.iter().position(|a| a == menu).unwrap() + 1];
                        std::fs::write(local, "set timeout=1\nmenuentry \"Linux lts\" {\nlinux\t/boot/vmlinuz-lts quiet\n}\n").unwrap();
                    }
                    output("")
                }
                "proxmox-auto-install-assistant" => {
                    if let Some(out) = after(&args, "--output") {
                        std::fs::write(out, b"host iso").unwrap();
                    }
                    output("")
                }
                "tailscale" => {
                    let mut peers = serde_json::Map::new();
                    if let Some((name, tag)) = self.peer.lock().unwrap().clone() {
                        peers.insert(
                            "p1".into(),
                            json!({
                                "HostName": name,
                                "DNSName": format!("{}.", self.dns_name),
                                "Online": true,
                                "Created": "2026-10-12T15:00:00Z",
                                "Tags": [tag],
                                "TailscaleIPs": ["100.64.0.5"],
                            }),
                        );
                    }
                    output(json!({"MagicDNSSuffix": "example.ts.net", "Peer": peers}).to_string())
                }
                "ssh" if args.iter().any(|a| a == "clear-zfs-labels") => {
                    self.cleared.store(true, Ordering::SeqCst);
                    output(
                        serde_json::to_vec(&ClearReport {
                            cleared: vec!["/dev/sda1".into()],
                            ..Default::default()
                        })
                        .unwrap(),
                    )
                }
                "ssh" if args.iter().any(|a| a == "bmac-inventory") => {
                    let mut inventory = self.inventory.clone();
                    if self.cleared.load(Ordering::SeqCst) {
                        for disk in &mut inventory.disks {
                            disk.zfs_labels.clear();
                        }
                    }
                    output(serde_json::to_vec(&inventory).unwrap())
                }
                _ => output(""),
            })
        })
    }
}

/// Serves fixed bytes for any URL.
struct Served;

const SOURCE: &[u8] = b"ubuntu server";
const TGZ: &[u8] = b"tailscale build";

impl Fetch for Served {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        _progress: Progress<'a>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let body = if url.ends_with(".tgz") { TGZ } else { SOURCE };
            std::fs::write(dest, body).map_err(|e| e.to_string())
        })
    }
}

fn sha(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[derive(Default)]
struct Book(Mutex<Vec<BuiltIso>>);

impl IsoBook for Book {
    fn record(&self, iso: BuiltIso) {
        self.0.lock().unwrap().push(iso);
    }
    fn delete(&self, path: &str) -> Result<(), String> {
        crate::shred::file(Path::new(path)).map_err(|e| e.to_string())?;
        self.0.lock().unwrap().retain(|i| i.path != path);
        Ok(())
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    root: PathBuf,
    machine: Arc<Machine>,
    book: Arc<Book>,
    env: Arc<Env>,
}

fn setup(inventory: Inventory, dns_name: &str, dashboard_port: u16) -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let machine = Arc::new(Machine {
        peer: Mutex::new(None),
        dns_name: dns_name.into(),
        inventory,
        cleared: AtomicBool::new(false),
        calls: Mutex::new(Vec::new()),
    });
    let binary = root.join("bmac-inventory");
    std::fs::write(&binary, b"\x7fELF static").unwrap();
    let packages = root.join("packages");
    std::fs::create_dir_all(&packages).unwrap();
    std::fs::write(
        packages.join("bmacd_3.0.0_amd64.deb"),
        b"!<arch>\ndebian-binary",
    )
    .unwrap();
    let book = Arc::new(Book::default());
    let tool = |name: &str| Some(PathBuf::from(format!("/usr/bin/{name}")));
    let env = Arc::new(Env {
        runner: machine.clone(),
        fetch: Arc::new(Served),
        tools: Tools {
            ssh: tool("ssh"),
            ssh_keyscan: tool("ssh-keyscan"),
            xorriso: tool("xorriso"),
            tailscale: tool("tailscale"),
            dpkg_deb: tool("dpkg-deb"),
        },
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        iso_dir: root.join("isos"),
        ssh_dir: root.join("ssh"),
        inventory_binary: Some(binary),
        packages_dir: Some(packages),
        link_wait: LinkWait {
            attempts: 10,
            interval_seconds: 3,
        },
        isos: book.clone(),
        clock: Arc::new(|| 1_000),
        poll: Duration::from_millis(10),
        discovery_sources: (
            Pinned {
                url: "https://example.test/ubuntu.iso".into(),
                sha256: sha(SOURCE),
            },
            Pinned {
                url: "https://example.test/tailscale.tgz".into(),
                sha256: sha(TGZ),
            },
        ),
        dashboard_port,
    });
    Setup {
        _dir: dir,
        root,
        machine,
        book,
        env,
    }
}

/// The request the task is waiting on, once it is.
async fn request(tasks: &Tasks, id: &str) -> (String, RequestForm) {
    for _ in 0..1000 {
        let view = tasks.view(id).unwrap();
        if view.task.state.is_terminal() {
            panic!("the task ended: {:#?}", view.events);
        }
        if let Some(pending) = &view.task.pending_request {
            if view.events.iter().rev().find_map(|e| match &e.body {
                EventBody::Request { request_id, .. } if request_id == pending => Some(false),
                EventBody::ValidationError { .. } => Some(true),
                _ => None,
            }) == Some(true)
            {
                panic!("the answer was refused: {:#?}", view.events.last());
            }
            let form = view.events.iter().rev().find_map(|e| match &e.body {
                EventBody::Request { request_id, form } if request_id == pending => {
                    Some((**form).clone())
                }
                _ => None,
            });
            if let Some(form) = form {
                return (pending.clone(), form);
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no request came");
}

async fn ended(tasks: &Tasks, id: &str) -> bmac_proto::deployer::TaskView {
    for _ in 0..1000 {
        let view = tasks.view(id).unwrap();
        if view.task.state.is_terminal() {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!(
        "the task never ended: {:#?}",
        tasks.view(id).unwrap().events
    );
}

fn refusals(tasks: &Tasks, id: &str) -> usize {
    tasks
        .view(id)
        .unwrap()
        .events
        .iter()
        .filter(|e| matches!(e.body, EventBody::ValidationError { .. }))
        .count()
}

/// Sends `answer` and waits for it to be refused with an error on `field`.
async fn refused(tasks: &Tasks, id: &str, request_id: &str, answer: Answer, field: &str) {
    let before = refusals(tasks, id);
    send(tasks, id, request_id, answer);
    for _ in 0..1000 {
        let view = tasks.view(id).unwrap();
        let errors: Vec<_> = view
            .events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ValidationError { field_errors, .. } => Some(field_errors.clone()),
                _ => None,
            })
            .collect();
        if errors.len() > before {
            let last = errors.last().unwrap();
            assert!(last.contains_key(field), "{last:?}");
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the answer wasn't refused");
}

fn send(tasks: &Tasks, id: &str, request_id: &str, answer: Answer) {
    tasks
        .answer(AnswerTask {
            task: id.into(),
            request_id: request_id.into(),
            answer,
        })
        .unwrap();
}

fn values(pairs: Value) -> Answer {
    Answer::Values {
        values: serde_json::from_value(pairs).unwrap(),
    }
}

fn title(form: &RequestForm) -> String {
    match form {
        RequestForm::Input { title, .. } => title.clone().unwrap_or_default(),
        RequestForm::InputGroup { title, .. }
        | RequestForm::Confirm { title, .. }
        | RequestForm::ManualAction { title, .. } => title.clone(),
    }
}

/// Answers the discovery settings, refusing a gateway off the subnet first.
async fn answer_discovery_settings(tasks: &Tasks, id: &str) {
    let (rid, form) = request(tasks, id).await;
    assert_eq!(title(&form), "Discovery settings");
    let mut answer = json!({
        "address": "203.0.113.33", "prefix": 24, "gateway": "198.51.100.1",
        "dns": "1.1.1.1", "tailscale_key": "tskey-auth-kDISC0001-secret",
        "method": "idrac", "idrac": "10.0.0.5",
    });
    send(tasks, id, &rid, values(answer.clone()));
    for _ in 0..1000 {
        if tasks
            .view(id)
            .unwrap()
            .events
            .iter()
            .any(|e| matches!(e.body, EventBody::ValidationError { .. }))
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    answer["gateway"] = json!("203.0.113.1");
    send(tasks, id, &rid, values(answer));
}

/// Acknowledges the boot steps and lets the machine appear.
async fn boot(s: &Setup, tasks: &Tasks, id: &str, tag: &str) -> Vec<Instruction> {
    let (rid, form) = request(tasks, id).await;
    let RequestForm::ManualAction { instructions, .. } = form else {
        panic!("expected the boot steps, got {form:?}");
    };
    let Instruction::WithCopy { copy, .. } = &instructions[0] else {
        panic!("the ISO's path comes first");
    };
    let name = if tag.contains("discovery") {
        Path::new(copy)
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    } else {
        "mox1".to_string()
    };
    *s.machine.peer.lock().unwrap() = Some((name, tag.to_string()));
    send(tasks, id, &rid, Answer::Acknowledge);
    instructions
}

fn labelled_inventory() -> Inventory {
    let mut inventory = inventory();
    let disk = &mut inventory.disks[2];
    disk.serial = Some("DATA1".into());
    disk.zfs_labels = vec![ZfsLabel {
        device: "/dev/sda1".into(),
        pool: Some("rpool".into()),
        pool_guid: Some("1234567890".into()),
        vdev_guid: None,
        state: ZfsPoolState::Exported,
        txg: None,
        hostname: None,
        copies: 4,
    }];
    inventory
}

#[tokio::test]
async fn discovery_reads_the_machine_and_clears_old_labels_once_confirmed() {
    let s = setup(labelled_inventory(), "bmac-discover.example.ts.net", 0);
    let tasks = Tasks::new(Arc::new(NoSink), Arc::new(|| 1_000));
    let id = start(&tasks, TaskKind::Discovery, s.env.clone()).id;

    answer_discovery_settings(&tasks, &id).await;
    let steps = boot(&s, &tasks, &id, tailnet::DISCOVERY_TAG).await;
    let text: Vec<String> = steps
        .iter()
        .map(|i| match i {
            Instruction::Text(t) => t.clone(),
            Instruction::WithCopy { text, .. } => text.clone(),
        })
        .collect();
    assert!(
        text.iter()
            .any(|t| t.contains("https://10.0.0.5/restgui/start.html"))
    );
    assert!(text.iter().any(|t| t.contains("Map Device")));
    assert_eq!(s.book.0.lock().unwrap().len(), 1);

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Clear old rpool labels?");
    send(&tasks, &id, &rid, values(json!({"clear": ["DATA1"]})));
    let (rid, form) = request(&tasks, &id).await;
    let RequestForm::Confirm {
        confirmation_text,
        details,
        ..
    } = &form
    else {
        panic!("expected the destructive confirmation, got {form:?}");
    };
    assert_eq!(confirmation_text.as_deref(), Some("clear"));
    assert!(details[0].contains("serial DATA1"));
    send(&tasks, &id, &rid, Answer::Confirm { confirmed: true });

    let view = ended(&tasks, &id).await;
    assert_eq!(
        view.task.state,
        bmac_core::job::JobState::Completed,
        "{:#?}",
        view.events
    );
    let calls = s.machine.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.contains("clear-zfs-labels")));
    assert!(calls.iter().any(|c| c.contains("poweroff")));
    for call in &calls {
        assert!(
            !call.contains("tskey-auth"),
            "a key on a command line: {call}"
        );
    }
    let result = view
        .events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::Result { data } => Some(data.clone()),
            _ => None,
        })
        .unwrap();
    let saved: Inventory =
        serde_json::from_slice(&std::fs::read(result["inventory_file"].as_str().unwrap()).unwrap())
            .unwrap();
    assert!(saved.disks.iter().all(|d| d.zfs_labels.is_empty()));
    assert!(s.book.0.lock().unwrap().is_empty(), "the ISO was shredded");
    assert_eq!(
        std::fs::read_dir(s.root.join("isos"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "iso")
            })
            .count(),
        0
    );
    assert!(view.events.iter().any(|e| matches!(&e.body,
        EventBody::Output { text, .. } if text.starts_with("Shredded "))));
    let recorded = serde_json::to_string(&view.events).unwrap();
    assert!(!recorded.contains("tskey-auth-kDISC0001-secret"));
    assert!(
        !s.root.join("state").exists()
            || std::fs::read_dir(s.root.join("state")).unwrap().count() == 0,
        "the session's keys are removed"
    );
}

#[tokio::test]
async fn declining_to_clear_keeps_the_labels_and_warns() {
    let s = setup(labelled_inventory(), "bmac-discover.example.ts.net", 0);
    let tasks = Tasks::new(Arc::new(NoSink), Arc::new(|| 1_000));
    let id = start(&tasks, TaskKind::Discovery, s.env.clone()).id;
    answer_discovery_settings(&tasks, &id).await;
    boot(&s, &tasks, &id, tailnet::DISCOVERY_TAG).await;
    let (rid, _) = request(&tasks, &id).await;
    send(&tasks, &id, &rid, values(json!({"clear": []})));
    let view = ended(&tasks, &id).await;
    assert_eq!(view.task.state, bmac_core::job::JobState::Completed);
    assert!(!s.machine.cleared.load(Ordering::SeqCst));
    assert!(view.events.iter().any(|e| matches!(&e.body,
        EventBody::Warning { message } if message.contains("(initramfs)"))));
    assert!(
        s.book.0.lock().unwrap().is_empty(),
        "the spent ISO was shredded"
    );
}

#[tokio::test]
async fn an_iso_the_machine_may_still_boot_from_is_kept() {
    let s = setup(inventory(), "bmac-discover.example.ts.net", 0);
    let tasks = Tasks::new(Arc::new(NoSink), Arc::new(|| 1_000));
    let id = start(&tasks, TaskKind::Discovery, s.env.clone()).id;
    answer_discovery_settings(&tasks, &id).await;
    let (rid, _) = request(&tasks, &id).await;
    send(&tasks, &id, &rid, Answer::Acknowledge);
    tasks.cancel(&id).unwrap();
    let view = ended(&tasks, &id).await;
    assert_eq!(view.task.state, bmac_core::job::JobState::Cancelled);
    let book = s.book.0.lock().unwrap().clone();
    assert_eq!(book.len(), 1);
    assert!(Path::new(&book[0].path).is_file());
    assert!(
        view.events.iter().any(|e| matches!(&e.body,
        EventBody::Output { text, .. } if text.contains("is kept"))),
        "{:#?}",
        view.events
    );
}

/// A TLS server on a free port presenting `cert`.
async fn dashboard(cert: &HttpsCertificate) -> u16 {
    use rustls::pki_types::pem::PemObject;
    let chain =
        vec![rustls::pki_types::CertificateDer::from_pem_slice(cert.cert_pem.as_bytes()).unwrap()];
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(cert.key_pem.as_bytes()).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let _ = acceptor.accept(tcp).await;
        }
    });
    port
}

#[tokio::test]
async fn a_host_whose_certificate_differs_gets_no_dashboard_link() {
    let other = HttpsCertificate::generate(&["localhost".into()], 0).unwrap();
    let port = dashboard(&other).await;
    let s = setup(inventory(), "localhost", port);

    let mut settings = cluster();
    let source = b"standard iso";
    settings.insert(
        "PROXMOX_ISO_FILE_URL".into(),
        "https://example.test/proxmox-ve_9.2-1.iso".into(),
    );
    settings.insert("PROXMOX_ISO_FILE_SHA256".into(), sha(source));
    let cluster_file = s.root.join("cluster.conf");
    std::fs::write(
        &cluster_file,
        bmac_settings::file_text(bmac_settings::CLUSTER_KEYS, &settings),
    )
    .unwrap();
    let cache = s.root.join("cache");
    std::fs::create_dir_all(cache.join("source-iso")).unwrap();
    std::fs::write(cache.join("source-iso/proxmox-ve_9.2-1.iso"), source).unwrap();
    let assistant = cache.join(format!(
        "tools/proxmox-auto-install-assistant-{}/usr/bin/proxmox-auto-install-assistant",
        crate::hostiso::assistant::VERSION
    ));
    std::fs::create_dir_all(assistant.parent().unwrap()).unwrap();
    std::fs::write(&assistant, b"").unwrap();
    let inventory_file = s.root.join("inventory.json");
    std::fs::write(&inventory_file, serde_json::to_vec(&inventory()).unwrap()).unwrap();

    let tasks = Tasks::new(Arc::new(NoSink), Arc::new(|| 1_000));
    let id = start(&tasks, TaskKind::Host, s.env.clone()).id;
    let (rid, _) = request(&tasks, &id).await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"origin": "new", "file": cluster_file.display().to_string()})),
    );
    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Host settings");
    let RequestForm::InputGroup { fields, .. } = &form else {
        panic!("expected the host settings");
    };
    assert!(
        fields.iter().all(|f| !f.id.starts_with("PROXMOX_")),
        "the network is chosen after discovery"
    );
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"slot": "mox1", "MAX_PROD_VM_COUNT_ON_THIS_HOST": 1,
                      "MAX_STAGING_VM_COUNT_ON_THIS_HOST": 4})),
    );
    let (rid, _) = request(&tasks, &id).await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"source": "file", "inventory_file": inventory_file.display().to_string()})),
    );
    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Network ports");
    let RequestForm::InputGroup { fields, .. } = &form else {
        panic!("expected the network form");
    };
    let private = fields.iter().find(|f| f.id == "private_nic").unwrap();
    assert_eq!(
        private.default,
        Some(json!("aa:bb:cc:dd:ee:01")),
        "the only other port with a link"
    );
    assert_eq!(private.options.len(), 2);
    let mut network = json!({
        "public_nic": "aa:bb:cc:dd:ee:ff", "PROXMOX_IP": "203.0.113.33",
        "PROXMOX_PREFIX": 24, "PROXMOX_GATEWAY": "203.0.113.1",
        "private_nic": "aa:bb:cc:dd:ee:ff",
        "private_address": "10.213.0.11", "private_prefix": 24,
        "route_private_gateway": true,
    });
    let route = fields
        .iter()
        .find(|f| f.id == "route_private_gateway")
        .unwrap();
    assert_eq!(
        route.default,
        Some(json!(false)),
        "no VLAN route unless asked"
    );
    refused(&tasks, &id, &rid, values(network.clone()), "private_nic").await;
    network["private_nic"] = json!("aa:bb:cc:dd:ee:01");
    refused(
        &tasks,
        &id,
        &rid,
        values(network.clone()),
        "PROXMOX_SECONDARY_GATEWAY",
    )
    .await;
    network["PROXMOX_SECONDARY_GATEWAY"] = json!("10.213.0.1");
    network["private_address"] = json!("10.213.0.200");
    refused(
        &tasks,
        &id,
        &rid,
        values(network.clone()),
        "private_address",
    )
    .await;
    network["private_address"] = json!("10.213.0.12");
    send(&tasks, &id, &rid, values(network));
    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Pool layout");
    send(
        &tasks,
        &id,
        &rid,
        values(json!({
            "boot": ["nvme-Samsung_S1BOOT", "nvme-Samsung_S2BOOT"],
            "data_kind": "mirror", "data": ["ata-Disk_DATA1", "ata-Disk_DATA2"],
            "encryption": "none",
        })),
    );
    let (rid, _) = request(&tasks, &id).await;
    send(
        &tasks,
        &id,
        &rid,
        values(
            json!({"root_password": PASSWORD, "root_password_again": PASSWORD,
                      "tailscale_key": TS_KEY}),
        ),
    );
    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "How will you boot mox1?");
    send(&tasks, &id, &rid, values(json!({"method": "usb"})));
    let (rid, form) = request(&tasks, &id).await;
    let RequestForm::Confirm {
        confirmation_text,
        details,
        ..
    } = &form
    else {
        panic!("expected the destructive warning, got {form:?}");
    };
    assert_eq!(confirmation_text.as_deref(), Some("mox1"));
    assert!(details.iter().any(|d| d.contains("S1BOOT")), "{details:#?}");
    send(&tasks, &id, &rid, Answer::Confirm { confirmed: true });
    boot(&s, &tasks, &id, tailnet::HOST_TAG).await;

    let view = ended(&tasks, &id).await;
    assert_eq!(view.task.state, bmac_core::job::JobState::Failed);
    assert!(
        view.task
            .message
            .as_deref()
            .unwrap()
            .contains("doesn't match"),
        "{:?}",
        view.task.message
    );
    assert!(
        !view
            .events
            .iter()
            .any(|e| matches!(e.body, EventBody::Result { .. })),
        "no link after a mismatch"
    );
    let recorded = serde_json::to_string(&view.events).unwrap();
    assert!(!recorded.contains(PASSWORD) && !recorded.contains(TS_KEY));
    for call in s.machine.calls.lock().unwrap().iter() {
        assert!(!call.contains(PASSWORD) && !call.contains(TS_KEY));
    }
    assert!(
        s.book.0.lock().unwrap().is_empty(),
        "the installed host no longer reads its ISO"
    );
    let card = std::fs::read_to_string(s.root.join("isos/mox1-card.txt")).unwrap();
    assert!(card.contains("HTTPS certificate SHA-256"));
}

#[test]
fn layouts_come_from_the_chosen_disks() {
    let values = |v: Value| -> Values { serde_json::from_value(v).unwrap() };
    let layout = host::layout_from(&values(json!({
        "boot": ["a", "b"], "data_kind": "raidz2", "data": ["c", "d", "e", "f"],
        "encryption": "luks2",
    })))
    .unwrap();
    assert_eq!(layout.vdevs.len(), 2);
    assert!(layout.vdevs[0].boot);
    assert_eq!(layout.encryption, bmac_storage::Encryption::Luks2);
    let refused = |v: Value, field: &str| {
        let (errors, _) = host::layout_from(&values(v)).unwrap_err();
        assert!(errors.contains_key(field), "{errors:?}");
    };
    refused(json!({"boot": [], "data_kind": "none"}), "boot");
    refused(
        json!({"boot": ["a"], "data_kind": "none", "data": ["a"]}),
        "data",
    );
    refused(
        json!({"boot": ["a"], "data_kind": "mirror", "data": ["b", "c", "d"]}),
        "data",
    );
    // The installer can be pointed at two boot disks at most.
    refused(
        json!({"boot": ["a", "b", "c"], "data_kind": "none"}),
        "boot",
    );
}

#[test]
fn empty_network_settings_mean_dhcp_and_partial_ones_are_refused() {
    let values = |v: Value| -> Values { serde_json::from_value(v).unwrap() };
    assert_eq!(discovery::static_network(&values(json!({}))).unwrap(), None);
    assert_eq!(
        discovery::static_network(&values(json!({"address": "", "prefix": "", "gateway": ""})))
            .unwrap(),
        None
    );
    let net = discovery::static_network(&values(json!({
        "address": "38.46.219.67", "prefix": "29", "gateway": "38.46.219.65",
    })))
    .unwrap()
    .unwrap();
    assert_eq!(net.prefix, 29);
    let (errors, _) =
        discovery::static_network(&values(json!({"address": "38.46.219.67"}))).unwrap_err();
    assert!(errors.contains_key("prefix"), "{errors:?}");
    let (errors, _) =
        discovery::static_network(&values(json!({"address": "38.46.219.67", "prefix": "29"})))
            .unwrap_err();
    assert!(errors.contains_key("gateway"), "{errors:?}");
}

#[test]
fn the_network_form_offers_what_discovery_saw() {
    let seen = crate::discovery::parse_seen_network(
        "MAC=AA:BB:CC:DD:EE:FF\nADDRESS=38.46.219.67/29\nGATEWAY=38.46.219.65\nMETHOD=dhcp\n",
    )
    .unwrap();
    let form = host::network_form(
        &inventory(),
        &Default::default(),
        Some(&seen),
        "mox1".parse().unwrap(),
        &cluster(),
    );
    let RequestForm::InputGroup {
        fields,
        description,
        ..
    } = &form
    else {
        panic!("expected a group");
    };
    let default = |id: &str| fields.iter().find(|f| f.id == id).unwrap().default.clone();
    assert_eq!(default("public_nic"), Some(json!("aa:bb:cc:dd:ee:ff")));
    assert_eq!(default("PROXMOX_IP"), Some(json!("38.46.219.67")));
    assert_eq!(default("PROXMOX_PREFIX"), Some(json!("29")));
    assert_eq!(default("PROXMOX_GATEWAY"), Some(json!("38.46.219.65")));
    assert_eq!(default("private_nic"), Some(json!("aa:bb:cc:dd:ee:01")));
    assert!(description.as_deref().unwrap().contains("DHCP"));
    assert!(crate::discovery::parse_seen_network("MAC=nonsense\n").is_none());
    assert_eq!(default("private_address"), Some(json!("10.213.0.11")));
    assert_eq!(default("private_prefix"), Some(json!("24")));
}

#[test]
fn the_private_prefix_follows_a_wider_cluster_subnet() {
    let mut wide = cluster();
    for (key, value) in [
        ("MAX_MOX_HOSTS", "32"),
        ("PRIVATE_SUBNET_CIDR", "10.213.0.0/22"),
        ("GUEST_EGRESS_VIP", "10.213.0.10/22"),
        ("MOX_IP_END", "10.213.0.60"),
        ("HAPROXY_IP_START", "10.213.0.61"),
        ("HAPROXY_IP_END", "10.213.0.100"),
        ("PRODUCTION_IP_START", "10.213.1.0"),
        ("PRODUCTION_IP_END", "10.213.1.255"),
        ("STAGING_IP_START", "10.213.2.0"),
        ("STAGING_IP_END", "10.213.2.200"),
        ("PROXMOX_MIGRATION_NETWORK", "10.213.3.192/26"),
        ("MOX_REPLICATION_IP_START", "10.213.3.193"),
        ("MOX_REPLICATION_IP_END", "10.213.3.240"),
    ] {
        wide.insert(key.into(), value.into());
    }
    bmac_settings::validate_cluster(&wide).unwrap();
    let form = host::network_form(
        &inventory(),
        &Default::default(),
        None,
        "mox32".parse().unwrap(),
        &wide,
    );
    let RequestForm::InputGroup { fields, .. } = &form else {
        panic!("expected a group");
    };
    let find = |id: &str| fields.iter().find(|f| f.id == id).unwrap();
    assert_eq!(find("private_address").default, Some(json!("10.213.0.42")));
    assert_eq!(find("private_prefix").default, Some(json!("22")));
    assert!(
        find("private_prefix")
            .help
            .as_deref()
            .unwrap()
            .contains("10.213.0.0/22")
    );
}

#[path = "tests_ssh.rs"]
mod ssh;
