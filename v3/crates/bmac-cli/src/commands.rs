// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The commands that go through the engine socket (`jobs`, `cancel`, `run`)
//! and `status`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bmac_core::job::JobState;
use bmac_core::{HostSlot, JobId};
use bmac_proto::api::{CancelRequest, JobList, JobSummary, LaunchJob, Role};
use bmac_proto::engine::{EngineCall, Hello};
use bmac_proto::operations::{Operation, OperationKind, OperationList, ParamType};
use bmacd::cluster::Membership;
use bmacd::web::client::EngineClient;
use serde::Serialize;
use serde_json::Value;

use crate::args::RunArgs;
use crate::follow::{self, Followed};
use crate::session::{self, CliError, Login, Result};
use crate::terminal::{self, Prompter};

/// What `bmac run --terminal` needs to run a script itself.
pub struct TerminalEnv {
    /// The legacy runtime the scripts run from.
    pub root: PathBuf,
    pub bash: PathBuf,
    pub env: Vec<(String, String)>,
    /// Writes the cluster settings for the script, as `bmacd` does before
    /// every run.
    pub prepare: Box<dyn Fn() -> std::result::Result<(), String> + Send + Sync>,
    /// The operations as this build of `bmacd` knows them, with scripts and
    /// flags, which the engine never sends.
    pub registry: Arc<bmacd::operations::Registry>,
}

pub struct Context {
    pub client: Arc<EngineClient>,
    pub poll: Duration,
    /// Ctrl-C stops following, logs out, and leaves the job running.
    pub handle_interrupt: bool,
    pub terminal: TerminalEnv,
}

impl Context {
    async fn login(&self, prompter: &mut dyn Prompter, user: Option<&str>) -> Result<LoggedIn> {
        let login = Login::new(self.client.clone(), prompter, user).await?;
        let watcher = self.handle_interrupt.then(|| {
            let logout = login.logout_later();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    terminal::restore();
                    eprintln!(
                        "\nStopped. A job already started keeps running; follow it with bmac jobs."
                    );
                    logout.await;
                    std::process::exit(130);
                }
            })
        });
        Ok(LoggedIn { login, watcher })
    }
}

struct LoggedIn {
    login: Login,
    watcher: Option<tokio::task::JoinHandle<()>>,
}

impl LoggedIn {
    async fn finish<T>(self, result: Result<T>) -> Result<T> {
        if let Some(watcher) = self.watcher {
            watcher.abort();
        }
        self.login.logout().await;
        result
    }
}

fn exit_code(followed: &Followed) -> i32 {
    match followed.job.state {
        JobState::Completed => 0,
        _ => 1,
    }
}

