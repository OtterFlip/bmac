// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Checks shared by every record, ported with v2's wording
//! (`cluster_registry.py` 231–502).

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::sync::LazyLock;

use bmac_core::HostSlot;
use regex::Regex;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::policy::Policy;
use crate::{RegistryError, Result};

fn re(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static pattern")
}

pub(crate) static PURPOSE_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[a-z][a-z0-9]*(?:-[a-z0-9]+)*$"));
pub(crate) static MOX_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^mox([1-9]|[12][0-9]|3[0-2])$"));
pub(crate) static RESOURCE_RE: LazyLock<Regex> = LazyLock::new(|| {
    re(
        r"^(?:prod(?P<production>[1-9][0-9]*)|stage(?P<stage>[1-9][0-9]*)(?P<source>prod[1-9][0-9]*))$",
    )
});
pub(crate) static SAFE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z0-9][A-Za-z0-9_.:/@+-]{0,255}$"));
pub(crate) static SHA256_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9a-f]{64}$"));
pub(crate) static SNAPSHOT_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z][A-Za-z0-9._-]{0,39}$"));
pub(crate) static GUID_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9]{1,20}$"));
pub(crate) static MAC_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^(?:[0-9a-f]{2}:){5}[0-9a-f]{2}$"));
pub(crate) static DOMAIN_LABEL_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$"));
pub(crate) static NONCE_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9a-f]{32}$"));
pub(crate) static VM_TARGET_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^vm:([1-9][0-9]*)$"));
pub(crate) static VOLUME_TARGET_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^[A-Za-z0-9][A-Za-z0-9_.+-]{0,63}:vm-([1-9][0-9]*)-disk-([0-9]+)$"));
pub(crate) static SNAPSHOT_TARGET_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^vm-([1-9][0-9]*)@([A-Za-z][A-Za-z0-9._-]{0,39})$"));
pub(crate) static REPLICATION_TARGET_RE: LazyLock<Regex> =
    LazyLock::new(|| re(r"^([1-9][0-9]*)-([0-9]+)$"));

pub(crate) fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(RegistryError::Invalid(message.into()))
}

/// Python's `repr` of a JSON value, for messages that quote it.
pub(crate) fn repr(value: &Value) -> String {
    match value {
        Value::String(text) => {
            let quote = if text.contains('\'') && !text.contains('"') {
                '"'
            } else {
                '\''
            };
            let mut out = String::from(quote);
            for c in text.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    '\t' => out.push_str("\\t"),
                    c if c == quote => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out.push(quote);
            out
        }
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}

/// Python's `str()` of a value, used where v2 matches a regex on `str(x)`.
pub(crate) fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => repr(other),
    }
}

/// `require_exact_keys`: unknown fields first, then missing ones.
pub(crate) fn exact_keys(value: &Value, expected: &[&str], context: &str) -> Result<()> {
    let Some(map) = value.as_object() else {
        return fail(format!("{context} must be an object"));
    };
    let expected: BTreeSet<&str> = expected.iter().copied().collect();
    let unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !expected.contains(k))
        .collect();
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|k| !map.contains_key(*k))
        .collect();
    if !unknown.is_empty() {
        return fail(format!(
            "{context} has unknown fields: {}",
            unknown.join(", ")
        ));
    }
    if !missing.is_empty() {
        return fail(format!(
            "{context} is missing fields: {}",
            missing.join(", ")
        ));
    }
    Ok(())
}

/// An integer that isn't a boolean, as v2's `isinstance(x, int)` checks
/// mean after excluding `bool`.
pub(crate) fn int(value: &Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
}

pub(crate) fn str_of(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

pub(crate) fn positive_revision(value: &Value, message: &str) -> Result<()> {
    match int(value) {
        Some(n) if n >= 1 => Ok(()),
        _ => fail(message),
    }
}

pub(crate) fn safe_id(value: &Value, description: &str) -> Result<()> {
    match value.as_str() {
        Some(text) if SAFE_ID_RE.is_match(text) => Ok(()),
        _ => fail(format!("{description} contains unsafe characters")),
    }
}

pub(crate) fn sha256(value: &Value, description: &str) -> Result<()> {
    match value.as_str() {
        Some(text) if SHA256_RE.is_match(text) => Ok(()),
        _ => fail(format!("{description} must be a lowercase SHA-256")),
    }
}

pub(crate) fn is_uuid(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|text| uuid::Uuid::parse_str(text).is_ok())
}

