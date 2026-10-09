//! Pure earned-value engine: PV / EV / AC and the derived SPI / CPI / EAC
//! family, computed from a frozen baseline, the current graph, statuses and
//! actuals. No I/O, no clocks: the caller supplies `as_of`.
//!
//! Costs are in hours multiplied by each deliverable's `metadata.cost_rate`
//! (finite, >= 0, default 1.0). Every division is guarded; an undefined ratio
//! becomes `None` plus an [`Undefined`] entry, never NaN or infinity.
//!
//! Rows are taken from the baseline only. Deliverables added after the
//! baseline contribute nothing to BAC, PV, EV or AC until re-baseline; they
//! are listed in [`EvReport::excluded_unbaselined`]. Cost rates are frozen in
//! the baseline, so later rate edits (or removed deliverables) do not change
//! budget or AC.

use crate::estimator::EffortEstimator;
pub use crate::plan::EarningRule;
use crate::plan::{Deliverable, DeliverableStatus, FINISH_ID, PlanGraph, PlannerError, START_ID};
use crate::schedule::{compute_cpm, effort_basis};
use crate::task::CriticalPathResult;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc, Weekday};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Working-time calendar for the EV clock. Unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
    /// Cost rate captured at baseline time; AC uses this, not the live graph.
    #[serde(default = "one")]
    pub cost_rate: f32,
}

fn one() -> f32 {
    1.0
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
    /// Ids in the live graph but absent from the baseline (sorted, synthetic
    /// endpoints excluded). They contribute nothing until re-baseline.
    pub excluded_unbaselined: Vec<String>,
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
            cost_rate: cost_rate(d)?,
        });
    }
    let bac = rows.iter().map(|r| f64::from(r.budget)).sum::<f64>() as f32;
    if !bac.is_finite() {
        return Err(PlannerError::InvalidGraph {
            reason: "budget at completion (sum of effort x cost_rate) exceeds the f32 range"
                .to_string(),
        });
    }
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
/// Rows are taken from the baseline only. Deliverables added after the baseline
/// contribute nothing to BAC, PV, EV or AC until re-baseline.
///
/// `critical_float_consumed_hours` reflects only graph/estimate changes: it
/// ignores statuses, actuals and `as_of`, so it does not show execution
/// slippage, and it includes scope added after the baseline.
///
/// Reported `actual_hours` that is non-finite or negative is treated as
/// absent (AC falls back to leased hours). TCPI can be negative when AC > BAC.
///
/// Alerts need the two most recent entries of `previous_snapshots`
/// (chronological) to both have the metric below 0.9.
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
        let rate = r.cost_rate;
        let valid = |h: f32| h.is_finite() && h >= 0.0;
        let hours = act
            .map(|a| match a.actual_hours {
                Some(h) if valid(h) => h,
                _ => a.leased_hours,
            })
            .filter(|h| valid(*h))
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
    let bac: f64 = baseline.rows.iter().map(|r| f64::from(r.budget)).sum();
    let mut undefined = Vec::new();
    // `None` input = guarded division; a value that overflows f32 is also undefined.
    let mut ratio = |field: &str, v: Option<f64>, reason: &str| -> Option<f64> {
        let out = v.filter(|x| (*x as f32).is_finite());
        if out.is_none() {
            let reason = if v.is_some() {
                "result is not finite as f32"
            } else {
                reason
            };
            undefined.push(Undefined {
                field: field.to_string(),
                reason: reason.to_string(),
            });
        }
        out
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
    let eac_reason = if cpi.is_none() {
        "CPI is undefined"
    } else {
        "CPI is 0, nothing earned for the cost spent"
    };
    let eac = ratio(
        "eac",
        cpi.filter(|c| *c != 0.0).map(|c| bac / c),
        eac_reason,
    );
    let etc = ratio("etc", eac.map(|e| e - ac), "EAC is undefined");
    let vac = ratio("vac", eac.map(|e| bac - e), "EAC is undefined");
    let tcpi = ratio(
        "tcpi",
        ((bac - ac).abs() > 1e-9 * bac.max(1.0)).then(|| (bac - ev) / (bac - ac)),
        "BAC - AC is 0, no budget remains",
    );
    let out = |v: Option<f64>| v.map(|x| x as f32);

    let cpm = compute_cpm(graph)?;
    let critical_float = (finish_of(&cpm) - baseline.finish_hours).max(0.0);

    let alerts = trend_alerts(previous_snapshots);
    for (field, v) in [
        ("pv", pv),
        ("ev", ev),
        ("ac", ac),
        ("sv", ev - pv),
        ("cv", ev - ac),
    ] {
        if !(v as f32).is_finite() {
            return Err(PlannerError::InvalidGraph {
                reason: format!(
                    "earned-value total {field} exceeds the f32 range (check cost_rate and \
                     reported hours)"
                ),
            });
        }
    }

    let report = EvReport {
        as_of,
        baseline_number: baseline.number,
        bac: baseline.bac,
        pv: pv as f32,
        ev: ev as f32,
        ac: ac as f32,
        sv: (ev - pv) as f32,
        cv: (ev - ac) as f32,
        spi: out(spi),
        cpi: out(cpi),
        eac: out(eac),
        etc: out(etc),
        vac: out(vac),
        tcpi: out(tcpi),
        undefined,
        rows,
        critical_float_consumed_hours: critical_float,
        alerts,
        excluded_unbaselined: {
            let known: std::collections::HashSet<&str> =
                baseline.rows.iter().map(|r| r.id.as_str()).collect();
            let mut v: Vec<String> = graph
                .deliverables
                .iter()
                .filter(|d| d.id != START_ID && d.id != FINISH_ID && !known.contains(d.id.as_str()))
                .map(|d| d.id.clone())
                .collect();
            v.sort();
            v
        },
    };
    Ok(report)
}

