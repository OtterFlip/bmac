// Copyright (c) 2026 BEENTHERE VENTURES, INC.
// SPDX-License-Identifier: GPL-3.0-only

//! Schedules without a leader (`ARCHITECTURE.md`, "Schedules without a
//! leader").
//!
//! A schedule names an operation, its parameters, and when it recurs. Every
//! host evaluates every schedule: when a slot comes due, each one admits the
//! job with an idempotency key made from the schedule ID and the slot, as
//! the schedule's own user, so the first admission creates the job and the
//! rest find it. A slot that can't start yet (no quorum, a conflicting job)
//! is tried again until it is [`GRACE_MS`] old, then counted as missed.
//! Times of day are in the cluster's time zone; a time skipped by a
//! daylight-saving change runs at the first moment after the gap, and a
//! repeated one runs once, the first time.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bmac_core::OperationId;
use bmac_pmxcfs::{LockOptions, Pmxcfs, PmxcfsError, RegistryLock, read_record, write_record};
use bmac_proto::api::{ApiError, ErrorCode, LaunchJob, Role};
use jiff::Timestamp;
use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Clock;
use crate::jobs::{Caller, Jobs};
use crate::log;
use crate::runner::Runner;

pub const SCHEDULES_DIR: &str = "/etc/pve/priv/bmac/schedules";
const LOCK_PREFIX: &str = "bmac-schedules-";
pub const RECORD_SCHEMA: u64 = 1;
pub const EVALUATE_EVERY: Duration = Duration::from_secs(30);
/// How late a slot may still start.
pub const GRACE_MS: i64 = 15 * 60_000;
const MAX_ID: usize = 48;
const MAX_TITLE: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Day {
    fn weekday(self) -> Weekday {
        match self {
            Day::Mon => Weekday::Monday,
            Day::Tue => Weekday::Tuesday,
            Day::Wed => Weekday::Wednesday,
            Day::Thu => Weekday::Thursday,
            Day::Fri => Weekday::Friday,
            Day::Sat => Weekday::Saturday,
            Day::Sun => Weekday::Sunday,
        }
    }
}

/// When a schedule comes due. `at` is a 24-hour `HH:MM` in the cluster's
/// time zone; intervals count from the Unix epoch, so every host agrees on
/// them whatever its zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Recurrence {
    Interval {
        minutes: u32,
    },
    Daily {
        at: String,
    },
    Weekly {
        days: Vec<Day>,
        at: String,
    },
    /// Days 1 to 28, so that every month has the day.
    Monthly {
        day: u8,
        at: String,
    },
}

/// One occurrence of a schedule: when it is due, and the label that makes
/// its idempotency key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub at_ms: i64,
    pub label: String,
}

fn time_of_day(at: &str) -> Result<(i8, i8), String> {
    let bad = || format!("{at:?} is not a 24-hour time like 03:00");
    let (h, m) = at.split_once(':').ok_or_else(bad)?;
    if h.len() != 2 || m.len() != 2 {
        return Err(bad());
    }
    let hour: i8 = h.parse().map_err(|_| bad())?;
    let minute: i8 = m.parse().map_err(|_| bad())?;
    if !(0..24).contains(&hour) || !(0..60).contains(&minute) {
        return Err(bad());
    }
    Ok((hour, minute))
}

