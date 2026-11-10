//! Thread affinity (docs/threads.md): a caller that completed a thread's
//! deliverable is offered its successor first; `thread` and
//! `affinity_seconds` on acquire; nothing bypasses file conflicts,
//! prerequisites or lease state; unthreaded plans lease as before.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use cpm_planner::affinity::{thread_token_error, thread_token_problem};
use cpm_planner::audit::NullAuditSink;
use cpm_planner::plan::{
    AcceptRequest, AcquireRequest, CallerId, Cohort, Deliverable, DeliverableStatus,
    MarkStatusRequest, PlanGraph, PlanId, ReviseRequest,
};
use cpm_planner::ports::Planner;
use cpm_planner::{BasicCpmPlanner, SqlitePlanStore};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn deliverable(id: &str, file: &str, prereqs: &[&str], hours: f32, metadata: Value) -> Deliverable {
    Deliverable {
        id: id.into(),
        owned_files: vec![file.into()],
        prerequisites: prereqs.iter().map(|p| (*p).into()).collect(),
        estimated_effort_hours: Some(hours),
        duration_hours: None,
        estimate: None,
        metadata,
        milestone: false,
        earning_rule: None,
    }
}

fn threaded(thread: &str) -> Value {
    json!({ "thread": thread })
}

/// `t1 -> t2` in thread `x`, plus `u`, an unthreaded long deliverable that
/// outranks `t2` by the usual priority (smaller latest start).
fn graph() -> PlanGraph {
    PlanGraph {
        deliverables: vec![
            deliverable("t1", "src/t1.rs", &[], 1.0, threaded("x")),
            deliverable("t2", "src/t2.rs", &["t1"], 1.0, threaded("x")),
            deliverable("u", "src/u.rs", &[], 10.0, Value::Null),
        ],
        max_chained_dispatch: None,
    }
}

fn caller(id: &str) -> CallerId {
    CallerId(id.into())
}

#[derive(Clone)]
struct Clock(Arc<Mutex<DateTime<Utc>>>);

impl Clock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(
            DateTime::parse_from_rfc3339("2026-11-10T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
        )))
    }

    fn advance(&self, seconds: i64) {
        let mut now = self.0.lock().unwrap();
        *now += chrono::Duration::seconds(seconds);
    }

    fn planner(&self) -> BasicCpmPlanner {
        let clock = self.0.clone();
        BasicCpmPlanner::new().with_clock(Arc::new(move || *clock.lock().unwrap()))
    }
}

fn ids(cohort: &Cohort) -> Vec<String> {
    cohort
        .rows
        .iter()
        .map(|r| r.deliverable.id.clone())
        .collect()
}

fn blocked_code(cohort: &Cohort, id: &str) -> Option<String> {
    cohort
        .blocked
        .iter()
        .find(|b| b.id == id)
        .map(|b| b.code.clone())
}

async fn acquire(planner: &BasicCpmPlanner, req: AcquireRequest) -> Cohort {
    planner.acquire_cohort(req).await.unwrap()
}

/// Lease `id` to `who` and mark it complete.
async fn complete(planner: &BasicCpmPlanner, plan_id: &PlanId, who: &str, id: &str) {
    acquire(
        planner,
        AcquireRequest::new(plan_id.clone(), caller(who), 1).with_ids(vec![id.into()]),
    )
    .await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            id,
            caller(who),
            DeliverableStatus::Complete,
        ))
        .await
        .unwrap();
}

/// A planner over `graph()` where `alice` completed `t1`.
async fn alice_did_t1(clock: &Clock) -> (BasicCpmPlanner, PlanId) {
    let planner = clock.planner();
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    complete(&planner, &plan_id, "alice", "t1").await;
    (planner, plan_id)
}

// ---------------------------------------------------------------------------
// Token rule
// ---------------------------------------------------------------------------

#[test]
fn a_short_token_is_valid() {
    assert_eq!(thread_token_problem("evidence-binding.v1_a"), None);
}

