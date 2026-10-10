import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createRequire } from 'node:module';
import { POSTINSTALL_JS, startServer, tempDir } from './helpers.mjs';

const { skipReason } = createRequire(import.meta.url)('../lib/postinstall.js');

test('postinstall skips the pre-fetch in CI', () => {
  assert.equal(skipReason({ CI: 'true' }), 'CI is set');
});

test('postinstall skips the pre-fetch when npm scripts are disabled', () => {
  assert.equal(skipReason({ npm_config_ignore_scripts: 'true' }), 'npm_config_ignore_scripts is set');
});

test('postinstall skips the pre-fetch when CPM_PLANNER_BINARY is set', () => {
  assert.equal(skipReason({ CPM_PLANNER_BINARY: '/usr/local/bin/cpm-planner' }), 'CPM_PLANNER_BINARY is set');
});

// Runs postinstall against a release server that has no assets (every download 404s).
async function runFailingPostinstall(t) {
  const { base } = await startServer(t, {});
  const env = { ...process.env, PRAXEC_ALLOW_INSECURE: '1', CPM_PLANNER_DOWNLOAD_BASE: base, CPM_PLANNER_CACHE_DIR: tempDir(t) };
  for (const k of ['CI', 'npm_config_ignore_scripts', 'CPM_PLANNER_BINARY', 'CPM_PLANNER_SKIP_DOWNLOAD']) delete env[k];
  const child = spawn(process.execPath, [POSTINSTALL_JS], { env });
  let stdout = '';
  child.stdout.on('data', (d) => { stdout += d; });
  child.stderr.resume();
  const status = await new Promise((resolve) => child.on('close', resolve));
  return { status, stdout };
}

test('postinstall exits 0 when the pre-fetch download fails', async (t) => {
  assert.equal((await runFailingPostinstall(t)).status, 0);
});

test('postinstall writes nothing to stdout', async (t) => {
  assert.equal((await runFailingPostinstall(t)).stdout, '');
});
