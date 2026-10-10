//! `cpm-planner skills install|uninstall|list`: copies the agent skills under
//! `skills/` (embedded at compile time) into the directories each agent tool
//! reads, per `docs/agents/tool-matrix.md`.
//!
//! Every skills root we write to gets a manifest, [`MANIFEST_FILE`], that
//! records the sha256 of each file as we wrote it. A later run uses it to tell
//! our files from the user's:
//!
//! - a file that is not in the manifest is someone else's ("foreign, skipped");
//! - a file whose hash no longer matches the manifest was edited by the user
//!   ("modified, skipped");
//! - a file that still matches the manifest is ours to update or remove.
//!
//! `--force` overwrites foreign and modified files. Writes are atomic (a temp
//! file in the same directory, then a rename), and a symlink that leads out of
//! the target root is refused. The installer never writes MCP client
//! configuration; it prints the registration command for the target instead.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/embedded_skills.rs"));

/// The manifest file written in each skills root.
pub const MANIFEST_FILE: &str = ".cpm-planner-skills.json";
/// The manifest schema version this binary writes and reads.
pub const MANIFEST_SCHEMA: u32 = 1;
/// The line that opens the managed block in `AGENTS.md`.
pub const BLOCK_BEGIN: &str = "<!-- cpm-planner:begin -->";
/// The line that closes the managed block in `AGENTS.md`.
pub const BLOCK_END: &str = "<!-- cpm-planner:end -->";
/// The npm launcher used in the printed MCP registration commands.
const NPX_ARGS: [&str; 2] = ["-y", "@matthew-cochran/cpm"];
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit code for a successful run (skipped files included).
pub const EXIT_OK: u8 = 0;
/// Exit code for an IO error or a refused write.
pub const EXIT_IO: u8 = 1;
/// Exit code for a usage error.
pub const EXIT_USAGE: u8 = 2;

/// Usage text for the `skills` subcommand.
pub const USAGE: &str = "\
Usage:
  cpm-planner skills install   --target <target> (--project <dir> | --user) [--dry-run] [--force]
  cpm-planner skills uninstall --target <target> (--project <dir> | --user) [--dry-run]
  cpm-planner skills list      [--project <dir> | --user]

Targets:
  claude     Claude Code      .claude/skills/          (user: ~/.claude/skills/)
  codex      OpenAI Codex     .agents/skills/          (user: ~/.agents/skills/)
  cursor     Cursor           .cursor/skills/          (user: ~/.cursor/skills/)
  copilot    GitHub Copilot   .github/skills/          (user: ~/.copilot/skills/)
  gemini     Gemini CLI       .agents/skills/ + .gemini/commands/cpm-*.toml
  agents-md  AGENTS.md block + .agents/skills/         (project only)
  all        .claude/skills/ + .agents/skills/         (read by every target above)

A file you edited, or one cpm-planner did not write, is kept and reported as
skipped; --force overwrites it. No MCP configuration is written: the command
to register the server is printed after the install.
";

/// Every file under `skills/` as (path relative to `skills/`, contents).
pub fn embedded_skills() -> &'static [(&'static str, &'static str)] {
    EMBEDDED_SKILLS
}

/// The names of the embedded skills (directories holding a `SKILL.md`), sorted.
pub fn skill_names() -> Vec<&'static str> {
    EMBEDDED_SKILLS
        .iter()
        .filter_map(|(path, _)| path.strip_suffix("/SKILL.md"))
        .filter(|name| !name.contains('/'))
        .collect()
}

/// Lower-case hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The per-root record of what cpm-planner wrote.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    /// [`MANIFEST_SCHEMA`] at write time.
    pub schema: u32,
    /// The cpm-planner version that wrote the manifest.
    pub cpm_planner_version: String,
    /// Path relative to the root (`/` separators) to the sha256 of the
    /// content cpm-planner wrote there.
    pub files: BTreeMap<String, String>,
    /// The sha256 of the managed `AGENTS.md` block, when the `agents-md`
    /// target wrote one next to this root (`<dir>/AGENTS.md` for
    /// `<dir>/.agents/skills`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_block: Option<String>,
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Created,
    Updated,
    Unchanged,
    Modified,
    Foreign,
    Removed,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Created => "created",
            Status::Updated => "updated",
            Status::Unchanged => "unchanged",
            Status::Modified => "modified, skipped",
            Status::Foreign => "foreign, skipped",
            Status::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Claude,
    Codex,
    Cursor,
    Copilot,
    Gemini,
    AgentsMd,
    All,
}

