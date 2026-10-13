// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The stored pool layout: top-level vdevs, each with a class, a type, and
//! member disks named by their `/dev/disk/by-id/` entry.

use std::fmt;
use std::str::FromStr;

use bmac_core::PoolName;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;
pub const DEFAULT_ASHIFT: u8 = 12;
pub const MIN_ASHIFT: u8 = 9;
pub const MAX_ASHIFT: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error("the layout is not valid JSON for schema version {SCHEMA_VERSION}: {0}")]
    Json(String),
    #[error("layout schema version {0} is not supported; this BMAC reads version {SCHEMA_VERSION}")]
    SchemaVersion(u32),
    #[error("{0:?} is not a /dev/disk/by-id/ name of a whole disk: {1}")]
    ById(String, &'static str),
    #[error("ashift {0} is outside {MIN_ASHIFT} to {MAX_ASHIFT}")]
    Ashift(u8),
    #[error("the pool needs at least one data vdev")]
    NoDataVdev,
    #[error("vdev {vdev}: a {class} vdev cannot be {kind}; it can be {allowed}")]
    TypeNotAllowed {
        vdev: usize,
        class: VdevClass,
        kind: VdevType,
        allowed: String,
    },
    #[error("vdev {vdev}: {message}")]
    Members { vdev: usize, message: String },
    #[error("vdev {vdev}: only a data vdev can be the boot vdev")]
    BootNotData { vdev: usize },
}

/// A disk's name under `/dev/disk/by-id/`, such as `wwn-0x5000c500a1b2c3d4`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ById(String);

impl ById {
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn path(&self) -> String {
        format!("/dev/disk/by-id/{}", self.0)
    }
}

impl FromStr for ById {
    type Err = LayoutError;

    fn from_str(text: &str) -> Result<Self, LayoutError> {
        let bad = |why| Err(LayoutError::ById(text.to_string(), why));
        if text.is_empty() || text.len() > 255 {
            return bad("it must be 1 to 255 characters");
        }
        if text.starts_with('.') || text.starts_with('-') {
            return bad("it must not start with . or -");
        }
        let allowed = |c: char| c.is_ascii_alphanumeric() || "._:+@,=-".contains(c);
        if !text.chars().all(allowed) {
            return bad("it may use only letters, digits, and ._:+@,=-");
        }
        if let Some((_, n)) = text.rsplit_once("-part")
            && !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
        {
            return bad("it names a partition");
        }
        Ok(ById(text.to_string()))
    }
}

impl TryFrom<String> for ById {
    type Error = LayoutError;
    fn try_from(text: String) -> Result<Self, LayoutError> {
        text.parse()
    }
}

impl From<ById> for String {
    fn from(id: ById) -> String {
        id.0
    }
}

impl fmt::Display for ById {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VdevClass {
    Data,
    Special,
    Log,
    Cache,
    Spare,
}

impl VdevClass {
    pub const ALL: [VdevClass; 5] = [
        Self::Data,
        Self::Special,
        Self::Log,
        Self::Cache,
        Self::Spare,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Data => "data",
            Self::Special => "special",
            Self::Log => "log",
            Self::Cache => "cache",
            Self::Spare => "spare",
        }
    }

    /// The vdev types this class may use, in the order the UI offers them.
    pub fn allowed_types(self) -> &'static [VdevType] {
        use VdevType::*;
        match self {
            Self::Data => &[Single, Mirror, Raidz1, Raidz2, Raidz3],
            Self::Special | Self::Log => &[Single, Mirror],
            Self::Cache | Self::Spare => &[Single],
        }
    }

    /// The word `zpool create` and `zpool add` put before this class's vdevs.
    pub fn zpool_keyword(self) -> Option<&'static str> {
        match self {
            Self::Data => None,
            Self::Special => Some("special"),
            Self::Log => Some("log"),
            Self::Cache => Some("cache"),
            Self::Spare => Some("spare"),
        }
    }
}

