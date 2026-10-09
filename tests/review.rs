//! P6 Task 2: the `plan.review` engine. Every test drives a scripted fake
//! [`JudgmentModel`]; nothing here touches the network.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use cpm_planner::edits::GraphEdit;
use cpm_planner::llm::{
    Answer, Decisions, JudgmentError, JudgmentErrorKind, JudgmentModel, LlmConfig, Question, Usage,
};
use cpm_planner::plan::PlanGraph;
use cpm_planner::resource_schedule::ScheduleRequest;
use cpm_planner::review::{
    FindingKind, Judge, MAX_QUESTIONS, NO_KEY_REASON, ReviewReport, ReviewRequest, ReviewStatus,
    review,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------- fake judge

type Script = BTreeMap<(String, Vec<String>), Answer>;

/// Records every call; answers each question from `script` keyed by the
/// question's `(kind, ids)`, else with a "no" answer.
#[derive(Default)]
struct FakeJudge {
    calls: Mutex<Vec<(Value, BTreeMap<String, Question>)>>,
    script: Script,
    fail: Option<JudgmentError>,
    host: Option<String>,
    usage: Option<Usage>,
    model: Option<String>,
    omit: Vec<(String, Vec<String>)>,
}

fn instructions(q: &Question) -> &Value {
    match q {
        Question::Choice { instructions, .. }
        | Question::Score { instructions, .. }
        | Question::Noul { instructions, .. } => instructions,
    }
}

fn key_of(q: &Question) -> (String, Vec<String>) {
    let i = instructions(q);
    let kind = i["kind"].as_str().unwrap_or_default().to_string();
    let ids = i["ids"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();
    (kind, ids)
}

fn default_answer(q: &Question) -> Answer {
    match q {
        Question::Noul { .. } => Answer::Noul { noul: 0.0 },
        Question::Choice { criteria, .. } => Answer::Choice {
            choice: "none".to_string(),
            probabilities: criteria
                .keys()
                .map(|k| (k.clone(), if k == "none" { 1.0 } else { 0.0 }))
                .collect(),
            confidence: 1.0,
        },
        Question::Score { criteria, .. } => Answer::Score {
            score: 0.0,
            probabilities: (0..criteria.len())
                .map(|i| (i.to_string(), if i == 0 { 1.0 } else { 0.0 }))
                .collect(),
            legend: criteria
                .iter()
                .enumerate()
                .map(|(i, v)| (i.to_string(), v.clone()))
                .collect(),
            confidence: 1.0,
        },
    }
}

impl FakeJudge {
    fn new() -> Self {
        Self::default()
    }

    fn with(mut self, kind: &str, ids: &[&str], answer: Answer) -> Self {
        self.script.insert(
            (
                kind.to_string(),
                ids.iter().map(|s| s.to_string()).collect(),
            ),
            answer,
        );
        self
    }

    fn noul(self, kind: &str, ids: &[&str], p: f64) -> Self {
        self.with(kind, ids, Answer::Noul { noul: p })
    }

    fn choice(self, ids: &[&str], choice: &str, p: f64) -> Self {
        let mut probabilities: BTreeMap<String, f64> =
            ["add_capacity", "fast_track", "reduce_scope", "none"]
                .iter()
                .map(|k| (k.to_string(), 0.0))
                .collect();
        probabilities.insert(choice.to_string(), p);
        self.with(
            "crash_option",
            ids,
            Answer::Choice {
                choice: choice.to_string(),
                probabilities,
                confidence: p,
            },
        )
    }

    fn failing(mut self, error: JudgmentError) -> Self {
        self.fail = Some(error);
        self
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn questions_sent(&self) -> usize {
        self.calls.lock().unwrap().first().map_or(0, |c| c.1.len())
    }

    fn kinds_sent(&self) -> Vec<(String, Vec<String>)> {
        self.calls
            .lock()
            .unwrap()
            .first()
            .map(|c| c.1.values().map(key_of).collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl JudgmentModel for FakeJudge {
    async fn decide(
        &self,
        state: Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        self.calls.lock().unwrap().push((state, questions.clone()));
        if let Some(e) = &self.fail {
            return Err(e.clone());
        }
        let answers = questions
            .iter()
            .filter(|(_, q)| !self.omit.contains(&key_of(q)))
            .map(|(id, q)| {
                let answer = self
                    .script
                    .get(&key_of(q))
                    .cloned()
                    .unwrap_or_else(|| default_answer(q));
                (id.clone(), answer)
            })
            .collect();
        Ok(Decisions {
            answers,
            model: self.model.clone().unwrap_or_else(|| "fake/jev".to_string()),
            usage: self.usage,
        })
    }

    fn endpoint_host(&self) -> Option<String> {
        self.host.clone()
    }
}

// ---------------------------------------------------------------- graphs

fn dv(id: &str, prereqs: &[&str], h: f32) -> Value {
    json!({
        "id": id,
        "owned_files": [],
        "prerequisites": prereqs,
        "estimated_effort_hours": h,
    })
}

fn graph(v: Vec<Value>) -> PlanGraph {
    serde_json::from_value(json!({ "deliverables": v })).expect("valid graph")
}

/// `a`(4) -> `c`(2); `b`(1). Makespan 6; without a -> c it is 4.
fn two_chains() -> PlanGraph {
    graph(vec![
        dv("a", &[], 4.0),
        dv("b", &[], 1.0),
        dv("c", &["a"], 2.0),
    ])
}

/// `a`(10) -> `c`(2); `b`(1). Makespan 12.
fn long_head() -> PlanGraph {
    graph(vec![
        dv("a", &[], 10.0),
        dv("b", &[], 1.0),
        dv("c", &["a"], 2.0),
    ])
}

/// `a`(5) -> `c`(1) -> `m` (milestone). Removing a -> c leaves `a` feeding
/// no milestone (a new FEEDS_NO_MILESTONE warning) while saving an hour.
fn milestone_chain() -> PlanGraph {
    let mut m = dv("m", &["c"], 0.0);
    m["milestone"] = json!(true);
    graph(vec![dv("a", &[], 5.0), dv("c", &["a"], 1.0), m])
}

/// 40 independent 1h deliverables plus a 40-long chain: far more than 64
/// candidate questions.
fn large_graph() -> PlanGraph {
    let mut v = Vec::new();
    for i in 0..40 {
        v.push(dv(&format!("free{i:02}"), &[], 1.0));
    }
    v.push(dv("chain00", &[], 2.0));
    for i in 1..40 {
        let prev = format!("chain{:02}", i - 1);
        v.push(dv(&format!("chain{i:02}"), &[prev.as_str()], 2.0));
    }
    graph(v)
}

fn cyclic() -> PlanGraph {
    graph(vec![dv("a", &["b"], 1.0), dv("b", &["a"], 1.0)])
}

async fn run(g: &PlanGraph, judge: &FakeJudge) -> ReviewReport {
    review(g, &ReviewRequest::default(), Judge::Model(judge))
        .await
        .expect("review")
}

async fn run_with(g: &PlanGraph, req: &ReviewRequest, judge: &FakeJudge) -> ReviewReport {
    review(g, req, Judge::Model(judge)).await.expect("review")
}

fn proposal_ids(r: &ReviewReport) -> Vec<String> {
    r.proposals.iter().map(|p| p.id.clone()).collect()
}

// ---------------------------------------------------------------- no key / config

#[tokio::test]
async fn review_without_a_judge_is_unavailable_with_the_no_key_reason() {
    let r = review(
        &two_chains(),
        &ReviewRequest::default(),
        Judge::NotConfigured,
    )
    .await
    .unwrap();
    assert_eq!(
        (r.status, r.reason.as_deref()),
        (ReviewStatus::ReviewUnavailable, Some(NO_KEY_REASON))
    );
}

#[tokio::test]
async fn review_without_a_judge_still_reports_lint() {
    let g = two_chains();
    let r = review(&g, &ReviewRequest::default(), Judge::NotConfigured)
        .await
        .unwrap();
    assert_eq!(r.lint, cpm_planner::lint::lint(&g));
}

#[tokio::test]
async fn review_with_a_config_error_reports_its_reason() {
    let r = review(
        &two_chains(),
        &ReviewRequest::default(),
        Judge::ConfigError("key file ignored: world-readable"),
    )
    .await
    .unwrap();
    assert_eq!(
        r.reason.as_deref(),
        Some("key file ignored: world-readable")
    );
}

#[test]
fn no_key_reason_names_openrouter() {
    assert_eq!(NO_KEY_REASON, "no OpenRouter key configured");
}

// ---------------------------------------------------------------- lint short-circuit

#[tokio::test]
async fn review_of_a_graph_with_lint_errors_is_invalid_graph() {
    let r = run(&cyclic(), &FakeJudge::new()).await;
    assert_eq!(r.status, ReviewStatus::InvalidGraph);
}

#[tokio::test]
async fn review_of_a_graph_with_lint_errors_never_calls_the_judge() {
    let judge = FakeJudge::new();
    run(&cyclic(), &judge).await;
    assert_eq!(judge.call_count(), 0);
}

// ---------------------------------------------------------------- batching and caps

#[tokio::test]
async fn review_calls_the_judge_exactly_once() {
    let judge = FakeJudge::new();
    run(&large_graph(), &judge).await;
    assert_eq!(judge.call_count(), 1);
}

#[tokio::test]
async fn review_sends_at_most_64_questions_by_default() {
    let judge = FakeJudge::new();
    run(&large_graph(), &judge).await;
    assert_eq!(judge.questions_sent(), usize::from(MAX_QUESTIONS));
}

#[tokio::test]
async fn review_honours_a_smaller_max_questions() {
    let judge = FakeJudge::new();
    let req = ReviewRequest {
        max_questions: Some(10),
        ..ReviewRequest::default()
    };
    run_with(&large_graph(), &req, &judge).await;
    assert_eq!(judge.questions_sent(), 10);
}

#[tokio::test]
async fn review_question_count_matches_questions_sent() {
    let judge = FakeJudge::new();
    let r = run(&large_graph(), &judge).await;
    assert_eq!(r.question_count, judge.questions_sent());
}

#[tokio::test]
async fn review_flags_truncation_when_candidates_exceed_the_cap() {
    let r = run(&large_graph(), &FakeJudge::new()).await;
    assert!(r.truncated);
}

#[tokio::test]
async fn review_of_a_small_graph_is_not_truncated() {
    let r = run(&two_chains(), &FakeJudge::new()).await;
    assert!(!r.truncated);
}

/// [`large_graph`] plus a 20h free deliverable (a split candidate) and two
/// unordered deliverables sharing an owner (a missing-dependency pair).
fn every_kind_graph() -> PlanGraph {
    let mut g = large_graph();
    let extra: Vec<Value> = vec![
        dv("big", &[], 20.0),
        json!({"id": "own1", "owned_files": [], "prerequisites": [],
               "estimated_effort_hours": 1.0, "metadata": {"owner": "ana"}}),
        json!({"id": "own2", "owned_files": [], "prerequisites": [],
               "estimated_effort_hours": 1.0, "metadata": {"owner": "ana"}}),
    ];
    for v in extra {
        g.deliverables.push(serde_json::from_value(v).unwrap());
    }
    g
}

#[tokio::test]
async fn review_truncation_keeps_every_candidate_kind() {
    let judge = FakeJudge::new();
    let req = ReviewRequest {
        max_questions: Some(10),
        ..ReviewRequest::default()
    };
    run_with(&every_kind_graph(), &req, &judge).await;
    let kinds: std::collections::BTreeSet<String> =
        judge.kinds_sent().into_iter().map(|(k, _)| k).collect();
    assert_eq!(
        kinds,
        [
            "crash_option",
            "false_dependency",
            "interface_split",
            "missing_dependency",
            "split_candidate"
        ]
        .into_iter()
        .map(String::from)
        .collect()
    );
}

#[tokio::test]
async fn review_rejects_max_questions_above_64() {
    let req = ReviewRequest {
        max_questions: Some(65),
        ..ReviewRequest::default()
    };
    let r = review(&two_chains(), &req, Judge::Model(&FakeJudge::new())).await;
    assert!(r.is_err());
}

#[tokio::test]
async fn review_rejects_zero_max_questions() {
    let req = ReviewRequest {
        max_questions: Some(0),
        ..ReviewRequest::default()
    };
    let r = review(&two_chains(), &req, Judge::Model(&FakeJudge::new())).await;
    assert!(r.is_err());
}

#[test]
fn review_request_rejects_unknown_fields() {
    let r: Result<ReviewRequest, _> = serde_json::from_value(json!({"max_question": 3}));
    assert!(r.is_err());
}

#[tokio::test]
async fn review_with_no_candidates_does_not_call_the_judge() {
    let mut m = dv("m", &[], 0.0);
    m["milestone"] = json!(true);
    let judge = FakeJudge::new();
    run(&graph(vec![m]), &judge).await;
    assert_eq!(judge.call_count(), 0);
}

// ---------------------------------------------------------------- candidates

#[tokio::test]
async fn review_asks_about_every_edge_as_a_false_dependency() {
    let judge = FakeJudge::new();
    run(&two_chains(), &judge).await;
    assert!(
        judge
            .kinds_sent()
            .contains(&("false_dependency".to_string(), vec!["a".into(), "c".into()]))
    );
}

#[tokio::test]
async fn review_asks_crash_options_for_critical_deliverables() {
    let judge = FakeJudge::new();
    run(&two_chains(), &judge).await;
    assert!(
        judge
            .kinds_sent()
            .contains(&("crash_option".to_string(), vec!["a".into()]))
    );
}

#[tokio::test]
async fn review_does_not_ask_crash_options_for_non_critical_deliverables() {
    let judge = FakeJudge::new();
    run(&two_chains(), &judge).await;
    assert!(
        !judge
            .kinds_sent()
            .contains(&("crash_option".to_string(), vec!["b".into()]))
    );
}

#[tokio::test]
async fn review_asks_split_for_a_deliverable_of_eight_hours_or_more() {
    let judge = FakeJudge::new();
    run(&long_head(), &judge).await;
    assert!(
        judge
            .kinds_sent()
            .contains(&("split_candidate".to_string(), vec!["a".into()]))
    );
}

#[tokio::test]
async fn review_asks_missing_dependency_for_an_unordered_pair_sharing_an_owner() {
    let mut x = dv("x", &[], 1.0);
    x["metadata"] = json!({"owner": "ana"});
    let mut y = dv("y", &[], 1.0);
    y["metadata"] = json!({"owner": "ana"});
    let judge = FakeJudge::new();
    run(&graph(vec![x, y]), &judge).await;
    assert!(judge.kinds_sent().contains(&(
        "missing_dependency".to_string(),
        vec!["x".into(), "y".into()]
    )));
}

#[tokio::test]
async fn review_asks_missing_dependency_when_a_description_mentions_another_id() {
    let mut x = dv("x", &[], 1.0);
    x["metadata"] = json!({"description": "wires up the y client"});
    let judge = FakeJudge::new();
    run(&graph(vec![x, dv("y", &[], 1.0)]), &judge).await;
    assert!(judge.kinds_sent().contains(&(
        "missing_dependency".to_string(),
        vec!["x".into(), "y".into()]
    )));
}

#[tokio::test]
async fn review_does_not_ask_missing_dependency_for_an_ordered_pair() {
    let mut x = dv("x", &[], 1.0);
    x["metadata"] = json!({"owner": "ana"});
    let mut y = dv("y", &["x"], 1.0);
    y["metadata"] = json!({"owner": "ana"});
    let judge = FakeJudge::new();
    run(&graph(vec![x, y]), &judge).await;
    assert!(
        !judge
            .kinds_sent()
            .iter()
            .any(|(k, _)| k == "missing_dependency")
    );
}

#[tokio::test]
async fn review_state_describes_the_questioned_deliverables() {
    let judge = FakeJudge::new();
    run(&two_chains(), &judge).await;
    let state = judge.calls.lock().unwrap()[0].0.clone();
    let ids: Vec<String> = state["deliverables"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids, vec!["a", "c"]);
}

// ---------------------------------------------------------------- proposals

#[tokio::test]
async fn likely_false_dependency_yields_a_remove_edge_proposal() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.9);
    let r = run(&two_chains(), &judge).await;
    assert_eq!(
        r.proposals.first().map(|p| p.edits.clone()),
        Some(vec![GraphEdit::RemoveEdge {
            from: "a".into(),
            to: "c".into()
        }])
    );
}

#[tokio::test]
async fn remove_edge_proposal_reports_simulated_hours_saved() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.9);
    let r = run(&two_chains(), &judge).await;
    assert_eq!(r.proposals.first().map(|p| p.hours_saved), Some(2.0));
}

#[tokio::test]
async fn remove_edge_proposal_costs_nothing() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.9);
    let r = run(&two_chains(), &judge).await;
    assert_eq!(r.proposals.first().map(|p| p.cost), Some(0.0));
}

