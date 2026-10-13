// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Downloads pinned by SHA-256 into the Deployer's cache, as v2's
//! `ensure_source_iso` does: a cached copy is reused only when its hash
//! matches, a download goes to a partial file that is renamed into place
//! only after its hash matches, and one lock per file keeps two tasks from
//! downloading it at once. Every file fetched here is pinned, so the hash,
//! not the transport, is what makes it trustworthy; plain HTTP is accepted
//! for Proxmox's package server, which has no valid certificate.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bmac_exec::BoxFuture;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use sha2::{Digest, Sha256};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";
const MAX_REDIRECTS: usize = 5;
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const ATTEMPTS: u32 = 3;

/// A file known by its URL and SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
}

/// Bytes so far and the total when the server says.
pub type Progress<'a> = &'a (dyn Fn(u64, Option<u64>) + Send + Sync);

pub trait Fetch: Send + Sync {
    /// Writes the body of `url` to `dest`.
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: Progress<'a>,
    ) -> BoxFuture<'a, Result<(), String>>;
}

/// The cache's name for a URL: its last path segment, which must be a
/// plain file name.
pub fn cache_name(url: &str) -> Result<String, String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let name = path.rsplit('/').next().unwrap_or("");
    let stem = name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or("");
    let ok = !stem.is_empty()
        && !name.starts_with(['.', '-'])
        && name.len() <= 200
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._+-~".contains(c));
    if !ok {
        return Err(format!(
            "{url} doesn't end in a file name the cache can use"
        ));
    }
    Ok(name.to_string())
}

/// SHA-256 of a file, lowercase hex.
pub fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

async fn sha256_of(path: PathBuf) -> io::Result<String> {
    tokio::task::spawn_blocking(move || file_sha256(&path))
        .await
        .map_err(io::Error::other)?
}

/// Holds the file's lock until dropped.
struct Lock(fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

async fn lock(path: &Path) -> Result<Lock, String> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Lock(file)),
            Err(fs::TryLockError::WouldBlock) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(format!("locking {}: {e}", path.display()));
            }
        }
    }
}

/// The verified file in `dir`, downloading it when the cache has no good
/// copy. `note` reports reuse and redownloads.
pub async fn ensure(
    dir: &Path,
    pin: &Pinned,
    fetch: &dyn Fetch,
    progress: Progress<'_>,
    note: &(dyn Fn(String) + Send + Sync),
) -> Result<PathBuf, String> {
    let expected = pin.sha256.to_ascii_lowercase();
    if expected.len() != 64 || !expected.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{} has no valid SHA-256 pin", pin.url));
    }
    let name = cache_name(&pin.url)?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(&name);
    let _lock = lock(&dir.join(format!("{name}.lock"))).await?;
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if meta.is_file() {
            let actual = sha256_of(path.clone())
                .await
                .map_err(|e| format!("{}: {e}", path.display()))?;
            if actual == expected {
                note(format!("Reusing the verified {name} in the cache."));
                return Ok(path);
            }
            note(format!(
                "{} doesn't match its SHA-256; downloading it again.",
                path.display()
            ));
        }
        fs::remove_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let partial = dir.join(format!("{name}.partial.{}", std::process::id()));
    let result = async {
        fetch.fetch(&pin.url, &partial, progress).await?;
        let meta = fs::symlink_metadata(&partial)
            .map_err(|e| format!("the download of {name} is missing: {e}"))?;
        if !meta.is_file() || meta.len() == 0 {
            return Err(format!("the download of {name} is empty"));
        }
        let actual = sha256_of(partial.clone())
            .await
            .map_err(|e| format!("{}: {e}", partial.display()))?;
        if actual != expected {
            return Err(format!(
                "The downloaded {name} doesn't match its SHA-256: expected {expected}, got {actual}."
            ));
        }
        fs::set_permissions(&partial, fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", partial.display()))?;
        fs::rename(&partial, &path).map_err(|e| format!("{}: {e}", path.display()))
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result.map(|()| path)
}

/// Fetches over HTTPS with the system's CA bundle, or plain HTTP, following
/// redirects.
pub struct HttpFetch {
    tls: Arc<rustls::ClientConfig>,
}

impl HttpFetch {
    pub fn new() -> Result<HttpFetch, String> {
        let mut roots = rustls::RootCertStore::empty();
        let certificates =
            CertificateDer::pem_file_iter(CA_BUNDLE).map_err(|e| format!("{CA_BUNDLE}: {e}"))?;
        for certificate in certificates {
            let certificate = certificate.map_err(|e| format!("{CA_BUNDLE}: {e}"))?;
            let _ = roots.add(certificate);
        }
        if roots.is_empty() {
            return Err(format!("{CA_BUNDLE} holds no certificates"));
        }
        let tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
        Ok(HttpFetch { tls: Arc::new(tls) })
    }
}

/// The parts of an `http` or `https` URL a request needs.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Target {
    pub tls: bool,
    pub host: String,
    pub port: u16,
    pub path: String,
}