fn ago(now_ms: i64, then_ms: i64) -> String {
    let seconds = (now_ms - then_ms).max(0) / 1000;
    match seconds {
        0..60 => format!("{seconds}s ago"),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

pub fn jobs_table(jobs: &[JobSummary], now_ms: i64) -> String {
    let mut text = format!(
        "{:<36}  {:<11}  {:<5}  {:<28}  {:<16}  UPDATED\n",
        "JOB", "STATE", "HOST", "OPERATION", "USER"
    );
    for job in jobs {
        let state = serde_json::to_value(job.state)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_default();
        let mut operation = job.operation.to_string();
        if job.dry_run {
            operation.push_str(" (dry run)");
        }
        if job.pending_request.is_some() {
            operation.push_str(" *waiting*");
        }
        text.push_str(&format!(
            "{:<36}  {:<11}  {:<5}  {:<28}  {:<16}  {}\n",
            job.id.to_string(),
            state,
            job.host.to_string(),
            operation,
            job.user,
            ago(now_ms, job.updated_at_ms)
        ));
    }
    text
}

pub async fn jobs(
    ctx: &Context,
    prompter: &mut dyn Prompter,
    json: bool,
    user: Option<&str>,
) -> Result<i32> {
    let session = ctx.login(prompter, user).await?;
    let result = session.login.call::<JobList>(EngineCall::Jobs {}).await;
    let list = session.finish(result).await?;
    if json {
        prompter.say(&serde_json::to_string_pretty(&list).unwrap_or_default());
    } else {
        prompter.say(jobs_table(&list.jobs, now_ms()).trim_end());
    }
    Ok(0)
}

pub async fn follow_job(
    ctx: &Context,
    prompter: &mut dyn Prompter,
    job: JobId,
    answer: bool,
    user: Option<&str>,
) -> Result<i32> {
    let session = ctx.login(prompter, user).await?;
    let result = follow::follow(&session.login, prompter, job, 1, answer, ctx.poll).await;
    let followed = session.finish(result).await?;
    Ok(exit_code(&followed))
}

pub async fn cancel(
    ctx: &Context,
    prompter: &mut dyn Prompter,
    job: JobId,
    force: bool,
    user: Option<&str>,
) -> Result<i32> {
    let session = ctx.login(prompter, user).await?;
    let call = EngineCall::Cancel {
        job,
        request: CancelRequest { force },
    };
    let result = session.login.call::<JobSummary>(call).await;
    let summary = session.finish(result).await?;
    prompter.say(&format!(
        "{} requested for job {job} ({}); it stops at the next point it allows",
        if force { "A forced cancel" } else { "A cancel" },
        summary.operation
    ));
    Ok(0)
}

fn flag_value(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" | "true" | "1" | "on" => Some(true),
        "n" | "no" | "false" | "0" | "off" => Some(false),
        _ => None,
    }
}

/// The launch parameters from `NAME=VALUE` words, the operation's defaults,
/// and prompts for required ones still missing.
pub fn launch_parameters(
    operation: &Operation,
    given: &[(String, Option<String>)],
    prompter: &mut dyn Prompter,
) -> Result<BTreeMap<String, Value>> {
    let names = || {
        operation
            .params
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut values = BTreeMap::new();
    for (name, value) in given {
        let Some(param) = operation.params.iter().find(|p| &p.id == name) else {
            let known = if operation.params.is_empty() {
                "it takes none".to_string()
            } else {
                format!("its parameters are {}", names())
            };
            return Err(format!("{} has no parameter {name}; {known}", operation.id).into());
        };
        let value = match (param.kind, value) {
            (ParamType::Flag, None) => Value::Bool(true),
            (ParamType::Flag, Some(text)) => Value::Bool(
                flag_value(text)
                    .ok_or_else(|| format!("{name} is on or off: {name}=yes or {name}=no"))?,
            ),
            (_, None) => return Err(format!("{name} needs a value: {name}=...").into()),
            (_, Some(text)) => Value::String(text.clone()),
        };
        values.insert(name.clone(), value);
    }
    for param in &operation.params {
        if !values.contains_key(&param.id)
            && let Some(default) = &param.default
        {
            values.insert(param.id.clone(), default.clone());
        }
    }
    for param in operation.params.iter().filter(|p| p.required) {
        if values.contains_key(&param.id) {
            continue;
        }
        if !prompter.interactive() {
            return Err(format!("{} needs {}={}", operation.id, param.id, param.label).into());
        }
        let mut intro = param.label.clone();
        if let Some(help) = &param.help {
            intro.push_str(&format!("\n  {help}"));
        }
        for choice in &param.choices {
            intro.push_str(&format!("\n  {}: {}", choice.value, choice.label));
        }
        prompter.say(&intro);
        let value = loop {
            let text = prompter
                .line(&format!("{} ({}): ", param.label, param.id))?
                .ok_or_else(|| CliError::Message("no value given".to_string()))?;
            let text = text.trim().to_string();
            if text.is_empty() {
                prompter.say("  A value is required.");
                continue;
            }
            match param.kind {
                ParamType::Flag => match flag_value(&text) {
                    Some(on) => break Value::Bool(on),
                    None => prompter.say("  Answer yes or no."),
                },
                _ => break Value::String(text),
            }
        };
        values.insert(param.id.clone(), value);
    }
    Ok(values)
}

pub async fn run(ctx: &Context, prompter: &mut dyn Prompter, args: RunArgs) -> Result<i32> {
    let session = ctx.login(prompter, args.user.as_deref()).await?;
    let result = run_logged_in(ctx, &session.login, prompter, &args).await;
    session.finish(result).await
}

async fn run_logged_in(
    ctx: &Context,
    login: &Login,
    prompter: &mut dyn Prompter,
    args: &RunArgs,
) -> Result<i32> {
    let list: OperationList = login.call(EngineCall::Operations {}).await?;
    let view = list
        .operations
        .iter()
        .find(|v| v.operation.id.as_str() == args.operation)
        .ok_or_else(|| format!("there is no operation {}", args.operation))?;
    if !view.allowed {
        let role = |r: Role| {
            serde_json::to_value(r)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
        };
        return Err(format!(
            "{} needs the {} role; {} is {}",
            view.operation.id,
            role(view.operation.min_role).unwrap_or_default(),
            login.session().user,
            role(login.session().role).unwrap_or_default()
        )
        .into());
    }
    let parameters = launch_parameters(&view.operation, &args.params, prompter)?;
    if args.terminal {
        return run_in_terminal(
            ctx,
            login,
            prompter,
            &view.operation,
            &parameters,
            args.dry_run,
        )
        .await;
    }
    let launch = LaunchJob {
        operation: view.operation.id.clone(),
        parameters,
        idempotency_key: uuid::Uuid::new_v4().to_string(),
        dry_run: args.dry_run,
    };
    let job: JobSummary = login
        .call(EngineCall::LaunchJob { request: launch })
        .await?;
    prompter.say(&format!(
        "Job {} admitted: {}{} on {}",
        job.id,
        job.title,
        if job.dry_run { " (dry run)" } else { "" },
        job.host
    ));
    if args.detach {
        prompter.say(&format!("Follow it with: bmac jobs {}", job.id));
        return Ok(0);
    }
    let followed = follow::follow(login, prompter, job.id, 1, true, ctx.poll).await?;
    Ok(exit_code(&followed))
}

/// `bmac run --terminal`: the script in this terminal, without `--json`, for
/// debugging. It runs outside the job system, so it takes no locks; it is
/// for administrators only, refuses while any job is unfinished, and needs
/// the operation's ID typed to go ahead.
async fn run_in_terminal(
    ctx: &Context,
    login: &Login,
    prompter: &mut dyn Prompter,
    operation: &Operation,
    parameters: &BTreeMap<String, Value>,
    dry_run: bool,
) -> Result<i32> {
    if login.session().role != Role::Admin {
        return Err("bmac run --terminal is for administrators"
            .to_string()
            .into());
    }
    if operation.kind != OperationKind::Script {
        return Err(format!("{} is not a script operation", operation.id).into());
    }
    let hello: Hello = session::call_anonymous(&ctx.client, EngineCall::Hello {}).await?;
    if hello.version != bmacd::VERSION {
        return Err(format!(
            "bmacd is {} and this bmac is {}; update them together",
            hello.version,
            bmacd::VERSION
        )
        .into());
    }
    let local = ctx
        .terminal
        .registry
        .get(operation.id.as_str())
        .ok_or_else(|| format!("this bmac doesn't know {}", operation.id))?;
    let prepared = bmacd::operations::prepare(local, parameters, dry_run)
        .map_err(|invalid| CliError::Api(invalid.into()))?;
    let script = local
        .script
        .as_deref()
        .ok_or_else(|| format!("{} has no script", operation.id))?;
    let jobs: JobList = login.call(EngineCall::Jobs {}).await?;
    let unfinished: Vec<String> = jobs
        .jobs
        .iter()
        .filter(|j| !j.state.is_terminal())
        .map(|j| format!("{} ({})", j.id, j.operation))
        .collect();
    if !unfinished.is_empty() {
        return Err(format!(
            "bmac run --terminal runs the script outside the job system, so it refuses while \
             jobs are unfinished: {}",
            unfinished.join(", ")
        )
        .into());
    }
    prompter.say(&format!(
        "This runs {script} {} in this terminal, outside the job system: no locks are taken and \
         the Dashboard won't show it.",
        prepared.args.join(" ")
    ));
    let typed = prompter
        .line(&format!("Type {} to run it: ", operation.id))?
        .unwrap_or_default();
    if typed.trim() != operation.id.as_str() {
        return Err("not confirmed; nothing was run".to_string().into());
    }
    (ctx.terminal.prepare)()?;
    let status = tokio::process::Command::new(&ctx.terminal.bash)
        .arg(ctx.terminal.root.join(script))
        .args(&prepared.args)
        .current_dir(&ctx.terminal.root)
        .env_clear()
        .envs(
            ctx.terminal
                .env
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str())),
        )
        .status()
        .await
        .map_err(|e| format!("cannot start {}: {e}", ctx.terminal.bash.display()))?;
    Ok(status.code().unwrap_or(1))
}

