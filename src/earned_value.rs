//! Pure earned-value engine: PV / EV / AC and the derived SPI / CPI / EAC
//! family, computed from a frozen baseline, the current graph, statuses and
//! actuals. No I/O, no clocks: the caller supplies `as_of`.
//!
//! Costs are in hours multiplied by each deliverable's `metadata.cost_rate`
//! (finite, >= 0, default 1.0). Every division is guarded; an undefined ratio
//! becomes `None` plus an [`Undefined`] entry, never NaN or infinity.

use crate::estimator::EffortEstimator;
use crate::plan::{Deliverable, DeliverableStatus, FINISH_ID, PlanGraph, PlannerError, START_ID};
use crate::schedule::{compute_cpm, effort_basis};
use crate::task::CriticalPathResult;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Working-time calendar for the EV clock.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Calendar {
    /// Working hours per workday, in (0, 24]. A workday contributes these
    /// hours starting at 00:00 local time.
    #[serde(default = "default_hours_per_day")]
    pub hours_per_day: f32,
    /// Lowercase 3-letter weekday names (`"mon"`..`"sun"`).
    #[serde(default = "default_workdays")]
    pub workdays: Vec<String>,
    /// Local offset from UTC in minutes, in [-1440, 1440].
    #[serde(default)]
    pub utc_offset_minutes: i32,
}

fn default_hours_per_day() -> f32 {
    8.0
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
            workdays: default_workdays(),
            utc_offset_minutes: 0,
        }
    }
}

/// How partial progress on a deliverable converts to earned percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EarningRule {
    /// 100% only when Complete.
    #[default]
    ZeroHundred,
    /// 50% once started (or any percent reported), 100% when Complete.
    FiftyFifty,
    /// The reported percent, 100% when Complete.
    Weighted,
}

/// One frozen baseline row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BaselineRow {
    pub id: String,
    /// Baseline earliest start, hours from plan start.
    pub es: f32,
    /// Baseline earliest finish, hours from plan start.
    pub ef: f32,
    /// Budget = effort basis x cost rate.
    pub budget: f32,
}

/// A frozen schedule/budget baseline.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub number: u32,
    pub start: DateTime<Utc>,
    /// `None` means wall-clock hours.
    pub calendar: Option<Calendar>,
    pub rows: Vec<BaselineRow>,
    pub bac: f32,
    /// Baseline project finish (`__finish__` earliest finish), in hours.
    pub finish_hours: f32,
}

/// Reported progress for one deliverable.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Actuals {
    pub earned_pct: Option<u8>,
    pub actual_hours: Option<f32>,
    pub leased_hours: f32,
    pub evidence: Vec<String>,
}

/// An EV field that could not be computed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Undefined {
    pub field: String,
    pub reason: String,
}

/// Per-deliverable EV row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvRow {
    pub id: String,
    pub budget: f32,
    pub pv: f32,
    pub earned_pct: f32,
    pub ev: f32,
    pub ac: f32,
    pub status: DeliverableStatus,
}

/// The ratios of an earlier snapshot, used for trend alerts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvSummary {
    pub spi: Option<f32>,
    pub cpi: Option<f32>,
}

/// Full earned-value report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvReport {
    pub as_of: DateTime<Utc>,
    pub baseline_number: u32,
    pub bac: f32,
    pub pv: f32,
    pub ev: f32,
    pub ac: f32,
    pub sv: f32,
    pub cv: f32,
    pub spi: Option<f32>,
    pub cpi: Option<f32>,
    pub eac: Option<f32>,
    pub etc: Option<f32>,
    pub vac: Option<f32>,
    pub tcpi: Option<f32>,
    pub undefined: Vec<Undefined>,
    pub rows: Vec<EvRow>,
    pub critical_float_consumed_hours: f32,
    pub alerts: Vec<String>,
}

fn weekday_from_name(name: &str) -> Option<Weekday> {
    Some(match name {
        "mon" => Weekday::Mon,
        "tue" => Weekday::Tue,
        "wed" => Weekday::Wed,
        "thu" => Weekday::Thu,
        "fri" => Weekday::Fri,
        "sat" => Weekday::Sat,
        "sun" => Weekday::Sun,
        _ => return None,
    })
}

fn workdays_per_week(days: &[Weekday]) -> usize {
    let mut uniq = days.to_vec();
    uniq.sort_by_key(|d| d.num_days_from_monday());
    uniq.dedup();
    uniq.len()
}

/// `metadata.cost_rate`: finite and >= 0, default 1.0.
fn cost_rate(d: &Deliverable) -> Result<f32, PlannerError> {
    match d.metadata.get("cost_rate") {
        None | Some(serde_json::Value::Null) => Ok(1.0),
        Some(v) => match v.as_f64() {
            Some(x) if x.is_finite() && x >= 0.0 && x <= f64::from(f32::MAX) => Ok(x as f32),
            _ => Err(PlannerError::InvalidGraph {
                reason: format!(
                    "deliverable {:?}: metadata.cost_rate must be a finite number >= 0, got {v}",
                    d.id
                ),
            }),
        },
    }
}