impl Target {
    fn parse(raw: &str) -> Option<Target> {
        Some(match raw {
            "claude" => Target::Claude,
            "codex" => Target::Codex,
            "cursor" => Target::Cursor,
            "copilot" => Target::Copilot,
            "gemini" => Target::Gemini,
            "agents-md" => Target::AgentsMd,
            "all" => Target::All,
            _ => return None,
        })
    }

    fn id(self) -> &'static str {
        match self {
            Target::Claude => "claude",
            Target::Codex => "codex",
            Target::Cursor => "cursor",
            Target::Copilot => "copilot",
            Target::Gemini => "gemini",
            Target::AgentsMd => "agents-md",
            Target::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
enum Scope {
    /// The user's home directory.
    User(PathBuf),
    /// A project directory.
    Project(PathBuf),
}

impl Scope {
    fn base(&self) -> &Path {
        match self {
            Scope::User(p) | Scope::Project(p) => p,
        }
    }

    fn is_user(&self) -> bool {
        matches!(self, Scope::User(_))
    }

    fn label(&self) -> &'static str {
        if self.is_user() { "user" } else { "project" }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RootKind {
    /// `<root>/<skill>/...`, every embedded file.
    Skills,
    /// `<root>/cpm-<x>.toml`, one Gemini CLI command per `cpm-*` skill.
    GeminiCommands,
}

#[derive(Debug, Clone)]
struct Root {
    dir: PathBuf,
    kind: RootKind,
}

/// A usage error (exit 2) or an IO error (exit 1).
#[derive(Debug)]
enum CliError {
    Usage(String),
    Io(String),
}

impl From<io::Error> for CliError {
    fn from(err: io::Error) -> Self {
        CliError::Io(err.to_string())
    }
}

fn io_err(context: impl std::fmt::Display, err: io::Error) -> CliError {
    CliError::Io(format!("{context}: {err}"))
}

/// The user's home: `HOME`, then (on Windows) `USERPROFILE`.
fn home_dir() -> Result<PathBuf, CliError> {
    let from = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let home = from("HOME");
    #[cfg(windows)]
    let home = home.or_else(|| from("USERPROFILE"));
    home.ok_or_else(|| CliError::Io("cannot find your home directory: HOME is not set".into()))
}

fn roots_for(target: Target, scope: &Scope) -> Vec<Root> {
    let base = scope.base();
    let skills = |rel: &str| Root {
        dir: base.join(rel).join("skills"),
        kind: RootKind::Skills,
    };
    match target {
        Target::Claude => vec![skills(".claude")],
        Target::Codex | Target::AgentsMd => vec![skills(".agents")],
        Target::Cursor => vec![skills(".cursor")],
        Target::Copilot => vec![skills(if scope.is_user() {
            ".copilot"
        } else {
            ".github"
        })],
        Target::Gemini => vec![
            skills(".agents"),
            Root {
                dir: base.join(".gemini").join("commands"),
                kind: RootKind::GeminiCommands,
            },
        ],
        Target::All => vec![skills(".claude"), skills(".agents")],
    }
}

/// Every root any target can write for `scope`, for `list`.
fn all_roots(scope: &Scope) -> Vec<Root> {
    let mut roots: Vec<Root> = Vec::new();
    for target in [
        Target::Claude,
        Target::Codex,
        Target::Cursor,
        Target::Copilot,
        Target::Gemini,
    ] {
        for root in roots_for(target, scope) {
            if !roots.iter().any(|r| r.dir == root.dir) {
                roots.push(root);
            }
        }
    }
    roots
}

/// The front-matter `description` of an embedded SKILL.md.
fn skill_description(name: &str) -> String {
    let path = format!("{name}/SKILL.md");
    let text = EMBEDDED_SKILLS
        .iter()
        .find(|(p, _)| *p == path)
        .map(|(_, t)| *t)
        .unwrap_or("");
    let mut lines = text.lines();
    if lines.next().map(str::trim) == Some("---") {
        for line in lines {
            if line.trim() == "---" {
                break;
            }
            if let Some(rest) = line.strip_prefix("description:") {
                return rest.trim().trim_matches('"').to_string();
            }
        }
    }
    format!("Run the {name} cpm-planner skill")
}

/// Escape for a TOML basic (or multi-line basic) string.
fn toml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\t' => out.push(ch),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// A Gemini CLI command that points Gemini at the installed skill. The skill
/// stays the single source of instructions (and its relative links to
/// `deliverable-cpm` keep resolving), so the command never goes stale.
fn gemini_command(name: &str, scope: &Scope) -> String {
    let skill_path = match scope {
        // Relative to the workspace root, so the repo can move or be cloned.
        Scope::Project(_) => format!(".agents/skills/{name}/SKILL.md"),
        Scope::User(home) => home
            .join(".agents")
            .join("skills")
            .join(name)
            .join("SKILL.md")
            .display()
            .to_string(),
    };
    let prompt = format!(
        "Follow the `{name}` agent skill for this request. Activate it with the activate_skill \
         tool if it is available; otherwise read the file {skill_path} and follow its \
         instructions exactly, resolving its relative links from that file's directory. It \
         uses the cpm-planner MCP server's plan.* tools.\n\nRequest: {{{{args}}}}\n"
    );
    format!(
        "# Generated by cpm-planner skills install --target gemini. An edited copy is kept and no longer updated.\n\
         description = \"{}\"\nprompt = \"\"\"\n{}\"\"\"\n",
        toml_escape(&skill_description(name)),
        toml_escape(&prompt)
    )
}

/// The files to write under `root`, as (relative path, content).
fn planned_files(root: &Root, scope: &Scope) -> Vec<(String, String)> {
    match root.kind {
        RootKind::Skills => EMBEDDED_SKILLS
            .iter()
            .map(|(p, c)| ((*p).to_string(), (*c).to_string()))
            .collect(),
        RootKind::GeminiCommands => skill_names()
            .into_iter()
            .filter(|n| n.starts_with("cpm-"))
            .map(|n| (format!("{n}.toml"), gemini_command(n, scope)))
            .collect(),
    }
}

/// The managed `AGENTS.md` block, markers included, no trailing newline.
fn agents_md_block() -> String {
    let mut block = String::new();
    block.push_str(BLOCK_BEGIN);
    block.push_str("\n## cpm-planner\n\n");
    block.push_str(
        "This project plans and tracks work with the cpm-planner MCP server (`plan.*` tools: \
         critical path, levelling, leased execution, earned value). Register it with your agent \
         as `npx -y @matthew-cochran/cpm`, or as the `cpm-planner` binary.\n\n",
    );
    block.push_str("Agent skills for it live in `.agents/skills/`:\n\n");
    for name in skill_names() {
        let _ = writeln!(block, "- `{name}`: `.agents/skills/{name}/SKILL.md`");
    }
    block.push_str(
        "\nInvoke a skill as `/cpm-plan` (Cursor, Copilot, Zed) or `$cpm-plan` (Codex), or let \
         the agent load it from its description. `deliverable-cpm` is the shared method the \
         `cpm-*` skills link to.\n\n",
    );
    block.push_str(
        "This block is managed by `cpm-planner skills install --target agents-md`; keep your \
         own notes outside the markers.\n",
    );
    block.push_str(BLOCK_END);
    block
}

/// Byte range of the first managed block in `text` (markers included).
fn find_block(text: &str) -> Result<Option<(usize, usize)>, CliError> {
    let Some(start) = text.find(BLOCK_BEGIN) else {
        return Ok(None);
    };
    match text[start..].find(BLOCK_END) {
        Some(off) => Ok(Some((start, start + off + BLOCK_END.len()))),
        None => Err(CliError::Io(format!(
            "AGENTS.md has `{BLOCK_BEGIN}` without `{BLOCK_END}`; fix it by hand"
        ))),
    }
}

/// Refuse when `path` (under `root`) is a symlink, or any directory between
/// `root` and `path` is a symlink that resolves outside `root`.
fn check_inside(root: &Path, path: &Path) -> Result<(), CliError> {
    let rel = path.strip_prefix(root).map_err(|_| {
        CliError::Io(format!(
            "{} is not under {}",
            path.display(),
            root.display()
        ))
    })?;
    let canonical_root = match fs::canonicalize(root) {
        Ok(p) => Some(p),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(io_err(root.display(), err)),
    };
    let mut current = root.to_path_buf();
    let count = rel.components().count();
    for (i, component) in rel.components().enumerate() {
        current.push(component);
        let meta = match fs::symlink_metadata(&current) {
            Ok(m) => m,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(err) => return Err(io_err(current.display(), err)),
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        let is_file = i + 1 == count;
        let resolved = fs::canonicalize(&current).ok();
        let inside = match (&resolved, &canonical_root) {
            (Some(r), Some(root)) => r.starts_with(root),
            _ => false,
        };
        if is_file || !inside {
            return Err(CliError::Io(format!(
                "refusing to follow the symlink {} out of {}",
                current.display(),
                root.display()
            )));
        }
    }
    Ok(())
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, CliError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_err(path.display(), err)),
    }
}

/// Write `bytes` to `path` via a temp file in the same directory and a rename.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), CliError> {
    let dir = path
        .parent()
        .ok_or_else(|| CliError::Io(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(dir).map_err(|e| io_err(dir.display(), e))?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.cpm-planner-tmp-{}", std::process::id()));
    let _ = fs::remove_file(&tmp);
    let result = (|| -> io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)
    })();
    if let Err(err) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(path.display(), err));
    }
    Ok(())
}

