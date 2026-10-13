// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The real runner. Each child leads its own process group, so a timeout or
//! cancellation stops everything it started (`supervisor.rs` 99–105, 215).

use std::os::unix::process::ExitStatusExt;
use std::process::{ExitStatus, Stdio};
use std::time::Instant;

use rustix::process::{Pid, Signal, kill_process_group};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::time::{sleep, sleep_until, timeout_at};

use crate::command::{Collector, Ending, finish};
use crate::{BoxFuture, Command, CommandRunner, ExecError, Exit, Output, Stream};

#[derive(Debug, Clone, Copy, Default)]
pub struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, command: Command) -> BoxFuture<'_, Result<Output, ExecError>> {
        Box::pin(run(command))
    }
}

fn exit_of(status: ExitStatus) -> Exit {
    match (status.code(), status.signal()) {
        (Some(code), _) => Exit::Code(code),
        (None, Some(signal)) => Exit::Signal(signal),
        (None, None) => Exit::Code(-1),
    }
}

fn signal_group(pgid: Option<Pid>, signal: Signal) {
    if let Some(pgid) = pgid {
        // The group may already be gone; nothing to do then.
        let _ = kill_process_group(pgid, signal);
    }
}

async fn collect<R: AsyncRead + Unpin>(
    mut reader: Option<R>,
    mut collector: Collector<'_>,
) -> crate::command::Captured {
    if let Some(reader) = reader.as_mut() {
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => collector.push(&buffer[..n]),
            }
        }
    }
    collector.finish()
}

/// SIGTERM to the group, then SIGKILL if the child outlives the grace period.
async fn stop(
    child: &mut Child,
    pgid: Option<Pid>,
    command: &Command,
) -> Result<ExitStatus, std::io::Error> {
    signal_group(pgid, Signal::TERM);
    tokio::select! {
        status = child.wait() => status,
        () = sleep(command.kill_after) => {
            signal_group(pgid, Signal::KILL);
            child.wait().await
        }
    }
}

