'use strict';
// Resolves, downloads, verifies and caches the cpm-planner release binary.
//
// Only Node built-ins are used. Everything is written to stderr (via `log`),
// never to stdout: stdout is the MCP stdio channel of the server we launch.

const crypto = require('node:crypto');
const fs = require('node:fs');
const http = require('node:http');
const https = require('node:https');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const PKG_VERSION = require('../package.json').version;
const BIN = 'cpm-planner';
const REPO = 'praxec/cpm-planner';
const DEFAULT_BASE = `https://github.com/${REPO}/releases`;
const RELEASES_PAGE = `https://github.com/${REPO}/releases`;
const INSTALL_DOCS = `https://github.com/${REPO}/blob/main/docs/AGENT-INSTALL.md`;
// GitHub serves release downloads from github.com and redirects to its asset
// CDN. Current releases redirect to release-assets.githubusercontent.com;
// objects.githubusercontent.com is the older CDN host.
const REDIRECT_HOSTS = ['github.com', 'objects.githubusercontent.com', 'release-assets.githubusercontent.com'];
const MAX_REDIRECTS = 5;
const MAX_BYTES = 134217728; // same cap as scripts/install.sh (PRAXEC_MAX_BYTES default)
const IDLE_TIMEOUT_MS = 30000;
const TOTAL_TIMEOUT_MS = 300000;
const LOCK_STALE_MS = 10 * 60 * 1000;
const LOCK_WAIT_MS = 6 * 60 * 1000;
const LOCK_POLL_MS = 100;

class LauncherError extends Error {}

// The six release targets, keyed by Node's process.platform/process.arch.
const TARGETS = {
  'linux-x64': 'x86_64-unknown-linux-gnu',
  'linux-arm64': 'aarch64-unknown-linux-gnu',
  'darwin-x64': 'x86_64-apple-darwin',
  'darwin-arm64': 'aarch64-apple-darwin',
  'win32-x64': 'x86_64-pc-windows-msvc',
  'win32-arm64': 'aarch64-pc-windows-msvc',
};

// Maps a platform/arch to the release target, asset name and binary name,
// matching .github/workflows/release.yml and scripts/release-manifest.sh.
// `translated` is true when an x64 Node runs under Rosetta on Apple Silicon;
// like install.sh we then prefer the native arm64 binary.
function resolveTarget(platform, arch, { translated = false } = {}) {
  const effectiveArch = platform === 'darwin' && arch === 'x64' && translated ? 'arm64' : arch;
  const target = TARGETS[`${platform}-${effectiveArch}`];
  if (!target) {
    throw new LauncherError(
      `unsupported platform ${platform}/${arch}; release binaries exist for linux, macOS and Windows on x64 and arm64`,
    );
  }
  const windows = platform === 'win32';
  const ext = windows ? 'zip' : 'tar.gz';
  return { target, ext, asset: `${BIN}-${target}.${ext}`, binary: windows ? `${BIN}.exe` : BIN };
}

function runningUnderRosetta() {
  try {
    return execFileSync('sysctl', ['-n', 'sysctl.proc_translated'], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] }).trim() === '1';
  } catch {
    return false;
  }
}

function hostTarget(platform = process.platform, arch = process.arch) {
  const translated = platform === 'darwin' && arch === 'x64' && runningUnderRosetta();
  return resolveTarget(platform, arch, { translated });
}

// Per-user cache root: CPM_PLANNER_CACHE_DIR, else %LOCALAPPDATA% on Windows,
// else $XDG_CACHE_HOME, else ~/.cache. Binaries live in <root>/cpm-planner/<version>/.
function cacheRoot(env = process.env, platform = process.platform) {
  if (env.CPM_PLANNER_CACHE_DIR) return env.CPM_PLANNER_CACHE_DIR;
  if (platform === 'win32' && env.LOCALAPPDATA) return env.LOCALAPPDATA;
  if (env.XDG_CACHE_HOME) return env.XDG_CACHE_HOME;
  return path.join(os.homedir(), '.cache');
}

function insecureAllowed(env) {
  return env.PRAXEC_ALLOW_INSECURE === '1';
}

