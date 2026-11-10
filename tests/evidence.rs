//! Evidence bound to its governed identity (docs/evidence.md, version 1):
//! record validation, scope rules on `mark_status` and `accept`, and the
//! current/stale verdict.

use cpm_planner::BasicCpmPlanner;
use cpm_planner::evidence::{
    DeliverableEvidenceStatus, Evidence, EvidenceRecord, Identity, IdentityField, Scope,
    StoredEvidence, Verdict, definition_digest,
};
use cpm_planner::plan::{
    AcceptRequest, AcquireRequest, CallerId, Deliverable, DeliverableStatus, MarkStatusRequest,
    PlanGraph, PlanId, ReviseRequest,
};
use cpm_planner::ports::Planner;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn deliverable(id: &str, files: &[&str], prereqs: &[&str], metadata: Value) -> Deliverable {
    Deliverable {
        id: id.into(),
        owned_files: files.iter().map(|f| (*f).into()).collect(),
        prerequisites: prereqs.iter().map(|p| (*p).into()).collect(),
        estimated_effort_hours: Some(1.0),
        duration_hours: None,
        estimate: None,
        metadata,
        milestone: false,
        earning_rule: None,
    }
}

fn identity(tree: &str) -> Identity {
    Identity::new()
        .with(IdentityField::Tree, tree)
        .with(IdentityField::DependencyLock, "sha256:lock1")
        .with(IdentityField::Toolchain, "rustc 1.89.0")
        .with(IdentityField::TestContract, "nextest:all@v1")
        .with(IdentityField::Environment, "linux-x86_64")
}

fn record(scope: Scope, tree: &str) -> EvidenceRecord {
    EvidenceRecord::new(scope, format!("{scope} green")).with_identity(identity(tree))
}

fn worker() -> CallerId {
    CallerId("w1".into())
}

/// `code` (work) -> `gate` (gate, candidate `code`) -> `approve` (manual,
/// declared `manual_approval`).
fn pipeline() -> PlanGraph {
    PlanGraph {
        deliverables: vec![
            deliverable("code", &["src/a.rs"], &[], json!({"artifact": "code"})),
            deliverable(
                "gate",
                &[],
                &["code"],
                json!({"artifact": "green run", "kind": "gate", "candidate": "code"}),
            ),
            deliverable(
                "approve",
                &[],
                &["gate"],
                json!({"artifact": "approval", "kind": "manual",
                       "evidence_scope": "manual_approval"}),
            ),
        ],
        max_chained_dispatch: None,
    }
}

async fn submit(graph: PlanGraph) -> (BasicCpmPlanner, PlanId) {
    let planner = BasicCpmPlanner::new();
    let plan_id = planner.submit_plan(graph).await.unwrap();
    (planner, plan_id)
}

/// Lease `code` and complete it with `evidence`.
async fn complete_code(planner: &BasicCpmPlanner, plan_id: &PlanId, evidence: impl Into<Evidence>) {
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
        .await
        .unwrap();
    planner
        .mark_status(
            MarkStatusRequest::new(
                plan_id.clone(),
                "code",
                worker(),
                DeliverableStatus::Complete,
            )
            .with_evidence(evidence),
        )
        .await
        .unwrap();
}

/// The pipeline with `code` complete on tree `t1`.
async fn code_done() -> (BasicCpmPlanner, PlanId) {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, record(Scope::Focused, "t1")).await;
    (planner, plan_id)
}

fn accept(plan_id: &PlanId, id: &str, evidence: impl Into<Evidence>) -> AcceptRequest {
    AcceptRequest::new(plan_id.clone(), id, "owner", evidence)
}

async fn status_of(
    planner: &BasicCpmPlanner,
    plan_id: &PlanId,
    id: &str,
    target: Option<&Identity>,
) -> DeliverableEvidenceStatus {
    planner
        .evidence_status(plan_id, target)
        .await
        .unwrap()
        .into_iter()
        .find(|s| s.deliverable_id == id)
        .unwrap()
}

fn code_of(err: impl std::fmt::Display) -> String {
    err.to_string()
        .split(':')
        .next()
        .unwrap_or_default()
        .to_string()
}

// ---------------------------------------------------------------------------
// Record and identity validation
// ---------------------------------------------------------------------------

#[test]
fn a_note_serialises_as_a_plain_json_string() {
    assert_eq!(
        serde_json::to_value(Evidence::from("merged")).unwrap(),
        json!("merged")
    );
}

#[test]
fn a_json_object_parses_as_a_record() {
    let parsed = Evidence::from_json(json!({
        "schema_version": 1, "scope": "manual_approval", "summary": "approved"
    }))
    .unwrap();
    assert_eq!(
        parsed,
        Evidence::Record(EvidenceRecord::new(Scope::ManualApproval, "approved"))
    );
}

