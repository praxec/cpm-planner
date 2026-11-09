# Delegating plan.review to another process

By default `plan.review` calls OpenRouter from the server process, which then needs the key. Set
`CPM_REVIEW_PROVIDER=command` to have an adapter you control answer instead, so the credentials stay
inside that adapter. Use this when a coordinator should stay least-privileged, for example when
Junior or OPV already holds the approved route. An unavailable adapter reports `review_unavailable`
and never falls back to a direct call. Findings stay advisory, and proposals still have to pass the
same deterministic simulation.

```sh
export CPM_REVIEW_PROVIDER=command
export CPM_REVIEW_COMMAND=/opt/cpm/review-adapter
export CPM_REVIEW_COMMAND_ENV=OP_SERVICE_ACCOUNT_TOKEN   # optional, names only
cpm-planner review doctor
```

`review doctor` prints a names-only report and exits 0 when the provider is ready:

```json
{"provider":"command","ready":true,"reason":null,"checks":[{"name":"CPM_REVIEW_COMMAND","present":true}]}
```

## Adapter protocol, version 1

The adapter is started without a shell or arguments, once per review. It gets one JSON object on
stdin and writes one to stdout, within `CPM_LLM_TIMEOUT_SECS` (default 30).

```json
{"schema_version": 1, "kind": "plan.review.judgment", "state": {}, "questions": {}}
```

`state` and `questions` are the same payload a direct review sends: the plan's review state and the
typed questions (`noul`, `score` or `choice`). An answer looks like this, with `usage` optional:

```json
{"schema_version": 1, "status": "ok", "model": "typesafe/jev-1.13",
 "answers": {"q00": {"type": "noul", "noul": 0.2}}}
```

A refusal looks like this:

```json
{"schema_version": 1, "status": "refused", "reason": "policy_denied"}
```

| Outcome | `failure_class` |
|---|---|
| The adapter cannot start or its pipes fail | `transport` |
| No reply in time; the adapter is killed | `timeout` |
| Non-zero exit | `upstream` |
| Not JSON, a wrong `schema_version`, or over 16 MiB | `decode` |
| `status: "refused"` | `refused` |

## Secret boundary

- The request holds only review state and questions, never a credential.
- The adapter's environment is cleared, then given `PATH`, `HOME`, the platform temp and system
  variables, and whatever `CPM_REVIEW_COMMAND_ENV` names. Naming `OPENROUTER_API_KEY` or
  `CPM_OPENROUTER_KEY_FILE` there is refused.
- Adapter stderr is discarded. `reason` and `model` are kept only when they are short tokens of
  letters, digits and `._:/-`, so a log line or key echoed there never reaches a result.

## Junior and OPV

Point `CPM_REVIEW_COMMAND` at a small script that gets its credential through your approved route
and passes stdin to Jev. With OPV, have the script start the judgment call under `opv` so the
credential exists only for that process. This repository ships no Junior-specific adapter: it needs
a Junior command that accepts this request, which you can wrap in the same way once one exists.