#[tokio::test]
async fn unlikely_false_dependency_yields_no_proposal() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.79);
    let r = run(&two_chains(), &judge).await;
    assert!(r.proposals.is_empty());
}

#[tokio::test]
async fn likely_false_dependency_is_reported_as_a_finding() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.9);
    let r = run(&two_chains(), &judge).await;
    assert!(
        r.findings
            .iter()
            .any(|f| f.kind == FindingKind::FalseDependency && f.ids == ["a", "c"])
    );
}

#[tokio::test]
async fn reduce_scope_proposal_sets_effort_to_seventy_percent() {
    let judge = FakeJudge::new().choice(&["a"], "reduce_scope", 0.9);
    let r = run(&long_head(), &judge).await;
    assert_eq!(
        r.proposals.first().map(|p| p.edits.clone()),
        Some(vec![GraphEdit::SetEffort {
            id: "a".into(),
            hours: 7.0
        }])
    );
}

#[tokio::test]
async fn add_capacity_proposal_costs_thirty_percent_of_effort() {
    let judge = FakeJudge::new().choice(&["a"], "add_capacity", 0.9);
    let r = run(&long_head(), &judge).await;
    assert_eq!(r.proposals.first().map(|p| p.cost), Some(3.0));
}

#[tokio::test]
async fn low_confidence_crash_option_yields_no_proposal() {
    let judge = FakeJudge::new().choice(&["a"], "reduce_scope", 0.59);
    let r = run(&long_head(), &judge).await;
    assert!(r.proposals.is_empty());
}

