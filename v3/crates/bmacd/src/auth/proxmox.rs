// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Checking a password and second factor with Proxmox's own login,
//! `POST /api2/json/access/ticket` on this host. The connection trusts only
//! the node's own certificate. BMAC keeps nothing Proxmox returns except the
//! user name: the ticket itself is discarded.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Request, StatusCode, header};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::pinned::Pinned;

pub const TICKET_ADDRESS: &str = "127.0.0.1:8006";
pub const TICKET_PATH: &str = "/api2/json/access/ticket";
/// `pveproxy` serves the custom certificate when there is one.
pub const NODE_CERTIFICATES: [&str; 2] = [
    "/etc/pve/local/pveproxy-ssl.pem",
    "/etc/pve/local/pve-ssl.pem",
];
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RESPONSE: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketForm {
    Password {
        user: String,
        password: String,
    },
    /// `response` is `totp:<code>` or `recovery:<key>`.
    SecondFactor {
        user: String,
        partial_ticket: String,
        response: String,
    },
}

/// The second factors a user has, from Proxmox's challenge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Challenge {
    pub totp: bool,
    pub recovery: bool,
    pub webauthn: bool,
    /// Yubico OTP or U2F.
    pub other: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketOutcome {
    Granted {
        user: String,
    },
    SecondFactor {
        user: String,
        partial_ticket: String,
        challenge: Challenge,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProxmoxError {
    /// Wrong password or code, or a disabled or unknown account: Proxmox
    /// doesn't say which.
    #[error("Proxmox refused the login")]
    Refused,
    #[error("cannot reach Proxmox's login service: {0}")]
    Unreachable(String),
    #[error("unexpected answer from Proxmox's login service: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone)]
pub struct ProxmoxTickets {
    pub address: SocketAddr,
    /// PEM files; the server must present the first certificate of one.
    pub certificates: Vec<PathBuf>,
}

impl ProxmoxTickets {
    pub fn system() -> ProxmoxTickets {
        ProxmoxTickets {
            address: TICKET_ADDRESS.parse().expect("valid address"),
            certificates: NODE_CERTIFICATES.iter().map(PathBuf::from).collect(),
        }
    }

    pub async fn ticket(&self, form: &TicketForm) -> Result<TicketOutcome, ProxmoxError> {
        match tokio::time::timeout(TIMEOUT, self.exchange(form)).await {
            Ok(result) => result,
            Err(_) => Err(ProxmoxError::Unreachable(format!(
                "no answer within {} seconds",
                TIMEOUT.as_secs()
            ))),
        }
    }

    async fn exchange(&self, form: &TicketForm) -> Result<TicketOutcome, ProxmoxError> {
        let unreachable = |e: &dyn std::fmt::Display| ProxmoxError::Unreachable(e.to_string());
        let pinned = Pinned::from_pem_files(&self.certificates);
        if pinned.is_empty() {
            return Err(ProxmoxError::Unreachable(
                "this node's Proxmox certificate is missing".into(),
            ));
        }
        let mut config = pinned
            .client_config(&[&rustls::version::TLS13, &rustls::version::TLS12])
            .map_err(|e| unreachable(&e))?;
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = TcpStream::connect(self.address)
            .await
            .map_err(|e| unreachable(&e))?;
        let name = ServerName::try_from("localhost").expect("valid name");
        let tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|e| unreachable(&e))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|e| unreachable(&e))?;
        let driver = tokio::spawn(connection);
        let request = Request::post(TICKET_PATH)
            .header(header::HOST, "localhost")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(header::ACCEPT, "application/json")
            .body(Full::new(Bytes::from(encode_form(form))))
            .map_err(|e| ProxmoxError::Protocol(e.to_string()))?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| unreachable(&e));
        let result = match response {
            Ok(response) => {
                let status = response.status();
                match Limited::new(response.into_body(), MAX_RESPONSE)
                    .collect()
                    .await
                {
                    Ok(body) => interpret(status, &body.to_bytes()),
                    Err(e) => Err(ProxmoxError::Protocol(format!(
                        "cannot read the answer: {e}"
                    ))),
                }
            }
            Err(e) => Err(e),
        };
        driver.abort();
        result
    }
}

fn encode_form(form: &TicketForm) -> String {
    let fields: Vec<(&str, &str)> = match form {
        TicketForm::Password { user, password } => {
            vec![
                ("username", user),
                ("password", password),
                ("new-format", "1"),
            ]
        }
        TicketForm::SecondFactor {
            user,
            partial_ticket,
            response,
        } => vec![
            ("username", user),
            ("password", response),
            ("tfa-challenge", partial_ticket),
            ("new-format", "1"),
        ],
    };
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", form_escape(k), form_escape(v)))
        .collect::<Vec<_>>()
        .join("&")
}

