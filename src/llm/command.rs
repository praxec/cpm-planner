//! A delegated `plan.review` judge: an operator-configured adapter process.
//!
//! `CPM_REVIEW_PROVIDER` selects who answers `plan.review`:
//!
//! - unset or `direct`: this process calls OpenRouter with its own key;
//! - `command`: the executable at `CPM_REVIEW_COMMAND` answers, so the
//!   credentials can live only inside that operator-supplied process;
//! - `disabled`: review reports `review_unavailable`.
//!
//! A configured provider never falls back to another one.
//!
//! # Adapter protocol, version 1
//!
//! The adapter gets one JSON object on stdin ([`AdapterRequest`]) and writes
//! one on stdout ([`AdapterReply`]):
//!
//! ```json
//! {"schema_version":1,"kind":"plan.review.judgment","state":{},"questions":{}}
//! {"schema_version":1,"status":"ok","model":"m","answers":{},"usage":null}
//! {"schema_version":1,"status":"refused","reason":"policy_denied"}
//! ```
//!
//! `docs/review-provider.md` is the published contract, and
//! `docs/review-provider/fixtures/` holds conformance examples.
//!
//! # Secret boundary
//!
//! The request holds only the review state and questions. The child's
//! environment is cleared, then given the platform basics and the variables
//! named in `CPM_REVIEW_COMMAND_ENV`; the OpenRouter key variables can never
//! be named. Stderr is discarded and no free text from the adapter reaches
//! the result: `reason` and `model` are dropped unless they are short tokens
//! with no key-shaped run of characters (see [`safe_token`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    Answer, ConfigError, DEFAULT_TIMEOUT_SECS, Decisions, JudgmentError, JudgmentErrorKind,
    JudgmentModel, LLM_TIMEOUT_ENV, OPENROUTER_API_KEY_ENV, OPENROUTER_KEY_FILE_ENV, Question,
    TIMEOUT_SECS_RANGE, Usage,
};

/// Environment variable selecting the review provider.
pub const REVIEW_PROVIDER_ENV: &str = "CPM_REVIEW_PROVIDER";
/// Environment variable holding the adapter's absolute path.
pub const REVIEW_COMMAND_ENV: &str = "CPM_REVIEW_COMMAND";
/// Environment variable naming extra variables to pass to the adapter.
pub const REVIEW_COMMAND_ENV_ENV: &str = "CPM_REVIEW_COMMAND_ENV";
/// The adapter protocol version.
pub const SCHEMA_VERSION: u32 = 1;
/// Largest adapter reply accepted, in bytes.
pub const MAX_REPLY_BYTES: u64 = 16 * 1024 * 1024;
/// Longest adapter token (`reason`, `model`) echoed into a review.
pub const MAX_TOKEN_CHARS: usize = 64;
/// Longest run of letters and digits a token may hold between separators.
/// A longer run looks like a key or a digest, so the token is dropped.
pub const MAX_TOKEN_SEGMENT_CHARS: usize = 23;
/// The request `kind` of protocol version 1.
pub const REQUEST_KIND: &str = "plan.review.judgment";
/// The provider name recorded in a review an adapter answered.
pub const PROVIDER_NAME: &str = "command";
/// What every child needs to start on any platform.
const BASE_ENV: [&str; 7] = [
    "PATH",
    "HOME",
    "USERPROFILE",
    "SYSTEMROOT",
    "TMP",
    "TEMP",
    "TMPDIR",
];

/// The review provider selected by the environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewProvider {
    Direct,
    Command(CommandConfig),
    Disabled,
}

/// A validated adapter configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandConfig {
    pub path: PathBuf,
    /// Variable names forwarded to the adapter, besides the platform basics.
    pub forward_env: Vec<String>,
    pub timeout: Duration,
}

impl ReviewProvider {
    /// Read the provider from the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Read the provider through `lookup`, so tests need no process state.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        match lookup(REVIEW_PROVIDER_ENV).as_deref().map(str::trim) {
            None | Some("") | Some("direct") => Ok(Self::Direct),
            Some("disabled") => Ok(Self::Disabled),
            Some("command") => CommandConfig::from_lookup(lookup).map(Self::Command),
            Some(_) => Err(ConfigError::InvalidReviewProvider {
                reason: "must be direct, command or disabled",
            }),
        }
    }
}

