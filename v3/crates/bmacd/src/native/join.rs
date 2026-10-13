// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The `hosts.join` job (`DEPLOYMENT.md`, "Joining a host"): run on the
//! member where an administrator approves a join request, it sets the new
//! host up from here. It checks the request against the bundle this cluster
//! issued, stores the new host's facts, and then runs the new host's own
//! `bmacd native hosts.setup` over SSH with the bundle's setup key, relaying
//! its phases, messages, and questions into this job. The new host's setup
//! keeps its own checkpoints, so when the connection drops (the boot drills
//! restart the host) this job reconnects and runs it again, and it carries
//! on where it stopped.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::HostSlot;
use bmac_core::job::{PhaseStatus, RequestForm, Severity};
use bmac_exec::{Command, CommandRunner};
use bmac_proto::api::Role;
use bmac_proto::bmac_ui::{CompletedStatus, WorkflowEvent};
use bmac_proto::bootstrap::{BOOTSTRAP_DIR, BOOTSTRAP_FILE, BootstrapOrigin, HostBootstrap};
use bmac_proto::operations::{
    CancelRule, Category, Operation, OperationKind, ResumeRule, TargetRule,
};
use serde_json::{Map, Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use super::{Stop, Workflow};
use crate::joins::{JoinRecord, JoinStore};
use crate::mesh::tokens::{PendingRecord, TokenKind, Tokens};

pub const OPERATION: &str = "hosts.join";
pub const SSH: &str = "/usr/bin/ssh";
const BMACD: &str = "/usr/lib/bmac/bmacd";
const MEMBERS: &str = "/etc/pve/.members";
const COROSYNC_CONF: &str = "/etc/pve/corosync.conf";
const CLUSTER_KEY: &str = "/root/.ssh/id_rsa";
/// How long the new host may take to come back after a restart.
const RECONNECT_WAIT: Duration = Duration::from_secs(20 * 60);
const RECONNECT_POLL: Duration = Duration::from_secs(10);
/// Runs of the new host's setup before this job gives up on it.
const MAX_RUNS: u32 = 12;

/// This job's own phases; the new host's come between `connect` and
/// `finish`.
const APPROVE: (&str, &str) = ("approve", "Approve the request");
const CONNECT: (&str, &str) = ("connect", "Connect to the new host");
const FINISH: (&str, &str) = ("finish", "Finish the join");

pub fn operation() -> Operation {
    Operation {
        id: OPERATION.parse().expect("a valid operation id"),
        title: "Approve a host's join".into(),
        summary: "Approve a new host's join request and set it up from this host.".into(),
        description: "Shows the new host's identity to check against the Deployer, stores its \
                      facts, and runs its setup from here over SSH: storage, boot drills, the \
                      private network, joining the Proxmox cluster, and the end-state checks. \
                      Joining asks for the root@pam password, which is handed to Proxmox once \
                      and kept nowhere."
            .into(),
        category: Category::Hosts,
        kind: OperationKind::Native,
        script: None,
        min_role: Role::Admin,
        mutating: true,
        destructive: true,
        supports_dry_run: false,
        params: vec![
            serde_json::from_value(json!({
                "id": "host",
                "label": "Host",
                "type": "host",
                "help": "The host whose join request to approve.",
                "required": true,
                "suggest": "free_host_slots",
            }))
            .expect("a valid parameter"),
        ],
        locks: vec!["cluster-membership".into()],
        target: TargetRule::Any {},
        cancel: CancelRule::Phases {
            not_during: vec!["storage".into(), "cluster".into()],
        },
        resume: ResumeRule::Checkpoint,
        notes: Some(
            "Replaces the join path of v2's add_proxmox_host.sh, which ran from the \
             workstation."
                .into(),
        ),
    }
}

fn parse_args(args: &[String]) -> Result<HostSlot, String> {
    match args {
        [host] => host
            .parse()
            .map_err(|_| format!("{host} is not a BMAC host name")),
        _ => Err("usage: hosts.join HOST".into()),
    }
}

/// Whether `corosync.conf` configures a quorum device.
pub fn has_qdevice(corosync_conf: &str) -> bool {
    corosync_conf
        .lines()
        .any(|l| l.split_whitespace().collect::<Vec<_>>() == ["device", "{"])
}

/// The members `/etc/pve/.members` lists as offline.
pub fn offline_members(members: &str) -> Result<Vec<String>, String> {
    let value: Value =
        serde_json::from_str(members).map_err(|e| format!("{MEMBERS} is not JSON: {e}"))?;
    let nodes = value["nodelist"]
        .as_object()
        .ok_or_else(|| format!("{MEMBERS} lists no nodes"))?;
    Ok(nodes
        .iter()
        .filter(|(_, node)| node["online"] != 1)
        .map(|(name, _)| name.clone())
        .collect())
}

/// What the approval recorded about the request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct Approved {
    request_id: String,
    address: String,
    ssh_host_key: String,
}

