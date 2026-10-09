//! Schema v4 earned-value storage: frozen baselines, per-deliverable actuals
//! and EV snapshots. Every function runs inside a caller-supplied
//! transaction so EV writes commit atomically with the planner state they
//! accompany (a lease ending adds its hours in the same transaction that
//! releases it).
//!
//! Tables (created by the v4 migration in [`crate::plan_store`]):
//! - `baselines(plan_id, number, start_us, calendar, rows, bac, reason, created_at_us)`
//! - `ev_actuals(plan_id, deliverable_id, earned_pct, actual_hours, leased_hours, evidence, updated_at_us)`
//! - `ev_snapshots(plan_id, taken_at_us, as_of_us, summary)`

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::earned_value::{Actuals, Baseline, BaselineRow, Calendar};
use crate::plan::{PlanId, PlannerError};
use crate::plan_store::backend;

/// A stored baseline with its bookkeeping columns.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))] // read by plan.baseline / plan.ev (P5 Task 3)
pub(crate) struct StoredBaseline {
    pub(crate) baseline: Baseline,
    /// Why this baseline was taken; `None` for a first baseline without one.
    pub(crate) reason: Option<String>,
    pub(crate) created_at: DateTime<Utc>,
}

/// One stored EV snapshot.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(not(test), allow(dead_code))] // read by plan.snapshot / plan.ev (P5 Task 3)
pub(crate) struct EvSnapshot {
    pub(crate) taken_at: DateTime<Utc>,
    pub(crate) as_of: DateTime<Utc>,
    pub(crate) summary: serde_json::Value,
}

/// Progress reported through `mark_status`, applied to `ev_actuals`.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ReportedActuals {
    /// Replaces the stored percent when set.
    pub(crate) earned_pct: Option<u8>,
    /// Replaces the stored actual hours when set.
    pub(crate) actual_hours: Option<f32>,
    /// Appended to the stored evidence list when set.
    pub(crate) evidence: Option<String>,
}

fn dt(us: i64, column: &str) -> Result<DateTime<Utc>, PlannerError> {
    DateTime::from_timestamp_micros(us).ok_or_else(|| {
        backend(anyhow::anyhow!(
            "corrupt timestamp in column {column}: {us}"
        ))
    })
}

/// Every `ev_actuals` row of `plan_id`, keyed by deliverable id.
#[cfg_attr(not(test), allow(dead_code))] // read by plan.ev (P5 Task 3)
pub(crate) fn load_actuals(
    conn: &Connection,
    plan_id: &PlanId,
) -> Result<HashMap<String, Actuals>, PlannerError> {
    let mut stmt = conn
        .prepare(
            "SELECT deliverable_id, earned_pct, actual_hours, leased_hours, evidence
             FROM ev_actuals WHERE plan_id = ?1",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map(params![plan_id.0], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?,
                r.get::<_, Option<f64>>(2)?,
                r.get::<_, f64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })
        .map_err(backend)?;
    let mut out = HashMap::new();
    for row in rows {
        let (id, pct, actual, leased, evidence) = row.map_err(backend)?;
        let earned_pct = pct
            .map(|p| {
                u8::try_from(p).ok().filter(|p| *p <= 100).ok_or_else(|| {
                    backend(anyhow::anyhow!(
                        "plan {plan_id}: stored earned_pct {p} for {id} is outside 0..=100"
                    ))
                })
            })
            .transpose()?;
        let evidence: Vec<String> = serde_json::from_str(&evidence).map_err(|e| {
            backend(anyhow::anyhow!(
                "plan {plan_id}: stored evidence for {id} is not a JSON string list: {e}"
            ))
        })?;
        out.insert(
            id,
            Actuals {
                earned_pct,
                actual_hours: actual.map(|h| h as f32),
                leased_hours: leased as f32,
                evidence,
            },
        );
    }
    Ok(out)
}

