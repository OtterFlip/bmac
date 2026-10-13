// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::process::Stdio;

use super::*;

fn plan() -> HelloPlan {
    HelloPlan {
        guest: "prod1".into(),
        primary: "example.com".into(),
        aliases: vec![
            "www.example.com".into(),
            "example.org".into(),
            "a.b.example.com".into(),
        ],
        max_stage: 3,
    }
}

fn guest() -> Guest {
    Guest {
        name: "prod1".into(),
        kind: "production".into(),
        state: "active".into(),
        vmid: 100,
        ip: "10.213.0.31".into(),
        node: Some("mox1".into()),
        status: "running".into(),
        primary_domain: "example.com".into(),
        aliases: vec!["www.example.com".into()],
        placement: vec!["mox1".into(), "mox3".into()],
    }
}

pub(crate) const NOW: i64 = 1_791_000_000;

/// A self-signed certificate for `names`, valid around [`NOW`] unless
/// `expired`.
pub(crate) fn certificate(names: &[&str], expired: bool) -> (String, String) {
    let mut params =
        rcgen::CertificateParams::new(names.iter().map(|n| n.to_string()).collect::<Vec<_>>())
            .unwrap();
    let (from, until) = if expired {
        (NOW - 86_400 * 30, NOW - 86_400)
    } else {
        (NOW - 86_400, NOW + 86_400 * 365)
    };
    params.not_before = time::OffsetDateTime::from_unix_timestamp(from).unwrap();
    params.not_after = time::OffsetDateTime::from_unix_timestamp(until).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key).unwrap();
    (cert.pem(), key.serialize_pem())
}

#[test]
fn every_production_and_staging_name_is_served() {
    assert_eq!(
        plan().server_names(),
        [
            "example.com",
            "www.example.com",
            "example.org",
            "a.b.example.com",
            "stage1prod1.example.com",
            "stage2prod1.example.com",
            "stage3prod1.example.com",
        ]
    );
    assert_eq!(
        plan().certificate_extra_names(),
        ["example.org", "a.b.example.com"]
    );
    let steps = plan().cloudflare_steps().join("\n");
    assert!(steps.contains("example.com and *.example.com, plus example.org, a.b.example.com"));
    let site = plan().site_config();
    assert_eq!(site.matches("        stage3prod1.example.com\n").count(), 2);
    assert!(site.contains("ssl_certificate_key /etc/nginx/ssl/cloudflare-origin.key;"));
    assert!(site.contains("return 301 https://$host$request_uri;"));
    assert!(plan().index_html().contains("<p>Deployed to prod1.</p>"));
}

#[test]
fn the_plan_takes_the_highest_staging_limit_of_the_placement() {
    let limits = BTreeMap::from([
        ("mox1".to_string(), 2),
        ("mox3".to_string(), 5),
        ("mox2".to_string(), 9),
    ]);
    let plan = HelloPlan::for_guest(&guest(), &limits).unwrap();
    assert_eq!(plan.max_stage, 5);

    let refuse =
        |g: Guest, limits: &BTreeMap<String, u32>| HelloPlan::for_guest(&g, limits).unwrap_err();
    assert!(
        refuse(
            Guest {
                kind: "staging".into(),
                ..guest()
            },
            &limits
        )
        .contains("not a production")
    );
    assert!(
        refuse(
            Guest {
                state: "creating".into(),
                ..guest()
            },
            &limits
        )
        .contains("must be active")
    );
    assert!(
        refuse(
            Guest {
                placement: vec!["mox1".into()],
                ..guest()
            },
            &limits
        )
        .contains("fewer than two")
    );
    assert!(
        refuse(
            Guest {
                primary_domain: "bad domain".into(),
                ..guest()
            },
            &limits
        )
        .contains("unusable domain")
    );
    assert!(
        refuse(guest(), &BTreeMap::from([("mox1".to_string(), 2)]))
            .contains("mox3's staging limit")
    );
}

