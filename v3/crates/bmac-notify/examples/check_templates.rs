// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Checks the embedded templates against the Postmark server whose token is
//! in POSTMARK_SERVER_TOKEN, read-only, as Settings does:
//!
//!   cargo run -p bmac-notify --example check_templates

use std::process::ExitCode;

use bmac_notify::postmark::{Https, check};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let Ok(token) = std::env::var("POSTMARK_SERVER_TOKEN") else {
        eprintln!("set POSTMARK_SERVER_TOKEN");
        return ExitCode::FAILURE;
    };
    let result = match Https::new(&token) {
        Ok(postmark) => check(&postmark).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(view) => {
            println!(
                "Postmark server {:?} ({})",
                view.server_name, view.delivery_type
            );
            for status in view.templates {
                println!("  {}: {:?}", status.alias, status.presence);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ERROR: {e}");
            ExitCode::FAILURE
        }
    }
}
