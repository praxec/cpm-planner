//! P6 Task 1: OpenRouter/Jev configuration, the `JudgmentModel` trait and
//! the Jev client. Every HTTP test talks to a local wiremock server; the
//! default `cargo test` never reaches the network.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use cpm_planner::llm::jev::JevJudge;
use cpm_planner::llm::openrouter::chat_client;
use cpm_planner::llm::{
    ApiKey, ConfigError, DEFAULT_JEV_ENDPOINT, DEFAULT_JEV_MODEL, DEFAULT_LLM_MODEL,
    JudgmentErrorKind, JudgmentModel, KeyFileIgnoredReason, LlmConfig, MAX_ERROR_MESSAGE_CHARS,
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
    assert!(matches!(err, ConfigError::InvalidTimeout));
}

#[test]
fn config_timeout_above_three_hundred_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_LLM_TIMEOUT_SECS", "301"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidTimeout));
}

#[test]
fn config_non_numeric_timeout_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_LLM_TIMEOUT_SECS", "soon"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidTimeout));
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
    let err = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        file.to_str().unwrap(),
    )]))
    .unwrap_err();
    assert!(matches!(
        err,
        ConfigError::KeyFileIgnored {
            reason: KeyFileIgnoredReason::WorldReadable,
            ..
        }
    ));
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
    assert_eq!(err.kind(), JudgmentErrorKind::Unauthorized);
}

#[tokio::test]
async fn jev_403_is_unauthorized() {
    let err = decide_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), JudgmentErrorKind::Unauthorized);
}

#[tokio::test]
async fn jev_429_is_rate_limited() {
    let err = decide_with(ResponseTemplate::new(429).set_body_string("slow down"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), JudgmentErrorKind::RateLimited);
}

#[tokio::test]
async fn jev_500_is_upstream() {
    let err = decide_with(ResponseTemplate::new(500).set_body_string("boom"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), JudgmentErrorKind::Upstream);
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
    eprintln!(
        "TEMP-DIAG pooled server {}: {:?} {}",
        server.uri(),
        err.kind(),
        err
    );
    assert_eq!(err.kind(), JudgmentErrorKind::Timeout);
}

// TEMP-DIAG: same scenario on a fresh (non-pooled) wiremock server.
#[tokio::test]
async fn temp_diag_slow_fresh_server() {
    let server = MockServer::builder().start().await;
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
    let started = std::time::Instant::now();
    let err = JevJudge::new(&cfg)
        .decide(state(), noul_question())
        .await
        .unwrap_err();
    eprintln!(
        "TEMP-DIAG fresh server {}: {:?} after {:?}: {}",
        server.uri(),
        err.kind(),
        started.elapsed(),
        err
    );
    panic!("TEMP-DIAG: show captured output");
}

// TEMP-DIAG: dump proxy settings visible to reqwest's system-proxy lookup.
#[test]
fn temp_diag_proxy_settings() {
    for k in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
    ] {
        eprintln!("TEMP-DIAG env {k}={:?}", std::env::var(k).ok());
    }
    let cmd: Option<(&str, Vec<&str>)> = if cfg!(windows) {
        Some((
            "reg",
            vec![
                "query",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            ],
        ))
    } else if cfg!(target_os = "macos") {
        Some(("scutil", vec!["--proxy"]))
    } else {
        None
    };
    if let Some((c, args)) = cmd {
        match std::process::Command::new(c).args(&args).output() {
            Ok(o) => eprintln!("TEMP-DIAG {c}: {}", String::from_utf8_lossy(&o.stdout)),
            Err(e) => eprintln!("TEMP-DIAG {c} failed: {e}"),
        }
    }
    panic!("TEMP-DIAG: show captured output");
}

#[tokio::test]
async fn jev_malformed_body_is_decode() {
    let err = decide_with(ResponseTemplate::new(200).set_body_string("not json at all"))
        .await
        .unwrap_err();
    assert_eq!(err.kind(), JudgmentErrorKind::Decode);
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
    assert_eq!(err.kind(), JudgmentErrorKind::Decode);
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
    assert_eq!(err.kind(), JudgmentErrorKind::Transport);
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
    assert_eq!(err.kind(), JudgmentErrorKind::InvalidRequest);
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
        rendered.push_str(&format!("{err} {err:?} {}\n", err.message()));
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
    let err = cpm_planner::llm::JudgmentError::scrubbed(
        JudgmentErrorKind::RateLimited,
        "status 429",
        config_with_key().api_key(),
    );
    assert_eq!(err.to_string(), "rate_limited: status 429");
}

#[test]
fn judgment_error_scrubbed_redacts_the_key() {
    let cfg = config_with_key();
    let err = cpm_planner::llm::JudgmentError::scrubbed(
        JudgmentErrorKind::Upstream,
        format!("echo {SENTINEL} back"),
        cfg.api_key(),
    );
    assert!(!err.message().contains(SENTINEL));
}

