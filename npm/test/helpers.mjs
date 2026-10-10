// Shared fixtures for the launcher tests: a local release server and archives
// shaped exactly like the GitHub release assets (binary at the archive root).
//
// The fixture server speaks plain http on 127.0.0.1 and the tests set
// PRAXEC_ALLOW_INSECURE=1, the same documented override scripts/install.sh
// and its tests use. Node cannot mint X.509 certificates without openssl, and
// a committed key would expire and trip secret scanners, so no https fixture.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
export const launcher = require('../lib/install.js');
export const PKG_VERSION = require('../package.json').version;
export const BIN_JS = path.join(HERE, '..', 'bin', 'cpm-planner.js');
export const POSTINSTALL_JS = path.join(HERE, '..', 'lib', 'postinstall.js');
export const INSTALL_JS = path.join(HERE, '..', 'lib', 'install.js');

export function tempDir(t, prefix = 'cpm-npm-') {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), prefix));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

export function sha256(buf) {
  return createHash('sha256').update(buf).digest('hex');
}

// Packs `content` as the target's binary into a release archive and returns its bytes.
export function buildArchive(t, target, content) {
  const work = tempDir(t, 'cpm-npm-archive-');
  const stage = path.join(work, 'stage');
  fs.mkdirSync(stage);
  fs.writeFileSync(path.join(stage, target.binary), content, { mode: 0o755 });
  if (target.ext === 'zip') {
    execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-Command',
      'Compress-Archive -Path $env:SRC -DestinationPath $env:DEST -Force'], {
      env: { ...process.env, SRC: path.join(stage, target.binary), DEST: path.join(work, target.asset) },
    });
  } else {
    execFileSync('tar', ['-czf', target.asset, '-C', 'stage', target.binary], { cwd: work });
  }
  return fs.readFileSync(path.join(work, target.asset));
}

// Release files for `version`: the archive plus a checksums.sha256 in the
// release-manifest.sh format. `checksumOf` lets a test publish a wrong digest.
export function releaseFiles(t, { version = PKG_VERSION, target, content = 'fake cpm-planner\n', checksumOf } = {}) {
  const archive = buildArchive(t, target, content);
  const digest = sha256(checksumOf ?? archive);
  return {
    [`/download/v${version}/${target.asset}`]: { body: archive },
    [`/download/v${version}/checksums.sha256`]: { body: `${digest}  ${target.asset}\n` },
  };
}

// Serves `routes` ({ path: { status?, headers?, body?, delayMs? } }) and counts hits per path.
export async function startServer(t, routes) {
  const hits = {};
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, 'http://x');
    hits[url.pathname] = (hits[url.pathname] ?? 0) + 1;
    const route = routes[url.pathname];
    if (!route) {
      res.writeHead(404).end('not found');
      return;
    }
    setTimeout(() => {
      res.writeHead(route.status ?? 200, route.headers ?? {});
      res.end(route.body ?? '');
    }, route.delayMs ?? 0);
  });
  await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
  t.after(() => new Promise((resolve) => server.close(resolve)));
  return { base: `http://127.0.0.1:${server.address().port}`, hits };
}

export function insecureEnv(base, extra = {}) {
  return { PRAXEC_ALLOW_INSECURE: '1', CPM_PLANNER_DOWNLOAD_BASE: base, ...extra };
}

export const quiet = () => {};
