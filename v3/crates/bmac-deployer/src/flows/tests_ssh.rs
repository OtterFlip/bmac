// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

use super::*;
use crate::hello::tests::{NOW, certificate};
use crate::jump_ssh::{self, Guest, GuestList};
use crate::workstation_ssh::{self, HostPin, merge_config, merge_known_hosts};

/// An Ed25519 public key whose bytes are all `n`.
fn key(n: u8) -> String {
    let mut blob = Vec::new();
    blob.extend_from_slice(&11u32.to_be_bytes());
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&32u32.to_be_bytes());
    blob.extend_from_slice(&[n; 32]);
    format!("ssh-ed25519 {}", STANDARD.encode(blob))
}

fn slot(host: &str) -> u8 {
    host.trim_start_matches("mox").parse().unwrap()
}

fn host_ip(host: &str) -> String {
    format!("100.64.0.{}", slot(host))
}

fn prod1() -> Guest {
    Guest {
        name: "prod1".into(),
        kind: "production".into(),
        state: "active".into(),
        vmid: 100,
        ip: "10.213.0.31".into(),
        node: Some("mox2".into()),
        status: "running".into(),
        primary_domain: "example.com".into(),
        aliases: vec!["www.example.com".into()],
        placement: vec!["mox2".into(), "mox3".into()],
    }
}

/// A cluster of hosts on the tailnet, as the workstation's programs see
/// it. `ssh -G` answers from the config the task wrote.
struct Cluster {
    ssh_dir: PathBuf,
    /// The hosts on the tailnet and in the cluster's node list.
    hosts: Vec<String>,
    /// Keys `ssh-keyscan` sees, when an attacker answers instead.
    scanned: Mutex<Option<String>>,
    /// Names whose `ssh <name> true` fails.
    unreachable: Vec<String>,
    calls: Mutex<Vec<String>>,
}

impl Cluster {
    fn effective(&self, name: &str) -> String {
        let config = std::fs::read_to_string(self.ssh_dir.join("config")).unwrap_or_default();
        let mut inside = false;
        let mut out = Vec::new();
        for line in config.lines() {
            if let Some(names) = line.strip_prefix("Host ") {
                inside = names.split_whitespace().any(|n| n == name);
                continue;
            }
            let Some((k, v)) = line.trim().split_once(' ').filter(|_| inside) else {
                continue;
            };
            let v = v.trim_matches('"');
            match k {
                "StrictHostKeyChecking" => out.push("stricthostkeychecking true".into()),
                "ProxyJump" if v == "none" => {}
                _ => out.push(format!("{} {v}", k.to_lowercase())),
            }
        }
        out.join("\n")
    }
}

impl CommandRunner for Cluster {
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
        let failed = || {
            Ok(Output {
                exit: Exit::Code(255),
                ..output("")
            })
        };
        Box::pin(async move {
            match program.as_str() {
                "tailscale" => {
                    let mut peers = serde_json::Map::new();
                    for host in &self.hosts {
                        peers.insert(
                            host.clone(),
                            json!({
                                "HostName": host,
                                "DNSName": format!("{host}.example.ts.net."),
                                "Online": true,
                                "Created": "2026-10-12T15:00:00Z",
                                "Tags": [tailnet::HOST_TAG.to_string()],
                                "TailscaleIPs": [host_ip(host)],
                            }),
                        );
                    }
                    Ok(output(
                        json!({"MagicDNSSuffix": "example.ts.net", "Peer": peers}).to_string(),
                    ))
                }
                "ssh-keyscan" => {
                    let address = args.last().unwrap().clone();
                    let n: u8 = address.rsplit('.').next().unwrap().parse().unwrap();
                    let key = self
                        .scanned
                        .lock()
                        .unwrap()
                        .clone()
                        .unwrap_or_else(|| key(n));
                    Ok(output(format!("{address} {key}\n")))
                }
                "ssh" if args[0] == "-G" => Ok(output(self.effective(&args[1]))),
                "ssh" if args.last().is_some_and(|a| a == "true") => {
                    let name = &args[args.len() - 2];
                    if self.unreachable.contains(name) {
                        failed()
                    } else {
                        Ok(output(""))
                    }
                }
                "ssh" if args.iter().any(|a| a.contains("/etc/pve/nodes")) => {
                    let lines: Vec<String> = self
                        .hosts
                        .iter()
                        .map(|h| format!("{h} {}", key(slot(h))))
                        .collect();
                    Ok(output(lines.join("\n")))
                }
                "ssh" if args.iter().any(|a| a == "guests") => {
                    let name = &args[args.iter().position(|a| a == "bmac").unwrap() - 1];
                    if self.unreachable.contains(name) {
                        return failed();
                    }
                    let list = GuestList {
                        guests: vec![
                            prod1(),
                            Guest {
                                name: "stopped1".into(),
                                status: "stopped".into(),
                                ..prod1()
                            },
                        ],
                        staging_limits: [("mox2".into(), 2), ("mox3".into(), 1)].into(),
                    };
                    Ok(output(serde_json::to_vec(&list).unwrap()))
                }
                "ssh" if args.iter().any(|a| a == "guest-host-key") => Ok(output(
                    json!({
                        "guest": "prod1", "vmid": 100, "node": "mox2", "key": key(99),
                        "fingerprint": jump_ssh::key_fingerprint(&key(99)).unwrap(),
                    })
                    .to_string(),
                )),
                "ssh" if args.iter().any(|a| a.contains("hello-app")) => {
                    Ok(output("hello app installed\n"))
                }
                _ => failed(),
            }
        })
    }
}

