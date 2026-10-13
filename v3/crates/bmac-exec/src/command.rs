// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The command description, its result, and the rules both runners share:
//! output limits, redaction, and expected exit codes.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// Shown in place of a registered secret in output, errors, and logs.
pub const REDACTED: &str = "[redacted]";

/// The `PATH` every child gets unless the command sets its own.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a cancelled or timed-out process group has between SIGTERM and
/// SIGKILL, as in v2's supervisor (`supervisor.rs` 37).
pub const DEFAULT_KILL_AFTER: Duration = Duration::from_secs(10);
pub const DEFAULT_STDOUT_LIMIT: usize = 16 * 1024 * 1024;
pub const DEFAULT_STDERR_LIMIT: usize = 1024 * 1024;
/// Streamed lines longer than this are cut.
pub const MAX_LINE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// One line of output, streamed while the command runs, with secrets
/// redacted and invalid UTF-8 replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLine {
    pub stream: Stream,
    pub text: String,
}

/// A command to run. Build it with [`Command::new`] and the setters.
#[derive(Clone)]
pub struct Command {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<String>,
    pub(crate) env: BTreeMap<String, String>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) timeout: Duration,
    pub(crate) kill_after: Duration,
    pub(crate) stdout_limit: usize,
    pub(crate) stderr_limit: usize,
    pub(crate) expected: Vec<i32>,
    pub(crate) secrets: Vec<String>,
    pub(crate) cancel: Option<CancellationToken>,
    pub(crate) lines: Option<UnboundedSender<OutputLine>>,
}

impl Command {
    /// `program` must be an absolute path; it is never looked up in `PATH`.
    pub fn new(program: impl Into<PathBuf>) -> Command {
        Command {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
            stdin: None,
            timeout: DEFAULT_TIMEOUT,
            kill_after: DEFAULT_KILL_AFTER,
            stdout_limit: DEFAULT_STDOUT_LIMIT,
            stderr_limit: DEFAULT_STDERR_LIMIT,
            expected: vec![0],
            secrets: Vec::new(),
            cancel: None,
            lines: None,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Command {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Command
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets one environment variable. The child sees only `PATH`, `LC_ALL=C`,
    /// `LANG=C`, and the variables set here.
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Command {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Passes one variable through from this process's environment, if set.
    pub fn inherit_env(self, key: &str) -> Command {
        match std::env::var(key) {
            Ok(value) => self.env(key, value),
            Err(_) => self,
        }
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Command {
        self.cwd = Some(dir.into());
        self
    }

    /// Bytes written to the child's stdin, which is then closed. Without
    /// this, stdin is `/dev/null`.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Command {
        self.stdin = Some(data.into());
        self
    }

    /// Like [`Command::stdin`], and also registers the text, without a
    /// trailing line ending, as a secret.
    pub fn secret_stdin(self, secret: impl Into<String>) -> Command {
        let secret = secret.into();
        let bare = secret.trim_end_matches(['\r', '\n']).to_string();
        self.stdin(secret.into_bytes()).secret(bare)
    }

    pub fn timeout(mut self, timeout: Duration) -> Command {
        self.timeout = timeout;
        self
    }

    pub fn kill_after(mut self, grace: Duration) -> Command {
        self.kill_after = grace;
        self
    }

    pub fn stdout_limit(mut self, bytes: usize) -> Command {
        self.stdout_limit = bytes;
        self
    }

    pub fn stderr_limit(mut self, bytes: usize) -> Command {
        self.stderr_limit = bytes;
        self
    }

    /// Exit codes that count as success. The default is `[0]`.
    pub fn expect_codes(mut self, codes: &[i32]) -> Command {
        self.expected = codes.to_vec();
        self
    }

    /// Text replaced by [`REDACTED`] in captured and streamed output, in
    /// errors, and in the command's own display. Empty strings are ignored.
    pub fn secret(mut self, secret: impl Into<String>) -> Command {
        let secret = secret.into();
        if !secret.is_empty() {
            self.secrets.push(secret);
        }
        self
    }

    pub fn cancel_on(mut self, token: CancellationToken) -> Command {
        self.cancel = Some(token);
        self
    }

    /// Streams each output line to `sender` while the command runs, in
    /// addition to capturing it.
    pub fn stream_lines(mut self, sender: UnboundedSender<OutputLine>) -> Command {
        self.lines = Some(sender);
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    pub fn arguments(&self) -> &[String] {
        &self.args
    }

    pub(crate) fn redact(&self, text: &str) -> String {
        redact(text, &self.secrets)
    }

    /// The environment the child receives.
    pub(crate) fn environment(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::from([
            ("PATH".to_string(), DEFAULT_PATH.to_string()),
            ("LC_ALL".to_string(), "C".to_string()),
            ("LANG".to_string(), "C".to_string()),
        ]);
        env.extend(self.env.clone());
        env
    }

    pub(crate) fn name(&self) -> String {
        self.program.display().to_string()
    }
}

/// The command line as an operator would read it, with secrets redacted.
/// For display only; it is never run by a shell.
impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.program.display())?;
        for arg in &self.args {
            let arg = self.redact(arg);
            if !arg.is_empty()
                && arg
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_./=:,@%+[]".contains(&b))
            {
                write!(f, " {arg}")?;
            } else {
                write!(f, " '{}'", arg.replace('\'', "'\\''"))?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Command")
            .field("command_line", &self.to_string())
            .field("env", &self.env.keys().collect::<Vec<_>>())
            .field("cwd", &self.cwd)
            .field("stdin_bytes", &self.stdin.as_ref().map(Vec::len))
            .field("timeout", &self.timeout)
            .field("expected", &self.expected)
            .finish_non_exhaustive()
    }
}

/// How the process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Code(i32),
    Signal(i32),
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Exit::Code(code) => write!(f, "exit code {code}"),
            Exit::Signal(signal) => write!(f, "signal {signal}"),
        }
    }
}

/// What the command produced. Output beyond the limits is discarded and the
/// `*_truncated` flag set; parsers must refuse truncated output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub exit: Exit,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub elapsed: Duration,
}