#[tokio::test]
async fn fast_track_yields_no_proposal() {
    let judge = FakeJudge::new().choice(&["a"], "fast_track", 0.95);
    let r = run(&long_head(), &judge).await;
    assert!(r.proposals.is_empty());
}

#[tokio::test]
async fn proposals_rank_by_hours_saved_per_cost() {
    // remove a -> c: saves 2, cost 0 -> 2; reduce_scope a: saves 3, cost 0
    // -> 3. Reduce scope ranks first.
    let judge = FakeJudge::new()
        .noul("false_dependency", &["a", "c"], 0.9)
        .choice(&["a"], "reduce_scope", 0.9);
    let r = run(&long_head(), &judge).await;
    assert_eq!(
        proposal_ids(&r),
        vec!["crash_option:reduce_scope:a", "false_dependency:a->c"]
    );
}

#[tokio::test]
async fn proposal_score_is_hours_saved_over_cost_floored_at_one() {
    let judge = FakeJudge::new().choice(&["a"], "add_capacity", 0.9);
    let r = run(&long_head(), &judge).await;
    assert_eq!(r.proposals.first().map(|p| p.score), Some(1.0));
}

#[tokio::test]
async fn proposal_that_adds_a_lint_warning_is_dropped() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.95);
    let r = run(&milestone_chain(), &judge).await;
    assert!(r.proposals.is_empty());
}

