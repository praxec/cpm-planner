//! `plan.review` end to end through a fake adapter process
//! (`CPM_REVIEW_PROVIDER=command`), and the published conformance fixtures
//! in `docs/review-provider/fixtures/`.
//!
//! Every adapter here is a shell script written to a temp dir; no test
//! touches the network or needs an OpenRouter key.

use std::path::PathBuf;

use cpm_planner::llm::command::{AdapterReply, AdapterRequest, REQUEST_KIND, safe_token};

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("docs/review-provider/fixtures")
        .join(name)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(fixture_path(name)).expect("fixture")
}

fn request_fixture() -> AdapterRequest {
    serde_json::from_str(&fixture("request-v1.json")).expect("request-v1.json")
}

fn reply_fixture(name: &str) -> AdapterReply {
    serde_json::from_str(&fixture(name)).expect("reply fixture")
}

/// The plan `request-v1.json` was produced from (used only by the unix adapter tests).
#[cfg(unix)]
fn fixture_graph() -> cpm_planner::plan::PlanGraph {
    serde_json::from_value(serde_json::json!({ "deliverables": [
        { "id": "parser", "owned_files": ["src/parser.rs"], "prerequisites": [],
          "estimated_effort_hours": 4.0,
          "metadata": { "artifact": "src/parser.rs parses plan files" } },
        { "id": "cli", "owned_files": ["src/cli.rs"],
          "prerequisites": [{ "id": "parser", "consumes": "src/parser.rs parse()" }],
          "estimated_effort_hours": 6.0,
          "metadata": { "artifact": "src/cli.rs runs the parser from the command line" } }
    ]}))
    .expect("fixture graph")
}

// ------------------------------------------------------------- fixtures

#[test]
fn the_request_fixture_parses_as_a_v1_request() {
    assert_eq!(request_fixture().kind, REQUEST_KIND);
}

#[test]
fn the_answer_fixture_parses_as_a_v1_answer() {
    assert!(matches!(
        reply_fixture("answer-v1.json"),
        AdapterReply::Ok {
            schema_version: 1,
            ..
        }
    ));
}

#[test]
fn the_refused_fixture_parses_as_a_v1_refusal() {
    assert!(matches!(
        reply_fixture("refused-v1.json"),
        AdapterReply::Refused {
            schema_version: 1,
            ..
        }
    ));
}

#[test]
fn the_answer_fixture_answers_every_request_question() {
    let AdapterReply::Ok { answers, .. } = reply_fixture("answer-v1.json") else {
        panic!("answer-v1.json is not an ok reply");
    };
    assert_eq!(
        answers.keys().collect::<Vec<_>>(),
        request_fixture().questions.keys().collect::<Vec<_>>()
    );
}

#[test]
fn the_answer_fixture_model_is_kept_as_a_token() {
    let AdapterReply::Ok { model, .. } = reply_fixture("answer-v1.json") else {
        panic!("answer-v1.json is not an ok reply");
    };
    assert!(model.as_deref().and_then(safe_token).is_some());
}

#[test]
fn the_refused_fixture_reason_is_kept_as_a_token() {
    let AdapterReply::Refused { reason, .. } = reply_fixture("refused-v1.json") else {
        panic!("refused-v1.json is not a refusal");
    };
    assert!(reason.as_deref().and_then(safe_token).is_some());
}

#[test]
fn a_key_shaped_token_is_not_kept() {
    assert_eq!(
        safe_token("sk-or-v1-0123456789abcdef0123456789abcdef"),
        None
    );
}

// ------------------------------------------------------------- adapters

#[cfg(unix)]
mod adapter {
    use super::*;

    use std::collections::BTreeMap;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::time::Duration;

    use async_trait::async_trait;
    use cpm_planner::llm::command::{CommandConfig, CommandJudge};
    use cpm_planner::llm::{Decisions, JudgmentError, JudgmentModel, Question};
    use cpm_planner::review::{Judge, ReviewReport, ReviewRequest, ReviewStatus, review};

    /// A key-shaped value no review result may ever hold.
    const SECRET: &str = "sk-or-v1-0123456789abcdef0123456789abcdef";

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("adapter.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write adapter");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    /// Review the fixture graph through an adapter running `body`.
    async fn review_with(body: &str, timeout_ms: u64) -> ReviewReport {
        let dir = tempfile::tempdir().expect("tempdir");
        let judge = CommandJudge::new(CommandConfig {
            path: script(dir.path(), body),
            forward_env: Vec::new(),
            timeout: Duration::from_millis(timeout_ms),
        });
        review(
            &fixture_graph(),
            &ReviewRequest::default(),
            Judge::Model(&judge),
        )
        .await
        .expect("review")
    }