impl Output {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    pub fn code(&self) -> Option<i32> {
        match self.exit {
            Exit::Code(code) => Some(code),
            Exit::Signal(_) => None,
        }
    }

    /// The last few lines of stderr, for error messages.
    pub fn stderr_tail(&self) -> String {
        let text = self.stderr_text();
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        let tail = lines[lines.len().saturating_sub(5)..].join("\n");
        if tail.len() > 2000 {
            let mut start = tail.len() - 2000;
            while !tail.is_char_boundary(start) {
                start += 1;
            }
            format!("...{}", &tail[start..])
        } else {
            tail
        }
    }
}

fn with_tail(output: &Output) -> String {
    let tail = output.stderr_tail();
    if tail.is_empty() {
        String::new()
    } else {
        format!(": {tail}")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("{program} is not an absolute program path")]
    RelativeProgram { program: String },
    #[error("could not start {command}: {source}")]
    Spawn {
        command: String,
        source: std::io::Error,
    },
    /// The timeout passed. The whole process group was stopped. v2 reported
    /// this as exit 124; here it is its own error, never a hang or empty
    /// output.
    #[error("{command} timed out after {}s", after.as_secs_f64())]
    TimedOut {
        command: String,
        after: Duration,
        output: Box<Output>,
    },
    #[error("{command} was cancelled")]
    Cancelled {
        command: String,
        output: Box<Output>,
    },
    #[error("{command} failed with {}{}", output.exit, with_tail(output))]
    UnexpectedExit {
        command: String,
        output: Box<Output>,
    },
    /// Only from [`crate::FakeRunner`]: no expectation matched.
    #[error("unexpected command: {command}")]
    Unexpected { command: String },
    #[error("i/o error while running {command}: {source}")]
    Io {
        command: String,
        source: std::io::Error,
    },
}

impl ExecError {
    /// The output captured before the failure, when there was a process.
    pub fn output(&self) -> Option<&Output> {
        match self {
            ExecError::TimedOut { output, .. }
            | ExecError::Cancelled { output, .. }
            | ExecError::UnexpectedExit { output, .. } => Some(output),
            _ => None,
        }
    }
}

/// Replaces every occurrence of each secret.
pub(crate) fn redact(text: &str, secrets: &[String]) -> String {
    let mut text = text.to_string();
    for secret in secrets {
        if !secret.is_empty() && text.contains(secret.as_str()) {
            text = text.replace(secret.as_str(), REDACTED);
        }
    }
    text
}

pub(crate) fn redact_bytes(data: Vec<u8>, secrets: &[String]) -> Vec<u8> {
    let mut data = data;
    for secret in secrets
        .iter()
        .map(String::as_bytes)
        .filter(|s| !s.is_empty())
    {
        if data.windows(secret.len()).any(|w| w == secret) {
            let mut out = Vec::with_capacity(data.len());
            let mut i = 0;
            while i < data.len() {
                if data[i..].starts_with(secret) {
                    out.extend_from_slice(REDACTED.as_bytes());
                    i += secret.len();
                } else {
                    out.push(data[i]);
                    i += 1;
                }
            }
            data = out;
        }
    }
    data
}

/// Why the process stopped, from the runner's point of view.
pub(crate) enum Ending {
    Exited(Exit),
    TimedOut(Exit),
    Cancelled(Exit),
}

/// Raw captured output: the kept bytes and whether more was discarded.
pub(crate) struct Captured {
    pub data: Vec<u8>,
    pub truncated: bool,
}

