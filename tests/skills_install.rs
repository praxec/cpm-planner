//! `cpm-planner skills install|uninstall|list`, `--version` and the no-argument
//! MCP server, driven through the real binary against temp directories.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use cpm_planner::skills::{
    BLOCK_BEGIN, BLOCK_END, LOCK_FILE, MANIFEST_FILE, embedded_skills, sha256_hex,
    skill_description,
};
use tempfile::TempDir;

/// Run the binary with `home` as the home directory and working directory.
fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cpm-planner"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .current_dir(home)
        .output()
        .expect("the binary runs")
}

fn project_cmd(dir: &Path, command: &str, target: &str, extra: &[&str]) -> Output {
    let dir_str = dir.to_str().expect("utf-8 temp path");
    let mut args = vec!["skills", command, "--target", target, "--project", dir_str];
    args.extend_from_slice(extra);
    run(dir, &args)
}

fn install(dir: &Path, target: &str, extra: &[&str]) -> Output {
    project_cmd(dir, "install", target, extra)
}

fn uninstall(dir: &Path, target: &str, extra: &[&str]) -> Output {
    project_cmd(dir, "uninstall", target, extra)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Paths of the embedded files that are missing under `root`.
fn missing_skill_files(root: &Path) -> Vec<String> {
    embedded_skills()
        .iter()
        .map(|(rel, _)| *rel)
        .filter(|rel| !root.join(rel).is_file())
        .map(String::from)
        .collect()
}

/// Install `target` into a fresh project; (exit code, files missing under `roots`).
fn installed(target: &str, roots: &[&str]) -> (Option<i32>, Vec<String>) {
    let dir = TempDir::new().unwrap();
    let code = install(dir.path(), target, &[]).status.code();
    let missing = roots
        .iter()
        .flat_map(|r| missing_skill_files(&dir.path().join(r)))
        .collect();
    (code, missing)
}

fn ok_and_complete() -> (Option<i32>, Vec<String>) {
    (Some(0), Vec::new())
}

fn embedded(rel: &str) -> &'static str {
    embedded_skills()
        .iter()
        .find(|(p, _)| *p == rel)
        .map(|(_, c)| *c)
        .expect("embedded skill file")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).expect("readable file")
}

fn agents_md(dir: &Path) -> PathBuf {
    dir.join("AGENTS.md")
}

fn claude_plan_skill(dir: &Path) -> PathBuf {
    dir.join(".claude/skills/cpm-plan/SKILL.md")
}

/// The per-file lines of an install/uninstall run (before the summary).
fn file_lines(out: &str) -> Vec<&str> {
    out.lines()
        .take_while(|l| !l.starts_with("cpm-planner "))
        .collect()
}

/// Rewrite the manifest in `root` with `edit`.
fn edit_manifest(root: &Path, edit: impl FnOnce(&mut serde_json::Value)) {
    let path = root.join(MANIFEST_FILE);
    let mut value: serde_json::Value = serde_json::from_str(&read(&path)).expect("manifest json");
    edit(&mut value);
    std::fs::write(&path, value.to_string()).expect("manifest written");
}

/// Install agents-md, then make AGENTS.md hold `prefix + old block + suffix`
/// as an older cpm-planner would have left it (manifest hash included).
fn agents_md_with_old_block(dir: &Path, prefix: &str, suffix: &str) {
    install(dir, "agents-md", &[]);
    let old_block = format!("{BLOCK_BEGIN}\nold cpm-planner notes\n{BLOCK_END}");
    std::fs::write(agents_md(dir), format!("{prefix}{old_block}{suffix}")).expect("write");
    let hash = sha256_hex(old_block.as_bytes());
    edit_manifest(&dir.join(".agents/skills"), |m| {
        m["agents_md_block"] = hash.into();
    });
}

/// Install agents-md over an AGENTS.md holding `text`; (exit code, AGENTS.md after).
fn install_over_agents_md(text: &str, extra: &[&str]) -> (Option<i32>, String) {
    let dir = TempDir::new().unwrap();
    std::fs::write(agents_md(dir.path()), text).unwrap();
    let code = install(dir.path(), "agents-md", extra).status.code();
    (code, read(&agents_md(dir.path())))
}