/// `validate_nodes`: `moxN` names within the policy's host count, no
/// duplicates.
pub(crate) fn nodes(
    values: &[Value],
    policy: &Policy,
    allow_empty: bool,
    exactly_one: bool,
) -> Result<Vec<HostSlot>> {
    if values.is_empty() && !allow_empty {
        return fail("at least one placement node is required");
    }
    if exactly_one && values.len() != 1 {
        return fail("exactly one placement node is required");
    }
    let mut seen = Vec::new();
    for value in values {
        let Some(text) = value.as_str() else {
            return fail("node names must be strings");
        };
        let Ok(slot) = text.parse::<HostSlot>() else {
            return fail(format!(
                "node must be named mox1 through mox32: {}",
                repr(value)
            ));
        };
        let max = policy.limits.max_hosts;
        if slot.number() > max {
            return fail(format!("{text} exceeds policy max_hosts={max}"));
        }
        if seen.contains(&slot) {
            return fail(format!("duplicate placement node: {text}"));
        }
        seen.push(slot);
    }
    Ok(seen)
}

pub(crate) fn node(value: &Value, policy: &Policy) -> Result<HostSlot> {
    Ok(nodes(std::slice::from_ref(value), policy, false, false)?[0])
}

pub(crate) fn node_list(
    value: &Value,
    policy: &Policy,
    allow_empty: bool,
    what: &str,
) -> Result<Vec<HostSlot>> {
    match value.as_array() {
        Some(values) => nodes(values, policy, allow_empty, false),
        None => fail(format!("{what} must be a JSON array")),
    }
}

/// `normalize_domain` for ASCII input. Records hold normalized names, so a
/// non-ASCII name is reported as invalid rather than converted with IDNA.
pub(crate) fn normalize_domain(original: &str) -> Result<String> {
    let mut domain = original.trim().trim_end_matches('.').to_lowercase();
    if domain.is_empty() || domain.starts_with("*.") {
        return fail(format!(
            "domain must be an exact hostname, not a wildcard: {}",
            repr(&original.into())
        ));
    }
    if domain.contains("://") || domain.contains('/') || domain.contains(':') {
        return fail(format!(
            "domain is not a bare hostname: {}",
            repr(&original.into())
        ));
    }
    if !domain.is_ascii() {
        return fail(format!(
            "domain cannot be converted to IDNA: {}",
            repr(&original.into())
        ));
    }
    if domain.len() > 253 || !domain.contains('.') {
        return fail(format!(
            "domain must be a fully-qualified hostname: {}",
            repr(&original.into())
        ));
    }
    if domain
        .split('.')
        .any(|label| !DOMAIN_LABEL_RE.is_match(label))
    {
        return fail(format!(
            "domain contains an invalid label: {}",
            repr(&original.into())
        ));
    }
    if domain.parse::<IpAddr>().is_ok() {
        return fail(format!(
            "domain may not be an IP address: {}",
            repr(&original.into())
        ));
    }
    domain.shrink_to_fit();
    Ok(domain)
}

pub(crate) fn snapshot_name(value: &str) -> Result<()> {
    if !SNAPSHOT_RE.is_match(value) || value.starts_with("__replicate_") {
        return fail("snapshot name is unsafe or uses Proxmox's reserved __replicate_ prefix");
    }
    Ok(())
}

/// Seconds and nanoseconds since the epoch, comparable across offsets.
pub(crate) type Instant = (i64, u32);

