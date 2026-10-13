// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The backup appliance service.

use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("bmac-backupd {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "bmac-backupd {}: not implemented yet",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::FAILURE
}
