// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Validated identifiers. Each parses from and displays as its text form,
//! and the accepted forms follow v2's validation (`cluster_registry.py`
//! 239–258 and `MAIN_DESIGN.md`, "Names, addresses, VMIDs, and MACs").

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

use crate::InvalidValue;

/// Implements `FromStr`, `TryFrom<String>`, `Display`, `as_str`, and serde as
/// a validated string for a newtype over `String`.
macro_rules! text_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal, $check:expr) => {
        $(#[$meta])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
        #[serde(try_from = "String", into = "String")]
        #[ts(type = "string")]
        pub struct $name(String);

        impl $name {
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl FromStr for $name {
            type Err = InvalidValue;
            fn from_str(text: &str) -> Result<Self, InvalidValue> {
                let check: fn(&str) -> Result<(), &'static str> = $check;
                check(text).map_err(|why| InvalidValue::new($kind, text, why))?;
                Ok($name(text.to_string()))
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidValue;
            fn try_from(text: String) -> Result<Self, InvalidValue> {
                text.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

/// Implements `TryFrom<String>`, `From<T> for String`, and serde as a string
/// for a type that already has `FromStr` and `Display`.
macro_rules! serde_as_text {
    ($name:ident) => {
        impl TryFrom<String> for $name {
            type Error = InvalidValue;
            fn try_from(text: String) -> Result<Self, InvalidValue> {
                text.parse()
            }
        }

        impl From<$name> for String {
            fn from(value: $name) -> String {
                value.to_string()
            }
        }
    };
}

/// A positive decimal number without a leading zero (`^[1-9][0-9]*$`).
fn positive_decimal(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty() && bytes[0] != b'0' && bytes.iter().all(u8::is_ascii_digit)
}

fn lower_hex(text: &str, len: usize) -> Result<(), &'static str> {
    if text.len() != len {
        return Err("wrong length");
    }
    if !text
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("only lowercase hex digits are allowed");
    }
    Ok(())
}

/// A host slot, 1 to 32, written `moxN` (`^mox([1-9]|[12][0-9]|3[0-2])$`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub struct HostSlot(u8);

impl HostSlot {
    pub const MAX: u8 = 32;

    pub fn new(number: u8) -> Result<Self, InvalidValue> {
        if (1..=Self::MAX).contains(&number) {
            Ok(HostSlot(number))
        } else {
            Err(InvalidValue::new(
                "host slot",
                &number.to_string(),
                "must be 1 to 32",
            ))
        }
    }

    pub fn number(self) -> u8 {
        self.0
    }

    pub fn all() -> impl Iterator<Item = HostSlot> {
        (1..=Self::MAX).map(HostSlot)
    }
}

impl FromStr for HostSlot {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        let invalid = || InvalidValue::new("host name", text, "expected mox1 to mox32");
        let digits = text.strip_prefix("mox").ok_or_else(invalid)?;
        if !positive_decimal(digits) || digits.len() > 2 {
            return Err(invalid());
        }
        HostSlot::new(digits.parse().map_err(|_| invalid())?).map_err(|_| invalid())
    }
}

impl fmt::Display for HostSlot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mox{}", self.0)
    }
}

serde_as_text!(HostSlot);

/// A Proxmox VMID (`^[1-9][0-9]*$`, within 32 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "u64", into = "u64")]
#[ts(type = "number")]
pub struct Vmid(u32);

impl Vmid {
    pub fn new(number: u32) -> Result<Self, InvalidValue> {
        if number == 0 {
            return Err(InvalidValue::new("VMID", "0", "must be positive"));
        }
        Ok(Vmid(number))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl FromStr for Vmid {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        if !positive_decimal(text) {
            return Err(InvalidValue::new(
                "VMID",
                text,
                "expected a positive whole number",
            ));
        }
        let number = text
            .parse()
            .map_err(|_| InvalidValue::new("VMID", text, "too large"))?;
        Ok(Vmid(number))
    }
}

impl TryFrom<u64> for Vmid {
    type Error = InvalidValue;
    fn try_from(number: u64) -> Result<Self, InvalidValue> {
        let small = u32::try_from(number)
            .map_err(|_| InvalidValue::new("VMID", &number.to_string(), "too large"))?;
        Vmid::new(small)
    }
}

impl From<Vmid> for u64 {
    fn from(value: Vmid) -> u64 {
        value.0.into()
    }
}

impl fmt::Display for Vmid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A managed guest: production `prodN`, or staging `stageMprodN`, the M-th
/// staging copy of `prodN` (`RESOURCE_RE`, `cluster_registry.py` 241–244).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub enum GuestName {
    Production { number: u32 },
    Staging { number: u32, source: u32 },
}

impl GuestName {
    pub fn is_production(self) -> bool {
        matches!(self, GuestName::Production { .. })
    }

    /// The production guest this one is, or was copied from.
    pub fn production(self) -> GuestName {
        match self {
            GuestName::Production { .. } => self,
            GuestName::Staging { source, .. } => GuestName::Production { number: source },
        }
    }
}

fn parse_prod(text: &str) -> Option<u32> {
    let digits = text.strip_prefix("prod")?;
    if positive_decimal(digits) {
        digits.parse().ok()
    } else {
        None
    }
}

impl FromStr for GuestName {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        let invalid = || InvalidValue::new("guest name", text, "expected prodN or stageMprodN");
        if let Some(number) = parse_prod(text) {
            return Ok(GuestName::Production { number });
        }
        let rest = text.strip_prefix("stage").ok_or_else(invalid)?;
        let split = rest.find("prod").ok_or_else(invalid)?;
        let (digits, source) = rest.split_at(split);
        if !positive_decimal(digits) {
            return Err(invalid());
        }
        let number = digits.parse().map_err(|_| invalid())?;
        let source = parse_prod(source).ok_or_else(invalid)?;
        Ok(GuestName::Staging { number, source })
    }
}