/// `parse_timestamp`: Python's `datetime.fromisoformat` for the forms v2
/// writes and reads, and a timezone is required.
pub(crate) fn timestamp(value: &Value) -> Result<Instant> {
    let invalid = || RegistryError::Invalid(format!("invalid registry timestamp: {}", repr(value)));
    let text = value.as_str().ok_or_else(invalid)?;
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = text.get(range)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(invalid());
    }
    let (year, month, day) = (
        digits(0..4).ok_or_else(invalid)?,
        digits(5..7).ok_or_else(invalid)?,
        digits(8..10).ok_or_else(invalid)?,
    );
    if !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month) || year < 1 {
        return Err(invalid());
    }
    let mut seconds = days_from_civil(year, month, day) * 86_400;
    let mut nanos = 0u32;
    let mut rest = &text[10..];
    if rest.is_empty() {
        return Err(RegistryError::Invalid(format!(
            "registry timestamp lacks timezone: {}",
            repr(value)
        )));
    }
    if !rest.starts_with(['T', ' ']) || rest.len() < 6 {
        return Err(invalid());
    }
    let time = &rest[1..];
    let field = |s: &str, at: usize| -> Option<i64> {
        let part = s.get(at..at + 2)?;
        if part.bytes().all(|b| b.is_ascii_digit()) {
            part.parse().ok()
        } else {
            None
        }
    };
    let hour = field(time, 0).ok_or_else(invalid)?;
    if time.as_bytes().get(2) != Some(&b':') {
        return Err(invalid());
    }
    let minute = field(time, 3).ok_or_else(invalid)?;
    let mut used = 5;
    let mut second = 0;
    if time.as_bytes().get(5) == Some(&b':') {
        second = field(time, 6).ok_or_else(invalid)?;
        used = 8;
        if time
            .as_bytes()
            .get(8)
            .is_some_and(|b| *b == b'.' || *b == b',')
        {
            let fraction: String = time[9..].chars().take_while(char::is_ascii_digit).collect();
            if fraction.is_empty() || fraction.len() > 9 {
                return Err(invalid());
            }
            nanos = format!("{fraction:0<9}").parse().map_err(|_| invalid())?;
            used = 9 + fraction.len();
        }
    }
    if hour > 23 || minute > 59 || second > 59 {
        return Err(invalid());
    }
    seconds += hour * 3600 + minute * 60 + second;
    rest = &time[used..];
    let offset = match rest {
        "" => {
            return Err(RegistryError::Invalid(format!(
                "registry timestamp lacks timezone: {}",
                repr(value)
            )));
        }
        "Z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return Err(invalid()),
            };
            let off = &rest[1..];
            let hours = field(off, 0).ok_or_else(invalid)?;
            let minutes = match off.len() {
                2 => 0,
                5 if off.as_bytes()[2] == b':' => field(off, 3).ok_or_else(invalid)?,
                4 => field(off, 2).ok_or_else(invalid)?,
                _ => return Err(invalid()),
            };
            if hours > 23 || minutes > 59 {
                return Err(invalid());
            }
            sign * (hours * 3600 + minutes * 60)
        }
    };
    seconds -= offset;
    Ok((seconds, nanos))
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Converts a record that passed its checks into its typed form.
pub(crate) fn typed<T: DeserializeOwned>(value: &Value, context: &str) -> Result<T> {
    serde_json::from_value(value.clone())
        .map_err(|e| RegistryError::Invalid(format!("{context} is malformed: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn host_names_run_from_mox1_to_mox32() {
        for name in ["mox1", "mox10", "mox19", "mox20", "mox32"] {
            assert!(MOX_RE.is_match(name), "{name}");
        }
        for name in ["mox0", "mox01", "mox33", "mox40", "mox100", "moxa"] {
            assert!(!MOX_RE.is_match(name), "{name}");
        }
    }

    #[test]
    fn timestamps_follow_fromisoformat() {
        assert_eq!(
            timestamp(&json!("1970-01-01T00:00:00+00:00")).unwrap(),
            (0, 0)
        );
        assert_eq!(
            timestamp(&json!("2026-10-11T13:44:05+00:00")).unwrap(),
            (1_791_726_245, 0)
        );
        assert_eq!(
            timestamp(&json!("2026-10-11T15:44:05+02:00")).unwrap(),
            (1_791_726_245, 0)
        );
        assert_eq!(
            timestamp(&json!("2026-10-11 13:44:05.5Z")).unwrap(),
            (1_791_726_245, 500_000_000)
        );
        assert_eq!(
            timestamp(&json!("2000-02-29T00:00-0130")).unwrap(),
            (951_782_400 + 5400, 0)
        );
        let lacks = timestamp(&json!("2026-10-11T13:44:05"))
            .unwrap_err()
            .to_string();
        assert_eq!(
            lacks,
            "registry timestamp lacks timezone: '2026-10-11T13:44:05'"
        );
        for bad in [
            json!("2026-02-30T00:00:00Z"),
            json!("yesterday"),
            json!(5),
            json!("2026-10-11T24:00:00Z"),
        ] {
            assert!(
                timestamp(&bad)
                    .unwrap_err()
                    .to_string()
                    .starts_with("invalid registry timestamp: ")
            );
        }
    }

    #[test]
    fn domains_normalize_like_v2() {
        assert_eq!(
            normalize_domain(" App.Example.COM. ").unwrap(),
            "app.example.com"
        );
        assert_eq!(
            normalize_domain("xn--bcher-kva.example").unwrap(),
            "xn--bcher-kva.example"
        );
        for (bad, why) in [
            ("*.example.com", "wildcard"),
            ("https://example.com", "not a bare hostname"),
            ("localhost", "fully-qualified"),
            ("a_b.example", "invalid label"),
            ("10.0.0.1", "IP address"),
            ("bücher.example", "IDNA"),
        ] {
            assert!(
                normalize_domain(bad).unwrap_err().to_string().contains(why),
                "{bad}"
            );
        }
    }

    #[test]
    fn repr_matches_python() {
        assert_eq!(repr(&json!("a'b")), "\"a'b\"");
        assert_eq!(repr(&json!("x")), "'x'");
        assert_eq!(repr(&json!(null)), "None");
        assert_eq!(repr(&json!(true)), "True");
    }
}
