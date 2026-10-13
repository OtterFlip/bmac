// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Disk identity: WWN when the disk reports a real one, otherwise serial
//! number (`STORAGE.md`, "Disks").

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::InvalidValue;

/// How a disk is recognized across reboots, controllers, and device-name
/// changes. Written `wwn:<value>` or `serial:<value>`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum DiskIdentity {
    /// Lowercase, as the kernel reports it (`0x5000c500a1b2c3d4`,
    /// `eui.0025388b91c1e2f3`, `naa.5000c500a1b2c3d4`).
    Wwn(String),
    Serial(String),
}

const MAX_LEN: usize = 128;

fn clean(text: &str) -> Option<&str> {
    let text = text.trim();
    let printable = text.bytes().all(|b| b.is_ascii_graphic() || b == b' ');
    (!text.is_empty() && text.len() <= MAX_LEN && printable).then_some(text)
}

/// Linux builds `nvme.<vendor>-<serial hex>-<model hex>-<nsid>` when a
/// namespace has no usable identifier. It only restates the serial.
fn is_synthetic_wwn(wwn: &str) -> bool {
    wwn.starts_with("nvme.")
}

impl DiskIdentity {
    /// The identity from what `lsblk` or sysfs reports. A missing, blank, or
    /// synthetic WWN falls back to the serial; `None` when there is neither,
    /// which `STORAGE.md` refuses ("disks with no serial number and no WWN").
    pub fn from_reported(wwn: Option<&str>, serial: Option<&str>) -> Option<DiskIdentity> {
        let wwn = wwn.and_then(clean).map(str::to_ascii_lowercase);
        if let Some(wwn) = wwn.filter(|w| !is_synthetic_wwn(w) && !w.contains(' ')) {
            return Some(DiskIdentity::Wwn(wwn));
        }
        serial
            .and_then(clean)
            .map(|s| DiskIdentity::Serial(s.to_string()))
    }

    pub fn value(&self) -> &str {
        match self {
            DiskIdentity::Wwn(v) | DiskIdentity::Serial(v) => v,
        }
    }
}

impl FromStr for DiskIdentity {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        let invalid = |why| InvalidValue::new("disk identity", text, why);
        if let Some(wwn) = text.strip_prefix("wwn:") {
            match DiskIdentity::from_reported(Some(wwn), None) {
                Some(identity) if identity.value() == wwn => Ok(identity),
                _ => Err(invalid("expected a lowercase WWN without spaces")),
            }
        } else if let Some(serial) = text.strip_prefix("serial:") {
            match clean(serial) {
                Some(s) if s == serial => Ok(DiskIdentity::Serial(s.to_string())),
                _ => Err(invalid(
                    "expected a printable serial number without surrounding spaces",
                )),
            }
        } else {
            Err(invalid("expected wwn:<value> or serial:<value>"))
        }
    }
}

impl fmt::Display for DiskIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiskIdentity::Wwn(v) => write!(f, "wwn:{v}"),
            DiskIdentity::Serial(v) => write!(f, "serial:{v}"),
        }
    }
}

impl TryFrom<String> for DiskIdentity {
    type Error = InvalidValue;
    fn try_from(text: String) -> Result<Self, InvalidValue> {
        text.parse()
    }
}

impl From<DiskIdentity> for String {
    fn from(value: DiskIdentity) -> String {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_a_real_wwn() {
        let identity =
            DiskIdentity::from_reported(Some("0x5000C500A1B2C3D4"), Some("ZA1B2C3D")).unwrap();
        assert_eq!(identity, DiskIdentity::Wwn("0x5000c500a1b2c3d4".into()));
        let nvme =
            DiskIdentity::from_reported(Some("eui.0025388b91c1e2f3"), Some("S4EWNX0R")).unwrap();
        assert_eq!(nvme.to_string(), "wwn:eui.0025388b91c1e2f3");
    }

    #[test]
    fn falls_back_to_serial_for_missing_blank_or_synthetic_wwns() {
        let lab_nvme = "nvme.1b36-4d4f58314e3043414342-51454d55204e564d65204374726c-00000001";
        for wwn in [None, Some(""), Some("   "), Some(lab_nvme)] {
            let identity = DiskIdentity::from_reported(wwn, Some(" MOX1N0CACB ")).unwrap();
            assert_eq!(
                identity,
                DiskIdentity::Serial("MOX1N0CACB".into()),
                "{wwn:?}"
            );
        }
    }

    #[test]
    fn no_wwn_and_no_serial_is_no_identity() {
        assert_eq!(DiskIdentity::from_reported(None, None), None);
        assert_eq!(DiskIdentity::from_reported(Some(""), Some(" ")), None);
        assert_eq!(
            DiskIdentity::from_reported(None, Some("bad\u{7}serial")),
            None
        );
    }

    #[test]
    fn text_form_round_trips_and_validates() {
        for text in [
            "wwn:0x5000c500a1b2c3d4",
            "serial:WD-WX12 34",
            "serial:QM00001",
        ] {
            assert_eq!(text.parse::<DiskIdentity>().unwrap().to_string(), text);
        }
        for text in [
            "",
            "0x5000",
            "wwn:",
            "wwn:0xABC",
            "wwn:nvme.1b36-00",
            "serial:",
            "serial: X",
            "uuid:1",
        ] {
            assert!(text.parse::<DiskIdentity>().is_err(), "{text:?}");
        }
        let json = serde_json::to_string(&DiskIdentity::Serial("X1".into())).unwrap();
        assert_eq!(json, "\"serial:X1\"");
    }
}
