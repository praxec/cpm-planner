//! P6 Task 1: OpenRouter/Jev configuration, the `JudgmentModel` trait and
//! the Jev client. Every HTTP test talks to a local wiremock server; the
//! default `cargo test` never reaches the network.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use cpm_planner::llm::jev::JevJudge;
use cpm_planner::llm::openrouter::chat_client;
use cpm_planner::llm::{
    ConfigError, DEFAULT_JEV_ENDPOINT, DEFAULT_JEV_MODEL, DEFAULT_LLM_MODEL, JudgmentErrorKind,
    JudgmentModel, LlmConfig,
};
use rig_typesafeai::types::{Answer, Question};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A key string that must never surface anywhere but the request header.
const SENTINEL: &str = "sk-or-SENTINEL-7f3a9c1e-do-not-leak";

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name| map.get(name).cloned()
}

fn config_with_key() -> LlmConfig {
    LlmConfig::from_lookup(lookup(&[("OPENROUTER_API_KEY", SENTINEL)]))
        .unwrap()
        .unwrap()
}

// ---------------------------------------------------------------- config

#[test]
fn config_without_any_key_is_none() {
    assert!(LlmConfig::from_lookup(lookup(&[])).unwrap().is_none());
}

#[test]
fn config_blank_env_key_is_treated_as_absent() {
    let cfg = LlmConfig::from_lookup(lookup(&[("OPENROUTER_API_KEY", "   ")])).unwrap();
    assert!(cfg.is_none());
}

#[test]
fn config_env_key_is_used() {
    assert_eq!(config_with_key().api_key().expose(), SENTINEL);
}

#[test]
fn config_jev_model_defaults_to_jev_1_13() {
    assert_eq!(config_with_key().jev_model(), DEFAULT_JEV_MODEL);
}

#[test]
fn config_jev_endpoint_defaults_to_openrouter_systemone() {
    assert_eq!(config_with_key().jev_endpoint(), DEFAULT_JEV_ENDPOINT);
}

#[test]
fn config_timeout_defaults_to_thirty_seconds() {
    assert_eq!(config_with_key().timeout(), Duration::from_secs(30));
}

#[test]
fn config_generative_model_defaults_to_gpt_5_mini() {
    assert_eq!(config_with_key().llm_model(), DEFAULT_LLM_MODEL);
}

#[test]
fn config_overrides_are_read_from_env() {
    let cfg = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_JEV_MODEL", "typesafe/jev-2"),
        ("CPM_JEV_ENDPOINT", "http://127.0.0.1:9/v1/systemone"),
        ("CPM_LLM_TIMEOUT_SECS", "300"),
        ("CPM_LLM_MODEL", "anthropic/claude-x"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        (
            cfg.jev_model(),
            cfg.jev_endpoint(),
            cfg.timeout(),
            cfg.llm_model()
        ),
        (
            "typesafe/jev-2",
            "http://127.0.0.1:9/v1/systemone",
            Duration::from_secs(300),
            "anthropic/claude-x"
        )
    );
}

#[test]
fn config_timeout_of_zero_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_LLM_TIMEOUT_SECS", "0"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidTimeout { .. }));
}

#[test]
fn config_timeout_above_three_hundred_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_LLM_TIMEOUT_SECS", "301"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidTimeout { .. }));
}

#[test]
fn config_non_numeric_timeout_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_LLM_TIMEOUT_SECS", "soon"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidTimeout { .. }));
}

#[test]
fn config_non_http_endpoint_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_JEV_ENDPOINT", "ftp://example.com/systemone"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidEndpoint { .. }));
}

#[test]
fn config_debug_redacts_key() {
    assert!(!format!("{:?}", config_with_key()).contains(SENTINEL));
}

#[test]
fn config_api_key_debug_redacts_key() {
    assert!(!format!("{:?}", config_with_key().api_key()).contains(SENTINEL));
}

