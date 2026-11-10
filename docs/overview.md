# Plan overview and explain

This page specifies two read-only tools: `plan.overview`, one snapshot of a
plan that joins what `plan.status`, `plan.get` and `plan.ev` return, and
`plan.explain`, why one deliverable is blocked and what completing it
changes. Both are deterministic. They make no AI or network calls and never
write. The existing tools stay as they are.

Status of each part:

| Part | State |
|---|---|
| Library: `cpm_planner::overview` (`overview`, `read_snapshot`, `build_overview`, `render_markdown`, `PlanIndex`) | implemented |
| MCP tool `plan.overview` and CLI command | specified here; wired by a later change |
| `plan.explain` (`src/explain.rs`, MCP and CLI) | specified here; not yet implemented |
| Evidence freshness (`evidence` on entries) | reserved field; always omitted in v1 |

## `plan.overview`

### Request

| Field | Type | Default | Meaning |
|---|---|---|---|
| `plan_id` / `plan` / `active` | selector | required | Exactly one, as for `plan.status`. `project` applies to `plan` and `active`. Resolved with the same `resolve_plan`, so `PLAN_NOT_FOUND`, `NO_ACTIVE_PLAN` and `AMBIGUOUS_ACTIVE_PLAN` pass through unchanged. |
| `as_of` | RFC 3339 | now | The one instant for the whole snapshot. It is EV's PV status date, and leases are judged live or expired against it. |
| `detail` | `"brief"` \| `"full"` | `"brief"` | How much each entry says (below). |
| `limit` | integer `1..=1000` | 10 brief, 100 full | Most entries per list. Out of range is `INVALID_VALUE`. |
| `filter.owner` | string | none | Keep only entries whose `metadata.owner` equals this. |
| `filter.milestone` | id | none | Keep only that milestone and the deliverables it waits on. An unknown id is `DELIVERABLE_NOT_FOUND`; a deliverable that is not a milestone is `INVALID_VALUE`. |
| `format` | `"json"` \| `"markdown"` | `"markdown"` for the CLI, `"json"` for MCP | Markdown is `render_markdown` of the same value. |

Filters narrow the lists only. `counts`, `ev` and `critical_path_hours`
always describe the whole plan.

### Consistency

The library reads the head revision and graph, then `status`, then `ev` at
`as_of`, then the head revision again. If the revision moved, the ids differ,
or an EV row's status disagrees with `status`, a write landed between the
reads, so it reads again, up to three times. After that it returns
`BACKEND_ERROR` and the caller retries. Leases are read once, from `status`,
so a heartbeat between the reads does no harm.

### Response (JSON, schema v1)

Every value is computed at the top-level `as_of`. Field order below is the
serialisation order. A field shown as optional is omitted when absent.

```json
{
  "schema": "cpm-planner/overview/v1",
  "as_of": "2026-03-02T11:00:00Z",
  "detail": "brief",
  "filter": {"owner": "agent"},
  "plan": {
    "plan_id": "plan_…", "name": "export", "variant": "main", "revision": 3,
    "selected": true, "archived": false, "active_plan": null,
    "definition_drift": false,
    "baseline": {"baseline_number": 1, "as_of": "…"},
    "plan_complete": false
  },
  "counts": {
    "total": 6, "pending": 3, "ready": 1, "in_progress": 1,
    "superseded_revision": 0, "complete": 1, "failed": 0,
    "milestones": 1, "milestones_complete": 0, "leases": 1
  },
  "ev": {"state": "baselined", "baseline_number": 1, "backfilled": false,
         "bac": 14.0, "pv": 2.0, "ev": 4.0, "ac": 3.5, "sv": 2.0, "cv": 0.5,
         "spi": 2.0, "cpi": 1.14, "eac": 12.25, "etc": 8.75, "vac": 1.75, "tcpi": 0.95,
         "undefined": [], "alerts": [], "critical_float_consumed_hours": 0.0,
         "excluded_unbaselined": 0},
  "critical_path_hours": 11.0,
  "critical_path": {"total": 4, "omitted": 0, "items": [PathEntry]},
  "milestones":    {"total": 1, "omitted": 0, "items": [MilestoneEntry]},
  "ready":         {"total": 1, "omitted": 0, "items": [ReadyEntry]},
  "blocked":       {"total": 3, "omitted": 0, "items": [BlockedEntry]},
  "leases":        {"total": 1, "omitted": 0, "items": [LeaseEntry]},
  "attention":     {"total": 0, "omitted": 0, "items": []}
}
```

Before the first baseline, `ev` is
`{"state": "not_baselined", "reason": "NOT_BASELINED: …"}`. When baselined, a
`null` ratio always has an `undefined` entry that names the field and says
why, as in `plan.ev`. Per-row EV stays in `plan.ev`.