impl Recurrence {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Recurrence::Interval { minutes } => {
                if !(1..=7 * 24 * 60).contains(minutes) {
                    return Err("an interval is 1 minute to 7 days".into());
                }
                Ok(())
            }
            Recurrence::Daily { at } => time_of_day(at).map(|_| ()),
            Recurrence::Weekly { days, at } => {
                if days.is_empty() {
                    return Err("a weekly schedule needs at least one day".into());
                }
                time_of_day(at).map(|_| ())
            }
            Recurrence::Monthly { day, at } => {
                if !(1..=28).contains(day) {
                    return Err("a monthly schedule runs on day 1 to 28".into());
                }
                time_of_day(at).map(|_| ())
            }
        }
    }

    fn on(&self, date: Date) -> bool {
        match self {
            Recurrence::Interval { .. } | Recurrence::Daily { .. } => true,
            Recurrence::Weekly { days, .. } => days.iter().any(|d| d.weekday() == date.weekday()),
            Recurrence::Monthly { day, .. } => date.day() == *day as i8,
        }
    }

    /// The latest slot at or before `now_ms`.
    pub fn latest(&self, now_ms: i64, zone: &TimeZone) -> Option<Slot> {
        let now = Timestamp::from_millisecond(now_ms).ok()?;
        let at = match self {
            Recurrence::Interval { minutes } => {
                let period = i64::from(*minutes) * 60_000;
                let at_ms = now_ms - now_ms.rem_euclid(period);
                let utc = Timestamp::from_millisecond(at_ms)
                    .ok()?
                    .to_zoned(TimeZone::UTC);
                return Some(Slot {
                    at_ms,
                    label: utc.strftime("%Y%m%dT%H%MZ").to_string(),
                });
            }
            Recurrence::Daily { at }
            | Recurrence::Weekly { at, .. }
            | Recurrence::Monthly { at, .. } => at,
        };
        let (hour, minute) = time_of_day(at).ok()?;
        let mut date = now.to_zoned(zone.clone()).date();
        for _ in 0..62 {
            if self.on(date) {
                let local = date.at(hour, minute, 0, 0);
                let at_ms = zone
                    .to_ambiguous_zoned(local)
                    .compatible()
                    .ok()?
                    .timestamp()
                    .as_millisecond();
                if at_ms <= now_ms {
                    return Some(Slot {
                        at_ms,
                        label: local.strftime("%Y%m%dT%H%M").to_string(),
                    });
                }
            }
            date = date.yesterday().ok()?;
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub schema_version: u64,
    pub record_type: String,
    #[serde(default)]
    pub revision: u64,
    pub id: String,
    pub title: String,
    pub operation: OperationId,
    #[serde(default)]
    pub parameters: BTreeMap<String, Value>,
    pub every: Recurrence,
    pub enabled: bool,
    /// The role its jobs run with: never more than the role of whoever
    /// saved it.
    pub run_as: Role,
    pub updated_by: String,
    pub updated_at_ms: i64,
}

impl Schedule {
    /// The user its jobs are admitted as.
    pub fn user(&self) -> String {
        format!("schedule:{}", self.id)
    }

    pub fn idempotency_key(&self, slot: &Slot) -> String {
        format!("schedule:{}:{}", self.id, slot.label)
    }

    pub fn validate(&self) -> Result<(), ApiError> {
        let invalid = |m: String| ApiError::new(ErrorCode::Invalid, m);
        if !is_schedule_id(&self.id) {
            return Err(invalid(format!(
                "a schedule ID is 1 to {MAX_ID} lowercase letters, digits, and hyphens, starting with a letter"
            )));
        }
        if self.title.trim().is_empty() || self.title.len() > MAX_TITLE {
            return Err(invalid(format!("a title is 1 to {MAX_TITLE} characters")));
        }
        self.every.validate().map_err(invalid)
    }
}

pub fn is_schedule_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id.starts_with(|c: char| c.is_ascii_lowercase())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn storage(e: PmxcfsError) -> ApiError {
    match e {
        PmxcfsError::RevisionConflict { .. } => ApiError::new(
            ErrorCode::Conflict,
            "the schedule changed since it was read; reload and try again",
        ),
        PmxcfsError::LockTimeout { .. } => {
            ApiError::new(ErrorCode::Unavailable, "the cluster is busy; try again")
        }
        other => ApiError::new(ErrorCode::Internal, format!("schedules: {other}")),
    }
}

/// The schedule records in pmxcfs.
#[derive(Clone)]
pub struct Schedules {
    pmxcfs: Pmxcfs,
    dir: PathBuf,
    lock_timeout: Duration,
}

impl Schedules {
    pub fn new(pmxcfs: Pmxcfs, dir: impl Into<PathBuf>) -> Schedules {
        Schedules {
            pmxcfs,
            dir: dir.into(),
            lock_timeout: Duration::from_secs(15),
        }
    }

    pub fn system() -> Schedules {
        Schedules::new(Pmxcfs::system(), SCHEDULES_DIR)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn locked<T>(
        &self,
        f: impl FnOnce(&RegistryLock) -> Result<T, ApiError>,
    ) -> Result<T, ApiError> {
        self.pmxcfs
            .ensure_directory(&self.dir, 0o700)
            .map_err(storage)?;
        let options = LockOptions {
            timeout: self.lock_timeout,
            prefix: LOCK_PREFIX.to_string(),
            short_lived: true,
            ..LockOptions::default()
        };
        let lock = RegistryLock::acquire(&self.pmxcfs, &self.dir, &options).map_err(storage)?;
        let result = f(&lock);
        let released = lock.release().map_err(storage);
        let value = result?;
        released?;
        Ok(value)
    }

    fn read(&self, path: &std::path::Path) -> Result<Option<Schedule>, ApiError> {
        let Some(value) =
            read_record(&self.pmxcfs, path, "schedule", RECORD_SCHEMA).map_err(storage)?
        else {
            return Ok(None);
        };
        serde_json::from_value(value)
            .map(Some)
            .map_err(|e| ApiError::new(ErrorCode::Internal, format!("{}: {e}", path.display())))
    }

    pub fn get(&self, id: &str) -> Result<Option<Schedule>, ApiError> {
        if !is_schedule_id(id) {
            return Ok(None);
        }
        self.read(&self.path(id))
    }

    /// Every readable schedule, by ID. One that can't be read is reported
    /// and left out, so it doesn't stop the others.
    pub fn list(&self) -> Result<Vec<Schedule>, ApiError> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(ApiError::new(
                    ErrorCode::Internal,
                    format!("cannot list {}: {e}", self.dir.display()),
                ));
            }
        };
        let mut out = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
            else {
                continue;
            };
            if !is_schedule_id(id) {
                continue;
            }
            match self.read(&path) {
                Ok(Some(schedule)) if schedule.id == id => out.push(schedule),
                Ok(Some(_)) => log::warning(&format!(
                    "schedules: {} names another schedule; ignored",
                    path.display()
                )),
                Ok(None) => {}
                Err(e) => log::warning(&format!("schedules: {}", e.message)),
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Creates a schedule (`expected` `None`) or replaces the revision the
    /// caller read, and returns it as stored.
    pub fn save(
        &self,
        mut schedule: Schedule,
        expected: Option<u64>,
    ) -> Result<Schedule, ApiError> {
        schedule.schema_version = RECORD_SCHEMA;
        schedule.record_type = "schedule".into();
        schedule.validate()?;
        self.locked(|lock| {
            let value = serde_json::to_value(&schedule).expect("schedules serialize");
            let written =
                write_record(lock, &self.path(&schedule.id), value, expected).map_err(storage)?;
            schedule.revision = written.revision;
            Ok(schedule)
        })
    }

    pub fn delete(&self, id: &str, expected: u64) -> Result<(), ApiError> {
        let missing = || ApiError::new(ErrorCode::NotFound, format!("no schedule {id}"));
        if !is_schedule_id(id) {
            return Err(missing());
        }
        self.locked(|_| {
            let current = self.read(&self.path(id))?.ok_or_else(missing)?;
            if current.revision != expected {
                return Err(storage(PmxcfsError::RevisionConflict {
                    path: self.path(id),
                    expected,
                    found: current.revision,
                }));
            }
            self.pmxcfs.remove(&self.path(id)).map_err(storage)
        })
    }
}

/// What evaluating one schedule did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// This host admitted the slot's job.
    Admitted(bmac_core::JobId),
    /// Another host, or an earlier evaluation here, already had.
    Existing(bmac_core::JobId),
    /// The job can't start yet; tried again next time.
    Waiting(String),
    /// The slot is past its grace period without a job.
    Missed,
    /// Admission refused the job outright; not tried again for this slot.
    Refused(String),
}

