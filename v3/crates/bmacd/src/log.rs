// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Lines on stderr with a syslog priority prefix, which journald reads as
//! the message's level, and, once [`attach`]ed, records in the host's record
//! log as well.

use std::sync::OnceLock;

use tokio::sync::mpsc;

use crate::store::{Entry, Store};

static RECORDS: OnceLock<mpsc::UnboundedSender<Entry>> = OnceLock::new();

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn line(priority: u8, level: &str, message: &str) {
    for part in message.lines() {
        eprintln!("<{priority}>{part}");
    }
    if let Some(records) = RECORDS.get() {
        let _ = records.send(Entry::log(now_ms(), level, message));
    }
}

/// Also stores every later line in `store`, from a writer task. Only the
/// first call in a process takes effect. A failure to store is reported on
/// stderr only, so it can't feed itself.
pub fn attach(store: Store) -> Option<tokio::task::JoinHandle<()>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Entry>();
    RECORDS.set(tx).ok()?;
    Some(tokio::spawn(async move {
        while let Some(first) = rx.recv().await {
            let mut batch = vec![first];
            while let Ok(more) = rx.try_recv() {
                batch.push(more);
            }
            if let Err(e) = store.append(batch).await {
                eprintln!("<3>cannot store log records: {e}");
            }
        }
    }))
}

pub fn error(message: &str) {
    line(3, "error", message);
}

pub fn warning(message: &str) {
    line(4, "warning", message);
}

pub fn info(message: &str) {
    line(6, "info", message);
}