/// How this job reaches the new host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub key: PathBuf,
    pub known_hosts: PathBuf,
    pub address: String,
}

impl Route {
    /// `ssh`'s arguments up to the remote command: only `key`, only the
    /// pinned host key, and no prompts.
    pub fn ssh_args(&self, host: HostSlot) -> Vec<String> {
        let option = |o: String| ["-o".to_string(), o];
        let mut args = vec![
            "-F".to_string(),
            "/dev/null".into(),
            "-i".into(),
            self.key.display().to_string(),
            "-l".into(),
            "root".into(),
        ];
        for o in [
            "IdentitiesOnly=yes".to_string(),
            format!("UserKnownHostsFile={}", self.known_hosts.display()),
            "GlobalKnownHostsFile=/dev/null".into(),
            "StrictHostKeyChecking=yes".into(),
            format!("HostKeyAlias={host}"),
            "BatchMode=yes".into(),
            "ConnectTimeout=15".into(),
            "ServerAliveInterval=15".into(),
            "ServerAliveCountMax=4".into(),
        ] {
            args.extend(option(o));
        }
        args.push(self.address.clone());
        args.push("--".into());
        args
    }
}

/// The new host's setup as one run reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    Completed(CompletedStatus, Option<String>, Option<String>),
    /// The connection closed before the run said how it ended.
    Dropped,
}

/// Adds `errors` to a relayed form, to ask it again.
fn with_errors(form: RequestForm, errors: &BTreeMap<String, String>) -> RequestForm {
    let note: Vec<String> = errors.iter().map(|(f, m)| format!("{f}: {m}")).collect();
    let note = note.join("\n");
    let add = |d: Option<String>| Some(d.map_or(note.clone(), |d| format!("{d}\n\n{note}")));
    match form {
        RequestForm::Input {
            title,
            description,
            context,
            field,
        } => RequestForm::Input {
            title,
            description: add(description),
            context,
            field,
        },
        RequestForm::InputGroup {
            title,
            description,
            context,
            layout,
            fields,
        } => RequestForm::InputGroup {
            title,
            description: add(description),
            context,
            layout,
            fields,
        },
        other => other,
    }
}

fn answer_line(request_id: &str, mut answer: Map<String, Value>) -> String {
    answer.insert("type".into(), "response".into());
    answer.insert("request_id".into(), request_id.into());
    let mut line = Value::Object(answer).to_string();
    line.push('\n');
    line
}

/// Relays one run of the new host's setup: its output on `lines`, its
/// answers to `answers`. A cancel of this job is passed on, and the run is
/// left to stop by itself.
pub async fn relay<R, W>(flow: &Workflow, lines: R, answers: &mut W) -> RunEnd
where
    R: tokio::io::AsyncBufRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = lines.lines();
    let mut asked: Option<(String, RequestForm)> = None;
    let mut error: Option<(String, Option<String>)> = None;
    let mut cancelled = false;
    loop {
        let line = tokio::select! {
            line = lines.next_line() => line,
            () = flow.token().cancelled(), if !cancelled => {
                cancelled = true;
                let _ = answers.write_all(b"{\"type\":\"cancel\"}\n").await;
                let _ = answers.flush().await;
                continue;
            }
        };
        let Ok(Some(line)) = line else {
            return RunEnd::Dropped;
        };
        let Ok(event) = serde_json::from_str::<WorkflowEvent>(&line) else {
            continue;
        };
        let (id, form) = match &event {
            WorkflowEvent::Protocol { .. }
            | WorkflowEvent::WorkflowStarted { .. }
            | WorkflowEvent::WorkflowMetadata { .. }
            | WorkflowEvent::Result { .. }
            | WorkflowEvent::NextStep { .. } => continue,
            WorkflowEvent::Error {
                message, details, ..
            } => {
                error = Some((message.clone(), details.clone()));
                continue;
            }
            WorkflowEvent::Completed {
                status, message, ..
            } => {
                let (message, details) = match (status, error.take()) {
                    (CompletedStatus::Failed, Some((m, d))) => (Some(m), d),
                    _ => (message.clone(), None),
                };
                return RunEnd::Completed(*status, message, details);
            }
            WorkflowEvent::ValidationError {
                request_id,
                field_errors,
                ..
            } => match &asked {
                Some((asked_id, form)) if asked_id == request_id => {
                    (request_id.clone(), with_errors(form.clone(), field_errors))
                }
                _ => continue,
            },
            other => match super::request_form(other) {
                Some(request) => request,
                None => {
                    flow.forward(other);
                    continue;
                }
            },
        };
        asked = Some((id.clone(), form.clone()));
        match flow.relay(form).await {
            Ok(answer) => {
                let _ = answers.write_all(answer_line(&id, answer).as_bytes()).await;
            }
            Err(_) => {
                cancelled = true;
                let _ = answers.write_all(b"{\"type\":\"cancel\"}\n").await;
            }
        }
        let _ = answers.flush().await;
    }
}

