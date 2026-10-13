// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The command line.

use bmac_core::JobId;
use bmac_storage::{BlockSize, ZvolAllocation};

pub const USAGE: &str = "\
usage:
  bmac status [--json]
  bmac jobs [--json] [--user USER]
  bmac jobs JOB [--no-answer] [--user USER]
  bmac cancel JOB [--force] [--user USER]
  bmac run OPERATION [NAME=VALUE...] [--dry-run] [--detach] [--terminal] [--user USER]
  bmac luks-enter
  bmac guests [--json]
  bmac guest-host-key GUEST [--json]
  bmac capacity [--json] [--pool POOL] [--volsize BYTES --block-size BYTES]
  bmac growth --json --volsize BYTES --block-size BYTES
              (--guest-free BYTES | --refreservation BYTES) < CAPACITIES
  bmac --version

status, guests, guest-host-key, and capacity read this host directly and work
with bmacd stopped. growth reads every placement host's capacity --json output
on standard input and prints how far a disk may grow on all of them. jobs, cancel, and run log in through bmacd with a Proxmox
user, password, and second factor, every time; nothing is remembered.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    pub operation: String,
    /// `NAME=VALUE`, or a bare `NAME` for a flag that is on.
    pub params: Vec<(String, Option<String>)>,
    pub dry_run: bool,
    pub detach: bool,
    pub terminal: bool,
    pub user: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Version,
    Help,
    Status {
        json: bool,
    },
    Jobs {
        json: bool,
        user: Option<String>,
    },
    Follow {
        job: JobId,
        answer: bool,
        user: Option<String>,
    },
    Cancel {
        job: JobId,
        force: bool,
        user: Option<String>,
    },
    Run(RunArgs),
    LuksEnter,
    Guests {
        json: bool,
    },
    GuestHostKey {
        guest: String,
        json: bool,
    },
    Capacity {
        json: bool,
        pool: String,
        zvol: Option<(u64, BlockSize)>,
    },
    Growth {
        allocation: ZvolAllocation,
        volsize: u64,
        block: BlockSize,
    },
}

struct Words<'a> {
    words: std::slice::Iter<'a, String>,
    positional: Vec<String>,
    flags: Vec<String>,
    values: Vec<(String, String)>,
}

impl<'a> Words<'a> {
    /// Splits `args` into positional words, `--flag`s, and `--option VALUE`
    /// pairs for the options named in `with_value`.
    fn split(args: &'a [String], with_value: &[&str]) -> Result<Words<'a>, String> {
        let mut words = Words {
            words: args.iter(),
            positional: Vec::new(),
            flags: Vec::new(),
            values: Vec::new(),
        };
        while let Some(word) = words.words.next() {
            if let Some(name) = word.strip_prefix("--") {
                let (name, inline) = match name.split_once('=') {
                    Some((name, value)) => (name, Some(value.to_string())),
                    None => (name, None),
                };
                if with_value.contains(&name) {
                    let value = match inline {
                        Some(value) => value,
                        None => words
                            .words
                            .next()
                            .cloned()
                            .ok_or_else(|| format!("--{name} needs a value"))?,
                    };
                    words.values.push((name.to_string(), value));
                } else if inline.is_some() {
                    return Err(format!("--{name} takes no value"));
                } else {
                    words.flags.push(name.to_string());
                }
            } else {
                words.positional.push(word.clone());
            }
        }
        Ok(words)
    }

    fn only(&self, flags: &[&str]) -> Result<(), String> {
        match self.flags.iter().find(|f| !flags.contains(&f.as_str())) {
            Some(flag) => Err(format!("unknown option --{flag}")),
            None => Ok(()),
        }
    }

    fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|f| f == name)
    }

    fn value(&self, name: &str) -> Option<String> {
        self.values
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.clone())
    }

    fn positional(&self, max: usize) -> Result<&[String], String> {
        if self.positional.len() > max {
            return Err(format!("unexpected argument {}", self.positional[max]));
        }
        Ok(&self.positional)
    }
}

fn job(text: &str) -> Result<JobId, String> {
    text.parse().map_err(|_| format!("{text} is not a job ID"))
}

