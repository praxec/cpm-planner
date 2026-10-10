---
name: cpm-revise
description: "Use when the definition of a cpm-planner plan (plan.* tools) must change, before or during execution; when an approved definition change must be applied: scope changes, a deliverable is added, removed, split or re-estimated for real, an edge is wrong, a contract changes, plan.status shows definition_drift, or the plan file and the stored head disagree. Covers editing the .cpm-planner/plans/<name>/<variant>.json file, plan.lint, plan.sync with its diff (added, removed, changed, reopened, released_locks), the carry-over rules, definition_drift true/false/null, and plan.export."
---

# cpm-revise: change the file, sync, resolve reopened work

The plan file is the definition. Every change goes through it. The full method is in
[the deliverable-cpm skill](../deliverable-cpm/SKILL.md) (section 9).
Summarises deliverable-cpm; if they ever disagree, the server's behaviour wins.

## When to use

- Scope or estimates change, or an edge turns out to be false or missing.
- `plan.status` shows `definition_drift: true` or `null`.
- Not for: trying an idea as a variant (split, crash, fast-track) → use cpm-improve.

## 1. Edit, lint, sync

1. Edit `.cpm-planner/plans/<name>/<variant>.json` (the selected variant's file).
2. Lint until clean:

```text
plan.lint {"path": ".cpm-planner/plans/<name>/<variant>.json"}
```

3. Sync:

```text
plan.sync {"path": ".cpm-planner/plans/<name>/<variant>.json"}
```

- Optional: `force: true` releases live locks of removed deliverables. Without it, removing
  a leased deliverable is refused with `LOCK_HELD`. Prefer waiting for the holder.
- Any `name`, `variant` or `project` passed must match the path.
- Archived variants refuse sync (`ARCHIVE_REFUSED`).

Don't hand-write `plan.revise` graphs. It exists (`{plan_id, graph, force?}`), but it
changes the head inline and leaves the file stale.

## 2. Read the diff

The response has `revision` and `diff` with `added`, `removed`, `changed`, `reopened` and
`released_locks`. Carry-over rules:
- an unchanged deliverable keeps its status and counters;
- a changed deliverable keeps its status, unless its set of prerequisite ids changed. Then
  a complete, ready or pending deliverable is re-derived, and so are its dependents,
  transitively;
- a deliverable with an `interface` edge to a changed `metadata.contract: true`
  deliverable is reopened;
- new deliverables start ready or pending, according to their prerequisites;
- removing a leased deliverable is refused with `LOCK_HELD` unless `force: true`;
- in-progress leases survive, and failed deliverables stay failed.

Resolve every id in `reopened`: re-run it, or re-accept it with evidence if the artifact
still meets the new criteria (`cpm-run`). Tell holders of `released_locks` their lease is
gone.

## 3. Check drift

```text
plan.status {"plan_id": "<plan_id>"}
```

`definition_drift`:
- `false`: the file matches the last synced or exported bytes, or its graph equals the
  head (re-formatting is not drift). Good.
- `true`: the file's graph differs from the head, or does not parse. If you edited the
  file, lint and sync it. If the head changed inline (`plan.revise` or
  `plan.sync {graph}`), write the head back:

```text
plan.export {"plan_id": "<plan_id>"}
```

  `plan.export` takes optional `path` (a confined root-relative path) and `force`. It
  refuses another variant's tracked file, or a file with unsynced local edits, unless
  `force: true`. Check whether those local edits matter before forcing.
- `null`: unknown. No project root for the plan's project, no tracked file, or the file is
  missing, unreadable or over 8 MiB. Find out which and fix it (sync from the file).

## 4. After the revise

- Check `critical_path` and `milestones` again (`cpm-plan` section 4); level again with
  `plan.schedule` if capacity matters.
- If scope changed, re-baseline with a reason: `plan.baseline {plan_id, reason}` (`cpm-ev`).
- Commit the plan file with the code it describes.

## Stop when

- lint is clean, the sync succeeded, `definition_drift` is `false`;
- every `reopened` deliverable is resolved or handed to `cpm-run`;
- an approved scope change is re-baselined with its reason.

## Pitfalls

- An unnamed plan (from `plan.submit` without `name`) cannot be adopted into a named line;
  its completions have to be re-accepted after syncing the named file.
- Changing a deliverable's prerequisite ids re-derives it and its dependents, so a
  completed deliverable can return to ready. Expect it before you change edges.
- Removing a deliverable keeps its recorded hours in AC; a baselined one reports status
  `removed` in `plan.ev`.
- Planner refusals (`LOCK_HELD`, `INVALID_GRAPH`, ...) come back as JSON-RPC `-32603`
  internal errors; read the code in the message.