#[test]
fn config_missing_key_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("absent.key");
    let err = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        missing.to_str().unwrap(),
    )]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::KeyFile { .. }));
}

#[cfg(unix)]
fn key_file(mode: u32, contents: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("openrouter.key");
    std::fs::write(&file, contents).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(mode)).unwrap();
    (dir, file)
}

#[cfg(unix)]
#[test]
fn config_owner_only_key_file_is_used_and_trimmed() {
    let (_dir, file) = key_file(0o600, &format!("  {SENTINEL}\n"));
    let cfg = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        file.to_str().unwrap(),
    )]))
    .unwrap()
    .unwrap();
    assert_eq!(cfg.api_key().expose(), SENTINEL);
}

#[cfg(unix)]
#[test]
fn config_world_readable_key_file_is_ignored() {
    let (_dir, file) = key_file(0o644, SENTINEL);
    let cfg = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        file.to_str().unwrap(),
    )]))
    .unwrap();
    assert!(cfg.is_none());
}

#[cfg(unix)]
#[test]
fn config_env_key_takes_precedence_over_key_file() {
    let (_dir, file) = key_file(0o600, "file-key");
    let cfg = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_OPENROUTER_KEY_FILE", file.to_str().unwrap()),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(cfg.api_key().expose(), SENTINEL);
}

#[test]
fn config_errors_never_contain_key() {
    let errors = [
        LlmConfig::from_lookup(lookup(&[
            ("OPENROUTER_API_KEY", SENTINEL),
            ("CPM_LLM_TIMEOUT_SECS", "0"),
        ]))
        .unwrap_err(),
        LlmConfig::from_lookup(lookup(&[
            ("OPENROUTER_API_KEY", SENTINEL),
            ("CPM_JEV_ENDPOINT", "nope"),
        ]))
        .unwrap_err(),
    ];
    let rendered: String = errors.iter().map(|e| format!("{e} {e:?}")).collect();
    assert!(!rendered.contains(SENTINEL));
}

// ---------------------------------------------------------------- jev

fn judge_for(server: &MockServer) -> JevJudge {
    let cfg = config_with_key().with_jev_endpoint(format!("{}/v1/systemone", server.uri()));
    JevJudge::new(&cfg)
}

fn noul_question() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        "dep_real".to_string(),
        Question::Noul {
            instructions: json!("Is the dependency a -> b real?"),
            criteria: None,
        },
    )])
}

fn state() -> serde_json::Value {
    json!({ "a": "design schema", "b": "write migration" })
}

fn ok_body() -> serde_json::Value {
    json!({
        "model": "typesafe/jev-1.13",
        "answers": { "dep_real": { "type": "noul", "noul": 0.82 } },
        "usage": { "input_tokens": 120, "output_tokens": 3, "total_tokens": 123 }
    })
}

async fn mount(server: &MockServer, response: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(response)
        .mount(server)
        .await;
}

async fn decide_with(
    response: ResponseTemplate,
) -> Result<cpm_planner::llm::Decisions, cpm_planner::llm::JudgmentError> {
    let server = MockServer::start().await;
    mount(&server, response).await;
    judge_for(&server).decide(state(), noul_question()).await
}

#[tokio::test]
async fn jev_success_decodes_answers() {
    let decisions = decide_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .await
        .unwrap();
    assert!(matches!(
        decisions.answers.get("dep_real"),
        Some(Answer::Noul { noul }) if (*noul - 0.82).abs() < 1e-9
    ));
}

#[tokio::test]
async fn jev_success_reports_model() {
    let decisions = decide_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .await
        .unwrap();
    assert_eq!(decisions.model, "typesafe/jev-1.13");
}

#[tokio::test]
async fn jev_success_reports_usage() {
    let decisions = decide_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .await
        .unwrap();
    assert_eq!(decisions.usage.and_then(|u| u.total_tokens), Some(123));
}

