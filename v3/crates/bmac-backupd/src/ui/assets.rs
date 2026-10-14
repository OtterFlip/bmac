// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The appliance UI's static files, embedded at build time.

use std::collections::BTreeMap;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
    <title>BMAC appliance</title></head><body><h1>BMAC appliance</h1>\
    <p>This build of bmac-backupd does not include its UI.</p></body></html>\n";

#[derive(Debug, Clone, Default)]
pub struct Assets {
    files: BTreeMap<String, &'static [u8]>,
}

pub fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
    match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

impl Assets {
    pub fn embedded() -> Assets {
        let assets = Assets::from_files(embedded::EMBEDDED.iter().copied());
        if assets.get("/index.html").is_some() {
            assets
        } else {
            Assets::from_files([("/index.html", PLACEHOLDER.as_bytes())])
        }
    }

    pub fn from_files(files: impl IntoIterator<Item = (&'static str, &'static [u8])>) -> Assets {
        Assets {
            files: files
                .into_iter()
                .map(|(path, bytes)| (path.to_string(), bytes))
                .collect(),
        }
    }

    pub fn get(&self, path: &str) -> Option<&'static [u8]> {
        self.files.get(path).copied()
    }

    /// The file for a request path; page routes get `index.html`.
    pub fn resolve<'p>(&self, path: &'p str) -> Option<(&'p str, &'static [u8])> {
        if let Some(bytes) = self.get(path).filter(|_| path != "/") {
            return Some((path, bytes));
        }
        let last = path.rsplit('/').next().unwrap_or_default();
        if path == "/"
            || (!last.contains('.') && !path.starts_with("/assets/") && !path.starts_with("/api/"))
        {
            return self.get("/index.html").map(|bytes| ("/index.html", bytes));
        }
        None
    }
}
