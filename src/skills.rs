//! `cpm-planner skills install|uninstall|list`: copies the agent skills under
//! `skills/` (embedded at compile time) into the directories each agent tool
//! reads, per `docs/agents/tool-matrix.md`.
//!
//! Every skills root we write to gets a manifest, [`MANIFEST_FILE`], that
//! records the sha256 of each file as we wrote it and the targets that
//! installed into the root. A later run uses it to tell our files from the
//! user's:
//!
//! - a file that is not in the manifest is someone else's ("foreign, skipped");
//! - a file whose hash no longer matches the manifest was edited by the user
//!   ("modified, skipped");
//! - a file that still matches the manifest is ours to update or remove.
//!
//! `--force` overwrites foreign and modified files. A root shared by several
//! targets (`.agents/skills`) keeps its files until the last of them is
//! uninstalled. Each run first validates everything (manifest paths, symlinks,
//! the `AGENTS.md` block) and only then writes, under a per-root lock file.
//! Writes are atomic (a temp file in the same directory, then a rename). The
//! installer never writes MCP client configuration; it prints the
//! registration command for the target instead.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

include!(concat!(env!("OUT_DIR"), "/embedded_skills.rs"));

/// The manifest file written in each skills root.
pub const MANIFEST_FILE: &str = ".cpm-planner-skills.json";
/// The lock file held in a skills root while a run changes it.
pub const LOCK_FILE: &str = ".cpm-planner-skills.lock";
/// The manifest schema version this binary writes and reads.
pub const MANIFEST_SCHEMA: u32 = 1;
/// The line that opens the managed block in `AGENTS.md`.
pub const BLOCK_BEGIN: &str = "<!-- cpm-planner:begin -->";
/// The line that closes the managed block in `AGENTS.md`.
pub const BLOCK_END: &str = "<!-- cpm-planner:end -->";
/// How long a run waits for another run's lock before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// The npm launcher used in the printed MCP registration commands.
const NPX_ARGS: [&str; 2] = ["-y", "@matthew-cochran/cpm"];
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Exit code for a successful run (skipped files included).
pub const EXIT_OK: u8 = 0;
/// Exit code for an IO error or a refused change.
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
  agents-md  AGENTS.md block + .agents/skills/         (project only; for user scope use codex)
  all        .claude/skills/ + .agents/skills/

A file you edited, or one cpm-planner did not write, is kept and reported as
skipped; --force overwrites it. No MCP configuration is written: the command
to register the server is printed after the install.
";

/// Every embedded file under `skills/` as (path relative to `skills/`, contents).
pub fn embedded_skills() -> &'static [(&'static str, &'static str)] {
    EMBEDDED_SKILLS
}

/// The names of the embedded skills (directories holding a `SKILL.md`), sorted.
pub fn skill_names() -> Vec<&'static str> {
    SKILL_DESCRIPTIONS.iter().map(|(name, _)| *name).collect()
}

/// The front-matter `description` of an embedded skill (validated at build time).
pub fn skill_description(name: &str) -> Option<&'static str> {
    SKILL_DESCRIPTIONS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, d)| *d)
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
    /// The targets (`claude`, `codex`, …, `all`) that installed into this root.
    /// The files stay until the last of them is uninstalled.
    #[serde(default)]
    pub targets: BTreeSet<String>,
    /// Path relative to the root (`/` separators) to the sha256 of the
    /// content cpm-planner wrote there.
    pub files: BTreeMap<String, String>,
    /// The sha256 of the managed `AGENTS.md` block, when the `agents-md`
    /// target wrote one next to this root (`<dir>/AGENTS.md` for
    /// `<dir>/.agents/skills`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_md_block: Option<String>,
    /// Whether cpm-planner created `AGENTS.md` (so uninstall may delete it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub agents_md_created: bool,
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
    const ALL: [Status; 6] = [
        Status::Created,
        Status::Updated,
        Status::Unchanged,
        Status::Modified,
        Status::Foreign,
        Status::Removed,
    ];

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