#[test]
fn a_record_carrying_a_server_field_is_invalid() {
    let err = EvidenceRecord::from_json(json!({
        "schema_version": 1, "scope": "manual_approval", "summary": "ok",
        "recorded_by": "me"
    }))
    .unwrap_err();
    assert_eq!(code_of(err), "INVALID_EVIDENCE");
}

#[test]
fn a_record_with_an_unknown_field_is_invalid() {
    let err = EvidenceRecord::from_json(json!({
        "schema_version": 1, "scope": "manual_approval", "summary": "ok", "colour": "red"
    }))
    .unwrap_err();
    assert_eq!(code_of(err), "INVALID_EVIDENCE");
}

#[test]
fn an_unknown_scope_is_invalid() {
    let err = EvidenceRecord::from_json(json!({
        "schema_version": 1, "scope": "regression", "summary": "ok"
    }))
    .unwrap_err();
    assert_eq!(code_of(err), "INVALID_EVIDENCE");
}

#[test]
fn an_unknown_record_schema_version_is_invalid() {
    let mut r = EvidenceRecord::new(Scope::ManualApproval, "ok");
    r.schema_version = 2;
    assert_eq!(code_of(r.validate().unwrap_err()), "INVALID_EVIDENCE");
}

#[test]
fn a_gate_record_missing_an_identity_field_is_invalid() {
    let partial = Identity::new().with(IdentityField::Tree, "t1");
    let r = EvidenceRecord::new(Scope::Gate, "green").with_identity(partial);
    assert_eq!(code_of(r.validate().unwrap_err()), "INVALID_EVIDENCE");
}

#[test]
fn a_merge_record_needs_only_the_tree() {
    let r = EvidenceRecord::new(Scope::Merge, "merged")
        .with_identity(Identity::new().with(IdentityField::Tree, "t1"));
    assert!(r.validate().is_ok());
}

#[test]
fn an_external_signoff_without_party_is_invalid() {
    let r = EvidenceRecord::new(Scope::ExternalSignoff, "signed");
    assert_eq!(code_of(r.validate().unwrap_err()), "INVALID_EVIDENCE");
}

#[test]
fn party_on_another_scope_is_invalid() {
    let r = EvidenceRecord::new(Scope::ManualApproval, "ok").with_party("acme");
    assert_eq!(code_of(r.validate().unwrap_err()), "INVALID_EVIDENCE");
}

#[test]
fn an_identity_value_with_surrounding_whitespace_is_invalid() {
    let id = Identity::new().with(IdentityField::Tree, " t1");
    assert_eq!(code_of(id.validate().unwrap_err()), "INVALID_EVIDENCE");
}

#[test]
fn an_identity_value_over_256_characters_is_invalid() {
    let id = Identity::new().with(IdentityField::Tree, "t".repeat(257));
    assert_eq!(code_of(id.validate().unwrap_err()), "INVALID_EVIDENCE");
}

#[test]
fn the_definition_digest_ignores_metadata_key_order() {
    let a = deliverable("d", &[], &[], json!({"a": 1, "b": 2}));
    let b = deliverable("d", &[], &[], json!({"b": 2, "a": 1}));
    assert_eq!(definition_digest(&a), definition_digest(&b));
}

// ---------------------------------------------------------------------------
// Backward compatibility
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_string_still_completes_work() {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, "commit abc123").await;
    let status = planner.status(&plan_id).await.unwrap();
    assert!(
        status
            .deliverables
            .iter()
            .any(|r| r.0 == "code" && r.1 == DeliverableStatus::Complete)
    );
}

#[tokio::test]
async fn a_plain_string_still_accepts_an_undeclared_gate() {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, "commit abc123").await;
    assert!(
        planner
            .accept(accept(&plan_id, "gate", "ci run 7 green"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_stored_note_reads_back_unchanged() {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, "commit abc123").await;
    let evidence = planner.evidence(&plan_id).await.unwrap();
    assert_eq!(
        evidence["code"],
        vec![StoredEvidence::Note("commit abc123".into())]
    );
}

#[tokio::test]
async fn a_note_is_unbound() {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, "commit abc123").await;
    assert_eq!(
        status_of(&planner, &plan_id, "code", None).await.verdict,
        Verdict::Unbound
    );
}

// ---------------------------------------------------------------------------
// Scope boundaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn focused_evidence_cannot_close_a_gate() {
    let (planner, plan_id) = code_done().await;
    let err = planner
        .accept(accept(&plan_id, "gate", record(Scope::Focused, "t1")))
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "EVIDENCE_SCOPE_MISMATCH");
}