/// Trend alerts over chronological snapshot ratios: `SPI_BELOW_0_9` /
/// `CPI_BELOW_0_9` when the metric is defined and below 0.9 on both of the
/// two most recent entries. Fewer than two entries raise nothing.
pub fn trend_alerts(snapshots: &[EvSummary]) -> Vec<String> {
    let mut alerts = Vec::new();
    if let [.., a, b] = snapshots {
        let low = |v: Option<f32>| v.is_some_and(|x| x < 0.9);
        if low(a.spi) && low(b.spi) {
            alerts.push("SPI_BELOW_0_9".to_string());
        }
        if low(a.cpi) && low(b.cpi) {
            alerts.push("CPI_BELOW_0_9".to_string());
        }
    }
    alerts
}

/// The stored summary of one EV snapshot: the report's totals and ratios
/// (an undefined ratio stays `None`, serialised as `null`) and the alerts
/// raised when it was taken.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub taken_at: DateTime<Utc>,
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
    pub alerts: Vec<String>,
}

impl SnapshotSummary {
    /// Summarise `report`, taken at `taken_at`.
    pub fn from_report(report: &EvReport, taken_at: DateTime<Utc>) -> Self {
        Self {
            taken_at,
            as_of: report.as_of,
            baseline_number: report.baseline_number,
            bac: report.bac,
            pv: report.pv,
            ev: report.ev,
            ac: report.ac,
            sv: report.sv,
            cv: report.cv,
            spi: report.spi,
            cpi: report.cpi,
            eac: report.eac,
            etc: report.etc,
            vac: report.vac,
            tcpi: report.tcpi,
            alerts: report.alerts.clone(),
        }
    }

    /// The ratios trend alerts look at.
    pub fn ratios(&self) -> EvSummary {
        EvSummary {
            spi: self.spi,
            cpi: self.cpi,
        }
    }
}