pub struct SchedulerConfig {
    pub every: Duration,
    pub grace_ms: i64,
    /// The cluster's time zone.
    pub zone: TimeZone,
    pub clock: Clock,
}

impl SchedulerConfig {
    pub fn system(clock: Clock) -> SchedulerConfig {
        SchedulerConfig {
            every: EVALUATE_EVERY,
            grace_ms: GRACE_MS,
            zone: TimeZone::system(),
            clock,
        }
    }
}

pub struct Scheduler {
    schedules: Schedules,
    jobs: Arc<Jobs>,
    config: SchedulerConfig,
    runner: Option<Arc<Runner>>,
    started_ms: i64,
    /// The last slot handled per schedule, and the last one warned about.
    handled: Mutex<HashMap<String, String>>,
    warned: Mutex<HashMap<String, String>>,
}

impl Scheduler {
    pub fn new(schedules: Schedules, jobs: Arc<Jobs>, config: SchedulerConfig) -> Scheduler {
        let started_ms = (config.clock)();
        Scheduler {
            schedules,
            jobs,
            config,
            runner: None,
            started_ms,
            handled: Mutex::new(HashMap::new()),
            warned: Mutex::new(HashMap::new()),
        }
    }

    /// Wakes `runner` when a job is admitted here.
    pub fn with_runner(mut self, runner: Arc<Runner>) -> Scheduler {
        self.runner = Some(runner);
        self
    }