/// Add `hours` of lease time to `deliverable_id`'s `leased_hours`.
pub(crate) fn add_leased_hours(
    conn: &Connection,
    plan_id: &PlanId,
    deliverable_id: &str,
    hours: f32,
    at: DateTime<Utc>,
) -> Result<(), PlannerError> {
    conn.execute(
        "INSERT INTO ev_actuals (plan_id, deliverable_id, leased_hours, updated_at_us)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (plan_id, deliverable_id) DO UPDATE
             SET leased_hours = leased_hours + excluded.leased_hours,
                 updated_at_us = excluded.updated_at_us",
        params![
            plan_id.0,
            deliverable_id,
            f64::from(hours),
            at.timestamp_micros()
        ],
    )
    .map_err(backend)?;
    Ok(())
}

/// Apply a `mark_status` progress report: set the percent and actual hours
/// that are given and append the evidence entry. A deliverable already at
/// [`crate::plan::MAX_EVIDENCE_ENTRIES`] entries refuses another
/// (`INVALID_ACTUALS`).
pub(crate) fn record_reported(
    conn: &Connection,
    plan_id: &PlanId,
    deliverable_id: &str,
    report: &ReportedActuals,
    at: DateTime<Utc>,
) -> Result<(), PlannerError> {
    let stored: Option<String> = conn
        .query_row(
            "SELECT evidence FROM ev_actuals WHERE plan_id = ?1 AND deliverable_id = ?2",
            params![plan_id.0, deliverable_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    let mut evidence: Vec<String> = match &stored {
        Some(json) => serde_json::from_str(json).map_err(|e| {
            backend(anyhow::anyhow!(
                "plan {plan_id}: stored evidence for {deliverable_id} is not a JSON string list: {e}"
            ))
        })?,
        None => Vec::new(),
    };
    if let Some(e) = &report.evidence {
        if evidence.len() >= crate::plan::MAX_EVIDENCE_ENTRIES {
            return Err(PlannerError::InvalidActuals {
                reason: format!(
                    "deliverable '{deliverable_id}' already has {} evidence entries",
                    crate::plan::MAX_EVIDENCE_ENTRIES
                ),
            });
        }
        evidence.push(e.clone());
    }
    let evidence = serde_json::to_string(&evidence).map_err(backend)?;
    conn.execute(
        "INSERT INTO ev_actuals
             (plan_id, deliverable_id, earned_pct, actual_hours, evidence, updated_at_us)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (plan_id, deliverable_id) DO UPDATE
             SET earned_pct = COALESCE(excluded.earned_pct, earned_pct),
                 actual_hours = COALESCE(excluded.actual_hours, actual_hours),
                 evidence = excluded.evidence,
                 updated_at_us = excluded.updated_at_us",
        params![
            plan_id.0,
            deliverable_id,
            report.earned_pct,
            report.actual_hours.map(f64::from),
            evidence,
            at.timestamp_micros()
        ],
    )
    .map_err(backend)?;
    Ok(())
}

/// Copy the actuals of `ids` from plan `from` to plan `to` (a selection
/// carrying their `Complete` status). Merged into an existing row of `to`:
/// leased hours add up, evidence is `from`'s then `to`'s, and `to`'s
/// percent and actual hours win where set. Ids without a row in `from` are
/// skipped.
pub(crate) fn carry_actuals(
    conn: &Connection,
    from: &PlanId,
    to: &PlanId,
    ids: &[String],
    at: DateTime<Utc>,
) -> Result<(), PlannerError> {
    if ids.is_empty() {
        return Ok(());
    }
    let source = load_actuals(conn, from)?;
    let mut target = load_actuals(conn, to)?;
    for id in ids {
        let Some(old) = source.get(id) else {
            continue;
        };
        let new = target.remove(id).unwrap_or_default();
        let mut evidence = old.evidence.clone();
        evidence.extend(new.evidence);
        let evidence = serde_json::to_string(&evidence).map_err(backend)?;
        conn.execute(
            "INSERT OR REPLACE INTO ev_actuals
                 (plan_id, deliverable_id, earned_pct, actual_hours, leased_hours,
                  evidence, updated_at_us)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                to.0,
                id,
                new.earned_pct.or(old.earned_pct),
                new.actual_hours.or(old.actual_hours).map(f64::from),
                f64::from(old.leased_hours) + f64::from(new.leased_hours),
                evidence,
                at.timestamp_micros()
            ],
        )
        .map_err(backend)?;
    }
    Ok(())
}

/// Store `baseline` under its own `number` (which must be unused for the plan).
#[cfg_attr(not(test), allow(dead_code))] // written by plan.baseline (P5 Task 3)
pub(crate) fn insert_baseline(
    conn: &Connection,
    plan_id: &PlanId,
    baseline: &Baseline,
    reason: Option<&str>,
    created_at: DateTime<Utc>,
) -> Result<(), PlannerError> {
    let calendar = baseline
        .calendar
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(backend)?;
    let rows = serde_json::to_string(&baseline.rows).map_err(backend)?;
    conn.execute(
        "INSERT INTO baselines
             (plan_id, number, start_us, calendar, rows, bac, reason, created_at_us)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            plan_id.0,
            baseline.number,
            baseline.start.timestamp_micros(),
            calendar,
            rows,
            f64::from(baseline.bac),
            reason,
            created_at.timestamp_micros()
        ],
    )
    .map_err(backend)?;
    Ok(())
}

