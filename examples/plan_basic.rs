//! End-to-end demonstration of `BasicCpmPlanner` used as a library.
//!
//! Run with:
//!
//! ```bash
//! cargo run -p cpm-planner --example plan_basic
//! ```
//!
//! This walks through the same surface an MCP client would drive over
//! the wire, but invokes the trait directly:
//!
//! 1. Submit a small deliverable graph (4 deliverables, mixed prereqs)
//! 2. Read the critical path
//! 3. Acquire a cohort, mark its deliverables complete, repeat
//! 4. Print the audit events captured along the way
//!
//! The example demonstrates that:
//! - Deliverables with no prerequisites start in `Ready` and are
//!   immediately acquirable.
//! - Dependents flip to `Ready` only when all their prerequisites are
//!   `Complete`.
//! - Locks are atomically acquired with the cohort and released by
//!   `mark_status(Complete)`.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use cpm_planner::BasicCpmPlanner;
use cpm_planner::audit::{AuditEvent, AuditSink};
use cpm_planner::plan::{CallerId, Deliverable, DeliverableStatus, PlanGraph};
use cpm_planner::ports::Planner;

/// A trivial audit sink that buffers every event for inspection.
#[derive(Debug, Default)]
struct BufferingAudit {
    events: Mutex<Vec<AuditEvent>>,
}

#[async_trait]
impl AuditSink for BufferingAudit {
    async fn record(&self, event: AuditEvent) -> anyhow::Result<()> {
        let mut events = self
            .events
            .lock()
            .map_err(|e| anyhow::anyhow!("audit buffer poisoned: {e}"))?;
        events.push(event);
        Ok(())
    }
}

fn deliverable(
    id: &str,
    owned_files: &[&str],
    prerequisites: &[&str],
    estimated_effort_hours: f32,
) -> Deliverable {
    Deliverable {
        id: id.to_string(),
        owned_files: owned_files.iter().map(PathBuf::from).collect(),
        prerequisites: prerequisites.iter().map(|s| s.to_string()).collect(),
        estimated_effort_hours: Some(estimated_effort_hours),
        metadata: serde_json::Value::Null,
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── 1. Build the planner with a capturing audit sink ────────────
    let audit = Arc::new(BufferingAudit::default());
    let planner = BasicCpmPlanner::with_audit(audit.clone());

    // ── 2. Submit a small graph ─────────────────────────────────────
    //
    //   d1 (1h) ─┐
    //            ├─► d3 (2h) ─► d4 (1h)
    //   d2 (3h) ─┘                   ▲
    //                                │
    //                            (critical
    //                             path: d2→d3→d4 = 6h)
    let graph = PlanGraph {
        deliverables: vec![
            deliverable("d1", &["src/a.rs"], &[], 1.0),
            deliverable("d2", &["src/b.rs"], &[], 3.0),
            deliverable("d3", &["src/c.rs"], &["d1", "d2"], 2.0),
            deliverable("d4", &["src/d.rs"], &["d3"], 1.0),
        ],
        max_chained_dispatch: None,
    };
    let plan_id = planner.submit_plan(graph).await?;
    println!("submitted plan: {plan_id}");

    // ── 3. Print the critical path ──────────────────────────────────
    let status = planner.status(&plan_id).await?;
    println!(
        "critical path: {:?} ({:.1}h total)",
        status.critical_path, status.critical_path_hours
    );

    // ── 4. Drive the plan to completion, cohort by cohort ───────────
    let caller = CallerId("demo-orchestrator".to_string());
    let mut round = 0;
    loop {
        round += 1;
        // Acquire up to 4 deliverables. The planner returns only those
        // whose prerequisites are Complete AND whose files don't overlap
        // with anything currently locked.
        let cohort = planner.acquire_cohort(&plan_id, &caller, 4).await?;
        if cohort.rows.is_empty() {
            // Two cases for empty: terminal (everything Complete) or
            // blocked (locks held by someone else, or no Ready work).
            // For this single-caller example, empty means terminal.
            println!("round {round}: no work remaining; plan is terminal.");
            break;
        }
        let ids: Vec<&str> = cohort
            .rows
            .iter()
            .map(|r| r.deliverable.id.as_str())
            .collect();
        println!("round {round}: acquired cohort {ids:?}");

        for row in &cohort.rows {
            planner
                .mark_status(
                    &plan_id,
                    &row.deliverable.id,
                    &caller,
                    DeliverableStatus::Complete,
                )
                .await?;
            println!("  marked {} complete", row.deliverable.id);
        }
    }

    // ── 5. Final status ─────────────────────────────────────────────
    let status = planner.status(&plan_id).await?;
    let complete = status
        .deliverables
        .iter()
        .filter(|(_, s, _, _, _)| matches!(s, DeliverableStatus::Complete))
        .count();
    println!(
        "final state: {complete}/{total} deliverables complete; {locks} locks held",
        total = status.deliverables.len(),
        locks = status.locks_held.len()
    );

    // ── 6. Audit trail ──────────────────────────────────────────────
    let events = audit
        .events
        .lock()
        .map_err(|e| anyhow::anyhow!("audit buffer poisoned: {e}"))?;
    println!("\naudit events ({} total):", events.len());
    for event in events.iter() {
        println!("  - {}", event.event_type);
    }

    Ok(())
}
