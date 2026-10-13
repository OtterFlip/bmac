// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! A simulated cluster for tests: every host has its own store, event hub,
//! jobs, runner, and peer listener on loopback, with node certificates from
//! a test CA issued the way Proxmox issues them, and all hosts share one
//! temporary pmxcfs directory. Jobs run the mock scripts through the real
//! supervisor.

#![allow(dead_code)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::{GuestName, HostSlot};
use bmac_pmxcfs::{Pmxcfs, RealFs};
use bmacd::auth::system_clock;
use bmacd::cluster::{ClusterView, Membership};
use bmacd::db::Database;
use bmacd::engine::events::EventHub;
use bmacd::jobs::{Jobs, JobsConfig};
use bmacd::mesh::tls::PeerTls;
use bmacd::mesh::tokens::Tokens;
use bmacd::mesh::{Mesh, MeshConfig};
use bmacd::operations::Registry;
use bmacd::runner::{EngineSink, Runner, RunnerConfig};
use bmacd::store::Store;
use bmacd::supervisor::{Supervisor, SupervisorConfig};
use bmacd::web::https::HttpsServer;
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, KeyPair};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub fn host(n: u8) -> HostSlot {
    HostSlot::new(n).unwrap()
}

pub struct Ca {
    pub pem: String,
    cert: rcgen::Certificate,
    key: KeyPair,
}

impl Ca {
    pub fn new(name: &str) -> Ca {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.distinguished_name.push(DnType::CommonName, name);
        let cert = params.self_signed(&key).unwrap();
        Ca {
            pem: cert.pem(),
            cert,
            key,
        }
    }

    /// A node certificate as Proxmox issues one: server use only.
    pub fn node(&self, name: &str) -> PeerTls {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec![name.to_string()]).unwrap();
        params.distinguished_name.push(DnType::CommonName, name);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let cert = params.signed_by(&key, &self.cert, &self.key).unwrap();
        PeerTls::new(
            self.pem.as_bytes(),
            cert.pem().as_bytes(),
            key.serialize_pem().as_bytes(),
        )
        .unwrap()
    }
}

pub struct Cluster(pub Mutex<Membership>);

impl ClusterView for Cluster {
    fn membership(&self) -> Result<Membership, String> {
        Ok(self.0.lock().unwrap().clone())
    }

    fn guest_owner(&self, _guest: GuestName) -> Result<Option<HostSlot>, String> {
        Ok(None)
    }

    fn placement(&self, _guest: GuestName) -> Result<Vec<HostSlot>, String> {
        Ok(Vec::new())
    }
}

