// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Runs one Deployer task for the lab (`lab/scenarios/m6.py`), as the
//! window would: the real workflow, programs, and downloads, with the
//! workstation's files under `--work`. Every task event and summary is a
//! JSON line on stdout; each line on stdin is an answer,
//! `{"request_id": ..., "answer": ...}`. It exits once the task ends.
//!
//! lab_task --kind host --work DIR --cache DIR --packages DIR --inventory BIN

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_deployer::download::HttpFetch;
use bmac_deployer::flows::{self, Env, IsoBook, Tools};
use bmac_deployer::preflight::Workstation;
use bmac_deployer::tasks::{TaskSink, Tasks};
use bmac_exec::RealRunner;
use bmac_proto::deployer::{AnswerTask, BuiltIso, LinkWait, TaskEvent, TaskKind, TaskSummary};
use serde_json::json;

struct Lines(Mutex<std::io::Stdout>);

impl Lines {
    fn print(&self, value: serde_json::Value) {
        let mut out = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writeln!(out, "{value}");
        let _ = out.flush();
    }
}

impl TaskSink for Lines {
    fn event(&self, event: &TaskEvent) {
        self.print(json!({ "event": event }));
    }

    fn updated(&self, summary: &TaskSummary) {
        self.print(json!({ "summary": summary }));
    }
}

/// Built ISOs, reported on stdout so the scenario can shred what is left.
struct Book(Arc<Lines>);

impl IsoBook for Book {
    fn record(&self, iso: BuiltIso) {
        self.0.print(json!({ "iso": iso }));
    }
    fn delete(&self, path: &str) -> Result<(), String> {
        std::fs::remove_file(path).map_err(|e| e.to_string())
    }
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let mut raw = std::env::args().skip(1);
    let mut args: BTreeMap<String, String> = BTreeMap::new();
    while let Some(flag) = raw.next() {
        let name = flag
            .strip_prefix("--")
            .ok_or(format!("unexpected {flag}"))?;
        args.insert(
            name.into(),
            raw.next().ok_or(format!("{flag} needs a value"))?,
        );
    }
    let get = |name: &str| {
        args.get(name)
            .map(PathBuf::from)
            .ok_or(format!("--{name} is required"))
    };
    let kind: TaskKind =
        serde_json::from_value(json!(args.get("kind").ok_or("--kind is required")?))
            .map_err(|e| format!("--kind: {e}"))?;
    let work = get("work")?;
    let lines = Arc::new(Lines(Mutex::new(std::io::stdout())));
    let env = Env {
        runner: Arc::new(RealRunner),
        fetch: Arc::new(HttpFetch::new()?),
        tools: Tools::find(&Workstation::detect()),
        cache_dir: get("cache")?,
        state_dir: work.join("state"),
        iso_dir: work.join("isos"),
        ssh_dir: work.join("ssh"),
        inventory_binary: Some(get("inventory")?),
        packages_dir: Some(get("packages")?),
        link_wait: LinkWait {
            attempts: 10,
            interval_seconds: 3,
        },
        isos: Arc::new(Book(lines.clone())),
        clock: Arc::new(bmac_deployer::now_ms),
        poll: Duration::from_secs(5),
        discovery_sources: flows::discovery_sources(None),
        dashboard_port: bmac_deployer::hostiso::DASHBOARD_PORT,
    };
    let tasks = Arc::new(Tasks::new(lines.clone(), Arc::new(bmac_deployer::now_ms)));
    let id = flows::start(&tasks, kind, Arc::new(env)).id;

    let answers = tasks.clone();
    let task = id.clone();
    let reply = lines.clone();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let parsed: Result<serde_json::Value, _> = serde_json::from_str(&line);
            let answer = parsed.map_err(|e| e.to_string()).and_then(|mut v| {
                v["task"] = json!(task);
                serde_json::from_value::<AnswerTask>(v).map_err(|e| e.to_string())
            });
            let outcome = answer.and_then(|a| answers.answer(a).map_err(|e| e.to_string()));
            if let Err(e) = outcome {
                reply.print(json!({ "answer_error": e }));
            }
        }
    });

    loop {
        let view = tasks.view(&id).map_err(|e| e.to_string())?;
        if view.task.state.is_terminal() {
            lines.print(json!({ "ended": view.task }));
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