impl fmt::Display for GuestName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GuestName::Production { number } => write!(f, "prod{number}"),
            GuestName::Staging { number, source } => write!(f, "stage{number}prod{source}"),
        }
    }
}

serde_as_text!(GuestName);

/// A ZFS snapshot's GUID, a 64-bit number written in decimal (`GUID_RE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub struct SnapshotGuid(u64);

impl SnapshotGuid {
    pub fn new(value: u64) -> Self {
        SnapshotGuid(value)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl FromStr for SnapshotGuid {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        if text.is_empty() || text.len() > 20 || !text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(InvalidValue::new(
                "snapshot GUID",
                text,
                "expected 1 to 20 decimal digits",
            ));
        }
        let value = text
            .parse()
            .map_err(|_| InvalidValue::new("snapshot GUID", text, "larger than 64 bits"))?;
        Ok(SnapshotGuid(value))
    }
}

impl fmt::Display for SnapshotGuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

serde_as_text!(SnapshotGuid);

/// A job's identifier: a random UUID, lowercase and hyphenated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, TS)]
#[serde(try_from = "String", into = "String")]
#[ts(type = "string")]
pub struct JobId(uuid::Uuid);

impl JobId {
    pub fn random() -> Self {
        JobId(uuid::Uuid::new_v4())
    }
}

impl FromStr for JobId {
    type Err = InvalidValue;
    fn from_str(text: &str) -> Result<Self, InvalidValue> {
        let invalid = || InvalidValue::new("job ID", text, "expected a lowercase hyphenated UUID");
        if text.len() != 36 || text.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(invalid());
        }
        uuid::Uuid::try_parse(text)
            .map(JobId)
            .map_err(|_| invalid())
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.hyphenated())
    }
}

serde_as_text!(JobId);

text_id!(
    /// An operation registry ID: lowercase words of `[a-z][a-z0-9_]*` joined
    /// by dots, at most 64 characters (`storage.add_vdev`, `add_prod_vm`).
    OperationId,
    "operation ID",
    |text| {
        if text.is_empty() || text.len() > 64 {
            return Err("must be 1 to 64 characters");
        }
        for word in text.split('.') {
            let mut bytes = word.bytes();
            if !bytes.next().is_some_and(|b| b.is_ascii_lowercase()) {
                return Err("each dot-separated word must start with a lowercase letter");
            }
            if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
                return Err("only lowercase letters, digits, underscores, and dots are allowed");
            }
        }
        Ok(())
    }
);