Every list is a section, `{total, omitted, items}`. `total` counts the
entries that match the filter, and `omitted` is `total - items.len()`.

Every entry begins with the same flattened fields:

| Field | Meaning |
|---|---|
| `id` | Deliverable id. |
| `title` | `metadata.title`, else `metadata.name`, `metadata.description`, `metadata.artifact`, else the id. Never empty. |
| `status` | `pending`, `ready`, `in_progress`, `superseded_revision`, `complete` or `failed`, the `plan.status` tag without the failure reason. |
| `kind` | `work`, `milestone`, `manual` (`metadata.kind == "manual"`), `external` (`metadata.kind == "external"` or `metadata.external == true`) or `gate` (`metadata.kind == "gate"`). |
| `owner`, `route` | `metadata.owner` / `metadata.route`, optional. |
| `evidence` | Reserved for evidence freshness: `{entries, freshness: current \| stale \| unscoped, scope?}`. Always omitted in v1. |

The sections:

- **`critical_path`**: `plan.status` `critical_path` without `__start__` and
  `__finish__`, in execution order. Each entry adds `hours` (scheduled
  length), `remaining_hours` (0 when complete, otherwise `hours` scaled by
  the EV row's unearned share) and `feeds_milestones` (the incomplete
  milestones it feeds, earliest first, as `{id, title}`; at most 3 at brief).
  `feeds_milestones` is the entry's milestone impact: a slip here moves each
  of them. Full detail adds `es` and `ef`.
