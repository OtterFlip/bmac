// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The export listener on 8445 (`BACKUP.md`, "Trusted peers" and
//! "Restore"). The handshake lets through a trusted peer's certificate, or
//! a host certificate that leads to the CA of a cluster this appliance is
//! enrolled with; anything else is refused before a request is read, and
//! the refusal is recorded. A peer reads every published PIT; a cluster
//! host reads only what a live restore grant names. A host a live recovery
//! grant names (a cluster rebuilt on new hardware) reads its namespace's
//! recovery manifest and every PIT of it, and may bind the namespace to its
//! cluster. Untrusting a peer, a cluster's enrollment going, or a recovery
//! grant ending closes its open connections. A cluster host's connection
//! with ALPN `bmac-source/1` is the host's backup connection, handed to
//! [`Tunnels`].

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::{DefaultBodyLimit, Extension, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bmac_core::SnapshotGuid;
use bmac_exec::BoxFuture;
use bmac_proto::api::{ApiError, ErrorCode};
use bmac_proto::appliance::{
    ExportEstimate, ExportRequest, GrantedBase, GrantedIndex, GrantedRequest, PeerIndex, PeerPit,
    PeerVolume, RecoveryBind, RecoveryIndex, RecoveryRequest, SOURCE_ALPN, TrustedPeer,
    on_appliance, trusted_peer,
};
use bytes::Bytes;
use http_body::Frame;
use hyper_util::rt::{TokioIo, TokioTimer};
use rustls::client::danger::HandshakeSignatureValid;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;
use uuid::Uuid;

use crate::audit::Audit;
use crate::client::{fingerprint, provider};
use crate::grants::{Grant, Grants};
use crate::journal::{Journal, Metadata, StoredPit};
use crate::peers::{Peers, Refusal};
use crate::pool::{Pool, ZFS};
use crate::state::{self, Identity, Paths};
use crate::tunnels::{self, Tunnels};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 64;
/// How often open connections are checked against the trust list.
const RECHECK: Duration = Duration::from_secs(5);
const CHUNK: usize = 256 * 1024;
const MAX_STDERR: usize = 4096;
const MAX_BODY: usize = 64 * 1024;

struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Who a verified connection belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Client {
    Peer {
        appliance_uuid: String,
        fingerprint: String,
    },
    Cluster {
        cluster_uuid: String,
        fingerprint: String,
    },
    /// The host a live recovery grant names.
    Recovery {
        cluster_uuid: String,
        grant_id: String,
        fingerprint: String,
    },
}

impl Client {
    fn label(&self) -> String {
        match self {
            Client::Peer { appliance_uuid, .. } => format!("peer {appliance_uuid}"),
            Client::Cluster { cluster_uuid, .. } => format!("cluster {cluster_uuid}"),
            Client::Recovery {
                cluster_uuid,
                grant_id,
                ..
            } => format!("recovery of {cluster_uuid} under grant {grant_id}"),
        }
    }
}

/// A cluster this appliance is enrolled with, by its CA's fingerprint.
#[derive(Debug, Clone)]
pub struct ClusterCa {
    pub cluster_uuid: String,
    pub ca_fingerprint: String,
}

/// Whether `end_entity` leads, through `intermediates`, to the CA with
/// `ca_fingerprint`, which must be among them.
fn leads_to(
    provider: &CryptoProvider,
    end_entity: &CertificateDer<'_>,
    intermediates: &[CertificateDer<'_>],
    ca_fingerprint: &str,
    now: UnixTime,
) -> bool {
    let Some(ca) = intermediates
        .iter()
        .find(|der| fingerprint(der).eq_ignore_ascii_case(ca_fingerprint))
    else {
        return false;
    };
    let Ok(anchor) = webpki::anchor_from_trusted_cert(ca) else {
        return false;
    };
    let Ok(cert) = webpki::EndEntityCert::try_from(end_entity) else {
        return false;
    };
    let anchors = [anchor];
    // A Proxmox host certificate may be issued for server use only; the
    // CA's signature is what identifies the host.
    [
        webpki::KeyUsage::client_auth(),
        webpki::KeyUsage::server_auth(),
    ]
    .into_iter()
    .any(|usage| {
        cert.verify_for_usage(
            provider.signature_verification_algorithms.all,
            &anchors,
            intermediates,
            now,
            usage,
            None,
            None,
        )
        .is_ok()
    })
}

/// One connection's handshake check, with what it decided.
#[derive(Debug)]
struct ExportVerifier {
    peers: Vec<TrustedPeer>,
    clusters: Vec<ClusterCa>,
    /// `(grant_id, cluster, host fingerprint)` of the live recovery grants.
    recoveries: Vec<(String, String, String)>,
    provider: Arc<CryptoProvider>,
    outcome: Mutex<Option<Result<Client, (String, String)>>>,
}

impl ExportVerifier {
    fn decide(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<Client, (String, String)> {
        let presented = fingerprint(end_entity);
        if let Some(peer) = trusted_peer(&self.peers, &presented) {
            return Ok(Client::Peer {
                appliance_uuid: peer.appliance_uuid.clone(),
                fingerprint: presented,
            });
        }
        if let Some((grant_id, cluster_uuid, _)) = self
            .recoveries
            .iter()
            .find(|(_, _, f)| f.eq_ignore_ascii_case(&presented))
        {
            return Ok(Client::Recovery {
                cluster_uuid: cluster_uuid.clone(),
                grant_id: grant_id.clone(),
                fingerprint: presented,
            });
        }
        for cluster in &self.clusters {
            if leads_to(
                &self.provider,
                end_entity,
                intermediates,
                &cluster.ca_fingerprint,
                now,
            ) {
                return Ok(Client::Cluster {
                    cluster_uuid: cluster.cluster_uuid.clone(),
                    fingerprint: presented,
                });
            }
        }
        Err((presented, state::claimed(end_entity)))
    }
}

impl ClientCertVerifier for ExportVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let decided = self.decide(end_entity, intermediates, now);
        let ok = decided.is_ok();
        *self.outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(decided);
        if ok {
            Ok(ClientCertVerified::assertion())
        } else {
            Err(rustls::Error::General("refused".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2 is not offered".into()))
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// What a request may read, decided from the journal and the grants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportPlan {
    pub snapshot: String,
    pub guid: SnapshotGuid,
    /// The incremental's base snapshot and its GUID.
    pub base: Option<(String, SnapshotGuid)>,
    pub resume_token: Option<String>,
    pub grant_id: Option<String>,
}

fn refuse(code: ErrorCode, message: impl Into<String>) -> ApiError {
    ApiError::new(code, message)
}

/// Decides an export: a peer reads any published PIT; a cluster host only
/// its own cluster's PITs that a live grant names. The base must be an
/// earlier published PIT of the same VM, volume, and GUID.
pub fn plan_export(
    journal: &Journal,
    grants: &Grants,
    client: &Client,
    request: &ExportRequest,
    now_ms: i64,
) -> Result<ExportPlan, ApiError> {
    let grant_id = match client {
        Client::Peer { .. } => None,
        Client::Cluster { cluster_uuid, .. } => {
            if *cluster_uuid != request.cluster_uuid {
                return Err(refuse(
                    ErrorCode::Forbidden,
                    "a host reads only its own cluster's PITs",
                ));
            }
            let token = request.grant.as_deref().ok_or_else(|| {
                refuse(
                    ErrorCode::Forbidden,
                    "reading a PIT needs a restore grant from the appliance administrator",
                )
            })?;
            let grant = grants
                .check(token, cluster_uuid, &request.pit_uuid, now_ms)
                .map_err(|e| refuse(ErrorCode::Forbidden, e))?;
            Some(grant.grant_id.clone())
        }
        Client::Recovery {
            cluster_uuid,
            grant_id,
            ..
        } => {
            if *cluster_uuid != request.cluster_uuid {
                return Err(refuse(
                    ErrorCode::Forbidden,
                    "a recovery reads only its grant's cluster",
                ));
            }
            let token = request.grant.as_deref().ok_or_else(|| {
                refuse(
                    ErrorCode::Forbidden,
                    "reading a PIT needs the recovery grant",
                )
            })?;
            grants
                .recovery(token, cluster_uuid, grant_id, now_ms)
                .map_err(|e| refuse(ErrorCode::Forbidden, e))?;
            Some(grant_id.clone())
        }
    };
    let pit = journal
        .pit(&request.cluster_uuid, &request.pit_uuid)
        .ok_or_else(|| refuse(ErrorCode::NotFound, "that PIT isn't published here"))?;
    let volume = pit
        .volume(&request.volume_uuid)
        .ok_or_else(|| refuse(ErrorCode::NotFound, "that PIT has no such volume"))?;
    let base = match request.from_guid {
        None => None,
        Some(guid) => {
            let found = journal
                .pits_of(&request.cluster_uuid, &pit.vm_uuid)
                .filter(|p| p.taken_s < pit.taken_s)
                .find_map(|p| {
                    p.volume(&request.volume_uuid)
                        .filter(|v| v.guid == guid)
                        .map(|v| p.snapshot(v))
                })
                .ok_or_else(|| {
                    refuse(
                        ErrorCode::NotFound,
                        "no earlier published snapshot of that volume has that GUID",
                    )
                })?;
            Some((found, guid))
        }
    };
    Ok(ExportPlan {
        snapshot: pit.snapshot(volume),
        guid: volume.guid,
        base,
        resume_token: request.resume_token.clone(),
        grant_id,
    })
}

/// `zfs send`'s arguments: large blocks, compressed and embedded blocks
/// as stored, never `-R` or `-p`.
pub fn send_args(plan: &ExportPlan, dry_run: bool) -> Vec<String> {
    let mut args = vec!["send".to_string()];
    if dry_run {
        args.push("-nP".into());
    }
    if let Some(token) = &plan.resume_token {
        args.extend(["-t".to_string(), token.clone()]);
        return args;
    }
    args.extend(["-L", "-c", "-e"].map(String::from));
    if let Some((base, _)) = &plan.base {
        args.extend(["-i".to_string(), base.clone()]);
    }
    args.push(plan.snapshot.clone());
    args
}

/// The fields of `zfs send -nv -t`'s token dump that matter.
fn token_fields(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let (k, v) = l.trim().split_once(" = ")?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect()
}

pub struct Exporter {
    pub paths: Paths,
    pub identity: Identity,
    pub metadata: Metadata,
    pub audit: Arc<Audit>,
    pub pool: Arc<dyn Pool>,
    pub unlocked: Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>,
    pub now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
    pub zfs: PathBuf,
    /// The cluster hosts' backup connections.
    pub tunnels: Arc<Tunnels>,
    cert: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    provider: Arc<CryptoProvider>,
    open: Mutex<BTreeMap<u64, (Client, AbortHandle)>>,
    next: AtomicU64,
}

impl Exporter {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        paths: Paths,
        identity: Identity,
        metadata: Metadata,
        audit: Arc<Audit>,
        pool: Arc<dyn Pool>,
        unlocked: Arc<dyn Fn() -> BoxFuture<'static, bool> + Send + Sync>,
        now_ms: Arc<dyn Fn() -> i64 + Send + Sync>,
        cert_pem: &str,
        key_pem: &str,
    ) -> Result<Exporter, String> {
        let cert = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("appliance certificate: {e}"))?;
        let key = PrivateKeyDer::from_pem_slice(key_pem.as_bytes())
            .map_err(|e| format!("appliance key: {e}"))?;
        Ok(Exporter {
            paths,
            identity,
            metadata,
            audit,
            pool,
            unlocked,
            now_ms,
            zfs: PathBuf::from(ZFS),
            tunnels: Arc::default(),
            cert,
            key,
            provider: provider(),
            open: Mutex::default(),
            next: AtomicU64::new(0),
        })
    }

    fn clusters(&self) -> Vec<ClusterCa> {
        state::enrollments(&self.paths)
            .unwrap_or_default()
            .into_iter()
            .map(|e| ClusterCa {
                cluster_uuid: e.cluster_uuid,
                ca_fingerprint: e.proxmox_ca_fingerprint,
            })
            .collect()
    }

    fn recoveries(&self) -> Vec<(String, String, String)> {
        Grants::load(&self.metadata)
            .unwrap_or_default()
            .recovery_hosts((self.now_ms)())
    }

    fn peers(&self) -> Peers {
        Peers::load(&self.metadata).unwrap_or_default()
    }

    /// Serves `listener`, and closes connections whose trust has gone.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        let _watcher = AbortOnDrop(tokio::spawn(self.clone().recheck()));
        let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
        loop {
            let (tcp, from) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    eprintln!("bmac-backupd: exports: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let exporter = self.clone();
            tokio::spawn(async move {
                exporter.connection(tcp, from).await;
                drop(permit);
            });
        }
    }

    async fn recheck(self: Arc<Self>) {
        loop {
            tokio::time::sleep(RECHECK).await;
            let peers = self.peers();
            let clusters = self.clusters();
            let recoveries = self.recoveries();
            let open = self.open.lock().unwrap_or_else(|p| p.into_inner());
            for (client, task) in open.values() {
                let still = match client {
                    Client::Peer { fingerprint, .. } => peers.trusted(fingerprint).is_some(),
                    Client::Cluster { cluster_uuid, .. } => {
                        clusters.iter().any(|c| &c.cluster_uuid == cluster_uuid)
                    }
                    Client::Recovery { grant_id, .. } => {
                        recoveries.iter().any(|(g, _, _)| g == grant_id)
                    }
                };
                if !still {
                    task.abort();
                }
            }
        }
    }

    fn config(&self, verifier: Arc<ExportVerifier>) -> Result<Arc<ServerConfig>, rustls::Error> {
        let mut config = ServerConfig::builder_with_provider(self.provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(verifier)
            .with_single_cert(self.cert.clone(), self.key.clone_key())?;
        config.alpn_protocols = vec![SOURCE_ALPN.to_vec(), b"http/1.1".to_vec()];
        // Every connection is verified afresh: no resumed sessions.
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        config.send_tls13_tickets = 0;
        Ok(Arc::new(config))
    }

    async fn connection(self: Arc<Self>, tcp: TcpStream, from: SocketAddr) {
        let _ = tcp.set_nodelay(true);
        let verifier = Arc::new(ExportVerifier {
            peers: self.peers().list(),
            clusters: self.clusters(),
            recoveries: self.recoveries(),
            provider: self.provider.clone(),
            outcome: Mutex::default(),
        });
        let Ok(config) = self.config(verifier.clone()) else {
            return;
        };
        let accepted = tokio::time::timeout(
            HANDSHAKE_TIMEOUT,
            tokio_rustls::TlsAcceptor::from(config).accept(tcp),
        )
        .await;
        let outcome = verifier
            .outcome
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let (tls, client) = match (accepted, outcome) {
            (Ok(Ok(tls)), Some(Ok(client))) => (tls, client),
            (_, Some(Err((fingerprint, claimed)))) => {
                self.refused(fingerprint, claimed, from).await;
                return;
            }
            _ => return,
        };
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut client = client;
        let serve = if tls.get_ref().1.alpn_protocol() == Some(SOURCE_ALPN) {
            let Some((cluster, host)) = self.backup_connection(&client, &tls, from) else {
                return;
            };
            client = Client::Cluster {
                cluster_uuid: cluster.clone(),
                fingerprint: match &client {
                    Client::Peer { fingerprint, .. }
                    | Client::Cluster { fingerprint, .. }
                    | Client::Recovery { fingerprint, .. } => fingerprint.clone(),
                },
            };
            let exporter = self.clone();
            tokio::spawn(async move {
                eprintln!("bmac-backupd: {host} of cluster {cluster} connected from {from}");
                let ended = exporter.tunnels.serve(&cluster, host, tls).await;
                let why = ended.err().unwrap_or_else(|| "closed".into());
                eprintln!("bmac-backupd: {host} of cluster {cluster}: the connection ended: {why}");
            })
        } else {
            let app = router(self.clone()).layer(Extension(client.clone()));
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .timer(TokioTimer::new())
                    .header_read_timeout(HANDSHAKE_TIMEOUT)
                    .serve_connection(
                        TokioIo::new(tls),
                        hyper_util::service::TowerToHyperService::new(app),
                    )
                    .await;
            })
        };
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id, (client, serve.abort_handle()));
        let _ = serve.await;
        self.open
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&id);
    }

    /// The cluster and host of a backup connection, if it may be one: a
    /// cluster host's, naming its slot.
    fn backup_connection(
        &self,
        client: &Client,
        tls: &tokio_rustls::server::TlsStream<TcpStream>,
        from: SocketAddr,
    ) -> Option<(String, bmac_core::HostSlot)> {
        let chain = tls.get_ref().1.peer_certificates().unwrap_or_default();
        let cluster = match client {
            Client::Cluster { cluster_uuid, .. } => Some(cluster_uuid.clone()),
            // The recovered cluster's host while its recovery grant still
            // names it: once the namespace is bound, its certificate is
            // also that cluster's.
            Client::Recovery { cluster_uuid, .. } => {
                chain.split_first().and_then(|(cert, rest)| {
                    self.clusters()
                        .into_iter()
                        .find(|c| {
                            c.cluster_uuid == *cluster_uuid
                                && leads_to(
                                    &self.provider,
                                    cert,
                                    rest,
                                    &c.ca_fingerprint,
                                    UnixTime::now(),
                                )
                        })
                        .map(|c| c.cluster_uuid)
                })
            }
            Client::Peer { .. } => None,
        };
        let Some(cluster_uuid) = cluster else {
            eprintln!(
                "bmac-backupd: exports: {} from {from} asked for a backup connection; only \
                 cluster hosts open one",
                client.label()
            );
            return None;
        };
        let host = chain.first().and_then(|cert| tunnels::host_of(cert));
        if host.is_none() {
            eprintln!(
                "bmac-backupd: exports: a certificate of cluster {cluster_uuid} from {from} names \
                 no host"
            );
        }
        Some((cluster_uuid, host?))
    }

    async fn refused(&self, fingerprint: String, claimed: String, from: SocketAddr) {
        let now = (self.now_ms)();
        let Ok(_guard) = self.metadata.lock().await else {
            return;
        };
        let mut peers = self.peers();
        let refusal = Refusal {
            fingerprint,
            claimed,
            from: from.to_string(),
            at_ms: now,
        };
        eprintln!(
            "bmac-backupd: exports: refused {} ({}) from {from}: not a trusted peer, cluster host, or recovery host",
            refusal.claimed, refusal.fingerprint
        );
        let audit = peers.refused(refusal.clone());
        let _ = peers.save(&self.metadata);
        if audit {
            let _ = self.audit.record(
                now,
                "refused_peer",
                "bmac-backupd",
                serde_json::to_value(&refusal).unwrap_or_default(),
            );
        }
    }

    async fn ready(&self) -> Result<Journal, ApiError> {
        if !(self.unlocked)().await {
            return Err(refuse(ErrorCode::Unavailable, "the pool is locked"));
        }
        self.metadata
            .load()
            .ok()
            .flatten()
            .ok_or_else(|| refuse(ErrorCode::Unavailable, "the journal can't be read"))
    }

    /// Notes when a peer last read from this appliance, once a minute at most.
    async fn served(&self, client: &Client) {
        let Client::Peer { appliance_uuid, .. } = client else {
            return;
        };
        let now = (self.now_ms)();
        let Ok(_guard) = self.metadata.lock().await else {
            return;
        };
        let mut peers = self.peers();
        if let Some(entry) = peers
            .peers
            .iter_mut()
            .find(|e| &e.peer.appliance_uuid == appliance_uuid)
            && entry.last_served_ms.is_none_or(|t| now - t > 60_000)
        {
            entry.last_served_ms = Some(now);
            let _ = peers.save(&self.metadata);
        }
    }

    async fn zfs_text(&self, args: &[String]) -> Result<String, String> {
        let output = tokio::process::Command::new(&self.zfs)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("cannot run zfs: {e}"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    /// The snapshots still have the journal's GUIDs, and a resume token
    /// continues this very stream.
    async fn check_live(&self, plan: &ExportPlan) -> Result<(), ApiError> {
        let mut names = vec![plan.snapshot.clone()];
        names.extend(plan.base.iter().map(|(b, _)| b.clone()));
        let live = self
            .pool
            .guids(&names)
            .await
            .map_err(|e| refuse(ErrorCode::Unavailable, e))?;
        if live.get(&plan.snapshot) != Some(&plan.guid)
            || plan
                .base
                .as_ref()
                .is_some_and(|(b, g)| live.get(b) != Some(g))
        {
            return Err(refuse(
                ErrorCode::Conflict,
                "the snapshot isn't the one the journal recorded",
            ));
        }
        if let Some(token) = &plan.resume_token {
            let text = self
                .zfs_text(&["send".into(), "-nv".into(), "-t".into(), token.clone()])
                .await
                .map_err(|e| refuse(ErrorCode::Invalid, format!("the resume token: {e}")))?;
            let fields = token_fields(&text);
            let to_ok = fields.get("toname") == Some(&plan.snapshot)
                && fields.get("toguid").and_then(|g| parse_guid(g)) == Some(plan.guid.get());
            let from = fields.get("fromguid").and_then(|g| parse_guid(g));
            let from_ok = from == plan.base.as_ref().map(|(_, g)| g.get());
            if !(to_ok && from_ok) {
                return Err(refuse(
                    ErrorCode::Invalid,
                    "the resume token is for another stream",
                ));
            }
        }
        Ok(())
    }
}

/// `zfs` prints GUIDs in decimal or, in token dumps, hex.
fn parse_guid(text: &str) -> Option<u64> {
    match text.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

fn api_error(error: ApiError) -> Response {
    let status = axum::http::StatusCode::from_u16(error.code.http_status())
        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(error)).into_response()
}

fn router(exporter: Arc<Exporter>) -> Router {
    Router::new()
        .route(on_appliance::PEER_INDEX, get(peer_index))
        .route(on_appliance::EXPORT, post(export))
        .route(on_appliance::GRANTED, post(granted))
        .route(on_appliance::RECOVERY_INDEX, post(recovery_index))
        .route(on_appliance::RECOVERY_BIND, post(recovery_bind))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .with_state(exporter)
}

fn peer_volumes(p: &StoredPit) -> Vec<PeerVolume> {
    p.volumes
        .iter()
        .map(|v| PeerVolume {
            volume_uuid: v.volume_uuid,
            guid: v.guid,
            volsize: v.volsize,
            volblocksize: v.volblocksize,
        })
        .collect()
}

fn peer_pit(p: &StoredPit, metadata: &Metadata) -> Option<PeerPit> {
    let manifest = metadata.manifest(&p.cluster_uuid, &p.pit_uuid).ok()??;
    Some(PeerPit {
        cluster_uuid: p.cluster_uuid.clone(),
        vm_uuid: p.vm_uuid,
        branch_uuid: p.branch_uuid,
        pit_uuid: p.pit_uuid,
        parent: p.parent,
        name: p.name.clone(),
        taken_s: p.taken_s,
        volumes: peer_volumes(p),
        manifest,
    })
}

/// Every published PIT, with its manifest; for trusted peers only.
pub fn index_of(journal: &Journal, metadata: &Metadata) -> PeerIndex {
    PeerIndex {
        appliance_uuid: journal.appliance_uuid,
        revision: journal.revision,
        pits: journal
            .pits
            .iter()
            .filter_map(|p| peer_pit(p, metadata))
            .collect(),
    }
}

/// What a grant lets its cluster read: the granted PITs with their
/// manifests, and every earlier published PIT of their VMs as possible
/// incremental bases, metadata only.
pub fn granted_index(journal: &Journal, metadata: &Metadata, grant: &Grant) -> GrantedIndex {
    let pits: Vec<PeerPit> = grant
        .pits
        .iter()
        .filter_map(|id| journal.pit(&grant.cluster_uuid, id))
        .filter_map(|p| peer_pit(p, metadata))
        .collect();
    let mut bases: Vec<GrantedBase> = Vec::new();
    for granted in &pits {
        for p in journal.pits_of(&grant.cluster_uuid, &granted.vm_uuid) {
            if p.taken_s < granted.taken_s && !bases.iter().any(|b| b.pit_uuid == p.pit_uuid) {
                bases.push(GrantedBase {
                    vm_uuid: p.vm_uuid,
                    branch_uuid: p.branch_uuid,
                    pit_uuid: p.pit_uuid,
                    parent: p.parent,
                    taken_s: p.taken_s,
                    volumes: peer_volumes(p),
                });
            }
        }
    }
    bases.sort_by_key(|b| (b.vm_uuid, b.taken_s, b.pit_uuid));
    GrantedIndex {
        appliance_uuid: journal.appliance_uuid,
        grant_id: grant.grant_id.clone(),
        expires_at_ms: grant.expires_at_ms,
        pits,
        bases,
    }
}

async fn granted(
    State(exporter): State<Arc<Exporter>>,
    Extension(client): Extension<Client>,
    Json(request): Json<GrantedRequest>,
) -> Response {
    let Client::Cluster { cluster_uuid, .. } = &client else {
        return api_error(refuse(ErrorCode::Forbidden, "grants are for cluster hosts"));
    };
    if *cluster_uuid != request.cluster_uuid {
        return api_error(refuse(
            ErrorCode::Forbidden,
            "a host reads only its own cluster's PITs",
        ));
    }
    let journal = match exporter.ready().await {
        Ok(j) => j,
        Err(e) => return api_error(e),
    };
    let grants = Grants::load(&exporter.metadata).unwrap_or_default();
    let now = (exporter.now_ms)();
    let grant = match grants.live_for(&request.grant, cluster_uuid, now) {
        Ok(grant) => grant,
        Err(e) => return api_error(refuse(ErrorCode::Forbidden, e)),
    };
    exporter.served(&client).await;
    Json(granted_index(&journal, &exporter.metadata, grant)).into_response()
}

/// The recovering host's grant, checked against its request.
fn recovery_grant(
    exporter: &Exporter,
    client: &Client,
    cluster: &str,
    token: &str,
) -> Result<Grant, ApiError> {
    let Client::Recovery {
        cluster_uuid,
        grant_id,
        ..
    } = client
    else {
        return Err(refuse(
            ErrorCode::Forbidden,
            "only the host a recovery grant names may recover",
        ));
    };
    if cluster_uuid != cluster {
        return Err(refuse(
            ErrorCode::Forbidden,
            "a recovery reads only its grant's cluster",
        ));
    }
    Grants::load(&exporter.metadata)
        .unwrap_or_default()
        .recovery(token, cluster_uuid, grant_id, (exporter.now_ms)())
        .cloned()
        .map_err(|e| refuse(ErrorCode::Forbidden, e))
}

/// Every PIT of a namespace with its manifest, and the newest recovery
/// manifest.
pub fn recovery_index_of(
    journal: &Journal,
    metadata: &Metadata,
    grant: &Grant,
    cluster_name: &str,
) -> RecoveryIndex {
    RecoveryIndex {
        appliance_uuid: journal.appliance_uuid,
        cluster_uuid: grant.cluster_uuid.clone(),
        cluster_name: cluster_name.to_string(),
        grant_id: grant.grant_id.clone(),
        expires_at_ms: grant.expires_at_ms,
        manifest: crate::recovery::newest(metadata, &grant.cluster_uuid)
            .ok()
            .flatten(),
        pits: journal
            .pits
            .iter()
            .filter(|p| p.cluster_uuid == grant.cluster_uuid)
            .filter_map(|p| peer_pit(p, metadata))
            .collect(),
    }
}

async fn recovery_index(
    State(exporter): State<Arc<Exporter>>,
    Extension(client): Extension<Client>,
    Json(request): Json<RecoveryRequest>,
) -> Response {
    let journal = match exporter.ready().await {
        Ok(j) => j,
        Err(e) => return api_error(e),
    };
    let cluster = match &client {
        Client::Recovery { cluster_uuid, .. } => cluster_uuid.clone(),
        _ => String::new(),
    };
    let grant = match recovery_grant(&exporter, &client, &cluster, &request.grant) {
        Ok(g) => g,
        Err(e) => return api_error(e),
    };
    let now = (exporter.now_ms)();
    if let Ok(_guard) = exporter.metadata.lock().await {
        let mut grants = Grants::load(&exporter.metadata).unwrap_or_default();
        grants.used(&grant.grant_id, now);
        let _ = grants.save(&exporter.metadata);
    }
    let _ = exporter.audit.record(
        now,
        "recovery_index",
        &client.label(),
        serde_json::json!({"cluster_uuid": grant.cluster_uuid, "grant": grant.grant_id}),
    );
    let cluster_name = state::load::<state::Enrollment>(&exporter.paths.enrollment(&cluster))
        .ok()
        .flatten()
        .map(|e| e.cluster_name)
        .unwrap_or_default();
    Json(recovery_index_of(
        &journal,
        &exporter.metadata,
        &grant,
        &cluster_name,
    ))
    .into_response()
}

/// Binds the namespace to the recovering cluster: its CA and hosts replace
/// the lost cluster's in the enrollment, and the check-ins follow.
async fn recovery_bind(
    State(exporter): State<Arc<Exporter>>,
    Extension(client): Extension<Client>,
    Json(request): Json<RecoveryBind>,
) -> Response {
    if let Err(e) = exporter.ready().await {
        return api_error(e);
    }
    let grant = match recovery_grant(&exporter, &client, &request.cluster_uuid, &request.grant) {
        Ok(g) => g,
        Err(e) => return api_error(e),
    };
    let Some(ca) = bmac_proto::appliance::normalized_fingerprint(&request.proxmox_ca_fingerprint)
    else {
        return api_error(refuse(
            ErrorCode::Invalid,
            "the CA fingerprint is not a SHA-256",
        ));
    };
    if request.hosts.is_empty() {
        return api_error(refuse(ErrorCode::Invalid, "the binding names no host"));
    }
    let now = (exporter.now_ms)();
    let path = exporter.paths.enrollment(&request.cluster_uuid);
    let Ok(Some(mut enrollment)) = state::load::<state::Enrollment>(&path) else {
        return api_error(refuse(
            ErrorCode::NotFound,
            "this appliance isn't enrolled with that cluster",
        ));
    };
    let before = enrollment.proxmox_ca_fingerprint.clone();
    enrollment.proxmox_ca_fingerprint = ca.clone();
    enrollment.hosts = request.hosts.clone();
    if !request.cluster_name.is_empty() {
        enrollment.cluster_name = request.cluster_name.clone();
    }
    if let Err(e) = state::save(&path, &enrollment) {
        return api_error(refuse(ErrorCode::Internal, e.to_string()));
    }
    let _ = exporter.audit.record(
        now,
        "recovery_bind",
        &client.label(),
        serde_json::json!({
            "cluster_uuid": request.cluster_uuid,
            "grant": grant.grant_id,
            "ca_before": before,
            "ca_after": ca,
            "hosts": request.hosts.iter().map(|h| h.host.to_string()).collect::<Vec<_>>(),
        }),
    );
    eprintln!(
        "bmac-backupd: cluster {} is bound to a recovered cluster (CA {ca}) under recovery grant {}",
        request.cluster_uuid, grant.grant_id
    );
    Json(serde_json::json!({})).into_response()
}

async fn peer_index(
    State(exporter): State<Arc<Exporter>>,
    Extension(client): Extension<Client>,
) -> Response {
    if !matches!(client, Client::Peer { .. }) {
        return api_error(refuse(
            ErrorCode::Forbidden,
            "the index is for trusted peers",
        ));
    }
    let journal = match exporter.ready().await {
        Ok(j) => j,
        Err(e) => return api_error(e),
    };
    exporter.served(&client).await;
    Json(index_of(&journal, &exporter.metadata)).into_response()
}

/// A response body fed by the task that reads `zfs send`.
struct ChannelBody(mpsc::Receiver<Result<Frame<Bytes>, io::Error>>);

impl http_body::Body for ChannelBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        self.0.poll_recv(cx)
    }
}

