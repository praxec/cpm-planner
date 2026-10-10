import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { launcher, PKG_VERSION, releaseFiles, sha256, startServer, tempDir, insecureEnv, quiet } from './helpers.mjs';

const { ensureBinary, hostTarget } = launcher;
const TARGET = hostTarget();
const POSIX = process.platform !== 'win32';
const IS_ROOT = POSIX && process.getuid() === 0;

// A cache that already holds a binary with a matching .verified digest.
function seededCache(t, { mode = 0o700 } = {}) {
  const cache = tempDir(t);
  const dir = path.join(cache, 'cpm-planner', PKG_VERSION);
  fs.mkdirSync(dir, { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(dir, TARGET.binary), 'cached', { mode: 0o755 });
  fs.writeFileSync(path.join(dir, '.verified'), `${sha256('cached')}\n`);
  fs.chmodSync(dir, mode);
  return { cache, dir };
}

const offline = { CPM_PLANNER_DOWNLOAD_BASE: 'https://unreachable.invalid' };

test('a cached binary modified after install is refused', async (t) => {
  const cache = tempDir(t);
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET }));
  const bin = await ensureBinary({ env: insecureEnv(base), cacheRoot: cache, log: quiet });
  fs.appendFileSync(bin, 'tampered');
  await assert.rejects(ensureBinary({ env: insecureEnv(base), cacheRoot: cache, log: quiet }), /does not match the verified/);
});

test('a cached binary without a recorded digest is refused', async (t) => {
  const { cache, dir } = seededCache(t);
  fs.rmSync(path.join(dir, '.verified'));
  await assert.rejects(ensureBinary({ env: offline, cacheRoot: cache, log: quiet }), /no \.verified digest/);
});

test('a cache directory writable by group or others is refused', { skip: !POSIX }, async (t) => {
  const { cache } = seededCache(t, { mode: 0o777 });
  await assert.rejects(ensureBinary({ env: offline, cacheRoot: cache, log: quiet }), /writable by group or others/);
});

test('a cache directory owned by another user is refused', { skip: !IS_ROOT && 'needs root to chown' }, async (t) => {
  const { cache, dir } = seededCache(t);
  fs.chownSync(dir, 65534, 65534);
  await assert.rejects(ensureBinary({ env: offline, cacheRoot: cache, log: quiet }), /is owned by uid 65534/);
});

test('a freshly created cache directory is private to the user', { skip: !POSIX }, async (t) => {
  const cache = tempDir(t);
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET }));
  await ensureBinary({ env: insecureEnv(base), cacheRoot: cache, log: quiet });
  assert.equal(fs.statSync(path.join(cache, 'cpm-planner', PKG_VERSION)).mode & 0o077, 0);
});
