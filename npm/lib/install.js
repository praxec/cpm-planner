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
const VERIFIED_FILE = '.verified';
const LOCK_FILE = '.lock';

// Download limits. ensureBinary({ limits }) overrides them (tests only).
const DEFAULT_LIMITS = {
  maxBytes: 134217728, // same cap as scripts/install.sh (PRAXEC_MAX_BYTES default)
  maxRedirects: 5,
  idleTimeoutMs: 30000,
  totalTimeoutMs: 300000,
};
// Lock timing. The holder refreshes the lock's mtime every heartbeatMs, so a
// lock not touched for staleMs is abandoned whatever pid it names (pids are
// reused, e.g. pid 1 in containers). A lock naming a dead pid on this host is
// taken over at once. Waiting outlasts both the stale threshold and a full
// download (DEFAULT_LIMITS.totalTimeoutMs).
const DEFAULT_LOCK = { staleMs: 2 * 60 * 1000, heartbeatMs: 30 * 1000, waitMs: 6 * 60 * 1000, pollMs: 100 };
// Tokens of the locks this process holds right now.
const heldTokens = new Set();

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

// Returns `url` without any user:password part, for logs and errors.
function redact(url) {
  try {
    const u = new URL(String(url));
    if (!u.username && !u.password) return u.href;
    u.username = '';
    u.password = '';
    return u.href;
  } catch {
    return String(url).replace(/\/\/[^/@\s]*@/, '//');
  }
}

// Validates the release base URL. https only unless PRAXEC_ALLOW_INSECURE=1,
// the same rule as scripts/install.sh and scripts/install.ps1.
function parseBase(base, env) {
  let url;
  try {
    url = new URL(base);
  } catch {
    throw new LauncherError(`invalid download base URL '${redact(base)}'`);
  }
  const ok = url.protocol === 'https:' || (insecureAllowed(env) && url.protocol === 'http:');
  if (!ok) {
    throw new LauncherError(
      `refusing non-https base URL '${redact(base)}' (set PRAXEC_ALLOW_INSECURE=1 to override for local testing)`,
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
function download(url, dest, { allowedHosts, env, version, limits, maxBytes = limits.maxBytes }) {
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
      () => finish(new LauncherError(`download timed out after ${limits.totalTimeoutMs / 1000}s: ${redact(url)}`)),
      limits.totalTimeoutMs,
    );

    const get = (current, redirectsLeft) => {
      const shown = redact(current);
      const mod = current.protocol === 'https:' ? https : http;
      req = mod.get(current, { headers: { 'user-agent': `cpm-planner-npm/${version}` }, timeout: limits.idleTimeoutMs }, (res) => {
        const status = res.statusCode || 0;
        if (status >= 300 && status < 400 && res.headers.location) {
          res.resume();
          if (redirectsLeft <= 0) return finish(new LauncherError(`too many redirects (more than ${limits.maxRedirects}): ${redact(url)}`));
          let next;
          try {
            next = new URL(res.headers.location, current);
            checkRedirect(next, allowedHosts, env);
          } catch (err) {
            return finish(err instanceof LauncherError ? err : new LauncherError(`invalid redirect from ${shown}`));
          }
          return get(next, redirectsLeft - 1);
        }
        if (status !== 200) {
          res.resume();
          const hint = status === 404 ? ` (is release v${version} published with this asset?)` : '';
          return finish(new LauncherError(`download failed with HTTP ${status}: ${shown}${hint}`));
        }
        let bytes = 0;
        const out = fs.createWriteStream(dest, { flags: 'wx', mode: 0o600 });
        res.on('data', (chunk) => {
          bytes += chunk.length;
          if (bytes > maxBytes) {
            res.destroy();
            out.destroy();
            finish(new LauncherError(`download exceeds ${maxBytes} bytes: ${shown}`));
          }
        });
        res.on('error', (err) => finish(new LauncherError(`download interrupted: ${shown}: ${err.message}`)));
        out.on('error', (err) => finish(new LauncherError(`cannot write ${dest}: ${err.message}`)));
        out.on('close', () => {
          if (bytes === 0) return finish(new LauncherError(`downloaded file is empty: ${shown}`));
          finish();
        });
        res.pipe(out);
      });
      req.on('timeout', () => req.destroy(new Error(`no data for ${limits.idleTimeoutMs / 1000}s`)));
      req.on('error', (err) => finish(err instanceof LauncherError ? err : new LauncherError(`download failed: ${shown}: ${err.message}`)));
    };
    get(new URL(url), limits.maxRedirects);
  });
}

