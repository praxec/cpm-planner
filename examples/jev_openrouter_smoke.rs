//! P6 Task 0 live spike: one Jev `Noul` question over OpenRouter.
//!
//! ```text
//! OPENROUTER_API_KEY=sk-or-... cargo run --example jev_openrouter_smoke
//! ```
//!
//! Uses the real network and spends a few tokens, so it is an example, not a
//! test. Optional overrides: `CPM_JEV_MODEL`, `CPM_JEV_ENDPOINT`,
//! `CPM_LLM_TIMEOUT_SECS`. The key is never printed.

use std::process::ExitCode;
use std::time::Duration;

use rig_typesafeai::{Evaluate, JevConfig, NamedQuery, Noul, Query};
use serde_json::json;

const DEFAULT_MODEL: &str = "typesafe/jev-1.13";
const DEFAULT_ENDPOINT: &str = "https://openrouter.ai/api/v1/systemone";

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// The one question the spike asks, under the id `dependency_real`.
fn build_question() -> Result<NamedQuery<Noul>, String> {
    Noul::new(
        "Is this dependency real: must 'design schema' finish before 'write migration' can start?",
    )
    .map_err(|e| e.to_string())?
    .named("dependency_real")
    .map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> ExitCode {
    let Some(key) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
    else {
        eprintln!(
            "OPENROUTER_API_KEY is not set; this live smoke test needs an OpenRouter key.\n\
             Run: OPENROUTER_API_KEY=sk-or-... cargo run --example jev_openrouter_smoke"
        );
        return ExitCode::from(2);
    };
    let model = env_or("CPM_JEV_MODEL", DEFAULT_MODEL);
    let endpoint = env_or("CPM_JEV_ENDPOINT", DEFAULT_ENDPOINT);
    let timeout = Duration::from_secs(
        env_or("CPM_LLM_TIMEOUT_SECS", "30")
            .parse()
            .unwrap_or(30)
            .clamp(1, 300),
    );

    let jev = JevConfig::new(key.trim())
        .model(model.as_str())
        .with_endpoint(endpoint.as_str())
        .client();

    let question = match build_question() {
        Ok(q) => q,
        Err(e) => {
            eprintln!("could not build question: {e}");
            return ExitCode::FAILURE;
        }
    };
    let state = json!({
        "from": { "id": "a", "title": "design schema" },
        "to": { "id": "b", "title": "write migration" }
    });

    println!(
        "model={model} endpoint={endpoint} timeout={}s",
        timeout.as_secs()
    );
    let evaluation = jev.evaluation();
    match tokio::time::timeout(timeout, evaluation.evaluate(&state, question)).await {
        Err(_) => {
            eprintln!("timeout: no reply within {}s", timeout.as_secs());
            ExitCode::FAILURE
        }
        Ok(Err(e)) => {
            // rig redacts the bearer header; scrub the body in case it echoes the key.
            eprintln!("error: {}", e.to_string().replace(key.trim(), "[redacted]"));
            ExitCode::FAILURE
        }
        Ok(Ok(result)) => {
            println!("answer: noul={:.4}", result.answers.noul);
            println!("reported model: {}", result.model);
            println!("usage: {:?}", result.usage);
            println!("provider request id: {:?}", result.provider_request_id);
            ExitCode::SUCCESS
        }
    }
}
