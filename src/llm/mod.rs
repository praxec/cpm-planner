//! OpenRouter / Jev configuration and the [`JudgmentModel`] seam (P6, #26).
//!
//! [`LlmConfig::from_env`] reads the OpenRouter key and the Jev / generative
//! model settings. `Ok(None)` means "no key configured": callers report
//! `review_unavailable` rather than silently falling back.
//!
//! [`JudgmentModel`] is the one async seam `plan.review` depends on, so the
//! review logic is testable with a fake. [`jev::JevJudge`] implements it with
//! `rig-typesafeai` pointed at OpenRouter's System One endpoint, and
//! [`openrouter::chat_client`] builds the rig-core OpenRouter chat client for
//! future headless use.
//!
//! # Key hygiene
//!
//! The key lives in an [`ApiKey`] whose `Debug` is redacted and which has no
//! `Display`/`Serialize`. [`JudgmentError`] messages are scrubbed of the key
//! before they are built, so neither logs, errors, audit records nor tool
//! output can carry it.

pub mod jev;
pub mod openrouter;

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
pub use rig_typesafeai::types::{Answer, Question, Usage};

/// Environment variable holding the OpenRouter key.
pub const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
/// Environment variable naming a file that holds the OpenRouter key.
pub const OPENROUTER_KEY_FILE_ENV: &str = "CPM_OPENROUTER_KEY_FILE";
/// Environment variable overriding the Jev model id.
pub const JEV_MODEL_ENV: &str = "CPM_JEV_MODEL";
/// Environment variable overriding the Jev endpoint URL.
pub const JEV_ENDPOINT_ENV: &str = "CPM_JEV_ENDPOINT";
/// Environment variable bounding every LLM network call, in seconds.
pub const LLM_TIMEOUT_ENV: &str = "CPM_LLM_TIMEOUT_SECS";
/// Environment variable naming the generative (chat) model.
pub const LLM_MODEL_ENV: &str = "CPM_LLM_MODEL";

/// Default Jev model on OpenRouter.
pub const DEFAULT_JEV_MODEL: &str = "typesafe/jev-1.13";
/// Default Jev endpoint: OpenRouter's System One route.
pub const DEFAULT_JEV_ENDPOINT: &str = "https://openrouter.ai/api/v1/systemone";
/// Default per-call timeout, seconds.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Inclusive bounds for [`LLM_TIMEOUT_ENV`], seconds.
pub const TIMEOUT_SECS_RANGE: std::ops::RangeInclusive<u64> = 1..=300;
/// Default generative model (not used by any tool yet).
pub const DEFAULT_LLM_MODEL: &str = "openai/gpt-5-mini";

/// Redaction marker used wherever the key would otherwise appear.
pub const REDACTED: &str = "[redacted]";
/// Upper bound on [`JudgmentError::message`], in chars (it becomes tool output).
pub const MAX_ERROR_MESSAGE_CHARS: usize = 300;
/// Largest key file accepted, in bytes.
pub const MAX_KEY_FILE_BYTES: u64 = 4096;
/// Shortest key prefix treated as a leak when it ends a truncated excerpt.
const MIN_KEY_FRAGMENT_CHARS: usize = 8;

/// The OpenRouter key. `Debug` is redacted; there is deliberately no
/// `Display` or `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    /// The raw key. Every call site is a place the key can leak: only the
    /// HTTP clients should call this.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every occurrence of the key replaced by [`REDACTED`].
    pub fn scrub(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_string()
        } else {
            text.replace(&self.0, REDACTED)
        }
    }

    /// At most `max_chars` of `text`, safe to show: the WHOLE text is
    /// scrubbed first, then truncated, and a trailing key prefix of
    /// [`MIN_KEY_FRAGMENT_CHARS`]+ chars (a key cut by the boundary, or a
    /// partial key in the input) is dropped.
    pub fn excerpt(&self, text: &str, max_chars: usize) -> String {
        let scrubbed = self.scrub(text);
        let mut out: Vec<char> = scrubbed.chars().take(max_chars).collect();
        let key: Vec<char> = self.0.chars().collect();
        let longest = key.len().min(out.len());
        if let Some(len) = (MIN_KEY_FRAGMENT_CHARS..=longest)
            .rev()
            .find(|&len| out[out.len() - len..] == key[..len])
        {
            out.truncate(out.len() - len);
        }
        out.into_iter().collect()
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(REDACTED)
    }
}