async fn export(
    State(exporter): State<Arc<Exporter>>,
    Extension(client): Extension<Client>,
    Json(request): Json<ExportRequest>,
) -> Response {
    let journal = match exporter.ready().await {
        Ok(j) => j,
        Err(e) => return api_error(e),
    };
    let now = (exporter.now_ms)();
    let grants = Grants::load(&exporter.metadata).unwrap_or_default();
    let plan = match plan_export(&journal, &grants, &client, &request, now) {
        Ok(plan) => plan,
        Err(e) => return api_error(e),
    };
    if let Err(e) = exporter.check_live(&plan).await {
        return api_error(e);
    }
    if let Some(id) = &plan.grant_id
        && let Ok(_guard) = exporter.metadata.lock().await
    {
        let mut grants = Grants::load(&exporter.metadata).unwrap_or_default();
        grants.used(id, now);
        let _ = grants.save(&exporter.metadata);
    }
    exporter.served(&client).await;
    if request.estimate {
        return match exporter.zfs_text(&send_args(&plan, true)).await {
            Ok(text) => {
                let bytes = text
                    .lines()
                    .find_map(|l| l.strip_prefix("size\t"))
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
                Json(ExportEstimate { bytes }).into_response()
            }
            Err(e) => api_error(refuse(ErrorCode::Internal, format!("zfs send -nP: {e}"))),
        };
    }
    let tag = format!("bmac-export-{}", Uuid::new_v4().simple());
    let mut held = Vec::new();
    for name in std::iter::once(&plan.snapshot).chain(plan.base.iter().map(|(b, _)| b)) {
        if let Err(e) = exporter.pool.hold(&tag, name).await {
            release(&*exporter.pool, &tag, &held).await;
            return api_error(refuse(ErrorCode::Conflict, e));
        }
        held.push(name.clone());
    }
    let child = tokio::process::Command::new(&exporter.zfs)
        .args(send_args(&plan, false))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let child = match child {
        Ok(child) => child,
        Err(e) => {
            release(&*exporter.pool, &tag, &held).await;
            return api_error(refuse(
                ErrorCode::Internal,
                format!("cannot run zfs send: {e}"),
            ));
        }
    };
    let (tx, rx) = mpsc::channel(4);
    tokio::spawn(stream(
        exporter, client, request, plan, child, tag, held, tx,
    ));
    Response::builder()
        .header(axum::http::header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::new(ChannelBody(rx)))
        .unwrap_or_else(|_| api_error(refuse(ErrorCode::Internal, "no response")))
}