async fn run(command: Command) -> Result<Output, ExecError> {
    if !command.program.is_absolute() {
        return Err(ExecError::RelativeProgram {
            program: command.name(),
        });
    }
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + command.timeout;
    let mut process = tokio::process::Command::new(&command.program);
    process
        .args(&command.args)
        .env_clear()
        .envs(command.environment())
        .current_dir(command.cwd.as_deref().unwrap_or("/".as_ref()))
        .stdin(if command.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(false);
    let mut child = process.spawn().map_err(|source| ExecError::Spawn {
        command: command.to_string(),
        source,
    })?;
    let pgid = child.id().and_then(|id| Pid::from_raw(id as i32));

    if let (Some(data), Some(mut writer)) = (command.stdin.clone(), child.stdin.take()) {
        // A child that exits without reading its input is not an error here;
        // its exit status says what happened.
        tokio::spawn(async move {
            let _ = writer.write_all(&data).await;
        });
    }
    let stdout = collect(
        child.stdout.take(),
        Collector::new(&command, Stream::Stdout),
    );
    let stderr = collect(
        child.stderr.take(),
        Collector::new(&command, Stream::Stderr),
    );
    let readers = async { tokio::join!(stdout, stderr) };
    tokio::pin!(readers);

    let cancelled = async {
        match &command.cancel {
            Some(token) => token.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(cancelled);
    let io_error = |source| ExecError::Io {
        command: command.to_string(),
        source,
    };

    enum Stop {
        Exited(ExitStatus),
        TimedOut,
        Cancelled,
    }
    // The pipes are drained while waiting, or a child writing more than a
    // pipe buffer would block forever.
    let mut captured = None;
    let first = loop {
        tokio::select! {
            status = child.wait() => break Stop::Exited(status.map_err(io_error)?),
            done = &mut readers, if captured.is_none() => captured = Some(done),
            () = sleep_until(deadline) => break Stop::TimedOut,
            () = &mut cancelled => break Stop::Cancelled,
        }
    };
    let (ending, (out, err)) = match first {
        Stop::Exited(status) => match captured {
            Some(done) => (Ending::Exited(exit_of(status)), done),
            // The child is gone, but something it started may still hold the
            // pipes open. Wait for them only until the deadline.
            None => match timeout_at(deadline, &mut readers).await {
                Ok(done) => (Ending::Exited(exit_of(status)), done),
                Err(_) => {
                    signal_group(pgid, Signal::KILL);
                    (Ending::TimedOut(exit_of(status)), readers.await)
                }
            },
        },
        Stop::TimedOut | Stop::Cancelled => {
            let status = stop(&mut child, pgid, &command).await.map_err(io_error)?;
            // Anything still alive in the group after the leader exits.
            signal_group(pgid, Signal::KILL);
            let done = match captured {
                Some(done) => done,
                None => readers.await,
            };
            let exit = exit_of(status);
            let ending = if matches!(first, Stop::TimedOut) {
                Ending::TimedOut(exit)
            } else {
                Ending::Cancelled(exit)
            };
            (ending, done)
        }
    };
    finish(&command, ending, out, err, started.elapsed())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use tokio::sync::mpsc;

    use super::*;
    use crate::CancellationToken;

    const SH: &str = "/bin/sh";

    fn sh(script: &str) -> Command {
        Command::new(SH)
            .args(["-c", script])
            .timeout(Duration::from_secs(20))
    }

    fn alive(pid: i32) -> bool {
        // A zombie has been killed; only count processes still running.
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map(|stat| {
                !stat
                    .rsplit(')')
                    .next()
                    .unwrap_or("")
                    .trim_start()
                    .starts_with('Z')
            })
            .unwrap_or(false)
    }

    async fn wait_dead(pid: i32) -> bool {
        for _ in 0..100 {
            if !alive(pid) {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
        false
    }

    #[tokio::test]
    async fn captures_output_and_exit_code() {
        let output = RealRunner.run(sh("echo out; echo err >&2")).await.unwrap();
        assert_eq!(output.exit, Exit::Code(0));
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
    }

    #[tokio::test]
    async fn unexpected_exit_codes_are_errors_with_the_stderr_tail() {
        let error = RealRunner
            .run(sh("echo 'no such pool' >&2; exit 3"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ExecError::UnexpectedExit { output, .. } if output.exit == Exit::Code(3))
        );
        assert!(
            error
                .to_string()
                .ends_with("failed with exit code 3: no such pool"),
            "{error}"
        );
        let allowed = RealRunner
            .run(sh("exit 1").expect_codes(&[0, 1]))
            .await
            .unwrap();
        assert_eq!(allowed.code(), Some(1));
    }

    #[tokio::test]
    async fn relative_programs_are_refused() {
        let error = RealRunner.run(Command::new("sh")).await.unwrap_err();
        assert!(matches!(error, ExecError::RelativeProgram { .. }));
    }

    #[tokio::test]
    async fn missing_programs_fail_to_spawn() {
        let error = RealRunner
            .run(Command::new("/nonexistent/bmac-test"))
            .await
            .unwrap_err();
        assert!(matches!(error, ExecError::Spawn { .. }), "{error}");
    }

    #[tokio::test]
    async fn environment_is_cleared_and_c_locale() {
        let output = RealRunner
            .run(Command::new("/usr/bin/env").env("FOO", "bar"))
            .await
            .unwrap();
        let mut lines: Vec<String> = output.stdout_text().lines().map(str::to_string).collect();
        lines.sort();
        assert_eq!(
            lines,
            [
                "FOO=bar",
                "LANG=C",
                "LC_ALL=C",
                &format!("PATH={}", crate::command::DEFAULT_PATH)
            ]
        );
    }

    #[tokio::test]
    async fn stdin_is_written_and_closed() {
        let output = RealRunner
            .run(Command::new("/bin/cat").stdin("hello"))
            .await
            .unwrap();
        assert_eq!(output.stdout, b"hello");
        let empty = RealRunner.run(Command::new("/bin/cat")).await.unwrap();
        assert!(
            empty.stdout.is_empty(),
            "without stdin the child reads /dev/null"
        );
    }

    #[tokio::test]
    async fn runs_in_the_given_directory() {
        let dir = tempfile::tempdir().unwrap();
        let output = RealRunner
            .run(Command::new("/bin/pwd").current_dir(dir.path()))
            .await
            .unwrap();
        let reported = output.stdout_text();
        assert_eq!(
            Path::new(reported.trim()).canonicalize().unwrap(),
            dir.path().canonicalize().unwrap()
        );
    }

    #[tokio::test]
    async fn timeouts_stop_the_whole_group() {
        let start = Instant::now();
        let error = RealRunner
            .run(
                sh("sleep 30 & echo $!; wait")
                    .timeout(Duration::from_millis(500))
                    .kill_after(Duration::from_secs(2)),
            )
            .await
            .unwrap_err();
        assert!(start.elapsed() < Duration::from_secs(10));
        let ExecError::TimedOut { output, .. } = &error else {
            panic!("{error}")
        };
        assert!(
            error.to_string().contains("timed out after 0.5s"),
            "{error}"
        );
        let grandchild: i32 = output.stdout_text().trim().parse().unwrap();
        assert!(
            wait_dead(grandchild).await,
            "the background sleep must be killed too"
        );
    }

    #[tokio::test]
    async fn cancellation_kills_grandchildren() {
        let token = CancellationToken::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(
            RealRunner.run(
                sh("sleep 30 & echo $!; wait")
                    .cancel_on(token.clone())
                    .stream_lines(tx),
            ),
        );
        let line = rx.recv().await.unwrap();
        let grandchild: i32 = line.text.parse().unwrap();
        assert!(alive(grandchild));
        token.cancel();
        let error = task.await.unwrap().unwrap_err();
        assert!(matches!(error, ExecError::Cancelled { .. }), "{error}");
        assert!(wait_dead(grandchild).await);
    }

    #[tokio::test]
    async fn a_child_ignoring_sigterm_is_killed_after_the_grace_period() {
        let start = Instant::now();
        let error = RealRunner
            .run(
                sh("trap '' TERM; while :; do sleep 0.1; done")
                    .timeout(Duration::from_millis(300))
                    .kill_after(Duration::from_millis(500)),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ExecError::TimedOut { output, .. } if output.exit == Exit::Signal(9)),
            "{error}"
        );
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn a_grandchild_holding_the_pipe_open_cannot_hang_the_runner() {
        let start = Instant::now();
        let error = RealRunner
            .run(sh("(sleep 30) & echo started").timeout(Duration::from_millis(700)))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ExecError::TimedOut { output, .. } if output.stdout == b"started\n"),
            "{error}"
        );
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn output_beyond_the_limit_is_discarded_and_flagged() {
        let output = RealRunner
            .run(sh("head -c 100000 /dev/zero").stdout_limit(1000))
            .await
            .unwrap();
        assert_eq!(output.stdout.len(), 1000);
        assert!(output.stdout_truncated && !output.stderr_truncated);
    }

    #[tokio::test]
    async fn secrets_are_redacted_everywhere() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let error = RealRunner
            .run(
                sh("read pw; echo \"got $pw\"; echo \"bad $pw\" >&2; exit 2")
                    .secret_stdin("hunter2\n")
                    .secret("hunter2")
                    .stream_lines(tx),
            )
            .await
            .unwrap_err();
        let output = error.output().unwrap();
        assert_eq!(output.stdout_text(), "got [redacted]\n");
        assert!(!error.to_string().contains("hunter2"), "{error}");
        let mut streamed = Vec::new();
        while let Ok(line) = rx.try_recv() {
            streamed.push(line.text);
        }
        streamed.sort();
        assert_eq!(streamed, ["bad [redacted]", "got [redacted]"]);
    }

    #[tokio::test]
    async fn lines_are_streamed_in_order_per_stream() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        RealRunner
            .run(sh("printf 'a\\nb\\r\\nc'").stream_lines(tx))
            .await
            .unwrap();
        let mut lines = Vec::new();
        while let Some(line) = rx.recv().await {
            assert_eq!(line.stream, Stream::Stdout);
            lines.push(line.text);
        }
        assert_eq!(lines, ["a", "b", "c"]);
    }
}
