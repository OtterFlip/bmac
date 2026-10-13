// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The systemd units in `packaging/` agree with the code and keep the front
//! end's sandbox. The lab checks the same units with `systemd-analyze
//! security` and by trying the forbidden paths.

use std::collections::BTreeMap;
use std::path::Path;

use bmac_proto::engine::ENGINE_SOCKET;
use bmacd::peers::WEB_USER;
use bmacd::settings::{INSTALLED, RUN_CONFIG_DIR};
use bmacd::supervisor::LEGACY_ROOT;
use bmacd::web::{CERT_CREDENTIAL, KEY_CREDENTIAL};

/// Every `Key=value` line, keyed by name, values in file order.
fn unit(name: &str) -> BTreeMap<String, Vec<String>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging")
        .join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut keys: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in text.lines().map(str::trim) {
        if line.starts_with('#') || line.starts_with('[') || line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("{name}: {line}"));
        keys.entry(key.to_string())
            .or_default()
            .push(value.to_string());
    }
    keys
}

fn one<'a>(unit: &'a BTreeMap<String, Vec<String>>, key: &str) -> &'a str {
    match unit.get(key).map(Vec::as_slice) {
        Some([value]) => value,
        other => panic!("{key}: expected one value, got {other:?}"),
    }
}

#[test]
fn engine_unit_matches_the_code() {
    let engine = unit("systemd/bmacd.service");
    assert_eq!(one(&engine, "ExecStart"), "/usr/lib/bmac/bmacd engine");
    assert_eq!(one(&engine, "Type"), "notify");
    let runtime = Path::new(ENGINE_SOCKET).parent().unwrap();
    assert_eq!(
        Path::new("/run").join(one(&engine, "RuntimeDirectory")),
        runtime
    );
    assert_eq!(one(&engine, "RuntimeDirectoryMode"), "0750");
    assert!(
        one(&engine, "Wants")
            .split(' ')
            .any(|u| u == "bmacd-web.service")
    );
    assert!(!engine.contains_key("User"), "the engine runs as root");
    assert_eq!(one(&engine, "RuntimeDirectoryPreserve"), "yes");
    let before: Vec<&str> = one(&engine, "Before").split(' ').collect();
    for unit in ["pve-guests.service", "pve-ha-lrm.service"] {
        assert!(
            before.contains(&unit),
            "guests start after the script config: {unit}"
        );
    }
}

/// The installed path of a legacy file, checked against this tree.
fn legacy_file(installed: &str) -> std::path::PathBuf {
    let relative = installed
        .strip_prefix(LEGACY_ROOT)
        .and_then(|p| p.strip_prefix('/'))
        .unwrap_or_else(|| panic!("{installed} is not under {LEGACY_ROOT}"));
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../legacy")
        .join(relative);
    assert!(path.is_file(), "{} is missing", path.display());
    path
}

#[test]
fn timer_services_run_installed_legacy_scripts_with_current_config() {
    let condition = format!("{RUN_CONFIG_DIR}/cluster.conf");
    for (service, timer, script) in [
        (
            "bmac-deferred-cleanup.service",
            "bmac-deferred-cleanup.timer",
            "process_deferred_cleanup.sh",
        ),
        (
            "bmac-haproxy-route-sync.service",
            "bmac-haproxy-route-sync.timer",
            "sync_haproxy_routes.sh",
        ),
    ] {
        let unit = unit(&format!("systemd/{service}"));
        assert_eq!(one(&unit, "Type"), "oneshot", "{service}");
        assert_eq!(one(&unit, "ConditionPathExists"), condition, "{service}");
        assert_eq!(
            one(&unit, "ExecStartPre"),
            "/usr/lib/bmac/bmacd render-config",
            "{service}"
        );
        let exec = one(&unit, "ExecStart");
        let program = exec.split(' ').next().unwrap();
        assert!(
            program.ends_with(&format!("/{script}")),
            "{service}: {exec}"
        );
        legacy_file(program);
        let timer = self::unit(&format!("systemd/{timer}"));
        assert_eq!(one(&timer, "WantedBy"), "timers.target");
        assert!(timer.contains_key("OnUnitActiveSec"));
    }
    let ingress = unit("systemd/bmac-haproxy-ingress.service.d/route-sync.conf");
    assert_eq!(
        ingress["ExecStartPre"].last().unwrap().split(' ').next(),
        one(
            &unit("systemd/bmac-haproxy-route-sync.service"),
            "ExecStart"
        )
        .split(' ')
        .next()
    );
}

#[test]
fn the_hook_snippet_is_the_legacy_hook() {
    let (_, path) = INSTALLED
        .iter()
        .find(|(name, _)| *name == "GUEST_ROLE_HOOK_PATH")
        .unwrap();
    let name = path.strip_prefix("local:snippets/").unwrap();
    legacy_file(&format!("{LEGACY_ROOT}/scripts/host_runtime/{name}"));
}

#[test]
fn front_end_unit_is_sandboxed() {
    let web = unit("systemd/bmacd-web.service");
    assert_eq!(one(&web, "ExecStart"), "/usr/lib/bmac/bmacd web");
    assert_eq!(one(&web, "User"), WEB_USER);
    assert_eq!(one(&web, "Group"), WEB_USER);
    assert_eq!(one(&web, "BindsTo"), "bmacd.service");
    assert_eq!(one(&web, "PartOf"), "bmacd.service");
    assert_eq!(
        web["LoadCredential"],
        [
            format!("{KEY_CREDENTIAL}:/etc/bmac/tls/https.key"),
            format!("{CERT_CREDENTIAL}:/etc/bmac/tls/https.pem")
        ]
    );
    for (key, value) in [
        ("NoNewPrivileges", "yes"),
        ("CapabilityBoundingSet", ""),
        ("AmbientCapabilities", ""),
        ("ProtectSystem", "strict"),
        ("ProtectHome", "yes"),
        ("PrivateTmp", "yes"),
        ("PrivateDevices", "yes"),
        ("RestrictAddressFamilies", "AF_INET AF_INET6 AF_UNIX"),
        ("RestrictSUIDSGID", "yes"),
        ("MemoryDenyWriteExecute", "yes"),
        ("SystemCallArchitectures", "native"),
    ] {
        assert_eq!(one(&web, key), value, "{key}");
    }
    let hidden: Vec<&str> = one(&web, "InaccessiblePaths").split(' ').collect();
    for path in ["-/etc/pve", "-/etc/bmac", "-/var/lib/bmac"] {
        assert!(hidden.contains(&path), "{path} must be inaccessible");
    }
    assert_eq!(
        web["SystemCallFilter"],
        ["@system-service", "~@privileged @resources"]
    );
    for forbidden in [
        "ReadWritePaths",
        "SupplementaryGroups",
        "PrivilegedMode",
        "PermissionsStartOnly",
    ] {
        assert!(!web.contains_key(forbidden), "{forbidden}");
    }
}

#[test]
fn sysusers_creates_the_front_end_user() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/sysusers.d/bmacd.conf");
    let text = std::fs::read_to_string(path).unwrap();
    let entry = text.lines().find(|l| l.starts_with("u ")).unwrap();
    let fields: Vec<&str> = entry.split_whitespace().collect();
    assert_eq!(fields[1], WEB_USER);
    assert!(entry.ends_with("/ /usr/sbin/nologin"));
}