#[derive(Debug, Clone, Serialize)]
pub struct MemberStatus {
    pub host: HostSlot,
    pub online: bool,
    pub duty: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub host: String,
    /// The engine's version, when it answers.
    pub engine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_error: Option<String>,
    pub quorate: Option<bool>,
    pub members: Vec<MemberStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cluster_error: Option<String>,
}

pub fn status_from(
    host: String,
    engine: std::result::Result<String, String>,
    membership: std::result::Result<Membership, String>,
) -> Status {
    let (engine, engine_error) = match engine {
        Ok(version) => (Some(version), None),
        Err(e) => (None, Some(e)),
    };
    let (quorate, members, cluster_error) = match membership {
        Ok(m) => {
            let duty = m.duty();
            let members = m
                .members
                .keys()
                .map(|h| MemberStatus {
                    host: *h,
                    online: m.online.contains(h),
                    duty: duty == Some(*h),
                })
                .collect();
            (Some(m.quorate), members, None)
        }
        Err(e) => (None, Vec::new(), Some(e)),
    };
    Status {
        host,
        engine,
        engine_error,
        quorate,
        members,
        cluster_error,
    }
}

pub fn status_text(status: &Status) -> String {
    let mut text = format!("Host: {}\n", status.host);
    match (&status.engine, &status.engine_error) {
        (Some(version), _) => text.push_str(&format!("bmacd: running, version {version}\n")),
        (None, Some(e)) => text.push_str(&format!("bmacd: not reachable ({e})\n")),
        (None, None) => text.push_str("bmacd: unknown\n"),
    }
    match (status.quorate, &status.cluster_error) {
        (Some(quorate), _) => {
            text.push_str(&format!(
                "Cluster: {}\n",
                if quorate {
                    "quorate"
                } else {
                    "NOT quorate; nothing can change"
                }
            ));
            for member in &status.members {
                text.push_str(&format!(
                    "  {:<6} {}{}\n",
                    member.host.to_string(),
                    if member.online { "online" } else { "offline" },
                    if member.duty { "  (duty host)" } else { "" }
                ));
            }
        }
        (None, Some(e)) => text.push_str(&format!("Cluster: unknown ({e})\n")),
        (None, None) => text.push_str("Cluster: unknown\n"),
    }
    text
}

