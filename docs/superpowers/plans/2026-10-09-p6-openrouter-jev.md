# P6 — OpenRouter Integration: rig + Jev, `plan.review` (#26) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** When an OpenRouter key is configured, `plan.review` returns deterministic lint findings plus Jev's calibrated judgments (false/missing dependencies, split candidates, crash techniques) with probabilities, and mechanically-verified proposals (`plan.simulate` hours saved, ranked by hours saved ÷ cost) that `plan.fork { edits }` can apply. Without a key it returns `review_unavailable` with the lint findings — never a silent fallback. Reasoning about improvements stays with the calling agent (skill, P7b).

**Architecture:** `src/llm/mod.rs` holds config (`LlmConfig` from env) and a `JudgmentModel` trait (`async fn decide(&self, state: serde_json::Value, questions: BTreeMap<String, rig_typesafeai::Question>) -> Result<Decisions, JudgmentError>`) so `src/review.rs` is testable with a fake. `src/llm/jev.rs` implements it with `rig_typesafeai::{JevConfig, DynamicQuery, Evaluate}` pointed at OpenRouter (`JevConfig::new(key).model(model).with_endpoint(endpoint)`), and `src/llm/openrouter.rs` builds the rig-core OpenRouter chat client (`rig_core::providers::openrouter`) for future headless use (constructed and health-checked, not wired to a tool). `src/review.rs` composes lint → candidate generation (deterministic pre-filtering) → one batched Jev call → proposals → `simulate` verification → ranking.

**Tech Stack:** Rust 1.99.0; `rig-core = "0.44"` (`default-features = false, features = ["reqwest", "rustls"]`), `rig-typesafeai = "0.44"` (`default-features = false, features = ["reqwest", "rustls"]`); tokio; a mock HTTP server for tests (`httpmock` or `wiremock`, dev-dependency).

**Spec:** roadmap § P6 (decisions: OpenRouter via rig standard; Jev via rig-typesafeai; reasoning lives in the skill; server = lint + Jev scores + simulate/compare) + issue #26.

## Global Constraints

- Config: `OPENROUTER_API_KEY` or `CPM_OPENROUTER_KEY_FILE` (file contents trimmed; file must not be world-readable on unix → else startup warning and key ignored); `CPM_JEV_MODEL` (default `typesafe/jev-1.13`); `CPM_JEV_ENDPOINT` (default `https://openrouter.ai/api/v1/systemone`); `CPM_LLM_TIMEOUT_SECS` (default 30, 1..=300); `CPM_LLM_MODEL` (generative, default `openai/gpt-5-mini`, unused by tools).
- The key is never logged, never included in errors/audit/tool output; `Debug` impls redact it.
- `plan.review` is read-only (no store writes); it is always listed; with no key it returns `{ status: "review_unavailable", reason: "no OpenRouter key configured", lint }`; on HTTP/timeout/decode failure `{ status: "review_unavailable", reason: "<class>: <safe message>", lint }`.
- Jev is called at most once per review (batched questions; ≤ 64 questions; deterministic pre-filtering ranks candidates and truncates with a `truncated: true` flag).
- Deterministic lint errors short-circuit: if lint has errors, no Jev call; return lint with `status: "invalid_graph"`.
- Every proposal is a list of `GraphEdit`s (P4P `src/edits.rs`), verified by `simulate` on the edited graph (must lint clean); `hours_saved = makespan(before) − makespan(after)` (resource makespan when capacities given); `cost` = Σ added effort hours (crash) or 0 (edge removal); ranked by `hours_saved / max(cost, 1)` desc, then id.
- Output records `model`, `endpoint` host, `question_count`, `usage` tokens, `prompt_hash` (sha256 of the canonical request JSON), `jev_called: bool`.
- Tool count/docs/README/CHANGELOG/instructions() updated; server tests use the fake or the mock server — never the network. One `#[ignore]` live smoke test runs against OpenRouter when the key is present.
- CRLF files keep CRLF; new files LF; fmt/test/clippy -D warnings green.

## Review Focus
1. No key → `review_unavailable` + lint, fast, no network.
2. OpenRouter 401/429/500/timeout → `review_unavailable` with a classed reason and no key leakage.
3. Lint errors → no Jev call.
4. A proposal that makes the graph invalid is dropped (never returned).
5. Large graph → ≤ 64 questions, `truncated: true`.