#[test]
fn judgment_error_scrubbed_caps_the_message() {
    let err = cpm_planner::llm::JudgmentError::scrubbed(
        JudgmentErrorKind::Upstream,
        "x".repeat(MAX_ERROR_MESSAGE_CHARS * 2),
        config_with_key().api_key(),
    );
    assert_eq!(err.message().chars().count(), MAX_ERROR_MESSAGE_CHARS);
}

#[test]
fn jev_judge_reports_the_parsed_endpoint_host() {
    let cfg = config_with_key().with_jev_endpoint("https://OpenRouter.AI:443/api/v1/systemone");
    assert_eq!(
        JevJudge::new(&cfg).endpoint_host().as_deref(),
        Some("openrouter.ai")
    );
}

#[test]
fn jev_judge_reports_no_host_for_an_unparseable_endpoint() {
    let cfg = config_with_key().with_jev_endpoint("not a url");
    assert_eq!(JevJudge::new(&cfg).endpoint_host(), None);
}

#[test]
fn config_endpoint_host_is_taken_from_the_parsed_url() {
    let cfg = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        (
            "CPM_JEV_ENDPOINT",
            "https://user-free.example.com/x?h=evil.com",
        ),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(
        cfg.jev_endpoint_host().as_deref(),
        Some("user-free.example.com")
    );
}

// ---------------------------------------------------------------- openrouter

#[test]
fn openrouter_chat_client_debug_redacts_key() {
    let client = chat_client(&config_with_key());
    assert!(!format!("{client:?}").contains(SENTINEL));
}

// ---------------------------------------------------------------- fix round 1

fn endpoint_result(endpoint: &str) -> Result<Option<LlmConfig>, ConfigError> {
    LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_JEV_ENDPOINT", endpoint),
    ]))
}

#[test]
fn http_endpoint_to_remote_host_is_rejected() {
    let err = endpoint_result("http://openrouter.ai/api/v1/systemone").unwrap_err();
    assert!(matches!(err, ConfigError::InvalidEndpoint { .. }));
}

#[test]
fn http_loopback_endpoint_is_accepted() {
    let ok = [
        "http://127.0.0.1:8080/v1/systemone",
        "http://[::1]:8080/x",
        "http://localhost/x",
    ]
    .iter()
    .all(|e| matches!(endpoint_result(e), Ok(Some(_))));
    assert!(ok);
}

#[test]
fn https_remote_endpoint_is_accepted() {
    assert!(matches!(
        endpoint_result("https://example.com/v1/systemone"),
        Ok(Some(_))
    ));
}

#[test]
fn endpoint_with_credentials_is_rejected() {
    let err = endpoint_result("https://user:pw@example.com/v1/systemone").unwrap_err();
    assert!(matches!(err, ConfigError::InvalidEndpoint { .. }));
}

#[test]
fn endpoint_rejection_never_echoes_credentials() {
    let err = endpoint_result("https://user:hunter2secret@example.com/v1").unwrap_err();
    assert!(!format!("{err} {err:?}").contains("hunter2secret"));
}

#[test]
fn malformed_endpoint_is_rejected_at_config_time() {
    let err = endpoint_result("https://exa mple.com:99999/x").unwrap_err();
    assert!(matches!(err, ConfigError::InvalidEndpoint { .. }));
}

#[test]
fn key_excerpt_straddling_truncation_boundary_drops_key_prefix() {
    let key = config_with_key().api_key().clone();
    let text = format!("{}{SENTINEL}", "x".repeat(190));
    let excerpt = key.excerpt(&text, 200);
    assert!(!contains_key_fragment(&excerpt, &key));
}

fn contains_key_fragment(text: &str, key: &ApiKey) -> bool {
    let k: Vec<char> = key.expose().chars().collect();
    k.windows(8)
        .any(|w| text.contains(&w.iter().collect::<String>()))
}

#[tokio::test]
async fn key_straddling_truncation_boundary_is_not_leaked() {
    let key = config_with_key().api_key().clone();
    let mut rendered = String::new();
    for pad in [128, 180, 190, 195, 199, 290, 295] {
        let body = format!("{}{SENTINEL} tail", "x".repeat(pad));
        let err = decide_with(ResponseTemplate::new(500).set_body_string(body))
            .await
            .unwrap_err();
        rendered.push_str(&format!("{err} {err:?}\n"));
    }
    assert!(!contains_key_fragment(&rendered, &key), "{rendered}");
}

#[tokio::test]
async fn upstream_body_is_not_in_error_message() {
    let err = decide_with(ResponseTemplate::new(502).set_body_string("UPSTREAM-BODY-MARKER"))
        .await
        .unwrap_err();
    assert!(!err.to_string().contains("UPSTREAM-BODY-MARKER"));
}

#[tokio::test]
async fn upstream_error_message_names_status() {
    let err = decide_with(ResponseTemplate::new(503).set_body_string("down"))
        .await
        .unwrap_err();
    assert!(err.message().contains("503"));
}

#[tokio::test]
async fn error_message_is_capped() {
    let err = decide_with(ResponseTemplate::new(200).set_body_string("y".repeat(5000)))
        .await
        .unwrap_err();
    assert!(err.message().chars().count() <= MAX_ERROR_MESSAGE_CHARS);
}