/// The engine's version, without logging in.
pub async fn engine_version(client: &EngineClient) -> std::result::Result<String, String> {
    match tokio::time::timeout(
        Duration::from_secs(3),
        session::call_anonymous::<Hello>(client, EngineCall::Hello {}),
    )
    .await
    {
        Ok(Ok(hello)) => Ok(hello.version),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err("no answer within 3 s".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use serde_json::json;

    use super::*;
    use crate::terminal::Scripted;

    fn host(n: u8) -> HostSlot {
        HostSlot::new(n).unwrap()
    }

    fn operation() -> Operation {
        serde_json::from_value(json!({
            "id": "prod.add", "title": "Add", "summary": "s", "description": "d",
            "category": "production", "kind": "script", "script": "scripts/add.sh",
            "min_role": "operator", "mutating": true, "destructive": false,
            "params": [
                {"id": "resource", "label": "VM", "type": "production", "flag": "--resource", "required": true},
                {"id": "cores", "label": "Cores", "type": "integer", "flag": "--cores", "default": "2"},
                {"id": "force", "label": "Force", "type": "flag", "flag": "--force"}
            ],
            "target": {"rule": "any"}, "cancel": {"rule": "script"}, "resume": "never"
        }))
        .unwrap()
    }

    #[test]
    fn parameters_come_from_words_defaults_and_prompts() {
        let op = operation();
        let mut prompter = Scripted::new(["", "prod2"]);
        let values = launch_parameters(&op, &[("force".into(), None)], &mut prompter).unwrap();
        assert_eq!(
            values,
            BTreeMap::from([
                ("cores".to_string(), json!("2")),
                ("force".to_string(), json!(true)),
                ("resource".to_string(), json!("prod2")),
            ])
        );
        assert!(prompter.text().contains("A value is required."));

        let mut prompter = Scripted::new(Vec::<String>::new());
        for (given, why) in [
            (
                vec![("nope".to_string(), Some("1".to_string()))],
                "has no parameter nope; its parameters are resource, cores, force",
            ),
            (
                vec![("force".to_string(), Some("maybe".to_string()))],
                "on or off",
            ),
            (vec![("cores".to_string(), None)], "cores needs a value"),
        ] {
            let error = launch_parameters(&op, &given, &mut prompter)
                .unwrap_err()
                .to_string();
            assert!(error.contains(why), "{error}");
        }
    }

    #[test]
    fn status_names_the_duty_host() {
        let membership = Membership {
            quorate: true,
            online: BTreeSet::from([host(2), host(3)]),
            members: BTreeMap::from([(host(1), None), (host(2), None), (host(3), None)]),
        };
        let status = status_from("mox2".into(), Ok("3.0.0".into()), Ok(membership));
        let text = status_text(&status);
        assert!(text.contains("bmacd: running, version 3.0.0"), "{text}");
        assert!(text.contains("mox1   offline\n"), "{text}");
        assert!(text.contains("mox2   online  (duty host)"), "{text}");
        let down = status_from(
            "mox2".into(),
            Err("connection refused".into()),
            Err("no /etc/pve".into()),
        );
        let text = status_text(&down);
        assert!(
            text.contains("not reachable (connection refused)")
                && text.contains("Cluster: unknown (no /etc/pve)")
        );
        let json = serde_json::to_value(&down).unwrap();
        assert_eq!(json["engine"], Value::Null);
        assert_eq!(json["engine_error"], "connection refused");
    }

    #[test]
    fn ages_are_short() {
        assert_eq!(ago(100_000, 95_000), "5s ago");
        assert_eq!(ago(10_000_000, 0), "2h ago");
        assert_eq!(ago(0, 5_000), "0s ago");
    }
}
