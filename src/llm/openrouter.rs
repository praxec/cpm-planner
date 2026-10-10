//! The rig-core OpenRouter chat client, for future headless use.
//!
//! No tool calls it yet: `plan.review` keeps reasoning with the calling agent
//! and only asks Jev for calibrated judgments. Constructing the client sends
//! nothing; any request it makes must be wrapped in
//! `tokio::time::timeout(config.timeout(), ..)`.

use rig_core::providers::openai::OpenAI;
use rig_core::providers::openrouter;

use super::LlmConfig;

/// An OpenRouter client (rig-core's OpenAI-shaped client on the OpenRouter
/// dialect) for `config`'s key. Use `.chat(config.llm_model())` for a model.
/// Its `Debug` redacts the key.
pub fn chat_client(config: &LlmConfig) -> OpenAI {
    openrouter::new(config.api_key().expose())
}
