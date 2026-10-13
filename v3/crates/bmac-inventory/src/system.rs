// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! CPU, memory, firmware mode, Secure Boot, and DMI.

use std::collections::BTreeSet;
use std::fs;

use bmac_proto::inventory::{CpuInfo, Firmware, FirmwareMode, SecureBoot, SystemInfo};

use crate::root::{Root, attr};

const SECURE_BOOT_VAR: &str = "SecureBoot-8be4df61-93ca-11d2-aa0d-00e098032b8c";

pub fn read_cpu(root: &Root) -> CpuInfo {
    let text = fs::read_to_string(root.proc.join("cpuinfo")).unwrap_or_default();
    let mut model = None;
    let mut threads = 0;
    let mut sockets = BTreeSet::new();
    let mut cores = BTreeSet::new();
    let mut socket = String::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "processor" => threads += 1,
            "model name" if model.is_none() => model = Some(value.to_string()),
            "physical id" => {
                socket = value.to_string();
                sockets.insert(socket.clone());
            }
            "core id" => {
                cores.insert((socket.clone(), value.to_string()));
            }
            _ => {}
        }
    }
    CpuInfo {
        model,
        sockets: sockets.len().max(usize::from(threads > 0)) as u32,
        cores: if cores.is_empty() {
            threads
        } else {
            cores.len() as u32
        },
        threads,
    }
}

pub fn read_memory(root: &Root) -> Option<u64> {
    let text = fs::read_to_string(root.proc.join("meminfo")).ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

pub fn read_firmware(root: &Root) -> Firmware {
    let efi = root.sys.join("firmware/efi");
    if !efi.is_dir() {
        return Firmware {
            mode: FirmwareMode::Bios,
            secure_boot: SecureBoot::NotApplicable,
        };
    }
    // Four bytes of attributes, then the value.
    let secure_boot = match fs::read(efi.join("efivars").join(SECURE_BOOT_VAR)) {
        Ok(data) if data.len() >= 5 => {
            if data[4] == 1 {
                SecureBoot::Enabled
            } else {
                SecureBoot::Disabled
            }
        }
        _ => SecureBoot::Unknown,
    };
    Firmware {
        mode: FirmwareMode::Uefi,
        secure_boot,
    }
}

pub fn read_system(root: &Root) -> SystemInfo {
    let dmi = root.sys.join("class/dmi/id");
    // Firmware fills unset fields with placeholders.
    let real = |value: String| {
        let lower = value.to_ascii_lowercase();
        let placeholder = [
            "to be filled",
            "not specified",
            "default string",
            "system serial number",
            "none",
        ]
        .iter()
        .any(|p| lower.contains(p));
        (!placeholder).then_some(value)
    };
    SystemInfo {
        vendor: attr(&dmi.join("sys_vendor")).and_then(real),
        product: attr(&dmi.join("product_name")).and_then(real),
        serial: attr(&dmi.join("product_serial")).and_then(real),
    }
}