// Validates the release base URL. https only unless PRAXEC_ALLOW_INSECURE=1,
// the same rule as scripts/install.sh and scripts/install.ps1.
function parseBase(base, env) {
  let url;
  try {
    url = new URL(base);
  } catch {
    throw new LauncherError(`invalid download base URL '${base}'`);
  }
  const ok = url.protocol === 'https:' || (insecureAllowed(env) && url.protocol === 'http:');
  if (!ok) {
    throw new LauncherError(
      `refusing non-https base URL '${base}' (set PRAXEC_ALLOW_INSECURE=1 to override for local testing)`,
    );
  }
  return url;
}

function checkRedirect(location, allowedHosts, env) {
  const httpsOk = location.protocol === 'https:' || (insecureAllowed(env) && location.protocol === 'http:');
  if (!httpsOk) throw new LauncherError(`refusing redirect to non-https URL ${location.origin}`);
  if (!allowedHosts.includes(location.host)) {
    throw new LauncherError(`refusing redirect to host '${location.host}' (allowed: ${allowedHosts.join(', ')})`);
  }
}

// Streams `url` into a new file at `dest`, following only allowlisted redirects.
function download(url, dest, { allowedHosts, env, version, maxBytes = MAX_BYTES }) {
  return new Promise((resolve, reject) => {
    let settled = false;
    let req;
    const finish = (err) => {
      if (settled) return;
      settled = true;
      clearTimeout(deadline);
      if (err) {
        if (req) req.destroy();
        reject(err);
      } else {
        resolve();
      }
    };
    const deadline = setTimeout(
      () => finish(new LauncherError(`download timed out after ${TOTAL_TIMEOUT_MS / 1000}s: ${url}`)),
      TOTAL_TIMEOUT_MS,
    );

    const get = (current, redirectsLeft) => {
      const mod = current.protocol === 'https:' ? https : http;
      req = mod.get(current, { headers: { 'user-agent': `cpm-planner-npm/${version}` }, timeout: IDLE_TIMEOUT_MS }, (res) => {
        const status = res.statusCode || 0;
        if (status >= 300 && status < 400 && res.headers.location) {
          res.resume();
          if (redirectsLeft <= 0) return finish(new LauncherError(`too many redirects: ${url}`));
          let next;
          try {
            next = new URL(res.headers.location, current);
            checkRedirect(next, allowedHosts, env);
          } catch (err) {
            return finish(err);
          }
          return get(next, redirectsLeft - 1);
        }
        if (status !== 200) {
          res.resume();
          const hint = status === 404 ? ` (is release v${version} published with this asset?)` : '';
          return finish(new LauncherError(`download failed with HTTP ${status}: ${current.href}${hint}`));
        }
        let bytes = 0;
        const out = fs.createWriteStream(dest, { flags: 'wx', mode: 0o600 });
        res.on('data', (chunk) => {
          bytes += chunk.length;
          if (bytes > maxBytes) {
            res.destroy();
            out.destroy();
            finish(new LauncherError(`download exceeds ${maxBytes} bytes: ${current.href}`));
          }
        });
        res.on('error', (err) => finish(new LauncherError(`download interrupted: ${current.href}: ${err.message}`)));
        out.on('error', (err) => finish(new LauncherError(`cannot write ${dest}: ${err.message}`)));
        out.on('close', () => {
          if (bytes === 0) return finish(new LauncherError(`downloaded file is empty: ${current.href}`));
          finish();
        });
        res.pipe(out);
      });
      req.on('timeout', () => req.destroy(new Error(`no data for ${IDLE_TIMEOUT_MS / 1000}s`)));
      req.on('error', (err) => finish(err instanceof LauncherError ? err : new LauncherError(`download failed: ${current.href}: ${err.message}`)));
    };
    get(new URL(url), MAX_REDIRECTS);
  });
}