impl fmt::Display for VdevClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VdevType {
    Single,
    Mirror,
    Raidz1,
    Raidz2,
    Raidz3,
}

impl VdevType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Mirror => "mirror",
            Self::Raidz1 => "raidz1",
            Self::Raidz2 => "raidz2",
            Self::Raidz3 => "raidz3",
        }
    }

    pub fn parity(self) -> Option<u32> {
        match self {
            Self::Raidz1 => Some(1),
            Self::Raidz2 => Some(2),
            Self::Raidz3 => Some(3),
            Self::Single | Self::Mirror => None,
        }
    }

    pub fn is_raidz(self) -> bool {
        self.parity().is_some()
    }

    /// The fewest members this type takes. raidz needs two data disks per
    /// row, as the Proxmox installer also requires.
    pub fn min_members(self) -> usize {
        match self {
            Self::Single => 1,
            Self::Mirror => 2,
            Self::Raidz1 | Self::Raidz2 | Self::Raidz3 => self.parity().unwrap_or(0) as usize + 2,
        }
    }

    pub fn max_members(self) -> Option<usize> {
        (self == Self::Single).then_some(1)
    }

    /// How many member failures a vdev of this type and width survives.
    pub fn failures_tolerated(self, members: usize) -> usize {
        match self {
            Self::Single => 0,
            Self::Mirror => members.saturating_sub(1),
            _ => self.parity().unwrap_or(0) as usize,
        }
    }

    /// The word `zpool create` uses, or `None` for a lone disk.
    pub fn zpool_keyword(self) -> Option<&'static str> {
        match self {
            Self::Single => None,
            other => Some(other.as_str()),
        }
    }
}

impl fmt::Display for VdevType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Encryption {
    #[default]
    None,
    Luks2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Vdev {
    pub class: VdevClass,
    #[serde(rename = "type")]
    pub kind: VdevType,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub boot: bool,
    pub members: Vec<ById>,
}

impl Vdev {
    pub fn new(class: VdevClass, kind: VdevType, members: Vec<ById>) -> Vdev {
        Vdev {
            class,
            kind,
            boot: false,
            members,
        }
    }

    pub fn boot(mut self) -> Vdev {
        self.boot = true;
        self
    }

    pub fn failures_tolerated(&self) -> usize {
        self.kind.failures_tolerated(self.members.len())
    }