/// Lines of `text` that are exactly a begin marker.
fn begin_lines(text: &str) -> usize {
    text.lines().filter(|l| l.trim() == BLOCK_BEGIN).count()
}

fn gemini_table(dir: &Path, name: &str) -> toml::Table {
    let text = read(&dir.join(format!(".gemini/commands/{name}.toml")));
    toml::from_str(&text).expect("valid TOML")
}

// ------------------------------------------------------------ per target

#[test]
fn install_creates_skill_files_for_claude() {
    assert_eq!(installed("claude", &[".claude/skills"]), ok_and_complete());
}

#[test]
fn install_creates_skill_files_for_codex() {
    assert_eq!(installed("codex", &[".agents/skills"]), ok_and_complete());
}

#[test]
fn install_creates_skill_files_for_cursor() {
    assert_eq!(installed("cursor", &[".cursor/skills"]), ok_and_complete());
}

#[test]
fn install_creates_skill_files_for_copilot() {
    assert_eq!(installed("copilot", &[".github/skills"]), ok_and_complete());
}

#[test]
fn install_creates_skill_files_for_gemini() {
    assert_eq!(installed("gemini", &[".agents/skills"]), ok_and_complete());
}

#[test]
fn install_creates_skill_files_for_agents_md() {
    assert_eq!(
        installed("agents-md", &[".agents/skills"]),
        ok_and_complete()
    );
}

#[test]
fn install_creates_skill_files_for_all() {
    assert_eq!(
        installed("all", &[".claude/skills", ".agents/skills"]),
        ok_and_complete()
    );
}

#[test]
fn install_creates_skill_files_for_copilot_user_scope() {
    let home = TempDir::new().unwrap();
    let code = run(
        home.path(),
        &["skills", "install", "--target", "copilot", "--user"],
    )
    .status
    .code();
    assert_eq!(
        (
            code,
            missing_skill_files(&home.path().join(".copilot/skills"))
        ),
        ok_and_complete()
    );
}

#[test]
fn install_creates_agents_md_block_for_agents_md() {
    let dir = TempDir::new().unwrap();
    let code = install(dir.path(), "agents-md", &[]).status.code();
    assert_eq!(
        (code, begin_lines(&read(&agents_md(dir.path())))),
        (Some(0), 1)
    );
}

#[test]
fn install_creates_one_command_per_cpm_skill_for_gemini() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    let missing: Vec<&str> = ["cpm-ev", "cpm-improve", "cpm-plan", "cpm-revise", "cpm-run"]
        .into_iter()
        .filter(|n| {
            !dir.path()
                .join(format!(".gemini/commands/{n}.toml"))
                .is_file()
        })
        .collect();
    assert_eq!(missing, Vec::<&str>::new());
}

#[test]
fn gemini_command_parses_with_the_skill_description() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    let table = gemini_table(dir.path(), "cpm-plan");
    assert_eq!(
        table.get("description").and_then(|v| v.as_str()),
        skill_description("cpm-plan")
    );
}

#[test]
fn gemini_command_prompt_points_at_the_installed_skill() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    let table = gemini_table(dir.path(), "cpm-plan");
    let prompt = table.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        prompt.contains("read the file .agents/skills/cpm-plan/SKILL.md")
            && prompt.contains("{{args}}")
    );
}

#[test]
fn gemini_user_command_parses_with_a_spaced_home() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home \"with\" spaces");
    std::fs::create_dir(&home).unwrap();
    run(
        &home,
        &["skills", "install", "--target", "gemini", "--user"],
    );
    let table = gemini_table(&home, "cpm-run");
    let skill = home.join(".agents/skills/cpm-run/SKILL.md");
    let prompt = table.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    assert!(prompt.contains(&skill.display().to_string()));
}

#[test]
fn install_for_all_writes_no_cursor_or_copilot_copies() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "all", &[]);
    assert!(!dir.path().join(".cursor").exists() && !dir.path().join(".github").exists());
}

#[test]
fn install_for_all_warns_cursor_and_copilot_may_list_twice() {
    let dir = TempDir::new().unwrap();
    let out = stdout(&install(dir.path(), "all", &[]));
    assert!(out.contains("may list each skill twice"));
}

