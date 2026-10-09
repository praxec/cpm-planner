//! Earned-value operations of [`BasicCpmPlanner`]: `plan.baseline`,
//! `plan.ev` and `plan.snapshot` (P5).
//!
//! Gating: taking a baseline and appending a snapshot are execution-side
//! writes, so both refuse a draft variant (`VARIANT_NOT_SELECTED`) and an
//! archived one (`ARCHIVE_REFUSED`), like `acquire_cohort`. Reading the
//! report ([`BasicCpmPlanner::earned_value`]) is read-only and works on any
//! plan, including drafts and archived variants, as long as it has a
//! baseline (`NOT_BASELINED` otherwise).

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::Transaction;
use serde_json::json;

use super::BasicCpmPlanner;
use crate::audit::AuditEvent;
use crate::earned_value::{
    BaselineOutcome, BaselineRequest, EarningRule, EvReport, EvSummary, MAX_BASELINE_REASON_CHARS,
    SNAPSHOT_EXPORT_LIMIT, SnapshotFormat, SnapshotOutcome, SnapshotRequest, SnapshotSummary,
    build_baseline, compute_ev, render_snapshots_markdown, trend_alerts,
};
use crate::ev_store;
use crate::locks::PlanState;
use crate::plan::{PlanGraph, PlanId, PlannerError};
use crate::plan_store::{backend, load_plan_state};

fn load_state(tx: &Transaction<'_>, plan_id: &PlanId) -> Result<PlanState, PlannerError> {
    load_plan_state(tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
        plan_id: plan_id.0.clone(),
    })
}

fn not_baselined(plan_id: &PlanId) -> PlannerError {
    PlannerError::NotBaselined {
        plan_id: plan_id.0.clone(),
    }
}

/// Every stored snapshot summary of `plan_id`, oldest first.
fn history(tx: &Transaction<'_>, plan_id: &PlanId) -> Result<Vec<SnapshotSummary>, PlannerError> {
    ev_store::snapshots(tx, plan_id)?
        .into_iter()
        .map(|s| {
            serde_json::from_value(s.summary).map_err(|e| {
                backend(anyhow::anyhow!(
                    "plan {plan_id}: stored snapshot taken at {} does not decode: {e}",
                    s.taken_at
                ))
            })
        })
        .collect()
}

fn rules_of(graph: &PlanGraph) -> HashMap<String, EarningRule> {
    graph
        .deliverables
        .iter()
        .map(|d| (d.id.clone(), d.earning_rule.unwrap_or_default()))
        .collect()
}

/// Normalise a baseline reason: trimmed, blank means absent, and at most
/// [`MAX_BASELINE_REASON_CHARS`] characters.
fn normalise_reason(reason: Option<&str>) -> Result<Option<String>, PlannerError> {
    let Some(reason) = reason else {
        return Ok(None);
    };
    if reason.chars().count() > MAX_BASELINE_REASON_CHARS {
        return Err(PlannerError::InvalidGraph {
            reason: format!(
                "baseline reason is longer than {MAX_BASELINE_REASON_CHARS} characters"
            ),
        });
    }
    let trimmed = reason.trim();
    Ok((!trimmed.is_empty()).then(|| trimmed.to_string()))
}

impl BasicCpmPlanner {
    /// Freeze the plan's current CPM schedule and budgets as its next
    /// baseline (`plan.baseline`). The first baseline is number 1; a
    /// re-baseline takes the next number and needs a non-blank `reason`
    /// (`INVALID_GRAPH` otherwise). Actuals and snapshots are kept: only
    /// the PV curve and budgets change. `start` defaults to the clock's now.
    /// Gated like execution (selected, unarchived variant only). Audited as
    /// `plan.ev.baselined`.
    pub async fn baseline(&self, req: BaselineRequest) -> Result<BaselineOutcome, PlannerError> {
        let reason = normalise_reason(req.reason.as_deref())?;
        let now = self.now();
        let start = req.start.unwrap_or(now);
        let plan_id = req.plan_id;
        let (outcome, previous) = self.store.write_tx(|tx| {
            crate::portfolio::ensure_executable(tx, &plan_id)?;
            let state = load_state(tx, &plan_id)?;
            let previous = ev_store::latest_baseline(tx, &plan_id)?.map(|b| b.baseline.number);
            let number = match previous {
                None => 1,
                Some(n) if reason.is_none() => {
                    return Err(PlannerError::InvalidGraph {
                        reason: format!(
                            "plan {plan_id} already has baseline {n}; re-baselining requires a \
                             non-empty reason"
                        ),
                    });
                }
                Some(n) => n.checked_add(1).ok_or_else(|| PlannerError::InvalidGraph {
                    reason: format!("plan {plan_id} has used every baseline number"),
                })?,
            };
            let baseline = build_baseline(
                &state.graph,
                &state.cached_result,
                start,
                req.calendar,
                number,
            )?;
            ev_store::insert_baseline(tx, &plan_id, &baseline, reason.as_deref(), now)?;
            let outcome = BaselineOutcome {
                plan_id: plan_id.0.clone(),
                baseline_number: baseline.number,
                start: baseline.start,
                calendar: baseline.calendar,
                bac: baseline.bac,
                finish_hours: baseline.finish_hours,
                deliverable_count: baseline.rows.len(),
                reason: reason.clone(),
            };
            Ok((outcome, previous))
        })?;
        let event = AuditEvent::new("plan.ev.baselined").with_payload(json!({
            "plan_id": outcome.plan_id,
            "baseline_number": outcome.baseline_number,
            "previous_baseline": previous,
            "start": outcome.start,
            "bac": outcome.bac,
            "finish_hours": outcome.finish_hours,
            "deliverable_count": outcome.deliverable_count,
            "reason": outcome.reason,
        }));
        self.flush_audit(vec![event]).await;
        Ok(outcome)
    }

