//! P6 Task 3: the `plan.review` MCP tool. Every test injects a fake judge
//! or a wiremock-backed `JevJudge`; nothing here reaches the network except
//! the `#[ignore]`d live smoke test.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use async_trait::async_trait;
use cpm_planner::audit::MemoryAuditSink;
use cpm_planner::llm::jev::JevJudge;
use cpm_planner::llm::{
    Answer, ConfigError, Decisions, JudgmentError, JudgmentModel, LlmConfig, Question,
};
use cpm_planner::review::NO_KEY_REASON;
use cpm_planner::{BasicCpmPlanner, PlanServer, TOOL_STATUS, TOOL_SUBMIT};
use rmcp::model::{CallToolRequestParams, ErrorCode, JsonObject};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOOL_REVIEW: &str = "plan.review";
/// A key string that must never surface in a response or an audit record.
const SENTINEL: &str = "sk-or-SENTINEL-3b81d0aa-do-not-leak";

// ---------------------------------------------------------------- helpers

fn call_args(name: &str, args: Value) -> CallToolRequestParams {
    let map: JsonObject = match args {
        Value::Object(m) => m,
        _ => panic!("call_args expects a JSON object"),
    };
    CallToolRequestParams::new(name.to_string()).with_arguments(map)
}

fn graph() -> Value {
    json!({
        "deliverables": [
            { "id": "d1", "owned_files": ["src/a.rs"], "prerequisites": [],
              "estimated_effort_hours": 4.0 },
            { "id": "d2", "owned_files": ["src/b.rs"], "prerequisites": ["d1"],
              "estimated_effort_hours": 2.0 }
        ]
    })
}

fn cyclic_graph() -> Value {
    json!({
        "deliverables": [
            { "id": "a", "owned_files": ["src/a.rs"], "prerequisites": ["b"],
              "estimated_effort_hours": 1.0 },
            { "id": "b", "owned_files": ["src/b.rs"], "prerequisites": ["a"],
              "estimated_effort_hours": 1.0 }
        ]
    })
}

/// A chain of `n` deliverables.
fn chain_graph(n: usize) -> Value {
    let deliverables: Vec<Value> = (0..n)
        .map(|i| {
            let prerequisites: Vec<String> = if i == 0 {
                Vec::new()
            } else {
                vec![format!("c{:04}", i - 1)]
            };
            json!({
                "id": format!("c{i:04}"),
                "owned_files": [format!("src/c{i}.rs")],
                "prerequisites": prerequisites,
                "estimated_effort_hours": 1.0 + (i % 7) as f64
            })
        })
        .collect();
    json!({ "deliverables": deliverables })
}

fn answer_for(question: &Question, noul: f64) -> Answer {
    match question {
        Question::Noul { .. } => Answer::Noul { noul },
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

/// Answers every yes/no question with `noul`, every crash question with
/// `none`, every split question with the lowest level.
struct FakeJudge {
    noul: f64,
}

#[async_trait]
impl JudgmentModel for FakeJudge {
    async fn decide(
        &self,
        _state: Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        Ok(Decisions {
            answers: questions
                .iter()
                .map(|(id, q)| (id.clone(), answer_for(q, self.noul)))
                .collect(),
            model: "fake/jev".to_string(),
            usage: None,
        })
    }

    fn endpoint_host(&self) -> Option<String> {
        Some("judge.invalid".to_string())
    }
}

/// Signals `entered` when called, then blocks its thread until released
/// (or 10 s pass), standing in for review work that occupies a thread.
struct GatedJudge {
    entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<mpsc::Receiver<()>>,
}

#[async_trait]
impl JudgmentModel for GatedJudge {
    async fn decide(
        &self,
        _state: Value,
        _questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        if let Some(tx) = self.entered.lock().unwrap().take() {
            let _ = tx.send(());
        }
        let _ = self
            .release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10));
        Ok(Decisions {
            answers: BTreeMap::new(),
            model: "fake/gated".to_string(),
            usage: None,
        })
    }
}

fn server_with(judge: Option<Arc<dyn JudgmentModel>>) -> PlanServer {
    PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_judge(judge)
}

fn audited_server(judge: Arc<dyn JudgmentModel>) -> (PlanServer, MemoryAuditSink) {
    let sink = MemoryAuditSink::new();
    let planner = BasicCpmPlanner::with_audit(Arc::new(sink.clone()));
    (
        PlanServer::new(Arc::new(planner)).with_judge(Some(judge)),
        sink,
    )
}

async fn review(server: &PlanServer, args: Value) -> Value {
    server
        .dispatch_call(call_args(TOOL_REVIEW, args))
        .await
        .expect("plan.review returns Ok")
}

