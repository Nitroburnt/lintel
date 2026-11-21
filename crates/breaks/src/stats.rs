// Break compliance: how each day's breaks ended, by kind. A count, never a score: a break that was
// skipped is a skipped break, not a failure. One file of the feature's own, kept for the same
// thirty days as the screen time, so the two read on one calendar.
//
// It lives apart from screen-time.json because it is counted whenever a reminder runs, while that
// file is only open while screen time is on.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The file, in the feature's data folder.
pub const FILE: &str = "breaks.json";
/// Days kept when the person leaves the setting alone, today included: the same window as the
/// screen time. 0 ("Unlimited") keeps every day.
pub const KEEP: usize = 30;

/// The ways a break can end, as `Ev::End` names them.
#[derive(Serialize, Deserialize, Default, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Counts {
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub skip: u64,
    #[serde(default)]
    pub snooze: u64,
    /// Ended because the person locked the PC or walked away.
    #[serde(default)]
    pub away: u64,
}

impl Counts {
    pub fn total(&self) -> u64 {
        self.done + self.skip + self.snooze + self.away
    }

    fn add(&mut self, why: &str) -> bool {
        match why {
            "done" => self.done += 1,
            "skip" => self.skip += 1,
            "snooze" => self.snooze += 1,
            "away" => self.away += 1,
            // "off" is the feature being turned off mid-break: not the person's answer.
            _ => return false,
        }
        true
    }
}

#[derive(Serialize, Deserialize, Default, Clone, Copy)]
struct DayStats {
    #[serde(default)]
    eye: Counts,
    #[serde(default, rename = "move")]
    mov: Counts,
}

#[derive(Serialize, Deserialize, Default)]
struct Store {
    #[serde(default)]
    days: BTreeMap<String, DayStats>,
}

pub struct Stats {
    path: Option<PathBuf>,
    store: Store,
    dirty: bool,
    /// How many days are kept, today included; 0 keeps every day.
    keep_days: usize,
}

impl Default for Stats {
    fn default() -> Self {
        Stats { path: None, store: Store::default(), dirty: false, keep_days: KEEP }
    }
}

impl Stats {
    pub fn load(path: Option<PathBuf>) -> Self {
        let store = path.as_deref().map(notch_core::storage::json_load).unwrap_or_default();
        Stats { path, store, dirty: false, keep_days: KEEP }
    }

    /// How many days the counts keep, today included; 0 keeps every day. A shorter window takes
    /// the days it no longer keeps at once. True when it did.
    pub fn set_keep(&mut self, days: usize) -> bool {
        self.keep_days = days;
        self.trim()
    }

    /// What is over the person's own window goes, oldest first; 0 keeps every day.
    fn trim(&mut self) -> bool {
        let mut gone = false;
        while self.keep_days > 0 && self.store.days.len() > self.keep_days {
            self.store.days.pop_first();
            gone = true;
        }
        if gone {
            self.dirty = true;
        }
        gone
    }

    /// One break of `kind` ("eye" or "move") ended for `why` on `day`. An end that is not the
    /// person's answer ("off") is not counted.
    pub fn record(&mut self, day: &str, kind: &str, why: &str) -> bool {
        let day_stats = self.store.days.entry(day.to_string()).or_default();
        let took = match kind {
            "eye" => day_stats.eye.add(why),
            "move" => day_stats.mov.add(why),
            _ => false,
        };
        if took {
            self.trim();
            self.dirty = true;
        }
        took
    }

    /// Today's counts, in the shape the page reads: `{ day, eye: {...}, move: {...} }`.
    pub fn snapshot(&self, day: &str) -> Value {
        let d = self.store.days.get(day).copied().unwrap_or_default();
        json!({
            "day": day,
            "eye": d.eye,
            "move": d.mov,
        })
    }

    pub fn clear(&mut self) {
        self.store.days.clear();
        self.dirty = true;
        self.save();
    }

    pub fn save(&mut self) {
        if let (true, Some(path)) = (self.dirty, self.path.as_deref()) {
            if let Err(e) = notch_core::storage::json_save_atomic(path, &self.store) {
                notch_core::log::line(format!("breaks: break counts not saved: {e}"));
                return;
            }
        }
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> Stats {
        Stats::load(None)
    }

    #[test]
    fn each_end_counts_under_its_own_kind() {
        let mut s = stats();
        assert!(s.record("2026-11-15", "eye", "done"));
        assert!(s.record("2026-11-15", "eye", "done"));
        assert!(s.record("2026-11-15", "eye", "skip"));
        assert!(s.record("2026-11-15", "eye", "snooze"));
        assert!(s.record("2026-11-15", "eye", "away"));
        assert!(s.record("2026-11-15", "move", "done"));
        let v = s.snapshot("2026-11-15");
        assert_eq!(v["eye"]["done"], 2);
        assert_eq!(v["eye"]["skip"], 1);
        assert_eq!(v["eye"]["snooze"], 1);
        assert_eq!(v["eye"]["away"], 1);
        assert_eq!(v["move"]["done"], 1);
        assert_eq!(v["move"]["skip"], 0);
    }

    #[test]
    fn a_day_nobody_broke_has_nothing_and_reads_as_zero() {
        let s = stats();
        let v = s.snapshot("2026-11-15");
        assert_eq!(v["day"], "2026-11-15");
        assert_eq!(v["eye"]["done"], 0);
        assert_eq!(v["move"]["away"], 0);
    }

    #[test]
    fn turning_the_feature_off_is_not_an_answer_and_is_not_counted() {
        let mut s = stats();
        assert!(!s.record("2026-11-15", "eye", "off"));
        assert!(!s.record("2026-11-15", "elbow", "done"));
        assert_eq!(s.snapshot("2026-11-15")["eye"]["done"], 0);
    }

    #[test]
    fn the_same_thirty_days_are_kept_by_default() {
        let mut s = stats();
        for day in 1..=31 {
            s.record(&format!("2026-10-{day:02}"), "eye", "done");
        }
        let all = s.store.days.len();
        assert_eq!(all, KEEP);
        assert!(!s.store.days.contains_key("2026-10-01"));
        assert_eq!(s.snapshot("2026-10-31")["eye"]["done"], 1);
    }

    #[test]
    fn unlimited_keeps_every_day_and_a_shorter_window_trims_at_once() {
        let mut s = stats();
        s.set_keep(0);
        for day in 0..400 {
            s.record(&format!("day-{day:03}"), "eye", "done");
        }
        assert_eq!(s.store.days.len(), 400);
        assert!(s.set_keep(10));
        assert_eq!(s.store.days.len(), 10);
        assert!(!s.store.days.contains_key("day-000"));
        assert!(s.store.days.contains_key("day-399"));
    }

    #[test]
    fn a_count_of_ends_adds_up() {
        let mut c = Counts::default();
        for why in ["done", "done", "skip", "snooze", "away", "off"] {
            c.add(why);
        }
        assert_eq!(c.total(), 5);
    }

    #[test]
    fn clearing_takes_every_day() {
        let mut s = stats();
        s.record("2026-11-15", "eye", "done");
        s.clear();
        assert_eq!(s.snapshot("2026-11-15")["eye"]["done"], 0);
    }
}