fn remove_file(path: &Path) -> Result<(), CliError> {
    fs::remove_file(path).map_err(|e| io_err(path.display(), e))
}

/// Remove now-empty directories from `path`'s parent up to (not including) `root`.
fn prune_empty_dirs(root: &Path, path: &Path) {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
}

fn load_manifest(root: &Path) -> Result<Option<Manifest>, CliError> {
    let path = root.join(MANIFEST_FILE);
    check_inside(root, &path)?;
    let Some(bytes) = read_existing(&path)? else {
        return Ok(None);
    };
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|e| {
        CliError::Io(format!(
            "{} is not a valid manifest ({e}); remove it to start over",
            path.display()
        ))
    })?;
    if manifest.schema > MANIFEST_SCHEMA {
        return Err(CliError::Io(format!(
            "{} was written by cpm-planner {} (manifest schema {}); upgrade cpm-planner",
            path.display(),
            manifest.cpm_planner_version,
            manifest.schema
        )));
    }
    Ok(Some(manifest))
}

fn save_manifest(root: &Path, manifest: &Manifest) -> Result<(), CliError> {
    let path = root.join(MANIFEST_FILE);
    check_inside(root, &path)?;
    if manifest.files.is_empty() && manifest.agents_md_block.is_none() {
        if path.exists() {
            remove_file(&path)?;
        }
        return Ok(());
    }
    let mut json = serde_json::to_string_pretty(manifest)
        .map_err(|e| CliError::Io(format!("cannot encode the manifest: {e}")))?;
    json.push('\n');
    write_atomic(&path, json.as_bytes())
}

