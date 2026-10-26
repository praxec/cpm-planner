# Compile a release workflow into a plan

`cpm-planner release` compiles a retained workflow/promotion snapshot into an
ordinary plan file before baselining. It makes PRs, parallel required checks,
check joins, approval waits, owner approvals, merges, producer attempts, retry
receipts, retained artifacts and central acceptance explicit deliverables.

Start with [examples/release-workflow.json](../examples/release-workflow.json).
It models two protected promotions, concurrent Linux/Windows checks, a failed
proof followed by a planned retry, and central acceptance of the final receipt.

```sh
cpm-planner release lint examples/release-workflow.json
cpm-planner release compile examples/release-workflow.json
cpm-planner release compile examples/release-workflow.json --report
cpm-planner release diff examples/release-workflow.json .cpm-planner/plans/release/main.json
```

`compile` prints `PlanGraph` JSON to stdout; save that JSON as the plan-as-code
file. `--report` instead prints `{schema_version, policy_digest, graph, lint}`.
`diff` exits 1 for a stale definition, 0 for an unchanged definition; errors exit
1 and usage errors exit 2. Inputs are limited to 8 MiB. All commands run offline,
read no credentials, and neither open the planner database nor modify files.

MCP equivalents are `plan.release.compile {workflow}` and
`plan.release.diff {workflow, graph}`. Their arguments are inline JSON, using
the same schema and validation as the CLI. Compile returns the report; diff
returns `{stale, policy_changed, current_policy_digest, added, removed, changed, graph_changes}`.
Unknown nested keys and schema versions are refused. Neither call submits,
baselines, leases, completes, accepts or revises any work.

## Input contract (schema version 1)

`repositories` is a bounded collection. Each item has a stable `id`, a forge
`repository` name, `allowed_edges` (`from`/`to`), bounded `promotions`, and optional
bounded `producers`. Repository and operation ids contain 1–64 ASCII letters,
digits, hyphens or underscores. Generated ids use these stable ids, never an
array index or random identifier. The resulting nodes are sorted by id.

A promotion declares `id`, `from`, `to`, `depends_on` (promotion ids in the same
repository), bounded `checks` (`id`, `workflow`) and bounded `approvals` (`id`,
`environment`). A predecessor must promote into the consuming promotion's
source. For example, `staging -> main` consumes the merge receipt of `dev ->
staging`. Declare `depends_on: []` when starting from an existing source branch.
The importer validates allowed edges and references; it does not infer the
contents of a branch or query the forge. Retain the workflow and branch-policy
snapshot used to produce this input, and refresh it before checking drift.

Every collection that expands into operations uses this shape:

```json
{"cardinality": 2, "items": [{"id": "linux", "workflow": "ci/linux"}, {"id": "windows", "workflow": "ci/windows"}]}
```

Alternatively, declare a bounded retained source:

```json
{"source": {"uri": "workflow-snapshot.json", "revision": "git:abc123", "max_items": 4}, "items": [{"id": "linux", "workflow": "ci/linux"}]}
```

The source URI is provenance, not a command, URL to fetch, or glob to expand.
The caller supplies the expanded items. `cardinality` must equal their count;
`source.max_items` must bound it and cannot exceed 5000. Both may be supplied.
Missing bounds produce `HIDDEN_CARDINALITY`; exceeded bounds produce
`UNBOUNDED_CARDINALITY`; wrong counts produce `CARDINALITY_MISMATCH`. An omitted
optional collection means an exact empty collection. Expanded plans cannot
exceed the planner's 5000-deliverable limit. Branch allowlists, trigger path
lists and dependency lists are finite references, not additional expansion
sources, and are also bounded to 5000 entries.

## Producers, retry evidence and skips

A producer declares `id`, `workflow`, the `promotion` whose merge it consumes,
bounded `attempts` (`id`, `outcome`), and bounded retained `artifacts` (`id`,
`path`). Attempts are ordered: every nonfinal attempt must be `failed` or
`cancelled`, and the final one must be `succeeded` or `planned`. Each run and its
receipt get separate nodes. A retry consumes the preceding failed/cancelled
receipt; acceptance consumes the artifacts of the final run. A terminal failed
proof is refused with `UNRESOLVED_PROOF_FAILURE`.