text_id!(
    /// A production guest's purpose slug (`PURPOSE_RE`): lowercase words of
    /// letters and digits joined by single hyphens, starting with a letter.
    Purpose,
    "purpose",
    |text| {
        if !text.bytes().next().is_some_and(|b| b.is_ascii_lowercase()) {
            return Err("must start with a lowercase letter");
        }
        if text.split('-').any(|word| word.is_empty() || !word.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())) {
            return Err("only lowercase letters and digits, in words joined by single hyphens");
        }
        Ok(())
    }
);

text_id!(
    /// A safe opaque identifier (`SAFE_ID_RE`): a letter or digit, then up to
    /// 255 of letters, digits, and `_.:/@+-`.
    SafeId,
    "identifier",
    |text| {
        let mut bytes = text.bytes();
        if !bytes.next().is_some_and(|b| b.is_ascii_alphanumeric()) {
            return Err("must start with a letter or digit");
        }
        if text.len() > 256 {
            return Err("longer than 256 characters");
        }
        if !bytes.all(|b| b.is_ascii_alphanumeric() || b"_.:/@+-".contains(&b)) {
            return Err("only letters, digits, and _.:/@+- are allowed");
        }
        Ok(())
    }
);

text_id!(
    /// A SHA-256 digest in lowercase hex (`SHA256_RE`).
    Sha256Hex,
    "SHA-256 digest",
    |text| lower_hex(text, 64)
);

text_id!(
    /// A snapshot name (`SNAPSHOT_RE`): a letter, then up to 39 of letters,
    /// digits, and `._-`.
    SnapshotName,
    "snapshot name",
    |text| {
        let mut bytes = text.bytes();
        if !bytes.next().is_some_and(|b| b.is_ascii_alphabetic()) {
            return Err("must start with a letter");
        }
        if text.len() > 40 {
            return Err("longer than 40 characters");
        }
        if !bytes.all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)) {
            return Err("only letters, digits, and ._- are allowed");
        }
        Ok(())
    }
);

text_id!(
    /// A MAC address in lowercase colon form (`MAC_RE`).
    MacAddress,
    "MAC address",
    |text| {
        let parts: Vec<&str> = text.split(':').collect();
        if parts.len() != 6 || parts.iter().any(|p| lower_hex(p, 2).is_err()) {
            return Err("expected six lowercase hex pairs separated by colons");
        }
        Ok(())
    }
);

text_id!(
    /// One DNS label (`DOMAIN_LABEL_RE`): lowercase letters, digits, and
    /// inner hyphens, 1 to 63 characters.
    DomainLabel,
    "domain label",
    |text| {
        if text.is_empty() || text.len() > 63 {
            return Err("must be 1 to 63 characters");
        }
        if text.starts_with('-') || text.ends_with('-') {
            return Err("must not start or end with a hyphen");
        }
        if !text.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err("only lowercase letters, digits, and hyphens are allowed");
        }
        Ok(())
    }
);

/// ZFS's own limit on a dataset name, including the pool.
const ZFS_NAME_MAX: usize = 255;

fn zfs_component(text: &str) -> Result<(), &'static str> {
    if text.is_empty() {
        return Err("has an empty component");
    }
    if !text
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
    {
        return Err("only letters, digits, and _-.: are allowed");
    }
    if text == "." || text == ".." {
        return Err("a component must not be . or ..");
    }
    Ok(())
}

