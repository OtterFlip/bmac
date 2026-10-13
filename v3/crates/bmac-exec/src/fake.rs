// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The fake runner for tests: it answers expected calls with recorded
//! results, refuses anything else, and keeps the sequence of calls so a
//! native operation can be compared with the script it replaces.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use crate::command::{Captured, Collector, Ending, finish};
use crate::{BoxFuture, Command, CommandRunner, ExecError, Exit, Output, Stream};

/// What an expected call returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    exit: Exit,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

impl Reply {
    /// Exit 0 with this stdout.
    pub fn ok(stdout: impl Into<Vec<u8>>) -> Reply {
        Reply {
            exit: Exit::Code(0),
            stdout: stdout.into(),
            stderr: Vec::new(),
            timed_out: false,
        }
    }

    /// Exit `code` with this stderr.
    pub fn fail(code: i32, stderr: impl Into<Vec<u8>>) -> Reply {
        Reply {
            exit: Exit::Code(code),
            stdout: Vec::new(),
            stderr: stderr.into(),
            timed_out: false,
        }
    }

    /// The command runs past its timeout.
    pub fn timeout() -> Reply {
        Reply {
            exit: Exit::Signal(15),
            stdout: Vec::new(),
            stderr: Vec::new(),
            timed_out: true,
        }
    }

    pub fn code(mut self, code: i32) -> Reply {
        self.exit = Exit::Code(code);
        self
    }

    pub fn stdout(mut self, stdout: impl Into<Vec<u8>>) -> Reply {
        self.stdout = stdout.into();
        self
    }

    pub fn stderr(mut self, stderr: impl Into<Vec<u8>>) -> Reply {
        self.stderr = stderr.into();
        self
    }
}

/// A call the fake received, as the command described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Only the variables the command set, not the runner's defaults.
    pub env: BTreeMap<String, String>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Duration,
}

impl RecordedCall {
    /// `program arg arg ...`, for comparing call sequences in tests.
    pub fn line(&self) -> String {
        std::iter::once(self.program.display().to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

struct Expectation {
    program: PathBuf,
    args: Vec<String>,
    reply: Reply,
    used: bool,
}

#[derive(Default)]
struct State {
    expectations: Vec<Expectation>,
    calls: Vec<RecordedCall>,
    unexpected: Vec<String>,
}

#[derive(Default)]
pub struct FakeRunner {
    state: Mutex<State>,
}

impl FakeRunner {
    pub fn new() -> FakeRunner {
        FakeRunner::default()
    }

    /// Expects one call of `program` with exactly `args`. Each expectation
    /// answers one call; add it twice for two calls. Calls may arrive in any
    /// order; [`FakeRunner::calls`] keeps the order they arrived in.
    pub fn expect<I, S>(&self, program: impl Into<PathBuf>, args: I, reply: Reply) -> &FakeRunner
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let expectation = Expectation {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            reply,
            used: false,
        };
        self.state.lock().unwrap().expectations.push(expectation);
        self
    }

    pub fn calls(&self) -> Vec<RecordedCall> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn call_lines(&self) -> Vec<String> {
        self.calls().iter().map(RecordedCall::line).collect()
    }

    /// Fails with every unexpected call and every unused expectation.
    pub fn verify(&self) -> Result<(), String> {
        let state = self.state.lock().unwrap();
        let mut problems: Vec<String> = state
            .unexpected
            .iter()
            .map(|c| format!("unexpected call: {c}"))
            .collect();
        for expectation in state.expectations.iter().filter(|e| !e.used) {
            let line = std::iter::once(expectation.program.display().to_string())
                .chain(expectation.args.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ");
            problems.push(format!("expected call never made: {line}"));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems.join("\n"))
        }
    }

    #[track_caller]
    pub fn assert_done(&self) {
        if let Err(problems) = self.verify() {
            panic!("{problems}");
        }
    }

    fn answer(&self, command: &Command) -> Result<Reply, ExecError> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(RecordedCall {
            program: command.program.clone(),
            args: command.args.clone(),
            env: command.env.clone(),
            stdin: command.stdin.clone(),
            timeout: command.timeout,
        });
        let found = state
            .expectations
            .iter_mut()
            .find(|e| !e.used && e.program == command.program && e.args == command.args);
        match found {
            Some(expectation) => {
                expectation.used = true;
                Ok(expectation.reply.clone())
            }
            None => {
                let line = command.to_string();
                state.unexpected.push(line.clone());
                Err(ExecError::Unexpected { command: line })
            }
        }
    }
}

fn capture(command: &Command, stream: Stream, data: &[u8]) -> Captured {
    let mut collector = Collector::new(command, stream);
    collector.push(data);
    collector.finish()
}

impl CommandRunner for FakeRunner {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
        Box::pin(async move {
            if !command.program.is_absolute() {
                return Err(ExecError::RelativeProgram {
                    program: command.name(),
                });
            }
            let reply = self.answer(&command)?;
            if command.cancel.as_ref().is_some_and(|t| t.is_cancelled()) {
                let empty = || Captured {
                    data: Vec::new(),
                    truncated: false,
                };
                return finish(
                    &command,
                    Ending::Cancelled(Exit::Signal(15)),
                    empty(),
                    empty(),
                    Duration::ZERO,
                );
            }
            let stdout = capture(&command, Stream::Stdout, &reply.stdout);
            let stderr = capture(&command, Stream::Stderr, &reply.stderr);
            let ending = if reply.timed_out {
                Ending::TimedOut(reply.exit)
            } else {
                Ending::Exited(reply.exit)
            };
            finish(&command, ending, stdout, stderr, Duration::ZERO)
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::CancellationToken;

    const ZPOOL: &str = "/usr/sbin/zpool";

    #[tokio::test]
    async fn answers_expected_calls_and_records_them() {
        let fake = FakeRunner::new();
        fake.expect(ZPOOL, ["list", "-H"], Reply::ok("rpool\n"))
            .expect(ZPOOL, ["status", "-P", "-p"], Reply::ok("pool: rpool\n"));
        let status = fake
            .run(Command::new(ZPOOL).args(["status", "-P", "-p"]))
            .await
            .unwrap();
        assert_eq!(status.stdout, b"pool: rpool\n");
        let list = fake
            .run(
                Command::new(ZPOOL)
                    .args(["list", "-H"])
                    .env("ZFS_COLOR", "0"),
            )
            .await
            .unwrap();
        assert_eq!(list.stdout, b"rpool\n");
        assert_eq!(
            fake.call_lines(),
            ["/usr/sbin/zpool status -P -p", "/usr/sbin/zpool list -H"]
        );
        assert_eq!(fake.calls()[1].env["ZFS_COLOR"], "0");
        fake.assert_done();
    }

    #[tokio::test]
    async fn rejects_unexpected_calls_even_if_the_caller_ignores_the_error() {
        let fake = FakeRunner::new();
        let error = fake
            .run(Command::new(ZPOOL).args(["destroy", "rpool"]))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecError::Unexpected { .. }));
        let problems = fake.verify().unwrap_err();
        assert!(
            problems.contains("unexpected call: /usr/sbin/zpool destroy rpool"),
            "{problems}"
        );
    }

