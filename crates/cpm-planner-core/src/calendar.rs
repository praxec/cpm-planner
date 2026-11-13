//! Working calendars and window arithmetic (`docs/calendars.md`).
//!
//! A [`Calendar`] opens one window per workday: `[day_start, day_start +
//! hours_per_day)` in local time, local time being UTC plus a fixed
//! `utc_offset_minutes`. A window may cross local midnight and belongs to the
//! weekday it opens on. A [`CalendarSpec`] is the `calendar` argument: either
//! one calendar or a per-resource [`CalendarMap`] with a `default` entry.
//!
//! [`WorkWindows`] is the open time of one deliverable (the intersection of
//! the calendars of the pools it claims), anchored at a plan start, with the
//! four window functions of the contract: [`WorkWindows::open_at`],
//! [`WorkWindows::advance`], [`WorkWindows::working_hours`] and
//! [`WorkWindows::retreat`]. All times are wall-clock hours from the plan
//! start.

use crate::plan::PlannerError;
use chrono::{DateTime, Datelike, Duration, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;

/// Hours in a week; windows repeat with this period.
const WEEK: f64 = 168.0;
/// Tolerance for comparing window boundaries and remaining work, in hours.
const TOL: f64 = 1e-9;
/// Maximum entries in a [`CalendarMap`], `default` included.
pub const MAX_CALENDAR_ENTRIES: usize = 256;
/// The fallback entry of a [`CalendarMap`].
pub const DEFAULT_KEY: &str = "default";

/// Working-time calendar. Unknown fields are rejected.
///
/// Every field is optional: `{}` is 8 hours Monday to Friday opening at
/// 00:00 UTC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Calendar {
    /// Working hours per workday, in (0, 24].
    #[serde(default = "default_hours_per_day")]
    pub hours_per_day: f32,
    /// Local opening time of each workday's window, `HH:MM` (24-hour, two
    /// digits each). Absent means `"00:00"`; output always echoes it.
    #[serde(default = "default_day_start")]
    pub day_start: String,
    /// Lowercase 3-letter weekday names (`"mon"`..`"sun"`); duplicates are
    /// ignored.
    #[serde(default = "default_workdays")]
    pub workdays: Vec<String>,
    /// Local offset from UTC in minutes, in [-1440, 1440]. Fixed: no
    /// daylight saving.
    #[serde(default)]
    pub utc_offset_minutes: i32,
}

fn default_hours_per_day() -> f32 {
    8.0
}

fn default_day_start() -> String {
    "00:00".to_string()
}

fn default_workdays() -> Vec<String> {
    ["mon", "tue", "wed", "thu", "fri"]
        .map(String::from)
        .to_vec()
}

impl Default for Calendar {
    fn default() -> Self {
        Self {
            hours_per_day: default_hours_per_day(),
            day_start: default_day_start(),
            workdays: default_workdays(),
            utc_offset_minutes: 0,
        }
    }
}

/// A rejected calendar field: the legacy range checks keep their
/// `INVALID_GRAPH` code on a single calendar, `day_start` is new and always
/// `INVALID_VALUE`.
enum Fault {
    Legacy(String),
    DayStart(String),
}

impl Fault {
    fn reason(self) -> String {
        match self {
            Fault::Legacy(r) | Fault::DayStart(r) => r,
        }
    }
}

/// Days from Monday for a lowercase 3-letter weekday name.
fn weekday_index(name: &str) -> Option<u32> {
    Some(match name {
        "mon" => 0,
        "tue" => 1,
        "wed" => 2,
        "thu" => 3,
        "fri" => 4,
        "sat" => 5,
        "sun" => 6,
        _ => return None,
    })
}