#[test]
fn oversized_key_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("big.key");
    std::fs::write(&file, "k".repeat(4097)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let err = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        file.to_str().unwrap(),
    )]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::KeyFile { .. }));
}

#[cfg(unix)]
#[test]
fn fifo_key_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("key.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(&fifo)
        .status()
        .unwrap();
    assert!(status.success(), "mkfifo failed");
    let err = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        fifo.to_str().unwrap(),
    )]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::KeyFile { .. }));
}

#[cfg(unix)]
#[test]
fn empty_key_file_is_reported_as_ignored() {
    let (_dir, file) = key_file(0o600, "  \n");
    let err = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        file.to_str().unwrap(),
    )]))
    .unwrap_err();
    assert!(matches!(
        err,
        ConfigError::KeyFileIgnored {
            reason: KeyFileIgnoredReason::Empty,
            ..
        }
    ));
}

#[test]
fn key_file_ignored_error_names_reason() {
    let err = ConfigError::KeyFileIgnored {
        reason: KeyFileIgnoredReason::WorldReadable,
    };
    assert!(err.to_string().contains("key file ignored: world-readable"));
}

#[test]
fn partial_key_cut_by_truncation_boundary_is_dropped() {
    let key = config_with_key().api_key().clone();
    let partial: String = SENTINEL.chars().take(20).collect();
    let text = format!("{}{partial}", "x".repeat(190));
    assert!(!contains_key_fragment(&key.excerpt(&text, 200), &key));
}

// ---------------------------------------------------------------- review reasons

#[test]
fn ignored_key_file_review_reason_names_the_setting() {
    let err = ConfigError::KeyFileIgnored {
        reason: KeyFileIgnoredReason::WorldReadable,
    };
    assert_eq!(
        err.review_reason(),
        "key file ignored: world-readable (CPM_OPENROUTER_KEY_FILE)"
    );
}

#[test]
fn invalid_timeout_review_reason_names_the_variable() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_LLM_TIMEOUT_SECS", "0"),
    ]))
    .unwrap_err();
    assert!(
        err.review_reason()
            .starts_with("CPM_LLM_TIMEOUT_SECS invalid: ")
    );
}

#[test]
fn key_file_error_never_echoes_the_path() {
    let err = LlmConfig::from_lookup(lookup(&[(
        "CPM_OPENROUTER_KEY_FILE",
        "/nonexistent/secret-dir/k",
    )]))
    .unwrap_err();
    assert!(!format!("{err} {err:?} {}", err.review_reason()).contains("secret-dir"));
}

#[test]
fn invalid_timeout_error_never_echoes_the_value() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", "k"),
        ("CPM_LLM_TIMEOUT_SECS", SENTINEL),
    ]))
    .unwrap_err();
    assert!(!format!("{err} {err:?}").contains(SENTINEL));
}

// ---------------------------------------------------------------- final review

#[test]
fn jev_model_with_a_disallowed_character_is_rejected() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_JEV_MODEL", "typesafe/jev 1.13"),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidModel { .. }));
}

#[test]
fn jev_model_over_128_chars_is_rejected() {
    let long = "m".repeat(129);
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_JEV_MODEL", &long),
    ]))
    .unwrap_err();
    assert!(matches!(err, ConfigError::InvalidModel { .. }));
}

#[test]
fn jev_model_with_namespace_and_version_is_accepted() {
    let cfg = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", SENTINEL),
        ("CPM_JEV_MODEL", "typesafe/jev-1.13:beta_2"),
    ]))
    .unwrap()
    .unwrap();
    assert_eq!(cfg.jev_model(), "typesafe/jev-1.13:beta_2");
}

#[test]
fn invalid_model_review_reason_names_the_variable_only() {
    let err = LlmConfig::from_lookup(lookup(&[
        ("OPENROUTER_API_KEY", "k"),
        ("CPM_JEV_MODEL", "bad model!"),
    ]))
    .unwrap_err();
    assert!(
        err.review_reason().starts_with("CPM_JEV_MODEL invalid: ")
            && !err.review_reason().contains("bad model")
    );
}

#[tokio::test]
async fn provider_model_echoing_the_key_is_scrubbed() {
    let body = json!({
        "model": format!("typesafe/{SENTINEL}"),
        "answers": { "dep_real": { "type": "noul", "noul": 0.82 } }
    });
    let decisions = decide_with(ResponseTemplate::new(200).set_body_json(body))
        .await
        .unwrap();
    assert!(!decisions.model.contains(SENTINEL));
}

#[tokio::test]
async fn transport_error_never_echoes_the_endpoint_query() {
    // A port nothing listens on: the connection is refused (Transport).
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let cfg = config_with_key()
        .with_timeout(Duration::from_secs(5))
        .with_jev_endpoint(format!(
            "http://127.0.0.1:{port}/v1/systemone?token=QUERY-SECRET-91f2#FRAG-SECRET-77"
        ));
    let err = JevJudge::new(&cfg)
        .decide(state(), noul_question())
        .await
        .unwrap_err();
    let text = format!("{err} {err:?}");
    assert!(!text.contains("-SECRET-"), "{text}");
}
