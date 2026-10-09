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
//!
//! Selection: exactly one variant per line is selected, and only it is
//! executable ([`ensure_executable`], checked inside the execution
//! operation's own transaction). Plans without a `variants` row (unnamed or
//! legacy) are always executable. Archived variants and lines stay readable
//! but refuse sync and selection.

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};

use std::collections::{HashMap, HashSet};

use crate::graph::prerequisite_ids;
use crate::locks::{PlanState, rederive_status, release_file_claims};
use crate::plan::{
    Deliverable, DeliverableStatus, LockInfo, PlanGraph, PlanId, PlanLineSummary, PlannerError,
    SelectOutcome, SyncRequest, VariantSummary,
};
use crate::plan_store::{
    backend, insert_plan, load_plan_state, replace_plan_state, save_plan_state,
};
use crate::planner::{all_complete, canonical_deliverable, hash_graph};
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
    archived: bool,
}

fn find_variant(
    tx: &Transaction<'_>,
    req: &SyncRequest,
) -> Result<Option<VariantRow>, PlannerError> {
    tx.query_row(
        "SELECT plan_id, head_revision, archived FROM variants
         WHERE project = ?1 AND name = ?2 AND variant = ?3",
        params![req.project, req.name, req.variant],
        |r| {
            Ok(VariantRow {
                plan_id: PlanId(r.get(0)?),
                head_revision: r.get(1)?,
                archived: r.get(2)?,
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
    if line(tx, &req.project, &req.name)?.is_some_and(|l| l.archived) {
        return Err(line_archived(&req.name));
    }
    let Some(row) = find_variant(tx, &req)? else {
        return create(tx, req, &content_hash, now, build);
    };
    if row.archived {
        return Err(variant_archived(&req.variant, &req.name));
    }

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

/// Refuse to revise an effectively archived variant (its line or itself)
/// with `ARCHIVE_REFUSED`, as sync does. Unnamed plans always pass.
pub(crate) fn ensure_revisable(tx: &Transaction<'_>, plan_id: &PlanId) -> Result<(), PlannerError> {
    let Some((project, name, variant, v_archived)) = variant_of(tx, plan_id)? else {
        return Ok(());
    };
    if line(tx, &project, &name)?.is_some_and(|l| l.archived) {
        return Err(line_archived(&name));
    }
    if v_archived {
        return Err(variant_archived(&variant, &name));
    }
    Ok(())
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
                // Effective: an archived line archives every variant.
                archived: archived || v_archived,
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

// ---------------------------------------------------------------------------
// Selection, archiving and execution gating
// ---------------------------------------------------------------------------

/// A `plan_lines` row.
struct LineRow {
    selected_variant: Option<String>,
    archived: bool,
}

fn line(tx: &Transaction<'_>, project: &str, name: &str) -> Result<Option<LineRow>, PlannerError> {
    tx.query_row(
        "SELECT selected_variant, archived FROM plan_lines WHERE project = ?1 AND name = ?2",
        params![project, name],
        |r| {
            Ok(LineRow {
                selected_variant: r.get(0)?,
                archived: r.get(1)?,
            })
        },
    )
    .optional()
    .map_err(backend)
}

/// The `variants` row owning `plan_id`: `(project, name, variant, archived)`.
fn variant_of(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
) -> Result<Option<(String, String, String, bool)>, PlannerError> {
    tx.query_row(
        "SELECT project, name, variant, archived FROM variants WHERE plan_id = ?1",
        params![plan_id.0],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
    .map_err(backend)
}

fn variant_plan_id(
    tx: &Transaction<'_>,
    project: &str,
    name: &str,
    variant: &str,
) -> Result<Option<PlanId>, PlannerError> {
    tx.query_row(
        "SELECT plan_id FROM variants WHERE project = ?1 AND name = ?2 AND variant = ?3",
        params![project, name, variant],
        |r| Ok(PlanId(r.get(0)?)),
    )
    .optional()
    .map_err(backend)
}

fn line_archived(name: &str) -> PlannerError {
    PlannerError::ArchiveRefused {
        reason: format!("plan line '{name}' is archived"),
    }
}

fn variant_archived(variant: &str, name: &str) -> PlannerError {
    PlannerError::ArchiveRefused {
        reason: format!("variant '{variant}' of '{name}' is archived"),
    }
}

fn line_not_found(project: &str, name: &str) -> PlannerError {
    PlannerError::PlanNotFound {
        plan_id: format!("plan line '{name}' in project '{project}'"),
    }
}

/// Execution gate for a named variant: `Err(ArchiveRefused)` when it is
/// effectively archived (its line or itself), else `Err(VariantNotSelected)`
/// when it is not its line's selected variant. A plan without a `variants` row (unnamed or legacy)
/// always passes, as does an unknown plan id (the caller reports
/// `PLAN_NOT_FOUND`).
pub(crate) fn ensure_executable(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
) -> Result<(), PlannerError> {
    let row: Option<(String, String, Option<String>, bool, bool)> = tx
        .query_row(
            "SELECT v.name, v.variant, l.selected_variant, COALESCE(l.archived, 0), v.archived
             FROM variants v
             LEFT JOIN plan_lines l ON l.project = v.project AND l.name = v.name
             WHERE v.plan_id = ?1",
            params![plan_id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .optional()
        .map_err(backend)?;
    match row {
        Some((name, _, _, true, _)) => Err(PlannerError::ArchiveRefused {
            reason: format!("'{name}' is archived; unarchive it to resume execution"),
        }),
        Some((name, variant, _, false, true)) => Err(PlannerError::ArchiveRefused {
            reason: format!(
                "variant '{variant}' of '{name}' is archived; unarchive it to resume execution"
            ),
        }),
        Some((name, variant, selected, false, false))
            if selected.as_deref() != Some(variant.as_str()) =>
        {
            Err(PlannerError::VariantNotSelected {
                plan_id: plan_id.0.clone(),
                name,
                variant,
                selected: selected.unwrap_or_else(|| "none".to_string()),
            })
        }
        _ => Ok(()),
    }
}

/// A committed selection, with what the planner needs to audit it.
pub(crate) struct Selected {
    pub(crate) outcome: SelectOutcome,
    /// Expired locks reaped (each one a lapse) on the previous variant, then
    /// on the newly selected one.
    pub(crate) reaped: Vec<LockInfo>,
    /// The previous variant's live locks released by a forced selection.
    pub(crate) released: Vec<LockInfo>,
    /// `Some(deliverable count)` when the carry-over made the newly selected
    /// plan all-complete (audited as `plan.completed`).
    pub(crate) completed: Option<usize>,
}

/// Select the variant owning `plan_id` (see
/// [`crate::ports::Planner::select_variant`]). The previous variant's
/// expired locks are reaped first; its live locks refuse the selection with
/// `LOCK_HELD` unless `force`, which releases them and re-derives the
/// released deliverables. Progress then carries over to the new variant:
/// for every deliverable in both with an identical canonical definition the
/// counters are copied (the larger of each pair, so no lapse or failure
/// pressure is lost) and a `Complete` status is copied onto a deliverable
/// that is not `Complete` and not leased. A carried `Complete` whose new
/// variant has any non-`Complete` prerequisite (checked transitively after
/// carrying) is not carried after all, matching revise's reopen rule.
/// Finally every unleased `Ready`/`Pending` deliverable of the new variant
/// is re-derived.
pub(crate) fn select(
    tx: &Transaction<'_>,
    plan_id: &PlanId,
    force: bool,
    now: DateTime<Utc>,
) -> Result<Selected, PlannerError> {
    let (project, name, variant, v_archived) =
        variant_of(tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
            plan_id: format!("{} (not a named plan variant)", plan_id.0),
        })?;
    let line_row = line(tx, &project, &name)?.ok_or_else(|| line_not_found(&project, &name))?;
    if line_row.archived {
        return Err(line_archived(&name));
    }
    if v_archived {
        return Err(variant_archived(&variant, &name));
    }
    let previous = line_row.selected_variant;
    let mut selected = Selected {
        outcome: SelectOutcome {
            plan_id: plan_id.clone(),
            project: project.clone(),
            name: name.clone(),
            variant: variant.clone(),
            previous: previous.clone(),
            changed: false,
            carried: Vec::new(),
            released_locks: Vec::new(),
        },
        reaped: Vec::new(),
        released: Vec::new(),
        completed: None,
    };
    if previous.as_deref() == Some(variant.as_str()) {
        return Ok(selected);
    }

    let old_plan_id = match &previous {
        Some(prev) => variant_plan_id(tx, &project, &name, prev)?,
        None => None,
    };
    let old_state = match &old_plan_id {
        Some(old_id) => {
            let mut old =
                load_plan_state(tx, old_id)?.ok_or_else(|| PlannerError::PlanNotFound {
                    plan_id: old_id.0.clone(),
                })?;
            selected.reaped = old.reap_expired(now);
            let mut live: Vec<LockInfo> = old.locks.values().cloned().collect();
            live.sort_by(|a, b| a.deliverable_id.cmp(&b.deliverable_id));
            if let Some(lock) = live.first()
                && !force
            {
                return Err(PlannerError::LockHeld {
                    plan_id: old_id.0.clone(),
                    deliverable_id: lock.deliverable_id.clone(),
                    holder: lock.caller_id.0.clone(),
                });
            }
            for lock in &live {
                release_lock(&mut old, &lock.deliverable_id);
            }
            selected.outcome.released_locks =
                live.iter().map(|l| l.deliverable_id.clone()).collect();
            selected.released = live;
            save_plan_state(tx, old_id, &old)?;
            Some(old)
        }
        None => None,
    };

    let mut new = load_plan_state(tx, plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
        plan_id: plan_id.0.clone(),
    })?;
    selected.reaped.extend(new.reap_expired(now));
    let was_complete = all_complete(&new);
    if let Some(old) = &old_state {
        selected.outcome.carried = carry_progress(old, &mut new);
    }
    selected.completed =
        (!was_complete && all_complete(&new)).then_some(new.graph.deliverables.len());
    save_plan_state(tx, plan_id, &new)?;

    tx.execute(
        "UPDATE plan_lines SET selected_variant = ?1 WHERE project = ?2 AND name = ?3",
        params![variant, project, name],
    )
    .map_err(backend)?;
    selected.outcome.changed = true;
    Ok(selected)
}

/// Drop `deliverable_id`'s lock and file claims and re-derive its status.
fn release_lock(state: &mut PlanState, deliverable_id: &str) {
    state.locks.remove(deliverable_id);
    if let Some(d) = state
        .graph
        .deliverables
        .iter()
        .find(|d| d.id == deliverable_id)
    {
        release_file_claims(&mut state.file_claims, deliverable_id, &d.owned_files);
        let status = rederive_status(d, &state.statuses);
        state.statuses.insert(deliverable_id.to_string(), status);
    }
}

/// Copy progress from `old` onto `new` (see [`select`]); returns the sorted
/// ids whose `Complete` status was carried.
fn carry_progress(old: &PlanState, new: &mut PlanState) -> Vec<String> {
    let old_by_id: HashMap<&str, &Deliverable> = old
        .graph
        .deliverables
        .iter()
        .map(|d| (d.id.as_str(), d))
        .collect();
    let mut carried = Vec::new();
    for d in &new.graph.deliverables {
        let Some(o) = old_by_id.get(d.id.as_str()) else {
            continue;
        };
        if canonical_deliverable(o) != canonical_deliverable(d) {
            continue;
        }
        let id = d.id.as_str();
        for (counts, old_count) in [
            (&mut new.attempt_counts, old.attempt_count(id)),
            (&mut new.failure_counts, old.failure_count(id)),
            (&mut new.lapse_counts, old.lapse_count(id)),
        ] {
            if old_count > 0 {
                let c = counts.entry(id.to_string()).or_insert(0);
                *c = (*c).max(old_count);
            }
        }
        let old_complete = old.statuses.get(id) == Some(&DeliverableStatus::Complete);
        let new_open = new.statuses.get(id) != Some(&DeliverableStatus::Complete)
            && !new.locks.contains_key(id);
        if old_complete && new_open {
            new.statuses
                .insert(id.to_string(), DeliverableStatus::Complete);
            carried.push(id.to_string());
        }
    }
    // A carried `Complete` must not sit above an open prerequisite in the
    // new variant (revise's reopen rule): un-carry it, then re-check, since
    // un-carrying may open a prerequisite of another carried deliverable.
    let mut kept: HashSet<String> = carried.iter().cloned().collect();
    loop {
        let open_above: Vec<String> = new
            .graph
            .deliverables
            .iter()
            .filter(|d| kept.contains(&d.id))
            .filter(|d| {
                prerequisite_ids(d)
                    .any(|p| new.statuses.get(p) != Some(&DeliverableStatus::Complete))
            })
            .map(|d| d.id.clone())
            .collect();
        if open_above.is_empty() {
            break;
        }
        for id in open_above {
            // Placeholder; the re-derivation below sets Ready/Pending.
            new.statuses.insert(id.clone(), DeliverableStatus::Pending);
            kept.remove(&id);
        }
    }
    carried.retain(|id| kept.contains(id));
    let rederived: Vec<(String, DeliverableStatus)> = new
        .graph
        .deliverables
        .iter()
        .filter(|d| {
            matches!(
                new.statuses.get(&d.id),
                Some(DeliverableStatus::Ready | DeliverableStatus::Pending)
            ) && !new.locks.contains_key(&d.id)
        })
        .map(|d| (d.id.clone(), rederive_status(d, &new.statuses)))
        .collect();
    new.statuses.extend(rederived);
    carried.sort();
    carried
}

/// A committed archive or unarchive.
pub(crate) struct Archived {
    /// The variant (with its plan id) whose own flag changed; empty for a
    /// line operation, which never touches variant flags.
    pub(crate) variants: Vec<(String, PlanId)>,
    /// True when the line's own flag changed.
    pub(crate) line: bool,
    /// Expired locks of the selected variant reaped before a line archive.
    pub(crate) reaped: Vec<LockInfo>,
    /// Live locks of the selected variant released by a forced line archive.
    pub(crate) released: Vec<LockInfo>,
}

impl Archived {
    pub(crate) fn changed(&self) -> bool {
        self.line || !self.variants.is_empty()
    }
}

/// Set the archived flag (`archived`) of the line `(project, name)` with
/// every variant (`variant: None`), or of one variant.
///
/// The line flag and the variant flags are independent; a variant is
/// effectively archived when `line.archived || variant.archived`. Archiving
/// one variant refuses the selected variant. Archiving a line sets only the
/// line flag, after reaping the selected variant's expired locks; a live
/// lock refuses with `LOCK_HELD` unless `force`, which releases it (an
/// archived line is not executable, so nobody could complete the lease).
/// Unarchiving a line clears only the line flag; unarchiving one variant of
/// an archived line is refused. Setting a flag to its current value changes
/// nothing.
pub(crate) fn archive(
    tx: &Transaction<'_>,
    project: &str,
    name: &str,
    variant: Option<&str>,
    archived: bool,
    force: bool,
    now: DateTime<Utc>,
) -> Result<Archived, PlannerError> {
    let line_row = line(tx, project, name)?.ok_or_else(|| line_not_found(project, name))?;
    let mut out = Archived {
        variants: Vec::new(),
        line: false,
        reaped: Vec::new(),
        released: Vec::new(),
    };
    match variant {
        None => {
            if archived && !line_row.archived {
                release_selected_locks(tx, project, name, &line_row, force, now, &mut out)?;
            }
            // Only the line's own flag: variant flags are independent, so
            // unarchiving the line restores each variant as it was.
            tx.execute(
                "UPDATE plan_lines SET archived = ?1 WHERE project = ?2 AND name = ?3",
                params![archived, project, name],
            )
            .map_err(backend)?;
            out.line = line_row.archived != archived;
        }
        Some(v) => {
            let Some(plan_id) = variant_plan_id(tx, project, name, v)? else {
                return Err(PlannerError::PlanNotFound {
                    plan_id: format!("variant '{v}' of plan line '{name}' in project '{project}'"),
                });
            };
            if archived && line_row.selected_variant.as_deref() == Some(v) {
                return Err(PlannerError::ArchiveRefused {
                    reason: format!(
                        "cannot archive the selected variant '{v}' of '{name}'; select another \
                         variant first"
                    ),
                });
            }
            if !archived && line_row.archived {
                return Err(PlannerError::ArchiveRefused {
                    reason: "unarchive the line first".to_string(),
                });
            }
            let changed = tx
                .execute(
                    "UPDATE variants SET archived = ?1
                     WHERE project = ?2 AND name = ?3 AND variant = ?4 AND archived <> ?1",
                    params![archived, project, name, v],
                )
                .map_err(backend)?;
            if changed > 0 {
                out.variants.push((v.to_string(), plan_id));
            }
        }
    }
    Ok(out)
}

/// Before a line archive: reap the selected variant's expired locks, then
/// refuse on a live one unless `force`, which releases them all.
fn release_selected_locks(
    tx: &Transaction<'_>,
    project: &str,
    name: &str,
    line_row: &LineRow,
    force: bool,
    now: DateTime<Utc>,
    out: &mut Archived,
) -> Result<(), PlannerError> {
    let Some(selected) = &line_row.selected_variant else {
        return Ok(());
    };
    let Some(plan_id) = variant_plan_id(tx, project, name, selected)? else {
        return Ok(());
    };
    let mut state = load_plan_state(tx, &plan_id)?.ok_or_else(|| PlannerError::PlanNotFound {
        plan_id: plan_id.0.clone(),
    })?;
    out.reaped = state.reap_expired(now);
    let mut live: Vec<LockInfo> = state.locks.values().cloned().collect();
    live.sort_by(|a, b| a.deliverable_id.cmp(&b.deliverable_id));
    if let Some(lock) = live.first()
        && !force
    {
        return Err(PlannerError::LockHeld {
            plan_id: plan_id.0.clone(),
            deliverable_id: lock.deliverable_id.clone(),
            holder: lock.caller_id.0.clone(),
        });
    }
    for lock in &live {
        release_lock(&mut state, &lock.deliverable_id);
    }
    out.released = live;
    save_plan_state(tx, &plan_id, &state)
}