struct Join<'a> {
    flow: &'a Workflow,
    runner: &'a dyn CommandRunner,
    me: HostSlot,
    host: HostSlot,
    store: JoinStore,
    tokens: Tokens,
    settings: crate::settings::Settings,
    now: crate::auth::Clock,
}

impl Join<'_> {
    fn record(&self) -> Result<JoinRecord, Stop> {
        self.store
            .join(self.host)
            .map_err(|e| e.message)?
            .ok_or_else(|| {
                Stop::failed(format!(
                    "{} has no outstanding join bundle; issue one from the Hosts page.",
                    self.host
                ))
            })
    }

    fn pending(&self) -> Result<PendingRecord, Stop> {
        self.tokens
            .pending()
            .map_err(|e| e.message)?
            .into_iter()
            .rev()
            .find(|p| p.kind == TokenKind::Join && p.host == Some(self.host))
            .ok_or_else(|| {
                Stop::failed(format!(
                    "{} hasn't asked to join yet. Boot it from its host ISO and wait for its \
                     request.",
                    self.host
                ))
            })
    }

    fn check_cluster(&self) -> Result<(), Stop> {
        let corosync = std::fs::read_to_string(COROSYNC_CONF)
            .map_err(|e| Stop::failed(format!("cannot read {COROSYNC_CONF}: {e}")))?;
        if has_qdevice(&corosync) {
            return Err(Stop::failed(
                "This cluster has a QDevice. Remove it before adding a host and add it again \
                 afterwards, so the vote count stays right.",
            ));
        }
        let members = std::fs::read_to_string(MEMBERS)
            .map_err(|e| Stop::failed(format!("cannot read {MEMBERS}: {e}")))?;
        let offline = offline_members(&members).map_err(Stop::failed)?;
        if !offline.is_empty() {
            return Err(Stop::failed(format!(
                "Every member must be online to add a host; offline: {}.",
                offline.join(", ")
            )));
        }
        Ok(())
    }

    async fn approve(&self) -> Result<Approved, Stop> {
        self.record()?;
        self.check_cluster()?;
        let pending = self.pending()?;
        let line = pending.ssh_host_key.clone().unwrap_or_default();
        let (ssh_host_key, ssh_fingerprint) = crate::identity::ssh_host_key(&line)
            .ok_or_else(|| Stop::failed("The request carries no usable SSH host key."))?;
        let https = pending
            .https_fingerprint
            .clone()
            .unwrap_or_else(|| "none given".into());
        let host = self.host.to_string();
        let confirmed = self
            .flow
            .confirm(
                &format!("Approve {host}'s join?"),
                &format!(
                    "Compare these with the host card the Deployer showed when it built \
                     {host}'s ISO. If any differs, don't approve: something else is asking to \
                     join. Approving sets {host} up from {}, wiping the data disks in its \
                     layout.",
                    self.me
                ),
                Severity::Destructive,
                Some(&host),
                vec![
                    format!("SSH host key: {ssh_fingerprint}"),
                    format!("HTTPS certificate: {https}"),
                    format!(
                        "Asked from {} (received by {})",
                        pending.address, pending.received_by
                    ),
                ],
            )
            .await?;
        if !confirmed {
            return Err(Stop::failed(format!("{host}'s join wasn't approved.")));
        }
        Ok(Approved {
            request_id: pending.id,
            address: pending.address,
            ssh_host_key,
        })
    }

    /// The setup key and the pinned host key, in the job's work directory.
    fn setup_route(&self, approved: &Approved) -> Result<Option<Route>, Stop> {
        let source = self.store.key_path(self.host);
        let private = match std::fs::read(&source) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(Stop::failed(format!("cannot read the setup key: {e}"))),
        };
        let work = self.flow.work_dir();
        let key = work.join("setup.key");
        let known_hosts = work.join("known_hosts");
        let write = |path: &Path, bytes: &[u8]| {
            std::fs::write(path, bytes)
                .and_then(|()| {
                    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                })
                .map_err(|e| Stop::failed(format!("cannot write {}: {e}", path.display())))
        };
        write(&key, &private)?;
        write(
            &known_hosts,
            format!("{} {}\n", self.host, approved.ssh_host_key).as_bytes(),
        )?;
        Ok(Some(Route {
            key,
            known_hosts,
            address: approved.address.clone(),
        }))
    }

    /// Once the host has joined, the cluster's own trust: root's cluster
    /// key and the host key the new host published in pmxcfs.
    fn cluster_route(&self) -> Result<Option<Route>, Stop> {
        let known_hosts = PathBuf::from(format!("/etc/pve/nodes/{}/ssh_known_hosts", self.host));
        if !known_hosts.is_file() {
            return Ok(None);
        }
        let Some(facts) = self.settings.host(self.host).map_err(|e| e.message)? else {
            return Ok(None);
        };
        let Some(cluster) = self.settings.cluster().map_err(|e| e.message)? else {
            return Ok(None);
        };
        let address = bmac_settings::host_private_ip(self.host, &facts.values, &cluster.values)
            .map_err(|e| e.message)?;
        Ok(Some(Route {
            key: PathBuf::from(CLUSTER_KEY),
            known_hosts,
            address,
        }))
    }

    async fn ssh(&self, route: &Route, remote: &str) -> Result<String, String> {
        let command = Command::new(SSH)
            .args(route.ssh_args(self.host))
            .arg(remote)
            .timeout(Duration::from_secs(60));
        self.runner
            .run(command)
            .await
            .map(|o| o.stdout_text())
            .map_err(|e| e.to_string())
    }

    /// The first route that answers, waiting up to `wait` for one.
    async fn reach(&self, approved: &Approved, wait: Duration) -> Result<Route, Stop> {
        let started = std::time::Instant::now();
        let mut last = String::new();
        loop {
            self.flow.check()?;
            for route in [self.setup_route(approved)?, self.cluster_route()?]
                .into_iter()
                .flatten()
            {
                match self.ssh(&route, "true").await {
                    Ok(_) => return Ok(route),
                    Err(e) => last = e,
                }
            }
            if started.elapsed() >= wait {
                return Err(Stop::Failed {
                    message: format!("{} can't be reached over SSH.", self.host),
                    details: Some(last),
                });
            }
            tokio::time::sleep(RECONNECT_POLL).await;
        }
    }

    /// Reads the new host's bootstrap and checks it is the bundle this
    /// cluster issued for it.
    async fn bootstrap(&self, route: &Route, record: &JoinRecord) -> Result<HostBootstrap, Stop> {
        let path = Path::new(BOOTSTRAP_DIR).join(BOOTSTRAP_FILE);
        let text = self
            .ssh(route, &format!("cat {}", path.display()))
            .await
            .map_err(|e| Stop::Failed {
                message: format!("cannot read {}'s bootstrap file", self.host),
                details: Some(e),
            })?;
        let bootstrap: HostBootstrap = serde_json::from_str(&text)
            .map_err(|e| Stop::failed(format!("{}'s bootstrap file: {e}", self.host)))?;
        let BootstrapOrigin::Join(bundle) = &bootstrap.origin else {
            return Err(Stop::failed(format!(
                "{} was built to start a new cluster, not to join this one.",
                self.host
            )));
        };
        let ours = self
            .store
            .cluster()
            .map_err(|e| e.message)?
            .ok_or_else(|| Stop::failed("This cluster has no identity record."))?;
        let token = crate::mesh::tokens::token_hash(&bundle.join_token);
        if bootstrap.host != self.host
            || bundle.slot != self.host
            || bundle.cluster_uuid != ours.cluster_uuid
            || token.as_deref() != Some(record.ticket_sha256.as_str())
        {
            return Err(Stop::failed(format!(
                "{}'s bootstrap file isn't from the bundle this cluster issued for it.",
                self.host
            )));
        }
        Ok(bootstrap)
    }

    /// Stores the new host's facts and the Tailscale key it used.
    fn store_facts(&self, bootstrap: &HostBootstrap) -> Result<(), Stop> {
        let BootstrapOrigin::Join(bundle) = &bootstrap.origin else {
            unreachable!("checked by bootstrap()");
        };
        let current = self.settings.host(self.host).map_err(|e| e.message)?;
        if current.as_ref().map(|f| &f.values) != Some(&bootstrap.host_settings) {
            self.settings
                .save_host(
                    self.host,
                    bootstrap.host_settings.clone(),
                    current.map(|f| f.revision),
                    "join",
                    (self.now)(),
                )
                .map_err(|e| Stop::failed(format!("{}'s facts: {}", self.host, e.message)))?;
        }
        self.store
            .record_cluster(&bundle.cluster_uuid, &bundle.used_tailscale_key_sha256)
            .map_err(|e| e.message)?;
        self.flow.info(format!("Stored {}'s facts.", self.host));
        Ok(())
    }

    /// The certificate this host serves on 8006, which the new host checks
    /// before it sends the password.
    fn own_fingerprint(&self) -> Result<String, Stop> {
        let node = format!("/etc/pve/nodes/{}", self.me);
        let custom = PathBuf::from(format!("{node}/pveproxy-ssl.pem"));
        let path = if custom.is_file() {
            custom
        } else {
            PathBuf::from(format!("{node}/pve-ssl.pem"))
        };
        let pem = std::fs::read(&path)
            .map_err(|e| Stop::failed(format!("cannot read {}: {e}", path.display())))?;
        crate::web::tls::pem_fingerprint(&pem)
            .map_err(|e| Stop::failed(format!("{}: {e}", path.display())))
    }

    fn own_private_ip(&self) -> Result<Ipv4Addr, Stop> {
        let facts = self
            .settings
            .host(self.me)
            .map_err(|e| e.message)?
            .ok_or_else(|| Stop::failed(format!("{} has no facts stored.", self.me)))?;
        let cluster = self
            .settings
            .cluster()
            .map_err(|e| e.message)?
            .ok_or_else(|| Stop::failed("This cluster has no settings."))?;
        bmac_settings::host_private_ip(self.me, &facts.values, &cluster.values)
            .map_err(|e| e.message)?
            .parse()
            .map_err(|_| Stop::failed(format!("{}'s private address isn't IPv4.", self.me)))
    }

    /// One run of the new host's setup over `route`.
    async fn run_setup(&self, route: &Route) -> Result<RunEnd, Stop> {
        let remote = format!(
            "{BMACD} native {} --json --driven-by {} --join-address {} --join-fingerprint {}",
            super::setup::OPERATION,
            self.me,
            self.own_private_ip()?,
            self.own_fingerprint()?
        );
        let mut child = tokio::process::Command::new(SSH)
            .args(route.ssh_args(self.host))
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Stop::failed(format!("cannot start ssh: {e}")))?;
        let stderr = Arc::new(Mutex::new(Vec::<String>::new()));
        if let Some(err) = child.stderr.take() {
            let kept = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut lines = BufReader::new(err).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut kept = kept.lock().unwrap();
                    kept.push(line);
                    if kept.len() > 20 {
                        kept.remove(0);
                    }
                }
            });
        }
        let (Some(stdout), Some(mut stdin)) = (child.stdout.take(), child.stdin.take()) else {
            return Err(Stop::failed("ssh has no pipes"));
        };
        let end = relay(self.flow, BufReader::new(stdout), &mut stdin).await;
        drop(stdin);
        let _ = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
        if end == RunEnd::Dropped {
            let tail = stderr.lock().unwrap().join("\n");
            if !tail.trim().is_empty() {
                self.flow.warning(format!("ssh: {}", tail.trim()));
            }
        }
        Ok(end)
    }

    async fn setup(&self, approved: &Approved) -> Result<(), Stop> {
        for run in 1..=MAX_RUNS {
            let wait = if run == 1 {
                Duration::from_secs(60)
            } else {
                RECONNECT_WAIT
            };
            let route = self.reach(approved, wait).await?;
            match self.run_setup(&route).await? {
                RunEnd::Completed(CompletedStatus::Success, ..) => return Ok(()),
                RunEnd::Completed(CompletedStatus::Cancelled, ..) => return Err(Stop::Cancelled),
                RunEnd::Completed(CompletedStatus::Failed, message, details) => {
                    return Err(Stop::Failed {
                        message: format!(
                            "{}'s setup failed: {}",
                            self.host,
                            message.unwrap_or_else(|| "no reason given".into())
                        ),
                        details,
                    });
                }
                RunEnd::Dropped => {
                    self.flow.check()?;
                    self.flow.info(format!(
                        "Lost the connection to {}; carrying on once it answers again.",
                        self.host
                    ));
                }
            }
        }
        Err(Stop::failed(format!(
            "{}'s setup lost its connection {MAX_RUNS} times; resume this job to try again.",
            self.host
        )))
    }

    async fn finish(&self, approved: &Approved) -> Result<(), Stop> {
        let record = self.store.join(self.host).map_err(|e| e.message)?;
        if let Some(record) = &record {
            self.tokens
                .revoke(&record.ticket_sha256)
                .map_err(|e| e.message)?;
        }
        self.tokens
            .remove_pending(&approved.request_id)
            .map_err(|e| e.message)?;
        self.tokens
            .remove_pending_for(self.host)
            .map_err(|e| e.message)?;
        self.store.remove_join(self.host).map_err(|e| e.message)?;
        for file in ["setup.key", "known_hosts"] {
            let _ = std::fs::remove_file(self.flow.work_dir().join(file));
        }
        super::cluster::pin_from_settings(Path::new("/"), &self.settings).map_err(Stop::failed)?;
        let members = std::fs::read_to_string(MEMBERS)
            .map_err(|e| Stop::failed(format!("cannot read {MEMBERS}: {e}")))?;
        let offline = offline_members(&members).map_err(Stop::failed)?;
        if !offline.is_empty() {
            return Err(Stop::failed(format!(
                "These members are offline after the join: {}.",
                offline.join(", ")
            )));
        }
        self.flow.info(format!(
            "{} is a member; its bundle and setup key are gone.",
            self.host
        ));
        Ok(())
    }

    async fn phases(&self) -> Result<(), Stop> {
        let mut done = self.flow.checkpoints();
        let (approve_id, approve_label) = APPROVE;
        let approved: Approved = match done.get(approve_id) {
            Some(data) => serde_json::from_value(data.clone())
                .map_err(|e| Stop::failed(format!("the approval checkpoint: {e}")))?,
            None => {
                self.flow
                    .phase(approve_id, approve_label, PhaseStatus::Running);
                let approved = self.approve().await?;
                done.record(
                    approve_id,
                    serde_json::to_value(&approved).expect("serializes"),
                )?;
                approved
            }
        };
        self.flow
            .phase(approve_id, approve_label, PhaseStatus::Complete);

        let (connect_id, connect_label) = CONNECT;
        if !done.is_done(connect_id) {
            self.flow
                .phase(connect_id, connect_label, PhaseStatus::Running);
            let record = self.record()?;
            let route = self.reach(&approved, Duration::from_secs(60)).await?;
            let bootstrap = self.bootstrap(&route, &record).await?;
            self.store_facts(&bootstrap)?;
            done.record(connect_id, json!({}))?;
        }
        self.flow
            .phase(connect_id, connect_label, PhaseStatus::Complete);

        let (finish_id, finish_label) = FINISH;
        if !done.is_done("setup") {
            self.setup(&approved).await?;
            done.record("setup", json!({}))?;
        }
        self.flow
            .phase(finish_id, finish_label, PhaseStatus::Running);
        self.finish(&approved).await?;
        done.record(finish_id, json!({}))?;
        self.flow
            .phase(finish_id, finish_label, PhaseStatus::Complete);
        self.flow.result(json!({ "host": self.host.to_string() }));
        Ok(())
    }
}

pub async fn run(flow: &Workflow, args: &[String]) -> ExitCode {
    flow.begin(
        OPERATION,
        "Approve a host's join",
        "Sets a new host up from this one and adds it to the cluster.",
    );
    let outcome = async {
        let host = parse_args(args)?;
        let me = crate::engine::listen::node_name()
            .parse::<HostSlot>()
            .map_err(|_| {
                Stop::failed(format!(
                    "{} is not a BMAC host name",
                    crate::engine::listen::node_name()
                ))
            })?;
        if host == me {
            return Err(Stop::failed(format!("{me} is already this host.")));
        }
        let now = crate::auth::system_clock();
        let runner = bmac_exec::RealRunner;
        Join {
            flow,
            runner: &runner,
            me,
            host,
            store: JoinStore::system(),
            tokens: Tokens::system(me, now.clone()),
            settings: crate::settings::Settings::system(),
            now,
        }
        .phases()
        .await
    }
    .await;
    flow.finish(&outcome)
}

#[cfg(test)]
#[path = "join_tests.rs"]
mod tests;
