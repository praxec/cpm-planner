//! Store-facing portfolio logic: named plan lines, their variants and every
//! plan's revision history (schema v3 tables `plan_lines`, `variants`,
//! `revisions`).
//!
//! Every function here runs inside a transaction its caller opened through
//! [`crate::plan_store::SqlitePlanStore::write_tx`] or `read_tx`, so a sync
//! or revision touches `plans`, statuses, locks, dedup and the portfolio
//! tables as ONE atomic step. Audit events are built by the planner from the
//! returned values after commit.
//!
//! `variants.content_hash` holds the hash of the plan file's bytes after a
//! file-backed sync (`source_path` set), or the canonical graph hash after an
//! inline sync or a revise. Drift detection may compare it against a file's
//! hash only when `source_path` is set; otherwise it is not a file hash.
//!
//! Revision numbering: a variant is created at revision 1. A legacy
//! (unnamed) plan has no `revisions` rows until its first revision, which
//! backfills revision 1 with the original graph.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::locks::PlanState;
use crate::plan::{
    DeliverableStatus, LockInfo, PlanGraph, PlanId, PlanLineSummary, PlannerError, SyncRequest,
    VariantSummary,
};
use crate::plan_store::{backend, insert_plan, load_plan_state, replace_plan_state};
use crate::planner::{all_complete, hash_graph};
use crate::revise::{RevisionDiff, plan_revision};

/// A committed revision, with what the planner needs to audit it.
pub(crate) struct Revised {
    pub(crate) revision: u32,
    pub(crate) diff: RevisionDiff,
    /// Expired locks reaped before revising (each one a lapse).
    pub(crate) reaped: Vec<LockInfo>,
    /// Live locks the forced revision released.
    pub(crate) released: Vec<LockInfo>,
    /// True when the graph was identical to the head: nothing was written
    /// and nothing is audited.
    pub(crate) no_op: bool,
    /// `Some(deliverable count)` when this revision flipped the plan from
    /// not-all-complete to all-complete (audited as `plan.completed`).
    pub(crate) completed: Option<usize>,
}

/// What a sync did.
pub(crate) enum Synced {
    Created { plan_id: PlanId, selected: bool },
    Unchanged { plan_id: PlanId, revision: u32 },
    Revised { plan_id: PlanId, revised: Revised },
}

struct VariantRow {
    plan_id: PlanId,
    head_revision: u32,
}