#[tokio::test]
async fn gate_evidence_cannot_close_a_manual_approval() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let err = planner
        .accept(accept(&plan_id, "approve", record(Scope::Gate, "t1")))
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "EVIDENCE_SCOPE_MISMATCH");
}

#[tokio::test]
async fn gate_evidence_may_close_work() {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, record(Scope::Gate, "t1")).await;
    let status = planner.status(&plan_id).await.unwrap();
    assert!(
        status
            .deliverables
            .iter()
            .any(|r| r.0 == "code" && r.1 == DeliverableStatus::Complete)
    );
}

#[tokio::test]
async fn a_declared_scope_refuses_a_note() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let err = planner
        .accept(accept(&plan_id, "approve", "looks good"))
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "EVIDENCE_REQUIRED");
}

#[tokio::test]
async fn a_declared_scope_is_met_by_a_matching_record() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let approval = EvidenceRecord::new(Scope::ManualApproval, "approved by owner");
    assert!(
        planner
            .accept(accept(&plan_id, "approve", approval))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_declared_scope_is_met_by_an_earlier_record() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let approval = EvidenceRecord::new(Scope::ManualApproval, "approved by owner");
    planner
        .mark_status(
            MarkStatusRequest::new(
                plan_id.clone(),
                "approve",
                CallerId("owner".into()),
                DeliverableStatus::InProgress,
            )
            .with_evidence(approval),
        )
        .await
        .unwrap();
    assert!(
        planner
            .accept(accept(&plan_id, "approve", "closing"))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_wrong_scope_record_rolls_back_the_mark() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let _ = planner
        .mark_status(
            MarkStatusRequest::new(
                plan_id.clone(),
                "approve",
                CallerId("owner".into()),
                DeliverableStatus::InProgress,
            )
            .with_evidence(record(Scope::Focused, "t1")),
        )
        .await;
    assert!(
        !planner
            .evidence(&plan_id)
            .await
            .unwrap()
            .contains_key("approve")
    );
}

#[tokio::test]
async fn an_invalid_declared_scope_refuses_a_record() {
    let mut graph = pipeline();
    graph.deliverables[0].metadata = json!({"artifact": "code", "evidence_scope": "gate"});
    let (planner, plan_id) = submit(graph).await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
        .await
        .unwrap();
    let err = planner
        .mark_status(
            MarkStatusRequest::new(
                plan_id.clone(),
                "code",
                worker(),
                DeliverableStatus::Complete,
            )
            .with_evidence(record(Scope::Focused, "t1")),
        )
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "EVIDENCE_SCOPE_MISMATCH");
}

// ---------------------------------------------------------------------------
// Gate candidate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_gate_closing_against_a_different_candidate_tree_is_refused() {
    let (planner, plan_id) = code_done().await;
    let err = planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t2")))
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "EVIDENCE_STALE");
}

#[tokio::test]
async fn a_gate_with_a_candidate_identity_refuses_a_note() {
    let (planner, plan_id) = code_done().await;
    let err = planner
        .accept(accept(&plan_id, "gate", "green"))
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "EVIDENCE_REQUIRED");
}