#[test]
fn an_empty_token_is_invalid() {
    assert!(thread_token_problem("").is_some());
}

#[test]
fn a_token_over_64_characters_is_invalid() {
    assert!(thread_token_problem(&"a".repeat(65)).is_some());
}

#[test]
fn a_token_with_a_space_is_invalid() {
    assert!(thread_token_problem("my thread").is_some());
}

#[test]
fn a_token_starting_with_punctuation_is_invalid() {
    assert!(thread_token_problem("-x").is_some());
}

#[test]
fn a_non_string_thread_value_is_invalid() {
    assert!(thread_token_error(&json!(7)).is_some());
}

#[tokio::test]
async fn an_invalid_thread_argument_is_refused() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    let err = planner
        .acquire_cohort(AcquireRequest::new(plan_id, caller("a"), 1).with_thread("not valid"))
        .await
        .unwrap_err();
    assert!(err.to_string().starts_with("INVALID_VALUE"));
}

// ---------------------------------------------------------------------------
// Preference
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_caller_that_completed_a_step_is_offered_the_next_step_first() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let cohort = acquire(&planner, AcquireRequest::new(plan_id, caller("alice"), 1)).await;
    assert_eq!(ids(&cohort), vec!["t2"]);
}

#[tokio::test]
async fn another_caller_is_offered_other_work_first() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let cohort = acquire(&planner, AcquireRequest::new(plan_id, caller("bob"), 1)).await;
    assert_eq!(ids(&cohort), vec!["u"]);
}

#[tokio::test]
async fn another_caller_may_still_take_the_next_step_without_a_hold() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let cohort = acquire(&planner, AcquireRequest::new(plan_id, caller("bob"), 2)).await;
    assert_eq!(ids(&cohort), vec!["u", "t2"]);
}

#[tokio::test]
async fn an_explicit_thread_is_offered_first() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("carol"), 1).with_thread("x"),
    )
    .await;
    assert_eq!(ids(&cohort), vec!["t1"]);
}

#[tokio::test]
async fn the_handoff_survives_a_new_planner_on_the_same_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plans.db");
    let open = |p: &Path| {
        BasicCpmPlanner::with_store(SqlitePlanStore::open(p).unwrap(), Arc::new(NullAuditSink))
    };
    let first = open(&path);
    let plan_id = first.submit_plan(graph()).await.unwrap();
    complete(&first, &plan_id, "alice", "t1").await;
    drop(first);
    let second = open(&path);
    let cohort = acquire(&second, AcquireRequest::new(plan_id, caller("alice"), 1)).await;
    assert_eq!(ids(&cohort), vec!["t2"]);
}

// ---------------------------------------------------------------------------
// Hold and expiry
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_deferring_caller_is_held_back_from_the_next_step() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("bob"), 2).with_affinity_seconds(600),
    )
    .await;
    assert_eq!(
        blocked_code(&cohort, "t2").as_deref(),
        Some("AFFINITY_HOLD")
    );
}

#[tokio::test]
async fn a_held_step_is_not_leased() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("bob"), 2).with_affinity_seconds(600),
    )
    .await;
    assert_eq!(ids(&cohort), vec!["u"]);
}

#[tokio::test]
async fn the_hold_expires_after_affinity_seconds() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    clock.advance(601);
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("bob"), 2).with_affinity_seconds(600),
    )
    .await;
    assert_eq!(ids(&cohort), vec!["u", "t2"]);
}

#[tokio::test]
async fn affinity_seconds_is_clamped_to_the_max_ttl() {
    let clock = Clock::new();
    let planner = clock.planner().with_max_ttl(Duration::from_secs(60));
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    complete(&planner, &plan_id, "alice", "t1").await;
    clock.advance(61);
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("bob"), 2).with_affinity_seconds(86_400),
    )
    .await;
    assert_eq!(ids(&cohort), vec!["u", "t2"]);
}