/// Applies redaction and the exit-code rules. Both runners end here, so the
/// fake behaves exactly as the real runner does.
pub(crate) fn finish(
    command: &Command,
    ending: Ending,
    stdout: Captured,
    stderr: Captured,
    elapsed: Duration,
) -> Result<Output, ExecError> {
    let exit = match ending {
        Ending::Exited(exit) | Ending::TimedOut(exit) | Ending::Cancelled(exit) => exit,
    };
    let output = Box::new(Output {
        exit,
        stdout: redact_bytes(stdout.data, &command.secrets),
        stderr: redact_bytes(stderr.data, &command.secrets),
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        elapsed,
    });
    let line = command.to_string();
    match ending {
        Ending::TimedOut(_) => Err(ExecError::TimedOut {
            command: line,
            after: command.timeout,
            output,
        }),
        Ending::Cancelled(_) => Err(ExecError::Cancelled {
            command: line,
            output,
        }),
        Ending::Exited(Exit::Code(code)) if command.expected.contains(&code) => Ok(*output),
        Ending::Exited(_) => Err(ExecError::UnexpectedExit {
            command: line,
            output,
        }),
    }
}

/// Collects output up to a limit and splits it into streamed lines.
pub(crate) struct Collector<'a> {
    command: &'a Command,
    stream: Stream,
    limit: usize,
    pub captured: Captured,
    partial: Vec<u8>,
}

impl<'a> Collector<'a> {
    pub fn new(command: &'a Command, stream: Stream) -> Collector<'a> {
        let limit = match stream {
            Stream::Stdout => command.stdout_limit,
            Stream::Stderr => command.stderr_limit,
        };
        Collector {
            command,
            stream,
            limit,
            captured: Captured {
                data: Vec::new(),
                truncated: false,
            },
            partial: Vec::new(),
        }
    }

    pub fn push(&mut self, chunk: &[u8]) {
        let room = self.limit.saturating_sub(self.captured.data.len());
        if chunk.len() > room {
            self.captured.truncated = true;
        }
        self.captured
            .data
            .extend_from_slice(&chunk[..chunk.len().min(room)]);
        if self.command.lines.is_none() {
            return;
        }
        for &byte in chunk {
            if byte == b'\n' {
                self.emit();
            } else if self.partial.len() < MAX_LINE {
                self.partial.push(byte);
            }
        }
    }

    pub fn finish(mut self) -> Captured {
        if !self.partial.is_empty() {
            self.emit();
        }
        self.captured
    }

    fn emit(&mut self) {
        let text = String::from_utf8_lossy(&self.partial);
        let text = self.command.redact(text.trim_end_matches('\r'));
        self.partial.clear();
        if let Some(lines) = &self.command.lines {
            let _ = lines.send(OutputLine {
                stream: self.stream,
                text,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_quotes_like_a_shell_and_redacts() {
        let command = Command::new("/usr/bin/echo")
            .args(["plain", "two words", "it's", "", "pw=hunter2"])
            .secret("hunter2");
        assert_eq!(
            command.to_string(),
            "/usr/bin/echo plain 'two words' 'it'\\''s' '' pw=[redacted]"
        );
        assert!(!format!("{command:?}").contains("hunter2"));
    }

    #[test]
    fn redaction_replaces_every_occurrence() {
        let secrets = vec!["s3cret".to_string(), String::new()];
        assert_eq!(
            redact("a s3cret b s3cret", &secrets),
            "a [redacted] b [redacted]"
        );
        assert_eq!(
            redact_bytes(b"xs3crets3cret".to_vec(), &secrets),
            b"x[redacted][redacted]"
        );
        assert_eq!(redact_bytes(b"nothing".to_vec(), &secrets), b"nothing");
    }

    #[test]
    fn environment_is_minimal_and_c_locale() {
        let env = Command::new("/bin/true").env("FOO", "1").environment();
        assert_eq!(
            env.keys().collect::<Vec<_>>(),
            ["FOO", "LANG", "LC_ALL", "PATH"]
        );
        assert_eq!(env["LC_ALL"], "C");
        let overridden = Command::new("/bin/true")
            .env("PATH", "/opt/bin")
            .environment();
        assert_eq!(overridden["PATH"], "/opt/bin");
    }

    #[test]
    fn stderr_tail_keeps_the_last_lines() {
        let output = Output {
            exit: Exit::Code(1),
            stdout: vec![],
            stderr: b"1\n2\n\n3\n4\n5\n6\n".to_vec(),
            stdout_truncated: false,
            stderr_truncated: false,
            elapsed: Duration::ZERO,
        };
        assert_eq!(output.stderr_tail(), "2\n3\n4\n5\n6");
    }
}
