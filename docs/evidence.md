# Evidence: identity, staleness and scope

This is the version 1 contract for verification evidence in a plan. A piece of
evidence names the scope of the check it records (a focused test, a full
regression gate, an owner's approval, a merge) and the identity of what was
checked: the source tree or image, the dependency lock, the toolchain, the test
contract and the environment. From those two facts the planner can say whether
evidence still applies to a target, and which kinds of deliverable it may close.

The contract serves three implementations that are built against it
independently:

- the evidence store (`src/evidence.rs`), which records evidence with its
  identity on `plan.mark_status` and `plan.accept`, checks its scope, and
  computes current or stale;
- the gate-scope lint findings (`src/lint.rs`, `src/waves.rs`), which check that
  the plan file declares scopes and candidates;
- the plan overview (`src/overview.rs`), which shows each deliverable's
  evidence status.

Plain evidence strings keep working exactly as they do now. Nothing in this
contract applies to a plan that never sends an evidence record or declares an
evidence scope.

## Evidence today

`plan.mark_status` takes an optional `evidence` string of at most 2048
characters. It is appended to the deliverable's list in `ev_actuals.evidence`,
a JSON array of strings with at most 100 entries; a mark carrying evidence past
that is refused with `INVALID_ACTUALS`. `plan.accept` requires an `evidence`
string, but writes it only to the `plan.deliverable.accepted` audit event.
`plan.select` merges two lists and keeps the newest 100. No read call returns
the list, and nothing compares evidence with anything.

Gates (`metadata.kind: "gate"`) and manual deliverables
(`metadata.kind: "manual"`) are never leased; they close through `plan.accept`
or a lockless `plan.mark_status`. The [delivery-gate guidance](../skills/deliverable-cpm/SKILL.md#software-delivery-code-in-bulk-gate-at-milestones)
asks authors to record the evidence identity in gate metadata as prose. This
contract makes that identity structured and checkable.

## Evidence entries

Wherever a call accepts `evidence`, it accepts either a string or an evidence
record object. The MCP input schema for the field becomes
`{"oneOf": [{"type": "string"}, {"type": "object"}]}`.

- A **string** is a note. It is stored and validated exactly as today. It has no
  scope and no identity, so its status is always `unbound`, and it can never
  satisfy a declared evidence scope.
- A **record** is the structured form below.

### Record, schema version 1

```json
{
  "schema_version": 1,
  "scope": "gate",
  "summary": "cargo nextest run --all-targets: 812 passed, 0 failed",
  "ref": "https://ci.example/runs/4411",
  "identity": {
    "schema_version": 1,
    "tree": "git-tree:4b825dc642cb6eb9a060e54bf8d69288fbee4904",
    "dependency_lock": "sha256:9f2c0e1a5b7d3c4e8f6a1b2c3d4e5f60718293a4b5c6d7e8f9a0b1c2d3e4f5a6",
    "toolchain": "rustc 1.89.0; cargo-nextest 0.9.104",
    "test_contract": "nextest:all-targets@sha256:51d2e0c4a8b6f3e1d7c9a2b4e6f8a0c1d3e5f7a9b1c3d5e7f9a1b3c5d7e9f1a3",
    "environment": "linux-x86_64; ubuntu-24.04; runner-image:sha256:0d4f2a6c"
  }
}
```

| Field | Required | Rule |
|---|---|---|
| `schema_version` | yes | The integer `1`. Any other value is refused. |
| `scope` | yes | One of the [scopes](#scopes). |
| `summary` | yes | 1 to 2048 characters: what was checked and the result. |
| `ref` | no | At most 2048 characters: a run, pull request, commit or document. Provenance only; never fetched. |
| `party` | only for `external_signoff` | 1 to 256 characters: who outside the team signed off. Refused on other scopes. |
| `identity` | per scope, see [required identity fields](#required-identity-fields) | An [evidence identity](#evidence-identity-schema-version-1). |

Unknown fields are refused, as everywhere else in the wire model.

### Fields the server adds

When it stores a record, the planner adds these fields. A caller that sends any
of them is refused with `INVALID_EVIDENCE`.

| Field | Value |
|---|---|
| `recorded_at` | RFC 3339 UTC time of the call. |
| `recorded_by` | The `caller_id` of `plan.mark_status`, or the `accepted_by` of `plan.accept`. |
| `via` | `"mark_status"` or `"accept"`. |
| `plan_revision` | The plan revision the record was stored against. |
| `definition_digest` | `sha256:` plus the lowercase hex SHA-256 of the deliverable's canonical definition at that revision (see [the definition field](#the-definition-field)). |

## Evidence identity, schema version 1

An identity names the inputs that govern whether a verification result still
holds. The planner never computes, fetches or normalises these values; the
caller supplies them and the planner compares them byte for byte.

| Field | Names | Recommended form |
|---|---|---|
| `tree` | The exact code or artifact checked. | `git-tree:<hex>` from `git rev-parse HEAD^{tree}`; `oci:sha256:<hex>` for an image; `git-commit:<hex>` is accepted but stricter, since a rebase with an identical tree changes it. |
| `dependency_lock` | The resolved dependency set. | `sha256:<hex>` of the lockfile bytes. With several lockfiles, the SHA-256 of their `<path>\0<sha256 hex>\n` lines sorted by path. `none` when the project has no lockfile. |
| `toolchain` | The compilers and test runners. | Each tool's own version output, joined with `; ` in sorted order. |
| `test_contract` | Which tests were run and how they were selected. | `<runner>:<selection>@<version or digest>`, where the digest covers the test configuration. It changes when tests are added, removed or reselected. |
| `environment` | Where the check ran. | OS and architecture, runner image digest, and any service endpoints the tests used, joined with `; `. |

Every field value is a string of 1 to 256 characters with no control characters
and no leading or trailing whitespace. The object also carries
`"schema_version": 1`; unknown fields and other versions are refused. Keep
secrets out of identities: they are stored and returned verbatim.

### Required identity fields

| Scope | `identity` | Fields required when present |
|---|---|---|
| `focused`, `gate` | required | all five |
| `merge`, `publish` | required | `tree` (the merged tree, or the published artifact's digest) |
| `activation` | required | `tree` and `environment` |
| `manual_approval`, `external_signoff` | optional | none; any subset may be given |

A field left out of a record is not governed by it: changing that input never
makes the record stale.

## Scopes

A scope says what a record proves. The vocabulary is closed; a scope outside it
is refused with `INVALID_EVIDENCE`.

| Scope | Proves | Typical source |
|---|---|---|
| `focused` | The deliverable's own cheap checks passed: tests for the changed code and its contracts and consumers, lint and format of touched files, an incremental build. | The worker, on `mark_status complete`. |
| `gate` | A full regression, end-to-end or CI run passed on one integrated candidate. | A gate's `plan.accept`. |
| `manual_approval` | A named owner approved the deliverable's artifact. | An owner's `plan.accept`. |
| `external_signoff` | A party outside the team accepted a contract or artifact. | An owner's `plan.accept`, with `party`. |
| `merge` | A change was merged into a protected branch. | The merge step. |
| `publish` | An artifact was published: a tag, release, package or image push. | The release step. |
| `activation` | A published artifact was activated in an environment, such as a production deploy or a flag flip. | The deploy step. |

The scopes do not nest. Only one implication holds: a `gate` record may also
close a work deliverable, because a full run of the integrated candidate
contains the focused checks. A `focused` record never satisfies a gate, and a
`gate` record never satisfies an approval, a sign-off, a merge, a publish or an
activation.

## Which scope closes which deliverable

A deliverable's kind is its `metadata.kind`: `"gate"`, `"manual"`, or anything
else (including absent), which is **work**. Milestones carry no evidence
requirement and are outside this section. A milestone that is also a gate (`"milestone": true`
with `metadata.kind: "gate"`) follows the gate rules and must declare a scope.

A deliverable may declare the scope it must be closed with in
`metadata.evidence_scope`. When it does not, the kind's default applies.

| Kind | May declare `evidence_scope` | Default | Scopes that may close it |
|---|---|---|---|
| work | `focused` | `focused` | `focused`, `gate` |
| `gate` | `gate`, `merge`, `publish`, `activation` | `gate` | exactly the declared or default scope |
| `manual` | `manual_approval`, `external_signoff`, `merge`, `publish`, `activation` | none | the declared scope; with no declaration, any scope |

A `gate` deliverable may also declare `metadata.candidate`: the id of the
direct prerequisite that produces the integrated candidate it verifies (an
integration-candidate deliverable, or the last work deliverable before it). The
candidate is what makes a gate's evidence checkable without a caller-supplied
target; see [target resolution](#target-resolution).

### Closing rules

These apply to a closing call, which is `plan.mark_status` with
`status: complete` or `plan.accept`, and are checked inside the call's
transaction before anything is written.

1. **A record whose scope may not close the deliverable is refused on any
   call**, closing or not, with `EVIDENCE_SCOPE_MISMATCH`. For example, a
   `focused` record on a gate, or a `gate` record on a manual deliverable that
   declares `manual_approval`. A string is never refused for its scope.
2. **A declared scope must be met.** When the deliverable has an explicit
   `metadata.evidence_scope`, the closing call must carry a record of an allowed
   scope, or the deliverable must already hold one whose `definition_digest`
   equals the current one. Otherwise the call is refused with
   `EVIDENCE_REQUIRED`. Strings do not count.
3. **A gate's record must match its candidate.** When a gate declares
   `metadata.candidate` and the candidate holds a record with an identity, the
   closing record (rule 2's record, or the newest allowed one) must be `current`
   against that identity. Otherwise the call is refused with `EVIDENCE_STALE`,
   naming the changed fields. If the gate has no allowed record at all, the
   call is refused with `EVIDENCE_REQUIRED`, and an `unknown` verdict fails
   this rule just as `stale` does.
4. **Without a declaration, closing works as today**: `plan.mark_status
   complete` needs no evidence and `plan.accept` needs a string or a record.
   Rule 1 still applies.

An idempotent `plan.accept` of a deliverable already `Complete` stays a no-op
and stores nothing. A record that rule 1 refuses rolls back the whole call,
like any other refused mark.

### Storage

`plan.accept` now appends its evidence (string or record) to `ev_actuals.evidence`
as well as to its audit event, so a gate's or owner's evidence is visible to
reads. Because acceptance must not be blocked by bookkeeping, an accept on a
deliverable already at 100 entries drops the oldest entry first, the same rule
`plan.select` uses. `plan.mark_status` keeps refusing at 100 entries.

`ev_actuals.evidence` stays one JSON array. Each element is either a string
(a note, as stored today) or a stored record object. Existing rows need no
migration. The 100-entry limit counts both kinds together, and `plan.select`
keeps the newest 100 of the merged list, as now.

### Errors

| Code | When |
|---|---|
| `INVALID_EVIDENCE` | A record or identity is malformed: unknown field or `schema_version`, unknown scope, a value out of bounds, a required identity field missing, `party` on the wrong scope, or a server-added field supplied by the caller. |
| `EVIDENCE_SCOPE_MISMATCH` | Rule 1. The message names the deliverable, its kind, the record's scope and the scopes allowed. |
| `EVIDENCE_REQUIRED` | Rule 2. The message names the declared scope. |
| `EVIDENCE_STALE` | Rule 3. The message names the candidate and the changed fields. |

String evidence keeps its current checks and codes (`INVALID_ACTUALS` on
`plan.mark_status`).

## Current or stale

### Target resolution

A record is judged against a **target identity**: an identity object of the
same schema in which every field is optional. For a record held by deliverable
`D`, the target is the first of:

1. the `evidence_target` argument of the read call (for example
   `plan.overview`), which applies to every record in the plan;
2. when `D` is a gate that declares `metadata.candidate: C`, the identity of the
   newest record held by `C` that has an identity;
3. no target.

The status reports which one was used as `target_source`: `"argument"`,
`"candidate:<id>"` or `null`.

### The definition field

Besides the five identity fields, every record is judged on `definition`: its
`definition_digest` against the digest of the deliverable's current canonical
definition. The canonical definition is the one `plan.revise` uses to decide
that a deliverable `changed` (`canonical_deliverable` in `src/planner.rs`), so a
record goes stale on `definition` exactly when a revision lists its deliverable
as changed. That includes a metadata-only change. `definition` needs no target.

### Verdict

For each identity field the record holds, in the fixed order `tree`,
`dependency_lock`, `toolchain`, `test_contract`, `environment`:

- the target has the field with the same value: it matches;
- the target has the field with a different value: it is **changed**;
- the target lacks the field (or there is no target): it is **unknown**.

Then `definition` is compared and, when the digests differ, added last to the
changed list. The verdict is:

| Verdict | When |
|---|---|
| `stale` | At least one field changed. |
| `unknown` | Nothing changed, but at least one field is unknown. |
| `current` | Every field the record holds matches, and so does `definition`. |
| `unbound` | The entry is a string. |

So a changed dependency lock, toolchain or test contract makes evidence stale
even when the tree is identical.

A verdict is reported, never acted on. Stale evidence does not reopen a
deliverable, change its status or release a lease; the operator does that with
`plan.revise` or a mark. Computing a verdict is read-only and deterministic: the
same plan, evidence and target always give the same result.

### Status of one record

```json
{
  "index": 3,
  "scope": "gate",
  "verdict": "stale",
  "target_source": "argument",
  "changed": [
    {"field": "dependency_lock", "recorded": "sha256:9f2c...", "target": "sha256:77ab..."},
    {"field": "definition", "recorded": "sha256:01de...", "target": "sha256:c3f0..."}
  ],
  "unknown": [],
  "recorded_at": "2026-11-10T13:41:52Z",
  "recorded_by": "ci"
}
```

`index` is the entry's position in the deliverable's evidence list, from 0.
`changed` keeps the field order above, then `definition`; `unknown` lists field
names in the same order. A string entry has `scope: null`, `verdict:
"unbound"`, `target_source: null` and empty lists.

### Status of a deliverable

A deliverable's evidence status is the status of its **governing record**:
the newest record whose scope may close it, else the newest record of any scope.

```json
{
  "deliverable_id": "regression-gate",
  "required_scope": "gate",
  "scope_source": "default",
  "entries": 4,
  "verdict": "stale",
  "record": { "index": 3, "scope": "gate", "verdict": "stale", "...": "..." }
}
```

| Field | Value |
|---|---|
| `required_scope` | The declared or default scope; `null` for an undeclared manual deliverable or a milestone. |
| `scope_source` | `"declared"` (from `metadata.evidence_scope`), `"default"`, or `null` with no required scope. |
| `entries` | Number of evidence entries of both kinds. |
| `verdict` | The governing record's verdict; `unbound` when the deliverable holds only strings; `none` when it holds nothing. |
| `record` | The governing record's status, or `null`. |

## Lint findings

The plan-file side is checked by `plan.lint`, deterministically, with a
remediation in each message. Lint never rewrites a plan.

| Code | Severity | Condition | Remediation |
|---|---|---|---|
| `EVIDENCE_SCOPE_INVALID` | error | `metadata.evidence_scope` is not a scope, or is not allowed for the kind (the table above). | Use a scope the kind may declare, or change the kind. |
| `GATE_SCOPE_MISSING` | warning | A gate has no `metadata.evidence_scope`. | Declare `gate` for a regression or CI run, or `merge`, `publish` or `activation` for that step. |
| `GATE_CANDIDATE_MISSING` | warning | A gate whose scope is `gate` has no `metadata.candidate`. | Name the direct prerequisite that produces the integrated candidate. |
| `GATE_CANDIDATE_INVALID` | error | `metadata.candidate` is not a string naming a direct prerequisite. | Point it at a direct prerequisite, or add that edge. |
| `SCOPE_FOLDED` | warning | A work or gate deliverable's scanned text names an operation of another scope, or a gate's text names a merge, publish or activation step its own scope is not. | Split that operation into its own deliverable (`manual` for approvals and sign-offs) with the matching `evidence_scope`, or set `metadata.scope_folded: false` if the text only mentions it. |

`SCOPE_FOLDED` scans the same text as `UNDECLARED_GATE` (the id and
`metadata.artifact`, `description`, `acceptance` and `verify`), lowercased and
matched on whole words. Its phrase lists are owned by the lint and cover at
least: approval and approve (`manual_approval`); sign off, sign-off and
signoff (`external_signoff`); merge the PR and merge to main (`merge`); publish,
release to and tag the release (`publish`); deploy to production, activate and
roll out (`activation`). A `manual` deliverable is never `SCOPE_FOLDED`.

## Overview

`plan.overview` shows the deliverable status above for each row it lists, and
accepts an optional `evidence_target` identity object, passed to
[target resolution](#target-resolution). Its compact text form is the verdict
followed by the changed field names, for example
`evidence: stale (dependency_lock, definition)`. Its stale-evidence list holds
every deliverable whose verdict is `stale`, complete deliverables first, then
by id. JSON output uses the shapes above unchanged.

## Backward compatibility

- A plan that sends only strings and declares no `metadata.evidence_scope`
  behaves as before: the same calls succeed and fail with the same codes, and
  stored strings read back unchanged.
- Gates and manual deliverables keep their default scopes, so rule 1 refuses
  only an explicitly wrong record, which no existing caller can send.
- The lint findings are new warnings on existing plans that use gates
  (`GATE_SCOPE_MISSING`, `GATE_CANDIDATE_MISSING`), never new errors unless a
  plan already uses `metadata.evidence_scope` or `metadata.candidate` with
  another meaning. The release compiler emits `manual` nodes, not gates, so its
  output is unaffected.
- `metadata.evidence_scope`, `metadata.candidate` and `metadata.scope_folded`
  are new keys. Adding one to a plan changes the deliverable's canonical
  definition, so `plan.sync` reports it as changed, as any metadata edit is.

## Release workflows

The release compiler's `delivery_gates[].identity` and `retained_identity`
([release workflows](release-workflows.md)) are a separate input format and do
not change. They carry `tree`, `dependency_lock`, `test_contract` and
`environment`, with the toolchain folded into `dependency_lock`. To reuse such
an identity as a version 1 identity, copy the four fields across and leave
`toolchain` out; a record built that way does not govern the toolchain.