pub(crate) fn parse_url(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(rest) = url.strip_prefix("https://") {
        (true, rest)
    } else if let Some(rest) = url.strip_prefix("http://") {
        (false, rest)
    } else {
        return Err(format!("{url} is not an http or https URL"));
    };
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() || authority.contains('@') || authority.starts_with('[') {
        return Err(format!("{url} has no plain host"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse().map_err(|_| format!("{url} has a bad port"))?,
        ),
        None => (authority, if tls { 443 } else { 80 }),
    };
    let path = if path.starts_with('?') {
        format!("/{path}")
    } else {
        path.to_string()
    };
    if path.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err(format!("{url} has spaces or control characters"));
    }
    Ok(Target {
        tls,
        host: host.to_ascii_lowercase(),
        port,
        path,
    })
}

/// Resolves a `Location` header against the URL it came from.
pub(crate) fn redirect(from: &str, location: &str) -> Result<String, String> {
    if location.starts_with("https://") || location.starts_with("http://") {
        return Ok(location.to_string());
    }
    let target = parse_url(from)?;
    let scheme = if target.tls { "https" } else { "http" };
    if let Some(path) = location.strip_prefix('/') {
        return Ok(format!("{scheme}://{}:{}/{path}", target.host, target.port));
    }
    Err(format!("unsupported redirect to {location}"))
}

fn host_header(target: &Target) -> String {
    let default = if target.tls { 443 } else { 80 };
    if target.port == default {
        target.host.clone()
    } else {
        format!("{}:{}", target.host, target.port)
    }
}

enum Step {
    Done,
    Redirect(String),
}

impl HttpFetch {
    async fn once(&self, url: &str, dest: &Path, progress: Progress<'_>) -> Result<Step, String> {
        let target = parse_url(url)?;
        let tcp = tokio::time::timeout(
            IDLE_TIMEOUT,
            TcpStream::connect((target.host.as_str(), target.port)),
        )
        .await
        .map_err(|_| format!("connecting to {} timed out", target.host))?
        .map_err(|e| format!("cannot connect to {}: {e}", target.host))?;
        let request = hyper::Request::builder()
            .method("GET")
            .uri(&target.path)
            .header("host", host_header(&target))
            .header(
                "user-agent",
                concat!("bmac-deployer/", env!("CARGO_PKG_VERSION")),
            )
            .body(Empty::<Bytes>::new())
            .map_err(|e| e.to_string())?;
        let response = if target.tls {
            let name = ServerName::try_from(target.host.clone()).map_err(|e| e.to_string())?;
            let stream = TlsConnector::from(self.tls.clone())
                .connect(name, tcp)
                .await
                .map_err(|e| format!("TLS with {}: {e}", target.host))?;
            send(TokioIo::new(stream), request).await?
        } else {
            send(TokioIo::new(tcp), request).await?
        };
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| format!("{url} redirected without a location"))?;
            return Ok(Step::Redirect(redirect(url, location)?));
        }
        if status != 200 {
            return Err(format!("{url} answered HTTP {status}"));
        }
        let total = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(dest)
            .map_err(|e| format!("{}: {e}", dest.display()))?;
        let mut body = response.into_body();
        let mut done = 0u64;
        progress(0, total);
        loop {
            let frame = tokio::time::timeout(IDLE_TIMEOUT, body.frame())
                .await
                .map_err(|_| format!("{url} stopped sending data"))?;
            let Some(frame) = frame else { break };
            let frame = frame.map_err(|e| format!("reading {url}: {e}"))?;
            if let Ok(data) = frame.into_data() {
                file.write_all(&data)
                    .map_err(|e| format!("{}: {e}", dest.display()))?;
                done += data.len() as u64;
                progress(done, total);
            }
        }
        file.sync_all()
            .map_err(|e| format!("{}: {e}", dest.display()))?;
        if total.is_some_and(|t| t != done) {
            return Err(format!("{url} ended after {done} bytes"));
        }
        Ok(Step::Done)
    }
}

async fn send<S>(
    io: TokioIo<S>,
    request: hyper::Request<Empty<Bytes>>,
) -> Result<hyper::Response<hyper::body::Incoming>, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    tokio::time::timeout(IDLE_TIMEOUT, sender.send_request(request))
        .await
        .map_err(|_| "the server didn't answer".to_string())?
        .map_err(|e| e.to_string())
}