---

### Task 0 (gate): Live spike
With the user-provided key: `cargo run --example jev_openrouter_smoke` (new `examples/jev_openrouter_smoke.rs`) builds `JevConfig::new(key).model("typesafe/jev-1.13").with_endpoint("https://openrouter.ai/api/v1/systemone").client()`, asks one `Noul` ("Is this dependency real?") with a tiny state, prints the answer + usage. Record the result (and any wire incompatibility) in the report. If OpenRouter rejects rig's wire format, implement `src/llm/jev.rs` against `POST https://openrouter.ai/api/alpha/decisions` with the same `Question`/`Answer` types (serde JSON identical to rig-typesafeai's `types.rs`), and file the incompatibility in the report for an upstream issue.

### Task 1: Config + `JudgmentModel` + Jev client
`src/llm/mod.rs`: `LlmConfig::from_env() -> Result<Option<LlmConfig>, ConfigError>` (None = no key), redacted Debug; `JudgmentModel` trait; `Decisions { answers: BTreeMap<String, rig_typesafeai::types::Answer>, model: String, usage: Option<Usage> }`; `JudgmentError { kind: Unauthorized|RateLimited|Timeout|Upstream|Decode|Transport, message }` (messages never contain the key). `src/llm/jev.rs`: `JevJudge::new(&LlmConfig)` using `DynamicQuery` with a `tokio::time::timeout`. `src/llm/openrouter.rs`: `fn chat_client(&LlmConfig) -> rig_core::providers::openrouter::Client` (constructed lazily; doc-only use). Tests with a mock server: success decodes answers; 401 → Unauthorized; 429 → RateLimited; 500 → Upstream; slow response → Timeout; key absent from every error string and Debug output; key file permission check (unix).

### Task 2: `src/review.rs`
`ReviewRequest { capacities: Option<ScheduleRequest>, max_questions: Option<u16> (≤ 64) }` (`deny_unknown_fields`); `ReviewReport { status: "ok"|"review_unavailable"|"invalid_graph", reason: Option<String>, lint: LintReport, findings: Vec<ReviewFinding>, proposals: Vec<Proposal>, truncated: bool, jev_called: bool, model: Option<String>, usage: Option<Usage>, prompt_hash: Option<String>, question_count: usize }`; `ReviewFinding { kind: "false_dependency"|"missing_dependency"|"split_candidate"|"interface_split"|"crash_option", ids: Vec<String>, probability: f32, evidence: String }`; `Proposal { id: String, kind, edits: Vec<GraphEdit>, hours_saved: f32, cost: f32, score: f32, rationale: String }`. Candidate generation (deterministic): every edge → `false_dependency` Noul (state: both deliverables' descriptions/artifacts/consumes); pairs (a, b) not ordered, sharing a path prefix / metadata.owner / mention of the other's id in description → `missing_dependency` Noul (top-K by heuristic score); deliverables with effort ≥ 2× median or ≥ 8h → `split_candidate` Score (5 levels); critical-path deliverables → `crash_option` Choice {add_capacity, fast_track, reduce_scope, none}. Proposals: false_dependency with p ≥ 0.8 → RemoveEdge; crash_option add_capacity/reduce_scope with confidence ≥ 0.6 → SetEffort to 70% (cost = 0 for reduce_scope, +30% effort for add_capacity…) — exact numbers documented in the module. Verify each proposal with `simulate`; drop failures. Splits/missing deps are findings only (the agent designs them).
Tests (fake judge): no key path; lint-error short-circuit (fake records zero calls); question cap + truncation; false-dependency proposal verified and ranked; invalid proposal dropped; prompt_hash stable across runs; deterministic output for a fixed fake.

### Task 3: `plan.review` tool + docs
Tool `plan.review { graph | plan_id | path, capacities?, max_questions? }`; server holds `Option<Arc<dyn JudgmentModel>>` built at startup from `LlmConfig::from_env()`; handler runs review on `spawn_blocking`/async without holding the store. Docs: instructions() (what Jev does, probabilities are advisory, apply via `plan.fork { edits }`), README (env vars, cost note: Jev ≈ $0.042 per million input tokens on OpenRouter), CHANGELOG. Server tests with an injected fake judge; `#[ignore]` live test `review_against_openrouter_live` reading `OPENROUTER_API_KEY`.
