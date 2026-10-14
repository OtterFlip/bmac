// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! The cluster-wide backup settings (`BACKUP.md`, "Backup policies").

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupSettings {
    /// Told to appliances in every check-in reply.
    pub checkin_interval_s: i64,
    /// An appliance is LATE after this long without a check-in.
    pub late_after_s: i64,
    /// An appliance checked in this recently counts as available for a
    /// capture.
    pub ready_window_s: i64,
    /// How long a new PIT's snapshot waits for an available appliance to
    /// start pulling.
    pub claim_window_s: i64,
    /// A transfer with no bytes for this long has stalled.
    pub stalled_after_s: i64,
    /// Also how long checkpoint bookmarks are kept.
    pub delinquency_s: i64,
    pub streams_per_appliance: u32,
    pub min_healthy_appliances: u32,
    /// For tests only: local retention's days last this many seconds
    /// instead of the cluster's calendar days.
    pub test_day_s: Option<i64>,
}

impl BackupSettings {
    /// The calendar local retention runs on, in the cluster's `zone`.
    pub fn calendar(&self, zone: jiff::tz::TimeZone) -> crate::retention::Calendar {
        crate::retention::Calendar::new(zone, self.test_day_s)
    }
}

impl Default for BackupSettings {
    fn default() -> BackupSettings {
        BackupSettings {
            checkin_interval_s: 60,
            late_after_s: 3 * 60,
            ready_window_s: 5 * 60,
            claim_window_s: 5 * 60,
            stalled_after_s: 10 * 60,
            delinquency_s: 72 * 3600,
            streams_per_appliance: 1,
            min_healthy_appliances: 1,
            test_day_s: None,
        }
    }
}

impl BackupSettings {
    pub fn validate(&self) -> Result<(), String> {
        let positive = [
            ("checkin_interval_s", self.checkin_interval_s),
            ("late_after_s", self.late_after_s),
            ("ready_window_s", self.ready_window_s),
            ("claim_window_s", self.claim_window_s),
            ("stalled_after_s", self.stalled_after_s),
            ("delinquency_s", self.delinquency_s),
        ];
        for (name, value) in positive {
            if value <= 0 {
                return Err(format!("{name} must be positive"));
            }
        }
        if self.late_after_s <= self.checkin_interval_s {
            return Err(
                "an appliance must be allowed more than one check-in interval before it is late"
                    .into(),
            );
        }
        if self.delinquency_s <= self.late_after_s {
            return Err("the delinquency period must be longer than the late threshold".into());
        }
        if self.test_day_s.is_some_and(|s| s <= 0) {
            return Err("test_day_s must be positive".into());
        }
        if self.streams_per_appliance == 0 {
            return Err("an appliance needs at least one stream".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_follow_backup_md_and_validate() {
        let s = BackupSettings::default();
        assert_eq!(
            (s.late_after_s, s.ready_window_s, s.delinquency_s),
            (180, 300, 259_200)
        );
        s.validate().unwrap();
        let parsed: BackupSettings = serde_json::from_str(r#"{"claim_window_s": 600}"#).unwrap();
        assert_eq!(parsed.claim_window_s, 600);
        assert_eq!(parsed.late_after_s, 180);
        let bad = BackupSettings {
            late_after_s: 30,
            ..s.clone()
        };
        assert!(bad.validate().is_err());
        let bad = BackupSettings {
            delinquency_s: 60,
            ..s
        };
        assert!(bad.validate().is_err());
    }
}