#[test]
fn install_writes_a_manifest_with_every_file_hash() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let manifest: serde_json::Value = serde_json::from_str(&read(
        &dir.path().join(".claude/skills").join(MANIFEST_FILE),
    ))
    .unwrap();
    let expected: serde_json::Map<String, serde_json::Value> = embedded_skills()
        .iter()
        .map(|(p, c)| ((*p).to_string(), sha256_hex(c.as_bytes()).into()))
        .collect();
    assert_eq!(manifest["files"], serde_json::Value::Object(expected));
}

#[test]
fn install_records_the_target_in_the_manifest() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    install(dir.path(), "gemini", &[]);
    let manifest: serde_json::Value = serde_json::from_str(&read(
        &dir.path().join(".agents/skills").join(MANIFEST_FILE),
    ))
    .unwrap();
    assert_eq!(manifest["targets"], serde_json::json!(["codex", "gemini"]));
}

#[test]
fn install_leaves_no_lock_file() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    assert!(!dir.path().join(".claude/skills").join(LOCK_FILE).exists());
}

#[test]
fn install_writes_no_mcp_config() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "all", &[]);
    let written: Vec<_> = [
        ".mcp.json",
        ".cursor/mcp.json",
        ".vscode/mcp.json",
        ".codex",
        ".gemini",
    ]
    .into_iter()
    .filter(|p| dir.path().join(p).exists())
    .collect();
    assert_eq!(written, Vec::<&str>::new());
}

#[test]
fn install_prints_the_mcp_registration_command() {
    let dir = TempDir::new().unwrap();
    let out = stdout(&install(dir.path(), "claude", &[]));
    assert!(out.contains(
        "claude mcp add --transport stdio --scope project cpm-planner -- npx -y @matthew-cochran/cpm"
    ));
}

#[test]
fn codex_project_install_prints_the_config_toml_snippet() {
    let dir = TempDir::new().unwrap();
    let out = stdout(&install(dir.path(), "codex", &[]));
    assert!(out.contains("[mcp_servers.cpm-planner]\n  command = \"npx\"\n  args = [\"-y\", \"@matthew-cochran/cpm\"]"));
}

#[test]
fn gemini_install_prints_npx_and_binary_commands() {
    let dir = TempDir::new().unwrap();
    let out = stdout(&install(dir.path(), "gemini", &[]));
    let exe = env!("CARGO_BIN_EXE_cpm-planner");
    assert!(
        out.contains("gemini mcp add -s project cpm-planner npx -y @matthew-cochran/cpm")
            && out.contains(&format!("gemini mcp add -s project cpm-planner {exe}"))
    );
}

// ------------------------------------------------------------ re-runs and edits

#[test]
fn second_install_reports_all_unchanged() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    let out = stdout(&install(dir.path(), "gemini", &[]));
    let statuses: BTreeSet<&str> = file_lines(&out)
        .into_iter()
        .filter_map(|l| l.split("  ").next())
        .collect();
    assert_eq!(statuses, BTreeSet::from(["unchanged"]));
}

#[test]
fn user_edited_file_is_kept_and_reported_modified() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let skill = claude_plan_skill(dir.path());
    std::fs::write(&skill, "my edit\n").unwrap();
    let output = install(dir.path(), "claude", &[]);
    let reported = stdout(&output).contains(&format!("modified, skipped  {}", skill.display()));
    assert_eq!(
        (output.status.code(), read(&skill), reported),
        (Some(0), "my edit\n".to_string(), true)
    );
}

#[test]
fn force_overwrites_modified_file() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let skill = claude_plan_skill(dir.path());
    std::fs::write(&skill, "my edit\n").unwrap();
    install(dir.path(), "claude", &["--force"]);
    assert_eq!(read(&skill), embedded("cpm-plan/SKILL.md"));
}

