//! [`JudgmentModel`] over Jev, via `rig-typesafeai`, pointed at OpenRouter.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use rig_core::driver::Model;
use rig_core::error::ProviderError;
use rig_typesafeai::{DynamicQuery, Evaluate, JevConfig};

use super::{
    ApiKey, Decisions, JudgmentError, JudgmentErrorKind, JudgmentModel, LlmConfig, Question,
};

/// Longest slice of a provider error body carried into a message.
const BODY_EXCERPT_CHARS: usize = 200;

/// Jev on OpenRouter's System One endpoint. Every call is bounded by the
/// configured timeout.
#[derive(Clone)]
pub struct JevJudge {
    model: Model<JevConfig>,
    model_id: String,
    endpoint: String,
    timeout: Duration,
    key: ApiKey,
}

impl JevJudge {
    /// Build the client from `config`. No network traffic happens here.
    pub fn new(config: &LlmConfig) -> Self {
        let jev = JevConfig::new(config.api_key().expose())
            .model(config.jev_model())
            .with_endpoint(config.jev_endpoint())
            .client();
        Self {
            model: jev.evaluation(),
            model_id: config.jev_model().to_string(),
            endpoint: config.jev_endpoint().to_string(),
            timeout: config.timeout(),
            key: config.api_key().clone(),
        }
    }

    /// The configured Jev model id.
    pub fn model_id(&self) -> &str {
        &self.model_id
    }

    /// The configured endpoint URL.
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn error(&self, kind: JudgmentErrorKind, message: impl AsRef<str>) -> JudgmentError {
        JudgmentError::new(kind, self.key.scrub(message.as_ref()))
    }

    fn classify(&self, error: &ProviderError) -> JudgmentError {
        use JudgmentErrorKind as K;
        match error {
            ProviderError::InvalidAuthentication(reply) => {
                self.error(K::Unauthorized, describe_reply(reply))
            }
            ProviderError::ProviderResponse(reply) => {
                let kind = match reply.status.map(|s| s.as_u16()) {
                    Some(401 | 403) => K::Unauthorized,
                    Some(429) => K::RateLimited,
                    _ => K::Upstream,
                };
                self.error(kind, describe_reply(reply))
            }
            ProviderError::CacheExpired { response, .. } => {
                self.error(K::Upstream, describe_reply(response))
            }
            ProviderError::Provider(_) | ProviderError::Relayed(_) => {
                self.error(K::Upstream, error.to_string())
            }
            ProviderError::Json(_)
            | ProviderError::Response(_)
            | ProviderError::Truncated
            | ProviderError::MismatchedDimensions { .. } => {
                self.error(K::Decode, error.to_string())
            }
            ProviderError::Http(_) => self.error(K::Transport, error.to_string()),
            ProviderError::Url(_)
            | ProviderError::Request(_)
            | ProviderError::UnsupportedOption(_) => {
                self.error(K::InvalidRequest, error.to_string())
            }
            // `ProviderError` is `#[non_exhaustive]`.
            _ => self.error(K::Upstream, error.to_string()),
        }
    }
}

/// `status N: <body excerpt>` — the excerpt is scrubbed by the caller.
fn describe_reply(reply: &rig_core::ProviderResponseError) -> String {
    let body: String = reply.body.chars().take(BODY_EXCERPT_CHARS).collect();
    match reply.status {
        Some(status) => format!("status {}: {}", status.as_u16(), body.trim()),
        None => body.trim().to_string(),
    }
}

impl fmt::Debug for JevJudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JevJudge")
            .field("model", &self.model_id)
            .field("endpoint", &self.endpoint)
            .field("timeout", &self.timeout)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl JudgmentModel for JevJudge {
    async fn decide(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        let query = DynamicQuery::new(questions).map_err(|e| self.classify(&e))?;
        let call = self.model.evaluate(&state, query);
        let result = tokio::time::timeout(self.timeout, call)
            .await
            .map_err(|_| {
                self.error(
                    JudgmentErrorKind::Timeout,
                    format!("no reply within {} ms", self.timeout.as_millis()),
                )
            })?
            .map_err(|e| self.classify(&e))?;
        Ok(Decisions {
            answers: result.answers,
            model: result.model,
            usage: result.usage,
        })
    }
}
