// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Postmark's API with a server token (`MONITORING.md`, "Sending"): one
//! `POST /email/withTemplate` per recipient, judged by HTTP status,
//! `ErrorCode`, and `MessageID`, and the template check and create-missing
//! action behind Settings. TLS is verified against the system's CA bundle,
//! and each request times out after 15 seconds.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bmac_proto::alerts::{TemplatePresence, TemplateStatus, TemplatesView};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use crate::templates::{self, Template};

pub const API_HOST: &str = "api.postmarkapp.com";
pub const TIMEOUT: Duration = Duration::from_secs(15);
const CA_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

pub type Answer = Result<(u16, Value), String>;
pub type Pending<'a> = Pin<Box<dyn Future<Output = Answer> + Send + 'a>>;

/// One request to Postmark: method, path, and JSON body.
pub trait Transport: Send + Sync {
    fn call<'a>(&'a self, method: &'a str, path: &'a str, body: Option<Value>) -> Pending<'a>;
}

/// What one send came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Sent {
        message_id: String,
    },
    /// Timeout, connection error, 429, or 5xx: try again later.
    Retry(String),
    /// `ErrorCode` 10: stop sending until the token is fixed.
    BadToken,
    /// `ErrorCode` 1101: hold the message until the templates exist.
    TemplateMissing,
    /// `ErrorCode` 400 or 401: the sender is not a confirmed Sender
    /// Signature; stop sending until it is, or the sender changes.
    SenderRefused,
    /// `ErrorCode` 406: the recipient is inactive in Postmark; no retry.
    InactiveRecipient,
    /// Any other refusal; no retry.
    Failed(String),
}

/// The error-handling table of `MONITORING.md`.
pub fn classify(answer: Answer) -> Outcome {
    let (status, body) = match answer {
        Ok(answer) => answer,
        Err(error) => return Outcome::Retry(error),
    };
    let code = body["ErrorCode"].as_i64();
    let message = body["Message"].as_str().unwrap_or("").to_string();
    if (200..300).contains(&status)
        && code == Some(0)
        && let Some(id) = body["MessageID"].as_str().filter(|id| !id.is_empty())
    {
        return Outcome::Sent {
            message_id: id.to_string(),
        };
    }
    match code {
        Some(10) => return Outcome::BadToken,
        Some(1101) => return Outcome::TemplateMissing,
        Some(400 | 401) => return Outcome::SenderRefused,
        Some(406) => return Outcome::InactiveRecipient,
        _ => {}
    }
    if status == 429 || status >= 500 {
        return Outcome::Retry(format!("HTTP {status}"));
    }
    if status == 401 {
        return Outcome::BadToken;
    }
    Outcome::Failed(match code {
        Some(code) => format!("HTTP {status}, ErrorCode {code}: {message}"),
        None => format!("HTTP {status}: {message}"),
    })
}

/// One templated message to one recipient.
#[derive(Debug, Clone, PartialEq)]
pub struct Email {
    pub from: String,
    pub to: String,
    pub template: String,
    pub model: Value,
    /// The condition category, or `test` or `digest`.
    pub tag: String,
    pub alert_id: Option<String>,
}

pub async fn send(transport: &dyn Transport, email: &Email) -> Outcome {
    let mut metadata = serde_json::Map::new();
    if let Some(id) = &email.alert_id {
        metadata.insert("alert_id".into(), Value::from(id.clone()));
    }
    let body = json!({
        "From": email.from,
        "To": email.to,
        "TemplateAlias": email.template,
        "TemplateModel": email.model,
        "MessageStream": "outbound",
        "Tag": email.tag,
        "Metadata": metadata,
        "TrackOpens": false,
        "TrackLinks": "None",
    });
    classify(
        transport
            .call("POST", "/email/withTemplate", Some(body))
            .await,
    )
}

fn refused(what: &str, status: u16, body: &Value) -> String {
    match body["ErrorCode"].as_i64() {
        Some(10) => "Postmark refused the server token".to_string(),
        Some(code) => format!(
            "{what}: HTTP {status}, ErrorCode {code}: {}",
            body["Message"].as_str().unwrap_or("")
        ),
        None => format!("{what}: HTTP {status}"),
    }
}

/// The server's name and delivery type. The rest of `GET /server`'s
/// answer holds the server's API tokens and is dropped here.
pub async fn server(transport: &dyn Transport) -> Result<(String, String), String> {
    let (status, body) = transport.call("GET", "/server", None).await?;
    if status != 200 {
        return Err(refused("GET /server", status, &body));
    }
    match (body["Name"].as_str(), body["DeliveryType"].as_str()) {
        (Some(name), Some(delivery)) => Ok((name.to_string(), delivery.to_string())),
        _ => Err("GET /server answered without a Name and DeliveryType".into()),
    }
}

async fn presence(
    transport: &dyn Transport,
    template: &Template,
) -> Result<TemplatePresence, String> {
    let path = format!("/templates/{}", template.alias());
    let (status, body) = transport.call("GET", &path, None).await?;
    if status == 404 || body["ErrorCode"].as_i64() == Some(1101) {
        return Ok(TemplatePresence::Missing);
    }
    if status != 200 {
        return Err(refused(&format!("GET {path}"), status, &body));
    }
    Ok(if template.matches(&body) {
        TemplatePresence::Present
    } else {
        TemplatePresence::Differs
    })
}

