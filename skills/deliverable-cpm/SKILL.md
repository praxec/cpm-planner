---
name: deliverable-cpm
description: Plan a multi-lane project as a deliverable-based critical-path (CPM) graph in cpm-planner — vet every dependency edge, level resources, crash the schedule, baseline earned value, and turn under-specified deliverables into tickets. Use when a project spans repos, agents or teams and the order of work matters.
---

# Deliverable-based CPM with cpm-planner

A plan is a graph of **deliverables** (inspectable artifacts) joined by **consumption
edges**. It is never a list of activities. cpm-planner holds the graph and its execution state.
This skill is the method for building a graph worth holding.

## 1. Nodes are artifacts

Every node names what can be inspected when it is done:
- a document;
- code with tests;
- a published release;
- a deployed service with evidence;
- an evidence report.

Put it in `metadata.artifact`.

- **Activities become artifacts.** "Review", "rulings", "gate" and "run" become what they
  produce: a sign-off recorded in the contract, a decision record, an evidence report.
- **Review that accepts a deliverable belongs to that deliverable's acceptance.** It is not a
  separate node. Add the reviewer's time to the estimate.
- **Mark acceptance points** with `metadata.milestone: true`.
- **Give each node an executor class** in `metadata.owner` (e.g. `agent`, `worker`, `owner`).
  The resource schedule uses it.

## 2. Edges are consumption

`B ← A` only when **B consumes something specific that A produces**. Record what in
`metadata.consumes[A]`. If nothing is consumed, there is no edge.

**Common false edges:**
- "A should happen first" with nothing consumed (e.g. publishing a pipeline before its content
  exists);
- two deliverables touching the same repo but not the same files;
- a downstream gate listing every upstream deliverable (keep only the direct ones; the rest
  are implied).

**Common missing edges:**
- **a release consuming the features it ships;**
- **a decision record consumed by implementation:** masking policy, naming, identity scheme;
- **external state consumed by acceptance:** something provisioned, something deployed;
- **a file another deliverable already changed** (sequence them, or split the file);
- **a contract, schema or format** consumed by both producer and consumer;
- **a fast-tracked edge hiding real consumption,** which shows up later as rework.

## 3. Lint until clean

```bash
python3 skills/deliverable-cpm/scripts/graph_lint.py graph.json
```

It reports:
- cycles;
- **redundant edges** (implied by another path);
- **edges without rationale;**
- **deliverables that feed no milestone** (extra work, or a missing milestone);
- **nodes without an artifact;**
- **unordered deliverables sharing a file.**

Fix each finding. Don't suppress it. A deliverable that feeds no milestone is either cut or
gets the milestone it actually serves.

## 4. Submit and verify the critical path

Submit with `plan.submit`, then read `plan.status`. **Verify** that every consecutive pair in
`critical_path` is a prerequisite edge and that `critical_path_hours` equals the longest path.
Planner versions have reported non-edge links (praxec/cpm-planner#18). If it fails, trust the
graph, not the report.

## 5. Level resources

```bash
python3 skills/deliverable-cpm/scripts/resource_schedule.py graph.json '{"agent":1,"worker":5,"owner":1}'
```

CPM assumes unlimited workers. The levelled **makespan** and **driving chain** are the real
schedule. A resource pool loaded far above its makespan share is the bottleneck.

## 6. Crash, measuring every move

Try one change at a time, re-lint and re-schedule. Keep it only if makespan or a milestone date
improves.

| Move | What it does | Watch for |
|---|---|---|
| **Add capacity** to the bottleneck pool | more workers on file-disjoint deliverables | diminishing returns; review becomes the next bottleneck |
| **Move work to a cheaper pool** with review time added | frees the scarce resource | security- or judgment-heavy work stays put |
| **Split a deliverable** along file or contract seams | more parallel lanes | each part still needs its own artifact |
| **Fast-track** by removing an edge | earlier start | only if nothing was actually consumed; otherwise it's hidden rework |
| **Split scope into milestones** | earlier first value | each milestone has its own acceptance artifact |
| **Contract first** between teams or agents | lanes build against the contract and fakes | the contract must be accepted, not just drafted |

Record each move and its measured effect in the plan. Rejected moves count as results too.

## 7. Baseline earned value
- **BAC** = sum of estimates. **PV** = estimates placed on the levelled schedule.
- **Earning:** deliverables ≤ 2 h earn 0/100. Longer ones earn 50% at tests committed and 100%
  at acceptance.
- **Earned only after manager acceptance** of the artifact, not when the worker reports done.
- **Track** SPI = EV/PV, CPI = EV/AC, EAC = BAC/CPI. Re-plan when either index stays below 0.9.

## 8. Ticket what isn't specified yet

Any deliverable whose artifact or acceptance can't yet be stated precisely becomes a ticket in
the repo that will produce it. The ticket carries:
- the artifact and acceptance criteria;
- the deliverables it consumes and the ones that consume it, by id;
- open questions.

Lanes owned by other agents or machines run from their tickets. Shared contracts are tickets
first.

## 9. Execute
- **Claim work:** `plan.acquire_cohort` hands out ready, file-disjoint deliverables.
- **Keep the lease:** heartbeat long work.
- **Close work:** `plan.mark_status` complete or failed, only after acceptance.
- **Re-plan:** submit the revised graph and re-baseline. Today that creates a new plan id
  (praxec/cpm-planner#23), so carry statuses over explicitly.
