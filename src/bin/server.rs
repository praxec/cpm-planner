//! `cpm-planner` — standalone MCP server for the Praxec
//! open-source CPM planner.
//!
//! Run from source with:
//!
//! ```bash
//! cargo run -p cpm-planner
//! ```
//!
//! After installing it (a release installer, or `cargo install --git` from a tag) the
//! binary is on your PATH as:
//!
//! ```bash
//! cpm-planner
//! ```
//!
//! With no arguments the binary is the MCP server. It also takes:
//!
//! ```bash
//! cpm-planner --version
//! cpm-planner --help
//! cpm-planner skills install --target claude --user
//! cpm-planner overview --active
//! cpm-planner explain --plan web --deliverable api
//! cpm-planner optimize --path .cpm-planner/plans/web/main.json
//! ```
//!
//! `overview`, `explain` and `optimize` run the `plan.overview`,
//! `plan.explain` and `plan.optimize` tools once, through the same argument
//! parsing and validation as the MCP server, and print the result:
//! Markdown by default for `overview` and `explain`, JSON for `optimize`.
//!
//! The server speaks MCP over stdio (the standard transport for Claude
//! Code, Cursor, and most MCP clients). Audit events are dropped on the
//! floor by default; this is the v0.6 baseline — operator-configurable
//! audit wiring (file path, syslog, etc.) is a follow-up.

use std::env::VarError;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use cpm_planner::audit::{AuditSink, NullAuditSink};
use cpm_planner::earned_value::BaselineMode;
use cpm_planner::llm::LlmConfig;
use cpm_planner::llm::command::{ReviewProvider, readiness};
use cpm_planner::planner::{DEFAULT_MAX_TTL, MAX_TTL_CEILING};
use cpm_planner::project::{PROJECT_ROOT_ENV, ProjectRoot};
use cpm_planner::{
    BasicCpmPlanner, PlanServer, STATE_MODE_ENV, SqlitePlanStore, StateOpenMode, StateOpenReport,
};
use rmcp::model::{CallToolRequestParams, JsonObject};
use serde_json::{Map, Value};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
cpm-planner: Critical Path Method planner as an MCP server.

Usage:
  cpm-planner                 Run the MCP server on stdio (how MCP clients start it)
  cpm-planner skills <cmd>    Install, uninstall or list the agent skills
  cpm-planner release <cmd>   Compile, lint or diff a bounded release workflow
  cpm-planner review doctor   Report plan.review readiness by setting name, no values
  cpm-planner overview ...    Print plan.overview (Markdown by default)
  cpm-planner explain ...     Print plan.explain (Markdown by default)
  cpm-planner optimize ...    Print plan.optimize proposals (JSON)
  cpm-planner --version       Print the version
  cpm-planner --help          Print this help
";

const READ_USAGE: &str = "\
Usage:
  cpm-planner overview SELECTOR [--as-of RFC3339] [--detail brief|full] [--limit N]
                       [--owner OWNER] [--milestone ID] [--evidence-target JSON]
                       [--format markdown|json]
  cpm-planner explain SELECTOR [--deliverable ID] [--as-of RFC3339] [--limit N]
                      [--format markdown|json]
  cpm-planner optimize (--graph FILE | --plan-id ID | --path PLAN_FILE)
                       [--capacities JSON] [--resource-key KEY]
                       [--project-buffer-pct N] [--max-proposals N]

SELECTOR is one of --plan-id ID, --plan NAME [--project KEY] or --active [--project KEY].
They read the planner database (CPM_PLANNER_DB) and project root (CPM_PROJECT_ROOT)
the server uses; optimize with --graph or --path reads no database. All three are
read-only. Errors print the tool's error code and message; the exit status is 1.
";

