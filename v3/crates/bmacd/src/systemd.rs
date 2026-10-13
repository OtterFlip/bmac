// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The parts of systemd's service interface `bmacd` uses: readiness
//! notification and credentials passed with `LoadCredential`.

use std::io;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};

/// Tells systemd the service is ready (`Type=notify`). Does nothing when
/// not started by systemd.
pub fn notify_ready() -> io::Result<()> {
    match std::env::var_os("NOTIFY_SOCKET") {
        Some(path) => notify(Path::new(&path), "READY=1"),
        None => Ok(()),
    }
}

/// Sends `state` to a notification socket; a leading `@` names an abstract
/// socket.
pub fn notify(path: &Path, state: &str) -> io::Result<()> {
    let text = path.to_string_lossy();
    let address = match text.strip_prefix('@') {
        Some(name) => SocketAddr::from_abstract_name(name.as_bytes())?,
        None => SocketAddr::from_pathname(path)?,
    };
    UnixDatagram::unbound()?.send_to_addr(state.as_bytes(), &address)?;
    Ok(())
}

/// The path of credential `name` in `directory`, which systemd passes as
/// `$CREDENTIALS_DIRECTORY`.
pub fn credential_in(directory: Option<&Path>, name: &str) -> io::Result<PathBuf> {
    let directory = directory.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no credentials were passed (CREDENTIALS_DIRECTORY is unset)",
        )
    })?;
    let path = directory.join(name);
    if !path.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("credential {name} was not passed"),
        ));
    }
    Ok(path)
}

pub fn credential(name: &str) -> io::Result<PathBuf> {
    let directory = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
    credential_in(directory.as_deref(), name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notify_sends_the_state_to_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notify");
        let receiver = UnixDatagram::bind(&path).unwrap();
        notify(&path, "READY=1").unwrap();
        let mut buffer = [0u8; 32];
        let n = receiver.recv(&mut buffer).unwrap();
        assert_eq!(&buffer[..n], b"READY=1");
    }

    #[test]
    fn notify_reaches_abstract_sockets() {
        let name = format!(
            "bmacd-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let receiver =
            UnixDatagram::bind_addr(&SocketAddr::from_abstract_name(name.as_bytes()).unwrap())
                .unwrap();
        notify(Path::new(&format!("@{name}")), "READY=1").unwrap();
        let mut buffer = [0u8; 32];
        assert_eq!(receiver.recv(&mut buffer).unwrap(), 7);
    }

    #[test]
    fn credentials_must_be_passed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("https.key"), "key").unwrap();
        assert_eq!(
            credential_in(Some(dir.path()), "https.key").unwrap(),
            dir.path().join("https.key")
        );
        let missing = credential_in(Some(dir.path()), "https.pem").unwrap_err();
        assert_eq!(missing.to_string(), "credential https.pem was not passed");
        assert_eq!(
            credential_in(None, "https.key").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