#[tokio::test]
async fn proposal_saving_nothing_under_resource_leveling_is_dropped() {
    // a -> b on one single-capacity resource: removing the edge saves 2h of
    // CPM time but none once leveled.
    let mut a = dv("a", &[], 2.0);
    a["metadata"] = json!({"owner": "dev"});
    let mut b = dv("b", &["a"], 2.0);
    b["metadata"] = json!({"owner": "dev"});
    let req = ReviewRequest {
        capacities: Some(ScheduleRequest {
            capacities: BTreeMap::from([("dev".to_string(), 1)]),
            resource_key: "owner".to_string(),
            project_buffer_pct: 0.0,
        }),
        ..ReviewRequest::default()
    };
    let judge = FakeJudge::new().noul("false_dependency", &["a", "b"], 0.95);
    let r = run_with(&graph(vec![a, b]), &req, &judge).await;
    assert!(r.proposals.is_empty());
}

#[tokio::test]
async fn non_finite_judge_probability_yields_no_finding() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], f64::NAN);
    let r = run(&two_chains(), &judge).await;
    assert!(r.findings.is_empty());
}

// ---------------------------------------------------------------- judge failure

#[tokio::test]
async fn judge_failure_is_review_unavailable_with_its_class() {
    let key = LlmConfig::new("sk-test");
    let judge = FakeJudge::new().failing(JudgmentError::scrubbed(
        JudgmentErrorKind::RateLimited,
        "status 429",
        key.api_key(),
    ));
    let r = run(&two_chains(), &judge).await;
    assert_eq!(
        (r.status, r.reason.as_deref()),
        (
            ReviewStatus::ReviewUnavailable,
            Some("rate_limited: status 429")
        )
    );
}

