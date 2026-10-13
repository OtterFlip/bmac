// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Canonical JSON and secret rejection, byte-for-byte the same as the legacy
//! `canonical_json` and `reject_secret_material` (`cluster_registry.py`
//! 231–238, 279–283, 303–315).

use std::sync::LazyLock;

use regex::Regex;
use serde_json::Value;

use crate::{PmxcfsError, Result};

static SECRET_KEY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(?:^|_)(?:password|passwd|secret|token|credential|private_key|auth_key)(?:$|_)",
    )
    .unwrap()
});
static SECRET_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:-----BEGIN [A-Z ]*PRIVATE KEY-----|tskey-(?:auth|client)-)").unwrap()
});

/// Refuses secret-shaped keys and unmistakable secret values anywhere in
/// `value`. `context` names the record in the message.
pub fn reject_secret_material(value: &Value, context: &str) -> Result<()> {
    match value {
        Value::Object(map) => {
            for (key, nested) in map {
                if SECRET_KEY.is_match(key) {
                    return Err(PmxcfsError::invalid(format!(
                        "{context} may not contain secret field {key:?}"
                    )));
                }
                reject_secret_material(nested, context)?;
            }
        }
        Value::Array(items) => items
            .iter()
            .try_for_each(|v| reject_secret_material(v, context))?,
        Value::String(text) if SECRET_VALUE.is_match(text) => {
            return Err(PmxcfsError::invalid(format!(
                "{context} contains secret material"
            )));
        }
        _ => {}
    }
    Ok(())
}

/// Python's `json.dumps(value, sort_keys=True, separators=(",", ":"),
/// ensure_ascii=True) + "\n"`.
pub fn canonical_json(value: &Value) -> Vec<u8> {
    let mut out = String::new();
    write_value(&mut out, value);
    out.push('\n');
    out.into_bytes()
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => {
            if let Some(i) = number.as_i64() {
                out.push_str(&i.to_string());
            } else if let Some(u) = number.as_u64() {
                out.push_str(&u.to_string());
            } else {
                out.push_str(&python_float(number.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(text) => write_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, item);
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Python sorts by code point; UTF-8 byte order is the same.
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, key);
                out.push(':');
                write_value(out, &map[key]);
            }
            out.push('}');
        }
    }
}

fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ' '..='~' => out.push(c),
            _ => {
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// Python's `repr(float)`: the shortest round-trip digits, in fixed notation
/// for decimal exponents from -4 to 15 and scientific (`1e+16`, `1.5e-05`)
/// otherwise.
fn python_float(value: f64) -> String {
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0".into()
        } else {
            "0.0".into()
        };
    }
    // Rust's `{:e}` gives the shortest round-trip digits: "-1.2345e-7".
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted.split_once('e').unwrap_or((&formatted, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let sign = if negative { "-" } else { "" };
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        let body = if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        };
        format!("{sign}{body}")
    } else {
        let fraction = if digits.len() > 1 {
            format!(".{}", &digits[1..])
        } else {
            String::new()
        };
        let exponent_sign = if exponent < 0 { '-' } else { '+' };
        format!(
            "{sign}{}{fraction}e{exponent_sign}{:02}",
            &digits[..1],
            exponent.abs()
        )
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn matches_python_canonical_form() {
        let value = json!({"b": 1, "a": "é😀\u{2028}\"\\\n\t\u{1}", "c": [1.5, 1e16, true, null, -3, 18446744073709551615u64]});
        let expected = "{\"a\":\"\\u00e9\\ud83d\\ude00\\u2028\\\"\\\\\\n\\t\\u0001\",\"b\":1,\"c\":[1.5,1e+16,true,null,-3,18446744073709551615]}\n";
        assert_eq!(String::from_utf8(canonical_json(&value)).unwrap(), expected);
    }

    #[test]
    fn floats_follow_python_repr() {
        let cases = [
            (0.1, "0.1"),
            (1.0, "1.0"),
            (100.0, "100.0"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e-5, "1.5e-05"),
            (0.0001, "0.0001"),
            (-2.5e-7, "-2.5e-07"),
            (123.456, "123.456"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (-0.0, "-0.0"),
        ];
        for (value, expected) in cases {
            assert_eq!(python_float(value), expected, "{value}");
        }
    }

    #[test]
    fn rejects_secret_keys_and_values() {
        for key in [
            "password",
            "root_password",
            "API_TOKEN",
            "tailscale_auth_key",
            "private_key",
            "secret_x",
        ] {
            let value = json!({"outer": [{key: "x"}]});
            assert!(reject_secret_material(&value, "record").is_err(), "{key}");
        }
        for key in [
            "tokens",
            "passwordless",
            "keys",
            "ssh_public_key",
            "secretary",
        ] {
            assert!(
                reject_secret_material(&json!({key: "x"}), "record").is_ok(),
                "{key}"
            );
        }
        for text in [
            "-----BEGIN OPENSSH PRIVATE KEY-----",
            "x tskey-auth-abc",
            "TSKEY-CLIENT-1",
        ] {
            let error =
                reject_secret_material(&json!({"note": [text]}), "registry data").unwrap_err();
            assert_eq!(error.to_string(), "registry data contains secret material");
        }
        assert!(
            reject_secret_material(&json!({"note": "-----BEGIN PUBLIC KEY-----"}), "r").is_ok()
        );
    }
}