#[test]
fn a_matching_current_certificate_covering_every_name_is_accepted() {
    let plan = plan();
    let names = plan.server_names();
    let (cert, key) = certificate(
        &[
            "example.com",
            "*.example.com",
            "example.org",
            "a.b.example.com",
        ],
        false,
    );
    check_certificate(cert.as_bytes(), key.as_bytes(), &names, NOW).unwrap();

    let (_, other_key) = certificate(&["example.com"], false);
    assert!(
        check_certificate(cert.as_bytes(), other_key.as_bytes(), &names, NOW)
            .unwrap_err()
            .contains("don't match")
    );
    let (narrow, narrow_key) =
        certificate(&["example.com", "*.example.com", "a.b.example.com"], false);
    assert_eq!(
        check_certificate(narrow.as_bytes(), narrow_key.as_bytes(), &names, NOW).unwrap_err(),
        "The certificate does not cover example.org."
    );
    let (old, old_key) = certificate(
        &[
            "example.com",
            "*.example.com",
            "example.org",
            "a.b.example.com",
        ],
        true,
    );
    assert!(
        check_certificate(old.as_bytes(), old_key.as_bytes(), &names, NOW)
            .unwrap_err()
            .contains("expired")
    );
    assert!(
        check_certificate(b"not a pem", key.as_bytes(), &names, NOW)
            .unwrap_err()
            .contains("PEM X.509")
    );
    assert!(
        check_certificate(cert.as_bytes(), b"nope", &names, NOW)
            .unwrap_err()
            .contains("private key")
    );
}

fn tool(name: &str) -> bool {
    std::process::Command::new(name)
        .arg("--version")
        .output()
        .is_ok()
}

#[test]
fn the_guest_checks_the_sums_before_running_anything() {
    if !(tool("tar") && tool("sha256sum") && tool("sh")) {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("ran");
    let script = format!(
        "#!/bin/bash\nset -e\nprintf '%s|%s\\n' \"$(basename \"$1\")\" \"$2\" > '{}'\nls \"$1\" >> '{}'\n",
        marker.display(),
        marker.display()
    );
    let files: [(&str, &[u8], u32); 2] = [
        ("cloudflare-origin.key", b"KEY", 0o600),
        ("remote-install.sh", script.as_bytes(), 0o700),
    ];
    let packed = pack(&files, 0);
    let command = remote_command("example.com").unwrap();
    let run = |payload: &[u8]| {
        use std::io::Write;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .env("TMPDIR", temp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(payload).unwrap();
        child.wait_with_output().unwrap()
    };
    let ok = run(&packed);
    assert!(
        ok.status.success(),
        "{}",
        String::from_utf8_lossy(&ok.stderr)
    );
    let ran = std::fs::read_to_string(&marker).unwrap();
    assert!(ran.starts_with("hello-app-deploy."), "{ran}");
    assert!(ran.contains("|example.com\n") && ran.contains("cloudflare-origin.key"));

    std::fs::remove_file(&marker).unwrap();
    let mut tampered = packed.clone();
    let at = tampered.windows(3).position(|w| w == b"KEY").unwrap();
    tampered[at] = b'J';
    let bad = run(&tampered);
    assert!(!bad.status.success());
    assert!(!marker.exists(), "the install script must not run");
}

#[test]
fn the_key_travels_only_on_stdin() {
    let (cert, key) = certificate(&["example.com"], false);
    let command = remote_command("example.com").unwrap();
    assert!(!command.contains("PRIVATE KEY"));
    let bundle = bundle(&plan(), cert.as_bytes(), key.as_bytes(), 0);
    assert!(bundle.windows(key.len()).any(|w| w == key.as_bytes()));
    assert!(remote_command("example.com; rm -rf /").is_err());
    assert!(remote_command("'quoted'.com").is_err());
}

#[test]
fn the_install_script_parses() {
    if !tool("bash") {
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("remote-install.sh");
    std::fs::write(&path, REMOTE_INSTALL).unwrap();
    let status = std::process::Command::new("bash")
        .arg("-n")
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
}
