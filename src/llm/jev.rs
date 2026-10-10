//! [`JudgmentModel`] over Jev, via `rig-typesafeai`, pointed at OpenRouter.
//!
//! The request is `rig-typesafeai`'s (its [`JevConfig`] wire encodes it and
//! [`DynamicQuery`] validates questions and answers), sent by rig's HTTP
//! driver. Only the reply type is ours (`Reply`): rig-typesafeai 0.44
//! decodes `usage` as rig-core's `Usage`, whose `cost` must be an object
//! with a `total`, while OpenRouter reports `usage.cost` as a bare number,
//! so every OpenRouter reply failed to decode. `Reply` reads `usage`
//! leniently: `cost` may be a number or an object, unknown fields are
//! ignored, and malformed accounting is dropped rather than failing the
//! judgment.
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
use rig_core::completion::request::Cost;
use rig_core::driver::Model;
use rig_core::error::{EncodeError, ProviderError};
use rig_core::operation::Whole;
use rig_core::wire::{
    Call, Descriptor, Encoded, Free, Json, Mode, Operation, Wire, WireFrame, document,
};
use rig_typesafeai::{DynamicQuery, JevConfig, Query};
use serde::Deserialize;

use super::{
    Answer, ApiKey, Decisions, JudgmentError, JudgmentErrorKind, JudgmentModel, LlmConfig,
    Question, Usage,
};

/// Longest slice of a provider error body written to the debug log.
const BODY_LOG_CHARS: usize = 200;

/// One Jev evaluation whose reply decodes as [`Reply`]; otherwise
/// rig-typesafeai's `Evaluation` (same request, one whole JSON reply).
struct Evaluation;

impl Operation for Evaluation {
    type Request = rig_typesafeai::types::Request;
    type Event = std::convert::Infallible;
    type End = Reply;
    type Response = Reply;
    type Fold = Whole<Self>;
    type Emit = Free;

    fn fold(_request: &Self::Request, _call: &mut Call<'_>) -> Whole<Self> {
        Whole::<Self>::new()
    }
}

/// rig-typesafeai's [`JevConfig`] wire (endpoint, model, bearer key, request
/// body), answering with [`Reply`].
#[derive(Clone)]
struct JevWire(JevConfig);

impl Wire for JevWire {
    type Op = Evaluation;
    type Payload = Encoded;
    type Frame = WireFrame;
    type Decoder<'id> = Json;
    type Reassembler = document::Unreassembled;

    fn describe(&self) -> Descriptor<'_> {
        self.0.describe()
    }

    fn encode(
        &self,
        request: rig_typesafeai::types::Request,
        mode: Mode,
    ) -> Result<Encoded, EncodeError> {
        self.0.encode(request, mode)
    }

    fn decoder<'id>(&self) -> Self::Decoder<'id> {
        Json
    }
}

/// A Jev reply. Fields other than these are ignored.
#[derive(Debug, Deserialize)]
struct Reply {
    model: String,
    answers: UniqueAnswers,
    #[serde(default, deserialize_with = "lenient_usage")]
    usage: Option<Usage>,
}

/// The `answers` object; like rig-typesafeai, an empty or repeated question
/// id is a decode error rather than a silently merged map.
#[derive(Debug)]
struct UniqueAnswers(BTreeMap<String, Answer>);

impl<'de> Deserialize<'de> for UniqueAnswers {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visit;
        impl<'de> serde::de::Visitor<'de> for Visit {
            type Value = UniqueAnswers;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an object keyed by unique question IDs")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<UniqueAnswers, M::Error> {
                let mut answers = BTreeMap::new();
                while let Some((id, answer)) = map.next_entry::<String, Answer>()? {
                    if id.is_empty() || answers.insert(id, answer).is_some() {
                        return Err(serde::de::Error::custom(
                            "question IDs must be nonempty and unique",
                        ));
                    }
                }
                Ok(UniqueAnswers(answers))
            }
        }
        deserializer.deserialize_map(Visit)
    }
}

/// `usage` as rig-core's [`Usage`], tolerating provider dialects: a bare
/// number `cost` (OpenRouter) is its total, a `cost` that is neither a
/// number nor a valid cost object is dropped, and a `usage` that still does
/// not fit is `None`. Token accounting never fails a judgment.
fn lenient_usage<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Usage>, D::Error> {
    let Some(serde_json::Value::Object(mut usage)) =
        Option::<serde_json::Value>::deserialize(deserializer)?
    else {
        return Ok(None);
    };
    let cost = usage.remove("cost").and_then(|cost| match cost {
        serde_json::Value::Number(total) => total.as_f64().map(Cost::from_total),
        other => serde_json::from_value::<Cost>(other).ok(),
    });
    Ok(
        serde_json::from_value::<Usage>(serde_json::Value::Object(usage))
            .ok()
            .map(|usage| usage.cost(cost)),
    )
}

