//! The agent skills under `skills/` stay true to the server: every tool they
//! name exists, their front matter fits the shared SKILL.md limits, the
//! `cpm-*` family links to the `deliverable-cpm` method, every relative link
//! resolves, and the example plan file lints clean.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cpm_planner::PLAN_TOOL_NAMES;
use cpm_planner::lint::lint;
use cpm_planner::plan::PlanGraph;

/// The strictest `description` limit of any SKILL.md target
/// (agentskills.io and VS Code; see docs/agents/tool-matrix.md).
const DESCRIPTION_LIMIT: usize = 1024;

/// Most body lines (after the front matter) of a focused skill.
const BODY_LINE_LIMIT: usize = 150;

/// Longest skill `name` allowed by the agentskills.io spec.
const NAME_LIMIT: usize = 64;

/// The five focused skills, each linking to the shared method.
const CPM_SKILLS: [&str; 5] = ["cpm-ev", "cpm-improve", "cpm-plan", "cpm-revise", "cpm-run"];

fn skills_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("skills")
}

fn skill_dir() -> PathBuf {
    skills_root().join("deliverable-cpm")
}

/// Every directory under `skills/`, sorted. A directory without a SKILL.md
/// is kept so the tests report it instead of skipping it.
fn skill_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(skills_root())
        .expect("readable skills/")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|n| n.to_str())
        .expect("utf-8 skill directory name")
        .to_string()
}

/// `path` with CRLF normalised to LF (Windows checkouts); empty when it
/// cannot be read, so a missing file fails the checks that need it.
fn read_lf(path: &Path) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .replace("\r\n", "\n")
}

/// SKILL.md of `dir`, LF-normalised.
fn read_skill(dir: &Path) -> String {
    read_lf(&dir.join("SKILL.md"))
}

/// Every `.md` file anywhere under `skills/`, sorted.
fn markdown_files() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("readable directory") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "md") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&skills_root(), &mut out);
    out.sort();
    out
}

/// Every relative Markdown link target `](target)` in `text`, without any
/// `#fragment`. URLs, pure fragments and `mailto:` links are skipped.
fn relative_links(text: &str) -> Vec<String> {
    text.split("](")
        .skip(1)
        .filter_map(|rest| rest.split(')').next())
        .map(|t| t.split('#').next().unwrap_or_default().trim().to_string())
        .filter(|t| !t.is_empty() && !t.contains("://") && !t.starts_with("mailto:"))
        .collect()
}

/// `name` is 1..=64 chars of `^[a-z0-9]+(-[a-z0-9]+)*$`.
fn is_valid_skill_name(name: &str) -> bool {
    name.len() <= NAME_LIMIT
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        })
}

/// The lines of `dir`'s SKILL.md after the closing front-matter fence.
fn body_line_count(dir: &Path) -> usize {
    let text = read_skill(dir);
    text.strip_prefix("---\n")
        .and_then(|rest| rest.find("\n---\n").map(|end| &rest[end + 5..]))
        .unwrap_or(&text)
        .lines()
        .count()
}