impl CommandConfig {
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let path = lookup(REVIEW_COMMAND_ENV)
            .filter(|value| !value.trim().is_empty())
            .ok_or(ConfigError::InvalidReviewProvider {
                reason: "command needs CPM_REVIEW_COMMAND",
            })?;
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(ConfigError::InvalidReviewProvider {
                reason: "CPM_REVIEW_COMMAND must be an absolute path",
            });
        }
        let forward_env: Vec<String> = lookup(REVIEW_COMMAND_ENV_ENV)
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();
        if forward_env
            .iter()
            .any(|name| name == OPENROUTER_API_KEY_ENV || name == OPENROUTER_KEY_FILE_ENV)
        {
            return Err(ConfigError::InvalidReviewProvider {
                reason: "CPM_REVIEW_COMMAND_ENV must not forward the OpenRouter key",
            });
        }
        let timeout = match lookup(LLM_TIMEOUT_ENV) {
            None => DEFAULT_TIMEOUT_SECS,
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .ok()
                .filter(|secs| TIMEOUT_SECS_RANGE.contains(secs))
                .ok_or(ConfigError::InvalidTimeout)?,
        };
        Ok(Self {
            path,
            forward_env,
            timeout: Duration::from_secs(timeout),
        })
    }
}

/// The one request an adapter reads on stdin (protocol version 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterRequest {
    /// Always [`SCHEMA_VERSION`].
    pub schema_version: u32,
    /// Always [`REQUEST_KIND`].
    pub kind: String,
    /// The plan's review state, as a direct review sends it.
    pub state: serde_json::Value,
    /// The typed questions, keyed `q00`, `q01`, ...
    pub questions: BTreeMap<String, Question>,
}

/// The one reply an adapter writes on stdout (protocol version 1).
///
/// A reply whose `schema_version` is not [`SCHEMA_VERSION`] decodes here but
/// the judge rejects it as a `decode` failure.
// Decoded and consumed at once, so the size gap between variants costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AdapterReply {
    /// The adapter answered.
    Ok {
        schema_version: u32,
        /// The model that answered; kept only when it is a [`safe_token`].
        #[serde(default)]
        model: Option<String>,
        /// One answer per question id.
        answers: BTreeMap<String, Answer>,
        /// Token accounting, when the adapter reports it.
        #[serde(default)]
        usage: Option<Usage>,
    },
    /// The adapter declined to answer.
    Refused {
        schema_version: u32,
        /// Why; kept only when it is a [`safe_token`].
        #[serde(default)]
        reason: Option<String>,
    },
}

/// `text` when it is a short identifier-like token: at most
/// [`MAX_TOKEN_CHARS`] letters, digits and `._:/-`, with no run of
/// letters and digits longer than [`MAX_TOKEN_SEGMENT_CHARS`]. Such a token
/// cannot carry a sentence, a log line or a key-shaped value.
pub fn safe_token(text: &str) -> Option<&str> {
    let separator = |c: char| "._:/-".contains(c);
    (!text.is_empty()
        && text.chars().count() <= MAX_TOKEN_CHARS
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || separator(c))
        && text
            .split(separator)
            .all(|segment| segment.chars().count() <= MAX_TOKEN_SEGMENT_CHARS))
    .then_some(text)
}

/// A [`JudgmentModel`] answered by the configured adapter process.
#[derive(Debug, Clone)]
pub struct CommandJudge {
    config: CommandConfig,
}

impl CommandJudge {
    pub fn new(config: CommandConfig) -> Self {
        Self { config }
    }

    async fn exchange(&self, request: Vec<u8>) -> Result<Vec<u8>, JudgmentError> {
        let mut command = tokio::process::Command::new(&self.config.path);
        command
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for name in BASE_ENV
            .iter()
            .copied()
            .chain(self.config.forward_env.iter().map(String::as_str))
        {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().map_err(|_| {
            JudgmentError::new(JudgmentErrorKind::Transport, "review adapter did not start")
        })?;
        let mut stdin = child.stdin.take().ok_or_else(|| {
            JudgmentError::new(JudgmentErrorKind::Transport, "review adapter has no stdin")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            JudgmentError::new(JudgmentErrorKind::Transport, "review adapter has no stdout")
        })?;
        // Written alongside the read: an adapter may fill its stdout first or
        // exit without reading, and neither may deadlock or fail this write.
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(&request).await;
            let _ = stdin.shutdown().await;
        });
        let mut reply = Vec::new();
        let mut stdout = stdout.take(MAX_REPLY_BYTES + 1);
        stdout.read_to_end(&mut reply).await.map_err(|_| {
            JudgmentError::new(JudgmentErrorKind::Transport, "review adapter stdout failed")
        })?;
        // Checked before the exit status: an adapter still writing would block
        // on the full pipe, or die of SIGPIPE once the pipe closes.
        if reply.len() as u64 > MAX_REPLY_BYTES {
            writer.abort();
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(JudgmentError::new(
                JudgmentErrorKind::Decode,
                "review adapter reply is too large",
            ));
        }
        drop(stdout);
        let status = child.wait().await.map_err(|_| {
            JudgmentError::new(JudgmentErrorKind::Transport, "review adapter was lost")
        })?;
        writer.abort();
        if !status.success() {
            let code = status
                .code()
                .map_or_else(|| "a signal".to_string(), |code| format!("code {code}"));
            return Err(JudgmentError::new(
                JudgmentErrorKind::Upstream,
                format!("review adapter exited with {code}"),
            ));
        }
        Ok(reply)
    }
}