A change-triggered producer can include
`trigger: {matched, paths, evidence}`. `matched: false` requires zero attempts
and a retained change-decision receipt. The compiler emits a skipped-campaign
receipt and its artifacts without allocating producer runner time. This is an
explicit evidenced decision; it does not infer a skip from an empty attempts
list. With `matched: true` or no trigger, at least one attempt is required.

Recorded outcomes are definition/evidence metadata, not runtime completion
claims. Compiling a prior successful run does not accept its generated node;
the normal evidence review and `plan.accept` workflow still applies.

## Resources and duration provenance

`estimates` maps each used stage (`pr`, `check`, `join`, `approval_wait`,
`approval`, `merge`, `producer`, `receipt`, `artifact`, `acceptance`) to
`{hours, source, confidence}`. Every used stage needs an explicit estimate,
including zero-duration joins. Hours must be finite, nonnegative and at most
the planner's normal maximum; confidence is a caller-provided value in [0, 1].
The compiler supplies no implicit duration guesses.

Agent tasks use `metadata.resources: {agent: 1}`. Approvals, merges and central
acceptance use `{owner: 1}`; checks and producer runs use `{external_runner: 1}`.
Owner, runner and wait nodes have `metadata.kind: "manual"` so an agent cannot lease
them. Approval waits use `duration_hours` plus `wall_clock_only: true`, consuming
elapsed time without occupying the owner. Runner nodes consume runner capacity
for their duration; their agent-effort cost is zero.

Optional `history` entries declare `node_id`, `duration_hours`, `outcome`,
`evidence` and `confidence`. Unknown node ids, planned history and invalid values
are refused. Succeeded, failed and cancelled retained runs contribute observed
duration; skipped runs do not. The estimate uses the minimum, lower median and
maximum observation as optimistic, likely and pessimistic. It retains the
sample receipts, method and minimum caller confidence across contributing
samples under `metadata.release.estimate`. This confidence is provenance, not
a computed reliability claim. Nodes without contributing history retain their
declared estimate source/confidence. Normal Monte Carlo samples three-point
agent/owner effort estimates; explicit runner/wait durations remain fixed,
following the existing `duration_hours` precedence.

## Acceptance and definition drift

`acceptance` declares a stable `id` and a bounded collection of dependencies.
Each names a repository and promotion, optionally a producer and artifact
(both must be supplied together). It consumes terminal merge or artifact
receipts and produces the central owner-only milestone. Every generated node
must feed that milestone; missing coverage, cycles, redundant dependencies or
other normal lint warnings/errors refuse compilation. For example, include
each independent repository's terminal receipt, and avoid also listing an
ancestor of a receipt already consumed.

The canonical serialized policy (excluding run history) is SHA-256 hashed.
Every node retains that digest, its expansion stage/detail, and estimate
provenance in `metadata.release`. Array ordering is part of the input contract;
reordering a policy collection changes the digest conservatively. Stable ids
remain stable across recompilation.

`release diff` recompiles the current snapshot and compares it with the retained
plan, reporting changed/added/removed ids and policy drift. History-only estimate
changes can make `stale: true` while `policy_changed: false`. Local edits or
missing compiler provenance also make the definition stale. Graph-level edits,
including `max_chained_dispatch`, are named in `graph_changes` and make the
definition stale even when no deliverable or workflow policy changed. A policy change can
mark otherwise identical nodes changed because their provenance digest changed.

Run ordinary `plan.lint`, `plan.simulate` and `plan.schedule` on the generated
graph, then save and `plan.sync` it before taking the baseline. Level with
capacities for `agent`, `owner` and `external_runner`. Review any stale diff and
apply the accepted new definition through the normal `plan.sync`/`plan.revise`
protocol; existing lease supersession, revision history, earned value and durable
state recovery semantics remain in force.
