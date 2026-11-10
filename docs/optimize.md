# Offline plan optimization

`cpm_planner::optimize` proposes edits that make a plan shorter, cheaper or simpler. It never
changes a plan. It runs offline, makes no model, provider or network call, and returns the same
proposals and scores for the same graph and request. Each proposal is an edit list that
`plan.fork { edits }` can apply, already checked by lint, the schedule and simulate.

This page specifies the library API and the `plan.optimize` MCP tool and CLI command that
expose it.

## What it looks for

| `kind` | Signal | Edits |
|---|---|---|
| `merge` | Two work deliverables with the same `metadata.owner` claim the same file, and at least one claim is exclusive. Groups of more than 32 claimants are skipped. | Replace both with one deliverable under the upstream id (the smaller id when unordered). It keeps both sets of prerequisites and files, the summed effort, and both acceptance criteria. Dependents are rewired to it, and `metadata.merged_from` lists the originals. |
| `false_dependency` | An edge that lint flags `RESOURCE_CONSTRAINT_EDGE`, `NO_RATIONALE` or `LOW_INFORMATION_CONSUMES`. | `remove_edge` |
| `parallel_cohort` | Three or more work deliverables joined only by such edges. | Remove all of those edges, so the members can run together. |
| `gate_consolidation` | Two consecutive delivery waves (see `plan.lint` `waves`), the first with a single gate and no `boundary_reason`. | Drop the first gate. Its prerequisites feed the second gate, which takes over its acceptance criteria and records it in `metadata.consolidates`. Work that waited on the dropped gate depends on its prerequisites instead. |
| `gate_bypass` | Work depends on a `metadata.kind: "gate"` deliverable, and the edge's `consumes` names neither the gate's id, its acceptance text, nor a result word (`approved`, `evidence`, `green`, `merged`, `pass`, `proven`, `published`, `release`, `report`, `result`, `signed`, `verified` and the like). | Remove the gate edge and depend on the gate's prerequisites. |

Work means a deliverable that is not a milestone, gate, `kind: "manual"` or `"external"`,
`external: true`, or `contract: true`.

## How each candidate is checked

1. A candidate that would merge, cut or rewire a protected deliverable (a manual or external
   approval, a gate, a milestone or an interface contract) is rejected before any edit runs. So
   are an interface edge, an edge lag the merge would lose, a file collision (two members claim
   the same file and could not be leased together), a wave with a `boundary_reason`, a wave whose
   gates form a chain, and two gates with different `metadata.command`.