/// A usage error (exit 2) or an IO error / refusal (exit 1).
#[derive(Debug)]
enum CliError {
    Usage(String),
    Io(String),
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

fn agents_root(base: &Path) -> Root {
    Root {
        dir: base.join(".agents").join("skills"),
        kind: RootKind::Skills,
    }
}

fn roots_for(target: Target, scope: &Scope) -> Vec<Root> {
    let base = scope.base();
    let skills = |rel: &str| Root {
        dir: base.join(rel).join("skills"),
        kind: RootKind::Skills,
    };
    match target {
        Target::Claude => vec![skills(".claude")],
        Target::Codex | Target::AgentsMd => vec![agents_root(base)],
        Target::Cursor => vec![skills(".cursor")],
        Target::Copilot => vec![skills(if scope.is_user() {
            ".copilot"
        } else {
            ".github"
        })],
        Target::Gemini => vec![
            agents_root(base),
            Root {
                dir: base.join(".gemini").join("commands"),
                kind: RootKind::GeminiCommands,
            },
        ],
        Target::All => vec![skills(".claude"), agents_root(base)],
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

// ---------------------------------------------------------------- quoting

/// Escape for the inside of a TOML basic (or multi-line basic) string.
fn toml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push('\n'),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04X}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// A one-line TOML basic string, quotes included.
fn toml_string(text: &str) -> String {
    format!("\"{}\"", toml_escape(text).replace('\n', "\\n"))
}

/// Quote one shell word for the platform's shells.
fn shell_quote(word: &str) -> String {
    quote_word(word, cfg!(windows))
}

/// Quote one shell word. POSIX (`windows == false`): bare when safe, else
/// single quotes with `'\''` for an embedded quote. Windows: bare when safe,
/// else double quotes with `\"` for an embedded quote. A double-quoted path
/// is one argument in both cmd and PowerShell (Windows paths cannot contain
/// `"`); `\"` inside JSON follows the cmd / MSVC argv convention.
fn quote_word(word: &str, windows: bool) -> String {
    let safe = |b: u8| {
        // cmd splits arguments on `=` and `,`, so they are bare only on POSIX.
        b.is_ascii_alphanumeric()
            || b"/._-+:@".contains(&b)
            || (if windows {
                b == b'\\'
            } else {
                b"=,".contains(&b)
            })
    };
    if !word.is_empty() && word.bytes().all(safe) {
        word.to_string()
    } else if windows {
        format!("\"{}\"", word.replace('"', "\\\""))
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

// ---------------------------------------------------------------- rendering

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
    let description = skill_description(name)
        .map(str::to_string)
        .unwrap_or_else(|| format!("Run the {name} cpm-planner skill"));
    let prompt = format!(
        "Follow the `{name}` agent skill for this request. Activate it with the activate_skill \
         tool if it is available; otherwise read the file {skill_path} and follow its \
         instructions exactly, resolving its relative links from that file's directory. It \
         uses the cpm-planner MCP server's plan.* tools.\n\nRequest: {{{{args}}}}\n"
    );
    format!(
        "# Generated by cpm-planner skills install --target gemini. An edited copy is kept and no longer updated.\n\
         description = {}\nprompt = \"\"\"\n{}\"\"\"\n",
        toml_string(&description),
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

/// The managed `AGENTS.md` block, markers included, no trailing line ending,
/// with `eol` line endings.
fn agents_md_block(eol: &str) -> String {
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
    if eol == "\n" {
        block
    } else {
        block.replace('\n', eol)
    }
}

/// The line ending a text file uses: CRLF if it has any, else LF.
fn line_ending(text: &str) -> &'static str {
    if text.contains("\r\n") { "\r\n" } else { "\n" }
}

/// Byte range of the managed block: from the start of the begin line to the
/// end of the end line (its line terminator excluded). Markers count only as
/// whole lines (surrounding whitespace and a CR allowed).
fn find_block(text: &str, path: &Path) -> Result<Option<(usize, usize)>, CliError> {
    let refuse = |what: &str| {
        CliError::Io(format!(
            "{}: {what}; fix the cpm-planner markers by hand (nothing was changed)",
            path.display()
        ))
    };
    let mut pos = 0;
    let mut open: Option<usize> = None;
    let mut found: Option<(usize, usize)> = None;
    for line in text.split_inclusive('\n') {
        let content = line.strip_suffix('\n').unwrap_or(line);
        let content = content.strip_suffix('\r').unwrap_or(content);
        match content.trim() {
            BLOCK_BEGIN if open.is_some() => {
                return Err(refuse(&format!("nested `{BLOCK_BEGIN}`")));
            }
            BLOCK_BEGIN if found.is_some() => {
                return Err(refuse("more than one cpm-planner block"));
            }
            BLOCK_BEGIN => open = Some(pos),
            BLOCK_END => match open.take() {
                Some(start) => found = Some((start, pos + content.len())),
                None => return Err(refuse(&format!("`{BLOCK_END}` without a begin marker"))),
            },
            _ => {}
        }
        pos += line.len();
    }
    if open.is_some() {
        return Err(refuse(&format!("`{BLOCK_BEGIN}` without `{BLOCK_END}`")));
    }
    Ok(found)
}

// ---------------------------------------------------------------- filesystem

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
        if i + 1 == count {
            return Err(CliError::Io(format!(
                "{} is a symlink; refusing to write through it",
                current.display()
            )));
        }
        let inside = match (fs::canonicalize(&current).ok(), &canonical_root) {
            (Some(r), Some(root)) => r.starts_with(root),
            _ => false,
        };
        if !inside {
            return Err(CliError::Io(format!(
                "refusing to follow the symlink {} out of {}",
                current.display(),
                root.display()
            )));
        }
    }
    Ok(())
}

/// The file to read and write for `<dir>/AGENTS.md`: the path itself, or the
/// target of a symlink that resolves inside `dir`.
fn resolve_agents_md(dir: &Path) -> Result<PathBuf, CliError> {
    let path = dir.join("AGENTS.md");
    match fs::symlink_metadata(&path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(path),
        Err(err) => return Err(io_err(path.display(), err)),
        Ok(meta) if !meta.file_type().is_symlink() => return Ok(path),
        Ok(_) => {}
    }
    let resolved = fs::canonicalize(&path).map_err(|_| {
        CliError::Io(format!(
            "{} is a symlink to a missing file; refusing to write through it",
            path.display()
        ))
    })?;
    let canonical_dir = fs::canonicalize(dir).map_err(|e| io_err(dir.display(), e))?;
    if !resolved.starts_with(&canonical_dir) || !resolved.is_file() {
        return Err(CliError::Io(format!(
            "{} is a symlink to {}, which is not a file inside the project {}; refusing to write through it",
            path.display(),
            resolved.display(),
            dir.display()
        )));
    }
    Ok(resolved)
}

fn read_existing(path: &Path) -> Result<Option<Vec<u8>>, CliError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(io_err(path.display(), err)),
    }
}