/// Minutes after local midnight for `HH:MM` (`00:00..=23:59`, exactly two
/// digits each), else `None`.
fn parse_day_start(s: &str) -> Option<u32> {
    let b = s.as_bytes();
    if b.len() != 5 || b[2] != b':' || ![0, 1, 3, 4].iter().all(|&i| b[i].is_ascii_digit()) {
        return None;
    }
    let digit = |i: usize| u32::from(b[i] - b'0');
    let (h, m) = (digit(0) * 10 + digit(1), digit(3) * 10 + digit(4));
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Calendar {
    /// Reject an unusable calendar with a clear reason.
    ///
    /// `hours_per_day`, `workdays` and `utc_offset_minutes` out of range are
    /// [`PlannerError::InvalidGraph`], as for the earned-value calendar; a
    /// malformed `day_start` is [`PlannerError::InvalidValue`].
    pub fn validate(&self) -> Result<(), PlannerError> {
        self.check("calendar").map_err(|fault| match fault {
            Fault::Legacy(reason) => PlannerError::InvalidGraph { reason },
            Fault::DayStart(reason) => PlannerError::InvalidValue { reason },
        })
    }

    fn check(&self, path: &str) -> Result<(), Fault> {
        if !(self.hours_per_day.is_finite()
            && self.hours_per_day > 0.0
            && self.hours_per_day <= 24.0)
        {
            return Err(Fault::Legacy(format!(
                "{path}.hours_per_day must be in (0, 24], got {}",
                self.hours_per_day
            )));
        }
        if !(-1440..=1440).contains(&self.utc_offset_minutes) {
            return Err(Fault::Legacy(format!(
                "{path}.utc_offset_minutes must be in [-1440, 1440], got {}",
                self.utc_offset_minutes
            )));
        }
        if self.workdays.is_empty() {
            return Err(Fault::Legacy(format!("{path}.workdays must not be empty")));
        }
        for name in &self.workdays {
            if weekday_index(name).is_none() {
                return Err(Fault::Legacy(format!(
                    "{path}.workdays entry {name:?} is not a lowercase 3-letter weekday (mon..sun)"
                )));
            }
        }
        if parse_day_start(&self.day_start).is_none() {
            return Err(Fault::DayStart(format!(
                "{path}.day_start must be HH:MM (00:00..23:59), got {:?}",
                self.day_start
            )));
        }
        Ok(())
    }

    /// Weekly open intervals in UTC hours from Monday 00:00, sorted,
    /// disjoint and merged where they touch. Assumes a validated calendar.
    fn weekly_intervals(&self) -> Vec<(f64, f64)> {
        let start_h = f64::from(parse_day_start(&self.day_start).unwrap_or(0)) / 60.0;
        let offset_h = f64::from(self.utc_offset_minutes) / 60.0;
        let len = f64::from(self.hours_per_day);
        let mut days: Vec<u32> = self
            .workdays
            .iter()
            .filter_map(|n| weekday_index(n))
            .collect();
        days.sort_unstable();
        days.dedup();
        let mut out = Vec::new();
        for day in days {
            let open = (f64::from(day) * 24.0 + start_h - offset_h).rem_euclid(WEEK);
            let close = open + len;
            if close > WEEK {
                out.push((open, WEEK));
                out.push((0.0, close - WEEK));
            } else {
                out.push((open, close));
            }
        }
        merge(out)
    }
}

/// Sort and merge touching or overlapping intervals.
fn merge(mut v: Vec<(f64, f64)>) -> Vec<(f64, f64)> {
    v.retain(|&(a, b)| b - a > TOL);
    v.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut out: Vec<(f64, f64)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(last) if a <= last.1 + TOL => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Intersection of two sorted, disjoint interval lists.
fn intersect(x: &[(f64, f64)], y: &[(f64, f64)]) -> Vec<(f64, f64)> {
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < x.len() && j < y.len() {
        let a = x[i].0.max(y[j].0);
        let b = x[i].1.min(y[j].1);
        if b - a > TOL {
            out.push((a, b));
        }
        if x[i].1 < y[j].1 {
            i += 1;
        } else {
            j += 1;
        }
    }
    merge(out)
}

/// Per-resource calendars: `default` plus named entries. Serialized as one
/// flat object with a `default` key.
#[derive(Debug, Clone, PartialEq)]
pub struct CalendarMap {
    /// Every entry, `default` included.
    entries: BTreeMap<String, Calendar>,
}

impl CalendarMap {
    /// A map whose every resource uses `calendar`.
    pub fn uniform(calendar: Calendar) -> Self {
        Self {
            entries: BTreeMap::from([(DEFAULT_KEY.to_string(), calendar)]),
        }
    }

    /// Build a map from its entries; `default` is required, keys must be
    /// non-empty, at most [`MAX_CALENDAR_ENTRIES`], and every calendar must
    /// validate. Every rejection is [`PlannerError::InvalidValue`] naming
    /// the path (`calendar.<key>.<field>`).
    pub fn new(entries: BTreeMap<String, Calendar>) -> Result<Self, PlannerError> {
        let bad = |reason: String| Err(PlannerError::InvalidValue { reason });
        if !entries.contains_key(DEFAULT_KEY) {
            return bad("calendar: a per-resource calendar map needs a `default` entry".into());
        }
        if entries.len() > MAX_CALENDAR_ENTRIES {
            return bad(format!(
                "calendar has {} entries; a calendar map holds at most {MAX_CALENDAR_ENTRIES}",
                entries.len()
            ));
        }
        for (key, calendar) in &entries {
            if key.is_empty() {
                return bad("calendar map keys must not be empty".into());
            }
            calendar
                .check(&format!("calendar.{key}"))
                .map_err(|f| PlannerError::InvalidValue { reason: f.reason() })?;
        }
        Ok(Self { entries })
    }

    /// Every entry, `default` included, by key.
    pub fn entries(&self) -> &BTreeMap<String, Calendar> {
        &self.entries
    }

    /// The entry key a pool resolves to: its own entry, else `default`.
    pub fn key_for<'a>(&'a self, pool: &'a str) -> &'a str {
        if self.entries.contains_key(pool) {
            pool
        } else {
            DEFAULT_KEY
        }
    }

    /// The calendar a pool resolves to (section 2.3).
    pub fn calendar_for(&self, pool: &str) -> &Calendar {
        &self.entries[self.key_for(pool)]
    }

    /// The sorted, deduplicated calendar keys of a deliverable claiming
    /// `pools` (section 2.5).
    pub fn keys_for<'a>(&self, pools: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut keys: Vec<String> = pools
            .into_iter()
            .map(|p| self.key_for(p).to_string())
            .collect();
        keys.sort();
        keys.dedup();
        keys
    }

    /// The windows of calendar `keys` (their intersection) anchored at
    /// `start`. An empty `keys` is always open. `None` when the
    /// intersection has no open time in a week.
    pub fn windows(&self, keys: &[String], start: DateTime<Utc>) -> Option<WorkWindows> {
        let mut weekly: Option<Vec<(f64, f64)>> = None;
        for key in keys {
            let own = self
                .entries
                .get(key)
                .unwrap_or(&self.entries[DEFAULT_KEY])
                .weekly_intervals();
            weekly = Some(match weekly {
                None => own,
                Some(acc) => intersect(&acc, &own),
            });
        }
        WorkWindows::from_weekly(weekly.unwrap_or_else(|| vec![(0.0, WEEK)]), start)
    }
}

impl Serialize for CalendarMap {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.entries.serialize(s)
    }
}