/// Why `state` breaks rig-typesafeai's state rule (a string, object or
/// array), if it does.
fn state_problem(state: &serde_json::Value) -> Option<&'static str> {
    match state {
        serde_json::Value::String(_)
        | serde_json::Value::Object(_)
        | serde_json::Value::Array(_) => None,
        serde_json::Value::Null => Some("state cannot be null"),
        _ => Some("question content must be a string, object, array, or null"),
    }
}

/// A reqwest client for one judge, configured like rig's shared client.
///
/// rig's `.client()` sends through one process-wide client whose pool hands
/// keep-alive connections to every caller, while each connection's driver
/// task runs on the tokio runtime that opened it. A judge on another runtime
/// could then check out a connection whose runtime was idle (the request
/// stalled until the timeout) or shutting down (an immediate transport error,
/// "runtime dropped the dispatch task"). A client per judge keeps a judge's
/// connections to itself. If reqwest cannot build a client (TLS setup), the
/// shared one is used: it reports that build error on every send.
fn own_http_client() -> rig_reqwest::ReqwestClient {
    match rig_reqwest::reqwest::Client::builder().build() {
        Ok(client) => rig_reqwest::ReqwestClient::from(client),
        Err(_) => rig_reqwest::ReqwestClient::default(),
    }
}

/// Jev on OpenRouter's System One endpoint. Every call is bounded by the
/// configured timeout. Each judge owns its HTTP client and connection pool;
/// pooled connections are driven by the runtime that opened them, so use a
/// judge from one tokio runtime (clones share the pool).
#[derive(Clone)]
pub struct JevJudge {
    model: Model<JevWire, rig_reqwest::ReqwestClient>,
    model_id: String,
    endpoint: String,
    timeout: Duration,
    key: ApiKey,
}

impl JevJudge {
    /// Build the client from `config`. No network traffic happens here.
    pub fn new(config: &LlmConfig) -> Self {
        let wire = JevConfig::new(config.api_key().expose())
            .model(config.jev_model())
            .with_endpoint(config.jev_endpoint());
        Self {
            model: Model::new(JevWire(wire), own_http_client()),
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

    /// A tool-facing error: endpoint query/fragment removed, scrubbed of the
    /// key, then capped at
    /// [`MAX_ERROR_MESSAGE_CHARS`](super::MAX_ERROR_MESSAGE_CHARS).
    fn error(&self, kind: JudgmentErrorKind, message: impl AsRef<str>) -> JudgmentError {
        JudgmentError::scrubbed(kind, self.redact_endpoint(message.as_ref()), &self.key)
    }

    /// `text` with the configured endpoint's query string and fragment
    /// replaced by [`REDACTED`](super::REDACTED): transport errors quote the
    /// request URL, and a query may carry a token.
    fn redact_endpoint(&self, text: &str) -> String {
        let Ok(url) = url::Url::parse(&self.endpoint) else {
            return text.to_string();
        };
        let mut out = text.to_string();
        for part in [url.query(), url.fragment()].into_iter().flatten() {
            if !part.is_empty() {
                out = out.replace(part, super::REDACTED);
            }
        }
        // The raw endpoint may differ from the parsed form (encoding).
        if let Some((_, rest)) = self.endpoint.split_once(['?', '#']) {
            for part in rest.split(['?', '#']).filter(|p| !p.is_empty()) {
                out = out.replace(part, super::REDACTED);
            }
        }
        out
    }

    /// [`ApiKey::excerpt`] of `text` with the endpoint redacted, for logs.
    fn log_excerpt(&self, text: &str) -> String {
        self.key
            .excerpt(&self.redact_endpoint(text), BODY_LOG_CHARS)
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
            body = ?self.log_excerpt(reply.body.trim()),
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
            detail = ?self.log_excerpt(&error.to_string()),
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
        if let Some(reason) = state_problem(&state) {
            return Err(self.classify(&ProviderError::request(reason)));
        }
        let request = rig_typesafeai::types::Request {
            state,
            questions: serde_json::value::to_raw_value(&query)
                .map_err(|e| self.classify(&e.into()))?,
        };
        let reply = tokio::time::timeout(self.timeout, self.model.call(request))
            .await
            .map_err(|_| {
                self.error(
                    JudgmentErrorKind::Timeout,
                    format!("no reply within {} ms", self.timeout.as_millis()),
                )
            })?
            .map_err(|e| self.classify(&e))?;
        // rig's own answer validation: ids match, kinds and distributions fit.
        let answers = query
            .decode(reply.answers.0)
            .map_err(|e| self.classify(&e))?;
        Ok(Decisions {
            answers,
            // Provider-authored: scrubbed of the key, then cut.
            model: self
                .key
                .excerpt(&reply.model, super::MAX_REPORTED_MODEL_CHARS),
            usage: reply.usage,
        })
    }

    fn endpoint_host(&self) -> Option<String> {
        JevJudge::endpoint_host(self)
    }

    fn model_id(&self) -> Option<String> {
        Some(self.model_id.clone())
    }
}