#[tokio::test]
async fn the_affine_caller_is_never_held_back() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("alice"), 1).with_affinity_seconds(600),
    )
    .await;
    assert_eq!(ids(&cohort), vec!["t2"]);
}

// ---------------------------------------------------------------------------
// Never a bypass
// ---------------------------------------------------------------------------

#[tokio::test]
async fn affinity_does_not_offer_a_step_whose_prerequisite_is_incomplete() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("carol"), 1)
            .with_ids(vec!["t2".into()])
            .with_thread("x"),
    )
    .await;
    assert_eq!(blocked_code(&cohort, "t2").as_deref(), Some("NOT_READY"));
}

#[tokio::test]
async fn affinity_does_not_offer_a_step_leased_to_someone_else() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    acquire(
        &planner,
        AcquireRequest::new(plan_id.clone(), caller("bob"), 1).with_ids(vec!["t2".into()]),
    )
    .await;
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("alice"), 1).with_ids(vec!["t2".into()]),
    )
    .await;
    assert_eq!(blocked_code(&cohort, "t2").as_deref(), Some("LOCKED"));
}

#[tokio::test]
async fn affinity_does_not_lease_a_manual_step() {
    let mut g = graph();
    g.deliverables[1].metadata = json!({"thread": "x", "kind": "manual"});
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(g).await.unwrap();
    complete(&planner, &plan_id, "alice", "t1").await;
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("alice"), 1).with_ids(vec!["t2".into()]),
    )
    .await;
    assert_eq!(blocked_code(&cohort, "t2").as_deref(), Some("MANUAL"));
}

#[tokio::test]
async fn affinity_does_not_bypass_a_file_conflict() {
    // A stored graph whose t2 claims the file u's live lease holds (only a
    // legacy or hand-edited store can hold one; submit refuses it).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("plans.db");
    let open = |p: &Path| {
        BasicCpmPlanner::with_store(SqlitePlanStore::open(p).unwrap(), Arc::new(NullAuditSink))
    };
    let first = open(&path);
    let plan_id = first.submit_plan(graph()).await.unwrap();
    complete(&first, &plan_id, "alice", "t1").await;
    acquire(
        &first,
        AcquireRequest::new(plan_id.clone(), caller("bob"), 1).with_ids(vec!["u".into()]),
    )
    .await;
    drop(first);
    let conn = rusqlite::Connection::open(&path).unwrap();
    let stored: String = conn
        .query_row(
            "SELECT graph FROM plans WHERE plan_id = ?1",
            [&plan_id.0],
            |r| r.get(0),
        )
        .unwrap();
    let mut g: PlanGraph = serde_json::from_str(&stored).unwrap();
    g.deliverables[1].owned_files = vec!["src/u.rs".into()];
    conn.execute(
        "UPDATE plans SET graph = ?1 WHERE plan_id = ?2",
        [&serde_json::to_string(&g).unwrap(), &plan_id.0],
    )
    .unwrap();
    drop(conn);
    let second = open(&path);
    let cohort = acquire(
        &second,
        AcquireRequest::new(plan_id, caller("alice"), 1).with_ids(vec!["t2".into()]),
    )
    .await;
    assert_eq!(
        blocked_code(&cohort, "t2").as_deref(),
        Some("FILE_CONFLICT")
    );
}

// ---------------------------------------------------------------------------
// What counts as a handoff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_step_records_no_handoff() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    acquire(
        &planner,
        AcquireRequest::new(plan_id.clone(), caller("alice"), 1).with_ids(vec!["t1".into()]),
    )
    .await;
    planner
        .mark_status(MarkStatusRequest::new(
            plan_id.clone(),
            "t1",
            caller("alice"),
            DeliverableStatus::Failed {
                reason: "red".into(),
            },
        ))
        .await
        .unwrap();
    let status = planner.status(&plan_id).await.unwrap();
    assert_eq!(status.threads[0].affine_caller, None);
}