fn read_text(path: &Path) -> Result<Option<String>, CliError> {
    read_existing(path)?
        .map(|b| {
            String::from_utf8(b)
                .map_err(|_| CliError::Io(format!("{} is not UTF-8", path.display())))
        })
        .transpose()
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

/// Remove now-empty directories from `path`'s parent up to (not including)
/// `root`, only while each one resolves inside `root`.
fn prune_empty_dirs(root: &Path, path: &Path) {
    let Ok(canonical_root) = fs::canonicalize(root) else {
        return;
    };
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) {
            break;
        }
        match fs::canonicalize(d) {
            Ok(cd) if cd.starts_with(&canonical_root) && cd != canonical_root => {}
            _ => break,
        }
        if fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}

fn join_rel(root: &Path, rel: &str) -> PathBuf {
    rel.split('/').fold(root.to_path_buf(), |p, c| p.join(c))
}

/// A held OS lock on `<root>/.cpm-planner-skills.lock`.
///
/// The lock is an exclusive `File::lock` (flock / LockFileEx), so the OS
/// releases it however the process ends (Ctrl-C and kill included). The file
/// itself stays on disk as an inert anchor: deleting it on release could race
/// with another process that already has it open. The only exception is a
/// root left holding nothing but the lock file (after the last uninstall, or a
/// refused install that created the directory): then the file and the empty
/// directories go too.
struct RootLock {
    file: Option<fs::File>,
    root: PathBuf,
    path: PathBuf,
    /// Directories this run created to take the lock, deepest first.
    created_dirs: Vec<PathBuf>,
    /// Remove the root when it holds only the lock file (uninstall).
    remove_if_empty: bool,
}

impl Drop for RootLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        if !self.remove_if_empty && self.created_dirs.is_empty() {
            return;
        }
        let only_lock = fs::read_dir(&self.root).is_ok_and(|entries| {
            let names: Vec<_> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name())
                .collect();
            names.len() == 1 && names[0] == LOCK_FILE
        });
        if !only_lock {
            return;
        }
        let _ = fs::remove_file(&self.path);
        if fs::remove_dir(&self.root).is_err() {
            return;
        }
        for dir in self.created_dirs.iter().filter(|d| **d != self.root) {
            if fs::remove_dir(dir).is_err() {
                break;
            }
        }
    }
}