    /// The earned-value report of `plan_id` against its latest baseline
    /// (`plan.ev`), as of `as_of` (default: the clock's now). Read-only and
    /// ungated. Trend alerts look at the two most recent stored snapshots.
    /// `NOT_BASELINED` before the first baseline.
    ///
    /// Blocking: the report recomputes the CPM, so async callers should run
    /// it off the runtime's worker threads (the MCP server does).
    pub fn earned_value(
        &self,
        plan_id: &PlanId,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<EvReport, PlannerError> {
        let as_of = as_of.unwrap_or_else(|| self.now());
        let (graph, statuses, baseline, actuals, ratios) = self.store.read_tx(|tx| {
            let state = load_state(tx, plan_id)?;
            let baseline = ev_store::latest_baseline(tx, plan_id)?
                .ok_or_else(|| not_baselined(plan_id))?
                .baseline;
            let actuals = ev_store::load_actuals(tx, plan_id)?;
            let ratios: Vec<EvSummary> = history(tx, plan_id)?
                .iter()
                .map(SnapshotSummary::ratios)
                .collect();
            Ok((state.graph, state.statuses, baseline, actuals, ratios))
        })?;
        compute_ev(
            &baseline,
            &graph,
            &statuses,
            &rules_of(&graph),
            &actuals,
            as_of,
            &ratios,
        )
    }

    /// Compute the report (as for [`Self::earned_value`]) and append it as
    /// a snapshot (`plan.snapshot`). Its alerts look at this snapshot and
    /// the one before it. `taken_at` is the clock's now, nudged one
    /// microsecond past the latest stored snapshot when the clock has not
    /// moved past it, so snapshots stay ordered and distinct. Gated like
    /// execution. Returns the summary and the newest
    /// [`SNAPSHOT_EXPORT_LIMIT`] snapshots rendered in `format`.
    /// `NOT_BASELINED` before the first baseline.
    ///
    /// Blocking, like [`Self::earned_value`].
    pub fn snapshot(&self, req: SnapshotRequest) -> Result<SnapshotOutcome, PlannerError> {
        let now = self.now();
        let as_of = req.as_of.unwrap_or(now);
        let plan_id = &req.plan_id;
        self.store.write_tx(|tx| {
            crate::portfolio::ensure_executable(tx, plan_id)?;
            let state = load_state(tx, plan_id)?;
            let baseline = ev_store::latest_baseline(tx, plan_id)?
                .ok_or_else(|| not_baselined(plan_id))?
                .baseline;
            let actuals = ev_store::load_actuals(tx, plan_id)?;
            let mut history = history(tx, plan_id)?;
            let mut ratios: Vec<EvSummary> = history.iter().map(SnapshotSummary::ratios).collect();
            let report = compute_ev(
                &baseline,
                &state.graph,
                &state.statuses,
                &rules_of(&state.graph),
                &actuals,
                as_of,
                &ratios,
            )?;
            let taken_at = match history.last() {
                Some(last) if now <= last.taken_at => {
                    last.taken_at + chrono::Duration::microseconds(1)
                }
                _ => now,
            };
            let mut summary = SnapshotSummary::from_report(&report, taken_at);
            ratios.push(summary.ratios());
            summary.alerts = trend_alerts(&ratios);
            let stored = serde_json::to_value(&summary).map_err(backend)?;
            ev_store::insert_snapshot(tx, plan_id, taken_at, as_of, &stored)?;
            history.push(summary.clone());
            let newest = &history[history.len().saturating_sub(SNAPSHOT_EXPORT_LIMIT)..];
            let export = match req.format {
                SnapshotFormat::Json => serde_json::to_value(newest).map_err(backend)?,
                SnapshotFormat::Markdown => json!(render_snapshots_markdown(newest)),
            };
            Ok(SnapshotOutcome {
                summary,
                format: req.format,
                export,
                snapshot_count: history.len(),
            })
        })
    }
}