/// Whether each template this version uses is on the server unchanged.
pub async fn check(transport: &dyn Transport) -> Result<TemplatesView, String> {
    let (server_name, delivery_type) = server(transport).await?;
    let mut statuses = Vec::new();
    for template in templates::all() {
        statuses.push(TemplateStatus {
            presence: presence(transport, &template).await?,
            alias: template.alias().to_string(),
        });
    }
    Ok(TemplatesView {
        server_name,
        delivery_type,
        templates: statuses,
    })
}

/// Creates the templates the server lacks, the layout first. A template
/// that is there with other content is left alone and reported.
pub async fn create_missing(transport: &dyn Transport) -> Result<TemplatesView, String> {
    let mut view = check(transport).await?;
    for (template, status) in templates::all().iter().zip(view.templates.iter_mut()) {
        if status.presence != TemplatePresence::Missing {
            continue;
        }
        let (code, body) = transport
            .call("POST", "/templates", Some(template.body()))
            .await?;
        if code != 200 || body["Alias"].as_str() != Some(template.alias()) {
            return Err(refused(
                &format!("creating {}", template.alias()),
                code,
                &body,
            ));
        }
        status.presence = TemplatePresence::Present;
    }
    Ok(view)
}

/// Postmark over HTTPS with a server token.
pub struct Https {
    token: String,
    host: String,
    tls: Arc<rustls::ClientConfig>,
}

impl std::fmt::Debug for Https {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Https")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// A TLS client that verifies servers against the system's CA bundle.
pub fn system_tls() -> Result<Arc<rustls::ClientConfig>, String> {
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
    Ok(Arc::new(tls))
}

impl Https {
    /// Postmark with the system's CA bundle.
    pub fn new(token: &str) -> Result<Https, String> {
        Ok(Https {
            token: token.trim().to_string(),
            host: API_HOST.to_string(),
            tls: system_tls()?,
        })
    }

    async fn request(&self, method: &str, path: &str, body: Option<Value>) -> Answer {
        let tcp = TcpStream::connect((self.host.as_str(), 443))
            .await
            .map_err(|e| format!("cannot connect to {}: {e}", self.host))?;
        let name = ServerName::try_from(self.host.clone()).map_err(|e| e.to_string())?;
        let tls = TlsConnector::from(self.tls.clone())
            .connect(name, tcp)
            .await
            .map_err(|e| format!("TLS with {}: {e}", self.host))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|e| e.to_string())?;
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let payload = body
            .map(|b| Bytes::from(serde_json::to_vec(&b).expect("JSON serializes")))
            .unwrap_or_default();
        let request = hyper::Request::builder()
            .method(method)
            .uri(path)
            .header("host", &self.host)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .header("x-postmark-server-token", &self.token)
            .body(Full::new(payload))
            .map_err(|e| e.to_string())?;
        let response = sender
            .send_request(request)
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let bytes = response
            .into_body()
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok((status, value))
    }
}

