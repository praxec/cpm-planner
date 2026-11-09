//! `plan.review` through a delegated adapter process (`CPM_REVIEW_PROVIDER=command`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use cpm_planner::llm::command::{CommandConfig, ReviewProvider, readiness};

fn lookup(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
    move |name| {
        pairs
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| (*v).to_string())
    }
}

#[test]
fn an_unset_provider_is_direct() {
    assert_eq!(
        ReviewProvider::from_lookup(lookup(&[])).expect("provider"),
        ReviewProvider::Direct
    );
}

#[test]
fn an_unknown_provider_is_refused() {
    assert!(ReviewProvider::from_lookup(lookup(&[("CPM_REVIEW_PROVIDER", "openrouter")])).is_err());
}

#[test]
fn the_command_provider_requires_a_command() {
    assert!(ReviewProvider::from_lookup(lookup(&[("CPM_REVIEW_PROVIDER", "command")])).is_err());
}

#[test]
fn the_command_provider_refuses_a_relative_path() {
    assert!(
        ReviewProvider::from_lookup(lookup(&[
            ("CPM_REVIEW_PROVIDER", "command"),
            ("CPM_REVIEW_COMMAND", "adapter.sh"),
        ]))
        .is_err()
    );
}

#[test]
fn the_openrouter_key_can_never_be_forwarded_to_an_adapter() {
    let exe = std::env::current_exe().expect("exe");
    let path = Box::leak(exe.display().to_string().into_boxed_str());
    let pairs: &'static [(&str, &str)] = Box::leak(Box::new([
        ("CPM_REVIEW_PROVIDER", "command"),
        ("CPM_REVIEW_COMMAND", &*path),
        ("CPM_REVIEW_COMMAND_ENV", "OPENROUTER_API_KEY"),
    ]));
    assert!(ReviewProvider::from_lookup(lookup(pairs)).is_err());
}

#[test]
fn the_readiness_report_never_holds_a_value() {
    let report = readiness(lookup(&[("OPENROUTER_API_KEY", "sk-secret-value")])).to_string();
    assert!(!report.contains("sk-secret-value"), "{report}");
}

#[test]
fn the_readiness_report_names_a_present_key() {
    let report = readiness(lookup(&[("OPENROUTER_API_KEY", "sk-secret-value")]));
    assert_eq!(report["ready"], true);
}

#[test]
fn a_disabled_provider_is_not_ready() {
    let report = readiness(lookup(&[("CPM_REVIEW_PROVIDER", "disabled")]));
    assert_eq!(report["ready"], false);
}

fn config(path: &Path, timeout_ms: u64) -> CommandConfig {
    CommandConfig {
        path: PathBuf::from(path),
        forward_env: Vec::new(),
        timeout: Duration::from_millis(timeout_ms),
    }
}

#[cfg(unix)]
mod adapter {
    use super::*;
    use cpm_planner::llm::command::CommandJudge;
    use cpm_planner::llm::{JudgmentErrorKind, JudgmentModel, Question};
    use cpm_planner::plan::PlanGraph;
    use cpm_planner::review::{Judge, ReviewRequest, ReviewStatus, review};
    use serde_json::json;
    use std::os::unix::fs::PermissionsExt;

    fn script(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("adapter.sh");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write adapter");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    fn questions() -> BTreeMap<String, Question> {
        serde_json::from_value(json!({
            "q00": { "type": "noul", "instructions": { "question": "true?" } }
        }))
        .expect("questions")
    }

    async fn error_kind(body: &str, timeout_ms: u64) -> JudgmentErrorKind {
        let dir = tempfile::tempdir().expect("tempdir");
        let judge = CommandJudge::new(config(&script(dir.path(), body), timeout_ms));
        judge
            .decide(json!({}), questions())
            .await
            .expect_err("adapter should fail")
            .kind()
    }