- **`milestones`**: `plan.status` `milestones`, in graph order. Each entry
  adds `hours` (earliest finish), `complete`, `critical` (on the plan's
  critical path), `remaining_on_path` (incomplete deliverables on the
  milestone's own critical path) and `next_on_path` (the first of them, which
  is the milestone's next action).
- **`ready`**: `plan.status` `ready`, in its order (least float first). Each
  entry adds `action`, `float`, `critical`, `next_milestone` and
  `file_conflicts`. Full detail adds `owned_files`. `action` is one of:
  - `acquire`: lease it with `plan.acquire_cohort`.
  - `accept`: manual or gate work. Do it, then close it with `plan.accept`.
  - `operator`: it is at the failure or lapse cap, so an operator must run
    `plan.force_release` with `reset_counters`.

  `file_conflicts` is `[{path, with, leased}]`, worked out by
  `acquire_cohort`'s rule: an exclusive claim conflicts with any claim, and
  two append claims can share a path. `leased: true` means `with` holds a
  lease now.
- **`blocked`**: every incomplete, unleased deliverable that has an
  incomplete prerequisite, least float first. Each entry adds:
  - `class`: what unblocks it, taken from the driving root blocker:
    `dependency` (ordinary work or a milestone), `manual`, `external` or
    `gate`.
  - `float` and `next_milestone`.
  - `waiting_on`: its direct incomplete prerequisites.
  - `root_blockers`: the incomplete ancestors that have no incomplete
    prerequisite of their own, which is the work that can move now. The
    driving blocker comes first, then the rest by latest earliest finish.
    Each has its own `class`. At most 3 at brief; `root_blockers_omitted`
    counts the rest.
  - `chain` (full detail only): the driving chain, this id first. Each step
    goes to the incomplete prerequisite with the latest earliest finish,
    ending at the driving root.
- **`leases`**: `plan.status` `locks_held`, soonest expiry first. Each entry
  adds `caller_id`, `acquired_at`, `expires_at`, `expired`
  (`expires_at <= as_of`; the next mutating call reaps it) and
  `seconds_remaining`.
- **`attention`**: incomplete deliverables that need a person, in graph
  order, with `code`, `reason` (for `FAILED`), `attempt_count`,
  `failure_count` and `lapse_count`. The first code that applies is used:
  `FAILURE_LIMIT`, `LAPSE_LIMIT`, `FAILED`, `SUPERSEDED_REVISION`, then
  `LAPSED` (it has lost a lease to TTL expiry).

### Markdown

`render_markdown` prints a header line (plan, revision, `as_of`, drift,
baseline), a counts line, an EV line, and one short table for each non-empty
section: critical path, milestones, ready, blocked, leases and attention.
Entries show as the title followed by the id in parentheses, or just the id when the title is the id.
`★` marks critical milestones and ready work. A truncated section ends with
a line saying how many entries were not shown. A brief overview of a plan
with hundreds of deliverables stays under about 60 lines.

### Library API

```rust
pub async fn overview<P: Planner + ?Sized>(planner: &P, req: OverviewRequest)
    -> Result<Overview, PlannerError>;
pub async fn read_snapshot<P: Planner + ?Sized>(planner: &P, plan_id: &PlanId, as_of: DateTime<Utc>)
    -> Result<PlanSnapshot, PlannerError>;
pub fn build_overview(snapshot: &PlanSnapshot, options: &OverviewOptions)
    -> Result<Overview, PlannerError>;
pub fn render_markdown(overview: &Overview) -> String;
```

`status` and `ev` recompute the CPM, so an async server runs `overview` off
the runtime's worker threads, as it already does for `plan.status`.

## `plan.explain`

This tool answers two questions about one deliverable: why it is not
moving, and what completing it changes. It also gives the plan's next-work
view, in classes. It is built on the same snapshot and on `PlanIndex`
(`blocker_chain`, `next_milestone`, `downstream_milestones`, `unlocks`,
`kind`, `is_accept_only`), so its answers agree with `plan.overview` at the
same `as_of`.

### Request

| Field | Type | Default | Meaning |
|---|---|---|---|
| `plan_id` / `plan` / `active` | selector | required | As for `plan.overview`. |
| `deliverable_id` | id | none | The deliverable to explain. If omitted, the response has only `next_work`. An unknown id is `DELIVERABLE_NOT_FOUND`. |
| `as_of` | RFC 3339 | now | As for `plan.overview`. |
| `limit` | integer `1..=1000` | 10 | Most entries per list. |
| `format` | `"json"` \| `"markdown"` | as for `plan.overview` | |

### Response (JSON, schema `cpm-planner/explain/v1`)

```json
{
  "schema": "cpm-planner/explain/v1",
  "as_of": "…",
  "plan": { "plan_id": "…", "name": "…", "variant": "…", "revision": 3 },
  "deliverable": {
    "id": "service", "title": "…", "status": "pending", "kind": "work", "owner": "agent",
    "critical": true, "float": 0.0,
    "state": "blocked",
    "blocked": {
      "class": "manual",
      "waiting_on": [{"id": "approve", "title": "Product sign-off"}],
      "root_blockers": [{"id": "approve", "title": "…", "status": "ready", "kind": "manual",
                         "class": "manual", "action": "accept"}],
      "root_blockers_omitted": 0,
      "chain": ["service", "approve"]
    },
    "lease": null,
    "unlocks": [{"id": "ship", "title": "Export shipped"}],
    "feeds_milestones": [{"id": "ship", "title": "Export shipped", "critical": true}],
    "critical_path_impact": "on the critical path: a slip of h hours moves the finish by h hours"
  },
  "next_work": {
    "ready_critical":  {"total": 1, "omitted": 0, "items": [NextWorkEntry]},
    "ready_slack":     {"total": 0, "omitted": 0, "items": []},
    "blocked_dependency": {"total": 0, "omitted": 0, "items": []},
    "blocked_manual":  {"total": 1, "omitted": 0, "items": []},
    "blocked_external":{"total": 0, "omitted": 0, "items": []},
    "blocked_gate":    {"total": 0, "omitted": 0, "items": []},
    "file_conflicting":{"total": 0, "omitted": 0, "items": []}
  }
}
```

`deliverable.state` is one of:

- `complete`.
- `leased`: `lease` is set, with `{caller_id, expires_at, expired}`.
- `ready`: it can start now. `action` is `acquire`, `accept` or `operator`,
  as in the overview.
- `blocked`: `blocked` is set. Its fields mean the same as in an overview
  `blocked` entry, and the chain is always present.
- `failed`: it carries `reason` and the counters. It is still re-acquirable
  until the failure cap.

`unlocks` lists the dependents whose only incomplete prerequisite is this
deliverable, which are exactly what becomes ready when it completes.
`feeds_milestones` is the incomplete milestones it feeds, earliest first.
`critical_path_impact` is a single sentence: on the critical path, or the
number of hours of float before a slip moves the finish, and whether a slip
moves the earliest milestone it feeds.

`next_work` puts every incomplete deliverable into exactly one class, checked
in this order:

| Class | Members |
|---|---|
| `file_conflicting` | Ready, but at least one owned file conflicts with a held lease or a higher-priority ready candidate, so `acquire_cohort` would not lease it with them now. |
| `ready_critical` | Ready (in `plan.status` `ready`), on the critical path (zero float) or feeding a critical milestone. |
| `ready_slack` | Ready with positive float. These can run in parallel with critical work. |
| `blocked_manual` | Blocked, and its driving root blocker is `manual` (owner action, `plan.accept`). |
| `blocked_external` | Blocked on an `external` sign-off. |
| `blocked_gate` | Blocked on a `gate` (`plan.accept` with evidence). |
| `blocked_dependency` | Blocked on ordinary work. |

Leased, complete and failed deliverables are in no class; the overview lists
them under `leases` and `attention`. A `NextWorkEntry` is the common entry
fields plus `float`, `next_milestone`, the driving root blocker (for the
`blocked_*` classes) or `file_conflicts` (for `file_conflicting`). Classes do
not lease and do not change `acquire_cohort`. File conflicts use
`acquire_cohort`'s rule, so a cohort request for a `ready_*` set gets the
same answer.