2. The edits are applied to a copy with `apply_edits`, under the same validation as `plan.submit`.
3. Ordering into milestones, gates and manual or external deliverables is kept. If an edit would
   let one of them stop waiting for work it waited for, an `add_edge` from that work is added to
   the list. If that edge would make a cycle, the candidate is rejected ("'m1' would stop waiting
   for …"). Edges the edits make redundant are removed (never interface edges).
4. Preservation is checked against the original:
   - every acceptance criterion (`metadata.artifact`, `acceptance`, `acceptance_criteria`,
     `verify`) is still present, in the same deliverable or the one that carries it;
   - every gate, milestone, manual or external approval and interface contract still exists
     unchanged. The only exception is a gate folded into another gate by `gate_consolidation`;
   - every interface edge still exists;
   - every owned file is still owned by the same deliverable, or by the one that carries it, and
     no `metadata.owner` changes.
5. `simulate` runs lint, CPM, the optional leveling and the scorecard. A lint error rejects the
   candidate. So does a warning the original does not have; ids are mapped through merges first.
6. The candidate must not lengthen the makespan (the leveled one when capacities are given) and
   must not add effort. Its score must be at least 0.01:

   `score = -Δmakespan + 0.5 × effort saved + 0.25 × deliverables removed + 0.1 × warnings resolved`

A rejected candidate keeps its edits and every reason. Candidates are generated and evaluated in
`(kind, id)` order, with at most 256 per run. Proposals are ranked by score, ties by id, and capped
at `max_proposals` (default 32). Hours are rounded to 0.01 and risk to 0.0001. `truncated` is set
when either cap drops something.

`plan.acquire_cohort` stays the authority for leases and file conflicts. A `parallel_cohort`
proposal only says the members could run together once forked.

## Library API

```rust
pub fn optimize(graph: &PlanGraph, req: &OptimizeRequest)
    -> Result<OptimizeReport, PlannerError>;
pub fn validate_edits(graph: &PlanGraph, req: &OptimizeRequest, edits: &[GraphEdit])
    -> Result<Result<OptimizeProposal, RejectedCandidate>, PlannerError>;
```

Both return `PlannerError::InvalidGraph` when the input graph does not simulate. `validate_edits`
applies the same checks to an edit list you supply (kind `edits`, id `edits`). It compensates the
same way, and anything a removed deliverable carried counts as lost.

## `plan.optimize`

Request: exactly one of `graph`, `plan_id` or `path`, as for `plan.lint`, `plan.simulate` and
`plan.review`. Optional fields:

| Field | Meaning |
|---|---|
| `capacities`, `resource_key`, `project_buffer_pct` | Level every candidate, as `plan.schedule` does. The leveled makespan must not grow, and resource wait is reported. |
| `max_proposals` | Cap on `proposals`, default 32. |

Response: the serialized `OptimizeReport`.

```json
{
  "baseline": {"deliverables": 8, "makespan": 6.0, "resource_wait_hours": 0.0,
               "total_effort": 12.0, "gate_hours": 3.0, "criticality_risk": 0.75,
               "lint_warnings": 1},
  "proposals": [{
    "id": "gate_bypass:regression->docs",
    "kind": "gate_bypass",
    "ids": ["docs", "regression"],
    "edits": [{"op": "remove_edge", "from": "regression", "to": "docs"},
              {"op": "add_edge", "from": "c1", "to": "docs",
               "consumes": "parser source code from 'c1' (not via gate 'regression')"}],
    "deltas": {"deliverables": 0, "makespan_hours": -1.0, "resource_wait_hours": 0.0,
               "effort_hours": 0.0, "gate_hours": 0.0, "criticality_risk": 0.0,
               "lint_warnings": 0},
    "after": {"deliverables": 8, "makespan": 5.0, "...": "..."},
    "score": 1.0,
    "rationale": "'docs' waits for gate 'regression' but consumes 'parser source code', not its result; ...; simulated makespan 6 h -> 5 h",
    "preserved": ["acceptance criteria: 6", "gates and manual/external approvals: 2",
                  "interface contracts: 0", "owned files: 4", "milestones: 1"],
    "trade_offs": ["'docs' may build on code 'regression' later rejects"]
  }],
  "rejected": [{"id": "gate_consolidation:regression->release", "kind": "gate_consolidation",
                "ids": ["regression", "release"], "edits": [], "reasons": ["..."]}],
  "candidates": 2,
  "truncated": false
}
```

`deltas` are `after − before`, so negative means better. `resource_makespan` and
`resource_makespan_hours` appear only when capacities are given. `resource_wait_hours` is the
leveled makespan minus the CPM makespan.

The tool is read-only: it never forks, selects or syncs. Apply a proposal with
`plan.fork { plan_id, variant, edits }`, check it with `plan.simulate` or `plan.compare`, then
`plan.select` it. The CLI command `cpm-planner optimize` takes the same selector and options and
prints the same JSON.

## Relation to `plan.review`

Run `plan.optimize` first. It is free, offline and deterministic, and it catches the structural
waste a graph shows by itself. `plan.review` stays optional: it calls the Jev judge (OpenRouter or
a review adapter) for judgments the graph cannot make alone, such as whether an edge that names
an artifact is still unnecessary, or which crash technique fits. Both return `plan.fork` edit
lists checked by simulation. Review ranks its proposals by hours saved per hour of added cost.
Optimize also counts effort saved, deliverables removed and warnings resolved, and it reports
rejected candidates with their reasons.