#[tokio::test]
async fn a_gate_matching_its_candidate_closes() {
    let (planner, plan_id) = code_done().await;
    assert!(
        planner
            .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_gate_is_judged_against_its_candidate_without_a_target() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let status = status_of(&planner, &plan_id, "gate", None).await;
    assert_eq!(
        status.record.unwrap().target_source.as_deref(),
        Some("candidate:code")
    );
}

// ---------------------------------------------------------------------------
// Current and stale
// ---------------------------------------------------------------------------

#[tokio::test]
async fn evidence_matching_the_target_is_current() {
    let (planner, plan_id) = code_done().await;
    let target = identity("t1");
    assert_eq!(
        status_of(&planner, &plan_id, "code", Some(&target))
            .await
            .verdict,
        Verdict::Current
    );
}

#[tokio::test]
async fn a_changed_dependency_lock_makes_evidence_stale() {
    let (planner, plan_id) = code_done().await;
    let target = identity("t1").with(IdentityField::DependencyLock, "sha256:lock2");
    assert_eq!(
        status_of(&planner, &plan_id, "code", Some(&target))
            .await
            .verdict,
        Verdict::Stale
    );
}

#[tokio::test]
async fn a_stale_verdict_names_the_changed_field() {
    let (planner, plan_id) = code_done().await;
    let target = identity("t1").with(IdentityField::TestContract, "nextest:all@v2");
    let status = status_of(&planner, &plan_id, "code", Some(&target)).await;
    let fields: Vec<IdentityField> = status
        .record
        .unwrap()
        .changed
        .iter()
        .map(|c| c.field)
        .collect();
    assert_eq!(fields, vec![IdentityField::TestContract]);
}

#[tokio::test]
async fn a_target_missing_a_pinned_field_is_unknown() {
    let (planner, plan_id) = code_done().await;
    let target = Identity::new().with(IdentityField::Tree, "t1");
    assert_eq!(
        status_of(&planner, &plan_id, "code", Some(&target))
            .await
            .verdict,
        Verdict::Unknown
    );
}

#[tokio::test]
async fn work_without_a_target_is_unknown() {
    let (planner, plan_id) = code_done().await;
    assert_eq!(
        status_of(&planner, &plan_id, "code", None).await.verdict,
        Verdict::Unknown
    );
}

#[tokio::test]
async fn a_deliverable_without_evidence_is_none() {
    let (planner, plan_id) = code_done().await;
    assert_eq!(
        status_of(&planner, &plan_id, "approve", None).await.verdict,
        Verdict::None
    );
}

#[tokio::test]
async fn a_revised_definition_makes_evidence_stale() {
    let (planner, plan_id) = code_done().await;
    let mut graph = pipeline();
    graph.deliverables[0].metadata = json!({"artifact": "code, revised"});
    planner
        .revise_plan(ReviseRequest::new(plan_id.clone(), graph))
        .await
        .unwrap();
    let target = identity("t1");
    let status = status_of(&planner, &plan_id, "code", Some(&target)).await;
    assert_eq!(
        status.record.unwrap().changed[0].field,
        IdentityField::Definition
    );
}

#[tokio::test]
async fn a_rebuilt_candidate_makes_the_gate_stale() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    // The candidate is rebuilt on a new tree after the gate ran.
    planner
        .mark_status(
            MarkStatusRequest::new(
                plan_id.clone(),
                "code",
                worker(),
                DeliverableStatus::Complete,
            )
            .with_evidence(record(Scope::Focused, "t2")),
        )
        .await
        .unwrap();
    assert_eq!(
        status_of(&planner, &plan_id, "gate", None).await.verdict,
        Verdict::Stale
    );
}

#[tokio::test]
async fn an_invalid_target_is_refused() {
    let (planner, plan_id) = code_done().await;
    let target = Identity::new().with(IdentityField::Tree, "");
    let err = planner
        .evidence_status(&plan_id, Some(&target))
        .await
        .unwrap_err();
    assert_eq!(code_of(err), "INVALID_EVIDENCE");
}

// ---------------------------------------------------------------------------
// Storage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stored_record_names_who_recorded_it() {
    let (planner, plan_id) = code_done().await;
    let evidence = planner.evidence(&plan_id).await.unwrap();
    assert_eq!(evidence["code"][0].as_record().unwrap().recorded_by, "w1");
}

#[tokio::test]
async fn a_stored_record_carries_the_definition_digest() {
    let (planner, plan_id) = code_done().await;
    let evidence = planner.evidence(&plan_id).await.unwrap();
    assert_eq!(
        evidence["code"][0].as_record().unwrap().definition_digest,
        definition_digest(&pipeline().deliverables[0])
    );
}

#[tokio::test]
async fn accept_stores_its_evidence() {
    let (planner, plan_id) = code_done().await;
    planner
        .accept(accept(&plan_id, "gate", record(Scope::Gate, "t1")))
        .await
        .unwrap();
    let evidence = planner.evidence(&plan_id).await.unwrap();
    assert_eq!(
        evidence["gate"][0].as_record().unwrap().recorded_by,
        "owner"
    );
}

#[tokio::test]
async fn accept_at_the_cap_drops_the_oldest_entry() {
    let (planner, plan_id) = submit(pipeline()).await;
    planner
        .acquire_cohort(AcquireRequest::new(plan_id.clone(), worker(), 1))
        .await
        .unwrap();
    for i in 0..100 {
        planner
            .mark_status(
                MarkStatusRequest::new(
                    plan_id.clone(),
                    "code",
                    worker(),
                    DeliverableStatus::InProgress,
                )
                .with_evidence(format!("e{i}")),
            )
            .await
            .unwrap();
    }
    planner
        .accept(
            AcceptRequest::new(plan_id.clone(), "code", "owner", "accepted").override_lock(true),
        )
        .await
        .unwrap();
    let evidence = planner.evidence(&plan_id).await.unwrap();
    assert_eq!(
        (evidence["code"].len(), evidence["code"][0].display_text()),
        (100, "e1")
    );
}

#[tokio::test]
async fn an_idempotent_accept_stores_nothing() {
    let (planner, plan_id) = submit(pipeline()).await;
    complete_code(&planner, &plan_id, "commit abc123").await;
    planner
        .accept(accept(&plan_id, "code", "again"))
        .await
        .unwrap();
    assert_eq!(planner.evidence(&plan_id).await.unwrap()["code"].len(), 1);
}
