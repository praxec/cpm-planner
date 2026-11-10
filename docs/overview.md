# Plan overview and explain

This page specifies two read-only tools: `plan.overview`, one snapshot of a
plan that joins what `plan.status`, `plan.get` and `plan.ev` return, along
with each deliverable's evidence status; and `plan.explain`, which says why one
deliverable is blocked and what completing it changes. Both are
deterministic. They make no AI or network calls and never write. The
existing tools stay as they are.

Status of each part:

| Part | State |
|---|---|
| Library: `cpm_planner::overview` (`overview`, `read_snapshot`, `build_overview`, `render_markdown`, `evidence_text`, `PlanIndex`) | implemented |
| Library: `cpm_planner::explain` (`explain`, `build_explanation`, `preview_cohort`, `classify_next_work`, `slack_to_milestone`, `render_markdown`) | implemented |
| Evidence status on overview entries ([evidence contract](evidence.md#overview)) | implemented |
| MCP tools `plan.overview` and `plan.explain`, and the CLI commands `cpm-planner overview` and `cpm-planner explain` | implemented |

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
| `evidence_target` | [identity](evidence.md#evidence-identity-schema-version-1) | none | The target every evidence record is judged against ([target resolution](evidence.md#target-resolution), step 1). Without it, a gate is judged against its candidate's newest identity and other records against no target. A malformed identity is `INVALID_EVIDENCE`, checked before any read. |
| `format` | `"json"` \| `"markdown"` | `"markdown"` for the CLI, `"json"` for MCP | Markdown is `render_markdown` of the same value. Over MCP it is returned as `{"format": "markdown", "markdown": "<text>"}`; the CLI prints the text. |

Filters narrow the lists only. `counts`, `evidence_counts`, `ev` and
`critical_path_hours` always describe the whole plan.

In the library, `detail`, `limit`, `filter` and `evidence_target` are
`OverviewOptions` (which deserialises from those wire fields).
`OverviewRequest::with_evidence_target` sets the target, and
`Identity::from_json` parses the wire value.

### Consistency

The library reads the head revision and graph, the stored evidence,
`status`, `ev` at `as_of`, the stored evidence again, and the head revision
again. If the revision moved, the evidence changed, the ids differ, or an EV
row's status disagrees with `status`, then a write landed between the reads
and it reads again, up to three times. After three attempts it returns
`BACKEND_ERROR` and the caller retries. Leases are read once, from `status`,
so a heartbeat between the reads does no harm.

The evidence read is `Planner::stored_evidence`. It is a read-only trait
method; its default implementation returns no evidence, for planners that
store none.

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
  "attention":     {"total": 0, "omitted": 0, "items": []},
  "evidence_counts": {"current": 0, "stale": 1, "unknown": 0, "unbound": 0, "none": 5},
  "stale_evidence":  {"total": 1, "omitted": 0, "items": [Entry]}
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
| `evidence` | The deliverable's evidence status, in the [contract's shape](evidence.md#status-of-a-deliverable), unchanged (example below). Always present on overview entries. |

An entry's `evidence` looks like this:

```json
{
  "deliverable_id": "contract",
  "required_scope": "focused",
  "scope_source": "default",
  "entries": 1,
  "verdict": "stale",
  "record": {
    "index": 0, "scope": "focused", "verdict": "stale", "target_source": "argument",
    "changed": [
      {"field": "dependency_lock", "recorded": "sha256:l1", "target": "sha256:l2"},
      {"field": "definition", "recorded": "sha256:01de…", "target": "sha256:c3f0…"}
    ],
    "unknown": [],
    "recorded_at": "2026-03-02T09:00:00Z", "recorded_by": "worker"
  }
}
```

`verdict` is one of:

- `current`: every field the governing record holds matches, and so does the
  definition.
- `stale`: at least one field changed. `record.changed` lists the identity
  fields in contract order, then `definition`.
- `unknown`: nothing changed, but some fields could not be compared (no
  target). `record.unknown` names them.
- `unbound`: the deliverable holds only plain notes.
- `none`: it holds no evidence, and `record` is `null`.

A verdict is reported, never acted on. It does not change any status.

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
    driving root (the end of `chain`) comes first, then the rest by latest
    earliest finish. Each has its own `class`. At most 3 at brief;
    `root_blockers_omitted` counts the rest.
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
- **`stale_evidence`**: every deliverable whose evidence verdict is `stale`,
  complete deliverables first, then by id, as the
  [evidence contract](evidence.md#overview) orders it. Entries carry only the
  common fields. Their `evidence` says what changed.

### Markdown

`render_markdown` prints:

- a header line: plan, revision, `as_of`, drift and baseline;
- a counts line;
- an evidence line (`evidence: N current · N stale · …`), but only when some
  deliverable holds evidence;
- an EV line;
- one short table for each non-empty section: critical path, milestones,
  ready, blocked, leases and attention;
- a stale-evidence list.

An entry shows as its title followed by its id in parentheses, or as just the
id when the title is the id. If the entry has evidence, the compact text
follows: the verdict, then the changed fields for `stale` or the unknown
fields for `unknown`. For example, ``Export API contract (`contract`) · evidence: stale (dependency_lock, definition)``.
`evidence_text` returns that text, and `none` shows nothing. `★` marks
critical milestones and ready work. A truncated section ends with a line
saying how many entries were not shown. A brief overview of a plan with
hundreds of deliverables stays under about 60 lines.

### Library API

```rust
pub async fn overview<P: Planner + ?Sized>(planner: &P, req: OverviewRequest)
    -> Result<Overview, PlannerError>;
pub async fn read_snapshot<P: Planner + ?Sized>(planner: &P, plan_id: &PlanId, as_of: DateTime<Utc>)
    -> Result<PlanSnapshot, PlannerError>;
pub fn build_overview(snapshot: &PlanSnapshot, options: &OverviewOptions)
    -> Result<Overview, PlannerError>;
pub fn render_markdown(overview: &Overview) -> String;
pub fn evidence_text(status: &DeliverableEvidenceStatus) -> Option<String>;
```

`status` and `ev` recompute the CPM, so an async server runs `overview` off
the runtime's worker threads, as it already does for `plan.status`.

## `plan.explain`

This tool answers two questions about one deliverable: why it is not
moving, and what completing it changes. It also gives the plan's next-work
view, in classes. It is built on the same snapshot and on `PlanIndex`
(`blocker_chain`, `next_milestone`, `downstream_milestones`, `unlocks`,
`kind`, `ready_action`), so its answers agree with `plan.overview` at the
same `as_of`. Its entries do not carry `evidence`; use `plan.overview` for
that.

### Request

| Field | Type | Default | Meaning |
|---|---|---|---|
| `plan_id` / `plan` / `active` | selector | required | As for `plan.overview`. |
| `deliverable_id` | id | none | The deliverable to explain. If omitted, the response has only `next_work`. An unknown id is `DELIVERABLE_NOT_FOUND`. |
| `as_of` | RFC 3339 | now | As for `plan.overview`. |
| `limit` | integer `1..=1000` | 10 | Most entries per list. Out of range is `INVALID_VALUE`. |
| `format` | `"json"` \| `"markdown"` | as for `plan.overview` | Markdown is `cpm_planner::explain::render_markdown`. |

### Response (JSON, schema `cpm-planner/explain/v1`)

```json
{
  "schema": "cpm-planner/explain/v1",
  "as_of": "…",
  "plan": { "plan_id": "…", "name": "export", "variant": "main", "revision": 3 },
  "deliverable": {
    "id": "service", "title": "…", "status": "pending", "kind": "work", "owner": "agent",
    "critical": true, "float": 0.0,
    "state": "blocked",
    "blocked": {
      "class": "manual",
      "waiting_on": [{"id": "approve", "title": "Product sign-off"}],
      "root_blockers": [{"id": "approve", "title": "Product sign-off", "status": "ready",
                         "kind": "manual", "owner": "pm", "class": "manual", "action": "accept"}],
      "root_blockers_omitted": 0,
      "chain": ["service", "approve"]
    },
    "attempt_count": 0, "failure_count": 0, "lapse_count": 0,
    "unlocks": [{"id": "ship", "title": "Export shipped"}],
    "unlocks_omitted": 0,
    "feeds_milestones": [{"id": "ship", "title": "Export shipped", "critical": true, "slack_hours": 0.0}],
    "feeds_milestones_omitted": 0,
    "critical_path_impact": "On the critical path: each hour it slips moves the plan finish by an hour; it drives milestone `ship`."
  },
  "next_work": {
    "ready_critical":     {"total": 1, "omitted": 0, "items": [NextWorkEntry]},
    "ready_slack":        {"total": 1, "omitted": 0, "items": [NextWorkEntry]},
    "blocked_dependency": {"total": 0, "omitted": 0, "items": []},
    "blocked_manual":     {"total": 2, "omitted": 0, "items": [NextWorkEntry]},
    "blocked_external":   {"total": 0, "omitted": 0, "items": []},
    "blocked_gate":       {"total": 0, "omitted": 0, "items": []},
    "file_conflicting":   {"total": 0, "omitted": 0, "items": []}
  }
}
```

`deliverable.state` is one of:

- `complete`.
- `leased`: `lease` is set, with `{caller_id, expires_at, expired}`.
- `in_progress`: marked in progress without a lease (owner or manual work).
- `ready`: it can start now. `action` is `acquire`, `accept` or `operator`,
  as in the overview.
- `blocked`: `blocked` is set. Its fields mean the same as in an overview
  `blocked` entry, except that `chain` is always present. Each root blocker
  in the ready set also carries its `action`.
- `failed`: `reason` is set. It is still re-acquirable until the failure cap.

`action`, `blocked`, `lease` and `reason` are left out when they do not
apply. The three counters are always present.

`unlocks` lists the dependents whose only incomplete prerequisite is this
deliverable, which are exactly what becomes ready when it completes. Graph
order; `unlocks_omitted` counts any beyond `limit`.

`feeds_milestones` lists the incomplete milestones it feeds, earliest finish
first; `feeds_milestones_omitted` counts any beyond `limit`. `slack_hours`
is the number of hours this deliverable can slip before that milestone's
earliest finish moves. It comes from a backward pass from the milestone's
earliest finish over its ancestors, with edge lags, and is 0 when the
deliverable drives the milestone.

`critical_path_impact` is one sentence:

- complete: `Complete: no remaining schedule impact.`
- zero float: `On the critical path: each hour it slips moves the plan finish by an hour`
- otherwise: `<float>h of float before a slip moves the plan finish`

followed, for the earliest milestone it feeds, by `; it drives milestone`
or `; <slack>h of slack before it moves milestone`, then the milestone's id
in backticks and a full stop.

`next_work` puts every incomplete, unleased, not-failed deliverable that is
ready or blocked into exactly one class, checked in this order:

| Class | Members |
|---|---|
| `file_conflicting` | Ready, but `acquire_cohort` would skip it now: an owned file conflicts with a live lease or with a higher-priority pick. |
| `ready_critical` | Ready (in `plan.status` `ready`) and either on the plan's critical path or on the driving path (`MilestoneRow.critical_path`) of an incomplete milestone. |
| `ready_slack` | Any other ready work. It can run in parallel with critical work. |
| `blocked_manual` | Blocked, and its driving root blocker is `manual` (owner action, `plan.accept`). |
| `blocked_external` | Blocked on an `external` sign-off. |
| `blocked_gate` | Blocked on a `gate` (`plan.accept` with evidence). |
| `blocked_dependency` | Blocked on ordinary work. |

Ready classes list in `plan.status` `ready` order. Blocked classes list by
least float, then latest start, then id. Leased, complete, failed and
lockless in-progress deliverables are in no class; the overview lists them
under `leases` and `attention`.

A `NextWorkEntry` is the common entry fields plus:

- `float`;
- `next_milestone`, optional;
- `action`, for the ready and file-conflicting classes;
- `blocked_by`, the driving root blocker as `{id, title}`, for the
  `blocked_*` classes;
- `file_conflicts`, for `file_conflicting`.

`preview_cohort` works out the file conflicts by replaying `acquire_cohort`'s
greedy fill without taking a lease:

- candidates are taken in priority order: smallest latest start, then least
  float, then id;
- manual, gate and capped deliverables are skipped, because `acquire_cohort`
  never leases them;
- leases still live at `as_of` hold their claims, and expired ones do not,
  because `acquire_cohort` reaps them first.

The preview's `leasable` list is exactly what an unrestricted
`acquire_cohort` would lease. `acquire_cohort` stays the authority.