    fn warn_once(&self, id: &str, label: &str, message: &str) {
        let mut warned = self.warned.lock().unwrap();
        if warned.get(id).map(String::as_str) != Some(label) {
            warned.insert(id.to_string(), label.to_string());
            log::warning(message);
        }
    }

    /// Evaluates every schedule once.
    pub fn evaluate(&self) -> Vec<(String, Outcome)> {
        let now = (self.config.clock)();
        let schedules = match self.schedules.list() {
            Ok(schedules) => schedules,
            Err(e) => {
                log::warning(&format!("schedules: {}", e.message));
                return Vec::new();
            }
        };
        self.handled
            .lock()
            .unwrap()
            .retain(|id, _| schedules.iter().any(|s| &s.id == id));
        let mut outcomes = Vec::new();
        for schedule in schedules.iter().filter(|s| s.enabled) {
            if let Some(outcome) = self.evaluate_one(schedule, now) {
                outcomes.push((schedule.id.clone(), outcome));
            }
        }
        outcomes
    }

    fn evaluate_one(&self, schedule: &Schedule, now: i64) -> Option<Outcome> {
        let slot = schedule.every.latest(now, &self.config.zone)?;
        if slot.at_ms < schedule.updated_at_ms {
            return None;
        }
        let id = schedule.id.as_str();
        if self.handled.lock().unwrap().get(id) == Some(&slot.label) {
            return None;
        }
        let handled = || {
            self.handled
                .lock()
                .unwrap()
                .insert(id.to_string(), slot.label.clone());
        };
        if now - slot.at_ms > self.config.grace_ms {
            handled();
            if slot.at_ms < self.started_ms {
                return None;
            }
            log::warning(&format!("schedule {id}: the {} run was missed", slot.label));
            return Some(Outcome::Missed);
        }
        let launch = LaunchJob {
            operation: schedule.operation.clone(),
            parameters: schedule.parameters.clone(),
            idempotency_key: schedule.idempotency_key(&slot),
            dry_run: false,
        };
        let user = schedule.user();
        let caller = Caller {
            user: &user,
            role: schedule.run_as,
        };
        match self.jobs.admit(caller, &launch) {
            Ok(admission) => {
                handled();
                if admission.existing {
                    return Some(Outcome::Existing(admission.job.id));
                }
                log::info(&format!(
                    "schedule {id}: started job {} for {}",
                    admission.job.id, slot.label
                ));
                if let Some(runner) = &self.runner {
                    runner.kick();
                }
                Some(Outcome::Admitted(admission.job.id))
            }
            Err(e)
                if matches!(
                    e.code,
                    ErrorCode::Unavailable
                        | ErrorCode::NoQuorum
                        | ErrorCode::Conflict
                        | ErrorCode::RateLimited
                        | ErrorCode::Internal
                ) =>
            {
                self.warn_once(
                    id,
                    &slot.label,
                    &format!(
                        "schedule {id}: the {} run is waiting: {}",
                        slot.label, e.message
                    ),
                );
                Some(Outcome::Waiting(e.message))
            }
            Err(e) => {
                handled();
                log::warning(&format!(
                    "schedule {id}: the {} run was refused: {}",
                    slot.label, e.message
                ));
                Some(Outcome::Refused(e.message))
            }
        }
    }

