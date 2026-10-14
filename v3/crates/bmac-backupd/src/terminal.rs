// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Prompts at the appliance's console: lines, secrets read with echo off,
//! and a scripted stand-in for tests.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use rustix::termios::{LocalModes, OptionalActions, Termios};

pub trait Prompter: Send {
    fn say(&mut self, text: &str);
    /// One line without its newline; `None` at the end of input.
    fn line(&mut self, prompt: &str) -> std::io::Result<Option<String>>;
    /// One line with echo off.
    fn secret(&mut self, prompt: &str) -> std::io::Result<Option<String>>;
}

/// Refuses a session that arrived over SSH: passphrases are typed at the
/// appliance's console (`BACKUP.md`, "Appliance platform").
pub fn at_console(env: impl Fn(&str) -> Option<String>) -> Result<(), String> {
    if ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"]
        .iter()
        .any(|name| env(name).is_some_and(|v| !v.is_empty()))
    {
        return Err("run this at the appliance's console, not over SSH".into());
    }
    Ok(())
}

static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Turns echo back on after an interrupted secret prompt.
pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = rustix::termios::tcsetattr(std::io::stdin().as_fd(), OptionalActions::Now, &saved);
    }
}

pub struct Tty;

fn read_line() -> std::io::Result<Option<String>> {
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line)? == 0 {
        return Ok(None);
    }
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
    Ok(Some(line))
}

impl Prompter for Tty {
    fn say(&mut self, text: &str) {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    fn line(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        read_line()
    }

    fn secret(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        eprint!("{prompt}");
        std::io::stderr().flush()?;
        let stdin = std::io::stdin();
        let fd = stdin.as_fd();
        let original = rustix::termios::tcgetattr(fd).ok();
        if let Some(original) = &original {
            let mut quiet = original.clone();
            quiet.local_modes.remove(LocalModes::ECHO);
            *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(original.clone());
            rustix::termios::tcsetattr(fd, OptionalActions::Now, &quiet)?;
        }
        let line = read_line();
        if original.is_some() {
            restore();
            eprintln!();
        }
        line
    }
}

/// Answers prompts from a list and records what was shown.
#[derive(Clone, Default)]
pub struct Scripted {
    answers: Arc<Mutex<VecDeque<String>>>,
    pub shown: Arc<Mutex<Vec<String>>>,
}

impl Scripted {
    pub fn new<S: Into<String>>(answers: impl IntoIterator<Item = S>) -> Scripted {
        Scripted {
            answers: Arc::new(Mutex::new(answers.into_iter().map(Into::into).collect())),
            shown: Arc::default(),
        }
    }

    pub fn text(&self) -> String {
        self.shown.lock().unwrap().join("\n")
    }

    fn next(&mut self, prompt: &str, secret: bool) -> std::io::Result<Option<String>> {
        let answer = self.answers.lock().unwrap().pop_front();
        let echoed = match (&answer, secret) {
            (Some(_), true) => "<secret>".to_string(),
            (Some(a), false) => a.clone(),
            (None, _) => "<end of input>".to_string(),
        };
        self.shown.lock().unwrap().push(format!("{prompt}{echoed}"));
        Ok(answer)
    }
}

impl Prompter for Scripted {
    fn say(&mut self, text: &str) {
        self.shown.lock().unwrap().push(text.to_string());
    }

    fn line(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        self.next(prompt, false)
    }

    fn secret(&mut self, prompt: &str) -> std::io::Result<Option<String>> {
        self.next(prompt, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_sessions_are_refused() {
        assert!(at_console(|_| None).is_ok());
        assert!(at_console(|n| (n == "SSH_TTY").then(|| "/dev/pts/0".into())).is_err());
        assert!(at_console(|n| (n == "SSH_CONNECTION").then(String::new)).is_ok());
    }
}
