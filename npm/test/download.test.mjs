import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import fs from 'node:fs';
import path from 'node:path';
import { launcher, INSTALL_JS, PKG_VERSION, releaseFiles, sha256, startServer, tempDir, insecureEnv, quiet } from './helpers.mjs';

const { ensureBinary, hostTarget } = launcher;
const TARGET = hostTarget();
const ASSET_PATH = `/download/v${PKG_VERSION}/${TARGET.asset}`;

function versionDir(cache) {
  return path.join(cache, 'cpm-planner', PKG_VERSION);
}

function install(cache, env) {
  return ensureBinary({ env, cacheRoot: cache, log: quiet });
}

test('a first run installs the binary whose archive matches the published checksum', async (t) => {
  const cache = tempDir(t);
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET, content: 'verified binary\n' }));
  const bin = await install(cache, insecureEnv(base));
  assert.equal(fs.readFileSync(bin, 'utf8'), 'verified binary\n');
});

test('a checksum mismatch aborts the install', async (t) => {
  const cache = tempDir(t);
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET, checksumOf: 'something else' }));
  await assert.rejects(install(cache, insecureEnv(base)), /checksum mismatch/);
});

test('a checksum mismatch leaves no partial download or binary in the cache', async (t) => {
  const cache = tempDir(t);
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET, checksumOf: 'something else' }));
  await install(cache, insecureEnv(base)).catch(() => {});
  assert.deepEqual(fs.readdirSync(versionDir(cache)), []);
});

test('a cached binary is used without any download', async (t) => {
  const cache = tempDir(t);
  fs.mkdirSync(versionDir(cache), { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(versionDir(cache), TARGET.binary), 'cached');
  fs.writeFileSync(path.join(versionDir(cache), '.verified'), `${sha256('cached')}\n`);
  const { base, hits } = await startServer(t, releaseFiles(t, { target: TARGET }));
  await install(cache, insecureEnv(base));
  assert.deepEqual(hits, {});
});

test('CPM_PLANNER_BINARY is returned as-is without touching the cache', async (t) => {
  const cache = tempDir(t);
  const local = path.join(tempDir(t), 'my-cpm-planner');
  fs.writeFileSync(local, 'local');
  await ensureBinary({ env: { CPM_PLANNER_BINARY: local }, cacheRoot: cache, log: quiet });
  assert.deepEqual(fs.readdirSync(cache), []);
});

test('a non-https download base is refused without PRAXEC_ALLOW_INSECURE=1', async (t) => {
  const cache = tempDir(t);
  await assert.rejects(install(cache, { CPM_PLANNER_DOWNLOAD_BASE: 'http://127.0.0.1:9' }), /refusing non-https base URL/);
});

test('a redirect to a host outside the allowlist is refused', async (t) => {
  const cache = tempDir(t);
  const routes = releaseFiles(t, { target: TARGET });
  routes[ASSET_PATH] = { status: 302, headers: { location: 'http://evil.example.com/cpm-planner.tar.gz' } };
  const { base } = await startServer(t, routes);
  await assert.rejects(install(cache, insecureEnv(base)), /refusing redirect to host 'evil\.example\.com'/);
});

test('a redirect within the download host is followed', async (t) => {
  const cache = tempDir(t);
  const routes = releaseFiles(t, { target: TARGET, content: 'redirected binary\n' });
  routes['/cdn/asset'] = routes[ASSET_PATH];
  routes[ASSET_PATH] = { status: 302, headers: { location: '/cdn/asset' } };
  const { base } = await startServer(t, routes);
  const bin = await install(cache, insecureEnv(base));
  assert.equal(fs.readFileSync(bin, 'utf8'), 'redirected binary\n');
});

test('a missing release asset fails with the HTTP status and the URL', async (t) => {
  const cache = tempDir(t);
  const { base } = await startServer(t, {});
  await assert.rejects(install(cache, insecureEnv(base)), /HTTP 404: .*cpm-planner-/);
});

// Two MCP clients starting at once are two processes racing on an empty cache.
function installInChildProcess(cache, base) {
  const script = `require(${JSON.stringify(INSTALL_JS)})` +
    '.ensureBinary({ log: () => {} }).then((p) => process.stdout.write(p), (e) => { console.error(e.message); process.exit(1); });';
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, ['-e', script], {
      env: { ...process.env, ...insecureEnv(base), CPM_PLANNER_CACHE_DIR: cache },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    let err = '';
    child.stderr.on('data', (d) => { err += d; });
    child.on('exit', (code) => (code === 0 ? resolve() : reject(new Error(err))));
  });
}

test('concurrent first runs download the asset once', async (t) => {
  const cache = tempDir(t);
  const routes = releaseFiles(t, { target: TARGET });
  routes[ASSET_PATH].delayMs = 500;
  const { base, hits } = await startServer(t, routes);
  await Promise.all([installInChildProcess(cache, base), installInChildProcess(cache, base)]);
  assert.equal(hits[ASSET_PATH], 1);
});
