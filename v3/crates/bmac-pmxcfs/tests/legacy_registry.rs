// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The Rust lock and writer against the Python `AllocatorLock` and
//! `atomic_write_json` of the legacy `cluster_registry.py` (copied from v2),
//! so `bmacd` and the script operations still using it exclude each other.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use bmac_pmxcfs::{
    LockOptions, Pmxcfs, PmxcfsError, RealFs, RegistryLock, canonical_json, read_record,
    write_record,
};
use serde_json::{Value, json};

fn legacy_registry() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../legacy/scripts/host_runtime/cluster_registry.py")
}

const PRELUDE: &str = r#"
import importlib.util, json, sys
spec = importlib.util.spec_from_file_location("cluster_registry", sys.argv[1])
r = importlib.util.module_from_spec(spec)
sys.modules["cluster_registry"] = r
spec.loader.exec_module(r)
"#;

struct Python {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Python {
    fn start(body: &str, args: &[&str]) -> Python {
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(format!("{PRELUDE}{body}"))
            .arg(legacy_registry())
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("python3 runs");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Python {
            child,
            stdin,
            stdout,
        }
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.stdout.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }

    fn send(&mut self, text: &str) {
        writeln!(self.stdin, "{text}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn finish(mut self) {
        drop(self.stdin);
        assert!(self.child.wait().unwrap().success(), "python helper failed");
    }
}

fn local(dir: &Path) -> Pmxcfs {
    Pmxcfs::new(dir.join("not-pmxcfs"), Arc::new(RealFs))
}

fn hostname() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

#[test]
fn python_and_rust_writers_share_one_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let counter = state.join("counter.json");
    let rounds = 40;
    let python = Python::start(
        r#"
from pathlib import Path
state = Path(sys.argv[2]); counter = state / "counter.json"
for _ in range(int(sys.argv[3])):
    with r.AllocatorLock(state, 60, 900, False):
        current = r.read_json_file(counter, "counter") if counter.exists() else {"count": 0, "revision": 0}
        r.atomic_write_json(counter, {"schema_version": 1, "count": current["count"] + 1, "revision": current["revision"] + 1})
"#,
        &[state.to_str().unwrap(), &rounds.to_string()],
    );
    let pmxcfs = local(temporary.path());
    let options = LockOptions {
        timeout: Duration::from_secs(60),
        ..LockOptions::default()
    };
    for _ in 0..rounds {
        let lock = RegistryLock::acquire(&pmxcfs, &state, &options).unwrap();
        let current = read_record(&pmxcfs, &counter, "counter", 1).unwrap();
        let (count, revision) = match &current {
            Some(v) => (v["count"].as_u64().unwrap(), v["revision"].as_u64()),
            None => (0, None),
        };
        write_record(
            &lock,
            &counter,
            json!({"schema_version": 1, "count": count + 1}),
            revision,
        )
        .unwrap();
        lock.release().unwrap();
    }
    python.finish();
    let last = read_record(&pmxcfs, &counter, "counter", 1)
        .unwrap()
        .unwrap();
    assert_eq!(last["count"], 2 * rounds);
    assert_eq!(last["revision"], 2 * rounds);
}

#[test]
fn each_side_sees_the_other_holding_the_lock() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let pmxcfs = local(temporary.path());
    let no_wait = LockOptions {
        timeout: Duration::ZERO,
        ..LockOptions::default()
    };

    let mut python = Python::start(
        r#"
from pathlib import Path
state = Path(sys.argv[2])
with r.AllocatorLock(state, 0, 900, False):
    print("held", flush=True)
    sys.stdin.readline()
print("released", flush=True)
sys.stdin.readline()
try:
    with r.AllocatorLock(state, 0, 900, False):
        print("acquired", flush=True)
except r.RegistryError as exc:
    print(exc, flush=True)
"#,
        &[state.to_str().unwrap()],
    );
    assert_eq!(python.line(), "held");
    let error = RegistryLock::acquire(&pmxcfs, &state, &no_wait)
        .err()
        .unwrap();
    assert!(matches!(error, PmxcfsError::LockTimeout { .. }));
    let owner = format!("owner {} pid={} since=", hostname(), python.child.id());
    assert!(error.to_string().contains(&owner), "{error}");

    python.send("");
    assert_eq!(python.line(), "released");
    let lock = RegistryLock::acquire(&pmxcfs, &state, &no_wait).unwrap();
    python.send("");
    let seen = python.line();
    let owner = format!(
        "timed out waiting for allocator lock (owner {} pid={} since=",
        hostname(),
        std::process::id()
    );
    assert!(seen.starts_with(&owner), "{seen}");
    lock.release().unwrap();
    python.finish();
}

#[test]
fn canonical_json_matches_python_byte_for_byte() {
    let temporary = tempfile::tempdir().unwrap();
    let value: Value = json!({
        "z": [1, -0.0, 0.1, 1e16, 1.5e-7, 123456789012345678_u64, -42, true, null],
        "a": {"é": "naïve \u{1F600} \u{7f} \t\n\"\\ /", "": ""},
        "M": 3.0,
        "ASCII": "plain",
    });
    let path = temporary.path().join("written.json");
    let python = Python::start(
        r#"
from pathlib import Path
r.atomic_write_json(Path(sys.argv[2]), json.loads(sys.stdin.read()))
"#,
        &[path.to_str().unwrap()],
    );
    let mut python = python;
    write!(python.stdin, "{value}").unwrap();
    python.finish();
    let from_python = std::fs::read(&path).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&canonical_json(&value)),
        String::from_utf8_lossy(&from_python)
    );
}
