// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Embeds the built web app (`webui/dist`, or `$BMACD_WEBUI_DIST`) into the
//! binary. Without a build, the front end serves a placeholder page.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect(&path, files);
        } else if path.is_file() {
            files.push(path);
        }
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=BMACD_WEBUI_DIST");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let dist = std::env::var_os("BMACD_WEBUI_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("../../webui/dist"));
    println!("cargo:rerun-if-changed={}", dist.display());

    let mut files = Vec::new();
    if dist.is_dir() {
        collect(&dist, &mut files);
    }
    let mut out = String::from("pub static EMBEDDED: &[(&str, &[u8])] = &[\n");
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let relative = file.strip_prefix(&dist).expect("inside dist");
        let url: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        let absolute = file.canonicalize().expect("canonical path");
        writeln!(
            out,
            "    ({:?}, include_bytes!({:?})),",
            format!("/{}", url.join("/")),
            absolute
        )
        .unwrap();
    }
    out.push_str("];\n");
    let target = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR")).join("assets.rs");
    std::fs::write(target, out).expect("write assets.rs");
}
