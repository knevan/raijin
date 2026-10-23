//! Per-queue weekly start/stop schedule.
//!
//! Semantics mirror ABDM `ScheduleTimes`: a day-of-week set plus one start time
//! and one stop time, each with its own enable flag. Evaluation is
//! edge-triggered — the scheduler asks for the next start/stop edge after "now"
//! and fires only when that edge has passed. A missed edge (process restarted,
//! machine suspended) therefore never fires late: the next edge is recomputed
//! from the current wall clock, so a schedule can never resurrect a queue the
//! user stopped hours ago.

use std::fmt;

use chrono::{Datelike, Duration, NaiveDateTime, NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Days a schedule can target, named as chrono's `Display` writes them.
const WEEKDAY_NAMES: [(Weekday, &str); 7] = [
    (Weekday::Mon, "mon"),
    (Weekday::Tue, "tue"),
    (Weekday::Wed, "wed"),
    (Weekday::Thu, "thu"),
    (Weekday::Fri, "fri"),
    (Weekday::Sat, "sat"),
    (Weekday::Sun, "sun"),
];

/// Errors produced by the typed schedule boundary.
#[derive(Debug, Error)]
pub enum QueueScheduleError {
    /// A schedule without a target day can never fire.
    #[error("queue schedule needs at least one day of week")]
    EmptyDays,
    /// The stored JSON does not match the schedule shape.
    #[error("queue schedule is not valid JSON: {0}")]
    InvalidJson(#[from] serde_json::Error),
}

/// Durable weekly window for one queue.
///
/// `start_enabled` and `end_enabled` are independent, matching ABDM: a
/// stop-only schedule (stop every night at 07:30) is valid, and so is a
/// start-only one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueSchedule {
    /// Days the edges apply to. Stored deduplicated; order is irrelevant.
    pub days: Vec<Weekday>,
    /// Local wall-clock time the queue should start.
    pub start: NaiveTime,
    /// Local wall-clock time the queue should stop.
    pub end: NaiveTime,
    /// Whether `start` is an active edge.
    pub start_enabled: bool,
    /// Whether `end` is an active edge.
    pub end_enabled: bool,
}

impl Default for QueueSchedule {
    fn default() -> Self {
        Self {
            days: all_days(),
            start: NaiveTime::from_hms_opt(2, 30, 0).unwrap_or_default(),
            end: NaiveTime::from_hms_opt(7, 30, 0).unwrap_or_default(),
            start_enabled: false,
            end_enabled: false,
        }
    }
}

impl QueueSchedule {
    /// Parses the persisted `queues.schedule_json` column.
    ///
    /// Tolerant on read: `NULL`, empty, or unparsable JSON yields `None` so a
    /// corrupt row disables scheduling instead of failing the whole boot.
    #[must_use]
    pub fn from_json(raw: Option<&str>) -> Option<Self> {
        let raw = raw?.trim();
        if raw.is_empty() {
            return None;
        }
        match serde_json::from_str(raw) {
            Ok(schedule) => Some(schedule),
            Err(error) => {
                tracing::warn!(%error, "ignoring unreadable queue schedule");
                None
            }
        }
    }

    /// Serializes the schedule for `queues.schedule_json`.
    ///
    /// # Errors
    ///
    /// Returns [`QueueScheduleError::EmptyDays`] when no day is selected, so
    /// an unusable schedule can never reach the database.
    pub fn to_json(&self) -> Result<String, QueueScheduleError> {
        if self.days.is_empty() {
            return Err(QueueScheduleError::EmptyDays);
        }
        serde_json::to_string(self).map_err(QueueScheduleError::from)
    }

    /// Returns a canonical copy with duplicate days removed.
    #[must_use]
    pub fn canonicalized(mut self) -> Self {
        let mut seen = Vec::with_capacity(self.days.len());
        self.days.retain(|day| {
            if seen.contains(day) {
                return false;
            }
            seen.push(*day);
            true
        });
        self
    }

    /// Returns true when at least one edge is enabled.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.start_enabled || self.end_enabled
    }

    /// Earliest enabled edge strictly after `now`.
    ///
    /// # Errors
    ///
    /// Returns [`QueueScheduleError::EmptyDays`] when the day set is empty.
    pub fn next_start_after(
        &self,
        now: NaiveDateTime,
    ) -> Result<Option<NaiveDateTime>, QueueScheduleError> {
        self.next_edge_after(now, self.start, self.start_enabled)
    }

    /// Earliest enabled stop edge strictly after `now`.
    ///
    /// # Errors
    ///
    /// Returns [`QueueScheduleError::EmptyDays`] when the day set is empty.
    pub fn next_stop_after(
        &self,
        now: NaiveDateTime,
    ) -> Result<Option<NaiveDateTime>, QueueScheduleError> {
        self.next_edge_after(now, self.end, self.end_enabled)
    }

    fn next_edge_after(
        &self,
        now: NaiveDateTime,
        at: NaiveTime,
        enabled: bool,
    ) -> Result<Option<NaiveDateTime>, QueueScheduleError> {
        if self.days.is_empty() {
            return Err(QueueScheduleError::EmptyDays);
        }
        if !enabled {
            return Ok(None);
        }
        // One week of days is enough: the set is non-empty, so a hit is certain
        // before the loop ends. Days are walked (not dated) so a DST day keeps
        // the schedule's wall-clock meaning.
        let today = now.date();
        for offset in 0..=7 {
            let Some(date) = today.checked_add_signed(Duration::days(offset)) else {
                continue;
            };
            if !self.days.contains(&date.weekday()) {
                continue;
            }
            let Some(candidate) = date.and_time(at).with_nanosecond(0) else {
                continue;
            };
            if candidate > now {
                return Ok(Some(candidate));
            }
        }
        Ok(None)
    }
}