#[tokio::test]
async fn jev_request_sends_bearer_key_model_and_questions() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header(
            "authorization",
            format!("Bearer {SENTINEL}").as_str(),
        ))
        .and(body_partial_json(json!({
            "model": DEFAULT_JEV_MODEL,
            "state": state(),
            "questions": { "dep_real": { "type": "noul" } }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(1)
        .mount(&server)
        .await;
    let result = judge_for(&server).decide(state(), noul_question()).await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn jev_401_is_unauthorized() {
    let err = decide_with(ResponseTemplate::new(401).set_body_string("{\"error\":\"bad key\"}"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Unauthorized);
}

#[tokio::test]
async fn jev_403_is_unauthorized() {
    let err = decide_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Unauthorized);
}

#[tokio::test]
async fn jev_429_is_rate_limited() {
    let err = decide_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::RateLimited);
}

#[tokio::test]
async fn jev_500_is_upstream() {
    let err = decide_with(ResponseTemplate::new(500).set_body_string("boom"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Upstream);
}

#[tokio::test]
async fn jev_slow_response_is_timeout() {
    let server = MockServer::start().await;
    mount(
        &server,
        ResponseTemplate::new(200)
            .set_body_json(ok_body())
            .set_delay(Duration::from_secs(5)),
    )
    .await;
    let cfg = config_with_key()
        .with_jev_endpoint(format!("{}/v1/systemone", server.uri()))
        .with_timeout(Duration::from_millis(100));
    let err = JevJudge::new(&cfg)
        .decide(state(), noul_question())
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Timeout);
}

#[tokio::test]
async fn jev_malformed_body_is_decode() {
    let err = decide_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Decode);
}

#[tokio::test]
async fn jev_answer_for_unasked_question_is_decode() {
    let body = json!({
        "model": "typesafe/jev-1.13",
        "answers": { "other": { "type": "noul", "noul": 0.5 } }
    });
    let err = decide_with(ResponseTemplate::new(200).set_body_json(body))
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Decode);
}

#[tokio::test]
async fn jev_refused_connection_is_transport() {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let cfg = config_with_key().with_jev_endpoint(format!("http://127.0.0.1:{port}/v1/systemone"));
    let err = JevJudge::new(&cfg)
        .decide(state(), noul_question())
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::Transport);
}

#[tokio::test]
async fn jev_invalid_question_is_rejected_before_sending() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .expect(0)
        .mount(&server)
        .await;
    let err = judge_for(&server)
        .decide(state(), BTreeMap::new())
        .await
        .unwrap_err();
    assert_eq!(err.kind, JudgmentErrorKind::InvalidRequest);
}

#[tokio::test]
async fn jev_errors_never_contain_key_even_when_upstream_echoes_it() {
    let echo = format!("{{\"error\":\"key {SENTINEL} rejected\"}}");
    let mut rendered = String::new();
    for status in [200, 401, 403, 429, 500, 502] {
        let body = if status == 200 {
            format!("garbage {SENTINEL}")
        } else {
            echo.clone()
        };
        let err = decide_with(ResponseTemplate::new(status).set_body_string(body))
            .await
            .unwrap_err();
        rendered.push_str(&format!("{err} {err:?} {}\n", err.message));
    }
    assert!(!rendered.contains(SENTINEL), "{rendered}");
}

#[test]
fn jev_judge_debug_redacts_key() {
    let judge = JevJudge::new(&config_with_key());
    assert!(!format!("{judge:?}").contains(SENTINEL));
}

#[test]
fn judgment_error_display_names_its_class() {
    let err = cpm_planner::llm::JudgmentError::new(JudgmentErrorKind::RateLimited, "status 429");
    assert_eq!(err.to_string(), "rate_limited: status 429");
}

// ---------------------------------------------------------------- openrouter

#[test]
fn openrouter_chat_client_debug_redacts_key() {
    let client = chat_client(&config_with_key());
    assert!(!format!("{client:?}").contains(SENTINEL));
}
