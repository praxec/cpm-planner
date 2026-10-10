import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { BIN_JS, launcher, releaseFiles, startServer, tempDir, insecureEnv } from './helpers.mjs';

const POSIX = process.platform !== 'win32';

// A stand-in server: node itself runs a small script, so the "binary" is a
// real executable on every OS. CPM_PLANNER_BINARY points the launcher at it.
function fakeServer(t, body) {
  const script = path.join(tempDir(t), 'fake-server.js');
  fs.writeFileSync(script, body);
  return script;
}

function cleanEnv(extra) {
  const env = { ...process.env, ...extra };
  for (const k of ['CPM_PLANNER_BINARY', 'CPM_PLANNER_DOWNLOAD_BASE', 'CPM_PLANNER_CACHE_DIR', 'PRAXEC_ALLOW_INSECURE']) {
    if (!(k in extra)) delete env[k];
  }
  return env;
}

function runLauncher(args, env) {
  return spawnSync(process.execPath, [BIN_JS, ...args], { encoding: 'utf8', timeout: 30000, env: cleanEnv(env) });
}

test('CPM_PLANNER_BINARY runs the local binary without contacting any download host', (t) => {
  const script = fakeServer(t, 'process.exit(0);\n');
  const res = runLauncher([script], {
    CPM_PLANNER_BINARY: process.execPath,
    CPM_PLANNER_DOWNLOAD_BASE: 'https://unreachable.invalid',
    CPM_PLANNER_CACHE_DIR: tempDir(t),
  });
  assert.equal(res.status, 0);
});

test('stdout carries only what the server writes', (t) => {
  const script = fakeServer(t, "process.stdout.write('{\"jsonrpc\":\"2.0\"}\\n');\n");
  const res = runLauncher([script], { CPM_PLANNER_BINARY: process.execPath });
  assert.equal(res.stdout, '{"jsonrpc":"2.0"}\n');
});

test('arguments are passed through to the server unchanged', (t) => {
  const script = fakeServer(t, 'process.stdout.write(JSON.stringify(process.argv.slice(2)));\n');
  const res = runLauncher([script, 'skills', 'install', '--target', 'claude', '--user'], { CPM_PLANNER_BINARY: process.execPath });
  assert.deepEqual(JSON.parse(res.stdout), ['skills', 'install', '--target', 'claude', '--user']);
});

test("the launcher exits with the server's exit code", (t) => {
  const script = fakeServer(t, 'process.exit(7);\n');
  const res = runLauncher([script], { CPM_PLANNER_BINARY: process.execPath });
  assert.equal(res.status, 7);
});

test('a launcher error goes to stderr and leaves stdout empty', () => {
  const res = runLauncher([], { CPM_PLANNER_BINARY: path.join('no', 'such', 'cpm-planner') });
  assert.equal(res.stdout, '');
});

test('a launcher error names CPM_PLANNER_BINARY and the manual install docs as the remedy', () => {
  const res = runLauncher([], { CPM_PLANNER_BINARY: path.join('no', 'such', 'cpm-planner') });
  assert.match(res.stderr, /remedy: set CPM_PLANNER_BINARY[\s\S]*AGENT-INSTALL\.md/);
});

test('SIGTERM sent to the launcher reaches the server', { skip: !POSIX }, async (t) => {
  const script = fakeServer(t, [
    "process.on('SIGTERM', () => process.exit(42));",
    "process.stdout.write('ready\\n');",
    'setInterval(() => {}, 1000);',
  ].join('\n'));
  const child = spawn(process.execPath, [BIN_JS, script], { env: cleanEnv({ CPM_PLANNER_BINARY: process.execPath }) });
  await new Promise((resolve) => child.stdout.once('data', resolve));
  child.kill('SIGTERM');
  const code = await new Promise((resolve) => child.on('exit', (c) => resolve(c)));
  assert.equal(code, 42);
});

test('a first run that downloads writes nothing to stdout before the server starts', { skip: !POSIX }, async (t) => {
  const target = launcher.hostTarget();
  const routes = releaseFiles(t, { target, content: '#!/bin/sh\nprintf "server-output\\n"\n' });
  const { base } = await startServer(t, routes);
  const env = cleanEnv(insecureEnv(base, { CPM_PLANNER_CACHE_DIR: tempDir(t) }));
  const child = spawn(process.execPath, [BIN_JS], { env });
  let stdout = '';
  child.stdout.on('data', (d) => { stdout += d; });
  await new Promise((resolve) => child.on('close', resolve));
  assert.equal(stdout, 'server-output\n');
});
