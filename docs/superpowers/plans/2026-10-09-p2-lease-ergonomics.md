# P2 — Lease Ergonomics (#17, #14, #24, #13) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One bad deliverable never fails a whole cohort; callers can target or filter what they lease; owners/managers can accept work without a lease; long-running work can hold a lease without 5-minute heartbeats.

**Architecture:** `Planner` methods move to request structs (`AcquireRequest`, `MarkStatusRequest`, `HeartbeatRequest`, `ForceReleaseRequest`) so this and later phases add fields without signature churn. `Cohort` gains a `blocked` list (wire: appended field). Completion logic (status → Complete, lock release, dependent promotion) is extracted into one helper shared by `mark_status` and the new `plan.accept`. TTL becomes per-call, clamped to a configurable server maximum.

**Tech Stack:** Rust 1.99.0, tokio, rusqlite, serde, rmcp.

**Spec:** `docs/superpowers/plans/2026-10-09-backlog-roadmap.md` § P2 + GitHub issues #17, #14, #24, #13.

## Global Constraints

- Wire compatibility: existing tool arguments keep working unchanged; new arguments are optional. Existing response fields keep their names/types; new response fields are appended with `#[serde(default)]`.
- Error-code prefixes in `PlannerError` messages MUST NOT change; new variants get new stable prefixes.
- Every new tool: constant + `PLAN_TOOL_NAMES` + schema in `plan_tool_definitions` + `instructions()` text + README tool table + `tests/server_integration.rs` roundtrip. Every new argument: schema + `deny_unknown_fields` struct + `instructions()` mention.
- Every state change that is not a normal lease lifecycle (accept, lockless complete, counter reset, lock override) emits an audit event.
- `CHANGELOG.md` `[Unreleased]` gets a line per user-visible change.
- `src/plan.rs`, `src/planner.rs`, `src/task.rs`, `tests/algorithm.rs` use CRLF line endings — preserve each file's endings.
- `cargo fmt --all --check`, `cargo test`, `cargo clippy --all-targets -- -D warnings` green on 1.99.0.
- Default TTL stays 5 minutes; longer leases are explicit opt-in.

## Review Focus