#[tokio::test]
async fn judge_failure_still_records_the_call() {
    let key = LlmConfig::new("sk-test");
    let judge = FakeJudge::new().failing(JudgmentError::scrubbed(
        JudgmentErrorKind::Timeout,
        "no reply",
        key.api_key(),
    ));
    let r = run(&two_chains(), &judge).await;
    assert!(r.jev_called);
}

// ---------------------------------------------------------------- report metadata

#[tokio::test]
async fn ok_review_reports_status_ok() {
    let r = run(&two_chains(), &FakeJudge::new()).await;
    assert_eq!(r.status, ReviewStatus::Ok);
}

#[tokio::test]
async fn ok_review_records_the_reported_model() {
    let r = run(&two_chains(), &FakeJudge::new()).await;
    assert_eq!(r.model.as_deref(), Some("fake/jev"));
}

#[tokio::test]
async fn ok_review_records_the_endpoint_host() {
    let judge = FakeJudge {
        host: Some("openrouter.ai".to_string()),
        ..FakeJudge::new()
    };
    let r = run(&two_chains(), &judge).await;
    assert_eq!(r.endpoint.as_deref(), Some("openrouter.ai"));
}

#[tokio::test]
async fn ok_review_drops_a_non_finite_usage_cost() {
    let mut usage = Usage::new();
    usage.cost = Some(rig_core::completion::Cost::from_total(f64::NAN));
    let judge = FakeJudge {
        usage: Some(usage),
        ..FakeJudge::new()
    };
    let r = run(&two_chains(), &judge).await;
    assert_eq!(r.usage.and_then(|u| u.cost), None);
}

