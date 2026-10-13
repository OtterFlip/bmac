// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The static web app, embedded at build time.

use std::collections::BTreeMap;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

/// Served when the binary was built without the web app.
const PLACEHOLDER: &str = "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
    <title>BMAC</title></head><body><h1>BMAC</h1>\
    <p>This build of bmacd does not include the web app.</p></body></html>\n";

#[derive(Debug, Clone, Default)]
pub struct Assets {
    files: BTreeMap<String, &'static [u8]>,
}

/// The `Content-Type` for a file name.
pub fn content_type(path: &str) -> &'static str {
    let extension = path.rsplit_once('.').map(|(_, e)| e).unwrap_or_default();
    match extension {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

impl Assets {
    /// The files built into this binary, or the placeholder page.
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

    /// The file for a request path. `/` and page routes without a file
    /// extension get `index.html`, since the app routes pages itself.
    pub fn resolve<'p>(&self, path: &'p str) -> Option<(&'p str, &'static [u8])> {
        if let Some(bytes) = self.get(path).filter(|_| path != "/") {
            return Some((path, bytes));
        }
        let last = path.rsplit('/').next().unwrap_or_default();
        if path == "/" || (!last.contains('.') && !path.starts_with("/assets/")) {
            return self.get("/index.html").map(|bytes| ("/index.html", bytes));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_routes_get_the_index() {
        let assets = Assets::from_files([
            ("/index.html", &b"index"[..]),
            ("/assets/app-1.js", &b"js"[..]),
        ]);
        assert_eq!(assets.resolve("/"), Some(("/index.html", &b"index"[..])));
        assert_eq!(
            assets.resolve("/hosts/mox1"),
            Some(("/index.html", &b"index"[..]))
        );
        assert_eq!(
            assets.resolve("/assets/app-1.js"),
            Some(("/assets/app-1.js", &b"js"[..]))
        );
        assert_eq!(assets.resolve("/assets/missing"), None);
        assert_eq!(assets.resolve("/favicon.ico"), None);
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("/index.html"), "text/html; charset=utf-8");
        assert_eq!(
            content_type("/assets/app.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(content_type("/x"), "application/octet-stream");
    }

    #[test]
    fn the_placeholder_stands_in_for_a_missing_build() {
        assert!(Assets::embedded().get("/index.html").is_some());
    }
}
