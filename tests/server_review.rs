//! P6 Task 3: the `plan.review` MCP tool. Every test injects a fake judge
//! or a wiremock-backed `JevJudge`; nothing here reaches the network except
//! the `#[ignore]`d live smoke test.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
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
    mock_with(ResponseTemplate::new(status).set_body_string(format!("bad key {SENTINEL}"))).await
}

async fn mock_with(response: ResponseTemplate) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(response)
        .mount(&mock)
        .await;
    mock
}

async fn reason_for(response: ResponseTemplate, timeout: Duration) -> String {
    let mock = mock_with(response).await;
    let cfg = LlmConfig::new(SENTINEL)
        .with_jev_endpoint(format!("{}/v1/systemone", mock.uri()))
        .with_timeout(timeout);
    let judge: Arc<dyn JudgmentModel> = Arc::new(JevJudge::new(&cfg));
    let resp = review(&server_with(Some(judge)), json!({ "graph": graph() })).await;
    resp["reason"].as_str().unwrap_or_default().to_string()
}

/// Captures every tracing line written while it is the default subscriber.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for LogCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl LogCapture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Counts callers and holds each until released (or 10 s pass).
#[derive(Default)]
struct SlotJudge {
    entered: AtomicUsize,
    released: Mutex<bool>,
    wake: Condvar,
}

impl SlotJudge {
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

#[async_trait]
impl JudgmentModel for SlotJudge {
    async fn decide(
        &self,
        _state: Value,
        _questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let guard = self.released.lock().unwrap();
        let _ = self
            .wake
            .wait_timeout_while(guard, Duration::from_secs(10), |released| !*released)
            .unwrap();
        Ok(Decisions {
            answers: BTreeMap::new(),
            model: "fake/slot".to_string(),
            usage: None,
        })
    }
}

struct PanickingJudge;

#[async_trait]
impl JudgmentModel for PanickingJudge {
    async fn decide(
        &self,
        _state: Value,
        _questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        panic!("judge exploded with secret detail");
    }
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
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new()))
        .with_llm_config(Err(ConfigError::InvalidTimeout));
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
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new()))
        .with_llm_config(Err(ConfigError::InvalidTimeout));
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
            "code",
            "endpoint",
            "failure_class",
            "jev_called",
            "model",
            "plan_id",
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

// ---------------------------------------------------------------- fix round 1

#[tokio::test]
async fn misplaced_key_in_config_vars_is_never_logged_or_returned() {
    let logs = LogCapture::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let configs = [
        LlmConfig::from_lookup(|name| {
            (name == "CPM_OPENROUTER_KEY_FILE").then(|| SENTINEL.to_string())
        }),
        LlmConfig::from_lookup(|name| match name {
            "OPENROUTER_API_KEY" => Some("real-key".to_string()),
            "CPM_LLM_TIMEOUT_SECS" => Some(SENTINEL.to_string()),
            _ => None,
        }),
    ];
    let mut output = String::new();
    for config in configs {
        let server = PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_llm_config(config);
        output.push_str(
            &review(&server, json!({ "graph": graph() }))
                .await
                .to_string(),
        );
    }
    let logs = logs.text();
    assert_eq!(
        (
            logs.contains("LLM configuration unusable"),
            format!("{logs}{output}").contains(SENTINEL)
        ),
        (true, false)
    );
}

#[tokio::test]
async fn review_engine_error_keeps_its_planner_error_prefix() {
    let judge: Arc<dyn JudgmentModel> = Arc::new(FakeJudge { noul: 0.0 });
    let err = server_with(Some(judge))
        .dispatch_call(call_args(
            TOOL_REVIEW,
            json!({ "graph": graph(), "capacities": { "nobody": 1 } }),
        ))
        .await
        .unwrap_err();
    assert!(
        err.message.starts_with("INVALID_CAPACITIES:"),
        "{}",
        err.message
    );
}

#[tokio::test]
async fn review_engine_error_is_audited_with_its_code() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let _ = server
        .dispatch_call(call_args(
            TOOL_REVIEW,
            json!({ "graph": graph(), "capacities": { "nobody": 1 } }),
        ))
        .await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(
        (event["status"].clone(), event["code"].clone()),
        (json!("error"), json!("INVALID_CAPACITIES"))
    );
}

#[tokio::test]
async fn review_audit_model_is_the_configured_model_when_the_judge_fails() {
    let mock = mock_replying(500).await;
    let (server, sink) = audited_server(jev_judge(&mock));
    review(&server, json!({ "graph": graph() })).await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(event["model"], json!(cpm_planner::llm::DEFAULT_JEV_MODEL));
}

#[tokio::test]
async fn review_audit_carries_plan_id_for_a_stored_plan() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let submitted = server
        .dispatch_call(call_args(TOOL_SUBMIT, json!({ "graph": graph() })))
        .await
        .unwrap();
    review(&server, json!({ "plan_id": submitted["plan_id"] })).await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(event["plan_id"], submitted["plan_id"]);
}

#[tokio::test]
async fn review_audit_plan_id_is_null_for_an_inline_graph() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    review(&server, json!({ "graph": graph() })).await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(event["plan_id"], Value::Null);
}

#[tokio::test]
async fn review_reports_truncated_when_max_questions_cuts_candidates() {
    let judge: Arc<dyn JudgmentModel> = Arc::new(FakeJudge { noul: 0.0 });
    let args = json!({ "graph": chain_graph(40), "max_questions": 5 });
    let resp = review(&server_with(Some(judge)), args).await;
    assert_eq!(resp["truncated"], json!(true));
}