#[tokio::test]
async fn ok_review_keeps_a_finite_usage_cost() {
    let mut usage = Usage::new();
    usage.cost = Some(rig_core::completion::Cost::from_total(0.25));
    let judge = FakeJudge {
        usage: Some(usage),
        ..FakeJudge::new()
    };
    let r = run(&two_chains(), &judge).await;
    assert_eq!(r.usage.and_then(|u| u.cost).map(|c| c.total), Some(0.25));
}

#[tokio::test]
async fn prompt_hash_is_a_sha256_hex_digest() {
    let r = run(&two_chains(), &FakeJudge::new()).await;
    assert_eq!(r.prompt_hash.map(|h| h.len()), Some(64));
}

#[tokio::test]
async fn prompt_hash_is_stable_across_runs() {
    let a = run(&large_graph(), &FakeJudge::new()).await;
    let b = run(&large_graph(), &FakeJudge::new()).await;
    assert_eq!(a.prompt_hash, b.prompt_hash);
}

#[tokio::test]
async fn prompt_hash_changes_with_the_graph() {
    let a = run(&two_chains(), &FakeJudge::new()).await;
    let b = run(&long_head(), &FakeJudge::new()).await;
    assert_ne!(a.prompt_hash, b.prompt_hash);
}

#[tokio::test]
async fn review_output_is_deterministic_for_a_fixed_judge() {
    let make = || {
        FakeJudge::new()
            .noul("false_dependency", &["a", "c"], 0.9)
            .choice(&["a"], "add_capacity", 0.9)
    };
    let a = serde_json::to_string(&run(&long_head(), &make()).await).unwrap();
    let b = serde_json::to_string(&run(&long_head(), &make()).await).unwrap();
    assert_eq!(a, b);
}

#[tokio::test]
async fn review_report_serializes_status_in_snake_case() {
    let r = review(
        &two_chains(),
        &ReviewRequest::default(),
        Judge::NotConfigured,
    )
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(&r).unwrap()["status"],
        json!("review_unavailable")
    );
}