impl Transport for Https {
    fn call<'a>(&'a self, method: &'a str, path: &'a str, body: Option<Value>) -> Pending<'a> {
        Box::pin(async move {
            match tokio::time::timeout(TIMEOUT, self.request(method, path, body)).await {
                Ok(answer) => answer,
                Err(_) => Err(format!("{method} {path}: timed out after 15 seconds")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[test]
    fn the_error_table() {
        let ok = |status: u16, body: Value| classify(Ok((status, body)));
        assert_eq!(
            ok(
                200,
                json!({"ErrorCode": 0, "Message": "OK", "MessageID": "m1"})
            ),
            Outcome::Sent {
                message_id: "m1".into()
            }
        );
        assert!(
            matches!(
                ok(200, json!({"ErrorCode": 0, "Message": "OK"})),
                Outcome::Failed(_)
            ),
            "no MessageID"
        );
        assert!(matches!(
            classify(Err("timed out".into())),
            Outcome::Retry(_)
        ));
        assert!(matches!(ok(429, json!({})), Outcome::Retry(_)));
        assert!(matches!(ok(503, Value::Null), Outcome::Retry(_)));
        assert_eq!(
            ok(401, json!({"ErrorCode": 10, "Message": "bad"})),
            Outcome::BadToken
        );
        assert_eq!(
            ok(422, json!({"ErrorCode": 1101, "Message": "no template"})),
            Outcome::TemplateMissing
        );
        assert_eq!(
            ok(422, json!({"ErrorCode": 406, "Message": "inactive"})),
            Outcome::InactiveRecipient
        );
        assert_eq!(
            ok(
                422,
                json!({"ErrorCode": 300, "Message": "Invalid email request"})
            ),
            Outcome::Failed("HTTP 422, ErrorCode 300: Invalid email request".into())
        );
        assert_eq!(
            ok(
                422,
                json!({"ErrorCode": 400, "Message": "not a Sender Signature"})
            ),
            Outcome::SenderRefused
        );
        assert_eq!(
            ok(422, json!({"ErrorCode": 401, "Message": "not confirmed"})),
            Outcome::SenderRefused
        );
        assert_eq!(ok(400, Value::Null), Outcome::Failed("HTTP 400: ".into()));
    }

    /// Postmark in memory.
    #[derive(Default)]
    pub struct Fake {
        pub templates: Mutex<BTreeMap<String, Value>>,
        pub calls: Mutex<Vec<(String, String, Option<Value>)>>,
    }

    impl Transport for Fake {
        fn call<'a>(&'a self, method: &'a str, path: &'a str, body: Option<Value>) -> Pending<'a> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap()
                    .push((method.into(), path.into(), body.clone()));
                let mut templates = self.templates.lock().unwrap();
                Ok(match (method, path) {
                    ("GET", "/server") => (
                        200,
                        json!({"Name": "bmac_lab", "DeliveryType": "Sandbox", "ApiTokens": ["tok"]}),
                    ),
                    ("POST", "/templates") => {
                        let body = body.unwrap();
                        let alias = body["Alias"].as_str().unwrap().to_string();
                        if body["TemplateType"] == "Standard"
                            && !templates.contains_key(body["LayoutTemplate"].as_str().unwrap())
                        {
                            return Ok((422, json!({"ErrorCode": 1100, "Message": "no layout"})));
                        }
                        templates.insert(alias.clone(), body);
                        (200, json!({"Alias": alias, "TemplateId": 1}))
                    }
                    ("GET", path) => match templates.get(path.trim_start_matches("/templates/")) {
                        Some(found) => (200, found.clone()),
                        None => (422, json!({"ErrorCode": 1101, "Message": "not found"})),
                    },
                    other => panic!("unexpected {other:?}"),
                })
            })
        }
    }

    #[tokio::test]
    async fn missing_templates_are_created_layout_first_and_changed_ones_left_alone() {
        let fake = Fake::default();
        let mut edited = templates::all()[4].body();
        edited["HtmlBody"] = Value::from("someone edited this");
        let edited_alias = templates::all()[4].alias().to_string();

        let view = check(&fake).await.unwrap();
        assert_eq!(view.server_name, "bmac_lab");
        assert!(
            view.templates
                .iter()
                .all(|t| t.presence == TemplatePresence::Missing)
        );

        fake.templates
            .lock()
            .unwrap()
            .insert(edited_alias.clone(), edited.clone());
        let view = create_missing(&fake).await.unwrap();
        for status in &view.templates {
            let expected = if status.alias == edited_alias {
                TemplatePresence::Differs
            } else {
                TemplatePresence::Present
            };
            assert_eq!(status.presence, expected, "{}", status.alias);
        }
        assert_eq!(fake.templates.lock().unwrap()[&edited_alias], edited);
        let created: Vec<String> = fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(m, p, _)| m == "POST" && p == "/templates")
            .map(|(_, _, b)| b.as_ref().unwrap()["Alias"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(created[0], templates::LAYOUT);
        assert_eq!(created.len(), templates::all().len() - 1);

        let view = check(&fake).await.unwrap();
        assert_eq!(
            view.templates
                .iter()
                .filter(|t| t.presence == TemplatePresence::Present)
                .count(),
            templates::all().len() - 1
        );
    }

    #[tokio::test]
    async fn a_send_is_one_templated_message_without_tracking() {
        struct Capture(Mutex<Option<Value>>);
        impl Transport for Capture {
            fn call<'a>(&'a self, _: &'a str, path: &'a str, body: Option<Value>) -> Pending<'a> {
                assert_eq!(path, "/email/withTemplate");
                *self.0.lock().unwrap() = body;
                Box::pin(async { Ok((200, json!({"ErrorCode": 0, "MessageID": "m9"}))) })
            }
        }
        let capture = Capture(Mutex::new(None));
        let email = Email {
            from: "bmac@example.com".into(),
            to: "ops@example.com".into(),
            template: "bmac-test-v1".into(),
            model: json!({"requested_by": "admin"}),
            tag: "test".into(),
            alert_id: Some("a1".into()),
        };
        assert_eq!(
            send(&capture, &email).await,
            Outcome::Sent {
                message_id: "m9".into()
            }
        );
        let body = capture.0.lock().unwrap().clone().unwrap();
        assert_eq!(body["TemplateAlias"], "bmac-test-v1");
        assert_eq!(body["MessageStream"], "outbound");
        assert_eq!(body["TrackOpens"], false);
        assert_eq!(body["TrackLinks"], "None");
        assert_eq!(body["Metadata"]["alert_id"], "a1");
        assert!(body.get("HtmlBody").is_none(), "never an untemplated send");
    }

    #[tokio::test]
    async fn the_server_tokens_are_dropped() {
        let fake = Fake::default();
        let (name, delivery) = server(&fake).await.unwrap();
        assert_eq!((name.as_str(), delivery.as_str()), ("bmac_lab", "Sandbox"));
    }
}