#[test]
fn force_with_a_value_is_usage_error() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        install(dir.path(), "claude", &["--force=yes"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn dry_run_with_a_value_is_usage_error() {
    let dir = TempDir::new().unwrap();
    assert_eq!(
        install(dir.path(), "claude", &["--dry-run=no"])
            .status
            .code(),
        Some(2)
    );
}

#[test]
fn foreign_file_is_skipped() {
    let dir = TempDir::new().unwrap();
    let skill = claude_plan_skill(dir.path());
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::fs::write(&skill, "mine\n").unwrap();
    let out = stdout(&install(dir.path(), "claude", &[]));
    let reported = out.contains(&format!("foreign, skipped  {}", skill.display()));
    assert_eq!((read(&skill), reported), ("mine\n".to_string(), true));
}

#[test]
fn dry_run_writes_nothing() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "agents-md", &["--dry-run"]);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[test]
fn install_handles_home_with_spaces() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path().join("home with spaces");
    std::fs::create_dir(&home).unwrap();
    run(
        &home,
        &["skills", "install", "--target", "claude", "--user"],
    );
    assert!(home.join(".claude/skills/cpm-plan/SKILL.md").is_file());
}

#[test]
fn install_handles_project_dir_with_spaces() {
    let tmp = TempDir::new().unwrap();
    let dir = tmp.path().join("my project");
    std::fs::create_dir(&dir).unwrap();
    install(&dir, "cursor", &[]);
    assert!(dir.join(".cursor/skills/cpm-plan/SKILL.md").is_file());
}

#[test]
fn upgrade_updates_files_matching_old_manifest() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let skill = claude_plan_skill(dir.path());
    let old = "cpm-plan as cpm-planner 0.0.9 wrote it\n";
    std::fs::write(&skill, old).unwrap();
    edit_manifest(&dir.path().join(".claude/skills"), |m| {
        m["cpm_planner_version"] = "0.0.9".into();
        m["files"]["cpm-plan/SKILL.md"] = sha256_hex(old.as_bytes()).into();
    });
    install(dir.path(), "claude", &[]);
    assert_eq!(read(&skill), embedded("cpm-plan/SKILL.md"));
}

#[test]
fn upgrade_removes_files_the_new_version_no_longer_ships() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let retired = dir.path().join(".claude/skills/cpm-retired/SKILL.md");
    std::fs::create_dir_all(retired.parent().unwrap()).unwrap();
    std::fs::write(&retired, "retired\n").unwrap();
    edit_manifest(&dir.path().join(".claude/skills"), |m| {
        m["files"]["cpm-retired/SKILL.md"] = sha256_hex(b"retired\n").into();
    });
    install(dir.path(), "claude", &[]);
    assert!(!retired.exists());
}

// ------------------------------------------------------------ manifest safety

#[test]
fn manifest_with_parent_component_is_refused_and_deletes_nothing() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let victim = dir.path().join("victim.conf");
    std::fs::write(&victim, "precious\n").unwrap();
    edit_manifest(&dir.path().join(".claude/skills"), |m| {
        m["files"]["../../victim.conf"] = sha256_hex(b"precious\n").into();
    });
    let code = uninstall(dir.path(), "claude", &[]).status.code();
    assert_eq!(
        (
            code,
            victim.exists(),
            claude_plan_skill(dir.path()).exists()
        ),
        (Some(1), true, true)
    );
}

#[test]
fn manifest_with_backslash_key_is_refused() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    edit_manifest(&dir.path().join(".claude/skills"), |m| {
        m["files"]["cpm-plan\\..\\..\\victim.conf"] = sha256_hex(b"x").into();
    });
    assert_eq!(install(dir.path(), "claude", &[]).status.code(), Some(1));
}

#[cfg(unix)]
#[test]
fn symlink_out_of_the_target_root_is_refused() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path().join(".claude/skills")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".claude/skills/cpm-plan")).unwrap();
    let output = install(dir.path(), "claude", &[]);
    assert_eq!(
        (
            output.status.code(),
            std::fs::read_dir(outside.path()).unwrap().count()
        ),
        (Some(1), 0)
    );
}

#[cfg(unix)]
#[test]
fn symlinked_skill_file_is_refused_and_its_target_untouched() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("elsewhere.md");
    std::fs::write(&target, "outside\n").unwrap();
    let skill = claude_plan_skill(dir.path());
    std::fs::create_dir_all(skill.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, &skill).unwrap();
    let code = install(dir.path(), "claude", &["--force"]).status.code();
    assert_eq!((code, read(&target)), (Some(1), "outside\n".to_string()));
}