text_id!(
    /// A ZFS pool name: it starts with a letter, uses letters, digits, and
    /// `_-.:`, and is not one of ZFS's reserved names (`mirror`, `raidz`,
    /// `draid`, `spare`, `log`, or `c` followed by a digit).
    PoolName,
    "pool name",
    |text| {
        zfs_component(text)?;
        if !text.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()) {
            return Err("must start with a letter");
        }
        if text.len() > ZFS_NAME_MAX {
            return Err("longer than 255 characters");
        }
        if ["mirror", "raidz", "draid", "spare"].iter().any(|r| text.starts_with(r)) || text == "log" {
            return Err("is reserved by ZFS");
        }
        let b = text.as_bytes();
        if b[0] == b'c' && b.get(1).is_some_and(u8::is_ascii_digit) {
            return Err("is reserved by ZFS");
        }
        Ok(())
    }
);

text_id!(
    /// A ZFS dataset name: a pool name, then `/`-separated components.
    DatasetName,
    "dataset name",
    |text| {
        if text.len() > ZFS_NAME_MAX {
            return Err("longer than 255 characters");
        }
        let mut parts = text.split('/');
        let pool = parts.next().unwrap_or_default();
        pool.parse::<PoolName>().map_err(|_| "does not start with a valid pool name")?;
        parts.try_for_each(zfs_component)
    }
);

impl DatasetName {
    pub fn pool(&self) -> PoolName {
        PoolName(self.0.split('/').next().unwrap_or_default().to_string())
    }

    pub fn child(&self, component: &str) -> Result<DatasetName, InvalidValue> {
        format!("{}/{component}", self.0).parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepts<T: FromStr + fmt::Display>(cases: &[&str]) {
        for case in cases {
            let value: T = case
                .parse()
                .unwrap_or_else(|_| panic!("{case:?} should parse"));
            assert_eq!(value.to_string(), *case, "{case:?} should round-trip");
        }
    }

    fn rejects<T: FromStr>(cases: &[&str]) {
        for case in cases {
            assert!(case.parse::<T>().is_err(), "{case:?} should be rejected");
        }
    }

    #[test]
    fn host_slots_follow_mox_re() {
        accepts::<HostSlot>(&["mox1", "mox9", "mox10", "mox19", "mox20", "mox32"]);
        rejects::<HostSlot>(&[
            "mox0", "mox33", "mox40", "mox01", "MOX1", "mox", "mox1 ", "moxa", "mox100", "prod1",
        ]);
        assert_eq!(HostSlot::all().count(), 32);
        assert!(HostSlot::new(0).is_err() && HostSlot::new(33).is_err());
    }

    #[test]
    fn vmids_are_positive_decimals() {
        accepts::<Vmid>(&["1", "100", "9111", "4294967295"]);
        rejects::<Vmid>(&["0", "0100", "-1", "1.0", "", "4294967296", " 100"]);
        assert_eq!(
            serde_json::to_string(&Vmid::new(100).unwrap()).unwrap(),
            "100"
        );
        assert!(serde_json::from_str::<Vmid>("0").is_err());
    }

    #[test]
    fn guest_names_follow_resource_re() {
        accepts::<GuestName>(&["prod1", "prod20", "prod123", "stage1prod1", "stage12prod3"]);
        rejects::<GuestName>(&[
            "prod0",
            "prod01",
            "prod",
            "stage1",
            "stage0prod1",
            "stage1prod0",
            "stage01prod1",
            "stageprod1",
            "Prod1",
            "prod1s1",
            "stage1prod1x",
            "haproxy1",
        ]);
        let staging: GuestName = "stage2prod7".parse().unwrap();
        assert_eq!(staging.production().to_string(), "prod7");
        assert!(!staging.is_production());
    }

    #[test]
    fn purposes_follow_purpose_re() {
        accepts::<Purpose>(&["web", "hello-app", "a1-b2", "x"]);
        rejects::<Purpose>(&["", "1web", "Web", "web-", "-web", "we--b", "we_b", "we b"]);
    }

    #[test]
    fn safe_ids_follow_safe_id_re() {
        accepts::<SafeId>(&["a", "Run-1", "user@pam", "a/b:c+d.e_f"]);
        rejects::<SafeId>(&["", "-a", ".a", "a b", "a;b", &"a".repeat(257)]);
        assert!("a".repeat(256).parse::<SafeId>().is_ok());
    }

    #[test]
    fn digests_snapshots_guids_and_macs() {
        accepts::<Sha256Hex>(&[&"ab".repeat(32)]);
        rejects::<Sha256Hex>(&[&"AB".repeat(32), &"ab".repeat(31), &"zz".repeat(32)]);
        accepts::<SnapshotName>(&[
            "a",
            "bmac-pit-1",
            "Snap.1_x",
            &format!("a{}", "b".repeat(39)),
        ]);
        rejects::<SnapshotName>(&["", "1a", "-a", "a b", &format!("a{}", "b".repeat(40))]);
        accepts::<SnapshotGuid>(&["0", "18446744073709551615"]);
        rejects::<SnapshotGuid>(&[
            "",
            "18446744073709551616",
            "123456789012345678901",
            "1a",
            "-1",
        ]);
        accepts::<MacAddress>(&["52:54:00:b3:01:00"]);
        rejects::<MacAddress>(&[
            "52:54:00:B3:01:00",
            "52-54-00-b3-01-00",
            "52:54:00:b3:01",
            "52:54:00:b3:01:0g",
        ]);
    }

    #[test]
    fn domain_labels_follow_domain_label_re() {
        accepts::<DomainLabel>(&["a", "mox1", "my-app", &"a".repeat(63)]);
        rejects::<DomainLabel>(&["", "-a", "a-", "A", "a.b", "a_b", &"a".repeat(64)]);
    }

    #[test]
    fn operation_ids_are_dotted_lowercase_words() {
        accepts::<OperationId>(&[
            "add_prod_vm",
            "storage.add_vdev",
            "prod.change_placement",
            "a1",
        ]);
        rejects::<OperationId>(&[
            "",
            "Add",
            "1a",
            "a..b",
            ".a",
            "a.",
            "a-b",
            "a b",
            &"a".repeat(65),
        ]);
    }

    #[test]
    fn job_ids_are_lowercase_uuids() {
        let id = JobId::random();
        assert_eq!(id.to_string().parse::<JobId>().unwrap(), id);
        rejects::<JobId>(&[
            "",
            "not-a-uuid",
            "6F9619FF-8B86-D011-B42D-00C04FC964FF",
            "6f9619ff8b86d011b42d00c04fc964ff",
        ]);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<JobId>(&json).unwrap(), id);
    }

