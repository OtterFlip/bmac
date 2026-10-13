// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Process execution through [`CommandRunner`], with a real and a fake
//! implementation (`WORKFLOW_MIGRATION.md`, "Native operations").
//!
//! Native code never builds a shell command line: a [`Command`] is a literal
//! program path and an argument vector. Every child runs in its own process
//! group with a clean environment, a timeout, and captured output limits.

mod apt;
mod command;
mod fake;
mod real;

use std::future::Future;
use std::pin::Pin;

pub use apt::{APT_LOCK_FILES, AptLockHolder, apt_lock_holder, wait_for_apt_locks};
pub use command::{Command, ExecError, Exit, Output, OutputLine, REDACTED, Stream};
pub use fake::{FakeRunner, RecordedCall, Reply};
pub use real::RealRunner;
pub use tokio_util::sync::CancellationToken;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runs commands. Adapters take `&dyn CommandRunner`, so tests swap in
/// [`FakeRunner`] and production uses [`RealRunner`].
pub trait CommandRunner: Send + Sync {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>>;
}