fn budget_of(d: &Deliverable, estimator: &EffortEstimator) -> Result<f32, PlannerError> {
    let budget = effort_basis(d, estimator) * cost_rate(d)?;
    if budget.is_finite() {
        Ok(budget)
    } else {
        Err(PlannerError::InvalidGraph {
            reason: format!("deliverable {:?}: budget is not finite", d.id),
        })
    }
}

/// Project finish in hours: the `__finish__` earliest finish, else the maximum.
fn finish_of(cpm: &CriticalPathResult) -> f32 {
    cpm.tasks
        .iter()
        .find(|t| t.id == FINISH_ID)
        .map(|t| t.earliest_finish)
        .unwrap_or_else(|| {
            cpm.tasks
                .iter()
                .map(|t| t.earliest_finish)
                .fold(0.0, f32::max)
        })
}

fn earned_pct(rule: EarningRule, status: &DeliverableStatus, reported: Option<u8>) -> f64 {
    if *status == DeliverableStatus::Complete {
        return 100.0;
    }
    match rule {
        EarningRule::ZeroHundred => 0.0,
        EarningRule::FiftyFifty => {
            if *status == DeliverableStatus::InProgress || reported.is_some() {
                50.0
            } else {
                0.0
            }
        }
        EarningRule::Weighted => f64::from(reported.unwrap_or(0)),
    }
}

impl Calendar {
    /// Reject an unusable calendar with a clear reason.
    pub fn validate(&self) -> Result<(), PlannerError> {
        let bad = |reason: String| Err(PlannerError::InvalidGraph { reason });
        if !(self.hours_per_day.is_finite()
            && self.hours_per_day > 0.0
            && self.hours_per_day <= 24.0)
        {
            return bad(format!(
                "calendar.hours_per_day must be in (0, 24], got {}",
                self.hours_per_day
            ));
        }
        if !(-1440..=1440).contains(&self.utc_offset_minutes) {
            return bad(format!(
                "calendar.utc_offset_minutes must be in [-1440, 1440], got {}",
                self.utc_offset_minutes
            ));
        }
        if self.workdays.is_empty() {
            return bad("calendar.workdays must not be empty".to_string());
        }
        for name in &self.workdays {
            if weekday_from_name(name).is_none() {
                return bad(format!(
                    "calendar.workdays entry {name:?} is not a lowercase 3-letter weekday (mon..sun)"
                ));
            }
        }
        Ok(())
    }
}

/// Elapsed EV-clock hours between `start` and `as_of`.
pub fn elapsed_hours(
    start: DateTime<Utc>,
    as_of: DateTime<Utc>,
    calendar: Option<&Calendar>,
) -> Result<f32, PlannerError> {
    if as_of <= start {
        return Ok(0.0);
    }
    let Some(calendar) = calendar else {
        return Ok((as_of - start).num_milliseconds() as f32 / 3_600_000.0);
    };
    calendar.validate()?;
    let off = Duration::minutes(i64::from(calendar.utc_offset_minutes));
    let (ls, le) = (start.naive_utc() + off, as_of.naive_utc() + off);
    let workdays: Vec<Weekday> = calendar
        .workdays
        .iter()
        .filter_map(|n| weekday_from_name(n))
        .collect();
    let hpd = f64::from(calendar.hours_per_day);
    let is_work = |d: NaiveDate| workdays.contains(&d.weekday());
    // Working hours inside [from, to] on local day `d` (window is [00:00, 00:00 + hpd)).
    let partial = |d: NaiveDate, from: chrono::NaiveDateTime, to: chrono::NaiveDateTime| -> f64 {
        if !is_work(d) {
            return 0.0;
        }
        let day0 = d.and_hms_opt(0, 0, 0).unwrap_or(from);
        let lo = (from - day0).num_milliseconds() as f64 / 3_600_000.0;
        let hi = (to - day0).num_milliseconds() as f64 / 3_600_000.0;
        (hi.min(hpd) - lo.max(0.0)).max(0.0)
    };
    let (sd, ed) = (ls.date(), le.date());
    let total = if sd == ed {
        partial(sd, ls, le)
    } else {
        let mut t = partial(sd, ls, le) + partial(ed, ls, le);
        // Interior days (strictly between) are full days; skip whole weeks arithmetically.
        let interior = (ed - sd).num_days() - 1;
        let weeks = interior / 7;
        let per_week = workdays_per_week(&workdays) as f64;
        t += weeks as f64 * per_week * hpd;
        let mut d = sd + Duration::days(1 + weeks * 7);
        while d < ed {
            if is_work(d) {
                t += hpd;
            }
            d += Duration::days(1);
        }
        t
    };
    Ok(total as f32)
}

