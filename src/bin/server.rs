//! `cpm-planner` — standalone MCP server for the Praxec
//! open-source CPM planner.
//!
//! Run from source with:
//!
//! ```bash
//! cargo run -p cpm-planner
//! ```
//!
//! After `cargo install cpm-planner` (or from a release bundle) the
//! binary is on your PATH as:
//!
//! ```bash
//! cpm-planner
//! ```
//!
//! The server speaks MCP over stdio (the standard transport for Claude
//! Code, Cursor, and most MCP clients). Audit events are dropped on the
//! floor by default; this is the v0.6 baseline — operator-configurable
//! audit wiring (file path, syslog, etc.) is a follow-up.

use std::sync::Arc;
use std::time::Duration;

use cpm_planner::audit::{AuditSink, NullAuditSink};
use cpm_planner::planner::DEFAULT_MAX_TTL;
use cpm_planner::{BasicCpmPlanner, PlanServer, SqlitePlanStore};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
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
    let planner = Arc::new(BasicCpmPlanner::with_store(store, audit).with_max_ttl(max_ttl));

    tracing::info!("starting cpm-planner stdio server");
    let server = PlanServer::new(planner);
    server.serve_stdio().await?;
    Ok(())
}

/// Resolve the lease TTL ceiling from `CPM_MAX_TTL_SECS`.
///
/// Unset -> [`DEFAULT_MAX_TTL`] (8h). A non-integer or zero value aborts
/// startup with a clear message rather than silently disabling the ceiling.
fn resolve_max_ttl() -> anyhow::Result<Duration> {
    match std::env::var("CPM_MAX_TTL_SECS") {
        Err(_) => Ok(DEFAULT_MAX_TTL),
        Ok(raw) => {
            let secs: u64 = raw.trim().parse().map_err(|_| {
                anyhow::anyhow!(
                    "CPM_MAX_TTL_SECS must be a positive integer number of seconds; got '{raw}'"
                )
            })?;
            if secs == 0 {
                anyhow::bail!("CPM_MAX_TTL_SECS must be >= 1; got 0");
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