/// The `calendar` argument: one calendar, or a per-resource map. An object
/// with a `default` key is a map; anything else is a single calendar
/// (section 2.4).
#[derive(Debug, Clone, PartialEq)]
pub enum CalendarSpec {
    Single(Calendar),
    Map(CalendarMap),
}

/// Why a `calendar` argument was rejected by [`CalendarSpec::parse`].
#[derive(Debug, thiserror::Error)]
pub enum CalendarArgError {
    /// The single form did not decode (wrong type, unknown field): the
    /// argument-parsing error (`INVALID_PARAMS`) a typed calendar gives today.
    #[error("{0}")]
    Params(String),
    /// The calendar decoded but is unusable: `INVALID_GRAPH` for a legacy
    /// field of a single calendar, `INVALID_VALUE` for `day_start` and for
    /// every map error.
    #[error(transparent)]
    Invalid(#[from] PlannerError),
}

impl CalendarSpec {
    /// Decode and validate a raw `calendar` argument (section 2.4).
    pub fn parse(value: &Value) -> Result<Self, CalendarArgError> {
        let is_map = value
            .as_object()
            .is_some_and(|o| o.contains_key(DEFAULT_KEY));
        if !is_map {
            let calendar = Calendar::deserialize(value).map_err(|e| {
                let mut msg = format!("calendar: {e}");
                if msg.contains("unknown field") {
                    msg.push_str("; a per-resource calendar map needs a `default` entry");
                }
                CalendarArgError::Params(msg)
            })?;
            calendar.validate()?;
            return Ok(Self::Single(calendar));
        }
        let object = value.as_object().expect("INVARIANT: is_map checked object");
        let mut entries = BTreeMap::new();
        for (key, entry) in object {
            if !entry.is_object() {
                return Err(PlannerError::InvalidValue {
                    reason: format!("calendar.{key} must be a calendar object"),
                }
                .into());
            }
            let calendar =
                Calendar::deserialize(entry).map_err(|e| PlannerError::InvalidValue {
                    reason: format!("calendar.{key}: {e}"),
                })?;
            entries.insert(key.clone(), calendar);
        }
        Ok(Self::Map(CalendarMap::new(entries)?))
    }

