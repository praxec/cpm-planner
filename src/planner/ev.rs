//! Earned-value operations of [`BasicCpmPlanner`] behind
//! [`crate::ports::Planner::baseline`], [`crate::ports::Planner::ev`] and
//! [`crate::ports::Planner::snapshot`] (`plan.baseline`, `plan.ev`,
//! `plan.snapshot`; P5).
//!
//! Gating: taking a baseline and appending a snapshot are execution-side
//! writes, so both refuse a draft variant (`VARIANT_NOT_SELECTED`) and an
//! archived one (`ARCHIVE_REFUSED`), like `acquire_cohort`. Reading the
//! report is read-only and works on any plan, including drafts and archived
//! variants, as long as it has a baseline (`NOT_BASELINED` otherwise).
//!
//! Baselines, actuals and snapshots belong to one plan id, so each variant
//! has its own: a newly selected variant takes baseline 1 (no reason
//! needed) and starts its own snapshot history.
//!
//! The export is read bounded ([`ev_store::snapshots`]: the newest 100 by
//! `as_of`, ties by `taken_at`) and kept in that order, so a backfilled
//! snapshot lands in date order. Alert windows are their own bounded,
//! per-baseline query ([`ev_store::alert_window`]): a snapshot's alerts
//! see only readings at or before its own `(as_of, taken_at)`, and never
//! backfilled ones.
//!
//! `as_of` is the PV status date only: EV and AC are read from the
//! statuses and actuals stored when the call runs. A snapshot whose `as_of`
//! is more than an hour before its `taken_at` is flagged `backfilled` and
//! raises no alerts of its own.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::Transaction;
use serde_json::json;

use super::BasicCpmPlanner;
use crate::audit::AuditEvent;
use crate::earned_value::{
    BaselineMode, BaselineOutcome, BaselineRequest, BaselineTaken, EarningRule, EvReport,
    EvSummary, FIRST_EXECUTION_REASON, MAX_BASELINE_REASON_CHARS, SNAPSHOT_EXPORT_LIMIT,
    SnapshotFormat, SnapshotOutcome, SnapshotRequest, SnapshotSummary, build_baseline, compute_ev,
    render_snapshots_markdown, trend_alerts,
};
use crate::ev_store;
use crate::locks::PlanState;
use crate::plan::{DeliverableStatus, PlanGraph, PlanId, PlannerError};
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