    #[tokio::test]
    async fn reports_unused_expectations() {
        let fake = FakeRunner::new();
        fake.expect(ZPOOL, ["list"], Reply::ok(""))
            .expect(ZPOOL, ["list"], Reply::ok(""));
        fake.run(Command::new(ZPOOL).arg("list")).await.unwrap();
        assert_eq!(
            fake.verify().unwrap_err(),
            "expected call never made: /usr/sbin/zpool list"
        );
    }

    #[tokio::test]
    async fn applies_the_same_rules_as_the_real_runner() {
        let fake = FakeRunner::new();
        fake.expect(
            ZPOOL,
            ["a"],
            Reply::fail(1, "cannot open 'tank': no such pool\n"),
        )
        .expect(ZPOOL, ["b"], Reply::fail(1, ""))
        .expect(ZPOOL, ["c"], Reply::timeout())
        .expect(ZPOOL, ["d"], Reply::ok("x".repeat(100)))
        .expect(ZPOOL, ["e"], Reply::ok("token=abc123\nok\n"));
        let error = fake.run(Command::new(ZPOOL).arg("a")).await.unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("exit code 1: cannot open 'tank': no such pool"),
            "{error}"
        );
        assert_eq!(
            fake.run(Command::new(ZPOOL).arg("b").expect_codes(&[0, 1]))
                .await
                .unwrap()
                .code(),
            Some(1)
        );
        assert!(matches!(
            fake.run(Command::new(ZPOOL).arg("c")).await,
            Err(ExecError::TimedOut { .. })
        ));
        let limited = fake
            .run(Command::new(ZPOOL).arg("d").stdout_limit(10))
            .await
            .unwrap();
        assert!(limited.stdout_truncated && limited.stdout.len() == 10);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let redacted = fake
            .run(
                Command::new(ZPOOL)
                    .arg("e")
                    .secret("abc123")
                    .stream_lines(tx),
            )
            .await
            .unwrap();
        assert_eq!(redacted.stdout_text(), "token=[redacted]\nok\n");
        assert_eq!(rx.recv().await.unwrap().text, "token=[redacted]");
        fake.assert_done();
    }

    #[tokio::test]
    async fn a_cancelled_token_cancels_the_call() {
        let fake = FakeRunner::new();
        fake.expect(ZPOOL, ["scrub", "rpool"], Reply::ok(""));
        let token = CancellationToken::new();
        token.cancel();
        let error = fake
            .run(
                Command::new(ZPOOL)
                    .args(["scrub", "rpool"])
                    .cancel_on(token),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ExecError::Cancelled { .. }));
    }

    #[tokio::test]
    async fn records_stdin_for_assertions() {
        let fake = FakeRunner::new();
        fake.expect(
            "/usr/sbin/cryptsetup",
            ["open", "/dev/sdb2", "luks-x"],
            Reply::ok(""),
        );
        fake.run(
            Command::new("/usr/sbin/cryptsetup")
                .args(["open", "/dev/sdb2", "luks-x"])
                .secret_stdin("pass"),
        )
        .await
        .unwrap();
        assert_eq!(fake.calls()[0].stdin.as_deref(), Some(&b"pass"[..]));
    }
}