    /// The per-resource map this argument means on the resource clock: a
    /// single calendar `C` is `{"default": C}`.
    pub fn to_map(&self) -> CalendarMap {
        match self {
            Self::Single(c) => CalendarMap::uniform(c.clone()),
            Self::Map(m) => m.clone(),
        }
    }
}

impl Serialize for CalendarSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Single(c) => c.serialize(s),
            Self::Map(m) => m.serialize(s),
        }
    }
}

impl<'de> Deserialize<'de> for CalendarSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(d)?;
        Self::parse(&value).map_err(serde::de::Error::custom)
    }
}

/// Calendar placement for `plan.schedule`, `plan.simulate` and Monte Carlo:
/// the `calendar` argument and the plan start hour 0 refers to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduleCalendar {
    pub calendar: CalendarSpec,
    pub start: DateTime<Utc>,
}

impl ScheduleCalendar {
    pub fn new(calendar: CalendarSpec, start: DateTime<Utc>) -> Self {
        Self { calendar, start }
    }

    /// The instant `hours` wall-clock hours after the plan start, to the
    /// millisecond.
    pub fn instant(&self, hours: f64) -> DateTime<Utc> {
        instant_after(self.start, hours)
    }
}

/// The instant `hours` wall-clock hours after `start`, to the millisecond.
pub fn instant_after(start: DateTime<Utc>, hours: f64) -> DateTime<Utc> {
    start + Duration::milliseconds((hours * 3_600_000.0).round() as i64)
}

/// Open time of one deliverable, anchored at a plan start. Times passed to
/// and returned by its methods are wall-clock hours from that start.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkWindows {
    /// Weekly open intervals in UTC hours from Monday 00:00; empty when
    /// always open.
    weekly: Vec<(f64, f64)>,
    /// Open hours per week.
    per_week: f64,
    /// Hours from the Monday 00:00 UTC before the plan start to the start.
    anchor: f64,
}

impl WorkWindows {
    /// Always open: the four functions are plain arithmetic.
    pub fn always_open() -> Self {
        Self {
            weekly: Vec::new(),
            per_week: WEEK,
            anchor: 0.0,
        }
    }

    /// The windows of one calendar anchored at `start`. Assumes a
    /// validated calendar.
    pub fn of(calendar: &Calendar, start: DateTime<Utc>) -> Self {
        Self::from_weekly(calendar.weekly_intervals(), start)
            .expect("INVARIANT: a validated calendar has open time every week")
    }

    fn from_weekly(weekly: Vec<(f64, f64)>, start: DateTime<Utc>) -> Option<Self> {
        let per_week: f64 = weekly.iter().map(|(a, b)| b - a).sum();
        if per_week <= TOL {
            return None;
        }
        if per_week >= WEEK - TOL {
            return Some(Self::always_open());
        }
        let anchor = f64::from(start.weekday().num_days_from_monday()) * 24.0
            + f64::from(start.num_seconds_from_midnight()) / 3600.0
            + f64::from(start.nanosecond()) / 3.6e12;
        Some(Self {
            weekly,
            per_week,
            anchor,
        })
    }

    /// Whether the windows are open around the clock.
    pub fn is_always_open(&self) -> bool {
        self.weekly.is_empty()
    }

    /// Open hours per week.
    pub fn open_hours_per_week(&self) -> f64 {
        self.per_week
    }

    /// Split an absolute hour into (week start, offset within the week).
    fn split(x: f64) -> (f64, f64) {
        let w = (x / WEEK).floor() * WEEK;
        (w, x - w)
    }