async fn review_error_code(args: Value) -> ErrorCode {
    let judge: Arc<dyn JudgmentModel> = Arc::new(FakeJudge { noul: 0.0 });
    server_with(Some(judge))
        .dispatch_call(call_args(TOOL_REVIEW, args))
        .await
        .unwrap_err()
        .code
}

/// A JevJudge with [`SENTINEL`] as its key, pointed at `mock`.
fn jev_judge(mock: &MockServer) -> Arc<dyn JudgmentModel> {
    let cfg = LlmConfig::new(SENTINEL).with_jev_endpoint(format!("{}/v1/systemone", mock.uri()));
    Arc::new(JevJudge::new(&cfg))
}

async fn mock_replying(status: u16) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(status).set_body_string(format!("bad key {SENTINEL}")))
        .mount(&mock)
        .await;
    mock
}

fn review_events(sink: &MemoryAuditSink) -> Vec<Value> {
    sink.snapshot()
        .into_iter()
        .filter(|e| e.event_type == "plan.review")
        .map(|e| e.payload)
        .collect()
}

// ---------------------------------------------------------------- availability

#[tokio::test]
async fn review_without_key_is_unavailable_with_no_key_reason() {
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_llm_config(Ok(None));
    let resp = review(&server, json!({ "graph": graph() })).await;
    assert_eq!(
        (resp["status"].clone(), resp["reason"].clone()),
        (json!("review_unavailable"), json!(NO_KEY_REASON))
    );
}

#[tokio::test]
async fn review_with_invalid_config_names_the_setting() {
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_llm_config(Err(
        ConfigError::InvalidTimeout {
            value: "0".to_string(),
        },
    ));
    let resp = review(&server, json!({ "graph": graph() })).await;
    assert!(
        resp["reason"]
            .as_str()
            .unwrap()
            .starts_with("CPM_LLM_TIMEOUT_SECS invalid: ")
    );
}

#[tokio::test]
async fn review_with_invalid_config_still_reports_lint_errors() {
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_llm_config(Err(
        ConfigError::InvalidTimeout {
            value: "0".to_string(),
        },
    ));
    let resp = review(&server, json!({ "graph": cyclic_graph() })).await;
    assert_eq!(resp["status"], json!("invalid_graph"));
}

#[tokio::test]
async fn review_upstream_unauthorized_is_unavailable_with_its_class() {
    let mock = mock_replying(401).await;
    let resp = review(
        &server_with(Some(jev_judge(&mock))),
        json!({ "graph": graph() }),
    )
    .await;
    assert!(
        resp["reason"]
            .as_str()
            .unwrap()
            .starts_with("unauthorized: ")
    );
}

#[tokio::test]
async fn review_response_and_audit_never_contain_the_key() {
    let mock = mock_replying(401).await;
    let (server, sink) = audited_server(jev_judge(&mock));
    let resp = review(&server, json!({ "graph": graph() })).await;
    let audit = serde_json::to_string(&sink.snapshot()).unwrap();
    assert!(!format!("{resp}{audit}").contains(SENTINEL));
}

// ---------------------------------------------------------------- params