impl fmt::Display for QueueSchedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let days = self
            .days
            .iter()
            .filter_map(|day| WEEKDAY_NAMES.iter().find(|(d, _)| d == day))
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(",");
        write!(
            f,
            "{} {:02}:{:02}→{:02}:{:02}",
            days,
            self.start.hour(),
            self.start.minute(),
            self.end.hour(),
            self.end.minute()
        )
    }
}

/// Local date for `now` in a schedule's own wall-clock terms.
#[must_use]
pub fn local_now(now: chrono::DateTime<chrono::Local>) -> NaiveDateTime {
    now.naive_local()
}

/// Returns every weekday, the default day set.
#[must_use]
pub fn all_days() -> Vec<Weekday> {
    WEEKDAY_NAMES.iter().map(|(day, _)| *day).collect()
}

/// Returns the persisted name of `day` (`"Mon"`, matching chrono's serde).
#[must_use]
pub fn weekday_name(day: Weekday) -> &'static str {
    WEEKDAY_NAMES
        .iter()
        .find(|(candidate, _)| *candidate == day)
        .map(|(_, name)| *name)
        .unwrap_or("mon")
}

/// Parses a three-letter weekday name, case-insensitively.
#[must_use]
pub fn parse_weekday(name: &str) -> Option<Weekday> {
    let name = name.trim().to_ascii_lowercase();
    WEEKDAY_NAMES
        .iter()
        .find(|(_, candidate)| *candidate == name)
        .map(|(day, _)| *day)
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveTime};

    use super::*;

    fn at(date: &str, time: &str) -> NaiveDateTime {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").expect("valid date");
        let time = NaiveTime::parse_from_str(time, "%H:%M:%S").expect("valid time");
        date.and_time(time)
    }

    fn weekdays_schedule() -> QueueSchedule {
        QueueSchedule {
            days: [Weekday::Mon].into_iter().collect(),
            start: NaiveTime::from_hms_opt(2, 30, 0).expect("valid time"),
            end: NaiveTime::from_hms_opt(7, 30, 0).expect("valid time"),
            start_enabled: true,
            end_enabled: true,
        }
    }

    #[test]
    fn next_start_should_use_today_when_time_has_not_passed() {
        // 2026-10-19 is a Monday.
        let schedule = weekdays_schedule();
        let next = schedule
            .next_start_after(at("2026-10-19", "01:00:00"))
            .expect("schedule is valid");

        assert_eq!(next, Some(at("2026-10-19", "02:30:00")));
    }

    #[test]
    fn next_start_should_skip_to_next_selected_day_after_the_time_passed() {
        let schedule = weekdays_schedule();
        let next = schedule
            .next_start_after(at("2026-10-19", "02:30:00"))
            .expect("schedule is valid");

        // Strictly after now, so Monday 02:30 is not a candidate any more.
        assert_eq!(next, Some(at("2026-10-26", "02:30:00")));
    }

    #[test]
    fn disabled_edges_should_report_no_next_time() {
        let schedule = QueueSchedule {
            start_enabled: false,
            end_enabled: false,
            ..weekdays_schedule()
        };
        let now = at("2026-10-19", "01:00:00");

        assert_eq!(schedule.next_start_after(now).ok().flatten(), None);
        assert_eq!(schedule.next_stop_after(now).ok().flatten(), None);
        assert!(!schedule.is_active());
    }

    #[test]
    fn overnight_window_should_keep_both_edges_on_their_own_day() {
        // 23:00 -> 06:00 with both edges on every day: the stop edge after
        // 23:30 is the *next* morning, not the same date.
        let schedule = QueueSchedule {
            days: all_days(),
            start: NaiveTime::from_hms_opt(23, 0, 0).expect("valid time"),
            end: NaiveTime::from_hms_opt(6, 0, 0).expect("valid time"),
            start_enabled: true,
            end_enabled: true,
        };
        let now = at("2026-10-19", "23:30:00");

        assert_eq!(
            schedule.next_start_after(now).expect("valid"),
            Some(at("2026-10-20", "23:00:00"))
        );
        assert_eq!(
            schedule.next_stop_after(now).expect("valid"),
            Some(at("2026-10-20", "06:00:00"))
        );
    }

    #[test]
    fn empty_day_set_should_be_rejected_on_write_and_reported_on_read() {
        let schedule = QueueSchedule {
            days: Vec::new(),
            ..QueueSchedule::default()
        };

        assert!(matches!(
            schedule.to_json(),
            Err(QueueScheduleError::EmptyDays)
        ));
        assert!(matches!(
            schedule.next_start_after(at("2026-10-19", "01:00:00")),
            Err(QueueScheduleError::EmptyDays)
        ));
    }

    #[test]
    fn read_should_tolerate_missing_garbage_and_empty_values() {
        assert!(QueueSchedule::from_json(None).is_none());
        assert!(QueueSchedule::from_json(Some("   ")).is_none());
        assert!(QueueSchedule::from_json(Some("{not json")).is_none());
        // Unknown fields are rejected, not silently accepted as data loss.
        assert!(
            QueueSchedule::from_json(Some(r#"{"days":["mon"],"bogus":1}"#)).is_none(),
            "unknown fields must not be tolerated"
        );
    }

    #[test]
    fn round_trip_should_preserve_every_field() {
        let schedule = weekdays_schedule();
        let raw = schedule.to_json().expect("valid schedule");
        let parsed = QueueSchedule::from_json(Some(&raw)).expect("round trip");

        assert_eq!(parsed, schedule);
        // Persisted names follow chrono's `Weekday` serde form.
        assert!(raw.contains("\"Mon\""), "unexpected schedule JSON: {raw}");
    }

    #[test]
    fn default_should_match_abdm_disabled_window() {
        let schedule = QueueSchedule::default();

        assert_eq!(schedule.days.len(), 7);
        assert!(!schedule.is_active());
        assert_eq!(schedule.start.hour(), 2);
        assert_eq!(schedule.start.minute(), 30);
        assert_eq!(schedule.end.hour(), 7);
        assert_eq!(schedule.end.minute(), 30);
    }

    #[test]
    fn weekday_names_should_round_trip() {
        for day in all_days() {
            let name = weekday_name(day);
            assert_eq!(parse_weekday(name), Some(day));
        }
        assert_eq!(parse_weekday("MON"), Some(Weekday::Mon));
        assert_eq!(parse_weekday("nope"), None);
    }
}
