---
name: cpm-run
description: Use when executing the selected variant of a cpm-planner plan (plan.* tools); when an agent should claim ready deliverables, take or renew a lease, continue a thread, report progress, record actual hours or evidence, mark work complete or failed, accept owner, gate or manual work, ask why a deliverable is blocked or what to do next, or recover a stalled plan (lease lapses, circuit breaker, needs_operator). Covers plan.overview, plan.explain, plan.acquire_cohort (ids, filter, thread, affinity_seconds, ttl_seconds, blocked), plan.heartbeat, plan.mark_status (earned_pct, actual_effort_hours, evidence records), plan.accept, lockless marks, plan.force_release and plan.status.
---

# cpm-run: lease, work, report, accept

Execute the selected variant's `plan_id` one cohort at a time. The full method is in
[the deliverable-cpm skill](../deliverable-cpm/SKILL.md) (section 7).
Summarises deliverable-cpm; if they ever disagree, the server's behaviour wins.

Calling the tools: see [Calling the tools](../deliverable-cpm/SKILL.md#calling-the-tools)
(registered MCP server or a host wrapper such as `junior tools call`).

## When to use

- The variant is selected (`cpm-improve`, or the only variant) and, for earned value,
  baselined (`cpm-ev`). Other variants refuse execution with `VARIANT_NOT_SELECTED`.
- Not for: changing what a deliverable is or adding work → use cpm-revise.

**Selecting the plan.** Every call takes exactly one of `plan_id`, `plan` (a line name, resolved to
its selected variant) or `active: true` (the project's single live line), plus optional `project`
for `plan` and `active`. Find the plan with `{"active": true}` or `{"plan": "<line>"}`, then keep the
`plan_id` that `plan.acquire_cohort` returned: `active` moves (or is `AMBIGUOUS_ACTIVE_PLAN`) when
another line goes live. `NO_ACTIVE_PLAN` means no live line; an archived id refuses with `ARCHIVE_REFUSED`.

## 0. Where is the plan, and what next?

```text
plan.overview {"plan": "<line>", "filter": {"owner": "agent"}}
plan.explain  {"plan": "<line>", "deliverable_id": "<id>"}
```

- `plan.overview` (read-only, one `as_of`) gives `counts`, `ev`, `critical_path`, `milestones`,
  `ready` (each with `action`: `acquire`, `accept` or `operator`, and `file_conflicts`), `blocked`
  (`class` and `root_blockers`), `leases`, `attention` and each entry's `evidence` verdict
  (`current`, `stale`, `unknown`, `unbound`, `none`). `format: "markdown"` for people; `detail: "full"`.
- `plan.explain` with a `deliverable_id` says why it is not moving (`state`, `blocked.chain`, the
  root blocker and its `action`), what completing it `unlocks`, and `critical_path_impact`.
  Without one it returns `next_work`: `ready_critical` first, then `ready_slack`, `blocked_*`,
  `file_conflicting`. Act on root blockers, not on the blocked item.

## 1. Claim a cohort

```text
plan.acquire_cohort {"plan_id": "<plan_id>", "caller_id": "agent-7", "max_count": 3,
                     "filter": {"metadata": {"owner": "agent"}}, "ttl_seconds": 1800,
                     "thread": "evidence", "affinity_seconds": 600}
```

- Required: `plan_id`, `caller_id` (stable per worker: thread handoffs are keyed by it).
- Optional: `max_count` (default: the number of `ids`, else 1), `ids` (unknown id is
  `DELIVERABLE_NOT_FOUND`), `filter: {"metadata": {key: value}}` (non-matching ids are ignored),
  `ttl_seconds` (default 300, clamped), `thread` (prefer that `metadata.thread`; a bad token is
  `INVALID_VALUE`), `affinity_seconds` (default 0: hold back another caller's thread for this long
  after its last completion).
- Without `thread`, the threads you last advanced come first, then unthreaded work, then other
  callers' threads. Affinity only reorders; it never bypasses files, prerequisites or limits.
- `blocked` lists each id not leased as `{id, code, reason}`: `MANUAL`, `NOT_READY`, `LOCKED`,
  `LAPSE_LIMIT`, `FILE_CONFLICT`, `MAX_COUNT` or `AFFINITY_HOLD` (names the holder and seconds left;
  take other work or retry later).
- `exhausted: true` with `needs_operator: true` means stalled on lapse-limited work, not finished.
  Fix the environment, then `plan.force_release {plan_id, deliverable_id, reason, reset_counters: true}`.
- The first acquire or mark with no baseline may take one (`baseline_taken`); with
  `CPM_BASELINE_MODE=refuse` it fails with `BASELINE_REQUIRED`.

## 2. Keep the lease

`plan.heartbeat {plan_id, deliverable_id, caller_id, ttl_seconds}` at least every ttl/3. A lapsed
lease counts toward the lapse limit (10). `LOCK_EXPIRED` or `LOCK_NOT_HELD`: stop and re-acquire.

## 3. Report progress and close

```text
plan.mark_status {"plan_id": "<plan_id>", "deliverable_id": "<id>", "caller_id": "agent-7",
                  "status": {"status": "complete"}, "actual_effort_hours": 3.5,
                  "evidence": {"schema_version": 1, "scope": "focused",
                    "summary": "cargo nextest run --test export_service: 14 passed",
                    "identity": {"schema_version": 1, "tree": "git-tree:<hex>",
                      "dependency_lock": "sha256:<hex>", "toolchain": "rustc 1.89.0",
                      "test_contract": "nextest:export_service@<digest>", "environment": "linux-x86_64"}}}
```

- `status`: `in_progress`, `complete`, `ready` or `{"status": "failed", "reason": "..."}`.
  Complete and failed release the lock. `earned_pct` (0..100) only with `in_progress`.
- `actual_effort_hours`: the total so far, not a delta; replaces leased hours as AC.
- `evidence` is a note string (up to 2048 chars) or a record: `scope`, `summary`, optional `ref`,
  and `identity` (`tree` from `git rev-parse HEAD^{tree}`, lockfile hash, tool versions, test
  selection, environment). At most 100 entries; past that a mark carrying evidence is `INVALID_ACTUALS`.
- Work closes with a `focused` record (or `gate`). A declared `metadata.evidence_scope` must be met.
- Run only `metadata.verify` (returned with the lease). Never push, open a PR or run the full suite
  in a work deliverable. Mark `complete` only when the artifact meets its acceptance criteria.
- A `failed` mark leaves it Failed; retry with a lockless `{"status": "ready"}`. Three failures
  trip the circuit breaker; revive with `plan.force_release {..., reset_counters: true}`.

## 4. Gates, owner and manual work (no lease)

`metadata.kind: "manual"` and `"gate"` are never leased. Close them with `plan.accept`:

```text
plan.accept {"plan_id": "<plan_id>", "deliverable_id": "<id>", "accepted_by": "matthew",
             "evidence": {"schema_version": 1, "scope": "manual_approval",
                          "summary": "approved docs/export-api.md at rev 3"}}
```

- Pick the scope by who decides: `manual_approval` for an owner inside the team;
  `external_signoff` with `party` for someone outside it; `gate` for a full regression or CI run
  (identity required, all five fields); `merge`, `publish`, `activation` for those steps.
- A gate with `metadata.candidate` needs a record `current` against the candidate's newest identity:
  re-run on the tree the candidate recorded, not a later one.
- `override_lock: true` takes over a live lease; only when the holder is gone. `plan.accept` records
  no hours: follow it with a lockless `plan.mark_status` (`complete`, `actual_effort_hours`).
- Start a gate only when it is ready: a lockless `in_progress`, one run, other work meanwhile. A red
  run is not a `failed` mark: add it as evidence on a lockless `in_progress`, fix every failure with
  focused checks, re-run once. After 2 red runs or a scope change, use `cpm-revise`.

Evidence refusals roll back the whole call: `INVALID_EVIDENCE` (malformed record or identity),
`EVIDENCE_SCOPE_MISMATCH` (wrong scope for the kind), `EVIDENCE_REQUIRED` (declared scope not met;
strings never count), `EVIDENCE_STALE` (gate record differs from its candidate; read `changed`).

## Lockless marks

A `plan.mark_status` without a lease is allowed and audited. It needs complete prerequisites
(`PREREQUISITES_INCOMPLETE` otherwise). A lockless `in_progress` persists across restarts, is not
leasable and is untouched by `plan.force_release`; hand it back with a lockless `ready`.

## Loop and stop

1. `plan.explain` for `next_work`, then `plan.acquire_cohort`; if nothing was leased, read `blocked`.
2. Do the work, heartbeating; mark `complete` with a record, or `failed` with a reason. A subagent
   gets the artifact, `owned_files`, `metadata.verify`, the branch and the no-heavy-work rule. Keep
   one agent on a thread: the same `caller_id` gets its next step first.
3. Accept manual work and gates as they become ready (`action: "accept"`).
4. Repeat until `plan_complete: true`, or you are blocked on a person, a lapse limit or a failure
   that needs a decision. Report which, with the `plan.explain` chain.

Take snapshots at a steady cadence (`cpm-ev`). If the definition must change, use `cpm-revise`.

**Adapt at natural pauses.** When a cohort closes or a gate returns, check: SPI or CPI below 0.9, a
gate over 1.5 times its estimate, a moved critical path, a 50% overrun, stale evidence in the
overview, or ready work waiting only on capacity. On a signal, run `cpm-improve` on remaining work;
never interrupt a lease or running gate.

## Pitfalls

- Planner refusals come back as JSON-RPC `-32603` internal errors; read the code in the message.
- A stale verdict is reported, never acted on: it reopens nothing. Re-run, or use `cpm-revise`.
- `plan.accept` needs the second lockless mark for AC; `actual_effort_hours` is cumulative.
