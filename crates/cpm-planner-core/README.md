# cpm-planner-core

The pure Critical Path Method (CPM) kernel of
[cpm-planner](https://github.com/praxec/cpm-planner), as a library with no
I/O.

It holds the plan graph model and the computations over it:

- `plan`: deliverables, prerequisites (with lag), owned files, statuses, ids,
  limits and `PlannerError`.
- `validate`: the structural checks every graph passes (unique ids, values in
  range, known prerequisites, ordered file claims, no cycles).
- `schedule::compute_cpm`: the forward and backward passes, earliest and
  latest start and finish, slack, the critical path, parallel batches and
  bottlenecks, with synthetic `__start__` and `__finish__` endpoints.
- `estimator`, `calendar`, `resource_schedule` (capacity-levelled
  scheduling), `monte_carlo` (seeded schedule risk), `drag`, `risk`,
  `network_health` and `earned_value` (PV, EV, AC, SPI, CPI, EAC).

Its dependency graph has no SQLite, async runtime, MCP or LLM client, so it
fits a program that links its own SQLite, for example through sqlx.
`scripts/check-core-deps.sh` in the repository keeps it that way in CI.

## Use

```toml
[dependencies]
cpm-planner-core = "0.7"
serde_json = "1"
```

```rust
use cpm_planner_core::plan::PlanGraph;
use cpm_planner_core::schedule::compute_cpm;

let graph: PlanGraph = serde_json::from_value(serde_json::json!({
    "deliverables": [
        {"id": "design", "owned_files": ["docs/design.md"], "prerequisites": [],
         "duration_hours": 4.0},
        {"id": "build", "owned_files": ["src/app.rs"], "prerequisites": ["design"],
         "duration_hours": 8.0}
    ]
}))?;

let cpm = compute_cpm(&graph)?; // Err(PlannerError) for an invalid graph
assert_eq!(cpm.critical_path, ["__start__", "design", "build", "__finish__"]);
for task in &cpm.tasks {
    println!("{} starts at {}h", task.id, task.earliest_start);
}
```

`compute_cpm` validates the graph first and returns
`PlannerError::InvalidGraph` with a precise reason when it fails.

## Relation to cpm-planner

`cpm-planner` (the MCP server, CLI, SQLite store and lock-aware planner)
depends on this crate and re-exports every module at its old path:
`cpm_planner::plan`, `cpm_planner::schedule`, `cpm_planner::earned_value` and
so on name the same types as `cpm_planner_core::…`. `cpm_planner::plan` also
adds the execution types (cohort requests, locks, `PlanStatus`). Both crates
always have the same version.

## Licence

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).