fn form_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'*' => {
                out.push(byte as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = std::str::from_utf8(bytes.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

/// Proxmox puts the challenge in the partial ticket:
/// `PVE:!tfa!<percent-encoded JSON>:<time>::<signature>`.
pub fn parse_challenge(partial_ticket: &str) -> Option<Challenge> {
    let encoded = partial_ticket
        .split(':')
        .find_map(|part| part.strip_prefix("!tfa!"))?;
    let value: Value = serde_json::from_str(&percent_decode(encoded)?).ok()?;
    let object = value.as_object()?;
    let present = |key: &str| match object.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(items)) => !items.is_empty(),
        Some(_) => true,
    };
    Some(Challenge {
        totp: present("totp"),
        recovery: present("recovery"),
        webauthn: present("webauthn"),
        other: present("yubico") || present("u2f"),
    })
}

fn interpret(status: StatusCode, body: &[u8]) -> Result<TicketOutcome, ProxmoxError> {
    if status == StatusCode::UNAUTHORIZED {
        return Err(ProxmoxError::Refused);
    }
    if status != StatusCode::OK {
        return Err(ProxmoxError::Protocol(format!("HTTP {status}")));
    }
    let value: Value =
        serde_json::from_slice(body).map_err(|e| ProxmoxError::Protocol(e.to_string()))?;
    let data = match value.get("data") {
        None | Some(Value::Null) => return Err(ProxmoxError::Refused),
        Some(data) => data,
    };
    let field = |name: &str| {
        data.get(name)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let user = field("username")
        .ok_or_else(|| ProxmoxError::Protocol("no user name in the answer".into()))?
        .to_string();
    let ticket =
        field("ticket").ok_or_else(|| ProxmoxError::Protocol("no ticket in the answer".into()))?;
    let need_tfa = match data.get("NeedTFA") {
        Some(Value::Number(n)) => n.as_i64() != Some(0),
        Some(Value::Bool(b)) => *b,
        _ => false,
    };
    if !need_tfa {
        return Ok(TicketOutcome::Granted { user });
    }
    let challenge = parse_challenge(ticket)
        .ok_or_else(|| ProxmoxError::Protocol("cannot read the second-factor challenge".into()))?;
    Ok(TicketOutcome::SecondFactor {
        user,
        partial_ticket: ticket.to_string(),
        challenge,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forms_are_url_encoded() {
        let form = TicketForm::SecondFactor {
            user: "alice@pve".into(),
            partial_ticket: "PVE:!tfa!%7B%7D:1::sig/+=".into(),
            response: "totp:123 456".into(),
        };
        assert_eq!(
            encode_form(&form),
            "username=alice%40pve&password=totp%3A123+456&tfa-challenge=PVE%3A%21tfa%21%257B%257D%3A1%3A%3Asig%2F%2B%3D&new-format=1"
        );
    }

    #[test]
    fn challenges_are_read_from_the_partial_ticket() {
        let ticket = "PVE:!tfa!%7B%22totp%22%3Atrue%2C%22recovery%22%3A%5B0%2C1%5D%2C%22webauthn%22%3A%7B%22challenge%22%3A%22x%22%7D%7D:6528ABCD::c2ln";
        assert_eq!(
            parse_challenge(ticket),
            Some(Challenge {
                totp: true,
                recovery: true,
                webauthn: true,
                other: false
            })
        );
        let used_up = "PVE:!tfa!%7B%22totp%22%3Atrue%2C%22recovery%22%3A%5B%5D%7D:6528ABCD::c2ln";
        assert_eq!(
            parse_challenge(used_up),
            Some(Challenge {
                totp: true,
                ..Challenge::default()
            })
        );
        assert_eq!(parse_challenge("PVE:alice@pve:6528ABCD::sig"), None);
    }

    #[test]
    fn answers_are_interpreted() {
        assert_eq!(
            interpret(StatusCode::UNAUTHORIZED, b""),
            Err(ProxmoxError::Refused)
        );
        assert_eq!(
            interpret(StatusCode::OK, br#"{"data":null}"#),
            Err(ProxmoxError::Refused)
        );
        assert!(matches!(
            interpret(StatusCode::INTERNAL_SERVER_ERROR, b""),
            Err(ProxmoxError::Protocol(_))
        ));
        assert_eq!(
            interpret(StatusCode::OK, br#"{"data":{"username":"alice@pve","ticket":"PVE:alice@pve:1::s","CSRFPreventionToken":"x"}}"#),
            Ok(TicketOutcome::Granted { user: "alice@pve".into() })
        );
        let tfa = br#"{"data":{"username":"alice@pve","ticket":"PVE:!tfa!%7B%22totp%22%3Atrue%7D:1::s","NeedTFA":1}}"#;
        assert!(matches!(
            interpret(StatusCode::OK, tfa),
            Ok(TicketOutcome::SecondFactor {
                challenge: Challenge { totp: true, .. },
                ..
            })
        ));
    }
}