    #[tokio::test]
    async fn an_adapter_answer_becomes_a_decision() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = script(
            dir.path(),
            r#"cat >/dev/null
echo '{"schema_version":1,"status":"ok","model":"jev-1","answers":{"q00":{"type":"noul","noul":0.75}}}'"#,
        );
        let decisions = CommandJudge::new(config(&path, 5000))
            .decide(json!({}), questions())
            .await
            .expect("decisions");
        assert_eq!(decisions.answers.len(), 1);
    }

    #[tokio::test]
    async fn an_adapter_receives_the_versioned_request() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = dir.path().join("seen.json");
        let path = script(
            dir.path(),
            &format!(
                r#"cat >"{}"
echo '{{"schema_version":1,"status":"ok","answers":{{}}}}'"#,
                seen.display()
            ),
        );
        CommandJudge::new(config(&path, 5000))
            .decide(json!({"makespan_hours": 3}), questions())
            .await
            .expect("decisions");
        let sent: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&seen).expect("seen")).expect("json");
        assert_eq!(sent["kind"], "plan.review.judgment");
    }

    #[tokio::test]
    async fn an_adapter_refusal_is_a_refused_error() {
        let kind = error_kind(
            r#"cat >/dev/null; echo '{"schema_version":1,"status":"refused","reason":"policy_denied"}'"#,
            5000,
        )
        .await;
        assert_eq!(kind, JudgmentErrorKind::Refused);
    }

    #[tokio::test]
    async fn a_slow_adapter_times_out() {
        assert_eq!(
            error_kind("sleep 30", 100).await,
            JudgmentErrorKind::Timeout
        );
    }

    #[tokio::test]
    async fn malformed_adapter_output_is_a_decode_error() {
        assert_eq!(
            error_kind("cat >/dev/null; echo not-json", 5000).await,
            JudgmentErrorKind::Decode
        );
    }

    #[tokio::test]
    async fn an_unsupported_schema_version_is_a_decode_error() {
        let kind = error_kind(
            r#"cat >/dev/null; echo '{"schema_version":2,"status":"ok","answers":{}}'"#,
            5000,
        )
        .await;
        assert_eq!(kind, JudgmentErrorKind::Decode);
    }

    #[tokio::test]
    async fn a_failing_adapter_is_an_upstream_error() {
        assert_eq!(
            error_kind("cat >/dev/null; exit 3", 5000).await,
            JudgmentErrorKind::Upstream
        );
    }

    #[tokio::test]
    async fn a_secret_in_an_adapter_refusal_never_reaches_the_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = script(
            dir.path(),
            r#"cat >/dev/null
echo sk-secret-value >&2
echo '{"schema_version":1,"status":"refused","reason":"bad key sk-secret-value"}'"#,
        );
        let error = CommandJudge::new(config(&path, 5000))
            .decide(json!({}), questions())
            .await
            .expect_err("refused");
        assert!(!error.message().contains("sk-secret-value"), "{error}");
    }

    #[tokio::test]
    async fn the_adapter_does_not_inherit_the_coordinator_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let seen = dir.path().join("env.txt");
        let path = script(
            dir.path(),
            &format!(
                r#"cat >/dev/null
/usr/bin/env >"{}"
echo '{{"schema_version":1,"status":"ok","answers":{{}}}}'"#,
                seen.display()
            ),
        );
        // CARGO_MANIFEST_DIR is set for every test process and never forwarded.
        CommandJudge::new(config(&path, 5000))
            .decide(json!({}), questions())
            .await
            .expect("decisions");
        let env = std::fs::read_to_string(&seen).expect("env");
        assert!(!env.contains("CARGO_"), "{env}");
    }

    #[tokio::test]
    async fn a_failed_adapter_leaves_the_review_unavailable_without_fallback() {
        let dir = tempfile::tempdir().expect("tempdir");
        let judge = CommandJudge::new(config(&script(dir.path(), "exit 1"), 5000));
        let graph: PlanGraph = serde_json::from_value(json!({ "deliverables": [
            { "id": "a", "owned_files": [], "prerequisites": [], "estimated_effort_hours": 1.0,
              "metadata": { "artifact": "x" } }
        ]}))
        .expect("graph");
        let report = review(&graph, &ReviewRequest::default(), Judge::Model(&judge))
            .await
            .expect("review");
        assert_eq!(report.status, ReviewStatus::ReviewUnavailable);
    }
}