impl Fetch for HttpFetch {
    fn fetch<'a>(
        &'a self,
        url: &'a str,
        dest: &'a Path,
        progress: Progress<'a>,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut last = String::new();
            for attempt in 1..=ATTEMPTS {
                let mut current = url.to_string();
                let mut outcome = Err(format!("{url} redirected more than {MAX_REDIRECTS} times"));
                for _ in 0..=MAX_REDIRECTS {
                    match self.once(&current, dest, progress).await {
                        Ok(Step::Done) => {
                            outcome = Ok(());
                            break;
                        }
                        Ok(Step::Redirect(next)) => current = next,
                        Err(e) => {
                            outcome = Err(e);
                            break;
                        }
                    }
                }
                match outcome {
                    Ok(()) => return Ok(()),
                    Err(e) => last = e,
                }
                if attempt < ATTEMPTS {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
            Err(last)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Served {
        body: Mutex<Vec<u8>>,
        calls: AtomicUsize,
    }

    impl Fetch for Served {
        fn fetch<'a>(
            &'a self,
            _url: &'a str,
            dest: &'a Path,
            progress: Progress<'a>,
        ) -> BoxFuture<'a, Result<(), String>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let body = self.body.lock().unwrap().clone();
                progress(body.len() as u64, Some(body.len() as u64));
                fs::write(dest, body).map_err(|e| e.to_string())
            })
        }
    }

    fn digest(data: &[u8]) -> String {
        Sha256::digest(data)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn pin(body: &[u8]) -> Pinned {
        Pinned {
            url: "https://example.test/iso/proxmox-ve_9.2-1.iso".into(),
            sha256: digest(body),
        }
    }

    #[test]
    fn cache_names_come_from_the_url() {
        assert_eq!(
            cache_name("https://enterprise.proxmox.com/iso/proxmox-ve_9.2-1.iso").unwrap(),
            "proxmox-ve_9.2-1.iso"
        );
        assert_eq!(
            cache_name("https://example.test/dl/proxmox.iso?mirror=1").unwrap(),
            "proxmox.iso"
        );
        assert!(cache_name("https://example.test/dl/.iso").is_err());
        assert!(cache_name("https://example.test/dl/").is_err());
    }

    #[tokio::test]
    async fn downloads_once_then_reuses_and_replaces_a_corrupt_copy() {
        let dir = tempfile::tempdir().unwrap();
        let served = Served {
            body: Mutex::new(b"good".to_vec()),
            calls: AtomicUsize::new(0),
        };
        let notes = Mutex::new(Vec::new());
        let note = |n: String| notes.lock().unwrap().push(n);
        let path = ensure(dir.path(), &pin(b"good"), &served, &|_, _| {}, &note)
            .await
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"good");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(served.calls.load(Ordering::SeqCst), 1);

        ensure(dir.path(), &pin(b"good"), &served, &|_, _| {}, &note)
            .await
            .unwrap();
        assert_eq!(served.calls.load(Ordering::SeqCst), 1);

        fs::write(&path, b"corrupt").unwrap();
        ensure(dir.path(), &pin(b"good"), &served, &|_, _| {}, &note)
            .await
            .unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"good");
        assert_eq!(served.calls.load(Ordering::SeqCst), 2);
        assert!(
            notes
                .lock()
                .unwrap()
                .iter()
                .any(|n| n.contains("downloading it again"))
        );
    }

    #[tokio::test]
    async fn a_download_with_the_wrong_hash_is_not_kept() {
        let dir = tempfile::tempdir().unwrap();
        let served = Served {
            body: Mutex::new(b"tampered".to_vec()),
            calls: AtomicUsize::new(0),
        };
        let error = ensure(dir.path(), &pin(b"good"), &served, &|_, _| {}, &|_| {})
            .await
            .unwrap_err();
        assert!(error.contains("doesn't match its SHA-256"), "{error}");
        let left: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(".lock"))
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn urls_and_redirects_parse() {
        assert_eq!(
            parse_url("https://enterprise.proxmox.com/iso/x.iso").unwrap(),
            Target {
                tls: true,
                host: "enterprise.proxmox.com".into(),
                port: 443,
                path: "/iso/x.iso".into(),
            }
        );
        assert_eq!(parse_url("http://h:8080").unwrap().path, "/");
        assert!(parse_url("ftp://h/x").is_err());
        assert!(parse_url("https://user@h/x").is_err());
        assert_eq!(
            redirect("https://a.test/x", "/y/z.iso").unwrap(),
            "https://a.test:443/y/z.iso"
        );
        assert_eq!(
            redirect("https://a.test/x", "https://b.test/z").unwrap(),
            "https://b.test/z"
        );
        assert!(redirect("https://a.test/x", "relative").is_err());
    }
}
