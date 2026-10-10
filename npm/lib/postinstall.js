'use strict';
// Optional pre-fetch of the release binary at install time, so the first MCP
// start does not wait for a download. It never fails the install: every error
// becomes a warning on stderr and the launcher retries on first run.
//
// Skipped when scripts are disabled (npm_config_ignore_scripts), in CI (CI is
// set), when CPM_PLANNER_BINARY names a local binary, or when
// CPM_PLANNER_SKIP_DOWNLOAD=1.

function skipReason(env) {
  if (env.npm_config_ignore_scripts === 'true' || env.npm_config_ignore_scripts === '1') return 'npm_config_ignore_scripts is set';
  if (env.CI && env.CI !== 'false' && env.CI !== '0') return 'CI is set';
  if (env.CPM_PLANNER_BINARY) return 'CPM_PLANNER_BINARY is set';
  if (env.CPM_PLANNER_SKIP_DOWNLOAD === '1') return 'CPM_PLANNER_SKIP_DOWNLOAD=1';
  return null;
}

function warn(msg) {
  try {
    process.stderr.write(`cpm-planner (npm postinstall): ${msg}\n`);
  } catch {
    // stderr closed; nothing else to do
  }
}

async function main() {
  const reason = skipReason(process.env);
  if (reason) return;
  try {
    const { ensureBinary } = require('./install.js');
    await ensureBinary({ log: warn });
  } catch (err) {
    warn(`pre-fetch skipped (${err && err.message ? err.message : err}); the binary will be downloaded on first run`);
  }
}

if (require.main === module) {
  main().finally(() => {
    process.exitCode = 0;
  });
}

module.exports = { skipReason };
