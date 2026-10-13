// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Proxmox VM lifecycle hook.

use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("bmac-hook {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "bmac-hook {}: not implemented yet",
        env!("CARGO_PKG_VERSION")
    );
    ExitCode::FAILURE
}