// ---------------------------------------------------------------- fix round 1

#[tokio::test]
async fn lint_errors_without_judge_return_invalid_graph() {
    let r = review(&cyclic(), &ReviewRequest::default(), Judge::NotConfigured)
        .await
        .unwrap();
    assert_eq!(r.status, ReviewStatus::InvalidGraph);
}

#[tokio::test]
async fn lint_errors_with_a_config_error_return_invalid_graph() {
    let r = review(
        &cyclic(),
        &ReviewRequest::default(),
        Judge::ConfigError("key file ignored: world-readable"),
    )
    .await
    .unwrap();
    assert_eq!(r.status, ReviewStatus::InvalidGraph);
}

#[tokio::test]
async fn unasked_choice_label_is_ignored() {
    let judge = FakeJudge::new().with(
        "crash_option",
        &["a"],
        Answer::Choice {
            choice: "teleport".to_string(),
            probabilities: BTreeMap::from([("teleport".to_string(), 1.0)]),
            confidence: 1.0,
        },
    );
    let r = run(&long_head(), &judge).await;
    assert!(r.findings.is_empty());
}

#[tokio::test]
async fn out_of_range_probability_is_ignored() {
    let judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 1.5);
    let r = run(&two_chains(), &judge).await;
    assert!(r.findings.is_empty());
}

#[tokio::test]
async fn wrong_answer_type_is_ignored() {
    let judge = FakeJudge::new().with(
        "false_dependency",
        &["a", "c"],
        Answer::Score {
            score: 4.0,
            probabilities: BTreeMap::from([("4".to_string(), 1.0)]),
            legend: BTreeMap::new(),
            confidence: 1.0,
        },
    );
    let r = run(&two_chains(), &judge).await;
    assert!(r.findings.is_empty());
}

#[tokio::test]
async fn missing_answer_yields_no_finding() {
    let mut judge = FakeJudge::new().noul("false_dependency", &["a", "c"], 0.9);
    judge
        .omit
        .push(("false_dependency".to_string(), vec!["a".into(), "c".into()]));
    let r = run(&two_chains(), &judge).await;
    assert!(r.findings.is_empty());
}

fn split_answer(score: f64, probabilities: &[(&str, f64)]) -> Answer {
    Answer::Score {
        score,
        probabilities: probabilities
            .iter()
            .map(|(k, p)| (k.to_string(), *p))
            .collect(),
        legend: BTreeMap::new(),
        confidence: 1.0,
    }
}

#[tokio::test]
async fn score_with_unknown_level_keys_is_ignored() {
    let judge = FakeJudge::new().with(
        "split_candidate",
        &["a"],
        split_answer(4.0, &[("4", 0.5), ("9", 0.5)]),
    );
    let r = run(&long_head(), &judge).await;
    assert!(r.findings.is_empty());
}

#[tokio::test]
async fn score_out_of_range_is_ignored() {
    let judge = FakeJudge::new().with("split_candidate", &["a"], split_answer(5.0, &[("4", 1.0)]));
    let r = run(&long_head(), &judge).await;
    assert!(r.findings.is_empty());
}

#[tokio::test]
async fn likely_split_is_reported_as_a_finding() {
    let judge = FakeJudge::new().with("split_candidate", &["a"], split_answer(4.0, &[("4", 1.0)]));
    let r = run(&long_head(), &judge).await;
    assert!(
        r.findings
            .iter()
            .any(|f| f.kind == FindingKind::SplitCandidate && f.ids == ["a"])
    );
}

#[tokio::test]
async fn long_model_name_is_truncated() {
    let judge = FakeJudge {
        model: Some("m".repeat(200)),
        ..FakeJudge::new()
    };
    let r = run(&two_chains(), &judge).await;
    assert_eq!(r.model.map(|m| m.chars().count()), Some(64));
}

