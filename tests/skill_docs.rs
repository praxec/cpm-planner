//! The `deliverable-cpm` agent skill stays true to the server: every tool it
//! names exists, and its example plan file lints clean.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cpm_planner::PLAN_TOOL_NAMES;
use cpm_planner::lint::lint;
use cpm_planner::plan::PlanGraph;

fn skill_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills/deliverable-cpm")
}

/// SKILL.md plus every file under `examples/` and `reference/`.
fn skill_docs() -> Vec<PathBuf> {
    let root = skill_dir();
    let mut files = vec![root.join("SKILL.md")];
    for sub in ["examples", "reference"] {
        if let Ok(entries) = std::fs::read_dir(root.join(sub)) {
            files.extend(entries.map(|e| e.expect("dir entry").path()));
        }
    }
    files.sort();
    files
}

/// Every `plan.<verb>` token in `text`. A `plan.` preceded by a path or
/// identifier character (as in `small-plan.json`) is not a tool name.
fn plan_tokens(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut out = BTreeSet::new();
    let mut from = 0;
    while let Some(pos) = text[from..].find("plan.") {
        let start = from + pos;
        let boundary = start == 0 || {
            let prev = bytes[start - 1];
            !(prev.is_ascii_alphanumeric() || b"_-./".contains(&prev))
        };
        let verb_len = text[start + 5..]
            .bytes()
            .take_while(|b| b.is_ascii_lowercase() || *b == b'_')
            .count();
        if boundary && verb_len > 0 {
            out.insert(text[start..start + 5 + verb_len].to_string());
        }
        from = start + 5;
    }
    out
}

#[test]
fn every_plan_tool_named_in_the_skill_exists() {
    let unknown: Vec<String> = skill_docs()
        .iter()
        .flat_map(|path| {
            let text = std::fs::read_to_string(path).expect("readable skill doc");
            plan_tokens(&text)
                .into_iter()
                .filter(|t| !PLAN_TOOL_NAMES.contains(&t.as_str()))
                .map(|t| format!("{}: {t}", path.display()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(unknown.is_empty(), "unknown tool names: {unknown:?}");
}

#[test]
fn small_plan_example_parses_and_lints_clean() {
    let text = std::fs::read_to_string(skill_dir().join("examples/small-plan.json"))
        .expect("readable example plan");
    let graph: PlanGraph = serde_json::from_str(&text).expect("example parses as a plan file");
    let report = lint(&graph);
    assert!(report.findings.is_empty(), "{:#?}", report.findings);
}