fn fresh_manifest(old: Option<&Manifest>) -> Manifest {
    Manifest {
        schema: MANIFEST_SCHEMA,
        cpm_planner_version: VERSION.to_string(),
        files: BTreeMap::new(),
        agents_md_block: old.and_then(|m| m.agents_md_block.clone()),
    }
}

/// Collects per-file lines and counts, then prints the summary.
#[derive(Default)]
struct Report {
    lines: Vec<String>,
    counts: BTreeMap<&'static str, usize>,
}

impl Report {
    fn record(&mut self, status: Status, path: &Path) {
        self.lines
            .push(format!("{}  {}", status.label(), path.display()));
        *self.counts.entry(status.label()).or_default() += 1;
    }

    fn summary(&self) -> String {
        [
            Status::Created,
            Status::Updated,
            Status::Unchanged,
            Status::Modified,
            Status::Foreign,
            Status::Removed,
        ]
        .iter()
        .map(|s| {
            format!(
                "{} {}",
                self.counts.get(s.label()).copied().unwrap_or(0),
                s.label().replace(", skipped", " (skipped)")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
    }
}

struct Options {
    target: Option<Target>,
    scope: Option<Scope>,
    dry_run: bool,
    force: bool,
}

fn parse_options(args: &[String], allow: &[&str]) -> Result<Options, CliError> {
    let mut opts = Options {
        target: None,
        scope: None,
        dry_run: false,
        force: false,
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f, Some(v.to_string())),
            _ => (arg.as_str(), None),
        };
        if !allow.contains(&flag) {
            return Err(CliError::Usage(format!("unexpected argument `{arg}`")));
        }
        let mut value = |name: &str| -> Result<String, CliError> {
            match inline.clone().or_else(|| iter.next().cloned()) {
                Some(v) if !v.is_empty() => Ok(v),
                _ => Err(CliError::Usage(format!("{name} needs a value"))),
            }
        };
        let set_scope = |opts: &mut Options, scope: Scope| -> Result<(), CliError> {
            if opts.scope.replace(scope).is_some() {
                return Err(CliError::Usage(
                    "give one of --project <dir> or --user".into(),
                ));
            }
            Ok(())
        };
        match flag {
            "--target" => {
                let raw = value("--target")?;
                opts.target = Some(Target::parse(&raw).ok_or_else(|| {
                    CliError::Usage(format!(
                        "unknown target `{raw}` (claude, codex, cursor, copilot, gemini, agents-md, all)"
                    ))
                })?);
            }
            "--project" => {
                let raw = value("--project")?;
                let dir = std::path::absolute(&raw)
                    .map_err(|e| CliError::Usage(format!("--project {raw}: {e}")))?;
                if !dir.is_dir() {
                    return Err(CliError::Usage(format!(
                        "--project {raw}: not an existing directory"
                    )));
                }
                set_scope(&mut opts, Scope::Project(dir))?;
            }
            "--user" => {
                if inline.is_some() {
                    return Err(CliError::Usage("--user takes no value".into()));
                }
                set_scope(&mut opts, Scope::User(PathBuf::new()))?;
            }
            "--dry-run" => opts.dry_run = true,
            "--force" => opts.force = true,
            _ => unreachable!("flag is in the allow list"),
        }
    }
    if let Some(Scope::User(_)) = opts.scope {
        opts.scope = Some(Scope::User(home_dir()?));
    }
    Ok(opts)
}

fn require(opts: &Options) -> Result<(Target, Scope), CliError> {
    let target = opts
        .target
        .ok_or_else(|| CliError::Usage("--target <target> is required".into()))?;
    let scope = opts
        .scope
        .clone()
        .ok_or_else(|| CliError::Usage("give one of --project <dir> or --user".into()))?;
    if target == Target::AgentsMd && scope.is_user() {
        return Err(CliError::Usage(
            "--target agents-md is project only (there is no user-level AGENTS.md); use --project <dir>"
                .into(),
        ));
    }
    Ok((target, scope))
}

/// Run `cpm-planner skills <args>`; returns the process exit code.
pub fn run(args: &[String]) -> u8 {
    let mut stdout = io::stdout().lock();
    let result = match args.first().map(String::as_str) {
        Some("install") => parse_options(
            &args[1..],
            &["--target", "--project", "--user", "--dry-run", "--force"],
        )
        .and_then(|o| install(&o, &mut stdout)),
        Some("uninstall") => parse_options(
            &args[1..],
            &["--target", "--project", "--user", "--dry-run"],
        )
        .and_then(|o| uninstall(&o, &mut stdout)),
        Some("list") => {
            parse_options(&args[1..], &["--project", "--user"]).and_then(|o| list(&o, &mut stdout))
        }
        Some("--help" | "-h" | "help") => {
            let _ = stdout.write_all(USAGE.as_bytes());
            Ok(())
        }
        Some(other) => Err(CliError::Usage(format!("unknown skills command `{other}`"))),
        None => Err(CliError::Usage("missing skills command".into())),
    };
    let _ = stdout.flush();
    match result {
        Ok(()) => EXIT_OK,
        Err(CliError::Usage(msg)) => {
            eprintln!("cpm-planner skills: {msg}\n\n{USAGE}");
            EXIT_USAGE
        }
        Err(CliError::Io(msg)) => {
            eprintln!("cpm-planner skills: error: {msg}");
            EXIT_IO
        }
    }
}

fn out_line(out: &mut dyn io::Write, line: &str) -> Result<(), CliError> {
    writeln!(out, "{line}").map_err(|e| io_err("stdout", e))
}

/// Decide what to do with one file and do it (unless `dry_run`). Returns the
/// status and the hash to record in the new manifest, if any.
fn sync_file(
    path: &Path,
    content: &[u8],
    recorded: Option<&String>,
    force: bool,
    dry_run: bool,
) -> Result<(Status, Option<String>), CliError> {
    let new_hash = sha256_hex(content);
    let Some(current) = read_existing(path)? else {
        if !dry_run {
            write_atomic(path, content)?;
        }
        return Ok((Status::Created, Some(new_hash)));
    };
    if current == content {
        return Ok((Status::Unchanged, Some(new_hash)));
    }
    let current_hash = sha256_hex(&current);
    let status = if force || recorded == Some(&current_hash) {
        Status::Updated
    } else if recorded.is_some() {
        return Ok((Status::Modified, recorded.cloned()));
    } else {
        return Ok((Status::Foreign, None));
    };
    if !dry_run {
        write_atomic(path, content)?;
    }
    Ok((status, Some(new_hash)))
}

fn install_root(
    root: &Root,
    scope: &Scope,
    opts: &Options,
    report: &mut Report,
) -> Result<(), CliError> {
    let old = load_manifest(&root.dir)?;
    let mut manifest = fresh_manifest(old.as_ref());
    let planned = planned_files(root, scope);
    for (rel, content) in &planned {
        let path = join_rel(&root.dir, rel);
        check_inside(&root.dir, &path)?;
        let recorded = old.as_ref().and_then(|m| m.files.get(rel));
        let (status, hash) = sync_file(
            &path,
            content.as_bytes(),
            recorded,
            opts.force,
            opts.dry_run,
        )?;
        report.record(status, &path);
        if let Some(hash) = hash {
            manifest.files.insert(rel.clone(), hash);
        }
    }
    // Files an older cpm-planner wrote that this one no longer ships.
    if let Some(old) = &old {
        for (rel, recorded) in &old.files {
            if planned.iter().any(|(p, _)| p == rel) {
                continue;
            }
            let path = join_rel(&root.dir, rel);
            check_inside(&root.dir, &path)?;
            let Some(current) = read_existing(&path)? else {
                continue;
            };
            if sha256_hex(&current) == *recorded {
                if !opts.dry_run {
                    remove_file(&path)?;
                    prune_empty_dirs(&root.dir, &path);
                }
                report.record(Status::Removed, &path);
            } else {
                report.record(Status::Modified, &path);
                manifest.files.insert(rel.clone(), recorded.clone());
            }
        }
    }
    if !opts.dry_run {
        save_manifest(&root.dir, &manifest)?;
    }
    Ok(())
}

/// Insert or replace the managed block in `<dir>/AGENTS.md`.
fn install_agents_md(
    dir: &Path,
    skills_root: &Path,
    opts: &Options,
    report: &mut Report,
) -> Result<(), CliError> {
    let path = dir.join("AGENTS.md");
    check_inside(dir, &path)?;
    let block = agents_md_block();
    let block_hash = sha256_hex(block.as_bytes());
    let old = load_manifest(skills_root)?;
    let recorded = old.as_ref().and_then(|m| m.agents_md_block.as_ref());
    let existing = read_existing(&path)?
        .map(|b| {
            String::from_utf8(b)
                .map_err(|_| CliError::Io(format!("{} is not UTF-8", path.display())))
        })
        .transpose()?;

    let (status, new_text) = match &existing {
        None => (Status::Created, Some(format!("{block}\n"))),
        Some(text) => match find_block(text)? {
            None => {
                let mut next = text.clone();
                if !next.is_empty() {
                    if !next.ends_with('\n') {
                        next.push('\n');
                    }
                    next.push('\n');
                }
                next.push_str(&block);
                next.push('\n');
                (Status::Updated, Some(next))
            }
            Some((start, end)) => {
                let current = &text[start..end];
                if current == block {
                    (Status::Unchanged, None)
                } else if opts.force || recorded == Some(&sha256_hex(current.as_bytes())) {
                    let next = format!("{}{block}{}", &text[..start], &text[end..]);
                    (Status::Updated, Some(next))
                } else if recorded.is_some() {
                    (Status::Modified, None)
                } else {
                    (Status::Foreign, None)
                }
            }
        },
    };
    report.record(status, &path);
    if opts.dry_run {
        return Ok(());
    }
    if let Some(text) = new_text {
        write_atomic(&path, text.as_bytes())?;
    }
    if matches!(
        status,
        Status::Created | Status::Updated | Status::Unchanged
    ) {
        let mut manifest = load_manifest(skills_root)?.unwrap_or_else(|| fresh_manifest(None));
        manifest.agents_md_block = Some(block_hash);
        save_manifest(skills_root, &manifest)?;
    }
    Ok(())
}

fn install(opts: &Options, out: &mut dyn io::Write) -> Result<(), CliError> {
    let (target, scope) = require(opts)?;
    let mut report = Report::default();
    let roots = roots_for(target, &scope);
    for root in &roots {
        install_root(root, &scope, opts, &mut report)?;
    }
    if target == Target::AgentsMd {
        install_agents_md(scope.base(), &roots[0].dir, opts, &mut report)?;
    }
    for line in &report.lines {
        out_line(out, line)?;
    }
    let dry = if opts.dry_run {
        " (dry run: nothing written)"
    } else {
        ""
    };
    out_line(
        out,
        &format!(
            "cpm-planner {VERSION} skills install --target {} ({}): {}{dry}",
            target.id(),
            scope.label(),
            report.summary()
        ),
    )?;
    if report.counts.contains_key(Status::Modified.label())
        || report.counts.contains_key(Status::Foreign.label())
    {
        out_line(
            out,
            "Skipped files were kept as they are; rerun with --force to overwrite them.",
        )?;
    }
    if target == Target::All {
        out_line(
            out,
            "Note: --target all writes .claude/skills (Claude Code) and .agents/skills (Codex, Cursor, \
             Copilot, Gemini) only; Cursor and Copilot also read those, so they list each skill once.",
        )?;
    }
    out_line(out, "")?;
    out_line(out, &registration(target, &scope))
}

/// The MCP registration instructions for `target` (never written, only printed).
fn registration(target: Target, scope: &Scope) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cpm-planner".to_string());
    let npx = format!("npx {}", NPX_ARGS.join(" "));
    let quoted = format!("\"{exe}\"");
    let json = |key: &str, typed: bool| -> String {
        let server = |command: &str, args: &[&str]| {
            let mut s = serde_json::Map::new();
            if typed {
                s.insert("type".into(), "stdio".into());
            }
            s.insert("command".into(), command.into());
            s.insert(
                "args".into(),
                args.iter()
                    .map(|a| (*a).into())
                    .collect::<Vec<serde_json::Value>>()
                    .into(),
            );
            serde_json::json!({ key: { "cpm-planner": serde_json::Value::Object(s) } }).to_string()
        };
        format!(
            "  {}\n  or, for this binary:\n  {}",
            server("npx", &NPX_ARGS),
            server(&exe, &[])
        )
    };
    let scope_flag = if scope.is_user() { "user" } else { "project" };
    let mut text =
        String::from("Register the MCP server (cpm-planner does not edit MCP configuration):\n");
    let claude = format!(
        "  claude mcp add --transport stdio --scope {scope_flag} cpm-planner -- {npx}\n  \
         or, for this binary:\n  claude mcp add --transport stdio --scope {scope_flag} cpm-planner -- {quoted}\n"
    );
    let codex = format!(
        "  codex mcp add cpm-planner -- {npx}\n  or, for this binary:\n  codex mcp add cpm-planner -- {quoted}\n"
    );
    match target {
        Target::Claude => text.push_str(&claude),
        Target::Codex => text.push_str(&codex),
        Target::Cursor => {
            let file = if scope.is_user() {
                "~/.cursor/mcp.json"
            } else {
                ".cursor/mcp.json"
            };
            let _ = writeln!(text, "  Add to {file}:\n{}", json("mcpServers", true));
        }
        Target::Copilot => {
            if scope.is_user() {
                let arg =
                    serde_json::json!({"name": "cpm-planner", "command": "npx", "args": NPX_ARGS})
                        .to_string();
                let _ = writeln!(text, "  code --add-mcp '{arg}'");
            } else {
                let _ = writeln!(
                    text,
                    "  Add to .vscode/mcp.json:\n{}",
                    json("servers", true)
                );
            }
        }
        Target::Gemini => {
            let file = if scope.is_user() {
                "~/.gemini/settings.json"
            } else {
                ".gemini/settings.json"
            };
            let _ = writeln!(
                text,
                "  gemini mcp add -s {scope_flag} cpm-planner {quoted}\n  or add to {file}:\n{}",
                json("mcpServers", false)
            );
        }
        Target::AgentsMd | Target::All => {
            text.push_str("  Claude Code:\n");
            text.push_str(&claude);
            text.push_str("  Codex:\n");
            text.push_str(&codex);
            let _ = writeln!(
                text,
                "  Cursor (.cursor/mcp.json), Gemini (.gemini/settings.json):\n{}\n  VS Code Copilot (.vscode/mcp.json):\n{}",
                json("mcpServers", true),
                json("servers", true)
            );
        }
    }
    text.push_str("See docs/agents/tool-matrix.md for every client.");
    text
}