function sha256File(file) {
  return crypto.createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

// Reads "<hex>  <asset>" (or "<hex> *<asset>") lines, the sha256sum format
// written by scripts/release-manifest.sh.
function expectedDigest(checksumsText, asset) {
  for (const line of checksumsText.split(/\r?\n/)) {
    const m = /^([0-9a-fA-F]{64})\s+\*?(.+?)\s*$/.exec(line);
    if (m && m[2] === asset) return m[1].toLowerCase();
  }
  return null;
}

function unsafeEntry(name) {
  const n = name.replace(/\\/g, '/');
  return n.startsWith('/') || /^[A-Za-z]:/.test(n) || n.split('/').includes('..');
}

// Extracts a .tar.gz with the system `tar` (GNU tar or bsdtar; Windows 10+
// ships bsdtar). Paths are relative to `cwd` so GNU tar never reads "C:" as a host.
function extractTarGz(workDir, archiveName, extractName) {
  const run = (args) => execFileSync('tar', args, { cwd: workDir, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 16 * 1024 * 1024 });
  let listing;
  let verbose;
  try {
    listing = run(['-tzf', archiveName]);
    verbose = run(['-tvzf', archiveName]);
  } catch (err) {
    throw new LauncherError(`not a readable .tar.gz archive (is \`tar\` installed?): ${err.message}`);
  }
  const entries = listing.split(/\r?\n/).filter(Boolean);
  if (entries.length === 0) throw new LauncherError('archive is empty');
  const bad = entries.find(unsafeEntry);
  if (bad) throw new LauncherError(`refusing unsafe path in archive: ${bad}`);
  if (verbose.split(/\r?\n/).filter(Boolean).some((l) => /^[lhbcp]/.test(l))) {
    throw new LauncherError('refusing archive containing links or special files');
  }
  fs.mkdirSync(path.join(workDir, extractName));
  try {
    run(['-xzf', archiveName, '-C', extractName, '--no-same-owner']);
  } catch {
    run(['-xzf', archiveName, '-C', extractName]);
  }
}

// Extracts a .zip on Windows with PowerShell's Expand-Archive after the same
// entry checks as scripts/install.ps1. Paths go through env vars, not quoting.
function extractZip(workDir, archiveName, extractName) {
  if (process.platform !== 'win32') throw new LauncherError('.zip assets are only extracted on Windows');
  const script = [
    "$ErrorActionPreference = 'Stop'",
    'Add-Type -AssemblyName System.IO.Compression.FileSystem',
    '$zip = [System.IO.Compression.ZipFile]::OpenRead($env:CPM_ZIP)',
    'try { foreach ($e in $zip.Entries) {',
    "  if ([System.IO.Path]::IsPathRooted($e.FullName) -or $e.FullName -match '(^|[\\\\/])\\.\\.([\\\\/]|$)') { throw \"unsafe path in archive: $($e.FullName)\" }",
    "  if ((($e.ExternalAttributes -shr 16) -band 0xF000) -eq 0xA000) { throw \"symlink in archive: $($e.FullName)\" }",
    '} } finally { $zip.Dispose() }',
    'Expand-Archive -LiteralPath $env:CPM_ZIP -DestinationPath $env:CPM_DEST -Force',
  ].join('\n');
  try {
    execFileSync('powershell.exe', ['-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-Command', script], {
      cwd: workDir,
      env: { ...process.env, CPM_ZIP: path.join(workDir, archiveName), CPM_DEST: path.join(workDir, extractName) },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
  } catch (err) {
    const detail = err.stderr ? String(err.stderr).trim() : err.message;
    throw new LauncherError(`cannot extract .zip archive: ${detail}`);
  }
}

function findBinary(dir, name) {
  const direct = path.join(dir, name);
  if (fs.existsSync(direct)) return direct;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.isDirectory()) {
      const found = findBinary(path.join(dir, entry.name), name);
      if (found) return found;
    }
  }
  return null;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Cross-process lock: an exclusively created file. Returns a release function,
// or null when `isDone()` became true while waiting (another process finished).
async function acquireLock(lockPath, isDone, { staleMs = LOCK_STALE_MS, waitMs = LOCK_WAIT_MS } = {}) {
  const started = Date.now();
  for (;;) {
    try {
      const fd = fs.openSync(lockPath, 'wx');
      fs.writeSync(fd, JSON.stringify({ pid: process.pid, at: new Date().toISOString() }));
      fs.closeSync(fd);
      return () => fs.rmSync(lockPath, { force: true });
    } catch (err) {
      if (err.code !== 'EEXIST') throw new LauncherError(`cannot create lock ${lockPath}: ${err.message}`);
    }
    if (isDone()) return null;
    try {
      if (Date.now() - fs.statSync(lockPath).mtimeMs > staleMs) {
        fs.rmSync(lockPath, { force: true });
        continue;
      }
    } catch {
      continue; // lock vanished between open and stat: retry at once
    }
    if (Date.now() - started > waitMs) {
      throw new LauncherError(`timed out waiting for another download to finish (lock ${lockPath}; delete it if no download is running)`);
    }
    await sleep(LOCK_POLL_MS);
  }
}

async function downloadAndInstall({ version, t, base, allowedHosts, env, versionDir, finalPath, log }) {
  const releaseUrl = `${base.href.replace(/\/+$/, '')}/download/v${version}`;
  const workDir = fs.mkdtempSync(path.join(versionDir, '.download-'));
  try {
    const archive = path.join(workDir, t.asset);
    const sums = path.join(workDir, 'checksums.sha256');
    log(`downloading ${BIN} v${version} for ${t.target} from ${releaseUrl}/${t.asset}`);
    await download(`${releaseUrl}/${t.asset}`, archive, { allowedHosts, env, version });
    await download(`${releaseUrl}/checksums.sha256`, sums, { allowedHosts, env, version, maxBytes: 1024 * 1024 });
    const expected = expectedDigest(fs.readFileSync(sums, 'utf8'), t.asset);
    if (!expected) throw new LauncherError(`no checksum entry for ${t.asset} in checksums.sha256`);
    const actual = sha256File(archive);
    if (actual !== expected) {
      throw new LauncherError(`checksum mismatch for ${t.asset} (expected ${expected}, got ${actual})`);
    }
    log(`checksum verified: ${actual}`);
    if (t.ext === 'zip') extractZip(workDir, t.asset, 'extract');
    else extractTarGz(workDir, t.asset, 'extract');
    const src = findBinary(path.join(workDir, 'extract'), t.binary);
    if (!src) throw new LauncherError(`binary '${t.binary}' not found inside ${t.asset}`);
    if (fs.lstatSync(src).isSymbolicLink()) throw new LauncherError('refusing a symlinked binary');
    if (process.platform !== 'win32') fs.chmodSync(src, 0o755);
    fs.renameSync(src, finalPath);
    log(`installed ${finalPath}`);
  } finally {
    fs.rmSync(workDir, { recursive: true, force: true });
  }
}

// Returns the path of a runnable cpm-planner binary, downloading and verifying
// it on first use. Options exist for tests; real runs use the defaults.
async function ensureBinary(opts = {}) {
  const env = opts.env || process.env;
  const log = opts.log || ((msg) => process.stderr.write(`cpm-planner (npm): ${msg}\n`));
  if (env.CPM_PLANNER_BINARY) {
    if (!fs.existsSync(env.CPM_PLANNER_BINARY)) {
      throw new LauncherError(`CPM_PLANNER_BINARY points to a missing file: ${env.CPM_PLANNER_BINARY}`);
    }
    return env.CPM_PLANNER_BINARY;
  }
  const version = opts.version || PKG_VERSION;
  const t = opts.target || hostTarget(opts.platform, opts.arch);
  const versionDir = path.join(opts.cacheRoot || cacheRoot(env), BIN, version);
  const finalPath = path.join(versionDir, t.binary);
  if (fs.existsSync(finalPath)) return finalPath;

  const base = parseBase(env.CPM_PLANNER_DOWNLOAD_BASE || DEFAULT_BASE, env);
  const allowedHosts = [...new Set([base.host, ...REDIRECT_HOSTS])];
  fs.mkdirSync(versionDir, { recursive: true });
  const release = await acquireLock(path.join(versionDir, '.lock'), () => fs.existsSync(finalPath), opts.lock);
  if (!release) return finalPath;
  try {
    if (!fs.existsSync(finalPath)) {
      await downloadAndInstall({ version, t, base, allowedHosts, env, versionDir, finalPath, log });
    }
    return finalPath;
  } finally {
    release();
  }
}

function formatError(err) {
  const msg = err instanceof LauncherError ? err.message : (err && err.stack) || String(err);
  return [
    `cpm-planner (npm): error: ${msg}`,
    '  remedy: set CPM_PLANNER_BINARY to a cpm-planner binary you installed yourself, or install one manually:',
    `    ${INSTALL_DOCS}`,
    `    ${RELEASES_PAGE}`,
    '',
  ].join('\n');
}

module.exports = {
  LauncherError,
  TARGETS,
  resolveTarget,
  hostTarget,
  cacheRoot,
  parseBase,
  expectedDigest,
  acquireLock,
  ensureBinary,
  formatError,
};
