// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Appliance states as the cluster sees them (`BACKUP.md`, "Check-ins"):
//!
//! ```text
//! PENDING -> SEEDING -> ACTIVE <-> LATE -> DELINQUENT -> REPAIRING -> ACTIVE
//!                                                     \-> RESEED_REQUIRED -> SEEDING
//! any -> PAUSED (operator)      any -> RETIRED (operator; data stays on the appliance)
//! ```
//!
//! A state is worked out from what the cluster recorded, never stored: the
//! last check-in, the last PIT published of those offered, the oldest offer
//! still unanswered, and what the appliance reported about itself. LOCKED
//! (its pool waits for the console passphrase) is reported separately and
//! never counts against it.

use serde::{Deserialize, Serialize};

use crate::settings::BackupSettings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ApplianceState {
    Pending,
    Seeding,
    Active,
    Late,
    Delinquent,
    Repairing,
    ReseedRequired,
    Paused,
    Retired,
}

/// What the appliance said about its own progress in its last check-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Progress {
    #[default]
    Normal,
    Repairing,
    ReseedRequired,
}

/// What the cluster knows of one appliance. Times are epoch seconds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ApplianceFacts {
    pub last_checkin_s: Option<i64>,
    /// The last time it published a PIT offered to it.
    pub last_published_s: Option<i64>,
    /// When the oldest offer it hasn't published yet was made.
    pub oldest_unpublished_offer_s: Option<i64>,
    pub progress: Progress,
    pub locked: bool,
    pub paused: bool,
    pub retired: bool,
}

pub fn state(facts: &ApplianceFacts, now_s: i64, settings: &BackupSettings) -> ApplianceState {
    if facts.retired {
        return ApplianceState::Retired;
    }
    if facts.paused {
        return ApplianceState::Paused;
    }
    let Some(last) = facts.last_checkin_s else {
        return ApplianceState::Pending;
    };
    let silent = now_s - last;
    let behind = facts
        .oldest_unpublished_offer_s
        .is_some_and(|offered| now_s - offered > settings.delinquency_s);
    if silent > settings.delinquency_s || behind {
        return ApplianceState::Delinquent;
    }
    if silent > settings.late_after_s {
        return ApplianceState::Late;
    }
    match facts.progress {
        Progress::Repairing => ApplianceState::Repairing,
        Progress::ReseedRequired => ApplianceState::ReseedRequired,
        Progress::Normal if facts.last_published_s.is_none() => ApplianceState::Seeding,
        Progress::Normal => ApplianceState::Active,
    }
}

/// Whether a capture counts the appliance as there to pull it: it checked
/// in within the ready window, isn't locked, paused, or retired.
pub fn available(facts: &ApplianceFacts, now_s: i64, settings: &BackupSettings) -> bool {
    !facts.locked
        && !facts.paused
        && !facts.retired
        && facts
            .last_checkin_s
            .is_some_and(|last| now_s - last <= settings.ready_window_s)
}

/// Whether the appliance counts toward a VM's minimum of healthy ones.
pub fn healthy(facts: &ApplianceFacts, now_s: i64, settings: &BackupSettings) -> bool {
    !facts.locked && state(facts, now_s, settings) == ApplianceState::Active
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn seen(ago: i64) -> ApplianceFacts {
        ApplianceFacts {
            last_checkin_s: Some(NOW - ago),
            last_published_s: Some(NOW - 3600),
            ..ApplianceFacts::default()
        }
    }

    #[test]
    fn states_follow_check_ins_and_publications() {
        let s = BackupSettings::default();
        assert_eq!(
            state(&ApplianceFacts::default(), NOW, &s),
            ApplianceState::Pending
        );
        assert_eq!(state(&seen(30), NOW, &s), ApplianceState::Active);
        assert_eq!(state(&seen(180), NOW, &s), ApplianceState::Active);
        assert_eq!(state(&seen(181), NOW, &s), ApplianceState::Late);
        assert_eq!(
            state(&seen(72 * 3600 + 1), NOW, &s),
            ApplianceState::Delinquent
        );
        let fresh = ApplianceFacts {
            last_published_s: None,
            ..seen(10)
        };
        assert_eq!(state(&fresh, NOW, &s), ApplianceState::Seeding);
        let stuck = ApplianceFacts {
            oldest_unpublished_offer_s: Some(NOW - 72 * 3600 - 1),
            ..seen(10)
        };
        assert_eq!(state(&stuck, NOW, &s), ApplianceState::Delinquent);
        let repairing = ApplianceFacts {
            progress: Progress::Repairing,
            ..seen(10)
        };
        assert_eq!(state(&repairing, NOW, &s), ApplianceState::Repairing);
        let reseed = ApplianceFacts {
            progress: Progress::ReseedRequired,
            ..seen(10)
        };
        assert_eq!(state(&reseed, NOW, &s), ApplianceState::ReseedRequired);
        assert_eq!(
            state(
                &ApplianceFacts {
                    paused: true,
                    ..seen(10)
                },
                NOW,
                &s
            ),
            ApplianceState::Paused
        );
        assert_eq!(
            state(
                &ApplianceFacts {
                    retired: true,
                    paused: true,
                    ..seen(10)
                },
                NOW,
                &s
            ),
            ApplianceState::Retired
        );
    }

    #[test]
    fn availability_uses_the_ready_window_and_the_lock() {
        let s = BackupSettings::default();
        assert!(available(&seen(300), NOW, &s));
        assert!(!available(&seen(301), NOW, &s));
        let locked = ApplianceFacts {
            locked: true,
            ..seen(10)
        };
        assert!(!available(&locked, NOW, &s));
        assert_eq!(
            state(&locked, NOW, &s),
            ApplianceState::Active,
            "LOCKED never counts as removal"
        );
        assert!(!healthy(&locked, NOW, &s));
        assert!(healthy(&seen(10), NOW, &s));
        assert!(!available(&ApplianceFacts::default(), NOW, &s));
    }
}
