---
name: cpm-run
description: Use when executing the selected variant of a cpm-planner plan (plan.* tools); when an agent should claim ready deliverables, take or renew a lease, report progress, record actual hours or evidence, mark work complete or failed, accept owner or manual work, or recover a stalled plan (lease lapses, circuit breaker, needs_operator). Covers plan.acquire_cohort (ids, filter, ttl_seconds, blocked), plan.heartbeat, plan.mark_status (earned_pct, actual_effort_hours, evidence), plan.accept, lockless marks, plan.force_release and plan.status.
---

# cpm-run: lease, work, report, accept

Execute the selected variant's `plan_id` one cohort at a time. The full method is in
[the deliverable-cpm skill](../deliverable-cpm/SKILL.md) (section 7).
Summarises deliverable-cpm; if they ever disagree, the server's behaviour wins.

## When to use

- The variant is selected (`cpm-improve`, or the only variant) and, for earned value,
  baselined (`cpm-ev`). Only the selected variant executes; others refuse with
  `VARIANT_NOT_SELECTED`.
- `plan.status {plan_id}` shows the `ready` set, `plan_complete` and `locks_held`.
- Not for: changing what a deliverable is or adding work → use cpm-revise.

## 1. Claim a cohort

```text
plan.acquire_cohort {"plan_id": "<plan_id>", "caller_id": "agent-7", "max_count": 3,
                     "filter": {"metadata": {"owner": "agent"}}, "ttl_seconds": 1800}
```

- Required: `plan_id`, `caller_id` (stable per worker), `max_count`.
- Optional: `ids: [...]` (only those; unknown id is `DELIVERABLE_NOT_FOUND`),
  `filter: {"metadata": {key: value}}` (every pair must match; ids that don't match are
  ignored, not reported), `ttl_seconds` (default 300, clamped to the server maximum).
- It hands out ready deliverables whose `owned_files` don't conflict, in priority order.
  Append claims on one path may be co-leased and are listed in `shared_paths`.
- `blocked` lists each requested id that was not leased, as `{id, code, reason}`, with code
  `MANUAL`, `NOT_READY`, `LOCKED`, `LAPSE_LIMIT`, `FILE_CONFLICT` or `MAX_COUNT`.
- `exhausted: true` with `needs_operator: true` means the plan is stalled on lapse-limited
  work, not finished. Fix the environment, then:

```text
plan.force_release {"plan_id": "<plan_id>", "deliverable_id": "<id>",
                    "reason": "runner crashed; fixed disk", "reset_counters": true}
```

## 2. Keep the lease

```text
plan.heartbeat {"plan_id": "<plan_id>", "deliverable_id": "<id>", "caller_id": "agent-7",
                "ttl_seconds": 1800}
```

Heartbeat at least every ttl/3. A lapsed lease counts toward the lapse limit (10). A
heartbeat after expiry fails with `LOCK_EXPIRED`, or `LOCK_NOT_HELD` once the lease was
reclaimed: stop, and re-acquire before continuing.

## 3. Report progress and close

```text
plan.mark_status {"plan_id": "<plan_id>", "deliverable_id": "<id>", "caller_id": "agent-7",
                  "status": {"status": "in_progress"}, "earned_pct": 40,
                  "actual_effort_hours": 3.5, "evidence": "https://git.example/commit/abc123"}
```

- `status` is `{"status": "in_progress"}`, `{"status": "complete"}`, `{"status": "ready"}` or
  `{"status": "failed", "reason": "..."}`. Complete and failed release the lock.
- `earned_pct`: integer 0..100, only meaningful with `in_progress`; ignored with `complete`,
  and refused with `INVALID_ACTUALS` with `ready` or `failed`.
- `actual_effort_hours`: total so far (not a delta), 0..1000000; it replaces the leased
  hours as actual cost.
- `evidence`: up to 2048 chars, appended; at most 100 entries per deliverable. Past that,
  any mark carrying `evidence` (including `complete`) is refused with `INVALID_ACTUALS`, so
  omit `evidence` then.
- Bad values are `INVALID_ACTUALS`.
- Mark `complete` only when the artifact meets its acceptance criteria, with evidence. A
  report of done is not acceptance.
- A `failed` mark leaves the deliverable Failed (acquire reports `NOT_READY`). To retry,
  hand it back with a lockless `{"status": "ready"}` mark, then acquire it again.
- Three explicit `failed` marks trip the circuit breaker, counted across those retries: the
  next acquire auto-fails it. Revive it with `plan.force_release {..., reset_counters: true}`
  after fixing the cause.

## 4. Owner and manual work (no lease)

`metadata.kind: "manual"` work is never leased. Close it, and any owner sign-off, with:

```text
plan.accept {"plan_id": "<plan_id>", "deliverable_id": "<id>", "accepted_by": "matthew",
             "evidence": "decision record docs/adr/007.md"}
```

- `evidence` is required. `override_lock: true` takes over a live lease; use it only when
  the holder is gone.
- `plan.accept` records no hours. To record the actual cost, follow it with a lockless mark
  (no lease held, your own `caller_id`):

```text
plan.mark_status {"plan_id": "<plan_id>", "deliverable_id": "<id>", "caller_id": "owner",
                  "status": {"status": "complete"}, "actual_effort_hours": 1.5}
```

## Lockless marks

A `plan.mark_status` without a lease is allowed and audited:
- a lockless `complete`, `ready` or `in_progress` requires complete prerequisites, or it
  is refused with `PREREQUISITES_INCOMPLETE`;
- a lockless `in_progress` (owner working by hand) persists across server restarts and is
  not leasable. Hand it back with a lockless `{"status": "ready"}` (or close it).
  `plan.force_release` does not affect it, since there is no lock.

## Loop and stop

1. `plan.acquire_cohort`; if nothing was leased, read `blocked` and `needs_operator`.
2. Do the work, heartbeating; mark `complete` with evidence, or `failed` with a reason.
3. Accept manual work as it becomes ready (`plan.status` `ready` set).
4. Repeat until `plan.status` shows `plan_complete: true`, or you are blocked on a person,
   a lapse limit or a failure that needs a decision. Report which.

Take snapshots at a steady cadence while running (`cpm-ev`). If the definition must change
mid-run, use `cpm-revise`.

## Pitfalls

- Planner refusals (`PREREQUISITES_INCOMPLETE`, `LOCK_HELD`, ...) come back as JSON-RPC
  `-32603` internal errors, not tool errors. Read the code at the start of the message.
- Work started before its prerequisites are complete cannot record actual cost until they
  close: a lockless mark is refused until then. Note the hours and record them afterwards.
- `plan.accept` needs the second lockless mark for AC every time; it is easy to forget.
- `actual_effort_hours` is cumulative; sending a delta understates AC.