    /// Working hours in `[0, x)` of absolute time.
    fn cumulative(&self, x: f64) -> f64 {
        let (w, r) = Self::split(x);
        let within: f64 = self
            .weekly
            .iter()
            .map(|&(a, b)| (r.min(b) - a).max(0.0))
            .sum();
        w / WEEK * self.per_week + within
    }

    /// The earliest instant `>= t` at which the windows are open. An
    /// instant exactly at a close is not open; exactly at an open is.
    pub fn open_at(&self, t: f64) -> f64 {
        if self.is_always_open() {
            return t;
        }
        let (w, r) = Self::split(self.anchor + t);
        let x = match self.weekly.iter().find(|&&(_, b)| b > r + TOL) {
            Some(&(a, _)) if a <= r => return t,
            Some(&(a, _)) => w + a,
            None => w + WEEK + self.weekly[0].0,
        };
        x - self.anchor
    }

    /// The close of the window open at `t` (open per [`Self::open_at`]),
    /// following touching windows across the week boundary.
    fn close_of(&self, t: f64) -> f64 {
        let (w, r) = Self::split(self.anchor + t);
        let &(_, b) = self
            .weekly
            .iter()
            .find(|&&(a, b)| a <= r + TOL && b > r + TOL)
            .expect("INVARIANT: close_of is called at an open instant");
        let mut close = w + b;
        if (b - WEEK).abs() <= TOL && self.weekly[0].0 <= TOL {
            close += self.weekly[0].1;
        }
        close - self.anchor
    }

    /// The earliest `f >= s` with `working_hours(s, f) = len`; `s` for
    /// `len <= 0`. Work pauses at each close and resumes at the next open,
    /// so work ending exactly at a close finishes at the close.
    pub fn advance(&self, s: f64, len: f64) -> f64 {
        if len <= 0.0 {
            return s;
        }
        if self.is_always_open() {
            return s + len;
        }
        let mut cur = self.open_at(s);
        let mut left = len;
        if left > self.per_week {
            let weeks = (left / self.per_week).ceil() - 1.0;
            left -= weeks * self.per_week;
            cur = self.open_at(cur + weeks * WEEK);
        }
        loop {
            let close = self.close_of(cur);
            if left <= close - cur + TOL {
                return cur + left;
            }
            left -= close - cur;
            cur = self.open_at(close);
        }
    }

    /// The length of `[a, b)` inside the windows; 0 when `b <= a`.
    pub fn working_hours(&self, a: f64, b: f64) -> f64 {
        if b <= a {
            return 0.0;
        }
        if self.is_always_open() {
            return b - a;
        }
        self.cumulative(self.anchor + b) - self.cumulative(self.anchor + a)
    }

    /// The latest `s <= f` with `working_hours(s, f) = len`; `f` for
    /// `len <= 0`. The backward-pass mirror of [`Self::advance`].
    pub fn retreat(&self, f: f64, len: f64) -> f64 {
        if len <= 0.0 {
            return f;
        }
        if self.is_always_open() {
            return f - len;
        }
        let mut cur = self.close_before(f);
        let mut left = len;
        if left > self.per_week {
            let weeks = (left / self.per_week).ceil() - 1.0;
            left -= weeks * self.per_week;
            cur = self.close_before(cur - weeks * WEEK);
        }
        loop {
            let open = self.open_of(cur);
            if left <= cur - open + TOL {
                return cur - left;
            }
            left -= cur - open;
            cur = self.close_before(open);
        }
    }

    /// The latest instant `<= t` that ends open time: `t` itself when the
    /// windows are open just before `t`, else the previous close.
    fn close_before(&self, t: f64) -> f64 {
        let (w, r) = Self::split(self.anchor + t);
        let x = match self.weekly.iter().rev().find(|&&(a, _)| a < r - TOL) {
            Some(&(_, b)) if b >= r => return t,
            Some(&(_, b)) => w + b,
            None => w - WEEK + self.weekly[self.weekly.len() - 1].1,
        };
        x - self.anchor
    }