#[test]
fn held_lock_makes_install_exit_1() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join(".claude/skills");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(LOCK_FILE), "12345\n").unwrap();
    assert_eq!(install(dir.path(), "claude", &[]).status.code(), Some(1));
}

// ------------------------------------------------------------ AGENTS.md

#[test]
fn agents_md_content_outside_block_is_preserved() {
    let dir = TempDir::new().unwrap();
    let prefix = "# My agents\r\nkeep   this  \n\n";
    let suffix = "\n\n## Later notes\n\ttabbed, no final newline";
    agents_md_with_old_block(dir.path(), prefix, suffix);
    install(dir.path(), "agents-md", &[]);
    let text = read(&agents_md(dir.path()));
    let start = text.find(BLOCK_BEGIN).unwrap();
    let end = text.find(BLOCK_END).unwrap() + BLOCK_END.len();
    assert_eq!((&text[..start], &text[end..]), (prefix, suffix));
}

#[test]
fn agents_md_block_is_replaced_not_duplicated() {
    let dir = TempDir::new().unwrap();
    agents_md_with_old_block(dir.path(), "# Mine\n\n", "\n");
    install(dir.path(), "agents-md", &[]);
    let text = read(&agents_md(dir.path()));
    assert_eq!(
        (begin_lines(&text), text.contains("old cpm-planner notes")),
        (1, false)
    );
}

#[test]
fn agents_md_without_block_keeps_user_content_first() {
    let (_, text) = install_over_agents_md("# Mine\nno newline", &[]);
    assert!(text.starts_with(&format!("# Mine\nno newline\n\n{BLOCK_BEGIN}\n")));
}

#[test]
fn agents_md_nested_begin_is_refused_unchanged() {
    let text = format!("a\n{BLOCK_BEGIN}\n{BLOCK_BEGIN}\nx\n{BLOCK_END}\n");
    assert_eq!(install_over_agents_md(&text, &[]), (Some(1), text));
}

#[test]
fn agents_md_duplicate_block_is_refused_unchanged() {
    let text = format!("{BLOCK_BEGIN}\nx\n{BLOCK_END}\n{BLOCK_BEGIN}\ny\n{BLOCK_END}\n");
    assert_eq!(install_over_agents_md(&text, &[]), (Some(1), text));
}

#[test]
fn agents_md_stray_end_is_refused_unchanged() {
    let text = format!("notes\n{BLOCK_END}\n");
    assert_eq!(install_over_agents_md(&text, &[]), (Some(1), text));
}

#[test]
fn agents_md_begin_without_end_is_refused_unchanged() {
    let text = format!("notes\n{BLOCK_BEGIN}\nrest\n");
    assert_eq!(install_over_agents_md(&text, &[]), (Some(1), text));
}

#[test]
fn agents_md_refusal_creates_no_skills_dir() {
    let dir = TempDir::new().unwrap();
    std::fs::write(agents_md(dir.path()), format!("{BLOCK_END}\n")).unwrap();
    install(dir.path(), "agents-md", &[]);
    assert!(!dir.path().join(".agents").exists());
}

#[test]
fn agents_md_markers_in_prose_are_not_matched() {
    let original = format!("Wrap notes in `{BLOCK_BEGIN}` and `{BLOCK_END}` lines.\n");
    let (code, text) = install_over_agents_md(&original, &[]);
    assert_eq!(
        (code, text.starts_with(&original), begin_lines(&text)),
        (Some(0), true, 1)
    );
}

#[test]
fn agents_md_force_does_not_corrupt_nested_markers() {
    let text = format!("{BLOCK_BEGIN}\n{BLOCK_BEGIN}\n{BLOCK_END}\n");
    assert_eq!(install_over_agents_md(&text, &["--force"]), (Some(1), text));
}

#[test]
fn agents_md_user_edited_block_is_kept() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "agents-md", &[]);
    let edited = format!("# Mine\n\n{BLOCK_BEGIN}\nmy own words\n{BLOCK_END}\n");
    std::fs::write(agents_md(dir.path()), &edited).unwrap();
    install(dir.path(), "agents-md", &[]);
    assert_eq!(read(&agents_md(dir.path())), edited);
}