struct Lab {
    _dir: tempfile::TempDir,
    cluster: Arc<Cluster>,
    env: Arc<Env>,
}

fn lab(hosts: &[&str], unreachable: &[&str]) -> Lab {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let ssh_dir = root.join("ssh");
    let cluster = Arc::new(Cluster {
        ssh_dir: ssh_dir.clone(),
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
        scanned: Mutex::new(None),
        unreachable: unreachable.iter().map(|h| h.to_string()).collect(),
        calls: Mutex::new(Vec::new()),
    });
    let tool = |name: &str| Some(PathBuf::from(format!("/usr/bin/{name}")));
    let env = Arc::new(Env {
        runner: cluster.clone(),
        fetch: Arc::new(Served),
        tools: Tools {
            ssh: tool("ssh"),
            ssh_keyscan: tool("ssh-keyscan"),
            xorriso: None,
            tailscale: tool("tailscale"),
            dpkg_deb: None,
        },
        cache_dir: root.join("cache"),
        state_dir: root.join("state"),
        iso_dir: root.join("isos"),
        ssh_dir,
        inventory_binary: None,
        packages_dir: None,
        link_wait: LinkWait {
            attempts: 1,
            interval_seconds: 1,
        },
        isos: Arc::new(Book::default()),
        clock: Arc::new(|| NOW * 1000),
        poll: Duration::from_millis(1),
        discovery_sources: discovery_sources(None),
        dashboard_port: 8006,
    });
    Lab {
        _dir: dir,
        cluster,
        env,
    }
}

impl Lab {
    fn config(&self) -> String {
        std::fs::read_to_string(self.env.ssh_dir.join("config")).unwrap_or_default()
    }

    fn known_hosts(&self) -> String {
        std::fs::read_to_string(self.env.ssh_dir.join("known_hosts")).unwrap_or_default()
    }

    /// Managed entries as an earlier run would have left them.
    fn pinned(&self, pins: &[(HostPin, &str)]) {
        std::fs::create_dir_all(&self.env.ssh_dir).unwrap();
        let known = self.env.ssh_dir.join("known_hosts");
        let mut config = self.config();
        let mut hosts = self.known_hosts();
        for (pin, name) in pins {
            let names = [name.to_string()];
            config = merge_config(&config, pin, &names, &known).unwrap();
            hosts = merge_known_hosts(&hosts, pin, &names);
        }
        std::fs::write(self.env.ssh_dir.join("config"), config).unwrap();
        std::fs::write(known, hosts).unwrap();
    }

    fn calls(&self) -> Vec<String> {
        self.cluster.calls.lock().unwrap().clone()
    }
}

fn host_pin(cluster: &str, host: &str) -> (HostPin, &'static str) {
    let name: &'static str = Box::leak(host.to_string().into_boxed_str());
    (
        HostPin {
            alias: workstation_ssh::pin_alias(cluster, host),
            address: host_ip(host),
            host_key: key(slot(host)),
            jump: None,
        },
        name,
    )
}