/// Freeze the current CPM schedule and budgets into a baseline.
pub fn build_baseline(
    graph: &PlanGraph,
    cpm: &CriticalPathResult,
    start: DateTime<Utc>,
    calendar: Option<Calendar>,
    number: u32,
) -> Result<Baseline, PlannerError> {
    if let Some(c) = &calendar {
        c.validate()?;
    }
    let by_id: HashMap<&str, &crate::task::Task> =
        cpm.tasks.iter().map(|t| (t.id.as_str(), t)).collect();
    let estimator = EffortEstimator::new();
    let mut rows = Vec::new();
    for d in &graph.deliverables {
        if d.id == START_ID || d.id == FINISH_ID {
            continue;
        }
        let Some(t) = by_id.get(d.id.as_str()) else {
            return Err(PlannerError::InvalidGraph {
                reason: format!("deliverable {:?} missing from CPM result", d.id),
            });
        };
        rows.push(BaselineRow {
            id: d.id.clone(),
            es: t.earliest_start,
            ef: t.earliest_finish,
            budget: budget_of(d, &estimator)?,
        });
    }
    let bac = rows.iter().map(|r| f64::from(r.budget)).sum::<f64>() as f32;
    Ok(Baseline {
        number,
        start,
        calendar,
        rows,
        bac,
        finish_hours: finish_of(cpm),
    })
}