#[async_trait]
impl JudgmentModel for CommandJudge {
    async fn decide(
        &self,
        state: serde_json::Value,
        questions: BTreeMap<String, Question>,
    ) -> Result<Decisions, JudgmentError> {
        let request = serde_json::to_vec(&AdapterRequest {
            schema_version: SCHEMA_VERSION,
            kind: REQUEST_KIND.to_string(),
            state,
            questions,
        })
        .map_err(|_| {
            JudgmentError::new(
                JudgmentErrorKind::InvalidRequest,
                "review request does not serialize",
            )
        })?;
        // Dropping the exchange on timeout kills the child (`kill_on_drop`).
        let reply = tokio::time::timeout(self.config.timeout, self.exchange(request))
            .await
            .map_err(|_| {
                JudgmentError::new(JudgmentErrorKind::Timeout, "review adapter timed out")
            })??;
        let malformed = || {
            JudgmentError::new(
                JudgmentErrorKind::Decode,
                "review adapter reply is not schema version 1",
            )
        };
        match serde_json::from_slice::<AdapterReply>(&reply).map_err(|_| malformed())? {
            AdapterReply::Ok {
                schema_version: SCHEMA_VERSION,
                model,
                answers,
                usage,
            } => Ok(Decisions {
                answers,
                model: model
                    .as_deref()
                    .and_then(safe_token)
                    .unwrap_or("unknown")
                    .to_string(),
                usage,
            }),
            AdapterReply::Refused {
                schema_version: SCHEMA_VERSION,
                reason,
            } => Err(JudgmentError::new(
                JudgmentErrorKind::Refused,
                format!(
                    "review adapter refused: {}",
                    reason
                        .as_deref()
                        .and_then(safe_token)
                        .unwrap_or("unspecified")
                ),
            )),
            _ => Err(malformed()),
        }
    }

    fn endpoint_host(&self) -> Option<String> {
        Some("command-adapter".to_string())
    }

    fn provider(&self) -> &'static str {
        PROVIDER_NAME
    }
}

fn check(name: &str, present: bool) -> serde_json::Value {
    serde_json::json!({ "name": name, "present": present })
}

/// A names-only readiness report: which settings are present and whether the
/// provider could run. It never holds a value or a path.
pub fn readiness(lookup: impl Fn(&str) -> Option<String>) -> serde_json::Value {
    let present = |name: &str| lookup(name).is_some_and(|v| !v.trim().is_empty());
    match ReviewProvider::from_lookup(&lookup) {
        Err(err) => serde_json::json!({
            "provider": "invalid",
            "ready": false,
            "reason": err.review_reason(),
            "checks": [check(REVIEW_PROVIDER_ENV, present(REVIEW_PROVIDER_ENV))],
        }),
        Ok(ReviewProvider::Disabled) => serde_json::json!({
            "provider": "disabled",
            "ready": false,
            "reason": "disabled by CPM_REVIEW_PROVIDER",
            "checks": [check(REVIEW_PROVIDER_ENV, true)],
        }),
        Ok(ReviewProvider::Direct) => {
            let ready = present(OPENROUTER_API_KEY_ENV) || present(OPENROUTER_KEY_FILE_ENV);
            serde_json::json!({
                "provider": "direct",
                "ready": ready,
                "reason": if ready { None } else { Some("no OpenRouter key configured") },
                "checks": [
                    check(OPENROUTER_API_KEY_ENV, present(OPENROUTER_API_KEY_ENV)),
                    check(OPENROUTER_KEY_FILE_ENV, present(OPENROUTER_KEY_FILE_ENV)),
                ],
            })
        }
        Ok(ReviewProvider::Command(config)) => {
            let is_file = Path::new(&config.path).is_file();
            let mut checks = vec![check(REVIEW_COMMAND_ENV, true)];
            let mut missing = false;
            for name in &config.forward_env {
                let found = present(name);
                missing |= !found;
                checks.push(check(name, found));
            }
            let reason = if !is_file {
                Some("CPM_REVIEW_COMMAND is not a file")
            } else if missing {
                Some("a variable named by CPM_REVIEW_COMMAND_ENV is unset")
            } else {
                None
            };
            serde_json::json!({
                "provider": "command",
                "ready": reason.is_none(),
                "reason": reason,
                "checks": checks,
            })
        }
    }
}