/// Decode stored snapshot summaries, keeping their order.
fn decode(
    plan_id: &PlanId,
    rows: Vec<ev_store::EvSnapshot>,
) -> Result<Vec<SnapshotSummary>, PlannerError> {
    rows.into_iter()
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

/// The newest stored snapshot summaries of `plan_id`, in `as_of` order
/// (ties by `taken_at`).
fn history(tx: &Transaction<'_>, plan_id: &PlanId) -> Result<Vec<SnapshotSummary>, PlannerError> {
    decode(plan_id, ev_store::snapshots(tx, plan_id)?)
}

/// Ratios of the latest `limit` snapshots of baseline `number` at or
/// before `position` (unbounded when `None`), oldest first.
fn alert_ratios(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    number: u32,
    position: Option<(DateTime<Utc>, DateTime<Utc>)>,
    limit: usize,
) -> Result<Vec<EvSummary>, PlannerError> {
    let rows = ev_store::alert_window(tx, plan_id, number, position, limit)?;
    Ok(decode(plan_id, rows)?
        .iter()
        .map(SnapshotSummary::ratios)
        .collect())
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

/// `true` when `state` shows work on the plan has started: a non-pending,
/// non-ready status, a lease or attempt history, a held lock, or queued
/// actuals. Selecting such a variant is an execution-starting call.
pub(super) fn state_has_progress(state: &PlanState) -> bool {
    state.statuses.values().any(|status| {
        matches!(
            status,
            DeliverableStatus::InProgress
                | DeliverableStatus::Complete
                | DeliverableStatus::Failed { .. }
        )
    }) || !state.attempt_counts.is_empty()
        || !state.failure_counts.is_empty()
        || !state.lapse_counts.is_empty()
        || !state.locks.is_empty()
        || !state.reported_actuals.is_empty()
}

/// Build and insert a plan's first baseline (number 1, reason
/// [`FIRST_EXECUTION_REASON`]) from `state`'s graph, inside the caller's
/// write transaction. Returns the summary and the `plan.ev.baselined`
/// audit event to flush after commit.
pub(super) fn insert_first_baseline(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    state: &PlanState,
    now: DateTime<Utc>,
) -> Result<(BaselineTaken, AuditEvent), PlannerError> {
    let baseline = build_baseline(&state.graph, &state.cached_result, now, None, 1)?;
    ev_store::insert_baseline(tx, plan_id, &baseline, Some(FIRST_EXECUTION_REASON), now)?;
    let taken = BaselineTaken {
        baseline_number: baseline.number,
        as_of: baseline.start,
        reason: FIRST_EXECUTION_REASON.to_string(),
    };
    let event = AuditEvent::new("plan.ev.baselined").with_payload(json!({
        "plan_id": plan_id.0,
        "baseline_number": baseline.number,
        "previous_baseline": serde_json::Value::Null,
        "start": baseline.start,
        "bac": baseline.bac,
        "finish_hours": baseline.finish_hours,
        "deliverable_count": baseline.rows.len(),
        "reason": FIRST_EXECUTION_REASON,
        "backfilled": baseline.backfilled,
    }));
    Ok((taken, event))
}

impl BasicCpmPlanner {
    /// Enforce the baseline requirement for an execution-starting call,
    /// inside the caller's write transaction. Auto mode takes the first
    /// baseline when the plan has none, recording its `plan.ev.baselined`
    /// event in `audit`; refuse mode returns
    /// [`PlannerError::BaselineRequired`]. `Ok(None)` when a baseline
    /// already exists.
    pub(super) fn ensure_execution_baseline(
        &self,
        tx: &Transaction<'_>,
        plan_id: &PlanId,
        state: &PlanState,
        now: DateTime<Utc>,
        audit: &mut Vec<AuditEvent>,
    ) -> Result<Option<BaselineTaken>, PlannerError> {
        if ev_store::latest_baseline(tx, plan_id)?.is_some() {
            return Ok(None);
        }
        match self.baseline_mode {
            BaselineMode::Refuse => Err(PlannerError::BaselineRequired {
                plan_id: plan_id.0.clone(),
            }),
            BaselineMode::Auto => {
                let (taken, event) = insert_first_baseline(tx, plan_id, state, now)?;
                audit.push(event);
                Ok(Some(taken))
            }
        }
    }

    /// `true` when `plan_id` has recorded progress ([`state_has_progress`])
    /// but no earned-value baseline: the state `plan.lint` reports as
    /// `NO_BASELINE`. Read-only.
    pub(crate) fn has_progress_without_baseline(
        &self,
        plan_id: &PlanId,
    ) -> Result<bool, PlannerError> {
        self.store.read_tx(|tx| {
            let state = load_state(tx, plan_id)?;
            if ev_store::latest_baseline(tx, plan_id)?.is_some() {
                return Ok(false);
            }
            Ok(state_has_progress(&state))
        })
    }

    /// Freeze the plan's current CPM schedule and budgets as its next
    /// baseline (`plan.baseline`). The first baseline is number 1; a
    /// re-baseline takes the next number and needs a non-blank `reason`
    /// (`INVALID_GRAPH` otherwise). Actuals and snapshots are kept: only
    /// the PV curve and budgets change. `start` defaults to the clock's now.
    /// `calendar` defaults to wall-clock hours on the first baseline and to
    /// the previous baseline's calendar on a re-baseline. `backfill_from`
    /// sets `start` retroactively and marks the baseline backfilled; it must
    /// not be in the future or before the plan's creation time
    /// (`INVALID_VALUE`).
    /// Gated like execution (selected, unarchived variant only). Audited as
    /// `plan.ev.baselined`.
    pub(super) async fn take_baseline(
        &self,
        req: BaselineRequest,
    ) -> Result<BaselineOutcome, PlannerError> {
        let reason = normalise_reason(req.reason.as_deref())?;
        let now = self.now();
        let backfill_from = req.backfill_from;
        let start = backfill_from.or(req.start).unwrap_or(now);
        let plan_id = req.plan_id;
        let (outcome, previous) = self.store.write_tx(|tx| {
            crate::portfolio::ensure_executable(tx, &plan_id)?;
            if let Some(backfill) = backfill_from {
                if backfill > now {
                    return Err(PlannerError::InvalidValue {
                        reason: format!("backfill_from {backfill} is in the future (now is {now})"),
                    });
                }
                if let Some(created) = ev_store::plan_created_at(tx, &plan_id)?
                    && backfill < created
                {
                    return Err(PlannerError::InvalidValue {
                        reason: format!(
                            "backfill_from {backfill} is before the plan's creation time {created}"
                        ),
                    });
                }
            }
            let state = load_state(tx, &plan_id)?;
            let latest = ev_store::latest_baseline(tx, &plan_id)?;
            let previous = latest.as_ref().map(|b| b.baseline.number);
            // A re-baseline without a calendar keeps the previous one.
            let calendar = req
                .calendar
                .or_else(|| latest.and_then(|b| b.baseline.calendar));
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
            let mut baseline =
                build_baseline(&state.graph, &state.cached_result, start, calendar, number)?;
            baseline.backfilled = backfill_from.is_some();
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
                backfilled: baseline.backfilled,
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
            "backfilled": outcome.backfilled,
        }));
        self.flush_audit(vec![event]).await;
        Ok(outcome)
    }

    /// The earned-value report of `plan_id` against its latest baseline
    /// (`plan.ev`), as of `as_of` (default: the clock's now). Read-only and
    /// ungated. `as_of` sets PV only; EV and AC are the progress stored
    /// now. Trend alerts look at the two latest stored non-backfilled
    /// snapshots (by `as_of`) of the latest baseline; the current reading is
    /// not one of them. `NOT_BASELINED` before the first baseline.
    ///
    /// Blocking: the report recomputes the CPM, so async callers should run
    /// it off the runtime's worker threads (the MCP server does).
    pub(super) fn ev_report(
        &self,
        plan_id: &PlanId,
        as_of: Option<DateTime<Utc>>,
    ) -> Result<EvReport, PlannerError> {
        let as_of = as_of.unwrap_or_else(|| self.now());
        let (graph, statuses, baseline, actuals, ratios, variant) = self.store.read_tx(|tx| {
            let state = load_state(tx, plan_id)?;
            let baseline = ev_store::latest_baseline(tx, plan_id)?
                .ok_or_else(|| not_baselined(plan_id))?
                .baseline;
            let actuals = ev_store::load_actuals(tx, plan_id)?;
            let ratios = alert_ratios(tx, plan_id, baseline.number, None, 2)?;
            let variant = match crate::portfolio::variant_info(tx, plan_id)? {
                Some(info) => {
                    let active_plan = if info.archived {
                        crate::portfolio::active_line_name(tx, &info.project)?
                    } else {
                        None
                    };
                    Some((info.archived, active_plan))
                }
                None => None,
            };
            Ok((
                state.graph,
                state.statuses,
                baseline,
                actuals,
                ratios,
                variant,
            ))
        })?;
        let mut report = compute_ev(
            &baseline,
            &graph,
            &statuses,
            &rules_of(&graph),
            &actuals,
            as_of,
            &ratios,
        )?;
        if let Some((archived, active_plan)) = variant {
            report.archived = archived;
            report.active_plan = active_plan;
        }
        Ok(report)
    }

    /// Compute the report (as for [`Self::ev_report`]) and append it as a
    /// snapshot (`plan.snapshot`). Its alerts look only at readings up to
    /// its own position: this snapshot and the latest earlier one, by
    /// `(as_of, taken_at)`, of the current baseline, so a backfill never
    /// takes alerts from newer readings.
    /// `taken_at` is the clock's now, nudged one microsecond past the
    /// latest stored `taken_at` when the clock has not moved past it, so
    /// snapshots stay distinct. Gated like execution. Returns the summary,
    /// the stored count and the newest [`SNAPSHOT_EXPORT_LIMIT`] snapshots
    /// by `as_of`, rendered in `format`. `NOT_BASELINED` before the first
    /// baseline.
    ///
    /// Blocking, like [`Self::ev_report`].
    pub(super) fn take_snapshot(
        &self,
        req: SnapshotRequest,
    ) -> Result<SnapshotOutcome, PlannerError> {
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
            let taken_at = match ev_store::latest_taken_at(tx, plan_id)? {
                Some(last) if now <= last => last + chrono::Duration::microseconds(1),
                _ => now,
            };
            let mut ratios =
                alert_ratios(tx, plan_id, baseline.number, Some((as_of, taken_at)), 1)?;
            let report = compute_ev(
                &baseline,
                &state.graph,
                &state.statuses,
                &rules_of(&state.graph),
                &actuals,
                as_of,
                &ratios,
            )?;
            let mut summary = SnapshotSummary::from_report(&report, taken_at);
            ratios.push(summary.ratios());
            summary.alerts = trend_alerts(&ratios, baseline.number);
            let stored = serde_json::to_value(&summary).map_err(backend)?;
            ev_store::insert_snapshot(tx, plan_id, taken_at, as_of, &stored)?;
            let history = history(tx, plan_id)?;
            let newest = &history[history.len().saturating_sub(SNAPSHOT_EXPORT_LIMIT)..];
            let export = match req.format {
                SnapshotFormat::Json => serde_json::to_value(newest).map_err(backend)?,
                SnapshotFormat::Markdown => json!(render_snapshots_markdown(newest)),
            };
            Ok(SnapshotOutcome {
                summary,
                format: req.format,
                export,
                snapshot_count: ev_store::snapshot_count(tx, plan_id)?,
            })
        })
    }
}
