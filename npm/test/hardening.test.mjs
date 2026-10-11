import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { launcher, PKG_VERSION, buildTarWith, releaseFiles, sha256, startServer, tempDir, insecureEnv, quiet } from './helpers.mjs';

const { ensureBinary, hostTarget, resolveTarget, unsafeEntry, specialEntry, expectedDigest, checkRedirect, redact } = launcher;
const TARGET = hostTarget();
const ASSET_PATH = `/download/v${PKG_VERSION}/${TARGET.asset}`;
const POSIX = process.platform !== 'win32';
// A tar.gz target works on every OS (Windows 10+ ships bsdtar).
const TAR_TARGET = resolveTarget('linux', 'x64');

function install(t, env, extra = {}) {
  return ensureBinary({ env, cacheRoot: tempDir(t), log: quiet, ...extra });
}

// --- archive entry checks --------------------------------------------------
for (const name of ['../evil', 'a/../../evil', '..\\evil', '/etc/passwd', 'C:/Windows/evil', 'C:evil']) {
  test(`archive entry ${JSON.stringify(name)} is unsafe`, () => {
    assert.equal(unsafeEntry(name), true);
  });
}

for (const name of ['cpm-planner', 'dir/cpm-planner', 'a..b']) {
  test(`archive entry ${JSON.stringify(name)} is safe`, () => {
    assert.equal(unsafeEntry(name), false);
  });
}

for (const line of ['lrwxrwxrwx 0 0 0 0 Jan 1 00:00 cpm-planner -> /bin/sh', 'hrw-r--r-- 0 0 0 0 Jan 1 00:00 link', 'crw-r--r-- dev', 'brw-r--r-- dev', 'prw-r--r-- fifo']) {
  test(`tar listing "${line.slice(0, 10)}" is a link or special file`, () => {
    assert.equal(specialEntry(line), true);
  });
}

test('a regular file in a tar listing is not special', () => {
  assert.equal(specialEntry('-rwxr-xr-x 0 0 0 5 Jan 1 00:00 cpm-planner'), false);
});

test('an archive whose binary is a symlink is refused', { skip: !POSIX }, async (t) => {
  const archive = buildTarWith(t, TAR_TARGET.asset, (stage) => fs.symlinkSync('/bin/sh', path.join(stage, 'cpm-planner')));
  const { base } = await startServer(t, releaseFiles(t, { target: TAR_TARGET, archive }));
  await assert.rejects(install(t, insecureEnv(base), { target: TAR_TARGET }), /links or special files/);
});

test('an archive with only a directory named like the binary is refused', async (t) => {
  const archive = buildTarWith(t, TAR_TARGET.asset, (stage) => {
    fs.mkdirSync(path.join(stage, 'cpm-planner'));
    fs.writeFileSync(path.join(stage, 'cpm-planner', 'README'), 'not a binary');
  });
  const { base } = await startServer(t, releaseFiles(t, { target: TAR_TARGET, archive }));
  await assert.rejects(install(t, insecureEnv(base), { target: TAR_TARGET }), /no executable file 'cpm-planner'/);
});

test('an archive whose binary is not executable is refused', { skip: !POSIX }, async (t) => {
  const archive = buildTarWith(t, TAR_TARGET.asset, (stage) => fs.writeFileSync(path.join(stage, 'cpm-planner'), 'x', { mode: 0o644 }));
  const { base } = await startServer(t, releaseFiles(t, { target: TAR_TARGET, archive }));
  await assert.rejects(install(t, insecureEnv(base), { target: TAR_TARGET }), /no executable file 'cpm-planner'/);
});

// --- checksums.sha256 parsing -----------------------------------------------
const SIX = [
  'x86_64-unknown-linux-gnu.tar.gz', 'aarch64-unknown-linux-gnu.tar.gz', 'x86_64-apple-darwin.tar.gz',
  'aarch64-apple-darwin.tar.gz', 'x86_64-pc-windows-msvc.zip', 'aarch64-pc-windows-msvc.zip',
].map((suffix) => `cpm-planner-${suffix}`);
const SIX_LINES = SIX.map((asset) => `${sha256(asset)}  ${asset}`).join('\n') + '\n';

test('the digest for one asset is picked out of the six-line checksums file', () => {
  assert.equal(expectedDigest(SIX_LINES, SIX[3]), sha256(SIX[3]));
});

test('a binary-mode "*asset" checksum line is accepted', () => {
  assert.equal(expectedDigest(`${sha256('a')} *${SIX[0]}\n`, SIX[0]), sha256('a'));
});

test('a checksums file with CRLF line endings is accepted', () => {
  assert.equal(expectedDigest(SIX_LINES.replace(/\n/g, '\r\n'), SIX[5]), sha256(SIX[5]));
});

test('an asset missing from checksums.sha256 has no digest', () => {
  assert.equal(expectedDigest(SIX_LINES, 'cpm-planner-riscv64.tar.gz'), null);
});

// --- redirects, limits, timeouts --------------------------------------------
test('a redirect from https to http is refused', () => {
  assert.throws(() => checkRedirect(new URL('http://github.com/x'), ['github.com'], {}), /refusing redirect to non-https URL/);
});

test('a redirect loop stops after the redirect limit', async (t) => {
  const routes = releaseFiles(t, { target: TARGET });
  routes[ASSET_PATH] = { status: 302, headers: { location: ASSET_PATH } };
  const { base } = await startServer(t, routes);
  await assert.rejects(install(t, insecureEnv(base)), /too many redirects \(more than 5\)/);
});

test('a download larger than the size cap is aborted', async (t) => {
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET }));
  await assert.rejects(install(t, insecureEnv(base), { limits: { maxBytes: 10 } }), /download exceeds 10 bytes/);
});

test('a server that stops sending data hits the idle timeout', async (t) => {
  const routes = releaseFiles(t, { target: TARGET });
  routes[ASSET_PATH].delayMs = 1500;
  const { base } = await startServer(t, routes);
  await assert.rejects(install(t, insecureEnv(base), { limits: { idleTimeoutMs: 100 } }), /no data for 0\.1s/);
});

// --- credential redaction -----------------------------------------------------
test('redact strips the user and password from a URL', () => {
  assert.equal(redact('https://user:s3cret@mirror.example/releases'), 'https://mirror.example/releases');
});

test('download errors do not reveal mirror credentials', async (t) => {
  const { base } = await startServer(t, {});
  const withCreds = base.replace('http://', 'http://user:s3cret@');
  await assert.rejects(install(t, insecureEnv(withCreds)), (err) => !/s3cret/.test(err.message));
});

test('progress messages do not reveal mirror credentials', async (t) => {
  const { base } = await startServer(t, releaseFiles(t, { target: TARGET }));
  const logged = [];
  await install(t, insecureEnv(base.replace('http://', 'http://user:s3cret@')), { log: (m) => logged.push(m) });
  assert.equal(logged.some((m) => m.includes('s3cret')), false);
});

// --- CPM_PLANNER_BINARY -------------------------------------------------------
for (const script of ['cpm-planner.cmd', 'CPM-PLANNER.BAT']) {
  test(`CPM_PLANNER_BINARY ending in ${path.extname(script)} is refused as a script`, async () => {
    await assert.rejects(ensureBinary({ env: { CPM_PLANNER_BINARY: script }, log: quiet }), /must be a native executable/);
  });
}