#[test]
fn agents_md_force_replaces_an_edited_block() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "agents-md", &[]);
    std::fs::write(
        agents_md(dir.path()),
        format!("# Mine\n\n{BLOCK_BEGIN}\nmy own words\n{BLOCK_END}\n"),
    )
    .unwrap();
    install(dir.path(), "agents-md", &["--force"]);
    let text = read(&agents_md(dir.path()));
    assert_eq!(
        (
            begin_lines(&text),
            text.contains("my own words"),
            text.starts_with("# Mine\n\n")
        ),
        (1, false, true)
    );
}

#[test]
fn agents_md_crlf_file_gets_a_crlf_block() {
    let (_, text) = install_over_agents_md("# Mine\r\nline\r\n", &[]);
    assert!(!text.replace("\r\n", "").contains('\n'));
}

#[test]
fn agents_md_install_then_uninstall_is_byte_identical() {
    let originals = [
        "# Mine\n",
        "# Mine",
        "# Mine\r\nmore\r\n",
        "# Mine\r\nno final newline",
        "",
        "a\n\n",
        "mixed\r\nends lf\n",
    ];
    let changed: Vec<&str> = originals
        .into_iter()
        .filter(|original| {
            let dir = TempDir::new().unwrap();
            std::fs::write(agents_md(dir.path()), original).unwrap();
            install(dir.path(), "agents-md", &[]);
            uninstall(dir.path(), "agents-md", &[]);
            std::fs::read(agents_md(dir.path())).ok() != Some(original.as_bytes().to_vec())
        })
        .collect();
    assert_eq!(changed, Vec::<&str>::new());
}

#[test]
fn agents_md_created_by_install_is_deleted_by_uninstall() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "agents-md", &[]);
    uninstall(dir.path(), "agents-md", &[]);
    assert!(!agents_md(dir.path()).exists());
}

#[cfg(unix)]
#[test]
fn agents_md_symlink_inside_the_project_is_written_through() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("CLAUDE.md"), "# Claude\n").unwrap();
    std::os::unix::fs::symlink("CLAUDE.md", agents_md(dir.path())).unwrap();
    install(dir.path(), "agents-md", &[]);
    let still_link = std::fs::symlink_metadata(agents_md(dir.path()))
        .unwrap()
        .file_type()
        .is_symlink();
    assert_eq!(
        (
            still_link,
            begin_lines(&read(&dir.path().join("CLAUDE.md")))
        ),
        (true, 1)
    );
}

#[cfg(unix)]
#[test]
fn agents_md_symlink_outside_the_project_is_refused() {
    let dir = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let target = outside.path().join("AGENTS.md");
    std::fs::write(&target, "# Elsewhere\n").unwrap();
    std::os::unix::fs::symlink(&target, agents_md(dir.path())).unwrap();
    let code = install(dir.path(), "agents-md", &[]).status.code();
    assert_eq!(
        (code, read(&target)),
        (Some(1), "# Elsewhere\n".to_string())
    );
}

#[test]
fn agents_md_with_user_scope_is_usage_error() {
    let home = TempDir::new().unwrap();
    let output = run(
        home.path(),
        &["skills", "install", "--target", "agents-md", "--user"],
    );
    assert_eq!(output.status.code(), Some(2));
}

// ------------------------------------------------------------ uninstall

#[test]
fn uninstall_keeps_user_modified_files() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let edited = claude_plan_skill(dir.path());
    std::fs::write(&edited, "my edit\n").unwrap();
    uninstall(dir.path(), "claude", &[]);
    let untouched = dir.path().join(".claude/skills/cpm-run/SKILL.md");
    assert_eq!((edited.is_file(), untouched.exists()), (true, false));
}

#[test]
fn uninstall_keeps_foreign_files() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let mine = dir.path().join(".claude/skills/my-skill/SKILL.md");
    std::fs::create_dir_all(mine.parent().unwrap()).unwrap();
    std::fs::write(&mine, "mine\n").unwrap();
    uninstall(dir.path(), "claude", &[]);
    assert!(mine.is_file());
}