/// Most bytes `optimize --graph` reads from its file.
const MAX_GRAPH_FILE_BYTES: u64 = 8 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    // No arguments: the MCP stdio server, exactly as every MCP client runs it.
    // Nothing else may reach stdout in this mode.
    if args.is_empty() {
        return run_server();
    }
    let Some(args) = args
        .into_iter()
        .map(|a| a.into_string().ok())
        .collect::<Option<Vec<String>>>()
    else {
        eprintln!("cpm-planner: arguments must be valid UTF-8");
        return ExitCode::from(cpm_planner::skills::EXIT_USAGE);
    };
    match args[0].as_str() {
        "--version" | "-V" if args.len() == 1 => {
            println!("cpm-planner {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "--help" | "-h" | "help" if args.len() == 1 => {
            print!(
                "{USAGE}\n{}\n{}\n{READ_USAGE}",
                cpm_planner::skills::USAGE,
                cpm_planner::release_cli::USAGE
            );
            ExitCode::SUCCESS
        }
        "overview" | "explain" | "optimize" => run_read_command(&args[0], &args[1..]),
        "skills" => ExitCode::from(cpm_planner::skills::run(&args[1..])),
        "release" => ExitCode::from(cpm_planner::release_cli::run(&args[1..])),
        "review" if args.len() == 2 && args[1] == "doctor" => {
            let report = readiness(|name| std::env::var(name).ok());
            println!("{report}");
            if report["ready"] == true {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        }
        _ => {
            eprintln!(
                "cpm-planner: unexpected arguments `{}`\n\n{USAGE}",
                args.join(" ")
            );
            ExitCode::from(cpm_planner::skills::EXIT_USAGE)
        }
    }
}

fn run_server() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("Error: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Error: {err:?}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> anyhow::Result<()> {
    init_tracing();
    let (planner, state_report) = open_planner()?;

    // plan.review's judge: the LLM settings are read once, here. A bad
    // setting is logged and only disables plan.review (it reports
    // review_unavailable naming the setting); it never stops the server.
    let review_provider = ReviewProvider::from_env();

    tracing::info!("starting cpm-planner stdio server");
    // The server reads the project root from the planner.
    PlanServer::new(Arc::new(planner))
        .with_durable_state_report(state_report)
        .with_review_provider(review_provider, LlmConfig::from_env)
        .serve_stdio()
        .await?;
    Ok(())
}

/// Open the durable planner exactly as the server does: database, state
/// mode, lease ceiling, baseline mode and project root, all from the
/// environment.
fn open_planner() -> anyhow::Result<(BasicCpmPlanner, StateOpenReport)> {
    // PA4 baseline: NullAuditSink. The planner trait impl swallows audit
    // sink failures with a `tracing::warn!`, so even when a real sink is
    // wired in later the planner's correctness is unaffected by sink
    // outages.
    let audit: Arc<dyn AuditSink> = Arc::new(NullAuditSink);

    // Durable state: plans, statuses, and cohort locks survive restarts
    // and are shared (atomically) with any other process pointed at the
    // same database — e.g. an external orchestrate CLI. Default path is
    // ~/.local/share/praxec/cpm-planner.db; override with CPM_PLANNER_DB
    // (":memory:" gives the old ephemeral behaviour).
    let db_path = SqlitePlanStore::default_db_path()?;
    let state_mode = resolve_state_mode()?;
    // Supervisors confirm the database by reading from "opening planner
    // database db=" to the end of the line, and the path is unquoted, so this
    // line must carry `db` first and nothing after it (#117).
    tracing::info!(db = %db_path.display(), "opening planner database");
    tracing::info!(?state_mode, "durable state mode");
    let (store, state_report) = SqlitePlanStore::open_with_report(&db_path, state_mode)?;
    let max_ttl = resolve_max_ttl()?;
    tracing::info!(max_ttl_secs = max_ttl.as_secs(), "lease TTL ceiling");
    let baseline_mode = resolve_baseline_mode()?;
    tracing::info!(?baseline_mode, "earned-value baseline mode");

    // Discover the repo root for plan-as-code tools: CPM_PROJECT_ROOT if
    // set, else the nearest ancestor of cwd containing .cpm-planner/ or
    // .git. The server stays usable without one (inline/unnamed plans);
    // only path-based portfolio tools report INVALID_PATH.
    let project_root = discover_project_root();
    match &project_root {
        Some(root) => tracing::info!(root = %root.root().display(), "project root discovered"),
        None => tracing::info!("no project root discovered; path-based tools disabled"),
    }

    let mut planner = BasicCpmPlanner::with_store(store, audit)
        .with_max_ttl(max_ttl)
        .with_baseline_mode(baseline_mode);
    if let Some(root) = project_root {
        planner = planner.with_project_root(root);
    }
    Ok((planner, state_report))
}

/// What one read command line asks for: the tool, its MCP arguments, and
/// whether to print the Markdown text instead of JSON.
#[derive(Debug, PartialEq)]
struct ReadCommand {
    tool: &'static str,
    arguments: Map<String, Value>,
    markdown: bool,
    /// The command reads stored plans, so it needs the durable database.
    needs_database: bool,
}

/// How one flag becomes an MCP argument.
#[derive(Debug, Clone, Copy)]
enum FlagKind {
    /// A string argument.
    Text(&'static str),
    /// A JSON number argument.
    Number(&'static str),
    /// A JSON value argument (object).
    Json(&'static str),
    /// A string inside the `filter` object.
    Filter(&'static str),
    /// A flag without a value, `true` when present.
    Switch(&'static str),
    /// A file whose JSON content is the argument.
    File(&'static str),
}

const SELECTOR_FLAGS: &[(&str, FlagKind)] = &[
    ("--plan-id", FlagKind::Text("plan_id")),
    ("--plan", FlagKind::Text("plan")),
    ("--active", FlagKind::Switch("active")),
    ("--project", FlagKind::Text("project")),
];

const OVERVIEW_FLAGS: &[(&str, FlagKind)] = &[
    ("--as-of", FlagKind::Text("as_of")),
    ("--detail", FlagKind::Text("detail")),
    ("--limit", FlagKind::Number("limit")),
    ("--owner", FlagKind::Filter("owner")),
    ("--milestone", FlagKind::Filter("milestone")),
    ("--evidence-target", FlagKind::Json("evidence_target")),
    ("--format", FlagKind::Text("format")),
];

const EXPLAIN_FLAGS: &[(&str, FlagKind)] = &[
    ("--deliverable", FlagKind::Text("deliverable_id")),
    ("--as-of", FlagKind::Text("as_of")),
    ("--limit", FlagKind::Number("limit")),
    ("--format", FlagKind::Text("format")),
];

const OPTIMIZE_FLAGS: &[(&str, FlagKind)] = &[
    ("--graph", FlagKind::File("graph")),
    ("--plan-id", FlagKind::Text("plan_id")),
    ("--path", FlagKind::Text("path")),
    ("--capacities", FlagKind::Json("capacities")),
    ("--resource-key", FlagKind::Text("resource_key")),
    (
        "--project-buffer-pct",
        FlagKind::Number("project_buffer_pct"),
    ),
    ("--max-proposals", FlagKind::Number("max_proposals")),
];

/// Parse `overview`, `explain` or `optimize` flags into the tool's MCP
/// arguments. Argument meaning (selector rules, ranges, codes) is left to
/// the tool, so the CLI and MCP never disagree; this only rejects flags it
/// does not know, a missing value, a repeated flag and malformed JSON.
fn parse_read_command(command: &str, args: &[String]) -> Result<ReadCommand, String> {
    let (tool, flags): (&'static str, Vec<(&str, FlagKind)>) = match command {
        "overview" => (
            cpm_planner::server::TOOL_OVERVIEW,
            SELECTOR_FLAGS
                .iter()
                .chain(OVERVIEW_FLAGS)
                .copied()
                .collect(),
        ),
        "explain" => (
            cpm_planner::server::TOOL_EXPLAIN,
            SELECTOR_FLAGS
                .iter()
                .chain(EXPLAIN_FLAGS)
                .copied()
                .collect(),
        ),
        "optimize" => (cpm_planner::server::TOOL_OPTIMIZE, OPTIMIZE_FLAGS.to_vec()),
        other => return Err(format!("unknown command `{other}`")),
    };
    let mut arguments = Map::new();
    let mut filter = Map::new();
    let mut seen: Vec<&str> = Vec::new();
    let mut rest = args.iter();
    while let Some(flag) = rest.next() {
        let Some(&(name, kind)) = flags.iter().find(|(name, _)| *name == flag.as_str()) else {
            return Err(format!("unknown argument `{flag}` for {command}"));
        };
        if seen.contains(&name) {
            return Err(format!("{name} given more than once"));
        }
        seen.push(name);
        if let FlagKind::Switch(key) = kind {
            arguments.insert(key.to_string(), Value::Bool(true));
            continue;
        }
        let value = rest.next().ok_or_else(|| format!("{name} needs a value"))?;
        match kind {
            FlagKind::Text(key) => {
                arguments.insert(key.to_string(), Value::String(value.clone()));
            }
            FlagKind::Filter(key) => {
                filter.insert(key.to_string(), Value::String(value.clone()));
            }
            FlagKind::Number(key) => {
                let number: serde_json::Number = serde_json::from_str(value)
                    .map_err(|_| format!("{name} must be a number, got `{value}`"))?;
                arguments.insert(key.to_string(), Value::Number(number));
            }
            FlagKind::Json(key) => {
                let parsed: Value =
                    serde_json::from_str(value).map_err(|e| format!("{name} must be JSON: {e}"))?;
                arguments.insert(key.to_string(), parsed);
            }
            FlagKind::File(key) => {
                arguments.insert(key.to_string(), read_json_file(name, value)?);
            }
            FlagKind::Switch(_) => unreachable!("handled above"),
        }
    }
    if !filter.is_empty() {
        arguments.insert("filter".to_string(), Value::Object(filter));
    }
    let markdown = match command {
        // The CLI default is Markdown (docs/overview.md); an explicit
        // format is passed through to the tool, which validates it.
        "overview" | "explain" => match arguments.get("format").and_then(Value::as_str) {
            None => {
                arguments.insert("format".to_string(), Value::String("markdown".into()));
                true
            }
            Some(format) => format == "markdown",
        },
        _ => false,
    };
    let needs_database = command != "optimize" || arguments.contains_key("plan_id");
    Ok(ReadCommand {
        tool,
        arguments,
        markdown,
        needs_database,
    })
}

/// Read a JSON file of at most [`MAX_GRAPH_FILE_BYTES`].
fn read_json_file(flag: &str, path: &str) -> Result<Value, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_GRAPH_FILE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{flag} {path}: {e}"))?;
    if bytes.len() as u64 > MAX_GRAPH_FILE_BYTES {
        return Err(format!("{flag} {path}: input exceeds 8 MiB"));
    }
    serde_json::from_slice(&bytes).map_err(|e| format!("{flag} {path}: {e}"))
}

/// Run `overview`, `explain` or `optimize` once and print the result.
fn run_read_command(command: &str, args: &[String]) -> ExitCode {
    if args == ["--help"] || args == ["-h"] {
        print!("{READ_USAGE}");
        return ExitCode::SUCCESS;
    }
    let parsed = match parse_read_command(command, args) {
        Ok(parsed) => parsed,
        Err(reason) => {
            eprintln!("cpm-planner {command}: {reason}\n\n{READ_USAGE}");
            return ExitCode::from(cpm_planner::skills::EXIT_USAGE);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("cpm-planner {command}: cannot start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(execute_read_command(parsed)) {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(reason) => {
            eprintln!("cpm-planner {command}: {reason}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch one read command through [`PlanServer::dispatch_call`] and
/// render its result: the Markdown text, or pretty JSON.
async fn execute_read_command(command: ReadCommand) -> anyhow::Result<String> {
    let planner = if command.needs_database {
        open_planner()?.0
    } else {
        // Inline graphs and plan files need no stored state: an in-memory
        // planner keeps the command from creating or touching a database.
        let planner = BasicCpmPlanner::new();
        match discover_project_root() {
            Some(root) => planner.with_project_root(root),
            None => planner,
        }
    };
    let request = CallToolRequestParams::new(command.tool)
        .with_arguments(JsonObject::from_iter(command.arguments));
    let value = PlanServer::new(Arc::new(planner))
        .dispatch_call(request)
        .await
        .map_err(|err| anyhow::anyhow!("{}", err.message))?;
    match (
        command.markdown,
        value.get("markdown").and_then(Value::as_str),
    ) {
        (true, Some(text)) => Ok(text.trim_end().to_string()),
        _ => Ok(serde_json::to_string_pretty(&value)?),
    }
}

/// The project root: `CPM_PROJECT_ROOT` when set (a warning and no root
/// when it is not a usable directory), else discovery from the current
/// directory. An unreadable current directory is a warning and no root, not
/// a startup failure: the server stays usable for inline and unnamed plans.
fn discover_project_root() -> Option<ProjectRoot> {
    if let Some(value) = std::env::var_os(PROJECT_ROOT_ENV).filter(|v| !v.is_empty()) {
        return match ProjectRoot::from_path(std::path::Path::new(&value)) {
            Ok(root) => Some(root),
            Err(err) => {
                tracing::warn!(error = %err, "{PROJECT_ROOT_ENV} is set but invalid; no project root");
                None
            }
        };
    }
    match std::env::current_dir() {
        Ok(cwd) => ProjectRoot::discover(&cwd),
        Err(err) => {
            tracing::warn!(error = %err, "cannot read the current directory; no project root");
            None
        }
    }
}

fn resolve_state_mode() -> anyhow::Result<StateOpenMode> {
    match std::env::var(STATE_MODE_ENV) {
        Err(VarError::NotPresent) => Ok(StateOpenMode::Normal),
        Ok(value) if value == "initialize" => Ok(StateOpenMode::Initialize),
        Ok(value) if value == "recover" => Ok(StateOpenMode::Recover),
        Ok(value) => anyhow::bail!(
            "{STATE_MODE_ENV} must be `initialize` or `recover` when set, got `{value}`"
        ),
        Err(VarError::NotUnicode(_)) => anyhow::bail!("{STATE_MODE_ENV} is not valid Unicode"),
    }
}

/// Resolve the lease TTL ceiling from `CPM_MAX_TTL_SECS`.
///
/// Unset -> [`DEFAULT_MAX_TTL`] (8h). A non-integer or zero value aborts
/// startup with a clear message rather than silently disabling the ceiling.
fn resolve_max_ttl() -> anyhow::Result<Duration> {
    parse_max_ttl(std::env::var("CPM_MAX_TTL_SECS"))
}

/// Pure parse of the `CPM_MAX_TTL_SECS` lookup result. Values above 30 days
/// ([`MAX_TTL_CEILING`]) abort startup.
fn parse_max_ttl(raw: Result<String, VarError>) -> anyhow::Result<Duration> {
    match raw {
        Err(VarError::NotPresent) => Ok(DEFAULT_MAX_TTL),
        Err(VarError::NotUnicode(_)) => {
            anyhow::bail!("CPM_MAX_TTL_SECS is not valid unicode")
        }
        Ok(raw) => {
            let secs: u64 = raw.trim().parse().map_err(|_| {
                anyhow::anyhow!(
                    "CPM_MAX_TTL_SECS must be a positive integer number of seconds; got '{raw}'"
                )
            })?;
            if secs == 0 {
                anyhow::bail!("CPM_MAX_TTL_SECS must be >= 1; got 0");
            }
            if secs > MAX_TTL_CEILING.as_secs() {
                anyhow::bail!("CPM_MAX_TTL_SECS must be <= 2592000 (30 days)");
            }
            Ok(Duration::from_secs(secs))
        }
    }
}

/// Resolve the earned-value baseline mode from `CPM_BASELINE_MODE`.
///
/// Unset -> [`BaselineMode::Auto`]. Anything but `auto` or `refuse` aborts
/// startup with a clear message rather than silently picking a mode.
fn resolve_baseline_mode() -> anyhow::Result<BaselineMode> {
    parse_baseline_mode(std::env::var("CPM_BASELINE_MODE"))
}

/// Pure parse of the `CPM_BASELINE_MODE` lookup result.
fn parse_baseline_mode(raw: Result<String, VarError>) -> anyhow::Result<BaselineMode> {
    match raw {
        Err(VarError::NotPresent) => Ok(BaselineMode::Auto),
        Err(VarError::NotUnicode(_)) => anyhow::bail!("CPM_BASELINE_MODE is not valid unicode"),
        Ok(raw) => match raw.trim() {
            "auto" => Ok(BaselineMode::Auto),
            "refuse" => Ok(BaselineMode::Refuse),
            other => anyhow::bail!("CPM_BASELINE_MODE must be 'auto' or 'refuse'; got '{other}'"),
        },
    }
}

fn init_tracing() {
    // Defaults to `info`. Log to stderr so stdout remains exclusively the
    // MCP transport channel (rmcp's `stdio()` uses stdin/stdout for the
    // JSON-RPC framing).
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_max_ttl_is_the_default() {
        assert_eq!(
            parse_max_ttl(Err(VarError::NotPresent)).unwrap(),
            DEFAULT_MAX_TTL
        );
    }

    #[test]
    fn zero_max_ttl_is_rejected() {
        assert!(parse_max_ttl(Ok("0".into())).is_err());
    }

    #[test]
    fn non_numeric_max_ttl_is_rejected() {
        assert!(parse_max_ttl(Ok("abc".into())).is_err());
    }

    #[test]
    fn max_ttl_above_thirty_days_is_rejected() {
        assert!(parse_max_ttl(Ok("2592001".into())).is_err());
    }

    #[test]
    fn max_ttl_of_one_hour_parses() {
        assert_eq!(
            parse_max_ttl(Ok("3600".into())).unwrap(),
            Duration::from_secs(3600)
        );
    }

    #[test]
    fn non_unicode_max_ttl_is_rejected() {
        let bad = VarError::NotUnicode(std::ffi::OsString::from("x"));
        assert!(parse_max_ttl(Err(bad)).is_err());
    }

    #[test]
    fn unset_baseline_mode_is_auto() {
        assert_eq!(
            parse_baseline_mode(Err(VarError::NotPresent)).unwrap(),
            BaselineMode::Auto
        );
    }

    #[test]
    fn explicit_auto_baseline_mode_parses() {
        assert_eq!(
            parse_baseline_mode(Ok("auto".into())).unwrap(),
            BaselineMode::Auto
        );
    }

    #[test]
    fn refuse_baseline_mode_parses() {
        assert_eq!(
            parse_baseline_mode(Ok("refuse".into())).unwrap(),
            BaselineMode::Refuse
        );
    }

    #[test]
    fn unknown_baseline_mode_is_rejected() {
        assert!(parse_baseline_mode(Ok("sometimes".into())).is_err());
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|a| (*a).to_string()).collect()
    }

    #[test]
    fn overview_defaults_to_markdown() {
        let parsed = parse_read_command("overview", &strings(&["--active"])).unwrap();
        assert_eq!(
            (parsed.markdown, parsed.arguments.get("format").cloned()),
            (true, Some(Value::String("markdown".into())))
        );
    }

    #[test]
    fn overview_format_json_prints_json() {
        let parsed =
            parse_read_command("overview", &strings(&["--active", "--format", "json"])).unwrap();
        assert!(!parsed.markdown);
    }

    #[test]
    fn overview_owner_and_milestone_become_the_filter() {
        let parsed = parse_read_command(
            "overview",
            &strings(&["--plan", "web", "--owner", "agent", "--milestone", "ship"]),
        )
        .unwrap();
        assert_eq!(
            parsed.arguments.get("filter"),
            Some(&serde_json::json!({ "owner": "agent", "milestone": "ship" }))
        );
    }

    #[test]
    fn overview_limit_is_a_number() {
        let parsed =
            parse_read_command("overview", &strings(&["--active", "--limit", "5"])).unwrap();
        assert_eq!(parsed.arguments.get("limit"), Some(&serde_json::json!(5)));
    }

    #[test]
    fn non_numeric_limit_is_a_usage_error() {
        assert!(parse_read_command("overview", &strings(&["--limit", "five"])).is_err());
    }

    #[test]
    fn explain_deliverable_becomes_deliverable_id() {
        let parsed = parse_read_command(
            "explain",
            &strings(&["--plan-id", "p", "--deliverable", "api"]),
        )
        .unwrap();
        assert_eq!(
            parsed.arguments.get("deliverable_id"),
            Some(&Value::String("api".into()))
        );
    }

    #[test]
    fn unknown_flag_is_a_usage_error() {
        assert!(parse_read_command("explain", &strings(&["--detail", "full"])).is_err());
    }

    #[test]
    fn repeated_flag_is_a_usage_error() {
        assert!(parse_read_command("overview", &strings(&["--plan", "a", "--plan", "b"])).is_err());
    }

    #[test]
    fn flag_without_value_is_a_usage_error() {
        assert!(parse_read_command("overview", &strings(&["--plan"])).is_err());
    }

    #[test]
    fn optimize_capacities_are_parsed_as_json() {
        let parsed = parse_read_command(
            "optimize",
            &strings(&["--path", "p.json", "--capacities", r#"{"agent": 2}"#]),
        )
        .unwrap();
        assert_eq!(
            parsed.arguments.get("capacities"),
            Some(&serde_json::json!({ "agent": 2 }))
        );
    }

    #[test]
    fn optimize_of_a_plan_file_needs_no_database() {
        let parsed = parse_read_command("optimize", &strings(&["--path", "p.json"])).unwrap();
        assert!(!parsed.needs_database);
    }

    #[test]
    fn optimize_of_a_stored_plan_needs_the_database() {
        let parsed = parse_read_command("optimize", &strings(&["--plan-id", "p"])).unwrap();
        assert!(parsed.needs_database);
    }

    #[test]
    fn optimize_prints_json() {
        let parsed = parse_read_command("optimize", &strings(&["--path", "p.json"])).unwrap();
        assert!(!parsed.markdown);
    }
}