fn uninstall_root(root: &Root, opts: &Options, report: &mut Report) -> Result<(), CliError> {
    let Some(old) = load_manifest(&root.dir)? else {
        return Ok(());
    };
    let mut manifest = fresh_manifest(Some(&old));
    for (rel, recorded) in &old.files {
        let path = join_rel(&root.dir, rel);
        check_inside(&root.dir, &path)?;
        let Some(current) = read_existing(&path)? else {
            continue;
        };
        if sha256_hex(&current) == *recorded {
            if !opts.dry_run {
                remove_file(&path)?;
                prune_empty_dirs(&root.dir, &path);
            }
            report.record(Status::Removed, &path);
        } else {
            report.record(Status::Modified, &path);
            manifest.files.insert(rel.clone(), recorded.clone());
        }
    }
    if !opts.dry_run {
        save_manifest(&root.dir, &manifest)?;
        let _ = fs::remove_dir(&root.dir);
    }
    Ok(())
}

fn uninstall_agents_md(
    dir: &Path,
    skills_root: &Path,
    opts: &Options,
    report: &mut Report,
) -> Result<(), CliError> {
    let path = dir.join("AGENTS.md");
    check_inside(dir, &path)?;
    let Some(bytes) = read_existing(&path)? else {
        return Ok(());
    };
    let text = String::from_utf8(bytes)
        .map_err(|_| CliError::Io(format!("{} is not UTF-8", path.display())))?;
    let Some((start, end)) = find_block(&text)? else {
        return Ok(());
    };
    let manifest = load_manifest(skills_root)?;
    let recorded = manifest.as_ref().and_then(|m| m.agents_md_block.clone());
    if recorded.as_deref() != Some(sha256_hex(&text.as_bytes()[start..end]).as_str()) {
        report.record(Status::Modified, &path);
        return Ok(());
    }
    let mut before = &text[..start];
    let mut after = &text[end..];
    after = after.strip_prefix('\n').unwrap_or(after);
    if before.ends_with("\n\n") {
        before = &before[..before.len() - 1];
    }
    let rest = format!("{before}{after}");
    report.record(Status::Removed, &path);
    if opts.dry_run {
        return Ok(());
    }
    if rest.trim().is_empty() {
        remove_file(&path)?;
    } else {
        write_atomic(&path, rest.as_bytes())?;
    }
    if let Some(mut manifest) = manifest {
        manifest.agents_md_block = None;
        save_manifest(skills_root, &manifest)?;
        let _ = fs::remove_dir(skills_root);
    }
    Ok(())
}

