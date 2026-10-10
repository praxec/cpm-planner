//! [`JudgmentModel`] over Jev, via `rig-typesafeai`, pointed at OpenRouter.
//!
//! Error messages are tool-facing: scrubbed of the key, capped, and limited
//! to the class plus an HTTP status or short reason. Upstream bodies are
//! logged at `debug` only (scrubbed, then truncated, and Debug-escaped so a
//! body cannot forge log lines).
//!
//! Follow-up (not implemented): the reply body size is not capped. rig's
//! driver reads replies through `HttpClientExt::send_streaming`, so a cap
//! needs a wrapper implementing all three `HttpClientExt` methods over a
//! boxed byte stream (plus direct `rig-reqwest`/`bytes`/`futures` deps), and
//! its overflow error would surface as a transport error rather than
//! `Decode`. The per-call timeout bounds how long a large reply can stream.

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

/// Longest slice of a provider error body written to the debug log.
const BODY_LOG_CHARS: usize = 200;

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

    /// Host of the configured endpoint, from the parsed URL.
    pub fn endpoint_host(&self) -> Option<String> {
        super::endpoint_host(&self.endpoint)
    }

    /// A tool-facing error: scrubbed, then capped at
    /// [`MAX_ERROR_MESSAGE_CHARS`](super::MAX_ERROR_MESSAGE_CHARS).
    fn error(&self, kind: JudgmentErrorKind, message: impl AsRef<str>) -> JudgmentError {
        JudgmentError::scrubbed(kind, message, &self.key)
    }

    /// The upstream body goes to the debug log only (scrubbed, then cut);
    /// the tool-facing message carries the status alone.
    fn reply_error(
        &self,
        kind: JudgmentErrorKind,
        reply: &rig_core::ProviderResponseError,
    ) -> JudgmentError {
        let status = reply.status.map(|s| s.as_u16());
        tracing::debug!(
            ?status,
            body = ?self.key.excerpt(reply.body.trim(), BODY_LOG_CHARS),
            "jev upstream error reply"
        );
        match status {
            Some(code) => self.error(kind, format!("status {code}")),
            None => self.error(kind, "provider error reply without an HTTP status"),
        }
    }

    /// A failure whose text may be provider-authored: logged at debug,
    /// replaced by `summary` in the tool-facing message.
    fn opaque_error(
        &self,
        kind: JudgmentErrorKind,
        summary: &str,
        error: &ProviderError,
    ) -> JudgmentError {
        tracing::debug!(
            detail = ?self.key.excerpt(&error.to_string(), BODY_LOG_CHARS),
            "jev provider failure"
        );
        self.error(kind, summary)
    }

    fn classify(&self, error: &ProviderError) -> JudgmentError {
        use JudgmentErrorKind as K;
        match error {
            ProviderError::InvalidAuthentication(reply) => self.reply_error(K::Unauthorized, reply),
            ProviderError::ProviderResponse(reply) => {
                let kind = match reply.status.map(|s| s.as_u16()) {
                    Some(401 | 403) => K::Unauthorized,
                    Some(429) => K::RateLimited,
                    _ => K::Upstream,
                };
                self.reply_error(kind, reply)
            }
            ProviderError::CacheExpired { response, .. } => self.reply_error(K::Upstream, response),
            // rig-authored, short reasons (serde position, id mismatch).
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
            ProviderError::Provider(_) | ProviderError::Relayed(_) => {
                self.opaque_error(K::Upstream, "provider reported a failure", error)
            }
            // `ProviderError` is `#[non_exhaustive]`.
            _ => self.opaque_error(K::Upstream, "unclassified provider failure", error),
        }
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

    fn endpoint_host(&self) -> Option<String> {
        JevJudge::endpoint_host(self)
    }

    fn model_id(&self) -> Option<String> {
        Some(self.model_id.clone())
    }
}
