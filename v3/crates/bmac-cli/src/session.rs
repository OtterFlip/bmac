// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Logging in through the engine socket with the same checks as the web UI:
//! a Proxmox password, then the second factor. The token lives only in this
//! process's memory and is logged out when the command ends; nothing is
//! written to disk, so every command logs in again.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use bmac_proto::api::{
    ApiError, LoginRequest, LoginResponse, LogoutRequest, SecondFactor, Session,
};
use bmac_proto::engine::{EngineCall, EngineOutcome};
use bmacd::web::client::EngineClient;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::terminal::Prompter;

/// The address the engine records for CLI calls.
pub const CLIENT: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

#[derive(Debug, thiserror::Error)]
pub enum CliError {
    #[error("{}", describe(.0))]
    Api(ApiError),
    #[error("{0}")]
    Message(String),
}

fn describe(error: &ApiError) -> String {
    let mut text = error.message.clone();
    for (field, why) in &error.field_errors {
        text.push_str(&format!("\n  {field}: {why}"));
    }
    text
}

impl From<String> for CliError {
    fn from(message: String) -> CliError {
        CliError::Message(message)
    }
}

impl From<std::io::Error> for CliError {
    fn from(error: std::io::Error) -> CliError {
        CliError::Message(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CliError>;

/// One unary call, without a session.
pub async fn call_anonymous<T: DeserializeOwned>(
    client: &EngineClient,
    call: EngineCall,
) -> Result<T> {
    decode(client.call(CLIENT, None, call).await)
}

fn decode<T: DeserializeOwned>(
    outcome: std::result::Result<EngineOutcome, bmacd::web::client::ClientError>,
) -> Result<T> {
    match outcome.map_err(|e| CliError::Message(e.to_string()))? {
        EngineOutcome::Ok { body } => serde_json::from_value(body)
            .map_err(|e| CliError::Message(format!("unexpected reply from the engine: {e}"))),
        EngineOutcome::Err { error } => Err(CliError::Api(error)),
        EngineOutcome::Event { .. } => Err(CliError::Message(
            "the engine answered a call with a stream event".to_string(),
        )),
    }
}

fn required(answer: Option<String>, what: &str) -> Result<String> {
    answer
        .map(|a| a.trim().to_string())
        .filter(|a| !a.is_empty())
        .ok_or_else(|| CliError::Message(format!("no {what} given")))
}

pub struct Login {
    client: Arc<EngineClient>,
    session: Session,
}

impl Login {
    /// Asks for the user (unless given), password, and second factor.
    pub async fn new(
        client: Arc<EngineClient>,
        prompter: &mut dyn Prompter,
        user: Option<&str>,
    ) -> Result<Login> {
        let user = match user {
            Some(user) => user.to_string(),
            None => required(prompter.line("Proxmox user (such as alice@pve): ")?, "user")?,
        };
        let password = prompter
            .secret(&format!("Password for {user}: "))?
            .filter(|p| !p.is_empty())
            .ok_or_else(|| CliError::Message("no password given".to_string()))?;
        let request = LoginRequest::Password {
            user: user.clone(),
            password,
        };
        let response: LoginResponse =
            call_anonymous(&client, EngineCall::Login { request }).await?;
        let session = match response {
            LoginResponse::Session(session) => session,
            LoginResponse::SecondFactor { challenge, methods } => {
                let (method, response) = second_factor(prompter, &user, &methods)?;
                let request = LoginRequest::SecondFactor {
                    challenge,
                    method,
                    response,
                };
                match call_anonymous(&client, EngineCall::Login { request }).await? {
                    LoginResponse::Session(session) => session,
                    LoginResponse::SecondFactor { .. } => {
                        return Err(CliError::Message(
                            "the engine asked for a second factor twice".to_string(),
                        ));
                    }
                }
            }
        };
        Ok(Login { client, session })
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn client(&self) -> &Arc<EngineClient> {
        &self.client
    }

    pub async fn call<T: DeserializeOwned>(&self, call: EngineCall) -> Result<T> {
        decode(
            self.client
                .call(CLIENT, Some(self.session.token.clone()), call)
                .await,
        )
    }

    /// Ends the session; the token is forgotten either way.
    pub async fn logout(self) {
        let _: Result<Value> = self
            .call(EngineCall::Logout {
                request: LogoutRequest::default(),
            })
            .await;
    }

    /// A copy of what [`Login::logout`] needs, for a signal handler.
    pub fn logout_later(&self) -> impl Future<Output = ()> + Send + 'static {
        let client = self.client.clone();
        let token = self.session.token.clone();
        async move {
            let call = EngineCall::Logout {
                request: LogoutRequest::default(),
            };
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                client.call(CLIENT, Some(token), call),
            )
            .await;
        }
    }
}

fn second_factor(
    prompter: &mut dyn Prompter,
    user: &str,
    methods: &[SecondFactor],
) -> Result<(SecondFactor, String)> {
    let totp = methods.contains(&SecondFactor::Totp);
    let recovery = methods.contains(&SecondFactor::Recovery);
    let method = match (totp, recovery) {
        (false, false) => {
            return Err(CliError::Message(format!(
                "{user}'s only second factor is WebAuthn, which needs a browser; \
                 use the Web Dashboard"
            )));
        }
        (true, false) => SecondFactor::Totp,
        (false, true) => SecondFactor::Recovery,
        (true, true) => {
            let choice = prompter.line("Second factor: [t]otp code or [r]ecovery key (t): ")?;
            match choice.as_deref().map(str::trim) {
                Some("" | "t" | "totp") => SecondFactor::Totp,
                Some("r" | "recovery") => SecondFactor::Recovery,
                _ => return Err(CliError::Message("choose t or r".to_string())),
            }
        }
    };
    let response = match method {
        SecondFactor::Totp => required(prompter.line("Authenticator code: ")?, "code")?,
        _ => required(prompter.secret("Recovery key: ")?, "recovery key")?,
    };
    Ok((method, response))
}
