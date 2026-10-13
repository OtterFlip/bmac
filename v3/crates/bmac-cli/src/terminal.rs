// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Talking to the operator: prompts on the controlling terminal, secrets
//! read with echo off, and a scripted stand-in for tests.

use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::os::fd::AsFd;
use std::sync::{Arc, Mutex};

use rustix::termios::{LocalModes, OptionalActions, Termios};

pub trait Prompter: Send {
    /// Shows a line of text.
    fn say(&mut self, text: &str);
    /// Reads one line without its newline; `None` at the end of input.
    fn line(&mut self, prompt: &str) -> std::io::Result<Option<String>>;
    /// Reads one line with echo off.
    fn secret(&mut self, prompt: &str) -> std::io::Result<Option<String>>;
    /// Whether someone is there to answer.
    fn interactive(&self) -> bool;
}

/// The terminal settings to put back if the program stops while echo is off.
static SAVED: Mutex<Option<Termios>> = Mutex::new(None);

/// Turns echo back on after an interrupted secret prompt.
pub fn restore() {
    if let Some(saved) = SAVED.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = rustix::termios::tcsetattr(std::io::stdin().as_fd(), OptionalActions::Now, &saved);
    }
}

pub fn stdin_is_terminal() -> bool {
    rustix::termios::isatty(std::io::stdin().as_fd())
}

/// Prompts on stderr and reads stdin, so `--json` output on stdout stays
/// clean.
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

    fn interactive(&self) -> bool {
        stdin_is_terminal()
    }
}

/// Answers prompts from a list and records what was shown, for tests.
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

    pub fn push(&self, answer: impl Into<String>) {
        self.answers.lock().unwrap().push_back(answer.into());
    }

    pub fn remaining(&self) -> usize {
        self.answers.lock().unwrap().len()
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

    fn interactive(&self) -> bool {
        true
    }
}