    /// An adapter that answers with the published answer fixture.
    fn answering() -> String {
        format!(
            "cat >/dev/null; cat '{}'",
            fixture_path("answer-v1.json").display()
        )
    }

    async fn answered() -> ReviewReport {
        review_with(&answering(), 5000).await
    }

    fn json_of(report: &ReviewReport) -> String {
        serde_json::to_string(report).expect("report json")
    }

    // ---------------------------------------------------------- conformance

    #[tokio::test]
    async fn the_request_fixture_is_what_a_review_of_its_graph_sends() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = dir.path().join("seen.json");
        let body = format!(
            "cat >'{}'; cat '{}'",
            seen.display(),
            fixture_path("answer-v1.json").display()
        );
        let judge = CommandJudge::new(CommandConfig {
            path: script(dir.path(), &body),
            forward_env: Vec::new(),
            timeout: Duration::from_secs(5),
        });
        review(
            &fixture_graph(),
            &ReviewRequest::default(),
            Judge::Model(&judge),
        )
        .await
        .expect("review");
        let sent: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&seen).expect("seen")).expect("sent json");
        let published: serde_json::Value =
            serde_json::from_str(&fixture("request-v1.json")).expect("fixture json");
        assert_eq!(sent, published);
    }

    // -------------------------------------------------------------- outcomes

    #[tokio::test]
    async fn a_delegated_review_completes_without_an_openrouter_key() {
        assert_eq!(answered().await.status, ReviewStatus::Ok);
    }

    #[tokio::test]
    async fn a_delegated_review_reports_the_adapters_findings() {
        assert!(!answered().await.findings.is_empty());
    }

    #[tokio::test]
    async fn an_adapter_refusal_is_review_unavailable_as_refused() {
        let report = review_with(
            &format!(
                "cat >/dev/null; cat '{}'",
                fixture_path("refused-v1.json").display()
            ),
            5000,
        )
        .await;
        assert_eq!(
            (report.status, report.failure_class.as_deref()),
            (ReviewStatus::ReviewUnavailable, Some("refused"))
        );
    }

    #[tokio::test]
    async fn a_slow_adapter_is_review_unavailable_as_timeout() {
        let report = review_with("cat >/dev/null; exec sleep 30", 300).await;
        assert_eq!(
            (report.status, report.failure_class.as_deref()),
            (ReviewStatus::ReviewUnavailable, Some("timeout"))
        );
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_timed_out_adapter_is_killed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("pid");
        review_with(
            &format!(
                "cat >/dev/null; echo $$ >'{}'; exec sleep 30",
                pid_file.display()
            ),
            300,
        )
        .await;
        let pid = std::fs::read_to_string(&pid_file).expect("pid");
        let stat = PathBuf::from("/proc").join(pid.trim()).join("stat");
        // Gone, or a zombie waiting to be reaped: either way no longer running.
        let stopped = || {
            std::fs::read_to_string(&stat).map_or(true, |s| {
                s.rsplit(')')
                    .next()
                    .is_some_and(|r| r.trim_start().starts_with('Z'))
            })
        };
        let mut waited = 0;
        while !stopped() && waited < 50 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            waited += 1;
        }
        assert!(stopped(), "adapter {} still running", pid.trim());
    }

    #[tokio::test]
    async fn a_failing_adapter_is_review_unavailable_as_upstream() {
        let report = review_with("cat >/dev/null; exit 3", 5000).await;
        assert_eq!(
            (report.status, report.failure_class.as_deref()),
            (ReviewStatus::ReviewUnavailable, Some("upstream"))
        );
    }

    #[tokio::test]
    async fn malformed_adapter_output_is_review_unavailable_as_decode() {
        let report = review_with("cat >/dev/null; echo '{\"schema_version\":1,'", 5000).await;
        assert_eq!(
            (report.status, report.failure_class.as_deref()),
            (ReviewStatus::ReviewUnavailable, Some("decode"))
        );
    }

    #[tokio::test]
    async fn a_wrong_schema_version_is_review_unavailable_as_decode() {
        let report = review_with(
            r#"cat >/dev/null; echo '{"schema_version":2,"status":"ok","answers":{}}'"#,
            5000,
        )
        .await;
        assert_eq!(
            (report.status, report.failure_class.as_deref()),
            (ReviewStatus::ReviewUnavailable, Some("decode"))
        );
    }

    #[tokio::test]
    async fn an_oversized_reply_is_review_unavailable_as_decode() {
        // Valid JSON padded with whitespace past the 16 MiB cap, so only the
        // size can fail it.
        let report = review_with(
            r#"cat >/dev/null
printf '{"schema_version":1,"status":"ok","answers":{}}'
head -c 17000000 /dev/zero | tr '\0' ' '"#,
            10_000,
        )
        .await;
        assert_eq!(
            (report.status, report.failure_class.as_deref()),
            (ReviewStatus::ReviewUnavailable, Some("decode"))
        );
    }

    // ------------------------------------------------------------ redaction

    #[tokio::test]
    async fn a_secret_in_the_adapter_environment_never_reaches_a_refusal() {
        let report = review_with(
            &format!(
                r#"export ADAPTER_TOKEN={SECRET}
cat >/dev/null
echo "{{\"schema_version\":1,\"status\":\"refused\",\"reason\":\"$ADAPTER_TOKEN\"}}""#
            ),
            5000,
        )
        .await;
        assert!(!json_of(&report).contains(SECRET), "{}", json_of(&report));
    }

    #[tokio::test]
    async fn a_secret_on_adapter_stderr_never_reaches_the_review() {
        let report = review_with(&format!("echo '{SECRET}' >&2; {}", answering()), 5000).await;
        assert!(!json_of(&report).contains(SECRET), "{}", json_of(&report));
    }

    #[tokio::test]
    async fn a_secret_in_a_refusal_reason_never_reaches_the_review() {
        let report = review_with(
            &format!(
                r#"cat >/dev/null; echo '{{"schema_version":1,"status":"refused","reason":"key {SECRET} rejected"}}'"#
            ),
            5000,
        )
        .await;
        assert!(!json_of(&report).contains(SECRET), "{}", json_of(&report));
    }

    #[tokio::test]
    async fn a_secret_reported_as_the_model_never_reaches_the_review() {
        let report = review_with(
            &format!(
                r#"cat >/dev/null; echo '{{"schema_version":1,"status":"ok","model":"{SECRET}","answers":{{}}}}'"#
            ),
            5000,
        )
        .await;
        assert!(!json_of(&report).contains(SECRET), "{}", json_of(&report));
    }

    // ------------------------------------------------------------ provenance

    #[tokio::test]
    async fn a_delegated_review_records_the_command_provider() {
        assert_eq!(answered().await.provider.as_deref(), Some("command"));
    }

    #[tokio::test]
    async fn a_delegated_review_records_the_adapters_model_token() {
        assert_eq!(answered().await.model.as_deref(), Some("typesafe/jev-1.13"));
    }

    #[tokio::test]
    async fn a_delegated_review_records_the_request_digest() {
        let digest = answered().await.prompt_hash.unwrap_or_default();
        assert!(
            digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()),
            "{digest}"
        );
    }

    #[tokio::test]
    async fn a_delegated_review_records_the_adapters_usage() {
        let usage = answered().await.usage.expect("usage");
        assert_eq!(
            (usage.input_tokens, usage.output_tokens),
            (Some(812), Some(64))
        );
    }

    #[tokio::test]
    async fn a_failed_delegated_review_still_records_the_provider() {
        let report = review_with("cat >/dev/null; exit 1", 5000).await;
        assert_eq!(report.provider.as_deref(), Some("command"));
    }

    /// An in-process judge that answers nothing, like a direct provider.
    struct InProcess;

    #[async_trait]
    impl JudgmentModel for InProcess {
        async fn decide(
            &self,
            _state: serde_json::Value,
            _questions: BTreeMap<String, Question>,
        ) -> Result<Decisions, JudgmentError> {
            Ok(Decisions {
                answers: BTreeMap::new(),
                model: "fake/judge".to_string(),
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn an_in_process_review_records_the_direct_provider() {
        let report = review(
            &fixture_graph(),
            &ReviewRequest::default(),
            Judge::Model(&InProcess),
        )
        .await
        .expect("review");
        assert_eq!(report.provider.as_deref(), Some("direct"));
    }
}