/// Render snapshots (in the given order) as a Markdown table with columns
/// date (the `as_of` instant, UTC, to the minute), PV, EV, AC, SPI, CPI
/// and EAC. Values have two decimals; an undefined ratio is `n/a`.
pub fn render_snapshots_markdown(snapshots: &[SnapshotSummary]) -> String {
    let ratio = |v: Option<f32>| v.map_or_else(|| "n/a".to_string(), |x| format!("{x:.2}"));
    let mut out = String::from(
        "| date | PV | EV | AC | SPI | CPI | EAC |\n|---|---:|---:|---:|---:|---:|---:|\n",
    );
    for s in snapshots {
        out.push_str(&format!(
            "| {} | {:.2} | {:.2} | {:.2} | {} | {} | {} |\n",
            s.as_of.format("%Y-%m-%dT%H:%MZ"),
            s.pv,
            s.ev,
            s.ac,
            ratio(s.spi),
            ratio(s.cpi),
            ratio(s.eac),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

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
            earning_rule: None,
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
            cost_rate: 1.0,
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
        let mut br = row("a", 0.0, 10.0, 100.0);
        br.cost_rate = rate.unwrap_or(1.0) as f32;
        let b = baseline(vec![br]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
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
    fn report_is_independent_of_map_insertion_order() {
        let b = baseline(vec![row("a", 0.0, 10.0, 50.0), row("b", 0.0, 10.0, 50.0)]);
        let g = graph(vec![
            deliverable("a", 50.0, &[]),
            deliverable("b", 50.0, &[]),
        ]);
        let act = |h: f32| Actuals {
            actual_hours: Some(h),
            ..Actuals::default()
        };
        let fwd = HashMap::from([("a".to_string(), act(3.0)), ("b".to_string(), act(4.0))]);
        let mut rev = HashMap::new();
        rev.insert("b".to_string(), act(4.0));
        rev.insert("a".to_string(), act(3.0));
        let e = HashMap::new();
        assert_eq!(
            report(&b, &g, &e, &e2(), &fwd, 5),
            report(&b, &g, &e, &e2(), &rev, 5)
        );
    }

    fn e2() -> HashMap<String, EarningRule> {
        HashMap::new()
    }

    #[test]
    fn removed_deliverable_keeps_baselined_cost_rate() {
        let mut br = row("a", 0.0, 10.0, 100.0);
        br.cost_rate = 2.0;
        let b = baseline(vec![br]);
        let g = graph(vec![]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(5.0),
                ..Actuals::default()
            },
        )]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &actuals, 5);
        assert!(near(r.ac, 10.0));
    }

    #[test]
    fn rate_edit_after_baseline_does_not_change_ac() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let mut d = deliverable("a", 100.0, &[]);
        d.metadata = serde_json::json!({ "cost_rate": 9.0 });
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(5.0),
                ..Actuals::default()
            },
        )]);
        let r = report(
            &b,
            &graph(vec![d]),
            &HashMap::new(),
            &HashMap::new(),
            &actuals,
            5,
        );
        assert!(near(r.ac, 5.0));
    }

    #[test]
    fn invalid_rate_added_after_baseline_does_not_fail_compute() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let mut d = deliverable("a", 100.0, &[]);
        d.metadata = serde_json::json!({ "cost_rate": -4.0 });
        let r = compute_ev(
            &b,
            &graph(vec![d]),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            b.start,
            &[],
        );
        assert!(r.is_ok());
    }

    #[test]
    fn baseline_captures_cost_rate_per_row() {
        let mut d = deliverable("a", 10.0, &[]);
        d.metadata = serde_json::json!({ "cost_rate": 3.0 });
        let g = graph(vec![d]);
        let cpm = compute_cpm(&g).unwrap();
        let b = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1).unwrap();
        assert!(near(b.rows[0].cost_rate, 3.0));
    }

    #[test]
    fn deliverable_added_after_baseline_is_excluded_and_listed() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![
            deliverable("z", 40.0, &[]),
            deliverable("a", 100.0, &[]),
            deliverable("m", 40.0, &[]),
        ]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 5);
        assert_eq!(
            r.excluded_unbaselined,
            vec!["m".to_string(), "z".to_string()]
        );
    }

    #[test]
    fn unbaselined_deliverable_adds_nothing_to_bac() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![
            deliverable("a", 100.0, &[]),
            deliverable("z", 40.0, &[]),
        ]);
        let r = report(&b, &g, &HashMap::new(), &HashMap::new(), &HashMap::new(), 5);
        assert!(near(r.bac, 100.0));
    }

    #[test]
    fn negative_reported_hours_fall_back_to_leased() {
        assert!(near(ac_for(Some(-2.0), 3.0, None), 3.0));
    }

    #[test]
    fn non_finite_reported_hours_fall_back_to_leased() {
        assert!(near(ac_for(Some(f32::NAN), 3.0, None), 3.0));
    }

    #[test]
    fn as_of_before_start_gives_zero_pv() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            -5,
        );
        assert!(near(r.pv, 0.0));
    }

    #[test]
    fn as_of_before_start_leaves_spi_null_with_reason() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let r = report(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            -5,
        );
        assert!(r.spi.is_none() && undefined_reason(&r, "spi").is_some());
    }

    #[test]
    fn pv_is_flat_over_a_weekend_on_a_working_calendar() {
        // Task spans 40 working hours (a full work week); Fri 2026-01-09 00:00 + 8h = 40h elapsed.
        let mut b = baseline(vec![row("a", 0.0, 80.0, 100.0)]);
        b.calendar = Some(Calendar::default());
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let at_fri_end = compute_ev(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            at(2026, 1, 9, 12),
            &[],
        )
        .unwrap();
        let mon = compute_ev(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            at(2026, 1, 12, 0),
            &[],
        )
        .unwrap();
        assert!(near(at_fri_end.pv, mon.pv));
    }

    #[test]
    fn eac_reason_when_cpi_is_zero_is_explained() {
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
        assert_eq!(
            undefined_reason(&r, "eac").as_deref(),
            Some("CPI is 0, nothing earned for the cost spent")
        );
    }

    #[test]
    fn tcpi_is_negative_when_ac_exceeds_bac() {
        let b = baseline(vec![row("a", 0.0, 10.0, 100.0)]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let st = statuses(&[("a", DeliverableStatus::InProgress)]);
        let rules = HashMap::from([("a".to_string(), EarningRule::Weighted)]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                earned_pct: Some(50),
                actual_hours: Some(150.0),
                ..Actuals::default()
            },
        )]);
        let r = report(&b, &g, &st, &rules, &actuals, 5);
        assert!(near(r.tcpi.unwrap(), -1.0));
    }

    #[test]
    fn f32_overflow_in_a_ratio_becomes_undefined() {
        let b = baseline(vec![
            row("a", 0.0, 10.0, 3.0e38),
            row("b", 0.0, 10.0, 3.0e38),
        ]);
        let g = graph(vec![deliverable("a", 1.0, &[]), deliverable("b", 1.0, &[])]);
        let st = statuses(&[("a", DeliverableStatus::Complete)]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(1.0e-30),
                ..Actuals::default()
            },
        )]);
        let r = report(&b, &g, &st, &HashMap::new(), &actuals, 5);
        assert!(r.cpi.is_none() && undefined_reason(&r, "cpi").is_some());
    }

    #[test]
    fn alert_needs_both_of_the_two_previous_snapshots() {
        // Oldest is low, but the latest two are (low, healthy): no alert.
        assert!(alerts_with(&[snap(0.5, 1.0), snap(0.5, 1.0), snap(1.0, 1.0)]).is_empty());
    }

    #[test]
    fn elapsed_matches_naive_minute_loop_over_random_spans() {
        let mut seed: u64 = 0x5eed_1234_abcd_ef01;
        let mut next = move |n: u64| {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) % n
        };
        let names = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        let mut mismatches = Vec::new();
        for case in 0..60 {
            let start = at(2026, 1, 3, 0) + Duration::minutes(next(60 * 24 * 14) as i64);
            let span = Duration::minutes(next(60 * 24 * 40) as i64 + 1);
            let off = next(2881) as i32 - 1440;
            let n_days = 1 + next(7) as usize;
            let first = next(7) as usize;
            let workdays: Vec<String> = (0..n_days)
                .map(|i| names[(first + i) % 7].to_string())
                .collect();
            let hpd = [8.0_f32, 6.5, 24.0][next(3) as usize];
            let c = Calendar {
                hours_per_day: hpd,
                workdays: workdays.clone(),
                utc_offset_minutes: off,
            };
            let got = elapsed_hours(start, start + span, Some(&c)).unwrap();
            let mut naive = 0u64;
            let window = (f64::from(hpd) * 60.0) as i64;
            let mut m = start;
            let end = start + span;
            while m < end {
                let local = m + Duration::minutes(i64::from(off));
                let name = names[local.weekday().num_days_from_monday() as usize];
                let mins = i64::from(local.hour()) * 60 + i64::from(local.minute());
                if workdays.iter().any(|w| w == name) && mins < window {
                    naive += 1;
                }
                m += Duration::minutes(1);
            }
            let want = naive as f32 / 60.0;
            if (got - want).abs() > 1e-2 {
                mismatches.push((case, got, want));
            }
        }
        assert!(mismatches.is_empty(), "{mismatches:?}");
    }

    // ── P5 Task 3: snapshot support ─────────────────────────────────────

    #[test]
    fn trend_alerts_fire_spi_when_two_latest_are_low() {
        let a = trend_alerts(&[snap(0.8, 1.0), snap(0.85, 1.0)]);
        assert_eq!(a, vec!["SPI_BELOW_0_9".to_string()]);
    }

    #[test]
    fn baseline_rejects_budget_total_beyond_f32() {
        let heavy = |id: &str| {
            let mut d = deliverable(id, 1.0, &[]);
            d.metadata = serde_json::json!({ "cost_rate": 3.0e38 });
            d
        };
        let g = graph(vec![heavy("a"), heavy("b")]);
        let cpm = compute_cpm(&g).unwrap();
        let e = build_baseline(&g, &cpm, at(2026, 1, 5, 0), None, 1);
        assert!(matches!(e, Err(PlannerError::InvalidGraph { .. })));
    }

    #[test]
    fn ev_rejects_actual_cost_beyond_f32() {
        let mut br = row("a", 0.0, 10.0, 100.0);
        br.cost_rate = 3.0e38;
        let b = baseline(vec![br]);
        let g = graph(vec![deliverable("a", 100.0, &[])]);
        let actuals = HashMap::from([(
            "a".to_string(),
            Actuals {
                actual_hours: Some(1.0e6),
                ..Actuals::default()
            },
        )]);
        let as_of = b.start + Duration::hours(5);
        let e = compute_ev(
            &b,
            &g,
            &HashMap::new(),
            &HashMap::new(),
            &actuals,
            as_of,
            &[],
        );
        assert!(matches!(e, Err(PlannerError::InvalidGraph { .. })));
    }

    #[test]
    fn calendar_rejects_unknown_fields() {
        let c = serde_json::from_value::<Calendar>(serde_json::json!({ "hours": 8 }));
        assert!(c.is_err());
    }

    fn summary_of(r: &EvReport) -> SnapshotSummary {
        SnapshotSummary::from_report(r, r.as_of)
    }

    #[test]
    fn snapshot_summary_carries_report_spi() {
        let r = textbook();
        assert_eq!(summary_of(&r).spi, r.spi);
    }

    #[test]
    fn snapshot_markdown_starts_with_header_row() {
        let md = render_snapshots_markdown(&[summary_of(&textbook())]);
        assert!(md.starts_with("| date | PV | EV | AC | SPI | CPI | EAC |\n"));
    }

    #[test]
    fn snapshot_markdown_renders_one_row_per_snapshot() {
        let s = summary_of(&textbook());
        let md = render_snapshots_markdown(&[s.clone(), s]);
        assert_eq!(md.lines().count(), 4);
    }

    #[test]
    fn snapshot_markdown_row_shows_textbook_values() {
        let md = render_snapshots_markdown(&[summary_of(&textbook())]);
        assert_eq!(
            md.lines().nth(2),
            Some("| 2026-01-05T10:00Z | 50.00 | 40.00 | 48.00 | 0.80 | 0.83 | 120.00 |")
        );
    }

    #[test]
    fn snapshot_markdown_renders_undefined_ratio_as_na() {
        let r = one(EarningRule::ZeroHundred, DeliverableStatus::Pending, None);
        let md = render_snapshots_markdown(&[summary_of(&r)]);
        assert!(
            md.lines()
                .nth(2)
                .is_some_and(|l| l.ends_with("| n/a | n/a |"))
        );
    }
}