1. **A lapse-limited deliverable alongside healthy ready ones** — acquire returns the healthy ones and lists the limited one in `blocked`; never an error for the whole call (Task 2).
2. **Remediation actually works** — after `force_release {reset_counters: true}` the previously lapse-limited deliverable is acquirable again (Task 2).
3. **Manual deliverables** (`metadata.kind == "manual"`) — never leased by any acquire, even when requested by id (they show in `blocked` with `MANUAL`), but completable via `plan.accept` (Tasks 3–4).
4. **Accept with incomplete prerequisites / foreign live lock** — rejected (`PREREQUISITES_INCOMPLETE` / `LOCK_HELD`) unless `override_lock: true` for the lock case (Task 4).
5. **Huge / zero TTL** — `ttl_seconds` above the server max is clamped (the lock's `expires_at` shows the clamp); `0` is rejected as invalid arguments (Task 5).

---

### Task 1: Request structs for the Planner trait (pure refactor)

**Files:**
- Modify: `src/ports.rs`, `src/plan.rs` (request types), `src/planner.rs`, `src/server.rs`, `examples/plan_basic.rs`
- Modify (call sites only): `tests/planner.rs`, `tests/locks.rs`, `tests/persistence.rs`, `tests/server_integration.rs`

**Interfaces:**
- Produces, in `src/plan.rs` (all `#[derive(Debug, Clone)]`, public fields):

```rust
pub struct AcquireRequest { pub plan_id: PlanId, pub caller_id: CallerId, pub max_count: usize }
impl AcquireRequest { pub fn new(plan_id: PlanId, caller_id: CallerId, max_count: usize) -> Self }

pub struct MarkStatusRequest { pub plan_id: PlanId, pub deliverable_id: String, pub caller_id: CallerId, pub status: DeliverableStatus }
impl MarkStatusRequest { pub fn new(plan_id: PlanId, deliverable_id: impl Into<String>, caller_id: CallerId, status: DeliverableStatus) -> Self }

pub struct HeartbeatRequest { pub plan_id: PlanId, pub deliverable_id: String, pub caller_id: CallerId }
impl HeartbeatRequest { pub fn new(plan_id: PlanId, deliverable_id: impl Into<String>, caller_id: CallerId) -> Self }

pub struct ForceReleaseRequest { pub plan_id: PlanId, pub deliverable_id: String, pub reason: String }
impl ForceReleaseRequest { pub fn new(plan_id: PlanId, deliverable_id: impl Into<String>, reason: impl Into<String>) -> Self }
```

- Trait (`src/ports.rs`) becomes:

```rust
async fn acquire_cohort(&self, req: AcquireRequest) -> Result<Cohort, PlannerError>;
async fn mark_status(&self, req: MarkStatusRequest) -> Result<(), PlannerError>;
async fn heartbeat(&self, req: HeartbeatRequest) -> Result<(), PlannerError>;
async fn force_release(&self, req: ForceReleaseRequest) -> Result<(), PlannerError>;
```
  (`submit_plan`, `status`, `get_plan` unchanged.)

- [ ] **Step 1:** Add the four request types + `new` constructors to `src/plan.rs`; re-export them from `src/lib.rs` alongside the other plan types.
- [ ] **Step 2:** Change the trait and `BasicCpmPlanner` impl to take the request structs (destructure at the top of each method; bodies otherwise unchanged).
- [ ] **Step 3:** Update `src/server.rs` handlers to build requests from the parsed args, and every call site in `tests/` and `examples/` (mechanical: `planner.acquire_cohort(&p, &c, n)` → `planner.acquire_cohort(AcquireRequest::new(p.clone(), c.clone(), n))`, etc.). No test logic or assertion changes.
- [ ] **Step 4:** Run `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check` → all pass with the same test count as before (record before/after counts in the report).
- [ ] **Step 5:** CHANGELOG `[Unreleased]` → `### Changed`: "- Library: `Planner` methods `acquire_cohort`, `mark_status`, `heartbeat`, `force_release` take request structs (`AcquireRequest`, `MarkStatusRequest`, `HeartbeatRequest`, `ForceReleaseRequest`)."
- [ ] **Step 6:** Commit: `refactor(ports): Planner methods take request structs`

---

### Task 2: Lapse-limited deliverables are skipped and reported; counters can be reset (#17)

**Files:**
- Modify: `src/plan.rs` (`BlockedDeliverable`, `Cohort.blocked`, `FlatCohort.blocked`, `ForceReleaseRequest.reset_counters`, `PlannerError::LapseLimit` message), `src/planner.rs` (`acquire_cohort` step 2a, `force_release`, new audit event), `src/server.rs` (`ForceReleaseArgs.reset_counters`, schema, `instructions()`), `CHANGELOG.md`
- Test: `tests/locks.rs`, `tests/server_integration.rs`

**Interfaces:**
- Consumes: Task 1 request structs.
- Produces:

```rust
/// A deliverable the acquire considered but did not lease, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedDeliverable {
    pub id: String,
    /// Stable code: "LAPSE_LIMIT" (this task); Task 3 adds "NOT_READY", "LOCKED", "FILE_CONFLICT", "MANUAL".
    pub code: String,
    pub reason: String,
}
```
  `Cohort { plan_id, rows, blocked: Vec<BlockedDeliverable> }`; `FlatCohort` gains `#[serde(default)] blocked: Vec<BlockedDeliverable>` (serialized after `locks`).
  `ForceReleaseRequest` gains `pub reset_counters: bool` (default `false` in `new`; builder `pub fn reset_counters(mut self, yes: bool) -> Self`).
  `PlannerError::LapseLimit` message (keep prefix `LAPSE_LIMIT:`) ends with the remediation: "…; clear it with plan.force_release {reset_counters: true}". The variant is no longer returned by `acquire_cohort` but stays for wire compatibility.
  Audit event kind `"counters_reset"` with payload `{plan_id, deliverable_id, reason, previous: {lapse_count, failure_count}}`.

- [ ] **Step 1: Failing tests** (`tests/locks.rs`; reuse its fake clock + helpers to drive a deliverable to `MAX_LAPSES` lapses, exactly as existing lapse tests do):

```rust
#[tokio::test]
async fn acquire_skips_lapse_limited_deliverable_and_leases_the_rest() {
    // graph: "stuck" and "healthy", independent, disjoint files; drive "stuck" to MAX_LAPSES.
    let cohort = /* acquire max 5 */;
    assert_eq!(cohort_ids(&cohort), vec!["healthy"]);
}

#[tokio::test]
async fn acquire_reports_lapse_limited_deliverable_as_blocked() {
    let cohort = /* same setup */;
    assert_eq!(cohort.blocked.iter().map(|b| (b.id.as_str(), b.code.as_str())).collect::<Vec<_>>(), vec![("stuck", "LAPSE_LIMIT")]);
}

#[tokio::test]
async fn force_release_with_reset_counters_makes_deliverable_acquirable_again() {
    // drive "stuck" to MAX_LAPSES, force_release(reset_counters=true), acquire
    assert!(cohort_ids(&cohort).contains(&"stuck".to_string()));
}

#[tokio::test]
async fn force_release_without_reset_keeps_lapse_count() {
    // drive to MAX_LAPSES, force_release(reset_counters=false), status lapse_count still MAX_LAPSES
    assert_eq!(lapse_count_of(&status, "stuck"), MAX_LAPSES);
}

#[tokio::test]
async fn reset_counters_revives_circuit_broken_deliverable() {
    // fail "poison" MAX_ATTEMPTS times so acquire turns it Failed; force_release(reset_counters=true)
    // with no prerequisites → status Ready
    assert!(matches!(status_of(&status, "poison"), DeliverableStatus::Ready));
}

#[tokio::test]
async fn reset_counters_emits_audit_event() {
    // MemoryAuditSink: after force_release(reset_counters=true) an event of kind "counters_reset" exists for "stuck"
    assert!(events.iter().any(|e| e.kind == "counters_reset" && e.payload["deliverable_id"] == "stuck"));
}
```
  `tests/server_integration.rs`: `plan_force_release_accepts_reset_counters` — dispatch `plan.force_release` with `"reset_counters": true` on a fresh plan → returns ok (one assertion on `ok == true` per the existing `OkResponse` shape).
  Write small helpers (`cohort_ids`, `lapse_count_of`, `status_of`) in the test file if not present; adapt the audit-event field names (`kind`, `payload`) to the real `AuditEvent` struct.

- [ ] **Step 2:** Run `cargo test --test locks` → FAIL (no field `blocked`, no `reset_counters`).
- [ ] **Step 3: Implement.** In `acquire_cohort` replace step 2a's early `return Err(LapseLimit…)` with: collect every Ready, unlocked deliverable with `lapse_count >= MAX_LAPSES` into `blocked` (`code: "LAPSE_LIMIT"`, `reason: format!("lease lapsed {n} times (limit {MAX_LAPSES}); clear with plan.force_release {{reset_counters: true}}")`) and exclude those ids from the ready set in step 4. In `force_release`, when `reset_counters` is true: remember previous counts, remove the id from `lapse_counts` and `failure_counts` (not `attempt_counts`), and if the status is `Failed` set it to `Ready` when every prerequisite is `Complete`, else `Pending`; push the `counters_reset` audit event. Existing behaviour (lock removal → Ready) is unchanged.
- [ ] **Step 4:** Server: `ForceReleaseArgs` gets `#[serde(default)] reset_counters: bool`; schema property `"reset_counters": {"type": "boolean", "description": "Also clear lapse and failure counters (revives a circuit-broken deliverable)."}`; `instructions()` documents `blocked` on acquire and `reset_counters`.
- [ ] **Step 5:** `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --all --check` → PASS. Update any existing test that asserted `acquire_cohort` returns `LapseLimit` to assert the new `blocked` behaviour instead (list each in the report).
- [ ] **Step 6:** CHANGELOG `[Unreleased]`: Fixed "- One lapse-limited deliverable no longer fails `plan.acquire_cohort` for the whole plan; it is reported in the new `blocked` list (#17)." Added "- `plan.force_release` `reset_counters: true` clears lapse/failure counters and revives circuit-broken deliverables."
- [ ] **Step 7:** Commit: `fix(acquire): skip and report lapse-limited deliverables; force_release can reset counters (#17)`

---

### Task 3: Targeted and filtered acquire; manual deliverables are never leased (#14)

**Files:**
- Modify: `src/plan.rs` (`AcquireRequest.ids`, `AcquireRequest.metadata_filter`), `src/planner.rs` (`acquire_cohort`), `src/server.rs` (`AcquireCohortArgs`, schema, `instructions()`), `CHANGELOG.md`
- Test: `tests/planner.rs`, `tests/server_integration.rs`

**Interfaces:**
- Consumes: Task 2's `BlockedDeliverable` and `Cohort.blocked`.
- Produces: `AcquireRequest` gains `pub ids: Option<Vec<String>>` and `pub metadata_filter: Option<serde_json::Map<String, serde_json::Value>>` (both `None` in `new`; builders `with_ids(Vec<String>)`, `with_metadata_filter(Map)`). Wire args: `"ids": [string]`, `"filter": {"metadata": {key: value}}` (server maps `filter.metadata` → `metadata_filter`; `filter` struct is `deny_unknown_fields`).
  Semantics:
  - A deliverable is **manual** iff `metadata.kind == "manual"`. Manual deliverables are never leased.
  - `metadata_filter`: a deliverable matches iff for every `(k, v)` in the filter, `metadata.get(k) == Some(v)` (JSON equality). Non-matching deliverables are silently not considered (not `blocked`).
  - `ids`: only those deliverables are considered. An id not in the graph → `Err(DeliverableNotFound)`. Every requested id that is not leased gets a `blocked` entry with code: `"MANUAL"`, `"NOT_READY"` (status not Ready — reason names the status), `"LOCKED"` (held by a lease), `"LAPSE_LIMIT"`, `"FILE_CONFLICT"` (overlaps a held lock or an earlier pick), or `"MAX_COUNT"` (cohort already full).
  - Without `ids`, `blocked` lists only `LAPSE_LIMIT` entries (Task 2) and `MANUAL` entries are not listed (avoid noise).

- [ ] **Step 1: Failing tests** (`tests/planner.rs`; graph: `code1`, `code2` with `metadata.executor = "claude"`, `ownerTask` with `metadata.kind = "manual"`, `jun` with `metadata.executor = "junior"`, all independent, disjoint files):

```rust
#[tokio::test]
async fn unfiltered_acquire_never_leases_manual_deliverables() {
    assert!(!cohort_ids(&acquire_all(&p, &id).await).contains(&"ownerTask".to_string()));
}
#[tokio::test]
async fn metadata_filter_leases_only_matching_deliverables() {
    // filter {"executor": "claude"}
    assert_eq!(sorted(cohort_ids(&cohort)), vec!["code1", "code2"]);
}
#[tokio::test]
async fn ids_lease_only_the_requested_deliverables() {
    // ids ["jun"]
    assert_eq!(cohort_ids(&cohort), vec!["jun"]);
}
#[tokio::test]
async fn requested_manual_deliverable_is_blocked_with_manual_code() {
    // ids ["ownerTask"]
    assert_eq!(codes(&cohort), vec![("ownerTask".into(), "MANUAL".into())]);
}
#[tokio::test]
async fn requested_pending_deliverable_is_blocked_not_ready() {
    // add "later" with prerequisite "code1"; ids ["later"]
    assert_eq!(codes(&cohort), vec![("later".into(), "NOT_READY".into())]);
}
#[tokio::test]
async fn requested_locked_deliverable_is_blocked_locked() {
    // caller A leases ids ["code1"]; caller B requests ids ["code1"]
    assert_eq!(codes(&cohort_b), vec![("code1".into(), "LOCKED".into())]);
}
#[tokio::test]
async fn requesting_unknown_id_is_deliverable_not_found() {
    assert!(err.to_string().starts_with("DELIVERABLE_NOT_FOUND"));
}
#[tokio::test]
async fn requested_ids_beyond_max_count_are_blocked_max_count() {
    // ids ["code1", "code2"], max_count 1 → one leased, other blocked MAX_COUNT
    assert_eq!(cohort.blocked.iter().map(|b| b.code.as_str()).collect::<Vec<_>>(), vec!["MAX_COUNT"]);
}
```
  `tests/server_integration.rs`: `plan_acquire_cohort_accepts_ids_and_filter` — dispatch with `"ids": [...]` and `"filter": {"metadata": {...}}`; assert the returned `deliverables` ids. `plan_acquire_cohort_rejects_unknown_filter_keys` — `"filter": {"bogus": 1}` → error.
- [ ] **Step 2:** `cargo test --test planner` → FAIL.
- [ ] **Step 3: Implement** in `acquire_cohort`: after step 2b, compute the candidate set (all deliverables, or the requested ids after validating existence), drop manual ones (recording `MANUAL` when requested by id), apply `metadata_filter`, then classify requested-but-unleasable ids into `blocked` with the codes above while building the ready set; during greedy fill record `FILE_CONFLICT` / `MAX_COUNT` for requested ids that are skipped. Ordering of the lease stays `priority_key`.
- [ ] **Step 4:** Server wiring + schema: `"ids": {"type": "array", "items": {"type": "string"}}`, `"filter": {"type": "object", "properties": {"metadata": {"type": "object"}}, "additionalProperties": false}`; `instructions()` explains ids/filter/manual and the blocked codes.
- [ ] **Step 5:** Full check → PASS.
- [ ] **Step 6:** CHANGELOG Added: "- `plan.acquire_cohort` accepts `ids` and `filter.metadata`; deliverables with `metadata.kind = \"manual\"` are never leased; requested ids that cannot be leased are reported in `blocked` with a code (#14)."
- [ ] **Step 7:** Commit: `feat(acquire): target ids, filter by metadata, never lease manual deliverables (#14)`

---

### Task 4: `plan.accept` and audited lockless completion (#24)

**Files:**
- Modify: `src/plan.rs` (`AcceptRequest`, `PlannerError::PrerequisitesIncomplete`), `src/ports.rs` (`accept`), `src/planner.rs` (extract `complete_deliverable` helper; `mark_status` lockless path; `accept`), `src/server.rs` (tool), `README.md`, `CHANGELOG.md`
- Test: `tests/planner.rs`, `tests/server_integration.rs`

**Interfaces:**
- Produces:

```rust
pub struct AcceptRequest {
    pub plan_id: PlanId,
    pub deliverable_id: String,
    pub accepted_by: String,
    pub evidence: String,
    pub override_lock: bool,
}
impl AcceptRequest { pub fn new(plan_id: PlanId, deliverable_id: impl Into<String>, accepted_by: impl Into<String>, evidence: impl Into<String>) -> Self } // override_lock false
// builder: pub fn override_lock(mut self, yes: bool) -> Self
```
  `PlannerError::PrerequisitesIncomplete { plan_id: String, deliverable_id: String, missing: Vec<String> }` with message `"PREREQUISITES_INCOMPLETE: {deliverable_id} in plan {plan_id} has incomplete prerequisites [{missing joined ', '}]"`.
  Trait: `async fn accept(&self, req: AcceptRequest) -> Result<(), PlannerError>;`
  Tool `plan.accept` (`TOOL_ACCEPT`), args `{plan_id, deliverable_id, accepted_by, evidence, override_lock?}` (`deny_unknown_fields`), result `{"ok": true}`.
  Audit events: `"accepted"` payload `{plan_id, deliverable_id, accepted_by, evidence, overrode_lock_of: caller_id|null}`; `"completed_without_lease"` payload `{plan_id, deliverable_id, caller_id}`.
  Private helper in planner.rs: `fn complete_deliverable(state: &mut PlanState, deliverable_id: &str, audit_buf: &mut Vec<AuditEvent>, release_reason: &str)` — removes any lock (+ file index, + released event), sets Complete, promotes dependents. Both `mark_status(Complete)` and `accept` use it (no duplicated promotion loop).

- [ ] **Step 1: Failing tests** (`tests/planner.rs`; graph `a` → `b`, plus manual `sign` with `metadata.kind = "manual"` depending on `a`):

```rust
#[tokio::test]
async fn accept_completes_a_ready_deliverable_without_a_lease() { /* accept a → status Complete */ }
#[tokio::test]
async fn accept_promotes_dependents_to_ready() { /* accept a → b Ready */ }
#[tokio::test]
async fn accept_completes_a_manual_deliverable() { /* complete a, accept sign → Complete */ }
#[tokio::test]
async fn accept_rejects_incomplete_prerequisites() { /* accept b first → err starts_with "PREREQUISITES_INCOMPLETE" */ }
#[tokio::test]
async fn accept_rejects_foreign_live_lock_without_override() { /* worker leases a; accept a → err starts_with "LOCK_HELD" */ }
#[tokio::test]
async fn accept_with_override_releases_foreign_lock_and_completes() { /* override_lock true → a Complete */ }
#[tokio::test]
async fn accept_emits_accepted_audit_event_with_evidence() { /* MemoryAuditSink has kind "accepted" with evidence */ }
#[tokio::test]
async fn lockless_mark_complete_rejects_incomplete_prerequisites() { /* mark_status(b, Complete) with no lock, a not complete → PREREQUISITES_INCOMPLETE */ }
#[tokio::test]
async fn lockless_mark_complete_emits_completed_without_lease_event() { /* mark a Complete with no lock → event kind "completed_without_lease" */ }
```
  Each test: one assertion. `tests/server_integration.rs`: `plan_accept_roundtrip` (accept a → ok true) and `plan_accept_is_listed` (tools list contains `plan.accept`) following existing listing tests if any; otherwise just the roundtrip.
- [ ] **Step 2:** `cargo test --test planner accept` → FAIL.
- [ ] **Step 3: Implement** the helper, `accept`, and the lockless-complete checks in `mark_status` (when no lock exists and status is Complete: check prerequisites → `PrerequisitesIncomplete`; push `completed_without_lease`). Lock-held-by-caller completion path behaviour is unchanged.
- [ ] **Step 4:** Server tool wiring (constant, `PLAN_TOOL_NAMES`, schema with `required: ["plan_id","deliverable_id","accepted_by","evidence"]`, dispatch, handler), `instructions()` line `plan.accept — a manager/owner marks a deliverable Complete without a lease (audited; evidence required; override_lock to take over a live lease)`, README tool-table row `| \`plan.accept\` | Manager/owner acceptance: complete a deliverable without holding its lease (audited, with evidence). |`.
- [ ] **Step 5:** Full check → PASS.
- [ ] **Step 6:** CHANGELOG Added "- `plan.accept` for manager/owner acceptance without a lease (#24)." Changed "- Completing a deliverable without a lease now requires its prerequisites to be complete and is audited."
- [ ] **Step 7:** Commit: `feat(accept): plan.accept for lease-free acceptance; audited lockless completion (#24)`

---

### Task 5: Per-call lease TTL with a server maximum (#13)

**Files:**
- Modify: `src/plan.rs` (`AcquireRequest.ttl`, `HeartbeatRequest.ttl`), `src/planner.rs` (`max_ttl`, `with_max_ttl`, effective TTL), `src/server.rs` (`ttl_seconds` args/schema/instructions), `src/bin/server.rs` (`CPM_MAX_TTL_SECS`), `README.md` (env var), `CHANGELOG.md`
- Test: `tests/locks.rs`, `tests/server_integration.rs`

**Interfaces:**
- Produces: `AcquireRequest.ttl: Option<std::time::Duration>` and `HeartbeatRequest.ttl: Option<std::time::Duration>` (builders `with_ttl(Duration)`); `pub const DEFAULT_MAX_TTL: Duration = Duration::from_secs(8 * 60 * 60);` and `BasicCpmPlanner::with_max_ttl(self, Duration) -> Self` in planner.rs. Effective TTL = `min(req.ttl.unwrap_or(self.ttl), self.max_ttl)`. Wire: `"ttl_seconds": integer >= 1` on `plan.acquire_cohort` and `plan.heartbeat`; `0` → invalid-params MCP error from the server layer. `CPM_MAX_TTL_SECS` env (positive integer) configures the max in `bin/server.rs`; invalid values abort startup with a clear message.

- [ ] **Step 1: Failing tests** (`tests/locks.rs`, fake clock):

```rust
#[tokio::test]
async fn acquire_with_ttl_sets_lock_expiry() { /* ttl 2h → lock.expires_at == now + 2h */ }
#[tokio::test]
async fn acquire_ttl_above_max_is_clamped() { /* planner.with_max_ttl(1h); ttl 5h → expires_at == now + 1h */ }
#[tokio::test]
async fn heartbeat_with_ttl_extends_to_requested_duration() { /* heartbeat ttl 3h → expires_at == now + 3h */ }
#[tokio::test]
async fn lease_with_long_ttl_survives_past_default_ttl() { /* ttl 1h; advance clock 30 min; acquire again → deliverable still locked (not reaped) */ }
```
  `tests/server_integration.rs`: `plan_acquire_cohort_rejects_zero_ttl` (`"ttl_seconds": 0` → error) and `plan_heartbeat_accepts_ttl_seconds` (→ ok).
- [ ] **Step 2:** `cargo test --test locks ttl` → FAIL.
- [ ] **Step 3: Implement** planner + server + bin wiring. `instructions()`: "Leases default to 5 minutes. Pass ttl_seconds (≤ server max, default 8h) on acquire/heartbeat for long-running work, and heartbeat at least every ttl/3."
- [ ] **Step 4:** README: document `CPM_MAX_TTL_SECS` next to `CPM_PLANNER_DB`.
- [ ] **Step 5:** Full check → PASS.
- [ ] **Step 6:** CHANGELOG Added "- `ttl_seconds` on `plan.acquire_cohort` and `plan.heartbeat`, clamped to `CPM_MAX_TTL_SECS` (default 8h) (#13)."
- [ ] **Step 7:** Commit: `feat(lease): per-call ttl_seconds clamped to a server maximum (#13)`
