# Security Policy

## Reporting a vulnerability

Please report security issues privately via GitHub's
[security advisories](https://github.com/praxec/cpm-planner/security/advisories/new)
rather than a public issue. You'll get an acknowledgement, and a fix or mitigation
will be coordinated before public disclosure.

## Scope

cpm-planner is an MCP server that speaks over stdio and holds plan/lock state in
its own process. It executes no user-supplied code and shells out to nothing —
its inputs are task graphs and lock operations. Of particular interest: any input
(a crafted `plan.submit` graph or lock sequence) that causes a panic, a hang, or
incorrect lock arbitration that could let two callers hold the same deliverable.
