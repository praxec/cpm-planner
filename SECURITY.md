# Security Policy

## Supported versions

Only the latest minor release receives security fixes.

| Version | Supported |
| ------- | --------- |
| 0.2.x   | Yes       |
| < 0.2   | No        |

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/praxec/cpm-planner/security/advisories/new)
rather than a public issue. You can also email <matthew@cochranweb.com>.

Expect an acknowledgement within 3 business days and an initial assessment
within 7 days. A fix or mitigation is coordinated before public disclosure.

## Scope

cpm-planner is an MCP server that speaks over stdio. It executes no
user-supplied code and never shells out to another process. The security-relevant
surface is:

- **Plan-file I/O (`src/project.rs`).** Plan files under
  `<root>/.cpm-planner/plans/` are read and written through `cap-std`
  directory handles confined to the project root. Symlinks are not followed,
  and a plan file larger than 8 MiB is refused. Any path escape, symlink
  traversal, or unbounded read is in scope.
- **Optional LLM calls (`src/llm/`).** `plan.review` can call OpenRouter over
  HTTPS. `https://` is required except for loopback hosts. The API key comes
  from `OPENROUTER_API_KEY` or a key file that is not world-readable (checked on unix; the file is
  ignored with a warning otherwise), and is scrubbed from
  every error and log line. Error and reported-model text is length-capped.
  Any leak of the key, or a way to send it to a non-HTTPS non-loopback
  endpoint, is in scope.
- **SQLite store.** Plan, lock, and earned-value state is kept in a local
  SQLite database at `CPM_PLANNER_DB` (default under the user data directory).
  Crafted inputs that corrupt it or let two callers hold the same deliverable
  lock are in scope.
- **`skills install` (coming in 0.2.0).** It writes only under the user or
  project skill directories and never overwrites files it did not write.
- **Task graphs and tool arguments.** A crafted `plan.submit` graph or lock
  sequence that causes a panic, a hang, or incorrect lock arbitration is in
  scope.