/// `baselines` columns after `plan_id`, in table order.
type BaselineColumns = (u32, i64, Option<String>, String, f64, Option<String>, i64);

/// The highest-numbered baseline of `plan_id`, if any.
///
/// Stored rows are validated: a `cost_rate` that is not finite and >= 0 is
/// a `BACKEND_ERROR` naming the plan. `finish_hours` is not stored; it is
/// the largest baseline row `ef` (0 for no rows), which equals the
/// `__finish__` earliest finish because lags are never negative.
#[cfg_attr(not(test), allow(dead_code))] // read by plan.baseline / plan.ev (P5 Task 3)
pub(crate) fn latest_baseline(
    conn: &Connection,
    plan_id: &PlanId,
) -> Result<Option<StoredBaseline>, PlannerError> {
    let row: Option<BaselineColumns> = conn
        .query_row(
            "SELECT number, start_us, calendar, rows, bac, reason, created_at_us
             FROM baselines WHERE plan_id = ?1 ORDER BY number DESC LIMIT 1",
            params![plan_id.0],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()
        .map_err(backend)?;
    let Some((number, start_us, calendar, rows, bac, reason, created_us)) = row else {
        return Ok(None);
    };
    let corrupt =
        |what: String| backend(anyhow::anyhow!("plan {plan_id}: baseline {number}: {what}"));
    let calendar: Option<Calendar> = calendar
        .map(|c| serde_json::from_str(&c))
        .transpose()
        .map_err(|e| corrupt(format!("stored calendar does not decode: {e}")))?;
    let rows: Vec<BaselineRow> = serde_json::from_str(&rows)
        .map_err(|e| corrupt(format!("stored rows do not decode: {e}")))?;
    if let Some(r) = rows
        .iter()
        .find(|r| !(r.cost_rate.is_finite() && r.cost_rate >= 0.0))
    {
        return Err(corrupt(format!(
            "row {:?} has cost_rate {}; it must be finite and >= 0",
            r.id, r.cost_rate
        )));
    }
    let finish_hours = rows.iter().map(|r| r.ef).fold(0.0, f32::max);
    Ok(Some(StoredBaseline {
        baseline: Baseline {
            number,
            start: dt(start_us, "baselines.start_us")?,
            calendar,
            rows,
            bac: bac as f32,
            finish_hours,
        },
        reason,
        created_at: dt(created_us, "baselines.created_at_us")?,
    }))
}

/// Store one snapshot. Two snapshots of one plan at the same `taken_at`
/// microsecond are refused (`BACKEND_ERROR`).
#[cfg_attr(not(test), allow(dead_code))] // written by plan.snapshot (P5 Task 3)
pub(crate) fn insert_snapshot(
    conn: &Connection,
    plan_id: &PlanId,
    taken_at: DateTime<Utc>,
    as_of: DateTime<Utc>,
    summary: &serde_json::Value,
) -> Result<(), PlannerError> {
    let summary = serde_json::to_string(summary).map_err(backend)?;
    conn.execute(
        "INSERT INTO ev_snapshots (plan_id, taken_at_us, as_of_us, summary)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            plan_id.0,
            taken_at.timestamp_micros(),
            as_of.timestamp_micros(),
            summary
        ],
    )
    .map_err(|e| {
        backend(anyhow::anyhow!(
            "plan {plan_id}: could not store snapshot taken at {taken_at}: {e}"
        ))
    })?;
    Ok(())
}