fn uninstall(opts: &Options, out: &mut dyn io::Write) -> Result<(), CliError> {
    let (target, scope) = require(opts)?;
    let mut report = Report::default();
    let roots = roots_for(target, &scope);
    if target == Target::AgentsMd {
        uninstall_agents_md(scope.base(), &roots[0].dir, opts, &mut report)?;
    }
    for root in &roots {
        uninstall_root(root, opts, &mut report)?;
    }
    for line in &report.lines {
        out_line(out, line)?;
    }
    let dry = if opts.dry_run {
        " (dry run: nothing removed)"
    } else {
        ""
    };
    out_line(
        out,
        &format!(
            "cpm-planner {VERSION} skills uninstall --target {} ({}): {}{dry}",
            target.id(),
            scope.label(),
            report.summary()
        ),
    )?;
    if matches!(
        target,
        Target::Codex | Target::Gemini | Target::AgentsMd | Target::All
    ) {
        out_line(
            out,
            "Note: .agents/skills is shared by the codex, gemini, agents-md and all targets; it is now removed for all of them.",
        )?;
    }
    Ok(())
}

fn list(opts: &Options, out: &mut dyn io::Write) -> Result<(), CliError> {
    let scopes = match &opts.scope {
        Some(scope) => vec![scope.clone()],
        None => {
            let cwd = std::env::current_dir().map_err(|e| io_err("current directory", e))?;
            vec![Scope::User(home_dir()?), Scope::Project(cwd)]
        }
    };
    let mut found = 0usize;
    for scope in &scopes {
        for root in all_roots(scope) {
            let Some(manifest) = load_manifest(&root.dir)? else {
                continue;
            };
            let (mut modified, mut missing) = (0usize, 0usize);
            for (rel, recorded) in &manifest.files {
                match read_existing(&join_rel(&root.dir, rel))? {
                    None => missing += 1,
                    Some(bytes) if sha256_hex(&bytes) != *recorded => modified += 1,
                    Some(_) => {}
                }
            }
            if !manifest.files.is_empty() {
                found += 1;
                out_line(
                    out,
                    &format!(
                        "{}  ({}) cpm-planner {}: {} files, {modified} modified, {missing} missing",
                        root.dir.display(),
                        scope.label(),
                        manifest.cpm_planner_version,
                        manifest.files.len()
                    ),
                )?;
            }
            if manifest.agents_md_block.is_some()
                && let Some(dir) = root.dir.parent().and_then(Path::parent)
            {
                found += 1;
                out_line(
                    out,
                    &format!(
                        "{}  ({}) cpm-planner {}: managed block",
                        dir.join("AGENTS.md").display(),
                        scope.label(),
                        manifest.cpm_planner_version
                    ),
                )?;
            }
        }
    }
    let where_ = scopes
        .iter()
        .map(|s| s.base().display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    out_line(
        out,
        &format!("{found} cpm-planner skill installs found under {where_}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_table_holds_every_skill() {
        assert_eq!(
            skill_names(),
            [
                "cpm-ev",
                "cpm-improve",
                "cpm-plan",
                "cpm-revise",
                "cpm-run",
                "deliverable-cpm"
            ]
        );
    }

    #[test]
    fn gemini_command_escapes_windows_backslashes() {
        let cmd = gemini_command("cpm-plan", &Scope::User(PathBuf::from("C:\\Users\\A B")));
        assert!(cmd.contains("C:\\\\Users\\\\A B"));
    }
}