/// Why the LLM configuration is unusable. Messages never contain the key.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// `CPM_LLM_TIMEOUT_SECS` is not an integer in 1..=300.
    #[error("{LLM_TIMEOUT_ENV} must be an integer number of seconds in 1..=300; got '{value}'")]
    InvalidTimeout { value: String },
    /// `CPM_JEV_ENDPOINT` is not an acceptable URL. The value itself is not
    /// echoed (it may carry credentials).
    #[error(
        "{JEV_ENDPOINT_ENV} must be an https:// URL (http:// only for a loopback host) \
         without credentials: {reason}"
    )]
    InvalidEndpoint { reason: String },
    /// `CPM_OPENROUTER_KEY_FILE` names something that cannot be used as a
    /// key file (missing, unreadable, not a regular file, too large).
    #[error("{OPENROUTER_KEY_FILE_ENV} '{}' cannot be read: {reason}", path.display())]
    KeyFile { path: PathBuf, reason: String },
    /// The key file was read but deliberately ignored (a warning is logged
    /// too). Distinct from "no key configured" so callers can say why.
    #[error("{OPENROUTER_KEY_FILE_ENV} '{}': key file ignored: {reason}", path.display())]
    KeyFileIgnored {
        path: PathBuf,
        reason: KeyFileIgnoredReason,
    },
}

/// Why a key file was ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyFileIgnoredReason {
    /// Other users can read it (unix mode `o+r`); `chmod 600` to use it.
    WorldReadable,
    /// It holds only whitespace.
    Empty,
}

impl fmt::Display for KeyFileIgnoredReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::WorldReadable => "world-readable",
            Self::Empty => "empty",
        })
    }
}

/// OpenRouter / Jev settings. Only constructed when a key is configured.
#[derive(Clone, Debug)]
pub struct LlmConfig {
    api_key: ApiKey,
    jev_model: String,
    jev_endpoint: String,
    timeout: Duration,
    llm_model: String,
}

impl LlmConfig {
    /// Settings for `api_key` with every other field at its default.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: ApiKey(api_key.into()),
            jev_model: DEFAULT_JEV_MODEL.to_string(),
            jev_endpoint: DEFAULT_JEV_ENDPOINT.to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            llm_model: DEFAULT_LLM_MODEL.to_string(),
        }
    }

    /// Read the process environment. `Ok(None)` = no key configured.
    pub fn from_env() -> Result<Option<Self>, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// [`Self::from_env`] over an arbitrary variable lookup (tests).
    ///
    /// The key comes from `OPENROUTER_API_KEY`, else from the file named by
    /// `CPM_OPENROUTER_KEY_FILE` (contents trimmed). A world-readable (unix)
    /// or empty key file is [`ConfigError::KeyFileIgnored`] plus a warning.
    /// A blank env key counts as absent. The endpoint is validated here:
    /// https, or http to a loopback host, with no credentials.
    pub fn from_lookup(
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Option<Self>, ConfigError> {
        let non_blank = |name: &str| lookup(name).filter(|v| !v.trim().is_empty());

        let key = match non_blank(OPENROUTER_API_KEY_ENV) {
            Some(key) => Some(key.trim().to_string()),
            None => match non_blank(OPENROUTER_KEY_FILE_ENV) {
                Some(path) => read_key_file(Path::new(path.trim()))?,
                None => None,
            },
        };

        let timeout = match non_blank(LLM_TIMEOUT_ENV) {
            Some(raw) => parse_timeout(&raw)?,
            None => Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        };
        let jev_endpoint = match non_blank(JEV_ENDPOINT_ENV) {
            Some(raw) => parse_endpoint(raw.trim())?,
            None => DEFAULT_JEV_ENDPOINT.to_string(),
        };
        let Some(key) = key else {
            return Ok(None);
        };
        let mut config = Self::new(key)
            .with_jev_endpoint(jev_endpoint)
            .with_timeout(timeout);
        if let Some(model) = non_blank(JEV_MODEL_ENV) {
            config.jev_model = model.trim().to_string();
        }
        if let Some(model) = non_blank(LLM_MODEL_ENV) {
            config.llm_model = model.trim().to_string();
        }
        Ok(Some(config))
    }

    /// Replace the Jev endpoint (complete URL including the route). Not
    /// validated: programmatic callers (tests) own it; the env path is
    /// validated by [`Self::from_lookup`].
    pub fn with_jev_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.jev_endpoint = endpoint.into();
        self
    }

    /// Replace the Jev model id.
    pub fn with_jev_model(mut self, model: impl Into<String>) -> Self {
        self.jev_model = model.into();
        self
    }

    /// Replace the per-call timeout. The env var is bounded to 1..=300 s;
    /// programmatic callers (tests) may pick any duration.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The OpenRouter key (redacted `Debug`).
    pub fn api_key(&self) -> &ApiKey {
        &self.api_key
    }

    /// The Jev model id.
    pub fn jev_model(&self) -> &str {
        &self.jev_model
    }

    /// The complete Jev endpoint URL.
    pub fn jev_endpoint(&self) -> &str {
        &self.jev_endpoint
    }

    /// The bound applied to every network call.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// The generative (chat) model id.
    pub fn llm_model(&self) -> &str {
        &self.llm_model
    }
}

fn parse_timeout(raw: &str) -> Result<Duration, ConfigError> {
    raw.trim()
        .parse::<u64>()
        .ok()
        .filter(|secs| TIMEOUT_SECS_RANGE.contains(secs))
        .map(Duration::from_secs)
        .ok_or_else(|| ConfigError::InvalidTimeout {
            value: raw.to_string(),
        })
}

