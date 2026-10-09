//! P6 Task 0 live spike: one Jev `Noul` question over OpenRouter, through
//! the production path (`LlmConfig::from_env` + `JevJudge`).
//!
//! ```text
//! OPENROUTER_API_KEY=sk-or-... cargo run --example jev_openrouter_smoke
//! ```
//!
//! Uses the real network and spends a few tokens, so it is an example, not a
//! test. Every setting the server reads applies: `CPM_OPENROUTER_KEY_FILE`,
//! `CPM_JEV_MODEL`, `CPM_JEV_ENDPOINT`, `CPM_LLM_TIMEOUT_SECS`. The key is
//! never printed.

use std::collections::BTreeMap;
use std::process::ExitCode;

use cpm_planner::llm::jev::JevJudge;
use cpm_planner::llm::{Answer, JudgmentModel, LlmConfig, Question};
use serde_json::json;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match LlmConfig::from_env() {
        Ok(Some(config)) => config,
        Ok(None) => {
            eprintln!(
                "OPENROUTER_API_KEY is not set (nor CPM_OPENROUTER_KEY_FILE); this live smoke \
                 test needs an OpenRouter key.\n\
                 Run: OPENROUTER_API_KEY=sk-or-... cargo run --example jev_openrouter_smoke"
            );
            return ExitCode::from(2);
        }
        Err(e) => {
            eprintln!("LLM configuration error: {e}");
            return ExitCode::from(2);
        }
    };
    println!(
        "model={} endpoint={} timeout={}s",
        config.jev_model(),
        config.jev_endpoint(),
        config.timeout().as_secs()
    );

    let questions = BTreeMap::from([(
        "dependency_real".to_string(),
        Question::Noul {
            instructions: json!(
                "Is this dependency real: must 'design schema' finish before \
                 'write migration' can start?"
            ),
            criteria: None,
        },
    )]);
    let state = json!({
        "from": { "id": "a", "title": "design schema" },
        "to": { "id": "b", "title": "write migration" }
    });

    match JevJudge::new(&config).decide(state, questions).await {
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
        Ok(decisions) => {
            match decisions.answers.get("dependency_real") {
                Some(Answer::Noul { noul }) => println!("answer: noul={noul:.4}"),
                other => println!("answer: {other:?}"),
            }
            println!("reported model: {}", decisions.model);
            println!("usage: {:?}", decisions.usage);
            ExitCode::SUCCESS
        }
    }
}