    /// Evaluates the schedules every [`SchedulerConfig::every`] until
    /// aborted.
    pub fn spawn(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(self.config.every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let scheduler = self.clone();
                let _ = tokio::task::spawn_blocking(move || scheduler.evaluate()).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(text: &str) -> i64 {
        text.parse::<Timestamp>().unwrap().as_millisecond()
    }

    fn eastern() -> TimeZone {
        TimeZone::posix("EST5EDT,M3.2.0,M11.1.0").unwrap()
    }

    fn slot(every: &Recurrence, now: &str) -> (String, String) {
        let slot = every.latest(ms(now), &eastern()).unwrap();
        let at = Timestamp::from_millisecond(slot.at_ms).unwrap().to_string();
        (at, slot.label)
    }

    fn daily(at: &str) -> Recurrence {
        Recurrence::Daily { at: at.into() }
    }

    #[test]
    fn a_daily_slot_is_today_once_its_time_has_come() {
        // 03:00 Eastern standard time is 08:00 UTC.
        assert_eq!(
            slot(&daily("03:00"), "2026-01-15T07:59:59Z"),
            ("2026-01-14T08:00:00Z".into(), "20260114T0300".into())
        );
        assert_eq!(
            slot(&daily("03:00"), "2026-01-15T08:00:00Z"),
            ("2026-01-15T08:00:00Z".into(), "20260115T0300".into())
        );
    }

    #[test]
    fn daylight_saving_changes_give_one_slot_a_day() {
        // 2026-03-08: 02:00 EST jumps to 03:00 EDT, so 02:30 never happens
        // and the slot runs at 03:00 EDT, the first moment after the gap.
        assert_eq!(
            slot(&daily("02:30"), "2026-03-08T12:00:00Z"),
            ("2026-03-08T07:30:00Z".into(), "20260308T0230".into())
        );
        // 2026-11-01: 01:30 happens twice; the slot is the first.
        assert_eq!(
            slot(&daily("01:30"), "2026-11-01T12:00:00Z"),
            ("2026-11-01T05:30:00Z".into(), "20261101T0130".into())
        );
        assert_eq!(
            slot(&daily("01:30"), "2026-11-01T06:10:00Z").1,
            "20261101T0130",
            "the second 01:30 is the same slot"
        );
    }

    #[test]
    fn weekly_monthly_and_interval_slots() {
        // 2026-10-11 is a Sunday.
        let weekly = Recurrence::Weekly {
            days: vec![Day::Mon, Day::Thu],
            at: "22:15".into(),
        };
        assert_eq!(slot(&weekly, "2026-10-11T12:00:00Z").1, "20261008T2215");
        assert_eq!(slot(&weekly, "2026-10-13T03:00:00Z").1, "20261012T2215");
        let monthly = Recurrence::Monthly {
            day: 28,
            at: "04:00".into(),
        };
        assert_eq!(slot(&monthly, "2026-03-01T12:00:00Z").1, "20260228T0400");
        let interval = Recurrence::Interval { minutes: 15 };
        assert_eq!(
            slot(&interval, "2026-10-11T12:44:59Z"),
            ("2026-10-11T12:30:00Z".into(), "20261011T1230Z".into())
        );
        assert_eq!(
            interval.latest(ms("2026-10-11T12:44:59Z"), &TimeZone::UTC),
            interval.latest(ms("2026-10-11T12:44:59Z"), &eastern()),
            "intervals ignore the zone"
        );
    }

    #[test]
    fn bad_recurrences_are_refused() {
        for (every, message) in [
            (daily("3:00"), "is not a 24-hour time"),
            (daily("24:00"), "is not a 24-hour time"),
            (Recurrence::Interval { minutes: 0 }, "1 minute to 7 days"),
            (
                Recurrence::Weekly {
                    days: vec![],
                    at: "01:00".into(),
                },
                "at least one day",
            ),
            (
                Recurrence::Monthly {
                    day: 31,
                    at: "01:00".into(),
                },
                "day 1 to 28",
            ),
        ] {
            let error = every.validate().unwrap_err();
            assert!(error.contains(message), "{every:?}: {error}");
        }
        assert!(is_schedule_id("pit-prod1"));
        for id in ["", "Pit", "1pit", "pit_1", &"a".repeat(49)] {
            assert!(!is_schedule_id(id), "{id}");
        }
    }
}