fn parse_endpoint(raw: &str) -> Result<String, ConfigError> {
    let invalid = |reason: &str| ConfigError::InvalidEndpoint {
        reason: reason.to_string(),
    };
    // url's ParseError messages are generic ("invalid port number") and
    // never echo the input.
    let url = url::Url::parse(raw).map_err(|e| invalid(&e.to_string()))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("credentials in the URL are not allowed"));
    }
    let loopback = match url.host() {
        Some(url::Host::Domain(domain)) => domain.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => return Err(invalid("missing host")),
    };
    match url.scheme() {
        "https" => Ok(raw.to_string()),
        "http" if loopback => Ok(raw.to_string()),
        "http" => Err(invalid("http:// is only allowed for a loopback host")),
        _ => Err(invalid("unsupported scheme")),
    }
}

/// Read and trim the key file. It must be a regular file of at most
/// [`MAX_KEY_FILE_BYTES`]; a world-readable (unix) or empty file is
/// [`ConfigError::KeyFileIgnored`] with a warning. Messages name the path,
/// never the key.
fn read_key_file(path: &Path) -> Result<Option<String>, ConfigError> {
    use std::io::Read;
    let key_file_error = |reason: String| ConfigError::KeyFile {
        path: path.to_path_buf(),
        reason,
    };
    let io_error = |e: std::io::Error| key_file_error(e.kind().to_string());
    let ignored = |reason: KeyFileIgnoredReason| {
        tracing::warn!(path = %path.display(), %reason, "{OPENROUTER_KEY_FILE_ENV}: key file ignored");
        ConfigError::KeyFileIgnored {
            path: path.to_path_buf(),
            reason,
        }
    };

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    // Non-blocking open so a FIFO cannot hang startup; the handle is then
    // checked to be a regular file, so the file checked is the file read.
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut options, libc::O_NONBLOCK);
    let file = options.open(path).map_err(io_error)?;
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.is_file() {
        return Err(key_file_error("not a regular file".to_string()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o004 != 0 {
            return Err(ignored(KeyFileIgnoredReason::WorldReadable));
        }
    }
    let mut bytes = Vec::new();
    file.take(MAX_KEY_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    if bytes.len() as u64 > MAX_KEY_FILE_BYTES {
        return Err(key_file_error(format!(
            "larger than {MAX_KEY_FILE_BYTES} bytes"
        )));
    }
    let contents =
        String::from_utf8(bytes).map_err(|_| key_file_error("not valid UTF-8".to_string()))?;
    let key = contents.trim();
    if key.is_empty() {
        return Err(ignored(KeyFileIgnoredReason::Empty));
    }
    Ok(Some(key.to_string()))
}

/// Jev's answers to one batched evaluation.
#[derive(Debug, Clone)]
pub struct Decisions {
    /// One answer per question id, keyed exactly as asked.
    pub answers: BTreeMap<String, Answer>,
    /// The model id the provider reports having used.
    pub model: String,
    /// Token accounting when the provider reports it.
    pub usage: Option<Usage>,
}

/// Failure class of a [`JudgmentError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JudgmentErrorKind {
    /// The provider rejected the key (401/403).
    Unauthorized,
    /// The provider throttled the call (429).
    RateLimited,
    /// No reply within `CPM_LLM_TIMEOUT_SECS`.
    Timeout,
    /// Any other provider-side failure (5xx, other non-success statuses).
    Upstream,
    /// The reply arrived but does not decode or does not answer the request.
    Decode,
    /// No reply: connection refused/reset, DNS, TLS.
    Transport,
    /// The questions or state were rejected locally before sending.
    InvalidRequest,
}

impl JudgmentErrorKind {
    /// Stable snake_case name used in `review_unavailable` reasons.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::RateLimited => "rate_limited",
            Self::Timeout => "timeout",
            Self::Upstream => "upstream",
            Self::Decode => "decode",
            Self::Transport => "transport",
            Self::InvalidRequest => "invalid_request",
        }
    }
}

impl fmt::Display for JudgmentErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A classed judgment failure. `message` never contains the key, is at most
/// [`MAX_ERROR_MESSAGE_CHARS`] chars, and carries only a short reason (an
/// HTTP status, a decode reason) — never the upstream body.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{kind}: {message}")]
pub struct JudgmentError {
    /// The failure class.
    pub kind: JudgmentErrorKind,
    /// A safe, human-readable detail.
    pub message: String,
}

impl JudgmentError {
    /// Build an error. Callers must pass an already-scrubbed message; it is
    /// cut to [`MAX_ERROR_MESSAGE_CHARS`].
    pub fn new(kind: JudgmentErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message
                .into()
                .chars()
                .take(MAX_ERROR_MESSAGE_CHARS)
                .collect(),
        }
    }
}

/// A calibrated-judgment model: one batched call answering runtime-defined
/// questions about one shared state.
#[async_trait]
pub trait JudgmentModel: Send + Sync {
    /// Ask every question in `questions` about `state` in a single call.
    async fn decide(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError>;
}
