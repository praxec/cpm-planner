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

cpm-planner is an MCP server that speaks over stdio. The server itself
executes no user-supplied code and never shells out to another process. Every
component below ships in 0.2.0 and is in scope:

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
  SQLite database at `CPM_PLANNER_DB`, by default
  `~/.local/share/praxec/cpm-planner.db` on every OS (`~` is `HOME`, falling
  back to `USERPROFILE`, so `%USERPROFILE%\.local\share\praxec\` on Windows).
  Crafted inputs that corrupt it or let two callers hold the same deliverable
  lock are in scope.
- **`cpm-planner skills install` / `uninstall` (`src/skills.rs`).** It writes
  only inside the user or project skill roots of the selected agents. Paths in
  the embedded manifest are validated before use and never resolve outside
  those roots. It never overwrites or removes a file it did not write, or one
  the user changed since it was written, unless `--force` is given. A way to
  write or delete outside the skill roots, or to clobber user files without
  `--force`, is in scope.
- **npm launcher (`npm/`, `@matthew-cochran/cpm`).** It downloads the release
  binary over HTTPS only, following redirects only to an allowlist of hosts,
  and verifies the archive's SHA-256 against the release `checksums.sha256`
  before extracting it. Extraction uses the system `tar` (or `Expand-Archive`
  on Windows) after every entry is checked: absolute paths, `..` segments and
  links (plus, for tar, special files) are refused. The cached binary's hash
  is re-verified on every start, and on unix the cache directory must be owned
  by the current user and not group- or world-writable. Concurrent installs are serialised by
  a pid-aware lock that recovers from a crashed holder. Two overrides exist:
  `PRAXEC_ALLOW_INSECURE=1` permits plain `http://` (local testing only), and
  `CPM_PLANNER_BINARY` runs a local binary you supply instead of downloading
  one. A way to run an unverified binary without those overrides, to escape
  the cache directory during extraction, or to redirect a download to an
  unlisted host is in scope.
- **Install scripts (`scripts/install.sh`, `scripts/install.ps1`).** They
  download over HTTPS only (unless `PRAXEC_ALLOW_INSECURE=1`) and verify the
  archive's SHA-256 against the release `checksums.sha256` before extracting
  and installing it. A bypass of either check is in scope.
- **Task graphs and tool arguments.** A crafted `plan.submit` graph or lock
  sequence that causes a panic, a hang, or incorrect lock arbitration is in
  scope.