#[tokio::test]
async fn review_rejects_max_questions_above_64_as_invalid_params() {
    let code = review_error_code(json!({ "graph": graph(), "max_questions": 65 })).await;
    assert_eq!(code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn review_rejects_zero_max_questions_as_invalid_params() {
    let code = review_error_code(json!({ "graph": graph(), "max_questions": 0 })).await;
    assert_eq!(code, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn review_rejects_buffer_pct_above_100_as_invalid_params() {
    let args = json!({
        "graph": graph(),
        "capacities": { "unassigned": 1 },
        "project_buffer_pct": 150.0
    });
    assert_eq!(review_error_code(args).await, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn review_rejects_resource_key_without_capacities_as_invalid_params() {
    let args = json!({ "graph": graph(), "resource_key": "team" });
    assert_eq!(review_error_code(args).await, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn review_rejects_unknown_fields_as_invalid_params() {
    let args = json!({ "graph": graph(), "questions": 3 });
    assert_eq!(review_error_code(args).await, ErrorCode::INVALID_PARAMS);
}

#[tokio::test]
async fn review_rejects_graph_and_plan_id_together_as_invalid_params() {
    let args = json!({ "graph": graph(), "plan_id": "p" });
    assert_eq!(review_error_code(args).await, ErrorCode::INVALID_PARAMS);
}

// ---------------------------------------------------------------- results

#[tokio::test]
async fn review_with_fake_judge_returns_verified_edge_removal() {
    let judge: Arc<dyn JudgmentModel> = Arc::new(FakeJudge { noul: 0.95 });
    let resp = review(&server_with(Some(judge)), json!({ "graph": graph() })).await;
    let ids: Vec<&str> = resp["proposals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["false_dependency:d1->d2"]);
}

#[tokio::test]
async fn review_of_stored_plan_reports_ok() {
    let judge: Arc<dyn JudgmentModel> = Arc::new(FakeJudge { noul: 0.0 });
    let server = server_with(Some(judge));
    let submitted = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": graph() })))
        .await
        .unwrap();
    let resp = review(&server, json!({ "plan_id": submitted["plan_id"] })).await;
    assert_eq!(resp["status"], json!("ok"));
}

#[tokio::test]
async fn review_honours_max_questions() {
    let judge: Arc<dyn JudgmentModel> = Arc::new(FakeJudge { noul: 0.0 });
    let args = json!({ "graph": chain_graph(40), "max_questions": 5 });
    let resp = review(&server_with(Some(judge)), args).await;
    assert_eq!(resp["question_count"], json!(5));
}

#[tokio::test]
async fn review_records_audit_event_with_prompt_hash() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let resp = review(&server, json!({ "graph": graph() })).await;
    let events = review_events(&sink);
    assert_eq!(
        events
            .iter()
            .map(|e| e["prompt_hash"].clone())
            .collect::<Vec<_>>(),
        vec![resp["prompt_hash"].clone()]
    );
}

#[tokio::test]
async fn review_audit_event_records_model_endpoint_status_and_count() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let resp = review(&server, json!({ "graph": graph() })).await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(
        (
            event["model"].clone(),
            event["endpoint"].clone(),
            event["status"].clone(),
            event["question_count"].clone()
        ),
        (
            json!("fake/jev"),
            json!("judge.invalid"),
            json!("ok"),
            resp["question_count"].clone()
        )
    );
}

#[tokio::test]
async fn review_audit_event_omits_the_prompt() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    review(&server, json!({ "graph": graph() })).await;
    let event = review_events(&sink).pop().unwrap();
    let keys: std::collections::BTreeSet<&str> = event
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "endpoint",
            "jev_called",
            "model",
            "prompt_hash",
            "question_count",
            "status"
        ]
        .into_iter()
        .collect()
    );
}

// ---------------------------------------------------------------- concurrency

#[tokio::test]
async fn review_does_not_block_other_tools() {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let judge: Arc<dyn JudgmentModel> = Arc::new(GatedJudge {
        entered: Mutex::new(Some(entered_tx)),
        release: Mutex::new(release_rx),
    });
    let server = server_with(Some(judge));
    let submitted = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": graph() })))
        .await
        .unwrap();
    let reviewing = tokio::spawn({
        let server = server.clone();
        async move { review(&server, json!({ "graph": chain_graph(2000) })).await }
    });
    entered_rx.await.unwrap();
    server
        .dispatch_call(call_args(
            TOOL_STATUS,
            json!({ "plan_id": submitted["plan_id"] }),
        ))
        .await
        .unwrap();
    let review_still_running = !reviewing.is_finished();
    release_tx.send(()).unwrap();
    reviewing.await.unwrap();
    assert!(review_still_running);
}

// ---------------------------------------------------------------- live

/// Live smoke test against OpenRouter: `OPENROUTER_API_KEY=… cargo test
/// --test server_review -- --ignored`. Skips when no key is configured.
#[tokio::test]
#[ignore = "calls OpenRouter; needs OPENROUTER_API_KEY"]
async fn review_against_openrouter_live() {
    let config = LlmConfig::from_env().expect("LLM config is valid");
    if config.is_none() {
        eprintln!("OPENROUTER_API_KEY not set; skipping");
        return;
    }
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_llm_config(Ok(config));
    let resp = review(&server, json!({ "graph": graph() })).await;
    assert_eq!(resp["status"], json!("ok"), "{resp}");
}

// ---------------------------------------------------------------- surface

#[test]
fn instructions_describe_plan_review() {
    use rmcp::ServerHandler;
    let info = PlanServer::new(Arc::new(BasicCpmPlanner::new())).get_info();
    assert!(info.instructions.unwrap().contains("plan.review"));
}

#[test]
fn review_tool_schema_caps_max_questions_at_64() {
    let tool = cpm_planner::plan_tool_definitions()
        .into_iter()
        .find(|t| t.name == TOOL_REVIEW)
        .unwrap();
    assert_eq!(
        tool.input_schema["properties"]["max_questions"]["maximum"],
        json!(64)
    );
}