/// Compute the earned-value report.
///
/// Critical-float consumption is approximated as
/// `max(0, current __finish__ EF - baseline finish)`.
/// Alerts need the two most recent entries of `previous_snapshots`
/// (chronological) to both have the metric below 0.9.
#[allow(clippy::too_many_arguments)]
pub fn compute_ev(
    baseline: &Baseline,
    graph: &PlanGraph,
    statuses: &HashMap<String, DeliverableStatus>,
    rules: &HashMap<String, EarningRule>,
    actuals: &HashMap<String, Actuals>,
    as_of: DateTime<Utc>,
    previous_snapshots: &[EvSummary],
) -> Result<EvReport, PlannerError> {
    let t = f64::from(elapsed_hours(
        baseline.start,
        as_of,
        baseline.calendar.as_ref(),
    )?);
    let by_id: HashMap<&str, &Deliverable> = graph
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();
    let (mut pv, mut ev, mut ac) = (0.0_f64, 0.0_f64, 0.0_f64);
    let mut rows = Vec::with_capacity(baseline.rows.len());
    for r in &baseline.rows {
        let budget = f64::from(r.budget);
        let (es, ef) = (f64::from(r.es), f64::from(r.ef));
        let row_pv = if ef <= es {
            if t >= es { budget } else { 0.0 }
        } else {
            budget * ((t - es) / (ef - es)).clamp(0.0, 1.0)
        };
        let status = statuses
            .get(&r.id)
            .cloned()
            .unwrap_or(DeliverableStatus::Pending);
        let act = actuals.get(&r.id);
        let rule = rules.get(&r.id).copied().unwrap_or_default();
        let reported = act.and_then(|a| a.earned_pct).map(|p| p.min(100));
        let pct = earned_pct(rule, &status, reported);
        let rate = match by_id.get(r.id.as_str()) {
            Some(d) => cost_rate(d)?,
            None => 1.0,
        };
        let hours = act
            .map(|a| a.actual_hours.unwrap_or(a.leased_hours))
            .filter(|h| h.is_finite() && *h >= 0.0)
            .unwrap_or(0.0);
        let row_ev = budget * pct / 100.0;
        let row_ac = f64::from(rate) * f64::from(hours);
        pv += row_pv;
        ev += row_ev;
        ac += row_ac;
        rows.push(EvRow {
            id: r.id.clone(),
            budget: r.budget,
            pv: row_pv as f32,
            earned_pct: pct as f32,
            ev: row_ev as f32,
            ac: row_ac as f32,
            status,
        });
    }
    let bac = f64::from(baseline.bac);
    let mut undefined = Vec::new();
    let mut ratio = |field: &str, v: Option<f64>, reason: &str| -> Option<f32> {
        match v.filter(|x| x.is_finite()) {
            Some(x) => Some(x as f32),
            None => {
                undefined.push(Undefined {
                    field: field.to_string(),
                    reason: reason.to_string(),
                });
                None
            }
        }
    };
    let spi = ratio(
        "spi",
        (pv != 0.0).then(|| ev / pv),
        "PV is 0 before any work was planned",
    );
    let cpi = ratio(
        "cpi",
        (ac != 0.0).then(|| ev / ac),
        "AC is 0, no cost recorded yet",
    );
    let eac_v = match cpi {
        Some(c) if c != 0.0 => Some(bac / f64::from(c)),
        _ => None,
    };
    let eac_reason = if cpi.is_none() {
        "CPI is undefined"
    } else {
        "CPI is 0, nothing earned for the cost spent"
    };
    let eac = ratio("eac", eac_v, eac_reason);
    let etc = ratio("etc", eac.map(|e| f64::from(e) - ac), "EAC is undefined");
    let vac = ratio("vac", eac.map(|e| bac - f64::from(e)), "EAC is undefined");
    let tcpi = ratio(
        "tcpi",
        (bac - ac != 0.0).then(|| (bac - ev) / (bac - ac)),
        "BAC - AC is 0, no budget remains",
    );

    let cpm = compute_cpm(graph)?;
    let critical_float = (finish_of(&cpm) - baseline.finish_hours).max(0.0);

    let mut alerts = Vec::new();
    if let [.., a, b] = previous_snapshots {
        let low = |v: Option<f32>| v.is_some_and(|x| x < 0.9);
        if low(a.spi) && low(b.spi) {
            alerts.push("SPI_BELOW_0_9".to_string());
        }
        if low(a.cpi) && low(b.cpi) {
            alerts.push("CPI_BELOW_0_9".to_string());
        }
    }

    Ok(EvReport {
        as_of,
        baseline_number: baseline.number,
        bac: baseline.bac,
        pv: pv as f32,
        ev: ev as f32,
        ac: ac as f32,
        sv: (ev - pv) as f32,
        cv: (ev - ac) as f32,
        spi,
        cpi,
        eac,
        etc,
        vac,
        tcpi,
        undefined,
        rows,
        critical_float_consumed_hours: critical_float,
        alerts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    const EPS: f32 = 1e-3;

    fn at(y: i32, m: u32, d: u32, h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, h, 0, 0).unwrap()
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < EPS
    }

    fn deliverable(id: &str, effort: f32, prereqs: &[&str]) -> Deliverable {
        Deliverable {
            id: id.to_string(),
            owned_files: vec![],
            prerequisites: prereqs
                .iter()
                .map(|p| crate::plan::Prerequisite::Id((*p).to_string()))
                .collect(),
            estimated_effort_hours: Some(effort),
            duration_hours: None,
            estimate: None,
            metadata: serde_json::Value::Null,
            milestone: false,
        }
    }

    fn graph(ds: Vec<Deliverable>) -> PlanGraph {
        PlanGraph {
            deliverables: ds,
            max_chained_dispatch: None,
        }
    }

    fn row(id: &str, es: f32, ef: f32, budget: f32) -> BaselineRow {
        BaselineRow {
            id: id.to_string(),
            es,
            ef,
            budget,
        }
    }

    fn baseline(rows: Vec<BaselineRow>) -> Baseline {
        let bac = rows.iter().map(|r| r.budget).sum();
        let finish_hours = rows.iter().map(|r| r.ef).fold(0.0, f32::max);
        Baseline {
            number: 1,
            start: at(2026, 1, 5, 0),
            calendar: None,
            rows,
            bac,
            finish_hours,
        }
    }

    fn statuses(pairs: &[(&str, DeliverableStatus)]) -> HashMap<String, DeliverableStatus> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    fn report(
        b: &Baseline,
        g: &PlanGraph,
        st: &HashMap<String, DeliverableStatus>,
        rules: &HashMap<String, EarningRule>,
        actuals: &HashMap<String, Actuals>,
        hours: i64,
    ) -> EvReport {
        let as_of = b.start + Duration::hours(hours);
        compute_ev(b, g, st, rules, actuals, as_of, &[]).unwrap()
    }

    fn one(rule: EarningRule, status: DeliverableStatus, pct: Option<u8>) -> EvReport {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let st = statuses(&[("a", status)]);
        let rules = HashMap::from([("a".to_string(), rule)]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                earned_pct: pct,
                ..Actuals::default()
            },
        )]);
        report(&b, &g, &st, &rules, &actuals, 5)
    }

    /// BAC 100 (two rows), PV 50, EV 40, AC 48.
    fn textbook() -> EvReport {
        let b = baseline(vec![row("a", 0.0, 10.0, 50.0), row("b", 10.0, 20.0, 50.0)]);
        let g = graph(vec![
            deliverable("a", 50.0, &[]),
            deliverable("b", 50.0, &["a"]),
        ]);
        let st = statuses(&[("a", DeliverableStatus::InProgress)]);
        let rules = HashMap::from([("a".to_string(), EarningRule::Weighted)]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                earned_pct: Some(80),
                actual_hours: Some(48.0),
                ..Actuals::default()
            },
        )]);
        report(&b, &g, &st, &rules, &actuals, 10)
    }

    #[test]
    fn wall_clock_elapsed_is_plain_hours() {
        let h = elapsed_hours(at(2026, 1, 5, 0), at(2026, 1, 6, 6), None).unwrap();
        assert!(near(h, 30.0));
    }

    #[test]
    fn elapsed_before_start_is_zero() {
        let h = elapsed_hours(at(2026, 1, 5, 0), at(2026, 1, 4, 0), None).unwrap();
        assert!(near(h, 0.0));
    }

    #[test]
    fn calendar_skips_weekends() {
        // Fri 2026-01-09 00:00 to Mon 2026-01-12 00:00: only Friday's 8h.
        let c = Calendar::default();
        let h = elapsed_hours(at(2026, 1, 9, 0), at(2026, 1, 12, 0), Some(&c)).unwrap();
        assert!(near(h, 8.0));
    }

    #[test]
    fn calendar_counts_partial_day() {
        // Monday 03:00 to 05:30 is 2.5 working hours.
        let c = Calendar::default();
        let start = at(2026, 1, 5, 3);
        let h = elapsed_hours(start, start + Duration::minutes(150), Some(&c)).unwrap();
        assert!(near(h, 2.5));
    }

    #[test]
    fn calendar_ignores_hours_after_the_working_window() {
        let c = Calendar::default();
        let h = elapsed_hours(at(2026, 1, 5, 0), at(2026, 1, 5, 20), Some(&c)).unwrap();
        assert!(near(h, 8.0));
    }

    #[test]
    fn calendar_over_many_weeks_counts_whole_weeks() {
        // 10 weeks from a Monday: 10 * 5 * 8 working hours.
        let c = Calendar::default();
        let h = elapsed_hours(at(2026, 1, 5, 0), at(2026, 3, 16, 0), Some(&c)).unwrap();
        assert!(near(h, 400.0));
    }

    #[test]
    fn calendar_utc_offset_shifts_the_local_day() {
        // +120 min: Mon 2026-01-05 22:00 UTC is Tue 00:00 local, so 2h later is 2 work hours.
        let c = Calendar {
            utc_offset_minutes: 120,
            ..Calendar::default()
        };
        let h = elapsed_hours(at(2026, 1, 5, 22), at(2026, 1, 6, 0), Some(&c)).unwrap();
        assert!(near(h, 2.0));
    }

    #[test]
    fn calendar_custom_workdays_count_only_listed_days() {
        let c = Calendar {
            workdays: vec!["sat".into()],
            ..Calendar::default()
        };
        let h = elapsed_hours(at(2026, 1, 5, 0), at(2026, 1, 12, 0), Some(&c)).unwrap();
        assert!(near(h, 8.0));
    }

    fn invalid(c: Calendar) -> bool {
        matches!(c.validate(), Err(PlannerError::InvalidGraph { .. }))
    }

    #[test]
    fn calendar_rejects_zero_hours_per_day() {
        assert!(invalid(Calendar {
            hours_per_day: 0.0,
            ..Calendar::default()
        }));
    }

    #[test]
    fn calendar_rejects_more_than_24_hours_per_day() {
        assert!(invalid(Calendar {
            hours_per_day: 24.5,
            ..Calendar::default()
        }));
    }

    #[test]
    fn calendar_rejects_nan_hours_per_day() {
        assert!(invalid(Calendar {
            hours_per_day: f32::NAN,
            ..Calendar::default()
        }));
    }

    #[test]
    fn calendar_rejects_out_of_range_offset() {
        assert!(invalid(Calendar {
            utc_offset_minutes: 1441,
            ..Calendar::default()
        }));
    }

    #[test]
    fn calendar_rejects_empty_workdays() {
        assert!(invalid(Calendar {
            workdays: vec![],
            ..Calendar::default()
        }));
    }

    #[test]
    fn calendar_rejects_unknown_weekday_name() {
        assert!(invalid(Calendar {
            workdays: vec!["monday".into()],
            ..Calendar::default()
        }));
    }

    #[test]
    fn calendar_accepts_a_24_hour_day() {
        assert!(
            Calendar {
                hours_per_day: 24.0,
                ..Calendar::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn pv_interpolates_linearly_inside_a_task() {
        let b = baseline(vec![row("a", 10.0, 20.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            15,
        );
        assert!(near(r.pv, 50.0));
    }

    #[test]
    fn pv_is_zero_before_task_start() {
        let b = baseline(vec![row("a", 10.0, 20.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 5);
        assert!(near(r.pv, 0.0));
    }

    #[test]
    fn pv_caps_at_budget_after_task_finish() {
        let b = baseline(vec![row("a", 10.0, 20.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            99,
        );
        assert!(near(r.pv, 100.0));
    }

    #[test]
    fn pv_of_zero_length_task_is_zero_before_its_start() {
        let b = baseline(vec![row("m", 10.0, 10.0, 5.0)]);
        let g = graph(vec![deliverable("m", 5.0, &[])]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 9);
        assert!(near(r.pv, 0.0));
    }

    #[test]
    fn pv_of_zero_length_task_steps_to_budget_at_its_start() {
        let b = baseline(vec![row("m", 10.0, 10.0, 5.0)]);
        let g = graph(vec![deliverable("m", 5.0, &[])]);
        let r = report(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            10,
        );
        assert!(near(r.pv, 5.0));
    }

    #[test]
    fn zero_hundred_earns_nothing_while_in_progress() {
        let r = one(
            EarningRule::ZeroHundred,
            DeliverableStatus::InProgress,
            Some(90),
        );
        assert!(near(r.ev, 0.0));
    }

    #[test]
    fn zero_hundred_earns_full_budget_when_complete() {
        let r = one(EarningRule::ZeroHundred, DeliverableStatus::Complete, None);
        assert!(near(r.ev, 100.0));
    }

    #[test]
    fn default_rule_is_zero_hundred_when_unmapped() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let st = statuses(&[("a", DeliverableStatus::InProgress)]);
        let r = report(&b, &g, &st, &HashMap::new(), &HashMap::new(), 5);
        assert!(near(r.ev, 0.0));
    }

    #[test]
    fn fifty_fifty_earns_half_when_in_progress() {
        let r = one(EarningRule::FiftyFifty, DeliverableStatus::InProgress, None);
        assert!(near(r.ev, 50.0));
    }

    #[test]
    fn fifty_fifty_earns_half_when_any_percent_reported() {
        let r = one(EarningRule::FiftyFifty, DeliverableStatus::Ready, Some(10));
        assert!(near(r.ev, 50.0));
    }

    #[test]
    fn fifty_fifty_earns_full_when_complete() {
        let r = one(EarningRule::FiftyFifty, DeliverableStatus::Complete, None);
        assert!(near(r.ev, 100.0));
    }

    #[test]
    fn weighted_earns_the_reported_percent() {
        let r = one(
            EarningRule::Weighted,
            DeliverableStatus::InProgress,
            Some(30),
        );
        assert!(near(r.ev, 30.0));
    }

    #[test]
    fn weighted_earns_nothing_without_a_report() {
        let r = one(EarningRule::Weighted, DeliverableStatus::InProgress, None);
        assert!(near(r.ev, 0.0));
    }

    #[test]
    fn weighted_ignores_reported_percent_once_complete() {
        let r = one(EarningRule::Weighted, DeliverableStatus::Complete, Some(40));
        assert!(near(r.ev, 100.0));
    }

    #[test]
    fn failed_deliverable_earns_nothing_under_zero_hundred() {
        let r = one(
            EarningRule::ZeroHundred,
            DeliverableStatus::Failed { reason: "x".into() },
            None,
        );
        assert!(near(r.ev, 0.0));
    }

    fn ac_for(actual: Option<f32>, leased: f32, rate: Option<f64>) -> f32 {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let mut d = deliverable("a", 100.0, &[]);
        if let Some(r) = rate {
            d.metadata = serde_json::json!({ "cost_rate": r });
        }
        let g = graph(vec![d]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: actual,
                leased_hours: leased,
                ..Actuals::default()
            },
        )]);
        report(&b, &g, &HashMap::new(), &HashMap::new(), &actuals, 5).ac
    }

    #[test]
    fn ac_prefers_reported_hours_over_leased() {
        assert!(near(ac_for(Some(7.0), 3.0, None), 7.0));
    }

    #[test]
    fn ac_falls_back_to_leased_hours() {
        assert!(near(ac_for(None, 3.0, None), 3.0));
    }

    #[test]
    fn ac_is_scaled_by_cost_rate() {
        assert!(near(ac_for(Some(4.0), 0.0, Some(2.5)), 10.0));
    }

    #[test]
    fn baseline_budget_is_effort_times_cost_rate() {
        let mut d = deliverable("a", 10.0, &[]);
        d.metadata = serde_json::json!({ "cost_rate": 3.0 });
        let g = graph(vec![d]);
        let cpm = compute_cpm(&g).unwrap();
        let b = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1).unwrap();
        assert!(near(b.bac, 30.0));
    }

    #[test]
    fn baseline_defaults_cost_rate_to_one() {
        let g = graph(vec![deliverable("a", 10.0, &[])]);
        let cpm = compute_cpm(&g).unwrap();
        let b = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1).unwrap();
        assert!(near(b.bac, 10.0));
    }

    #[test]
    fn baseline_excludes_synthetic_endpoints() {
        let g = graph(vec![deliverable("a", 10.0, &[])]);
        let cpm = compute_cpm(&g).unwrap();
        let b = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1).unwrap();
        assert_eq!(b.rows.len(), 1);
    }

    #[test]
    fn baseline_freezes_cpm_start_and_finish() {
        let g = graph(vec![
            deliverable("a", 10.0, &[]),
            deliverable("b", 5.0, &["a"]),
        ]);
        let cpm = compute_cpm(&g).unwrap();
        let b = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1).unwrap();
        let rb = b.rows.iter().find(|r| r.id == "b").unwrap();
        assert!(near(rb.es, 10.0) && near(rb.ef, 15.0));
    }

    #[test]
    fn baseline_records_finish_hours() {
        let g = graph(vec![
            deliverable("a", 10.0, &[]),
            deliverable("b", 5.0, &["a"]),
        ]);
        let cpm = compute_cpm(&g).unwrap();
        let b = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1).unwrap();
        assert!(near(b.finish_hours, 15.0));
    }

    #[test]
    fn baseline_rejects_negative_cost_rate() {
        let mut d = deliverable("a", 10.0, &[]);
        d.metadata = serde_json::json!({ "cost_rate": -1.0 });
        let g = graph(vec![d]);
        let cpm = compute_cpm(&g).unwrap();
        let e = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1);
        assert!(matches!(e, Err(PlannerError::InvalidGraph { .. })));
    }

    #[test]
    fn baseline_rejects_non_numeric_cost_rate() {
        let mut d = deliverable("a", 10.0, &[]);
        d.metadata = serde_json::json!({ "cost_rate": "free" });
        let g = graph(vec![d]);
        let cpm = compute_cpm(&g).unwrap();
        let e = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1);
        assert!(matches!(e, Err(PlannerError::InvalidGraph { .. })));
    }

    #[test]
    fn baseline_rejects_invalid_calendar() {
        let g = graph(vec![deliverable("a", 10.0, &[])]);
        let cpm = compute_cpm(&g).unwrap();
        let c = Calendar {
            hours_per_day: 0.0,
            ..Calendar::default()
        };
        let e = build_baseline(&g, &cpm, at(2026, 1, 5, 0), Some(c), 1);
        assert!(matches!(e, Err(PlannerError::InvalidGraph { .. })));
    }

    #[test]
    fn budget_in_ev_report_is_baseline_budget() {
        let r = textbook();
        assert!(near(r.bac, 100.0));
    }

    #[test]
    fn textbook_pv() {
        assert!(near(textbook().pv, 50.0));
    }

    #[test]
    fn textbook_ev() {
        assert!(near(textbook().ev, 40.0));
    }

    #[test]
    fn textbook_ac() {
        assert!(near(textbook().ac, 48.0));
    }

    #[test]
    fn textbook_sv() {
        assert!(near(textbook().sv, -10.0));
    }

    #[test]
    fn textbook_cv() {
        assert!(near(textbook().cv, -8.0));
    }

    #[test]
    fn textbook_spi() {
        assert!(near(textbook().spi.unwrap(), 0.8));
    }

    #[test]
    fn textbook_cpi() {
        assert!(near(textbook().cpi.unwrap(), 0.8333));
    }

    #[test]
    fn textbook_eac() {
        assert!(near(textbook().eac.unwrap(), 120.0));
    }

    #[test]
    fn textbook_etc() {
        assert!(near(textbook().etc.unwrap(), 72.0));
    }

    #[test]
    fn textbook_vac() {
        assert!(near(textbook().vac.unwrap(), -20.0));
    }

    #[test]
    fn textbook_tcpi() {
        assert!(near(textbook().tcpi.unwrap(), 60.0 / 52.0));
    }

    #[test]
    fn textbook_has_no_undefined_fields() {
        assert!(textbook().undefined.is_empty());
    }

    fn undefined_reason(r: &EvReport, field: &str) -> Option<String> {
        r.undefined
            .iter()
            .find(|u| u.field == field)
            .map(|u| u.reason.clone())
    }

    #[test]
    fn spi_is_null_when_pv_is_zero() {
        let b = baseline(vec![row("a", 10.0, 20.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 0);
        assert!(r.spi.is_none());
    }

    #[test]
    fn spi_zero_pv_reason_is_explained() {
        let b = baseline(vec![row("a", 10.0, 20.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 0);
        assert_eq!(
            undefined_reason(&r, "spi").as_deref(),
            Some("PV is 0 before any work was planned")
        );
    }

    #[test]
    fn cpi_is_null_when_ac_is_zero() {
        let r = one(EarningRule::ZeroHundred, DeliverableStatus::Complete, None);
        assert!(r.cpi.is_none());
    }

    #[test]
    fn cpi_zero_ac_reason_is_explained() {
        let r = one(EarningRule::ZeroHundred, DeliverableStatus::Complete, None);
        assert_eq!(
            undefined_reason(&r, "cpi").as_deref(),
            Some("AC is 0, no cost recorded yet")
        );
    }

    #[test]
    fn eac_is_null_when_cpi_is_null() {
        let r = one(EarningRule::ZeroHundred, DeliverableStatus::Complete, None);
        assert!(r.eac.is_none());
    }

    #[test]
    fn dependents_of_eac_are_null_and_explained() {
        let r = one(EarningRule::ZeroHundred, DeliverableStatus::Complete, None);
        assert!(r.etc.is_none() && r.vac.is_none());
        assert!(undefined_reason(&r, "etc").is_some() && undefined_reason(&r, "vac").is_some());
    }

    #[test]
    fn eac_is_null_when_cpi_is_zero() {
        // Spent 10h, earned nothing: CPI = 0, EAC would divide by zero.
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(10.0),
                ..Actuals::default()
            },
        )]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &actuals, 5);
        assert!(r.eac.is_none());
    }

    #[test]
    fn tcpi_is_null_when_bac_equals_ac() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(100.0),
                ..Actuals::default()
            },
        )]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &actuals, 5);
        assert!(r.tcpi.is_none());
    }

    #[test]
    fn tcpi_zero_denominator_reason_is_explained() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(100.0),
                ..Actuals::default()
            },
        )]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &actuals, 5);
        assert_eq!(
            undefined_reason(&r, "tcpi").as_deref(),
            Some("BAC - AC is 0, no budget remains")
        );
    }

    #[test]
    fn empty_baseline_yields_all_finite_values() {
        let b = baseline(vec![]);
        let g = graph(vec![]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 5);
        assert!(r.pv.is_finite() && r.ev.is_finite() && r.ac.is_finite());
    }

    #[test]
    fn rows_report_per_deliverable_ev() {
        let r = textbook();
        let a = r.rows.iter().find(|x| x.id == "a").unwrap();
        assert!(near(a.ev, 40.0));
    }

    #[test]
    fn rows_report_status() {
        let r = textbook();
        let a = r.rows.iter().find(|x| x.id == "a").unwrap();
        assert_eq!(a.status, DeliverableStatus::InProgress);
    }

    #[test]
    fn missing_status_defaults_to_pending() {
        let r = textbook();
        let b = r.rows.iter().find(|x| x.id == "b").unwrap();
        assert_eq!(b.status, DeliverableStatus::Pending);
    }

    #[test]
    fn critical_float_consumed_is_forecast_overrun() {
        // Baseline finish 5h, current CPM finish 12h.
        let mut b = baseline(vec![row("a", 0.0, 5.0, 12.0)]);
        b.finish_hours = 5.0;
        let g = graph(vec![deliverable("a", 12.0, &[])]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 1);
        assert!(near(r.critical_float_consumed_hours, 7.0));
    }

    #[test]
    fn critical_float_consumed_never_negative() {
        let mut b = baseline(vec![row("a", 0.0, 20.0, 12.0)]);
        b.finish_hours = 20.0;
        let g = graph(vec![deliverable("a", 12.0, &[])]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 1);
        assert!(near(r.critical_float_consumed_hours, 0.0));
    }

    fn snap(spi: f32, cpi: f32) -> EvSummary {
        EvSummary {
            spi: Some(spi),
            cpi: Some(cpi),
        }
    }

    fn alerts_with(prev: &[EvSummary]) -> Vec<String> {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let as_of = b.start + Duration::hours(5);
        compute_ev(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            as_of,
            prev,
        )
        .unwrap()
        .alerts
    }

    #[test]
    fn spi_alert_fires_when_two_latest_snapshots_are_below_threshold() {
        let a = alerts_with(&[snap(0.95, 1.0), snap(0.8, 1.0), snap(0.85, 1.0)]);
        assert_eq!(a, vec!["SPI_BELOW_0_9".to_string()]);
    }

    #[test]
    fn alert_does_not_fire_on_a_single_low_snapshot() {
        assert!(alerts_with(&[snap(0.95, 1.0), snap(0.8, 1.0)]).is_empty());
    }

    #[test]
    fn alert_does_not_fire_with_one_snapshot() {
        assert!(alerts_with(&[snap(0.5, 0.5)]).is_empty());
    }

    #[test]
    fn cpi_alert_fires_when_two_latest_snapshots_are_below_threshold() {
        let a = alerts_with(&[snap(1.0, 0.7), snap(1.0, 0.89)]);
        assert_eq!(a, vec!["CPI_BELOW_0_9".to_string()]);
    }

    #[test]
    fn alert_ignores_snapshots_with_undefined_metric() {
        let none = EvSummary {
            spi: None,
            cpi: None,
        };
        assert!(alerts_with(&[snap(0.5, 0.5), none]).is_empty());
    }

    #[test]
    fn reports_are_deterministic() {
        assert_eq!(textbook(), textbook());
    }
}
