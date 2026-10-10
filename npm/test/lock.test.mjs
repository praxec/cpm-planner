import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { launcher, tempDir } from './helpers.mjs';

const { acquireLock } = launcher;
const FAST = { pollMs: 10, waitMs: 2000 };
const never = () => false;

function writeLock(file, owner) {
  fs.writeFileSync(file, JSON.stringify({ host: os.hostname(), token: 'someone-else', at: new Date().toISOString(), ...owner }));
}

// A pid that existed a moment ago and has exited.
function deadPid() {
  return spawnSync(process.execPath, ['-e', '']).pid;
}

test('an orphaned lock from a dead pid is taken over', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: deadPid() });
  const release = await acquireLock(lock, never, FAST);
  assert.equal(typeof release, 'function');
});

test('a lock held by a live process is waited on', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: process.ppid });
  await assert.rejects(acquireLock(lock, never, { pollMs: 10, waitMs: 200 }), /timed out waiting for another download/);
});

test("release does not remove another owner's lock", async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  const release = await acquireLock(lock, never, FAST);
  writeLock(lock, { pid: process.pid, token: 'new-owner' });
  release();
  assert.equal(fs.existsSync(lock), true);
});

test('release removes the lock it still owns', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  const release = await acquireLock(lock, never, FAST);
  release();
  assert.equal(fs.existsSync(lock), false);
});

test('an unreadable lock older than the stale threshold is taken over', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  fs.writeFileSync(lock, 'not json');
  const past = new Date(Date.now() - 60000);
  fs.utimesSync(lock, past, past);
  const release = await acquireLock(lock, never, { ...FAST, staleMs: 1000 });
  assert.equal(typeof release, 'function');
});

test('a waiter returns without the lock once the other process has installed the binary', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: process.ppid });
  assert.equal(await acquireLock(lock, () => true, FAST), null);
});

test('a lock naming our own pid that we do not hold is taken over', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: process.pid });
  const release = await acquireLock(lock, never, FAST);
  assert.equal(typeof release, 'function');
});

test('an old lock whose pid was reused by a live process is taken over by age', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: process.ppid });
  const past = new Date(Date.now() - 60000);
  fs.utimesSync(lock, past, past);
  const release = await acquireLock(lock, never, { ...FAST, staleMs: 1000 });
  assert.equal(typeof release, 'function');
});

test('the heartbeat keeps a held lock fresh past the stale threshold', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  const release = await acquireLock(lock, never, { ...FAST, heartbeatMs: 20 });
  t.after(release);
  const past = new Date(Date.now() - 60000);
  fs.utimesSync(lock, past, past);
  await new Promise((r) => setTimeout(r, 150));
  assert.ok(Date.now() - fs.statSync(lock).mtimeMs < 1000);
});
