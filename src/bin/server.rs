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
//! ```
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
use cpm_planner::llm::LlmConfig;
use cpm_planner::planner::{DEFAULT_MAX_TTL, MAX_TTL_CEILING};
use cpm_planner::project::{PROJECT_ROOT_ENV, ProjectRoot};
use cpm_planner::{BasicCpmPlanner, PlanServer, SqlitePlanStore};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
cpm-planner: Critical Path Method planner as an MCP server.

Usage:
  cpm-planner                 Run the MCP server on stdio (how MCP clients start it)
  cpm-planner skills <cmd>    Install, uninstall or list the agent skills
  cpm-planner --version       Print the version
  cpm-planner --help          Print this help
";

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
            print!("{USAGE}\n{}", cpm_planner::skills::USAGE);
            ExitCode::SUCCESS
        }
        "skills" => ExitCode::from(cpm_planner::skills::run(&args[1..])),
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
    tracing::info!(db = %db_path.display(), "opening planner database");
    let store = SqlitePlanStore::open(&db_path)?;
    let max_ttl = resolve_max_ttl()?;
    tracing::info!(max_ttl_secs = max_ttl.as_secs(), "lease TTL ceiling");

    // Discover the repo root for plan-as-code tools: CPM_PROJECT_ROOT if
    // set, else the nearest ancestor of cwd containing .cpm-planner/ or
    // .git. The server stays usable without one (inline/unnamed plans);
    // only path-based portfolio tools report INVALID_PATH.
    let project_root = discover_project_root();
    match &project_root {
        Some(root) => tracing::info!(root = %root.root().display(), "project root discovered"),
        None => tracing::info!("no project root discovered; path-based tools disabled"),
    }

    let mut planner_builder = BasicCpmPlanner::with_store(store, audit).with_max_ttl(max_ttl);
    if let Some(root) = project_root {
        planner_builder = planner_builder.with_project_root(root);
    }
    let planner = Arc::new(planner_builder);

    // plan.review's judge: the LLM settings are read once, here. A bad
    // setting is logged and only disables plan.review (it reports
    // review_unavailable naming the setting); it never stops the server.
    let llm_config = LlmConfig::from_env();

    tracing::info!("starting cpm-planner stdio server");
    // The server reads the project root from the planner.
    PlanServer::new(planner)
        .with_llm_config(llm_config)
        .serve_stdio()
        .await?;
    Ok(())
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
}