fn byte_count(name: &str, text: &str) -> Result<u64, String> {
    text.parse::<u64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("--{name} needs a positive number of bytes"))
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((name, rest)) = args.split_first() else {
        return Ok(Command::Help);
    };
    let words = |with_value: &[&str]| Words::split(rest, with_value);
    match name.as_str() {
        "--version" | "version" => Ok(Command::Version),
        "--help" | "-h" | "help" => Ok(Command::Help),
        "status" => {
            let w = words(&[])?;
            w.only(&["json"])?;
            w.positional(0)?;
            Ok(Command::Status {
                json: w.flag("json"),
            })
        }
        "jobs" => {
            let w = words(&["user"])?;
            w.only(&["json", "no-answer"])?;
            let user = w.value("user");
            match w.positional(1)? {
                [] => Ok(Command::Jobs {
                    json: w.flag("json"),
                    user,
                }),
                [id] => Ok(Command::Follow {
                    job: job(id)?,
                    answer: !w.flag("no-answer"),
                    user,
                }),
                _ => unreachable!("at most one"),
            }
        }
        "cancel" => {
            let w = words(&["user"])?;
            w.only(&["force"])?;
            match w.positional(1)? {
                [id] => Ok(Command::Cancel {
                    job: job(id)?,
                    force: w.flag("force"),
                    user: w.value("user"),
                }),
                _ => Err("cancel needs a job ID".to_string()),
            }
        }
        "run" => {
            let w = words(&["user"])?;
            w.only(&["dry-run", "detach", "terminal"])?;
            let Some((operation, params)) = w.positional.split_first() else {
                return Err("run needs an operation".to_string());
            };
            let params = params
                .iter()
                .map(|p| match p.split_once('=') {
                    Some((name, value)) => (name.to_string(), Some(value.to_string())),
                    None => (p.clone(), None),
                })
                .collect();
            if w.flag("terminal") && w.flag("detach") {
                return Err("--terminal runs in this terminal, so it can't --detach".to_string());
            }
            Ok(Command::Run(RunArgs {
                operation: operation.clone(),
                params,
                dry_run: w.flag("dry-run"),
                detach: w.flag("detach"),
                terminal: w.flag("terminal"),
                user: w.value("user"),
            }))
        }
        "luks-enter" => {
            let w = words(&[])?;
            w.only(&[])?;
            w.positional(0)?;
            Ok(Command::LuksEnter)
        }
        "guests" => {
            let w = words(&[])?;
            w.only(&["json"])?;
            w.positional(0)?;
            Ok(Command::Guests {
                json: w.flag("json"),
            })
        }
        "guest-host-key" => {
            let w = words(&[])?;
            w.only(&["json"])?;
            match w.positional(1)? {
                [guest] => Ok(Command::GuestHostKey {
                    guest: guest.clone(),
                    json: w.flag("json"),
                }),
                _ => Err("guest-host-key needs a guest name".to_string()),
            }
        }
        "capacity" => {
            let w = words(&["pool", "volsize", "block-size"])?;
            w.only(&["json"])?;
            w.positional(0)?;
            let zvol = match (w.value("volsize"), w.value("block-size")) {
                (None, None) => None,
                (Some(volsize), Some(block)) => {
                    let block = u32::try_from(byte_count("block-size", &block)?)
                        .map_err(|e| e.to_string())
                        .and_then(BlockSize::try_from)?;
                    Some((byte_count("volsize", &volsize)?, block))
                }
                _ => return Err("--volsize and --block-size go together".to_string()),
            };
            Ok(Command::Capacity {
                json: w.flag("json"),
                pool: w.value("pool").unwrap_or_else(|| "rpool".to_string()),
                zvol,
            })
        }
        "growth" => {
            let w = words(&["volsize", "block-size", "guest-free", "refreservation"])?;
            w.only(&["json"])?;
            w.positional(0)?;
            if !w.flag("json") {
                return Err("growth only prints JSON; give --json".to_string());
            }
            let needed = |name: &str| {
                w.value(name)
                    .ok_or_else(|| format!("growth needs --{name}"))
                    .and_then(|value| byte_count(name, &value))
            };
            let block = u32::try_from(needed("block-size")?)
                .map_err(|e| e.to_string())
                .and_then(BlockSize::try_from)?;
            let allocation = match (w.value("guest-free"), w.value("refreservation")) {
                (Some(free), None) => ZvolAllocation::Sparse {
                    guest_free: free
                        .parse()
                        .map_err(|_| "--guest-free needs a number of bytes".to_string())?,
                },
                (None, Some(reserved)) => ZvolAllocation::Reserved {
                    refreservation: byte_count("refreservation", &reserved)?,
                },
                _ => {
                    return Err("growth needs one of --guest-free and --refreservation".to_string());
                }
            };
            Ok(Command::Growth {
                allocation,
                volsize: needed("volsize")?,
                block,
            })
        }
        other => Err(format!("unknown command {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(line: &str) -> Result<Command, String> {
        let args: Vec<String> = line.split_whitespace().map(str::to_string).collect();
        parse(&args)
    }

    #[test]
    fn commands_and_their_options() {
        assert_eq!(parsed(""), Ok(Command::Help));
        assert_eq!(parsed("status --json"), Ok(Command::Status { json: true }));
        assert_eq!(
            parsed("run prod.add resource=prod2 cores=4 force --dry-run --user alice@pve"),
            Ok(Command::Run(RunArgs {
                operation: "prod.add".into(),
                params: vec![
                    ("resource".into(), Some("prod2".into())),
                    ("cores".into(), Some("4".into())),
                    ("force".into(), None),
                ],
                dry_run: true,
                detach: false,
                terminal: false,
                user: Some("alice@pve".into()),
            }))
        );
        let job = "6f9619ff-8b86-4011-b42d-00c04fc964ff";
        assert!(matches!(
            parsed(&format!("jobs {job} --no-answer")),
            Ok(Command::Follow { answer: false, .. })
        ));
        assert!(matches!(
            parsed(&format!("cancel {job} --force")),
            Ok(Command::Cancel { force: true, .. })
        ));
        assert_eq!(
            parsed("capacity --json --volsize=1073741824 --block-size 65536"),
            Ok(Command::Capacity {
                json: true,
                pool: "rpool".into(),
                zvol: Some((1 << 30, BlockSize::K64)),
            })
        );
        assert_eq!(
            parsed("growth --json --volsize 1073741824 --block-size 16384 --guest-free 0"),
            Ok(Command::Growth {
                allocation: ZvolAllocation::Sparse { guest_free: 0 },
                volsize: 1 << 30,
                block: BlockSize::K16,
            })
        );
        assert_eq!(
            parsed("guest-host-key prod1 --json"),
            Ok(Command::GuestHostKey {
                guest: "prod1".into(),
                json: true
            })
        );
    }

    #[test]
    fn mistakes_are_refused() {
        for (line, why) in [
            ("status --verbose", "unknown option --verbose"),
            ("jobs not-a-job", "not a job ID"),
            ("run", "needs an operation"),
            ("run x --terminal --detach", "can't --detach"),
            ("run x --user", "--user needs a value"),
            ("capacity --volsize 10", "go together"),
            (
                "capacity --volsize 10 --block-size 1000",
                "not a block size",
            ),
            (
                "growth --volsize 1 --block-size 16384 --guest-free 0",
                "give --json",
            ),
            (
                "growth --json --volsize 1 --block-size 16384",
                "one of --guest-free and --refreservation",
            ),
            (
                "growth --json --volsize 1 --block-size 16384 --guest-free 0 --refreservation 1",
                "one of --guest-free",
            ),
            (
                "growth --json --block-size 16384 --guest-free 0",
                "needs --volsize",
            ),
            ("guests extra", "unexpected argument extra"),
            ("luks-enter --json", "unknown option --json"),
            ("frobnicate", "unknown command"),
        ] {
            let error = parsed(line).unwrap_err();
            assert!(error.contains(why), "{line}: {error}");
        }
    }
}
