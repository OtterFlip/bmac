// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! UTC timestamps in v2's form (`utc_now`, `cluster_registry.py` 265).

use std::time::{SystemTime, UNIX_EPOCH};

/// The current time as `YYYY-MM-DDTHH:MM:SS+00:00`, whole seconds.
pub fn utc_now() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    format_utc(seconds)
}

pub(crate) fn format_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}+00:00",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to a date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_like_python_isoformat() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00+00:00");
        assert_eq!(format_utc(946_684_800), "2000-01-01T00:00:00+00:00");
        assert_eq!(format_utc(951_782_400), "2000-02-29T00:00:00+00:00");
        assert_eq!(format_utc(1_791_726_245), "2026-10-11T13:44:05+00:00");
        assert_eq!(utc_now().len(), 25);
    }
}