/// Every skill's SKILL.md plus every file under its `examples/` and
/// `reference/`.
fn skill_docs() -> Vec<PathBuf> {
    let mut files = Vec::new();
    for root in skill_dirs() {
        files.push(root.join("SKILL.md"));
        for sub in ["examples", "reference"] {
            if let Ok(entries) = std::fs::read_dir(root.join(sub)) {
                files.extend(entries.map(|e| e.expect("dir entry").path()));
            }
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

/// The value of `key` in `dir`'s SKILL.md YAML frontmatter (single-line
/// values). Windows checkouts may convert the skill to CRLF; either parses.
fn frontmatter_of(dir: &Path, key: &str) -> Option<String> {
    let text = read_skill(dir);
    let body = text.strip_prefix("---\n")?;
    let end = body.find("\n---\n")?;
    let prefix = format!("{key}: ");
    body[..end]
        .lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .map(str::to_string)
}

fn frontmatter(key: &str) -> Option<String> {
    frontmatter_of(&skill_dir(), key)
}

#[test]
fn skill_frontmatter_names_deliverable_cpm() {
    assert_eq!(frontmatter("name").as_deref(), Some("deliverable-cpm"));
}

#[test]
fn skill_description_starts_with_use_when_and_fits_limit() {
    let description = frontmatter("description").unwrap_or_default();
    assert!(
        description.starts_with("Use when") && description.chars().count() <= DESCRIPTION_LIMIT,
        "description must start with \"Use when\" and be at most {DESCRIPTION_LIMIT} chars: {description:?}"
    );
}

#[test]
fn every_plan_tool_named_in_every_skill_exists() {
    let unknown: Vec<String> = skill_docs()
        .iter()
        .flat_map(|path| {
            let text = read_lf(path);
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

#[test]
fn every_skill_frontmatter_name_equals_its_directory() {
    let mismatched: Vec<String> = skill_dirs()
        .iter()
        .filter(|dir| frontmatter_of(dir, "name").as_deref() != Some(dir_name(dir).as_str()))
        .map(|dir| dir_name(dir))
        .collect();
    assert!(
        mismatched.is_empty(),
        "name differs from directory: {mismatched:?}"
    );
}

#[test]
fn every_skill_description_starts_with_use_when_and_fits_limit() {
    let bad: Vec<String> = skill_dirs()
        .iter()
        .filter(|dir| {
            let d = frontmatter_of(dir, "description").unwrap_or_default();
            !(d.starts_with("Use when") && d.chars().count() <= DESCRIPTION_LIMIT)
        })
        .map(|dir| dir_name(dir))
        .collect();
    assert!(
        bad.is_empty(),
        "description must start with \"Use when\" and be at most {DESCRIPTION_LIMIT} chars: {bad:?}"
    );
}

#[test]
fn every_cpm_skill_links_to_deliverable_cpm() {
    let unlinked: Vec<String> = skill_dirs()
        .iter()
        .filter(|dir| dir_name(dir).starts_with("cpm-"))
        .filter(|dir| !read_skill(dir).contains("](../deliverable-cpm/SKILL.md)"))
        .map(|dir| dir_name(dir))
        .collect();
    assert!(
        unlinked.is_empty(),
        "no link to ../deliverable-cpm/SKILL.md: {unlinked:?}"
    );
}

#[test]
fn cpm_skill_directories_are_the_five_expected() {
    let found: BTreeSet<String> = std::fs::read_dir(skills_root())
        .expect("readable skills/")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.is_dir())
        .map(|p| dir_name(&p))
        .filter(|n| n.starts_with("cpm-"))
        .collect();
    let expected: BTreeSet<String> = CPM_SKILLS.iter().map(|s| s.to_string()).collect();
    assert_eq!(found, expected);
}

#[test]
fn every_skill_directory_has_a_skill_md() {
    let missing: Vec<String> = skill_dirs()
        .iter()
        .filter(|dir| !dir.join("SKILL.md").is_file())
        .map(|dir| dir_name(dir))
        .collect();
    assert!(
        missing.is_empty(),
        "skill directories without SKILL.md: {missing:?}"
    );
}

#[test]
fn every_skill_name_is_lowercase_hyphenated_and_at_most_64_chars() {
    let invalid: Vec<String> = skill_dirs()
        .iter()
        .map(|dir| frontmatter_of(dir, "name").unwrap_or_default())
        .filter(|name| !is_valid_skill_name(name))
        .collect();
    assert!(invalid.is_empty(), "invalid skill names: {invalid:?}");
}

#[test]
fn every_relative_link_in_the_skills_resolves() {
    let broken: Vec<String> = markdown_files()
        .iter()
        .flat_map(|path| {
            let base = path.parent().expect("file has a parent").to_path_buf();
            relative_links(&read_lf(path))
                .into_iter()
                .filter(move |target| !base.join(target).exists())
                .map(move |target| format!("{}: {target}", path.display()))
        })
        .collect();
    assert!(broken.is_empty(), "broken relative links: {broken:?}");
}

#[test]
fn every_skill_body_is_at_most_150_lines() {
    let long: Vec<String> = skill_dirs()
        .iter()
        .filter(|dir| dir_name(dir) != "deliverable-cpm")
        .filter(|dir| body_line_count(dir) > BODY_LINE_LIMIT)
        .map(|dir| format!("{}: {} lines", dir_name(dir), body_line_count(dir)))
        .collect();
    assert!(
        long.is_empty(),
        "bodies over {BODY_LINE_LIMIT} lines: {long:?}"
    );
}
