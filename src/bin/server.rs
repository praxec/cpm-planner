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

use cpm_planner::audit::{AuditSink, NullAuditSink};
use cpm_planner::{BasicCpmPlanner, PlanServer};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    // PA4 baseline: NullAuditSink. The planner trait impl swallows audit
    // sink failures with a `tracing::warn!`, so even when a real sink is
    // wired in later the planner's correctness is unaffected by sink
    // outages.
    let audit: Arc<dyn AuditSink> = Arc::new(NullAuditSink);
    let planner = Arc::new(BasicCpmPlanner::with_audit(audit));

    tracing::info!("starting cpm-planner stdio server");
    let server = PlanServer::new(planner);
    server.serve_stdio().await?;
    Ok(())
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