#[tokio::test]
async fn an_accept_records_no_handoff() {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(graph()).await.unwrap();
    planner
        .accept(AcceptRequest::new(plan_id.clone(), "t1", "owner", "done"))
        .await
        .unwrap();
    let cohort = acquire(&planner, AcquireRequest::new(plan_id, caller("owner"), 1)).await;
    assert_eq!(ids(&cohort), vec!["u"]);
}

#[tokio::test]
async fn re_threading_the_completed_step_makes_its_handoff_inert() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let mut g = graph();
    g.deliverables[0].metadata = threaded("y");
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), g))
        .await
        .unwrap();
    let cohort = acquire(&planner, AcquireRequest::new(plan_id, caller("alice"), 1)).await;
    assert_eq!(ids(&cohort), vec!["u"]);
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_lists_the_thread_with_its_affine_caller_and_next_step() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    let thread = planner.status(&plan_id).await.unwrap().threads.remove(0);
    assert_eq!(
        (
            thread.thread.as_str(),
            thread.affine_caller.as_deref(),
            thread.next_ready.as_deref(),
            thread.completed,
            thread.total,
        ),
        ("x", Some("alice"), Some("t2"), 1, 2)
    );
}

#[tokio::test]
async fn status_names_the_holder_of_a_live_lease() {
    let clock = Clock::new();
    let (planner, plan_id) = alice_did_t1(&clock).await;
    acquire(
        &planner,
        AcquireRequest::new(plan_id.clone(), caller("alice"), 1),
    )
    .await;
    let thread = planner.status(&plan_id).await.unwrap().threads.remove(0);
    assert_eq!(thread.holder.as_deref(), Some("alice"));
}

#[tokio::test]
async fn an_unthreaded_status_has_no_threads_key() {
    let mut g = graph();
    for d in &mut g.deliverables {
        d.metadata = Value::Null;
    }
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(g).await.unwrap();
    let status = serde_json::to_value(planner.status(&plan_id).await.unwrap()).unwrap();
    assert!(status.get("threads").is_none());
}

// ---------------------------------------------------------------------------
// Unthreaded plans are unchanged
// ---------------------------------------------------------------------------

/// Eight independent deliverables with varied priorities and no threads,
/// leased all at once.
async fn unthreaded_cohort(metadata: Value) -> Vec<String> {
    let deliverables = (0..8)
        .map(|i| {
            deliverable(
                &format!("d{i}"),
                &format!("src/d{i}.rs"),
                &[],
                ((i * 5) % 7 + 1) as f32,
                metadata.clone(),
            )
        })
        .collect();
    let planner = BasicCpmPlanner::new();
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables,
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    let cohort = acquire(
        &planner,
        AcquireRequest::new(plan_id, caller("a"), 8)
            .with_thread("x")
            .with_affinity_seconds(600),
    )
    .await;
    ids(&cohort)
}

#[tokio::test]
async fn an_unthreaded_plan_leases_in_the_same_order_with_affinity_arguments() {
    let planner = BasicCpmPlanner::new();
    let deliverables = (0..8)
        .map(|i| {
            deliverable(
                &format!("d{i}"),
                &format!("src/d{i}.rs"),
                &[],
                ((i * 5) % 7 + 1) as f32,
                Value::Null,
            )
        })
        .collect();
    let plan_id = planner
        .submit_plan(PlanGraph {
            deliverables,
            max_chained_dispatch: None,
        })
        .await
        .unwrap();
    let plain = ids(&acquire(&planner, AcquireRequest::new(plan_id, caller("a"), 8)).await);
    assert_eq!(unthreaded_cohort(Value::Null).await, plain);
}

#[tokio::test]
async fn invalid_thread_tokens_lease_as_unthreaded() {
    assert_eq!(
        unthreaded_cohort(json!({"thread": "not a token"})).await,
        unthreaded_cohort(Value::Null).await
    );
}
