// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! `bmac-inventory`: prints the machine's inventory as JSON, or clears the
//! old ZFS labels named on stdin.

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bmac_inventory::clear::{ZpoolLabelClear, clear_zfs_labels};
use bmac_inventory::nics::{IpLink, LeaveAsIs, LinkControl};
use bmac_inventory::{Root, take};
use bmac_proto::inventory::{ClearRequest, LinkWait};

const USAGE: &str = "\
usage: bmac-inventory [--link-attempts N] [--link-interval SECONDS] [--no-link-wait]
       bmac-inventory clear-zfs-labels < request.json

Prints this machine's disks, NICs, CPU, memory, and firmware as JSON. Every
physical NIC is brought up and given time to get carrier first (10 checks,
3 seconds apart, unless changed).

clear-zfs-labels reads {\"disks\": [{\"serial\", \"wwn\", \"pool_guid\"}]} on stdin,
re-identifies each disk, and clears that pool's labelled devices only (with
zpool labelclear -f, or by zeroing the four label slots where ZFS isn't
installed). Nothing is touched if any disk doesn't match. Prints a report;
exits 1 unless every label is gone.";

struct Args {
    clear: bool,
    wait: Option<LinkWait>,
    root: Root,
}

fn number(
    value: Option<String>,
    name: &str,
    range: std::ops::RangeInclusive<u32>,
) -> Result<u32, String> {
    let value = value.ok_or_else(|| format!("{name} needs a value"))?;
    match value.parse::<u32>() {
        Ok(n) if range.contains(&n) => Ok(n),
        _ => Err(format!(
            "{name} must be a whole number from {} to {}",
            range.start(),
            range.end()
        )),
    }
}

fn parse(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut parsed = Args {
        clear: false,
        wait: Some(LinkWait::default()),
        root: Root::system(),
    };
    let mut args = args.peekable();
    if args.peek().map(String::as_str) == Some("clear-zfs-labels") {
        args.next();
        parsed.clear = true;
    }
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--link-attempts" if !parsed.clear => {
                let attempts = number(args.next(), "--link-attempts", 1..=100)?;
                parsed.wait = Some(LinkWait {
                    attempts,
                    ..parsed.wait.unwrap_or_default()
                });
            }
            "--link-interval" if !parsed.clear => {
                let interval_seconds = number(args.next(), "--link-interval", 1..=60)?;
                parsed.wait = Some(LinkWait {
                    interval_seconds,
                    ..parsed.wait.unwrap_or_default()
                });
            }
            "--no-link-wait" if !parsed.clear => parsed.wait = None,
            // For tests and debugging: read a fixture tree instead.
            "--root" => {
                let base = args.next().ok_or("--root needs a directory")?;
                parsed.root = Root::under(Path::new(&base));
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(parsed)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let args = match parse(raw.into_iter()) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("bmac-inventory: {message}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.clear {
        let mut input = String::new();
        if let Err(error) = std::io::stdin().read_to_string(&mut input) {
            eprintln!("bmac-inventory: couldn't read the request: {error}");
            return ExitCode::from(2);
        }
        let request: ClearRequest = match serde_json::from_str(&input) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("bmac-inventory: the request isn't valid: {error}");
                return ExitCode::from(2);
            }
        };
        let report = clear_zfs_labels(&args.root, &request, &ZpoolLabelClear::installed());
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
        return if report.succeeded() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }
    // A fixture tree's NICs aren't this machine's; never bring real ones up for it.
    let control: &dyn LinkControl = if args.root == Root::system() {
        &IpLink
    } else {
        &LeaveAsIs
    };
    let inventory = take(
        &args.root,
        args.wait,
        control,
        &mut |d: Duration| std::thread::sleep(d),
        now_ms(),
    );
    println!(
        "{}",
        serde_json::to_string_pretty(&inventory).unwrap_or_default()
    );
    ExitCode::SUCCESS
}
