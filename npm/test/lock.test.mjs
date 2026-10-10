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

test('orphaned_lock_from_dead_pid_is_taken_over', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: deadPid() });
  const release = await acquireLock(lock, never, FAST);
  assert.equal(typeof release, 'function');
});

test('lock_held_by_live_process_is_waited_on', async (t) => {
  const lock = path.join(tempDir(t), '.lock');
  writeLock(lock, { pid: process.pid });
  await assert.rejects(acquireLock(lock, never, { pollMs: 10, waitMs: 200 }), /timed out waiting for another download/);
});

test('release_does_not_remove_another_owners_lock', async (t) => {
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
  writeLock(lock, { pid: process.pid });
  assert.equal(await acquireLock(lock, () => true, FAST), null);
});