#[tokio::test]
async fn review_upstream_rate_limit_is_unavailable_with_its_class() {
    let reason = reason_for(ResponseTemplate::new(429), Duration::from_secs(5)).await;
    assert!(reason.starts_with("rate_limited: "), "{reason}");
}

#[tokio::test]
async fn review_upstream_server_error_is_unavailable_with_its_class() {
    let reason = reason_for(ResponseTemplate::new(500), Duration::from_secs(5)).await;
    assert!(reason.starts_with("upstream: "), "{reason}");
}

#[tokio::test]
async fn review_upstream_timeout_is_unavailable_with_its_class() {
    let slow = ResponseTemplate::new(200).set_delay(Duration::from_secs(5));
    let reason = reason_for(slow, Duration::from_millis(100)).await;
    assert!(reason.starts_with("timeout: "), "{reason}");
}

#[tokio::test]
async fn review_panic_is_a_generic_internal_error() {
    let judge: Arc<dyn JudgmentModel> = Arc::new(PanickingJudge);
    let err = server_with(Some(judge))
        .dispatch_call(call_args(TOOL_REVIEW, json!({ "graph": graph() })))
        .await
        .unwrap_err();
    assert_eq!(err.message, "review task failed");
}

#[tokio::test]
async fn third_concurrent_review_waits_for_a_slot() {
    let judge = Arc::new(SlotJudge::default());
    let shared: Arc<dyn JudgmentModel> = judge.clone();
    let server = server_with(Some(shared));
    let reviews: Vec<_> = (0..3)
        .map(|_| {
            let server = server.clone();
            tokio::spawn(async move { review(&server, json!({ "graph": graph() })).await })
        })
        .collect();
    while judge.entered.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let entered_while_two_held = judge.entered.load(Ordering::SeqCst);
    judge.release();
    for r in reviews {
        r.await.unwrap();
    }
    assert_eq!(entered_while_two_held, 2);
}

// ---------------------------------------------------------------- final review

fn capture_logs() -> (LogCapture, tracing::subscriber::DefaultGuard) {
    let logs = LogCapture::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(move || writer.clone())
        .finish();
    (logs, tracing::subscriber::set_default(subscriber))
}

#[tokio::test]
async fn key_pasted_into_model_var_is_rejected_and_never_logged() {
    let (logs, _guard) = capture_logs();
    let config = LlmConfig::from_lookup(|name| match name {
        "OPENROUTER_API_KEY" | "CPM_JEV_MODEL" => Some(SENTINEL.to_string()),
        _ => None,
    });
    let rejected = matches!(config, Err(ConfigError::InvalidModel { .. }));
    let server = PlanServer::new(Arc::new(BasicCpmPlanner::new())).with_llm_config(config);
    let output = review(&server, json!({ "graph": graph() }))
        .await
        .to_string();
    let logs = logs.text();
    assert_eq!(
        (
            rejected,
            logs.contains("LLM configuration unusable"),
            format!("{logs}{output}").contains(SENTINEL)
        ),
        (true, true, false)
    );
}

#[tokio::test]
async fn review_audit_records_the_failure_class_of_a_judge_failure() {
    let mock = mock_replying(401).await;
    let (server, sink) = audited_server(jev_judge(&mock));
    review(&server, json!({ "graph": graph() })).await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(event["failure_class"], json!("unauthorized"));
}

#[tokio::test]
async fn review_audit_failure_class_is_null_without_a_judge_failure() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    review(&server, json!({ "graph": graph() })).await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(event["failure_class"], Value::Null);
}

#[tokio::test]
async fn review_of_unknown_plan_is_audited_with_its_code() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let _ = server
        .dispatch_call(call_args(TOOL_REVIEW, json!({ "plan_id": "nope" })))
        .await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(
        (event["status"].clone(), event["code"].clone()),
        (json!("error"), json!("PLAN_NOT_FOUND"))
    );
}

#[tokio::test]
async fn review_of_path_without_root_is_audited_with_its_code() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let _ = server
        .dispatch_call(call_args(
            TOOL_REVIEW,
            json!({ "path": ".cpm-planner/plans/a/main.json" }),
        ))
        .await;
    let event = review_events(&sink).pop().unwrap();
    assert_eq!(
        (event["status"].clone(), event["code"].clone()),
        (json!("error"), json!("INVALID_PATH"))
    );
}

#[tokio::test]
async fn rejected_params_are_not_audited() {
    let (server, sink) = audited_server(Arc::new(FakeJudge { noul: 0.0 }));
    let _ = server
        .dispatch_call(call_args(
            TOOL_REVIEW,
            json!({ "graph": graph(), "plan_id": "p" }),
        ))
        .await;
    assert_eq!(review_events(&sink).len(), 0);
}

#[tokio::test]
async fn review_without_judge_does_not_wait_for_a_slot() {
    let judge = Arc::new(SlotJudge::default());
    let shared: Arc<dyn JudgmentModel> = judge.clone();
    let server = server_with(Some(shared));
    let holders: Vec<_> = (0..2)
        .map(|_| {
            let server = server.clone();
            tokio::spawn(async move { review(&server, json!({ "graph": graph() })).await })
        })
        .collect();
    while judge.entered.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Lint errors: the judge will not be called, so no slot is needed.
    let lint_only = tokio::time::timeout(
        Duration::from_secs(5),
        review(&server, json!({ "graph": cyclic_graph() })),
    )
    .await;
    judge.release();
    for h in holders {
        h.await.unwrap();
    }
    assert!(lint_only.is_ok());
}