    /// A short description for messages: "data raidz2 of 6 disks".
    pub fn describe(&self) -> String {
        let width = self.members.len();
        match self.kind {
            VdevType::Single => format!("single-disk {} vdev", self.class),
            VdevType::Mirror => format!("{}-way {} mirror", width, self.class),
            kind => format!("{} {kind} of {width} disks", self.class),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolLayout {
    pub schema_version: u32,
    pub pool: PoolName,
    pub ashift: u8,
    #[serde(default)]
    pub encryption: Encryption,
    pub vdevs: Vec<Vdev>,
}

impl PoolLayout {
    pub fn new(pool: PoolName, encryption: Encryption, vdevs: Vec<Vdev>) -> PoolLayout {
        PoolLayout {
            schema_version: SCHEMA_VERSION,
            pool,
            ashift: DEFAULT_ASHIFT,
            encryption,
            vdevs,
        }
    }

    /// Reads a stored layout and checks its structure.
    pub fn from_json(text: &str) -> Result<PoolLayout, LayoutError> {
        #[derive(Deserialize)]
        struct Version {
            schema_version: u32,
        }
        let version: Version =
            serde_json::from_str(text).map_err(|e| LayoutError::Json(e.to_string()))?;
        if version.schema_version != SCHEMA_VERSION {
            return Err(LayoutError::SchemaVersion(version.schema_version));
        }
        let layout: PoolLayout =
            serde_json::from_str(text).map_err(|e| LayoutError::Json(e.to_string()))?;
        layout.validate()?;
        Ok(layout)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// The structural rules: allowed types per class, member counts, and
    /// ashift range. Rules that need the disks themselves are in `checks`.
    pub fn validate(&self) -> Result<(), LayoutError> {
        if self.schema_version != SCHEMA_VERSION {
            return Err(LayoutError::SchemaVersion(self.schema_version));
        }
        if !(MIN_ASHIFT..=MAX_ASHIFT).contains(&self.ashift) {
            return Err(LayoutError::Ashift(self.ashift));
        }
        if !self.vdevs.iter().any(|v| v.class == VdevClass::Data) {
            return Err(LayoutError::NoDataVdev);
        }
        for (vdev, v) in self.vdevs.iter().enumerate() {
            let allowed = v.class.allowed_types();
            if !allowed.contains(&v.kind) {
                let allowed = allowed
                    .iter()
                    .map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(LayoutError::TypeNotAllowed {
                    vdev,
                    class: v.class,
                    kind: v.kind,
                    allowed,
                });
            }
            let count = v.members.len();
            if count < v.kind.min_members() {
                let message = match v.kind {
                    VdevType::Single => "a single-disk vdev needs one disk".to_string(),
                    kind => format!("{kind} needs at least {} disks", kind.min_members()),
                };
                return Err(LayoutError::Members { vdev, message });
            }
            if v.kind.max_members().is_some_and(|max| count > max) {
                let message = format!("a single-disk vdev has exactly one disk, not {count}");
                return Err(LayoutError::Members { vdev, message });
            }
            if v.boot && v.class != VdevClass::Data {
                return Err(LayoutError::BootNotData { vdev });
            }
        }
        Ok(())
    }

    pub fn data_vdevs(&self) -> impl Iterator<Item = &Vdev> {
        self.vdevs.iter().filter(|v| v.class == VdevClass::Data)
    }

    pub fn boot_vdevs(&self) -> impl Iterator<Item = (usize, &Vdev)> {
        self.vdevs.iter().enumerate().filter(|(_, v)| v.boot)
    }

    pub fn members(&self) -> impl Iterator<Item = &ById> {
        self.vdevs.iter().flat_map(|v| v.members.iter())
    }

    pub fn has_raidz(&self) -> bool {
        self.vdevs.iter().any(|v| v.kind.is_raidz())
    }

    /// Whether `zpool remove` stays possible: no raidz vdev anywhere, since
    /// BMAC always keeps one ashift per pool.
    pub fn allows_vdev_removal(&self) -> bool {
        !self.has_raidz()
    }

    /// The fewest member failures any data vdev survives.
    pub fn data_redundancy(&self) -> usize {
        self.data_vdevs()
            .map(Vdev::failures_tolerated)
            .min()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"{
      "schema_version": 1,
      "pool": "rpool",
      "ashift": 12,
      "encryption": "luks2",
      "vdevs": [
        { "class": "data", "type": "raidz2", "boot": true,
          "members": ["wwn-0x5000c500a1b2c3d4", "wwn-0x5000c500a1b2c3d5",
                      "wwn-0x5000c500a1b2c3d6", "wwn-0x5000c500a1b2c3d7"] },
        { "class": "special", "type": "mirror",
          "members": ["nvme-eui.0025385b71b0a1c2", "nvme-eui.0025385b71b0a1c3"] },
        { "class": "spare", "type": "single",
          "members": ["wwn-0x5000c500a1b2c3d8"] }
      ]
    }"#;

    #[test]
    fn storage_md_example_round_trips() {
        let layout = PoolLayout::from_json(EXAMPLE).unwrap();
        assert_eq!(layout.encryption, Encryption::Luks2);
        assert_eq!(layout.vdevs[0].kind, VdevType::Raidz2);
        assert!(layout.vdevs[0].boot);
        assert!(!layout.allows_vdev_removal());
        assert_eq!(layout.data_redundancy(), 2);
        let again = PoolLayout::from_json(&layout.to_json()).unwrap();
        assert_eq!(again, layout);
        let value: serde_json::Value = serde_json::from_str(&layout.to_json()).unwrap();
        assert_eq!(value["vdevs"][0]["type"], "raidz2");
        assert!(value["vdevs"][1].get("boot").is_none());
    }

    #[test]
    fn schema_version_and_unknown_fields_are_refused() {
        let future = EXAMPLE.replace("\"schema_version\": 1", "\"schema_version\": 2");
        assert_eq!(
            PoolLayout::from_json(&future),
            Err(LayoutError::SchemaVersion(2))
        );
        let extra = EXAMPLE.replace("\"ashift\": 12", "\"ashift\": 12, \"draid\": true");
        assert!(matches!(
            PoolLayout::from_json(&extra),
            Err(LayoutError::Json(_))
        ));
        let draid = EXAMPLE.replace("\"raidz2\"", "\"draid2\"");
        assert!(matches!(
            PoolLayout::from_json(&draid),
            Err(LayoutError::Json(_))
        ));
    }

    #[test]
    fn by_id_names_must_be_whole_disks() {
        for good in [
            "wwn-0x5000c500a1b2c3d4",
            "nvme-eui.0025385b71b0a1c2",
            "usb-SanDisk_Ultra_123-0:0",
        ] {
            assert!(good.parse::<ById>().is_ok(), "{good}");
        }
        for bad in [
            "",
            "../sda",
            "wwn-0x5000c500a1b2c3d4-part3",
            "nvme a",
            "-x",
            ".hidden",
        ] {
            assert!(bad.parse::<ById>().is_err(), "{bad}");
        }
        let id: ById = "wwn-1".parse().unwrap();
        assert_eq!(id.path(), "/dev/disk/by-id/wwn-1");
    }

    fn ids(n: usize) -> Vec<ById> {
        (0..n)
            .map(|i| format!("wwn-{i}").parse().unwrap())
            .collect()
    }

    fn pool(vdevs: Vec<Vdev>) -> PoolLayout {
        PoolLayout::new("tank".parse().unwrap(), Encryption::None, vdevs)
    }

    #[test]
    fn structure_rules() {
        use VdevClass::*;
        use VdevType::*;
        assert_eq!(
            pool(vec![Vdev::new(Log, Mirror, ids(2))]).validate(),
            Err(LayoutError::NoDataVdev)
        );
        let err = pool(vec![
            Vdev::new(Data, Mirror, ids(2)),
            Vdev::new(Cache, Mirror, ids(2)),
        ])
        .validate();
        assert_eq!(
            err.unwrap_err().to_string(),
            "vdev 1: a cache vdev cannot be mirror; it can be single"
        );
        let err = pool(vec![Vdev::new(Data, Raidz2, ids(3))])
            .validate()
            .unwrap_err();
        assert_eq!(err.to_string(), "vdev 0: raidz2 needs at least 4 disks");
        let err = pool(vec![Vdev::new(Data, Single, ids(2))])
            .validate()
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "vdev 0: a single-disk vdev has exactly one disk, not 2"
        );
        let err = pool(vec![
            Vdev::new(Data, Mirror, ids(2)),
            Vdev::new(Log, Single, ids(1)).boot(),
        ])
        .validate()
        .unwrap_err();
        assert_eq!(err, LayoutError::BootNotData { vdev: 1 });
        let mut layout = pool(vec![Vdev::new(Data, Mirror, ids(2))]);
        layout.ashift = 8;
        assert_eq!(layout.validate(), Err(LayoutError::Ashift(8)));
    }

    #[test]
    fn failures_tolerated_per_type() {
        assert_eq!(VdevType::Single.failures_tolerated(1), 0);
        assert_eq!(VdevType::Mirror.failures_tolerated(3), 2);
        assert_eq!(VdevType::Raidz3.failures_tolerated(9), 3);
        assert_eq!(VdevType::Raidz1.min_members(), 3);
        assert_eq!(VdevType::Raidz3.min_members(), 5);
    }
}