fn outputs(view: &bmac_proto::deployer::TaskView) -> String {
    view.events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::Output { text, .. } => Some(text.clone()),
            EventBody::Warning { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn tasks() -> Tasks {
    Tasks::new(Arc::new(NoSink), Arc::new(|| 0))
}

#[tokio::test]
async fn a_confirmed_host_key_is_pinned_and_the_others_follow_from_the_cluster() {
    let lab = lab(&["mox1", "mox2", "mox3"], &[]);
    let tasks = tasks();
    let id = start(&tasks, TaskKind::HostSsh, lab.env.clone()).id;

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Which host?");
    refused(
        &tasks,
        &id,
        &rid,
        values(json!({"cluster": "lab", "host": "mox1", "names": "ok bad/name"})),
        "names",
    )
    .await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"cluster": "lab", "host": "mox1", "names": "pve1"})),
    );

    let (rid, form) = request(&tasks, &id).await;
    let RequestForm::Confirm {
        message, severity, ..
    } = &form
    else {
        panic!("{form:?}")
    };
    assert_eq!(*severity, bmac_core::job::Severity::Warning);
    let fingerprint = jump_ssh::key_fingerprint(&key(1)).unwrap();
    assert!(message.contains(&fingerprint), "{message}");
    assert!(message.contains("100.64.0.1"));
    assert!(
        lab.config().is_empty(),
        "nothing is written before the check"
    );
    send(&tasks, &id, &rid, Answer::Confirm { confirmed: true });

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Set up the other hosts too?");
    assert_eq!(form.fields()[0].default, Some(json!(["mox2", "mox3"])));
    send(&tasks, &id, &rid, values(json!({"hosts": ["mox3"]})));

    let view = ended(&tasks, &id).await;
    assert_eq!(
        view.task.state,
        bmac_core::job::JobState::Completed,
        "{:#?}",
        view.events
    );
    let config = lab.config();
    assert!(config.contains("# BEGIN bmac managed host bmac-lab-mox1\nHost mox1 pve1\n"));
    assert!(config.contains("Host mox3\n    HostName 100.64.0.3\n"));
    assert!(!config.contains("bmac-lab-mox2"), "mox2 wasn't chosen");
    let known = lab.known_hosts();
    assert!(known.contains(&format!("bmac-lab-mox1 {}", key(1))));
    assert!(known.contains(&format!("bmac-lab-mox3 {}", key(3))));
    let scans = lab
        .calls()
        .iter()
        .filter(|c| c.starts_with("ssh-keyscan"))
        .count();
    assert_eq!(scans, 1, "only the confirmed host is scanned");
}

#[tokio::test]
async fn a_key_the_operator_doesnt_recognize_writes_nothing() {
    let lab = lab(&["mox1"], &[]);
    let tasks = tasks();
    let id = start(&tasks, TaskKind::HostSsh, lab.env.clone()).id;
    let (rid, _) = request(&tasks, &id).await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"cluster": "lab", "host": "mox1"})),
    );
    let (rid, _) = request(&tasks, &id).await;
    send(&tasks, &id, &rid, Answer::Confirm { confirmed: false });
    let view = ended(&tasks, &id).await;
    assert_eq!(view.task.state, bmac_core::job::JobState::Failed);
    assert!(lab.config().is_empty());
    assert!(lab.known_hosts().is_empty());
}

#[tokio::test]
async fn a_host_that_isnt_on_the_tailnet_is_named() {
    let lab = lab(&["mox1"], &[]);
    let tasks = tasks();
    let id = start(&tasks, TaskKind::HostSsh, lab.env.clone()).id;
    let (rid, _) = request(&tasks, &id).await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"cluster": "lab", "host": "mox7"})),
    );
    let view = ended(&tasks, &id).await;
    assert_eq!(view.task.state, bmac_core::job::JobState::Failed);
    assert!(
        view.task
            .message
            .unwrap()
            .contains("mox7 isn't on this workstation's tailnet")
    );
}

#[tokio::test]
async fn entries_for_a_departed_host_and_the_guests_behind_it_are_reported() {
    let lab = lab(&["mox1", "mox2"], &[]);
    let departed = host_pin("lab", "mox4");
    let guest = HostPin {
        alias: workstation_ssh::pin_alias("lab", "prod1"),
        address: "10.213.0.31".into(),
        host_key: key(99),
        jump: Some("mox4".into()),
    };
    lab.pinned(&[departed, (guest, "prod1")]);
    let tasks = tasks();
    let id = start(&tasks, TaskKind::HostSsh, lab.env.clone()).id;

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(
        form.fields()[0].default,
        Some(json!("lab")),
        "the cluster comes from the managed entries"
    );
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"cluster": "lab", "host": "mox1"})),
    );
    let (rid, _) = request(&tasks, &id).await;
    send(&tasks, &id, &rid, Answer::Confirm { confirmed: true });
    let (rid, _) = request(&tasks, &id).await;
    send(&tasks, &id, &rid, values(json!({"hosts": ["mox2"]})));

    let view = ended(&tasks, &id).await;
    assert_eq!(
        view.task.state,
        bmac_core::job::JobState::Completed,
        "{:#?}",
        view.events
    );
    let text = outputs(&view);
    assert!(text.contains("mox4 is no longer in the cluster"), "{text}");
    assert!(text.contains("prod1"), "{text}");
}