async fn release(pool: &dyn Pool, tag: &str, held: &[String]) {
    for name in held {
        let _ = pool.release(tag, name).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream(
    exporter: Arc<Exporter>,
    client: Client,
    request: ExportRequest,
    plan: ExportPlan,
    mut child: tokio::process::Child,
    tag: String,
    held: Vec<String>,
    tx: mpsc::Sender<Result<Frame<Bytes>, io::Error>>,
) {
    let started = Instant::now();
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let errors = tokio::spawn(async move {
        let mut text = Vec::new();
        let _ = (&mut stderr)
            .take(MAX_STDERR as u64)
            .read_to_end(&mut text)
            .await;
        String::from_utf8_lossy(&text).trim().to_string()
    });
    let mut bytes = 0u64;
    let mut error = None;
    let mut buffer = vec![0u8; CHUNK];
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) => break,
            Ok(n) => {
                bytes += n as u64;
                if tx
                    .send(Ok(Frame::data(Bytes::copy_from_slice(&buffer[..n]))))
                    .await
                    .is_err()
                {
                    error = Some("the client closed the stream".to_string());
                    let _ = child.start_kill();
                    break;
                }
            }
            Err(e) => {
                error = Some(format!("reading zfs send: {e}"));
                let _ = child.start_kill();
                break;
            }
        }
    }
    let status = child.wait().await;
    let stderr = errors.await.unwrap_or_default();
    if error.is_none() {
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => error = Some(format!("zfs send exited with {s}: {stderr}")),
            Err(e) => error = Some(format!("zfs send: {e}")),
        }
        if let Some(e) = &error {
            let _ = tx.send(Err(io::Error::other(e.clone()))).await;
        }
    }
    drop(tx);
    release(&*exporter.pool, &tag, &held).await;
    let _ = exporter.audit.record(
        (exporter.now_ms)(),
        "export",
        &client.label(),
        serde_json::json!({
            "cluster_uuid": request.cluster_uuid,
            "pit_uuid": request.pit_uuid,
            "volume_uuid": request.volume_uuid,
            "incremental_from": plan.base.as_ref().map(|(_, g)| g),
            "resumed": plan.resume_token.is_some(),
            "grant": plan.grant_id,
            "bytes": bytes,
            "duration_ms": started.elapsed().as_millis() as u64,
            "error": error,
        }),
    );
}

#[cfg(test)]
#[path = "export_tests.rs"]
mod tests;