/// Every snapshot of `plan_id`, oldest `taken_at` first.
#[cfg_attr(not(test), allow(dead_code))] // read by plan.snapshot / plan.ev (P5 Task 3)
pub(crate) fn snapshots(
    conn: &Connection,
    plan_id: &PlanId,
) -> Result<Vec<EvSnapshot>, PlannerError> {
    let mut stmt = conn
        .prepare(
            "SELECT taken_at_us, as_of_us, summary FROM ev_snapshots
             WHERE plan_id = ?1 ORDER BY taken_at_us",
        )
        .map_err(backend)?;
    let rows = stmt
        .query_map(params![plan_id.0], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .map_err(backend)?;
    let mut out = Vec::new();
    for row in rows {
        let (taken, as_of, summary) = row.map_err(backend)?;
        out.push(EvSnapshot {
            taken_at: dt(taken, "ev_snapshots.taken_at_us")?,
            as_of: dt(as_of, "ev_snapshots.as_of_us")?,
            summary: serde_json::from_str(&summary).map_err(|e| {
                backend(anyhow::anyhow!(
                    "plan {plan_id}: stored snapshot summary does not decode: {e}"
                ))
            })?,
        });
    }
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;
    use crate::locks::PlanState;
    use crate::plan::{Deliverable, DeliverableStatus, PlanGraph};
    use crate::plan_store::SqlitePlanStore;
    use chrono::TimeZone;

    fn at(h: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 1, 5, h, 0, 0).unwrap()
    }

    fn store_with_plan() -> (SqlitePlanStore, PlanId) {
        let store = SqlitePlanStore::open_in_memory().unwrap();
        let graph = PlanGraph {
            deliverables: vec![Deliverable {
                id: "a".into(),
                owned_files: vec![],
                prerequisites: vec![],
                estimated_effort_hours: Some(2.0),
                duration_hours: None,
                estimate: None,
                metadata: serde_json::Value::Null,
                milestone: false,
                earning_rule: None,
            }],
            max_chained_dispatch: None,
        };
        let statuses = HashMap::from([("a".to_string(), DeliverableStatus::Ready)]);
        let plan_id = store
            .submit_or_get("h", || {
                Ok((
                    PlanId("p".into()),
                    PlanState::new(graph, statuses, Default::default()),
                ))
            })
            .unwrap();
        (store, plan_id)
    }

    fn baseline(number: u32, calendar: Option<Calendar>) -> Baseline {
        Baseline {
            number,
            start: at(0),
            calendar,
            rows: vec![BaselineRow {
                id: "a".into(),
                es: 0.0,
                ef: 2.0,
                budget: 4.0,
                cost_rate: 2.0,
            }],
            bac: 4.0,
            finish_hours: 2.0,
        }
    }

    fn raw_baseline(store: &SqlitePlanStore, plan_id: &PlanId, rows: &str) {
        store
            .write_tx(|tx| {
                tx.execute(
                    "INSERT INTO baselines
                         (plan_id, number, start_us, calendar, rows, bac, reason, created_at_us)
                     VALUES (?1, 1, 0, NULL, ?2, 1.0, NULL, 0)",
                    params![plan_id.0, rows],
                )
                .map_err(backend)?;
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn saving_state_twice_does_not_double_count_leased_hours() {
        let (store, plan_id) = store_with_plan();
        store
            .write_tx(|tx| {
                let mut state = crate::plan_store::load_plan_state(tx, &plan_id)?.unwrap();
                let lock = crate::plan::LockInfo {
                    plan_id: plan_id.clone(),
                    deliverable_id: "a".into(),
                    caller_id: crate::plan::CallerId("w".into()),
                    acquired_at: at(0),
                    expires_at: at(5),
                };
                state.record_lease_end(&lock, at(1));
                crate::plan_store::save_plan_state(tx, &plan_id, &mut state)?;
                crate::plan_store::save_plan_state(tx, &plan_id, &mut state)
            })
            .unwrap();
        let got = store.read_tx(|tx| load_actuals(tx, &plan_id)).unwrap();
        assert_eq!(got["a"].leased_hours, 1.0);
    }

    #[test]
    fn latest_baseline_is_none_before_any_baseline() {
        let (store, plan_id) = store_with_plan();
        let got = store.read_tx(|tx| latest_baseline(tx, &plan_id)).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn baseline_round_trips_through_the_store() {
        let (store, plan_id) = store_with_plan();
        let b = baseline(1, Some(Calendar::default()));
        store
            .write_tx(|tx| insert_baseline(tx, &plan_id, &b, Some("first"), at(1)))
            .unwrap();
        let got = store.read_tx(|tx| latest_baseline(tx, &plan_id)).unwrap();
        assert_eq!(
            got,
            Some(StoredBaseline {
                baseline: b,
                reason: Some("first".into()),
                created_at: at(1),
            })
        );
    }

    #[test]
    fn latest_baseline_is_the_highest_number() {
        let (store, plan_id) = store_with_plan();
        store
            .write_tx(|tx| {
                insert_baseline(tx, &plan_id, &baseline(2, None), Some("re"), at(2))?;
                insert_baseline(tx, &plan_id, &baseline(1, None), None, at(1))
            })
            .unwrap();
        let got = store.read_tx(|tx| latest_baseline(tx, &plan_id)).unwrap();
        assert_eq!(got.map(|s| s.baseline.number), Some(2));
    }

    #[test]
    fn baseline_number_cannot_be_reused() {
        let (store, plan_id) = store_with_plan();
        let err = store.write_tx(|tx| {
            insert_baseline(tx, &plan_id, &baseline(1, None), None, at(1))?;
            insert_baseline(tx, &plan_id, &baseline(1, None), None, at(2))
        });
        assert!(matches!(err, Err(PlannerError::BackendError(_))));
    }

    #[test]
    fn stored_baseline_with_negative_cost_rate_is_a_backend_error_naming_the_plan() {
        let (store, plan_id) = store_with_plan();
        raw_baseline(
            &store,
            &plan_id,
            r#"[{"id":"a","es":0,"ef":1,"budget":1,"cost_rate":-1}]"#,
        );
        let err = store
            .read_tx(|tx| latest_baseline(tx, &plan_id))
            .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("BACKEND_ERROR: plan p: baseline 1: row \"a\" has cost_rate -1"),
            "{err}"
        );
    }

    #[test]
    fn stored_baseline_with_infinite_cost_rate_is_a_backend_error() {
        let (store, plan_id) = store_with_plan();
        raw_baseline(
            &store,
            &plan_id,
            r#"[{"id":"a","es":0,"ef":1,"budget":1,"cost_rate":1e39}]"#,
        );
        let err = store.read_tx(|tx| latest_baseline(tx, &plan_id));
        assert!(matches!(err, Err(PlannerError::BackendError(_))));
    }

    #[test]
    fn stored_baseline_row_without_cost_rate_loads_rate_one() {
        let (store, plan_id) = store_with_plan();
        raw_baseline(&store, &plan_id, r#"[{"id":"a","es":0,"ef":1,"budget":1}]"#);
        let got = store
            .read_tx(|tx| latest_baseline(tx, &plan_id))
            .unwrap()
            .unwrap();
        assert_eq!(got.baseline.rows[0].cost_rate, 1.0);
    }

    #[test]
    fn baseline_finish_hours_is_the_latest_row_finish() {
        let (store, plan_id) = store_with_plan();
        raw_baseline(
            &store,
            &plan_id,
            r#"[{"id":"a","es":0,"ef":3,"budget":3},{"id":"b","es":0,"ef":5,"budget":5}]"#,
        );
        let got = store
            .read_tx(|tx| latest_baseline(tx, &plan_id))
            .unwrap()
            .unwrap();
        assert_eq!(got.baseline.finish_hours, 5.0);
    }

    #[test]
    fn load_actuals_is_empty_for_a_plan_without_actuals() {
        let (store, plan_id) = store_with_plan();
        let got = store.read_tx(|tx| load_actuals(tx, &plan_id)).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn leased_hours_add_up() {
        let (store, plan_id) = store_with_plan();
        store
            .write_tx(|tx| {
                add_leased_hours(tx, &plan_id, "a", 1.5, at(1))?;
                add_leased_hours(tx, &plan_id, "a", 0.5, at(2))
            })
            .unwrap();
        let got = store.read_tx(|tx| load_actuals(tx, &plan_id)).unwrap();
        assert_eq!(got["a"].leased_hours, 2.0);
    }

    #[test]
    fn reported_evidence_is_appended() {
        let (store, plan_id) = store_with_plan();
        let report = |e: &str| ReportedActuals {
            evidence: Some(e.into()),
            ..Default::default()
        };
        store
            .write_tx(|tx| {
                record_reported(tx, &plan_id, "a", &report("one"), at(1))?;
                record_reported(tx, &plan_id, "a", &report("two"), at(2))
            })
            .unwrap();
        let got = store.read_tx(|tx| load_actuals(tx, &plan_id)).unwrap();
        assert_eq!(
            got["a"].evidence,
            vec!["one".to_string(), "two".to_string()]
        );
    }

    #[test]
    fn a_report_keeps_fields_it_does_not_set() {
        let (store, plan_id) = store_with_plan();
        store
            .write_tx(|tx| {
                let first = ReportedActuals {
                    earned_pct: Some(40),
                    actual_hours: Some(3.0),
                    evidence: None,
                };
                record_reported(tx, &plan_id, "a", &first, at(1))?;
                let second = ReportedActuals {
                    earned_pct: Some(60),
                    ..Default::default()
                };
                record_reported(tx, &plan_id, "a", &second, at(2))
            })
            .unwrap();
        let got = store.read_tx(|tx| load_actuals(tx, &plan_id)).unwrap();
        assert_eq!(
            (got["a"].earned_pct, got["a"].actual_hours),
            (Some(60), Some(3.0))
        );
    }

    #[test]
    fn a_report_keeps_accumulated_leased_hours() {
        let (store, plan_id) = store_with_plan();
        store
            .write_tx(|tx| {
                add_leased_hours(tx, &plan_id, "a", 1.0, at(1))?;
                let report = ReportedActuals {
                    actual_hours: Some(3.0),
                    ..Default::default()
                };
                record_reported(tx, &plan_id, "a", &report, at(2))
            })
            .unwrap();
        let got = store.read_tx(|tx| load_actuals(tx, &plan_id)).unwrap();
        assert_eq!(got["a"].leased_hours, 1.0);
    }

    #[test]
    fn snapshots_come_back_oldest_first() {
        let (store, plan_id) = store_with_plan();
        store
            .write_tx(|tx| {
                insert_snapshot(tx, &plan_id, at(3), at(3), &serde_json::json!({"n": 2}))?;
                insert_snapshot(tx, &plan_id, at(1), at(1), &serde_json::json!({"n": 1}))
            })
            .unwrap();
        let got = store.read_tx(|tx| snapshots(tx, &plan_id)).unwrap();
        let order: Vec<_> = got.iter().map(|s| s.summary["n"].clone()).collect();
        assert_eq!(order, vec![serde_json::json!(1), serde_json::json!(2)]);
    }

    #[test]
    fn a_second_snapshot_at_the_same_instant_is_refused() {
        let (store, plan_id) = store_with_plan();
        let err = store.write_tx(|tx| {
            insert_snapshot(tx, &plan_id, at(1), at(1), &serde_json::json!({}))?;
            insert_snapshot(tx, &plan_id, at(1), at(2), &serde_json::json!({}))
        });
        assert!(matches!(err, Err(PlannerError::BackendError(_))));
    }
}