/// A mock-script operation that runs on the duty host.
pub fn operation(id: &str, script: &str, lock: &str) -> Value {
    json!({
        "id": id, "title": id, "summary": "s", "description": "d",
        "category": "diagnostics", "kind": "script", "script": script,
        "min_role": "operator", "mutating": true, "destructive": false,
        "params": [], "locks": [lock],
        "target": {"rule": "duty"}, "cancel": {"rule": "script"}, "resume": "never"
    })
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

type Addresses = Arc<Mutex<HashMap<HostSlot, SocketAddr>>>;

pub struct Lab {
    pub dir: tempfile::TempDir,
    pub ca: Ca,
    pub pmxcfs: Pmxcfs,
    pub cluster: Arc<Cluster>,
    pub registry: Arc<Registry>,
    legacy: PathBuf,
    addresses: Addresses,
}

pub struct Node {
    pub mesh: Arc<Mesh>,
    pub store: Store,
    pub hub: Arc<EventHub>,
    pub jobs: Arc<Jobs>,
    pub runner: Arc<Runner>,
    tasks: Vec<JoinHandle<()>>,
}

impl Node {
    /// Opens links to the other members.
    pub fn link(&mut self) {
        self.tasks.push(self.mesh.spawn_links());
    }

    /// Starts catch-up with the other members.
    pub fn catch_up(&mut self) {
        self.tasks.push(self.mesh.spawn_catch_up());
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Lab {
    /// A cluster of `members`, all online, running the mock operations.
    pub fn new(members: &[u8]) -> Lab {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("legacy");
        let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        for sub in ["scripts/lib", "scripts/utilities"] {
            copy_tree(&crate_dir.join("../../legacy").join(sub), &legacy.join(sub));
        }
        copy_tree(&crate_dir.join("tests/mock-scripts"), &legacy.join("mock"));
        let operations = json!({
            "schema_version": 1,
            "operations": [
                operation("test.interactive", "mock/interactive.sh", "staging"),
                operation("test.long", "mock/long_running.sh", "capacity"),
            ],
        });
        let registry = Arc::new(Registry::from_parts(&operations.to_string(), Vec::new()).unwrap());
        Lab {
            pmxcfs: Pmxcfs::new(dir.path().join("pve"), Arc::new(RealFs)),
            dir,
            ca: Ca::new("Proxmox Virtual Environment test CA"),
            cluster: Arc::new(Cluster(Mutex::new(Membership {
                quorate: true,
                online: members.iter().map(|n| host(*n)).collect(),
                members: members.iter().map(|n| (host(*n), None)).collect(),
            }))),
            registry,
            legacy,
            addresses: Arc::default(),
        }
    }

    pub fn db_path(&self, n: u8) -> PathBuf {
        let path = self.dir.path().join(format!("mox{n}/bmac.db"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        path
    }

    pub fn address(&self, n: u8) -> SocketAddr {
        self.addresses.lock().unwrap()[&host(n)]
    }

    /// Host `n`'s view of the shared job records, with nothing running.
    pub fn jobs(&self, n: u8) -> Arc<Jobs> {
        Arc::new(Jobs::new(
            JobsConfig {
                pmxcfs: self.pmxcfs.clone(),
                dir: self.dir.path().join("pve/priv/bmac/jobs"),
                host: host(n),
                lease_ttl_ms: 60_000,
                claim_timeout_ms: 300_000,
                keep_finished: 500,
                lock_timeout: Duration::from_secs(30),
                clock: system_clock(),
            },
            self.registry.clone(),
            self.cluster.clone(),
        ))
    }

    /// Starts a simulated host, or restarts one on its existing database
    /// as a new incarnation. Its links stay closed until [`Node::link`].
    pub async fn node(&self, n: u8) -> Node {
        let store = Store::open(
            Database::open(&self.db_path(n)).unwrap(),
            format!("mox{n}"),
            0,
        )
        .await
        .unwrap();
        let hub = EventHub::with_epoch(format!("h{n}"));
        let tokens = Arc::new(Tokens::new(
            self.pmxcfs.clone(),
            self.dir.path().join("pve/priv/bmac/enrollment"),
            host(n),
            system_clock(),
        ));
        let addresses = self.addresses.clone();
        let mut config = MeshConfig::new(host(n), self.ca.node(&format!("mox{n}")));
        config.resolve = Arc::new(move |h, _| {
            addresses
                .lock()
                .unwrap()
                .get(&h)
                .copied()
                .into_iter()
                .collect()
        });
        config.connect_timeout = Duration::from_secs(2);
        config.keepalive = Duration::from_millis(300);
        config.idle_timeout = Duration::from_secs(2);
        config.links_every = Duration::from_millis(100);
        config.retry_max = Duration::from_millis(200);
        config.catch_up_every = Duration::from_millis(200);
        let mesh = Mesh::new(
            config.clone(),
            self.cluster.clone(),
            store.clone(),
            hub.clone(),
            tokens,
            system_clock(),
        );

        let jobs = self.jobs(n);
        let (sink, changes) = EngineSink::new(hub.clone(), store.clone());
        let mut sup = SupervisorConfig::system(system_clock(), Arc::new(|_: &str| false));
        sup.root = self.legacy.clone();
        sup.bash = PathBuf::from("bash");
        sup.work_root = self.legacy.parent().unwrap().join(format!("mox{n}/work"));
        sup.env = vec![(
            "PATH".into(),
            std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into()),
        )];
        let runner = Runner::new(
            jobs.clone(),
            Supervisor::new(sup, sink.clone()),
            sink,
            RunnerConfig {
                poll_every: Duration::from_millis(100),
                renew_every: Duration::from_secs(5),
                cancel_check_every: Duration::from_millis(100),
            },
        )
        .with_relay(mesh.clone());
        mesh.set_runner(runner.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.addresses
            .lock()
            .unwrap()
            .insert(host(n), listener.local_addr().unwrap());
        let server = tokio::spawn(
            HttpsServer::new(config.tls.server.clone(), mesh.router()).serve(listener),
        );
        let runner_task = runner.spawn(changes);
        Node {
            mesh,
            store,
            hub,
            jobs,
            runner,
            tasks: vec![server, runner_task],
        }
    }
}