// Streams the file through the hash so a large binary is never fully in memory.
function sha256File(file) {
  return new Promise((resolve, reject) => {
    const hash = crypto.createHash('sha256');
    fs.createReadStream(file)
      .on('error', reject)
      .on('data', (chunk) => hash.update(chunk))
      .on('end', () => resolve(hash.digest('hex')));
  });
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

// True for archive entry names that could land outside the extract dir.
function unsafeEntry(name) {
  const n = name.replace(/\\/g, '/');
  return n.startsWith('/') || /^[A-Za-z]:/.test(n) || n.split('/').includes('..');
}

// True for a `tar -tv` listing line describing a link or special file.
function specialEntry(listingLine) {
  return /^[lhbcp]/.test(listingLine);
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
  if (verbose.split(/\r?\n/).filter(Boolean).some(specialEntry)) {
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

// A regular file (not a link or directory) that is executable on POSIX.
function isRunnableFile(file) {
  const st = fs.lstatSync(file);
  if (!st.isFile()) return false;
  return process.platform === 'win32' || (st.mode & 0o111) !== 0;
}

// Finds the binary at the archive root, or anywhere below it as install.sh does.
function findBinary(dir, name) {
  const entries = fs.readdirSync(dir, { withFileTypes: true });
  for (const entry of entries) {
    if (entry.name === name && isRunnableFile(path.join(dir, name))) return path.join(dir, name);
  }
  for (const entry of entries) {
    if (entry.isDirectory()) {
      const found = findBinary(path.join(dir, entry.name), name);
      if (found) return found;
    }
  }
  return null;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// process.kill(pid, 0) probes a pid without signalling it: ESRCH means no such
// process, EPERM means it exists but belongs to someone else.
function pidAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return err.code !== 'ESRCH';
  }
}

function readLock(lockPath) {
  try {
    const raw = fs.readFileSync(lockPath, 'utf8');
    let owner = null;
    try {
      owner = JSON.parse(raw);
    } catch {
      // being written right now, or garbage: judged by age only
    }
    return { raw, owner, mtimeMs: fs.statSync(lockPath).mtimeMs };
  } catch {
    return null; // vanished
  }
}

function lockIsStale(lock, staleMs) {
  if (Date.now() - lock.mtimeMs > staleMs) return true; // no heartbeat: abandoned
  const { owner } = lock;
  if (!owner || !Number.isInteger(owner.pid) || owner.host !== os.hostname()) return false;
  // Our own pid on a lock we do not hold: a previous process with this pid died.
  if (owner.pid === process.pid) return !heldTokens.has(owner.token);
  return !pidAlive(owner.pid);
}

// Cross-process lock: an exclusively created file holding the owner's pid,
// host and a random token, kept fresh by a heartbeat while held. Returns a
// release function that stops the heartbeat and deletes the lock only while it
// still carries our token, or null when `isDone()` became true while waiting
// (another process finished the download).
async function acquireLock(lockPath, isDone, opts = {}) {
  const { staleMs, heartbeatMs, waitMs, pollMs } = { ...DEFAULT_LOCK, ...opts };
  const token = crypto.randomBytes(16).toString('hex');
  const ours = () => {
    const lock = readLock(lockPath);
    return Boolean(lock && lock.owner && lock.owner.token === token);
  };
  const started = Date.now();
  for (;;) {
    try {
      const fd = fs.openSync(lockPath, 'wx', 0o600);
      fs.writeSync(fd, JSON.stringify({ pid: process.pid, host: os.hostname(), token, at: new Date().toISOString() }));
      fs.closeSync(fd);
      heldTokens.add(token);
      const heartbeat = setInterval(() => {
        try {
          if (ours()) fs.utimesSync(lockPath, new Date(), new Date());
        } catch {
          // lock gone or unwritable: the next waiter will judge it
        }
      }, heartbeatMs);
      heartbeat.unref();
      return () => {
        clearInterval(heartbeat);
        heldTokens.delete(token);
        if (ours()) fs.rmSync(lockPath, { force: true });
      };
    } catch (err) {
      if (err.code !== 'EEXIST') throw new LauncherError(`cannot create lock ${lockPath}: ${err.message}`);
    }
    if (isDone()) return null;
    const lock = readLock(lockPath);
    if (!lock) continue; // released between open and read: retry at once
    if (lockIsStale(lock, staleMs)) {
      // Only remove the lock we judged; a fresh one written meanwhile stays.
      const again = readLock(lockPath);
      if (again && again.raw === lock.raw) fs.rmSync(lockPath, { force: true });
      continue;
    }
    if (Date.now() - started > waitMs) {
      throw new LauncherError(`timed out waiting for another download to finish (lock ${lockPath}; delete it if no download is running)`);
    }
    await sleep(pollMs);
  }
}

// Removes work dirs left by a holder that died without cleaning up (SIGKILL,
// power loss). Called only while holding the lock, so none of them is live.
function removeStaleWorkDirs(versionDir) {
  for (const name of fs.readdirSync(versionDir)) {
    if (name.startsWith('.download-')) fs.rmSync(path.join(versionDir, name), { recursive: true, force: true });
  }
}

// Refuses a cache directory another user could have written to (POSIX).
function checkCacheDirTrust(versionDir) {
  if (process.platform === 'win32' || typeof process.getuid !== 'function') return;
  const st = fs.statSync(versionDir);
  const remedy = `remove ${versionDir} so it is downloaded again, or set CPM_PLANNER_CACHE_DIR to a private directory`;
  if (st.uid !== process.getuid()) {
    throw new LauncherError(`refusing cached binary: ${versionDir} is owned by uid ${st.uid}, not ${process.getuid()}; ${remedy}`);
  }
  if ((st.mode & 0o022) !== 0) {
    throw new LauncherError(`refusing cached binary: ${versionDir} is writable by group or others (mode ${(st.mode & 0o777).toString(8)}); ${remedy}`);
  }
}

// Re-checks a cached binary against the digest recorded when it was installed.
async function checkCachedBinary(versionDir, finalPath) {
  checkCacheDirTrust(versionDir);
  const remedy = `remove ${versionDir} to download it again`;
  let recorded;
  try {
    recorded = fs.readFileSync(path.join(versionDir, VERIFIED_FILE), 'utf8').trim();
  } catch {
    throw new LauncherError(`refusing cached binary ${finalPath}: no ${VERIFIED_FILE} digest; ${remedy}`);
  }
  const actual = await sha256File(finalPath);
  if (actual !== recorded) {
    throw new LauncherError(`refusing cached binary ${finalPath}: sha256 ${actual} does not match the verified ${recorded}; ${remedy}`);
  }
}

async function downloadAndInstall({ version, t, base, allowedHosts, env, versionDir, finalPath, log, limits, state }) {
  const releaseUrl = `${base.href.replace(/\/+$/, '')}/download/v${version}`;
  removeStaleWorkDirs(versionDir);
  const workDir = fs.mkdtempSync(path.join(versionDir, '.download-'));
  state.workDir = workDir;
  try {
    const archive = path.join(workDir, t.asset);
    const sums = path.join(workDir, 'checksums.sha256');
    log(`downloading ${BIN} v${version} for ${t.target} from ${redact(`${releaseUrl}/${t.asset}`)}`);
    await download(`${releaseUrl}/${t.asset}`, archive, { allowedHosts, env, version, limits });
    await download(`${releaseUrl}/checksums.sha256`, sums, { allowedHosts, env, version, limits, maxBytes: 1024 * 1024 });
    const expected = expectedDigest(fs.readFileSync(sums, 'utf8'), t.asset);
    if (!expected) throw new LauncherError(`no checksum entry for ${t.asset} in checksums.sha256`);
    const actual = await sha256File(archive);
    if (actual !== expected) {
      throw new LauncherError(`checksum mismatch for ${t.asset} (expected ${expected}, got ${actual})`);
    }
    log(`checksum verified: ${actual}`);
    if (t.ext === 'zip') extractZip(workDir, t.asset, 'extract');
    else extractTarGz(workDir, t.asset, 'extract');
    const src = findBinary(path.join(workDir, 'extract'), t.binary);
    if (!src) throw new LauncherError(`no executable file '${t.binary}' inside ${t.asset}`);
    if (process.platform !== 'win32') fs.chmodSync(src, 0o755);
    // Record the digest first: a binary without a matching .verified is refused.
    const verifiedTmp = path.join(workDir, VERIFIED_FILE);
    fs.writeFileSync(verifiedTmp, `${await sha256File(src)}\n`, { mode: 0o600 });
    fs.renameSync(verifiedTmp, path.join(versionDir, VERIFIED_FILE));
    fs.renameSync(src, finalPath);
    log(`installed ${finalPath}`);
  } finally {
    fs.rmSync(workDir, { recursive: true, force: true });
    state.workDir = null;
  }
}

// While the lock is held, a terminating signal removes the work dir and the
// lock, then re-raises the signal so the process still dies by it.
function cleanupOnSignals(cleanup) {
  const signals = process.platform === 'win32' ? ['SIGINT', 'SIGTERM'] : ['SIGINT', 'SIGTERM', 'SIGHUP'];
  const handlers = signals.map((sig) => {
    const handler = () => {
      remove();
      try {
        cleanup();
      } finally {
        process.kill(process.pid, sig);
      }
    };
    process.on(sig, handler);
    return [sig, handler];
  });
  function remove() {
    for (const [sig, handler] of handlers) process.removeListener(sig, handler);
  }
  return remove;
}

function checkLocalBinary(file) {
  if (/\.(cmd|bat)$/i.test(file)) {
    throw new LauncherError(`CPM_PLANNER_BINARY must be a native executable (.exe on Windows), not a .cmd/.bat script: ${file}`);
  }
  if (!fs.existsSync(file)) {
    throw new LauncherError(`CPM_PLANNER_BINARY points to a missing file: ${file}`);
  }
  return file;
}

// Returns the path of a runnable cpm-planner binary, downloading and verifying
// it on first use. Options other than the defaults exist for tests.
async function ensureBinary(opts = {}) {
  const env = opts.env || process.env;
  const log = opts.log || ((msg) => process.stderr.write(`cpm-planner (npm): ${msg}\n`));
  if (env.CPM_PLANNER_BINARY) return checkLocalBinary(env.CPM_PLANNER_BINARY);
  const limits = { ...DEFAULT_LIMITS, ...opts.limits };
  const version = opts.version || PKG_VERSION;
  const t = opts.target || hostTarget(opts.platform, opts.arch);
  const versionDir = path.join(opts.cacheRoot || cacheRoot(env), BIN, version);
  const finalPath = path.join(versionDir, t.binary);
  if (fs.existsSync(finalPath)) {
    await checkCachedBinary(versionDir, finalPath);
    return finalPath;
  }

  const base = parseBase(env.CPM_PLANNER_DOWNLOAD_BASE || DEFAULT_BASE, env);
  const allowedHosts = [...new Set([base.host, ...REDIRECT_HOSTS])];
  fs.mkdirSync(versionDir, { recursive: true, mode: 0o700 });
  checkCacheDirTrust(versionDir);
  // Handlers go in before the lock exists, so no signal can orphan it.
  const state = { workDir: null, release: null };
  const removeSignalHandlers = cleanupOnSignals(() => {
    if (state.workDir) fs.rmSync(state.workDir, { recursive: true, force: true });
    if (state.release) state.release();
  });
  try {
    state.release = await acquireLock(path.join(versionDir, LOCK_FILE), () => fs.existsSync(finalPath), opts.lock);
    if (state.release && !fs.existsSync(finalPath)) {
      await downloadAndInstall({ version, t, base, allowedHosts, env, versionDir, finalPath, log, limits, state });
    } else {
      await checkCachedBinary(versionDir, finalPath);
    }
    return finalPath;
  } finally {
    removeSignalHandlers();
    if (state.release) state.release();
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
  checkRedirect,
  redact,
  expectedDigest,
  unsafeEntry,
  specialEntry,
  acquireLock,
  ensureBinary,
  formatError,
};