#[tokio::test]
async fn a_guest_is_pinned_to_its_attested_key_through_its_host() {
    let lab = lab(&["mox1", "mox2", "mox3"], &["mox1"]);
    lab.pinned(&[host_pin("lab", "mox2"), host_pin("lab", "mox1")]);
    let tasks = tasks();
    let id = start(&tasks, TaskKind::GuestSsh, lab.env.clone()).id;

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Which guest?");
    refused(
        &tasks,
        &id,
        &rid,
        values(json!({"guest": "stopped1"})),
        "guest",
    )
    .await;
    send(&tasks, &id, &rid, values(json!({"guest": "prod1"})));

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(
        form.fields()[0].default,
        Some(json!("mox2")),
        "the guest's own node"
    );
    refused(
        &tasks,
        &id,
        &rid,
        values(json!({"jump": "mox2", "alias": "mox3"})),
        "alias",
    )
    .await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"jump": "mox2", "alias": "prod1"})),
    );

    let view = ended(&tasks, &id).await;
    assert_eq!(
        view.task.state,
        bmac_core::job::JobState::Completed,
        "{:#?}",
        view.events
    );
    let config = lab.config();
    assert!(config.contains("# BEGIN bmac managed guest bmac-lab-prod1\nHost prod1\n"));
    assert!(config.contains("    ProxyJump mox2\n"));
    assert!(
        lab.known_hosts()
            .contains(&format!("bmac-lab-prod1 {}", key(99)))
    );
    let text = outputs(&view);
    assert!(text.contains("mox1 didn't answer"), "{text}");
    assert!(text.contains(&jump_ssh::key_fingerprint(&key(99)).unwrap()));
}

#[tokio::test]
async fn guest_ssh_needs_a_host_set_up_first() {
    let lab = lab(&["mox1"], &[]);
    let tasks = tasks();
    let id = start(&tasks, TaskKind::GuestSsh, lab.env.clone()).id;
    let view = ended(&tasks, &id).await;
    assert_eq!(view.task.state, bmac_core::job::JobState::Failed);
    assert!(
        view.task
            .message
            .unwrap()
            .contains("Set up SSH to a host first")
    );
}

#[tokio::test]
async fn the_hello_app_goes_out_only_with_a_certificate_for_every_name() {
    let lab = lab(&["mox1", "mox2", "mox3"], &[]);
    lab.pinned(&[host_pin("lab", "mox1")]);
    let names = [
        "example.com",
        "www.example.com",
        "stage1prod1.example.com",
        "stage2prod1.example.com",
    ];
    let (short_cert, short_key) = certificate(&names[..3], false);
    let (cert, key) = certificate(&names, false);

    let tasks = tasks();
    let id = start(&tasks, TaskKind::Hello, lab.env.clone()).id;
    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(
        form.fields()[0].options.len(),
        1,
        "only the running guest can be chosen: {form:?}"
    );
    send(&tasks, &id, &rid, values(json!({"guest": "prod1"})));

    let (rid, form) = request(&tasks, &id).await;
    let RequestForm::InputGroup { description, .. } = &form else {
        panic!("{form:?}")
    };
    assert!(
        description
            .as_deref()
            .unwrap()
            .contains("stage2prod1.example.com")
    );
    refused(
        &tasks,
        &id,
        &rid,
        values(json!({"ssh_name": "prod1", "cert": "not a certificate", "key": key})),
        "cert",
    )
    .await;
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"ssh_name": "prod1", "cert": short_cert, "key": short_key})),
    );
    let mut refusal = None;
    for _ in 0..1000 {
        refusal = tasks
            .view(&id)
            .unwrap()
            .events
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                EventBody::ValidationError { message, .. } => message.clone(),
                _ => None,
            });
        if refusal.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(
        refusal
            .unwrap()
            .contains("does not cover stage2prod1.example.com"),
        "a certificate missing a staging name is refused"
    );
    send(
        &tasks,
        &id,
        &rid,
        values(json!({"ssh_name": "prod1", "cert": cert, "key": key})),
    );

    let (rid, form) = request(&tasks, &id).await;
    assert_eq!(title(&form), "Deploy the hello app to prod1?");
    assert!(
        !lab.calls().iter().any(|c| c.contains("hello-app")),
        "nothing is sent before the confirmation"
    );
    send(&tasks, &id, &rid, Answer::Confirm { confirmed: true });

    let view = ended(&tasks, &id).await;
    assert_eq!(
        view.task.state,
        bmac_core::job::JobState::Completed,
        "{:#?}",
        view.events
    );
    assert!(outputs(&view).contains("hello app installed"));
    let secret = key.lines().nth(1).unwrap();
    let recorded = serde_json::to_string(&view.events).unwrap();
    assert!(
        !recorded.contains(secret),
        "the key isn't kept in the task's record"
    );
    assert!(recorded.contains("BEGIN CERTIFICATE"), "the certificate is");
    assert!(
        !lab.calls().iter().any(|c| c.contains(secret)),
        "the key never travels on a command line"
    );
}
