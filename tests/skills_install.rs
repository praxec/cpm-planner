//! `cpm-planner skills install|uninstall|list`, `--version` and the no-argument
//! MCP server, driven through the real binary against temp directories.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use cpm_planner::skills::{BLOCK_BEGIN, BLOCK_END, MANIFEST_FILE, embedded_skills, sha256_hex};
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

fn install(dir: &Path, target: &str, extra: &[&str]) -> Output {
    let dir = dir.to_str().expect("utf-8 temp path");
    let mut args = vec!["skills", "install", "--target", target, "--project", dir];
    args.extend_from_slice(extra);
    run(Path::new(dir), &args)
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
    std::fs::write(
        dir.join("AGENTS.md"),
        format!("{prefix}{old_block}{suffix}"),
    )
    .expect("write");
    let hash = sha256_hex(old_block.as_bytes());
    edit_manifest(&dir.join(".agents/skills"), |m| {
        m["agents_md_block"] = hash.into();
    });
}

#[test]
fn install_creates_skill_files_for_claude() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".claude/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn install_creates_skill_files_for_codex() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "codex", &[]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".agents/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn install_creates_skill_files_for_cursor() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "cursor", &[]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".cursor/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn install_creates_skill_files_for_copilot() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "copilot", &[]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".github/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn install_creates_skill_files_for_copilot_user_scope() {
    let home = TempDir::new().unwrap();
    run(
        home.path(),
        &["skills", "install", "--target", "copilot", "--user"],
    );
    assert_eq!(
        missing_skill_files(&home.path().join(".copilot/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn install_creates_skill_files_for_gemini() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".agents/skills")),
        Vec::<String>::new()
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
fn gemini_command_points_at_the_installed_skill() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    let toml = read(&dir.path().join(".gemini/commands/cpm-plan.toml"));
    assert!(toml.contains("read the file .agents/skills/cpm-plan/SKILL.md"));
}

#[test]
fn install_creates_skill_files_for_agents_md() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "agents-md", &[]);
    assert_eq!(
        missing_skill_files(&dir.path().join(".agents/skills")),
        Vec::<String>::new()
    );
}

#[test]
fn install_creates_agents_md_block_for_agents_md() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "agents-md", &[]);
    assert!(read(&dir.path().join("AGENTS.md")).contains(BLOCK_BEGIN));
}

#[test]
fn install_creates_skill_files_for_all() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "all", &[]);
    let mut missing = missing_skill_files(&dir.path().join(".claude/skills"));
    missing.extend(missing_skill_files(&dir.path().join(".agents/skills")));
    assert_eq!(missing, Vec::<String>::new());
}

#[test]
fn install_for_all_writes_no_cursor_or_copilot_copies() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "all", &[]);
    assert!(!dir.path().join(".cursor").exists() && !dir.path().join(".github").exists());
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
fn second_install_reports_all_unchanged() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "gemini", &[]);
    let out = stdout(&install(dir.path(), "gemini", &[]));
    let statuses: std::collections::BTreeSet<&str> = file_lines(&out)
        .into_iter()
        .filter_map(|l| l.split("  ").next())
        .collect();
    assert_eq!(statuses, std::collections::BTreeSet::from(["unchanged"]));
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
fn agents_md_content_outside_block_is_preserved() {
    let dir = TempDir::new().unwrap();
    let prefix = "# My agents\r\nkeep   this  \n\n";
    let suffix = "\n\n## Later notes\n\ttabbed, no final newline";
    agents_md_with_old_block(dir.path(), prefix, suffix);
    install(dir.path(), "agents-md", &[]);
    let text = read(&dir.path().join("AGENTS.md"));
    let start = text.find(BLOCK_BEGIN).unwrap();
    let end = text.find(BLOCK_END).unwrap() + BLOCK_END.len();
    assert_eq!((&text[..start], &text[end..]), (prefix, suffix));
}

#[test]
fn agents_md_block_is_replaced_not_duplicated() {
    let dir = TempDir::new().unwrap();
    agents_md_with_old_block(dir.path(), "# Mine\n\n", "\n");
    install(dir.path(), "agents-md", &[]);
    let text = read(&dir.path().join("AGENTS.md"));
    assert_eq!(
        (
            text.matches(BLOCK_BEGIN).count(),
            text.contains("old cpm-planner notes")
        ),
        (1, false)
    );
}

#[test]
fn agents_md_without_block_keeps_user_content_first() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Mine\nno newline").unwrap();
    install(dir.path(), "agents-md", &[]);
    let text = read(&dir.path().join("AGENTS.md"));
    assert!(text.starts_with(&format!("# Mine\nno newline\n\n{BLOCK_BEGIN}\n")));
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
fn uninstall_keeps_user_modified_files() {
    let dir = TempDir::new().unwrap();
    install(dir.path(), "claude", &[]);
    let edited = claude_plan_skill(dir.path());
    std::fs::write(&edited, "my edit\n").unwrap();
    let path = dir.path().to_str().unwrap();
    run(
        dir.path(),
        &[
            "skills",
            "uninstall",
            "--target",
            "claude",
            "--project",
            path,
        ],
    );
    let untouched = dir.path().join(".claude/skills/cpm-run/SKILL.md");
    assert_eq!((edited.is_file(), untouched.exists()), (true, false));
}

#[test]
fn uninstall_removes_the_agents_md_block_and_keeps_the_rest() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Mine\n").unwrap();
    install(dir.path(), "agents-md", &[]);
    let path = dir.path().to_str().unwrap();
    run(
        dir.path(),
        &[
            "skills",
            "uninstall",
            "--target",
            "agents-md",
            "--project",
            path,
        ],
    );
    assert_eq!(read(&dir.path().join("AGENTS.md")), "# Mine\n");
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

#[test]
fn agents_md_with_user_scope_is_usage_error() {
    let home = TempDir::new().unwrap();
    let output = run(
        home.path(),
        &["skills", "install", "--target", "agents-md", "--user"],
    );
    assert_eq!(output.status.code(), Some(2));
}

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