#[tokio::test]
async fn crash_proposal_saves_time_under_resource_leveling() {
    // a(10, dev) -> c(2, dev); b(1, ops). Leveled 12h; reduce_scope a -> 9h.
    let with_owner = |mut v: Value, owner: &str| {
        v["metadata"] = json!({ "owner": owner });
        v
    };
    let g = graph(vec![
        with_owner(dv("a", &[], 10.0), "dev"),
        with_owner(dv("b", &[], 1.0), "ops"),
        with_owner(dv("c", &["a"], 2.0), "dev"),
    ]);
    let req = ReviewRequest {
        capacities: Some(ScheduleRequest {
            capacities: BTreeMap::from([("dev".to_string(), 1), ("ops".to_string(), 1)]),
            resource_key: "owner".to_string(),
            project_buffer_pct: 0.0,
        }),
        ..ReviewRequest::default()
    };
    let judge = FakeJudge::new().choice(&["a"], "reduce_scope", 0.9);
    let r = run_with(&g, &req, &judge).await;
    assert_eq!(r.proposals.first().map(|p| p.hours_saved), Some(3.0));
}

fn owning(id: &str, path: &str) -> Value {
    json!({"id": id, "owned_files": [path], "prerequisites": [],
           "estimated_effort_hours": 1.0})
}

#[tokio::test]
async fn nested_directories_signal_missing_dependency() {
    let judge = FakeJudge::new();
    run(
        &graph(vec![
            owning("x", "src/api/x.rs"),
            owning("y", "src/api/v2/y.rs"),
        ]),
        &judge,
    )
    .await;
    assert!(judge.kinds_sent().contains(&(
        "missing_dependency".to_string(),
        vec!["x".into(), "y".into()]
    )));
}

#[tokio::test]
async fn string_prefix_directories_do_not_signal_missing_dependency() {
    let judge = FakeJudge::new();
    run(
        &graph(vec![
            owning("x", "src/ap/x.rs"),
            owning("y", "src/api/y.rs"),
        ]),
        &judge,
    )
    .await;
    assert!(
        !judge
            .kinds_sent()
            .iter()
            .any(|(k, _)| k == "missing_dependency")
    );
}

#[tokio::test]
async fn crash_hours_round_to_the_nearest_hundredth() {
    // 0.7 x 3.3 = 2.31 (f32 arithmetic alone drifts off the hundredth).
    let g = graph(vec![dv("a", &[], 3.3)]);
    let judge = FakeJudge::new().choice(&["a"], "reduce_scope", 0.9);
    let r = run(&g, &judge).await;
    assert_eq!(
        r.proposals.first().map(|p| p.edits.clone()),
        Some(vec![GraphEdit::SetEffort {
            id: "a".into(),
            hours: 2.31
        }])
    );
}

#[tokio::test]
async fn add_capacity_proposal_shortens_duration_not_effort() {
    let judge = FakeJudge::new().choice(&["a"], "add_capacity", 0.9);
    let r = run(&long_head(), &judge).await;
    assert_eq!(
        r.proposals.first().map(|p| p.edits.clone()),
        Some(vec![GraphEdit::SetDuration {
            id: "a".into(),
            hours: Some(7.0)
        }])
    );
}

// ---------------------------------------------------------------- fix round 2

#[tokio::test]
async fn same_directory_signal_outranks_ancestor_signal() {
    // m and n share src/x (same directory); a's src is an ancestor of
    // both. By id alone (a, m) would sort first; by score (m, n) does.
    let judge = FakeJudge::new();
    run(
        &graph(vec![
            owning("a", "src/a.rs"),
            owning("m", "src/x/m.rs"),
            owning("n", "src/x/n.rs"),
        ]),
        &judge,
    )
    .await;
    let first = judge
        .kinds_sent()
        .into_iter()
        .find(|(k, _)| k == "missing_dependency");
    assert_eq!(
        first,
        Some((
            "missing_dependency".to_string(),
            vec!["m".into(), "n".into()]
        ))
    );
}

#[tokio::test]
async fn ancestor_with_a_large_subtree_carries_no_signal() {
    // src holds hub's file plus 33 deliverables below it: subtree 34 > 32.
    let mut v = vec![owning("hub", "src/hub.rs")];
    for i in 0..33 {
        v.push(owning(&format!("leaf{i:02}"), &format!("src/m{i:02}/f.rs")));
    }
    let judge = FakeJudge::new();
    run(&graph(v), &judge).await;
    assert!(
        !judge
            .kinds_sent()
            .iter()
            .any(|(k, ids)| k == "missing_dependency" && ids.contains(&"hub".to_string()))
    );
}
