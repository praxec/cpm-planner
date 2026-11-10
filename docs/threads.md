# Threads: keep related work with one agent

A thread groups deliverables that one agent session should do in order, so
the context it builds on one step carries into the next. Examples: the
failing tests and then the implementation of one feature, or a contract and
its first consumer. The planner remembers who last advanced each thread and
offers that caller the thread's next ready deliverable first.

Thread affinity is a preference, never a lock. It never bypasses file
conflicts, prerequisites, manual and gate exclusion, the lapse limit or the
circuit breaker. A plan with no `metadata.thread` leases exactly as before,
in the same order.

## Declaring a thread

Put a token in `metadata.thread` on each deliverable of the thread:

```json
{"id": "evidence-binding", "owned_files": ["src/evidence.rs"],
 "prerequisites": ["evidence-contract"],
 "metadata": {"thread": "evidence", "artifact": "src/evidence.rs ..."}}
```

A token is 1 to 64 characters: ASCII letters, digits, `.`, `_` and `-`,
starting with a letter or digit. The planner treats a deliverable whose
`metadata.thread` is not a valid token, or not a string, as unthreaded. The
`acquire_cohort` `thread` argument follows the same rule and is refused with
`INVALID_VALUE` when it breaks it. The rule is `cpm_planner::affinity::thread_token_problem`,
which the lint shares.

A thread should be one dependency chain: each deliverable after the first
consumes the one before it, directly or through deliverables of the same
thread. A thread split across parallel branches, or interleaved with another
thread's deliverable, cannot stay with one agent; the lint reports it as
`THREAD_BROKEN`.

## Sizing deliverables per thread

- **Thread the steps that share context, not everything one person owns.** A
  thread is worth having when the second step is cheaper for the agent that did
  the first: the same files, the same contract, the same failing test.
- **Keep each step a whole artifact.** Threading is not a reason to cut work
  into micro-deliverables; each step still has its own artifact, acceptance
  criteria and owned files, and could be done by a fresh agent if it had to.
- **Keep threads short.** Two to five deliverables, a few hours to a working
  day in total, is a sensible range; that is what one agent session holds in
  context. Split a longer chain into two threads at a natural hand-off, such as
  a contract document another agent can read.
- **Leave parallel work unthreaded.** Deliverables that can run beside the
  thread belong to other threads or none, so other agents can take them while
  the thread's agent works.
- **Name threads by the line of work** (`evidence`, `overview`, `lint`), not by
  the agent; who holds a thread is decided at lease time.

## How leasing uses threads

When a caller completes a threaded deliverable with `plan.mark_status`, the
planner records a **handoff** for that thread: the caller and the time. One
handoff per thread is kept; each completion replaces it. `plan.accept`,
failures, `plan.force_release` and lapsed leases are not completions by an
executor and record nothing.

`plan.acquire_cohort` then ranks each ready deliverable for the requesting
caller, and leases in that order (ties keep the usual priority order):

1. **Affine**: the request's explicit `thread`, or, without one, every thread
   this caller last advanced.
2. **Neutral**: unthreaded deliverables, and threads nobody has advanced.
3. **Another caller's**: threads another caller last advanced.

So the agent that finished a step is offered the next one first, and other
agents take other work first. The ranking only reorders the deliverables the
acquire would lease anyway.

A requester can also **defer** with `affinity_seconds`: a ready deliverable
whose thread another caller advanced less than `affinity_seconds` ago is held
back from this request and reported in `blocked` with code `AFFINITY_HOLD`,
naming that caller and the seconds left. The default is 0, no hold. Values
above the planner's maximum lease TTL are clamped to it. After the window,
any caller may lease the deliverable. A caller is never held back from its own
thread.

A handoff is honoured only while it still describes the current plan: its
deliverable still exists, is `Complete` and still carries the same thread. A
revision that removes the deliverable or moves it to another thread makes the
handoff inert, so it cannot grant preference to the wrong work.

Handoffs are stored in the `thread_handoffs` table (schema v7), written in the
same transaction as the completing mark.

## Threads in `plan.status`

`plan.status` has a `threads` list, sorted by token, when the plan has
threads (the key is absent otherwise):

```json
{"threads": [{"thread": "evidence", "holder": null, "affine_caller": "agent-evidence",
              "next_ready": "thread-affinity", "completed": 2, "total": 3}]}
```

| Field | Value |
|---|---|
| `thread` | The token. |
| `holder` | The caller with a live lease on one of the thread's deliverables (the smallest id when several do), else `null`. |
| `affine_caller` | The caller of the thread's current handoff, else `null`. |
| `next_ready` | The thread's first ready, unleased deliverable in graph order, else `null`. |
| `completed`, `total` | How many of the thread's deliverables are complete, and how many there are. |

The rows are read from one snapshot of their own, so they are consistent with
each other; the rest of the status comes from the status call's snapshot.

## Library API

`AcquireRequest::with_thread(token)` and
`AcquireRequest::with_affinity_seconds(seconds)` set the two request fields.
`cpm_planner::affinity` holds the token rule, `thread_of`, and the
`ThreadStatus` row.