fn open_lock_file(path: &Path) -> io::Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never follow a planted symlink.
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn lock_root(root: &Path, remove_if_empty: bool) -> Result<RootLock, CliError> {
    let mut created_dirs = Vec::new();
    let mut dir = Some(root);
    while let Some(d) = dir {
        if d.exists() {
            break;
        }
        created_dirs.push(d.to_path_buf());
        dir = d.parent();
    }
    fs::create_dir_all(root).map_err(|e| io_err(root.display(), e))?;
    let path = root.join(LOCK_FILE);
    check_inside(root, &path)?;
    let file = open_lock_file(&path).map_err(|e| io_err(path.display(), e))?;
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(RootLock {
                    file: Some(file),
                    root: root.to_path_buf(),
                    path,
                    created_dirs,
                    remove_if_empty,
                });
            }
            Err(fs::TryLockError::WouldBlock) => {
                if Instant::now() >= deadline {
                    return Err(CliError::Io(format!(
                        "{} is locked by another running cpm-planner skills command; wait \
                         for it to finish and try again",
                        root.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(fs::TryLockError::Error(err)) => return Err(io_err(path.display(), err)),
        }
    }
}

/// Lock every distinct root, in path order.
fn lock_all(roots: &[&Path], remove_if_empty: bool) -> Result<Vec<RootLock>, CliError> {
    let mut dirs: Vec<&Path> = roots.to_vec();
    dirs.sort();
    dirs.dedup();
    dirs.into_iter()
        .map(|root| lock_root(root, remove_if_empty))
        .collect()
}

// ---------------------------------------------------------------- manifest

/// `^cpm-[a-z0-9-]+$`
fn is_cpm_name(name: &str) -> bool {
    name.strip_prefix("cpm-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    })
}

/// Whether a manifest key is a safe relative path that this root kind can
/// hold: plain `/`-separated segments only, starting with a known skill name
/// (or a `cpm-*` name an older version may have shipped).
fn valid_manifest_key(kind: RootKind, key: &str) -> bool {
    if key.is_empty()
        || key.starts_with('/')
        || key.contains(['\\', ':'])
        || key.chars().any(char::is_control)
    {
        return false;
    }
    let segments: Vec<&str> = key.split('/').collect();
    if segments
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return false;
    }
    if !Path::new(key)
        .components()
        .all(|c| matches!(c, Component::Normal(_)))
    {
        return false;
    }
    match kind {
        RootKind::Skills => {
            segments.len() >= 2
                && (skill_names().contains(&segments[0]) || is_cpm_name(segments[0]))
        }
        RootKind::GeminiCommands => {
            segments.len() == 1 && key.strip_suffix(".toml").is_some_and(is_cpm_name)
        }
    }
}

fn load_manifest(root: &Root) -> Result<Option<Manifest>, CliError> {
    let path = root.dir.join(MANIFEST_FILE);
    check_inside(&root.dir, &path)?;
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
    if let Some(bad) = manifest
        .files
        .keys()
        .find(|k| !valid_manifest_key(root.kind, k))
    {
        return Err(CliError::Io(format!(
            "{} lists the unsafe or unknown path {bad:?}; refusing to touch {} \
             (nothing was changed; delete the manifest to start over)",
            path.display(),
            root.dir.display()
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
    let mut manifest = manifest.clone();
    manifest.schema = MANIFEST_SCHEMA;
    manifest.cpm_planner_version = VERSION.to_string();
    let mut json = serde_json::to_string_pretty(&manifest)
        .map_err(|e| CliError::Io(format!("cannot encode the manifest: {e}")))?;
    json.push('\n');
    write_atomic(&path, json.as_bytes())
}

// ---------------------------------------------------------------- report

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

    fn count(&self, status: Status) -> usize {
        self.counts.get(status.label()).copied().unwrap_or(0)
    }

    fn summary(&self) -> String {
        Status::ALL
            .iter()
            .map(|s| {
                format!(
                    "{} {}",
                    self.count(*s),
                    s.label().replace(", skipped", " (skipped)")
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ---------------------------------------------------------------- options

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
        if matches!(flag, "--user" | "--dry-run" | "--force") && inline.is_some() {
            return Err(CliError::Usage(format!("{flag} takes no value")));
        }
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
            "--user" => set_scope(&mut opts, Scope::User(PathBuf::new()))?,
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
            "--target agents-md is project only (there is no user-level AGENTS.md); \
             for user scope use --target codex --user"
                .into(),
        ));
    }
    Ok((target, scope))
}

/// Run `cpm-planner skills <args>`; returns the process exit code.
pub fn run(args: &[String]) -> u8 {
    let mut stdout = io::stdout().lock();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("install") => parse_options(
            rest,
            &["--target", "--project", "--user", "--dry-run", "--force"],
        )
        .and_then(|o| install(&o, &mut stdout)),
        Some("uninstall") => parse_options(rest, &["--target", "--project", "--user", "--dry-run"])
            .and_then(|o| uninstall(&o, &mut stdout)),
        Some("list") => {
            parse_options(rest, &["--project", "--user"]).and_then(|o| list(&o, &mut stdout))
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

fn print_report(out: &mut dyn io::Write, report: &Report) -> Result<(), CliError> {
    for line in &report.lines {
        out_line(out, line)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- plans

/// The manifest effect of one file operation once it has succeeded.
#[derive(Debug, Clone)]
enum Entry {
    Set(String),
    Keep,
    Drop,
}

#[derive(Debug, Clone)]
struct FileOp {
    path: PathBuf,
    rel: String,
    /// `None`: a silent manifest-only change (e.g. a file that is already gone).
    status: Option<Status>,
    write: Option<Vec<u8>>,
    remove: bool,
    entry: Entry,
}

struct RootPlan {
    root: Root,
    manifest: Manifest,
    ops: Vec<FileOp>,
    /// `kept (still used by …)` for an uninstall that leaves the files.
    kept_by: Option<Vec<String>>,
}

/// Decide what install does with one file.
fn decide(
    current: Option<&[u8]>,
    content: &[u8],
    recorded: Option<&String>,
    force: bool,
) -> (Status, bool, Entry) {
    let new_hash = sha256_hex(content);
    let Some(current) = current else {
        return (Status::Created, true, Entry::Set(new_hash));
    };
    if current == content {
        return (Status::Unchanged, false, Entry::Set(new_hash));
    }
    if force || recorded == Some(&sha256_hex(current)) {
        (Status::Updated, true, Entry::Set(new_hash))
    } else if recorded.is_some() {
        (Status::Modified, false, Entry::Keep)
    } else {
        (Status::Foreign, false, Entry::Keep)
    }
}

fn plan_install_root(
    root: &Root,
    scope: &Scope,
    target: Target,
    force: bool,
) -> Result<RootPlan, CliError> {
    let old = load_manifest(root)?;
    let mut manifest = old.clone().unwrap_or_default();
    manifest.targets.insert(target.id().to_string());
    let planned = planned_files(root, scope);
    let mut ops = Vec::new();
    for (rel, content) in &planned {
        let path = join_rel(&root.dir, rel);
        check_inside(&root.dir, &path)?;
        let current = read_existing(&path)?;
        let recorded = old.as_ref().and_then(|m| m.files.get(rel));
        let (status, write, entry) =
            decide(current.as_deref(), content.as_bytes(), recorded, force);
        ops.push(FileOp {
            path,
            rel: rel.clone(),
            status: Some(status),
            write: write.then(|| content.as_bytes().to_vec()),
            remove: false,
            entry,
        });
    }
    // Files an older cpm-planner wrote that this one no longer ships.
    for (rel, recorded) in old.iter().flat_map(|m| &m.files) {
        if planned.iter().any(|(p, _)| p == rel) {
            continue;
        }
        ops.push(plan_removal(&root.dir, rel, recorded)?);
    }
    Ok(RootPlan {
        root: root.clone(),
        manifest,
        ops,
        kept_by: None,
    })
}

/// Remove `rel` if it still matches the manifest; keep it if edited.
fn plan_removal(root: &Path, rel: &str, recorded: &str) -> Result<FileOp, CliError> {
    let path = join_rel(root, rel);
    check_inside(root, &path)?;
    let (status, remove, entry) = match read_existing(&path)? {
        None => (None, false, Entry::Drop),
        Some(current) if sha256_hex(&current) == recorded => {
            (Some(Status::Removed), true, Entry::Drop)
        }
        Some(_) => (Some(Status::Modified), false, Entry::Keep),
    };
    Ok(FileOp {
        path,
        rel: rel.to_string(),
        status,
        write: None,
        remove,
        entry,
    })
}

fn plan_uninstall_root(root: &Root, target: Target) -> Result<Option<RootPlan>, CliError> {
    let Some(mut manifest) = load_manifest(root)? else {
        return Ok(None);
    };
    if manifest.targets.is_empty() {
        manifest.targets.insert(target.id().to_string());
    }
    manifest.targets.remove(target.id());
    if !manifest.targets.is_empty() {
        let kept_by = manifest.targets.iter().cloned().collect();
        return Ok(Some(RootPlan {
            root: root.clone(),
            manifest,
            ops: Vec::new(),
            kept_by: Some(kept_by),
        }));
    }
    let ops = manifest
        .files
        .iter()
        .map(|(rel, recorded)| plan_removal(&root.dir, rel, recorded))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Some(RootPlan {
        root: root.clone(),
        manifest,
        ops,
        kept_by: None,
    }))
}

/// Run a root plan. On a failed write the manifest is saved for what was
/// done so far, so the next run still recognises those files.
fn execute_root(plan: &RootPlan, dry_run: bool, report: &mut Report) -> Result<(), CliError> {
    let mut manifest = plan.manifest.clone();
    for op in &plan.ops {
        if !dry_run {
            let result = if let Some(bytes) = &op.write {
                write_atomic(&op.path, bytes)
            } else if op.remove {
                remove_file(&op.path).map(|()| prune_empty_dirs(&plan.root.dir, &op.path))
            } else {
                Ok(())
            };
            if let Err(err) = result {
                let _ = save_manifest(&plan.root.dir, &manifest);
                return Err(err);
            }
        }
        match &op.entry {
            Entry::Set(hash) => {
                manifest.files.insert(op.rel.clone(), hash.clone());
            }
            Entry::Drop => {
                manifest.files.remove(&op.rel);
            }
            Entry::Keep => {}
        }
        if let Some(status) = op.status {
            report.record(status, &op.path);
        }
    }
    if let Some(kept_by) = &plan.kept_by {
        report.lines.push(format!(
            "kept (still used by {})  {}",
            kept_by.join(", "),
            plan.root.dir.display()
        ));
    }
    if !dry_run {
        save_manifest(&plan.root.dir, &manifest)?;
    }
    Ok(())
}

/// A planned change to `AGENTS.md`.
struct AgentsPlan {
    /// `<dir>/AGENTS.md`, for messages.
    shown: PathBuf,
    /// The file actually written (a symlink's target inside the project).
    file: PathBuf,
    status: Status,
    new_text: Option<String>,
    /// The block hash to record (`None`: leave the manifest alone).
    record: Option<Option<String>>,
    created: bool,
    delete_file: bool,
}

fn plan_install_agents_md(
    dir: &Path,
    recorded: Option<&String>,
    force: bool,
) -> Result<AgentsPlan, CliError> {
    let shown = dir.join("AGENTS.md");
    let file = resolve_agents_md(dir)?;
    let existing = read_text(&file)?;
    let eol = existing.as_deref().map_or("\n", line_ending);
    let block = agents_md_block(eol);
    let block_hash = sha256_hex(block.as_bytes());
    let mut created = false;
    let (status, new_text) = match &existing {
        None => {
            created = true;
            (Status::Created, Some(format!("{block}{eol}")))
        }
        Some(text) => match find_block(text, &shown)? {
            None if text.is_empty() => (Status::Updated, Some(format!("{block}{eol}"))),
            None if text.ends_with(eol) => {
                (Status::Updated, Some(format!("{text}{eol}{block}{eol}")))
            }
            None => (Status::Updated, Some(format!("{text}{eol}{eol}{block}"))),
            Some((start, end)) => {
                let current = &text[start..end];
                if current == block {
                    (Status::Unchanged, None)
                } else if force || recorded == Some(&sha256_hex(current.as_bytes())) {
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
    let record = matches!(
        status,
        Status::Created | Status::Updated | Status::Unchanged
    )
    .then_some(Some(block_hash));
    Ok(AgentsPlan {
        shown,
        file,
        status,
        new_text,
        record,
        created,
        delete_file: false,
    })
}

fn plan_uninstall_agents_md(
    dir: &Path,
    recorded: Option<&String>,
    created: bool,
) -> Result<Option<AgentsPlan>, CliError> {
    let shown = dir.join("AGENTS.md");
    let file = resolve_agents_md(dir)?;
    let Some(text) = read_text(&file)? else {
        return Ok(None);
    };
    let Some((start, end)) = find_block(&text, &shown)? else {
        return Ok(None);
    };
    if recorded != Some(&sha256_hex(&text.as_bytes()[start..end])) {
        let status = if recorded.is_some() {
            Status::Modified
        } else {
            Status::Foreign
        };
        return Ok(Some(AgentsPlan {
            shown,
            file,
            status,
            new_text: None,
            record: None,
            created: false,
            delete_file: false,
        }));
    }
    // Undo exactly what install added, so install + uninstall is byte-identical.
    let eol = line_ending(&text);
    let before = &text[..start];
    let after = &text[end..];
    let terminator = if after.starts_with("\r\n") {
        "\r\n"
    } else if after.starts_with('\n') {
        "\n"
    } else {
        ""
    };
    let rest = if after.is_empty() {
        let twice = format!("{eol}{eol}");
        before
            .strip_suffix(twice.as_str())
            .unwrap_or(before)
            .to_string()
    } else if after == terminator {
        before
            .strip_suffix(terminator)
            .unwrap_or(before)
            .to_string()
    } else {
        format!("{before}{}", &after[terminator.len()..])
    };
    let delete_file = rest.is_empty() && created;
    Ok(Some(AgentsPlan {
        shown,
        file,
        status: Status::Removed,
        new_text: (!delete_file).then_some(rest),
        record: Some(None),
        created: false,
        delete_file,
    }))
}

fn execute_agents_md(
    plan: &AgentsPlan,
    skills_root: &Root,
    dry_run: bool,
    report: &mut Report,
) -> Result<(), CliError> {
    report.record(plan.status, &plan.shown);
    if dry_run {
        return Ok(());
    }
    if plan.delete_file {
        remove_file(&plan.file)?;
    } else if let Some(text) = &plan.new_text {
        write_atomic(&plan.file, text.as_bytes())?;
    }
    if let Some(record) = &plan.record {
        let mut manifest = load_manifest(skills_root)?.unwrap_or_default();
        manifest.agents_md_created =
            record.is_some() && (plan.created || manifest.agents_md_created);
        manifest.agents_md_block = record.clone();
        save_manifest(&skills_root.dir, &manifest)?;
    }
    Ok(())
}

// ---------------------------------------------------------------- commands

fn install(opts: &Options, out: &mut dyn io::Write) -> Result<(), CliError> {
    let (target, scope) = require(opts)?;
    let roots = roots_for(target, &scope);
    let _locks = if opts.dry_run {
        Vec::new()
    } else {
        lock_all(
            &roots.iter().map(|r| r.dir.as_path()).collect::<Vec<_>>(),
            false,
        )?
    };
    // Validate and plan everything before the first write.
    let plans = roots
        .iter()
        .map(|root| plan_install_root(root, &scope, target, opts.force))
        .collect::<Result<Vec<_>, _>>()?;
    let agents = if target == Target::AgentsMd {
        let recorded = plans[0].manifest.agents_md_block.as_ref();
        Some(plan_install_agents_md(scope.base(), recorded, opts.force)?)
    } else {
        None
    };

    let mut report = Report::default();
    let mut result = plans
        .iter()
        .try_for_each(|plan| execute_root(plan, opts.dry_run, &mut report));
    if let (Ok(()), Some(agents)) = (&result, &agents) {
        result = execute_agents_md(agents, &roots[0], opts.dry_run, &mut report);
    }
    print_report(out, &report)?;
    result?;

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
    if report.count(Status::Modified) + report.count(Status::Foreign) > 0 {
        out_line(
            out,
            "Skipped files were kept as they are; rerun with --force to overwrite them.",
        )?;
    }
    if target == Target::All {
        out_line(
            out,
            "Note: --target all writes .claude/skills (Claude Code) and .agents/skills (Codex, \
             Gemini and others) only. Cursor and Copilot read both directories and may list \
             each skill twice; to avoid that, install with --target cursor or --target copilot \
             alone.",
        )?;
    }
    out_line(out, "")?;
    out_line(out, &registration(target, &scope))
}

fn uninstall(opts: &Options, out: &mut dyn io::Write) -> Result<(), CliError> {
    let (target, scope) = require(opts)?;
    let roots: Vec<Root> = roots_for(target, &scope)
        .into_iter()
        .filter(|r| r.dir.is_dir())
        .collect();
    let locks = if opts.dry_run {
        Vec::new()
    } else {
        lock_all(
            &roots.iter().map(|r| r.dir.as_path()).collect::<Vec<_>>(),
            true,
        )?
    };
    let plans = roots
        .iter()
        .map(|root| plan_uninstall_root(root, target))
        .collect::<Result<Vec<_>, _>>()?;
    let agents_skills = agents_root(scope.base());
    let agents = if target == Target::AgentsMd {
        let manifest = plans
            .iter()
            .flatten()
            .find(|p| p.root.dir == agents_skills.dir)
            .map(|p| &p.manifest);
        plan_uninstall_agents_md(
            scope.base(),
            manifest.and_then(|m| m.agents_md_block.as_ref()),
            manifest.is_some_and(|m| m.agents_md_created),
        )?
    } else {
        None
    };

    let mut report = Report::default();
    let mut result = plans
        .iter()
        .flatten()
        .try_for_each(|plan| execute_root(plan, opts.dry_run, &mut report));
    if let (Ok(()), Some(agents)) = (&result, &agents) {
        result = execute_agents_md(agents, &agents_skills, opts.dry_run, &mut report);
    }
    drop(locks);
    if !opts.dry_run {
        for root in &roots {
            let _ = fs::remove_dir(&root.dir);
        }
    }
    print_report(out, &report)?;
    result?;

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
    let removed_shared = plans.iter().flatten().any(|p| {
        p.root.dir == agents_skills.dir && p.ops.iter().any(|op| op.status == Some(Status::Removed))
    });
    if removed_shared && !opts.dry_run {
        out_line(
            out,
            "Note: removed the skills from .agents/skills, which Codex, Cursor, Copilot and \
             Gemini all read.",
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
            let Some(manifest) = load_manifest(&root)? else {
                continue;
            };
            let targets = manifest
                .targets
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ");
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
                        "{}  ({}) cpm-planner {} [{targets}]: {} files, {modified} modified, {missing} missing",
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

// ---------------------------------------------------------------- registration

/// The MCP registration instructions for `target` (never written, only printed).
fn registration(target: Target, scope: &Scope) -> String {
    let exe = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cpm-planner".to_string());
    let npx = format!("npx {}", NPX_ARGS.join(" "));
    let exe_word = shell_quote(&exe);
    let json = |key: &str, typed: bool| -> String {
        let server = |command: &str, args: &[&str]| {
            let mut s = serde_json::Map::new();
            if typed {
                s.insert("type".into(), "stdio".into());
            }
            s.insert("command".into(), command.into());
            s.insert("args".into(), serde_json::json!(args));
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
         or, for this binary:\n  claude mcp add --transport stdio --scope {scope_flag} cpm-planner -- {exe_word}\n"
    );
    let codex = if scope.is_user() {
        format!(
            "  codex mcp add cpm-planner -- {npx}\n  or, for this binary:\n  codex mcp add cpm-planner -- {exe_word}\n"
        )
    } else {
        format!(
            "  Add to .codex/config.toml (trusted projects only):\n  [mcp_servers.cpm-planner]\n  \
             command = \"npx\"\n  args = [{}]\n  or, for this binary:\n  [mcp_servers.cpm-planner]\n  \
             command = {}\n  args = []\n",
            NPX_ARGS.map(toml_string).join(", "),
            toml_string(&exe)
        )
    };
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
                let npx_arg =
                    serde_json::json!({"name": "cpm-planner", "command": "npx", "args": NPX_ARGS});
                let exe_arg =
                    serde_json::json!({"name": "cpm-planner", "command": &exe, "args": []});
                let _ = writeln!(
                    text,
                    "  code --add-mcp {}\n  or, for this binary:\n  code --add-mcp {}\n  \
                     or run \"MCP: Open User Configuration\" in VS Code and add:\n{}",
                    shell_quote(&npx_arg.to_string()),
                    shell_quote(&exe_arg.to_string()),
                    json("servers", true)
                );
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
                "  gemini mcp add -s {scope_flag} cpm-planner {npx}\n  or, for this binary:\n  \
                 gemini mcp add -s {scope_flag} cpm-planner {exe_word}\n  or add to {file}:\n{}",
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
    if cfg!(windows) {
        text.push_str(
            "On Windows these commands work in cmd and PowerShell. A path with spaces is in \
             double quotes; in PowerShell, a path containing `$` or a backtick needs single \
             quotes instead. For JSON arguments in PowerShell, prefer the JSON snippet.\n",
        );
    }
    text.push_str("See docs/agents/tool-matrix.md for every client.");
    text
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

    #[test]
    fn toml_string_escapes_quotes_backslashes_and_newlines() {
        assert_eq!(toml_string("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn posix_quote_wraps_spaces_and_single_quotes() {
        assert_eq!(quote_word("/a b/it's", false), "'/a b/it'\\''s'");
    }

    #[test]
    fn posix_quote_leaves_plain_paths_bare() {
        assert_eq!(
            quote_word("/usr/local/bin/cpm-planner", false),
            "/usr/local/bin/cpm-planner"
        );
    }

    #[test]
    fn windows_quote_double_quotes_a_spaced_path() {
        assert_eq!(
            quote_word("C:\\Program Files\\cpm\\cpm-planner.exe", true),
            "\"C:\\Program Files\\cpm\\cpm-planner.exe\""
        );
    }

    #[test]
    fn windows_quote_leaves_plain_paths_bare() {
        assert_eq!(
            quote_word("C:\\tools\\cpm-planner.exe", true),
            "C:\\tools\\cpm-planner.exe"
        );
    }

    #[test]
    fn windows_quote_escapes_json_quotes() {
        assert_eq!(quote_word("{\"a\":1}", true), "\"{\\\"a\\\":1}\"");
    }

    #[test]
    fn gemini_command_toml_round_trips_quotes_and_backslashes() {
        let home = PathBuf::from("/h/a \"q\" b\\c");
        let cmd = gemini_command("cpm-plan", &Scope::User(home.clone()));
        let table: toml::Table = toml::from_str(&cmd).expect("valid TOML");
        let skill = home
            .join(".agents")
            .join("skills")
            .join("cpm-plan")
            .join("SKILL.md");
        let prompt = table.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        assert!(prompt.contains(&skill.display().to_string()));
    }

    #[test]
    fn manifest_keys_must_be_plain_known_paths() {
        let rejected: Vec<&str> = [
            "../../victim",
            "cpm-plan/../../victim",
            "cpm-plan\\..\\victim",
            "/etc/passwd",
            "C:/x/y",
            "cpm-plan//SKILL.md",
            "cpm-plan/./SKILL.md",
            "other/SKILL.md",
            "cpm-plan",
        ]
        .into_iter()
        .filter(|k| valid_manifest_key(RootKind::Skills, k))
        .collect();
        assert_eq!(rejected, Vec::<&str>::new());
    }

    #[test]
    fn markers_inside_a_line_are_not_markers() {
        let text = format!("see `{BLOCK_BEGIN}` here\n");
        assert_eq!(find_block(&text, Path::new("AGENTS.md")).ok(), Some(None));
    }
}
