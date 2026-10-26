use std::process::Command;

use cpm_planner::plan::PlanGraph;
use serde_json::Value;

fn binary() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_cpm-planner"));
    // Offline release commands must work even if normal server state cannot open.
    cmd.env(
        "CPM_PLANNER_DB",
        "/nonexistent-release-cli-test/not-a-database",
    );
    cmd
}

#[test]
fn cli_compile_prints_a_syncable_plan_without_opening_the_database() {
    let output = binary()
        .args(["release", "compile", "examples/release-workflow.json"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph: PlanGraph = serde_json::from_slice(&output.stdout).unwrap();
    assert!(cpm_planner::lint::lint(&graph).clean);
}

#[test]
fn cli_diff_roundtrips_the_compiled_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.json");
    let compiled = binary()
        .args(["release", "compile", "examples/release-workflow.json"])
        .output()
        .unwrap();
    std::fs::write(&path, compiled.stdout).unwrap();
    let output = binary()
        .args(["release", "diff", "examples/release-workflow.json"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["stale"],
        false
    );
}

#[test]
fn cli_lint_names_missing_cardinality_and_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("workflow.json");
    let mut workflow: Value =
        serde_json::from_str(include_str!("../examples/release-workflow.json")).unwrap();
    workflow["repositories"]
        .as_object_mut()
        .unwrap()
        .remove("cardinality");
    std::fs::write(&path, serde_json::to_vec(&workflow).unwrap()).unwrap();
    let output = binary()
        .args(["release", "lint"])
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(
        (
            output.status.code(),
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["findings"][0]["code"]
                .as_str()
        ),
        (Some(1), Some("HIDDEN_CARDINALITY"))
    );
}

#[test]
fn cli_diff_exits_one_for_stale_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.json");
    let mut graph = cpm_planner::release_dag::compile(
        &serde_json::from_str(include_str!("../examples/release-workflow.json")).unwrap(),
    )
    .unwrap()
    .graph;
    graph.deliverables[0].metadata["release"]["policy_digest"] = "stale".into();
    std::fs::write(&path, serde_json::to_vec(&graph).unwrap()).unwrap();
    let output = binary()
        .args(["release", "diff", "examples/release-workflow.json"])
        .arg(path)
        .output()
        .unwrap();
    assert_eq!(
        (
            output.status.code(),
            serde_json::from_slice::<Value>(&output.stdout).unwrap()["stale"].as_bool()
        ),
        (Some(1), Some(true))
    );
}

#[test]
fn cli_diff_exits_one_for_a_graph_dispatch_policy_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("main.json");
    let mut graph = cpm_planner::release_dag::compile(
        &serde_json::from_str(include_str!("../examples/release-workflow.json")).unwrap(),
    )
    .unwrap()
    .graph;
    graph.max_chained_dispatch = Some(1);
    std::fs::write(&path, serde_json::to_vec(&graph).unwrap()).unwrap();
    let output = binary()
        .args(["release", "diff", "examples/release-workflow.json"])
        .arg(path)
        .output()
        .unwrap();
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        (
            output.status.code(),
            result["stale"].as_bool(),
            result["graph_changes"].clone()
        ),
        (
            Some(1),
            Some(true),
            serde_json::json!(["max_chained_dispatch"])
        )
    );
}

#[test]
fn cli_refuses_oversized_input_before_parsing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("too-large.json");
    std::fs::write(
        &path,
        vec![b' '; cpm_planner::release_dag::MAX_INPUT_BYTES as usize + 1],
    )
    .unwrap();
    let output = binary()
        .args(["release", "compile"])
        .arg(path)
        .output()
        .unwrap();
    assert!(
        output.status.code() == Some(1)
            && String::from_utf8_lossy(&output.stderr).contains("input exceeds 8 MiB")
    );
}

#[test]
fn cli_bad_arguments_have_usage_exit_code() {
    assert_eq!(
        binary()
            .args(["release", "compile"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
}
