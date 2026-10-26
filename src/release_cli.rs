//! Offline release workflow compiler command line interface.

use std::io::Read;

use serde::de::DeserializeOwned;

use crate::release_dag::{MAX_INPUT_BYTES, ReleaseWorkflow, compile, diff, lint_workflow};

pub const USAGE: &str = "\
Usage:
  cpm-planner release compile <workflow.json> [--report]
  cpm-planner release lint <workflow.json>
  cpm-planner release diff <workflow.json> <plan.json>

compile prints ordinary PlanGraph JSON (or its provenance report with --report).
lint reports workflow cardinality/contract errors. diff exits 1 when the plan is stale.
All commands are offline and read-only. Each input is limited to 8 MiB.
";

fn read<T: DeserializeOwned>(path: &str) -> anyhow::Result<T> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_INPUT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() as u64 <= MAX_INPUT_BYTES,
        "{path}: input exceeds 8 MiB"
    );
    Ok(serde_json::from_slice(&bytes)?)
}

fn execute(args: &[String]) -> anyhow::Result<(serde_json::Value, u8)> {
    let input: ReleaseWorkflow = read(&args[1])?;
    match args[0].as_str() {
        "compile" => {
            let result = compile(&input)?;
            Ok((
                if args.len() == 3 {
                    serde_json::to_value(result)?
                } else {
                    serde_json::to_value(result.graph)?
                },
                0,
            ))
        }
        "lint" => {
            let report = lint_workflow(&input);
            let code = u8::from(!report.clean);
            Ok((serde_json::to_value(report)?, code))
        }
        "diff" => {
            let report = diff(&input, &read(&args[2])?)?;
            let code = u8::from(report.stale);
            Ok((serde_json::to_value(report)?, code))
        }
        _ => unreachable!("run validates the command"),
    }
}

/// Run without starting the MCP server or opening its durable database.
pub fn run(args: &[String]) -> u8 {
    if args == ["--help"] || args == ["-h"] || args == ["help"] {
        print!("{USAGE}");
        return 0;
    }
    let valid = match args.first().map(String::as_str) {
        Some("compile") => args.len() == 2 || (args.len() == 3 && args[2] == "--report"),
        Some("lint") => args.len() == 2,
        Some("diff") => args.len() == 3,
        _ => false,
    };
    if !valid {
        eprintln!("{USAGE}");
        return 2;
    }
    match execute(args).and_then(|(value, code)| Ok((serde_json::to_string_pretty(&value)?, code)))
    {
        Ok((text, code)) => {
            println!("{text}");
            code
        }
        Err(error) => {
            eprintln!("cpm-planner release: {error}");
            1
        }
    }
}
