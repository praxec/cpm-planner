# Delegating plan.review to another process

By default `plan.review` calls OpenRouter from the server process, which then needs the key. Set
`CPM_REVIEW_PROVIDER=command` to have an adapter you control answer instead, so the credentials stay
inside that adapter. Use this when the process running cpm-planner should stay least-privileged and
some other process you trust can make the judgment call. An unavailable adapter reports
`review_unavailable` and never falls back to a direct call. Findings stay advisory, and proposals
still have to pass the same deterministic simulation.

```sh
export CPM_REVIEW_PROVIDER=command
export CPM_REVIEW_COMMAND=/opt/cpm/review-adapter
export CPM_REVIEW_COMMAND_ENV=REVIEW_ADAPTER_PROFILE   # optional, names only
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
typed questions (`noul`, `score` or `choice`), keyed `q00`, `q01` and so on. An answer looks like
this, with `model` and `usage` optional:

```json
{"schema_version": 1, "status": "ok", "model": "typesafe/jev-1.13",
 "answers": {"q00": {"type": "noul", "noul": 0.2}},
 "usage": {"input_tokens": 812, "output_tokens": 64, "total_tokens": 876}}
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

An answer that is missing, of the wrong type or out of range yields no finding for that question;
it does not fail the review.

### Conformance fixtures

These files are the contract in example form. Test an adapter against them without reading
cpm-planner's source:

- [request-v1.json](review-provider/fixtures/request-v1.json): the request a review of a two-deliverable
  plan (`parser`, then `cli`) sends. Your adapter must accept it.
- [answer-v1.json](review-provider/fixtures/answer-v1.json): a valid answer to every question in that
  request.
- [refused-v1.json](review-provider/fixtures/refused-v1.json): a valid refusal.

cpm-planner's own tests parse each fixture with the protocol types, check that a review of that plan
sends exactly `request-v1.json`, and run a review end to end through an adapter that replies with
`answer-v1.json`, so the fixtures cannot drift from the code.

## Provenance

Every review a configured judge handles records where its answer came from, without credentials:

| Field | Meaning |
|---|---|
| `provider` | `direct` or `command`, set even when the adapter failed |
| `endpoint` | the direct endpoint's host, or `command-adapter` |
| `prompt_hash` | sha256 of the canonical `{questions, state}`: the request digest |
| `question_count` | the questions sent |
| `model` | the model token the adapter reported, or `unknown` |
| `usage` | the token counts and cost the adapter reported, when it did |

## Secret boundary

- The request holds only review state and questions, never a credential.
- The adapter's environment is cleared, then given `PATH`, `HOME`, the platform temp and system
  variables, and whatever `CPM_REVIEW_COMMAND_ENV` names. Naming `OPENROUTER_API_KEY` or
  `CPM_OPENROUTER_KEY_FILE` there is refused.
- Adapter stderr is discarded. `reason` and `model` are kept only when they are tokens of at most
  64 letters, digits and `._:/-`, with no run of more than 23 letters and digits between those
  separators. A log line, a sentence or a key-shaped value echoed there never reaches a result:
  `model` becomes `unknown` and `reason` becomes `unspecified`.

## Writing an adapter

An adapter is any executable that speaks the protocol above. It decides where its credential comes
from and which endpoint answers. Keep it small:

1. Read all of stdin and check `schema_version` is 1. Otherwise refuse.
2. Get the credential through a route you control (a file only the adapter's user can read, an
   agent socket, a workload identity). Never print it, and never pass it as an argument, since
   other users can read a process's argv.
3. Forward `state` and `questions` unchanged to a judgment endpoint, within the timeout.
4. Write one reply. Exit 0 for `ok` and `refused`; exit non-zero for anything that went wrong.

This reference adapter reads an OpenRouter key from a file and calls the same System One endpoint a
direct review uses. It needs `sh`, `curl` and `jq` on the forwarded `PATH`.

```sh
#!/bin/sh
# plan.review adapter, protocol v1: forwards one judgment to a System One endpoint.
set -eu
KEY_FILE=/etc/cpm-review/key          # chmod 600, owned by the user that runs the adapter
ENDPOINT=https://openrouter.ai/api/v1/systemone
MODEL=typesafe/jev-1.13

refuse() { printf '{"schema_version":1,"status":"refused","reason":"%s"}\n' "$1"; exit 0; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
cat >"$tmp/request.json"
[ "$(jq -r .schema_version "$tmp/request.json")" = 1 ] || refuse unsupported_schema_version
[ -r "$KEY_FILE" ] || refuse no_credential

jq --arg model "$MODEL" '{model: $model, state, questions}' "$tmp/request.json" >"$tmp/body.json"
# The key goes to curl on stdin as a header, never in argv.
printf 'Authorization: Bearer %s\n' "$(cat "$KEY_FILE")" |
  curl --silent --fail --max-time 25 -H @- -H 'Content-Type: application/json' \
    --data-binary @"$tmp/body.json" "$ENDPOINT" >"$tmp/reply.json"

jq '{schema_version: 1, status: "ok", model, answers,
     usage: (.usage | if . then {input_tokens: (.input_tokens // .prompt_tokens),
                                output_tokens: (.output_tokens // .completion_tokens)}
                      else null end)}' "$tmp/reply.json"
```

To test an adapter like this without the network, point `ENDPOINT` at a local stub, pipe
`request-v1.json` into it, and check that its reply decodes like `answer-v1.json` and that its
refusals look like `refused-v1.json`.