    /// The open of the window that is open just before `t` (per
    /// [`Self::close_before`]), following touching windows back across the
    /// week boundary.
    fn open_of(&self, t: f64) -> f64 {
        let (w, r) = Self::split(self.anchor + t);
        let (w, r) = if r <= TOL { (w - WEEK, WEEK) } else { (w, r) };
        let &(a, _) = self
            .weekly
            .iter()
            .find(|&&(a, b)| a < r - TOL && b >= r - TOL)
            .expect("INVARIANT: open_of is called just after open time");
        let mut open = w + a;
        let last = self.weekly[self.weekly.len() - 1];
        if a <= TOL && (last.1 - WEEK).abs() <= TOL {
            open -= WEEK - last.0;
        }
        open - self.anchor
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> Calendar {
        Calendar {
            day_start: "09:00".into(),
            utc_offset_minutes: -300,
            ..Calendar::default()
        }
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("rfc3339")
    }

    #[test]
    fn day_start_accepts_last_minute_of_day() {
        assert_eq!(parse_day_start("23:59"), Some(23 * 60 + 59));
    }

    #[test]
    fn day_start_rejects_single_digit_hour() {
        assert_eq!(parse_day_start("9:00"), None);
    }

    #[test]
    fn day_start_rejects_twenty_four() {
        assert_eq!(parse_day_start("24:00"), None);
    }

    #[test]
    fn owner_calendar_opens_at_fourteen_utc_on_weekdays() {
        assert_eq!(owner().weekly_intervals()[0], (14.0, 22.0));
    }

    #[test]
    fn sunday_night_window_wraps_into_monday() {
        let c = Calendar {
            day_start: "22:00".into(),
            workdays: vec!["sun".into()],
            ..Calendar::default()
        };
        assert_eq!(c.weekly_intervals(), vec![(0.0, 6.0), (166.0, 168.0)]);
    }

    #[test]
    fn advance_crosses_touching_windows_over_the_week_boundary() {
        let c = Calendar {
            day_start: "22:00".into(),
            workdays: vec!["sun".into()],
            ..Calendar::default()
        };
        // Sunday 22:00 UTC start; 8 hours run straight through to Monday 06:00.
        let w = WorkWindows::of(&c, at("2026-11-15T22:00:00Z"));
        assert_eq!(w.advance(0.0, 8.0), 8.0);
    }

    #[test]
    fn retreat_crosses_touching_windows_over_the_week_boundary() {
        let c = Calendar {
            day_start: "22:00".into(),
            workdays: vec!["sun".into()],
            ..Calendar::default()
        };
        let w = WorkWindows::of(&c, at("2026-11-15T22:00:00Z"));
        assert_eq!(w.retreat(8.0, 8.0), 0.0);
    }

    #[test]
    fn retreat_inverts_advance_across_many_weeks() {
        let w = WorkWindows::of(&owner(), at("2026-11-13T19:00:00Z"));
        let s = w.open_at(3.0);
        let f = w.advance(s, 123.5);
        assert!((w.retreat(f, 123.5) - s).abs() < 1e-6);
    }

    #[test]
    fn working_hours_of_advance_equals_length() {
        let w = WorkWindows::of(&owner(), at("2026-11-13T19:00:00Z"));
        let f = w.advance(0.0, 97.25);
        assert!((w.working_hours(0.0, f) - 97.25).abs() < 1e-6);
    }

    #[test]
    fn whole_week_skip_matches_window_walk() {
        let w = WorkWindows::of(&owner(), at("2026-11-13T19:00:00Z"));
        // 80 hours is exactly two working weeks: it must end at a Friday close,
        // not at the following open.
        let s = w.open_at(0.0);
        let mut walked = s;
        for _ in 0..10 {
            walked = w.advance(w.open_at(walked), 8.0);
        }
        assert_eq!(w.advance(s, 80.0), walked);
    }

    #[test]
    fn intersection_of_disjoint_calendars_has_no_windows() {
        let map = CalendarMap::new(BTreeMap::from([
            ("default".to_string(), owner()),
            (
                "night".to_string(),
                Calendar {
                    day_start: "18:00".into(),
                    hours_per_day: 4.0,
                    utc_offset_minutes: -300,
                    ..Calendar::default()
                },
            ),
        ]))
        .expect("map");
        let keys = vec!["default".to_string(), "night".to_string()];
        assert!(map.windows(&keys, at("2026-11-13T19:00:00Z")).is_none());
    }
}