fn find_variant(
    tx: &Transaction<'_>,
    req: &SyncRequest,
) -> Result<Option<VariantRow>, PlannerError> {
    tx.query_row(
        "SELECT plan_id, head_revision FROM variants
         WHERE project = ?1 AND name = ?2 AND variant = ?3",
        params![req.project, req.name, req.variant],
        |r| {
            Ok(VariantRow {
                plan_id: PlanId(r.get(0)?),
                head_revision: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(backend)
}

fn stored_graph(tx: &Transaction<'_>, plan_id: &PlanId) -> Result<Option<PlanGraph>, PlannerError> {
    let json: Option<String> = tx
        .query_row(
            "SELECT graph FROM plans WHERE plan_id = ?1",
            params![plan_id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    json.map(|g| serde_json::from_str(&g).map_err(backend))
        .transpose()
}

fn insert_revision(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    revision: u32,
    graph: &PlanGraph,
    content_hash: &str,
    created_at_us: i64,
) -> Result<(), PlannerError> {
    tx.execute(
        "INSERT INTO revisions (plan_id, revision, graph, content_hash, created_at_us)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            plan_id.0,
            revision,
            serde_json::to_string(graph).map_err(backend)?,
            content_hash,
            created_at_us
        ],
    )
    .map_err(backend)?;
    Ok(())
}

fn max_revision(tx: &Transaction<'_>, plan_id: &PlanId) -> Result<Option<u32>, PlannerError> {
    tx.query_row(
        "SELECT MAX(revision) FROM revisions WHERE plan_id = ?1",
        params![plan_id.0],
        |r| r.get(0),
    )
    .map_err(backend)
}

/// Register or update the variant named by `req`. `graph_hash` is the
/// canonical hash of `req.graph` (already validated); `build` creates the
/// initial plan state for a new variant.
pub(crate) fn sync(
    tx: &Transaction<'_>,
    req: SyncRequest,
    graph_hash: &str,
    now: DateTime<Utc>,
    build: impl FnOnce(PlanGraph) -> Result<(PlanId, PlanState), PlannerError>,
) -> Result<Synced, PlannerError> {
    let content_hash = req
        .content_hash
        .clone()
        .unwrap_or_else(|| graph_hash.to_string());
    let Some(row) = find_variant(tx, &req)? else {
        return create(tx, req, &content_hash, now, build);
    };

    // Unchanged means the same canonical graph, whatever the file bytes.
    if stored_graph(tx, &row.plan_id)?.is_some_and(|g| hash_graph(&g) == graph_hash) {
        // The file may have been re-formatted or moved: track it, but the
        // plan itself has not changed.
        tx.execute(
            "UPDATE variants SET content_hash = ?1, source_path = COALESCE(?2, source_path)
             WHERE plan_id = ?3",
            params![content_hash, req.source_path, row.plan_id.0],
        )
        .map_err(backend)?;
        return Ok(Synced::Unchanged {
            plan_id: row.plan_id,
            revision: row.head_revision,
        });
    }

    let revised = revise(
        tx,
        &row.plan_id,
        req.graph,
        Some(&content_hash),
        req.source_path.as_deref(),
        req.force,
        now,
    )?;
    Ok(Synced::Revised {
        plan_id: row.plan_id,
        revised,
    })
}

fn create(
    tx: &Transaction<'_>,
    req: SyncRequest,
    content_hash: &str,
    now: DateTime<Utc>,
    build: impl FnOnce(PlanGraph) -> Result<(PlanId, PlanState), PlannerError>,
) -> Result<Synced, PlannerError> {
    let (plan_id, state) = build(req.graph)?;
    insert_plan(tx, &plan_id, &state, now)?;
    // First variant of a line (or of a line with no selection) is selected.
    tx.execute(
        "INSERT INTO plan_lines (project, name, selected_variant) VALUES (?1, ?2, ?3)
         ON CONFLICT (project, name) DO UPDATE
             SET selected_variant = COALESCE(plan_lines.selected_variant, excluded.selected_variant)",
        params![req.project, req.name, req.variant],
    )
    .map_err(backend)?;
    let selected_variant: Option<String> = tx
        .query_row(
            "SELECT selected_variant FROM plan_lines WHERE project = ?1 AND name = ?2",
            params![req.project, req.name],
            |r| r.get(0),
        )
        .map_err(backend)?;
    tx.execute(
        "INSERT INTO variants
             (project, name, variant, plan_id, source_path, content_hash, head_revision)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
        params![
            req.project,
            req.name,
            req.variant,
            plan_id.0,
            req.source_path,
            content_hash
        ],
    )
    .map_err(backend)?;
    insert_revision(
        tx,
        &plan_id,
        1,
        &state.graph,
        content_hash,
        now.timestamp_micros(),
    )?;
    Ok(Synced::Created {
        plan_id,
        selected: selected_variant.as_deref() == Some(req.variant.as_str()),
    })
}

/// Revise `plan_id` in place to `graph`: reap expired locks, carry progress
/// over ([`plan_revision`]), persist the new graph, CPM result, statuses and
/// locks, record the next revision and keep the dedup map pointing at the
/// graph the plan now holds. `content_hash` defaults to the graph hash;
/// `source_path` replaces the variant's when given.
pub(crate) fn revise(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    graph: PlanGraph,
    content_hash: Option<&str>,
    source_path: Option<&str>,
    force: bool,
    now: DateTime<Utc>,
) -> Result<Revised, PlannerError> {
    let mut state = load_plan_state(tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
        plan_id: plan_id.0.clone(),
    })?;
    let new_hash = hash_graph(&graph);
    if hash_graph(&state.graph) == new_hash {
        return Ok(Revised {
            revision: max_revision(tx, plan_id)?.unwrap_or(1),
            diff: RevisionDiff::default(),
            reaped: Vec::new(),
            released: Vec::new(),
            no_op: true,
            completed: None,
        });
    }
    let reaped = state.reap_expired(now);
    let was_complete = all_complete(&state);
    let (new_state, diff) = plan_revision(&state, &graph, force, now)?;
    let completed =
        (!was_complete && all_complete(&new_state)).then_some(new_state.graph.deliverables.len());
    let released: Vec<LockInfo> = diff
        .released_locks
        .iter()
        .filter_map(|id| state.locks.get(id).cloned())
        .collect();
    replace_plan_state(tx, plan_id, &new_state)?;

    // An unnamed plan (no `variants` row) must be found by the global dedup
    // under the graph it now holds; named plans are never in that map. If
    // another unnamed plan already owns the new hash, that mapping is kept
    // and the revised plan stays reachable by its id only.
    let named: bool = tx
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM variants WHERE plan_id = ?1)",
            params![plan_id.0],
            |r| r.get(0),
        )
        .map_err(backend)?;
    if !named {
        tx.execute(
            "DELETE FROM submit_dedup WHERE plan_id = ?1",
            params![plan_id.0],
        )
        .map_err(backend)?;
        tx.execute(
            "INSERT INTO submit_dedup (graph_hash, plan_id) VALUES (?1, ?2)
             ON CONFLICT (graph_hash) DO NOTHING",
            params![new_hash, plan_id.0],
        )
        .map_err(backend)?;
    }

    let head = match max_revision(tx, plan_id)? {
        Some(head) => head,
        None => {
            // Legacy plan: record the original graph as revision 1.
            let created_at_us: i64 = tx
                .query_row(
                    "SELECT created_at_us FROM plans WHERE plan_id = ?1",
                    params![plan_id.0],
                    |r| r.get(0),
                )
                .map_err(backend)?;
            insert_revision(
                tx,
                plan_id,
                1,
                &state.graph,
                &hash_graph(&state.graph),
                created_at_us,
            )?;
            1
        }
    };
    let revision = head + 1;
    let content_hash = content_hash.unwrap_or(&new_hash);
    insert_revision(
        tx,
        plan_id,
        revision,
        &graph,
        content_hash,
        now.timestamp_micros(),
    )?;
    tx.execute(
        "UPDATE variants SET head_revision = ?1, content_hash = ?2,
             source_path = COALESCE(?3, source_path)
         WHERE plan_id = ?4",
        params![revision, content_hash, source_path, plan_id.0],
    )
    .map_err(backend)?;

    Ok(Revised {
        revision,
        diff,
        reaped,
        released,
        no_op: false,
        completed,
    })
}

/// Plan lines of `project` sorted by name, variants sorted by variant.
pub(crate) fn list(
    tx: &Transaction<'_>,
    project: &str,
    include_archived: bool,
) -> Result<Vec<PlanLineSummary>, PlannerError> {
    let lines: Vec<(String, Option<String>, bool)> = {
        let mut stmt = tx
            .prepare(
                "SELECT name, selected_variant, archived FROM plan_lines
                 WHERE project = ?1 AND (?2 OR archived = 0) ORDER BY name",
            )
            .map_err(backend)?;
        stmt.query_map(params![project, include_archived], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .map_err(backend)?
        .collect::<Result<_, _>>()
        .map_err(backend)?
    };

    let mut out = Vec::with_capacity(lines.len());
    for (name, selected_variant, archived) in lines {
        let rows: Vec<(String, String, Option<String>, u32, bool)> = {
            let mut stmt = tx
                .prepare(
                    "SELECT variant, plan_id, source_path, head_revision, archived FROM variants
                     WHERE project = ?1 AND name = ?2 AND (?3 OR archived = 0)
                     ORDER BY variant",
                )
                .map_err(backend)?;
            stmt.query_map(params![project, name, include_archived], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(backend)?
            .collect::<Result<_, _>>()
            .map_err(backend)?
        };
        let mut variants = Vec::with_capacity(rows.len());
        for (variant, plan_id, source_path, head_revision, v_archived) in rows {
            let plan_id = PlanId(plan_id);
            let state =
                load_plan_state(tx, &plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
                    plan_id: plan_id.0.clone(),
                })?;
            let total = state.graph.deliverables.len();
            let complete = state
                .graph
                .deliverables
                .iter()
                .filter(|d| matches!(state.statuses.get(&d.id), Some(DeliverableStatus::Complete)))
                .count();
            variants.push(VariantSummary {
                selected: selected_variant.as_deref() == Some(variant.as_str()),
                variant,
                plan_id,
                archived: v_archived,
                head_revision,
                source_path,
                complete,
                total,
                plan_complete: complete == total,
                makespan: state.cached_result.critical_path_duration,
            });
        }
        out.push(PlanLineSummary {
            project: project.to_string(),
            name,
            selected_variant,
            archived,
            variants,
        });
    }
    Ok(out)
}

/// The graph of `revision` of `plan_id` (the head when `None`) and its
/// number. A plan without revision rows is at revision 1.
pub(crate) fn revision_graph(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    revision: Option<u32>,
) -> Result<(u32, PlanGraph), PlannerError> {
    let head_graph = stored_graph(tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
        plan_id: plan_id.0.clone(),
    })?;
    let head = max_revision(tx, plan_id)?.unwrap_or(1);
    let wanted = revision.unwrap_or(head);
    if wanted == head {
        return Ok((head, head_graph));
    }
    let json: Option<String> = tx
        .query_row(
            "SELECT graph FROM revisions WHERE plan_id = ?1 AND revision = ?2",
            params![plan_id.0, wanted],
            |r| r.get(0),
        )
        .optional()
        .map_err(backend)?;
    let json = json.ok_or_else(|| PlannerError::PlanNotFound {
        plan_id: format!("{} (revision {wanted})", plan_id.0),
    })?;
    Ok((wanted, serde_json::from_str(&json).map_err(backend)?))
}
