// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! ts-rs writes each type to `<Name>.ts`, so two exported types with the
//! same name silently overwrite each other in `ui/src/generated`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

fn exported_names(dir: &Path, found: &mut BTreeMap<String, Vec<String>>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            exported_names(&path, found);
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let mut derives_ts = false;
        let mut renamed: Option<String> = None;
        for line in text.lines().map(str::trim) {
            if line.starts_with("#[derive(") && line.contains("TS") {
                derives_ts = true;
            } else if let Some(rest) = line.strip_prefix("#[ts(rename = \"") {
                renamed = rest.split('"').next().map(str::to_string);
            } else if let Some(rest) = line
                .strip_prefix("pub struct ")
                .or_else(|| line.strip_prefix("pub enum "))
            {
                if derives_ts {
                    let name = rest
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .next()
                        .unwrap()
                        .to_string();
                    let name = renamed.take().unwrap_or(name);
                    found
                        .entry(name)
                        .or_default()
                        .push(path.display().to_string());
                }
                derives_ts = false;
                renamed = None;
            } else if !line.starts_with("#[") && !line.starts_with("///") && !line.is_empty() {
                derives_ts = false;
                renamed = None;
            }
        }
    }
}

#[test]
fn every_exported_type_has_its_own_name() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut found = BTreeMap::new();
    for krate in ["bmac-proto", "bmac-core"] {
        exported_names(&crates.join(krate).join("src"), &mut found);
    }
    assert!(
        found.contains_key("InventoryDisk"),
        "the scan found nothing"
    );
    let clashes: Vec<_> = found.iter().filter(|(_, paths)| paths.len() > 1).collect();
    assert!(
        clashes.is_empty(),
        "types exported under the same name: {clashes:?}"
    );
}