#[test]
fn uninstall_dry_run_removes_nothing() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    uninstall(dir.path(), "codex", &["--dry-run"]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".agents/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn uninstall_dry_run_prints_no_shared_root_note() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    let out = stdout(&uninstall(dir.path(), "codex", &["--dry-run"]));
    assert!(!out.contains("Note:"));
}

#[test]
fn uninstall_removes_the_agents_md_block_and_keeps_the_rest() {
    let dir = TempDir::new().unwrap();
    std::fs::write(agents_md(dir.path()), "# Mine\n").unwrap();
    install(dir.path(), "agents-md", &[]);
    uninstall(dir.path(), "agents-md", &[]);
    assert_eq!(read(&agents_md(dir.path())), "# Mine\n");
}

#[test]
fn uninstall_one_target_keeps_shared_root_for_other_target() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    install(dir.path(), "agents-md", &[]);
    let out = stdout(&uninstall(dir.path(), "codex", &[]));
    assert_eq!(
        (
            missing_skill_files(&dir.path().join(".agents/skills")),
            out.contains("kept (still used by agents-md)")
        ),
        (Vec::<String>::new(), true)
    );
}

#[test]
fn uninstall_gemini_keeps_agents_skills_for_codex_and_removes_commands() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    install(dir.path(), "gemini", &[]);
    uninstall(dir.path(), "gemini", &[]);
    assert_eq!(
        (
            missing_skill_files(&dir.path().join(".agents/skills")),
            dir.path().join(".gemini/commands").exists()
        ),
        (Vec::<String>::new(), false)
    );
}

#[test]
fn uninstall_all_keeps_separate_claude_install() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    install(dir.path(), "all", &[]);
    uninstall(dir.path(), "all", &[]);
    assert_eq!(
        (
            missing_skill_files(&dir.path().join(".claude/skills")),
            dir.path().join(".agents/skills").exists()
        ),
        (Vec::<String>::new(), false)
    );
}

#[test]
fn last_target_uninstall_removes_files() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    install(dir.path(), "gemini", &[]);
    uninstall(dir.path(), "codex", &[]);
    uninstall(dir.path(), "gemini", &[]);
    assert!(!dir.path().join(".agents").join("skills").exists());
}

#[test]
fn list_reports_an_installed_root() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let path = dir.path().to_str().unwrap();
    let out = stdout(&run(dir.path(), &["skills", "list", "--project", path]));
    assert!(out.contains(&format!(
        "{}  (project)",
        dir.path().join(".claude/skills").display()
    )));
}

// ------------------------------------------------------------ CLI

#[test]
fn unknown_target_is_usage_error() {
    let dir = TempDir::new().unwrap();
    assert_eq!(install(dir.path(), "emacs", &[]).status.code(), Some(2));
}

#[test]
fn install_without_scope_is_usage_error() {
    let home = TempDir::new().unwrap();
    let output = run(home.path(), &["skills", "install", "--target", "claude"]);
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn version_flag_prints_crate_version() {
    let home = TempDir::new().unwrap();
    let out = stdout(&run(home.path(), &["--version"]));
    assert_eq!(out, format!("cpm-planner {}\n", env!("CARGO_PKG_VERSION")));
}

#[test]
fn help_flag_lists_the_skills_command() {
    let home = TempDir::new().unwrap();
    assert!(stdout(&run(home.path(), &["--help"])).contains("cpm-planner skills install"));
}

#[test]
fn no_arguments_still_starts_the_mcp_server() {
    let home = TempDir::new().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_cpm-planner"))
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("CPM_PLANNER_DB", ":memory:")
        .env("CPM_PROJECT_ROOT", home.path())
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("server starts");
    let mut stdin = child.stdin.take().unwrap();
    writeln!(
        stdin,
        r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"skills-install-test","version":"0"}}}}}}"#
    )
    .unwrap();
    stdin.flush().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = reader.read_line(&mut line);
        let _ = tx.send(line);
    });
    let reply = rx.recv_timeout(Duration::from_secs(30)).unwrap_or_default();
    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();
    let reply: serde_json::Value = serde_json::from_str(&reply).unwrap_or_default();
    assert!(reply["result"]["serverInfo"].is_object());
}
