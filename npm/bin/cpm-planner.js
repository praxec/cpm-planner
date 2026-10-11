#!/usr/bin/env node
'use strict';
// npx launcher for the cpm-planner MCP server.
//
// stdout belongs to the MCP stdio protocol, so this file never writes to it:
// download progress and errors go to stderr, and the server inherits stdin,
// stdout and stderr directly. Arguments, the exit code and termination
// signals are passed through to and from the server.

const { spawn } = require('node:child_process');
const { ensureBinary, formatError, LauncherError } = require('../lib/install.js');

const FORWARDED_SIGNALS = process.platform === 'win32' ? ['SIGINT', 'SIGTERM'] : ['SIGINT', 'SIGTERM', 'SIGHUP'];

function run(binary, args) {
  const child = spawn(binary, args, { stdio: 'inherit' });
  const handlers = new Map();
  for (const sig of FORWARDED_SIGNALS) {
    const handler = () => {
      try {
        child.kill(sig);
      } catch {
        // child already gone
      }
    };
    handlers.set(sig, handler);
    process.on(sig, handler);
  }
  child.on('error', (err) => {
    process.stderr.write(formatError(new LauncherError(`cannot start ${binary}: ${err.message}`)));
    process.exit(1);
  });
  child.on('exit', (code, signal) => {
    for (const [sig, handler] of handlers) process.removeListener(sig, handler);
    if (signal) {
      // Die by the same signal so our parent sees what the server saw.
      process.kill(process.pid, signal);
      setTimeout(() => process.exit(1), 1000);
      return;
    }
    process.exit(code === null ? 1 : code);
  });
}

ensureBinary().then(
  (binary) => run(binary, process.argv.slice(2)),
  (err) => {
    process.stderr.write(formatError(err));
    process.exit(1);
  },
);