    #[test]
    fn pool_and_dataset_names_follow_zfs_rules() {
        accepts::<PoolName>(&["rpool", "tank", "bmac-backup", "p.1:x"]);
        rejects::<PoolName>(&[
            "", "1pool", "mirror", "mirror2", "raidz", "draid1", "spare", "log", "c0", "c1d0",
            "a/b", "a b",
        ]);
        assert!("logs".parse::<PoolName>().is_ok());
        accepts::<DatasetName>(&["rpool", "rpool/data", "rpool/data-32k/vm-100-disk-0"]);
        rejects::<DatasetName>(&[
            "",
            "rpool/",
            "rpool//data",
            "/rpool",
            "rpool/a b",
            "rpool/..",
            "1pool/data",
        ]);
        let dataset: DatasetName = "rpool/data".parse().unwrap();
        assert_eq!(dataset.pool().as_str(), "rpool");
        assert_eq!(
            dataset.child("vm-100-disk-0").unwrap().as_str(),
            "rpool/data/vm-100-disk-0"
        );
        assert!(dataset.child("a/../b").is_err());
    }

    #[test]
    fn serde_validates_and_round_trips() {
        let slot: HostSlot = serde_json::from_str("\"mox3\"").unwrap();
        assert_eq!(serde_json::to_string(&slot).unwrap(), "\"mox3\"");
        assert!(serde_json::from_str::<HostSlot>("\"mox33\"").is_err());
        let guest: GuestName = serde_json::from_str("\"stage1prod2\"").unwrap();
        assert_eq!(serde_json::to_string(&guest).unwrap(), "\"stage1prod2\"");
        let error = serde_json::from_str::<PoolName>("\"mirror\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("reserved by ZFS"), "{error}");
    }
}
