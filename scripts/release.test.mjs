// release.test.mjs — fast, offline behavioral tests for the Praxec release
// installers and packaging metadata. Each test makes exactly one behavioral
// assertion about a public interface (install.sh, install.ps1,
// release-manifest.sh, mcp-smoke.mjs). No network, no machine-config changes.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { spawnSync, execFileSync } from 'node:child_process';
import { createHash } from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const ROOT = import.meta.dirname;
const TAG = 'v1.2.3';
const SIX_TARGETS = [
  ['x86_64-unknown-linux-gnu', 'tar.gz'],
  ['aarch64-unknown-linux-gnu', 'tar.gz'],
  ['x86_64-apple-darwin', 'tar.gz'],
  ['aarch64-apple-darwin', 'tar.gz'],
  ['x86_64-pc-windows-msvc', 'zip'],
  ['aarch64-pc-windows-msvc', 'zip'],
];
const REPOS = [{ repo: 'cpm-planner', bin: 'cpm-planner' }];

function scriptPath(repo, name) {
  return path.join(ROOT, name);
}

function run(cmd, args, opts = {}) {
  const res = spawnSync(cmd, args, {
    encoding: 'utf8',
    timeout: 60000,
    cwd: opts.cwd ?? ROOT,
    // Fixtures are served over file://, so allow insecure by default; tests of the https rule override it.
    env: { ...process.env, PRAXEC_ALLOW_INSECURE: '1', ...(opts.env ?? {}) },
  });
  return { status: res.status, stdout: res.stdout ?? '', stderr: res.stderr ?? '' };
}

function sha256(file) {
  return createHash('sha256').update(fs.readFileSync(file)).digest('hex');
}

function rewriteChecksums(releaseDir) {
  const lines = fs.readdirSync(releaseDir)
    .filter((f) => f.endsWith('.tar.gz'))
    .sort()
    .map((f) => `${sha256(path.join(releaseDir, f))}  ${f}`);
  fs.writeFileSync(path.join(releaseDir, 'checksums.sha256'), lines.join('\n') + '\n');
}

// Builds a directory tree shaped like a GitHub release download page, served
// to the installer over file:// so tests stay fully offline.
function makeReleaseFixture(bin, assets, mutate) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'praxec-release-'));
  const releaseDir = path.join(root, 'releases', 'download', TAG);
  fs.mkdirSync(releaseDir, { recursive: true });
  for (const asset of assets) {
    const stage = path.join(root, 'stage', asset.target);
    fs.mkdirSync(stage, { recursive: true });
    const binary = path.join(stage, bin);
    fs.writeFileSync(binary, asset.payload, { mode: 0o755 });
    execFileSync('tar', ['-czf', path.join(releaseDir, `${bin}-${asset.target}.tar.gz`), '-C', stage, bin]);
  }
  if (mutate) mutate({ root, releaseDir });
  rewriteChecksums(releaseDir);
  return { root, releaseDir, baseUrl: `file://${path.join(root, 'releases')}` };
}

function tempDir(t) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'praxec-dest-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  return dir;
}

function installArgs(fixture, dest) {
  return [
    '--base-url', fixture.baseUrl,
    '--version', TAG,
    '--install-dir', dest,
  ];
}

// ---------------------------------------------------------------------------
// install.sh: target resolution across all three repositories
// ---------------------------------------------------------------------------
for (const { repo, bin } of REPOS) {
  test(`${repo}/install.sh resolves the native linux/x86_64 release target`, () => {
    const r = run('sh', [scriptPath(repo, 'install.sh'), '--print-target'], {
      env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
    });
    assert.equal(r.stdout.trim(), 'x86_64-unknown-linux-gnu');
  });
}

// ---------------------------------------------------------------------------
// install.sh: end-to-end install behavior
// ---------------------------------------------------------------------------
for (const { repo, bin } of REPOS) {
  test(`${repo}/install.sh installs the linux/x86_64 asset into the managed directory`, (t) => {
    const fixture = makeReleaseFixture(bin, [
      { target: 'x86_64-unknown-linux-gnu', payload: 'NATIVE-X64\n' },
      { target: 'aarch64-unknown-linux-gnu', payload: 'NATIVE-ARM\n' },
    ]);
    t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
    const dest = tempDir(t);
    run('sh', [scriptPath(repo, 'install.sh'), ...installArgs(fixture, dest)], {
      env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
    });
    assert.equal(fs.readFileSync(path.join(dest, bin), 'utf8'), 'NATIVE-X64\n');
  });
}

test('cpm-planner/install.sh selects the aarch64 asset for an arm64 host', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'NATIVE-X64\n' },
    { target: 'aarch64-unknown-linux-gnu', payload: 'NATIVE-ARM\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'aarch64' },
  });
  assert.equal(fs.readFileSync(path.join(dest, 'cpm-planner'), 'utf8'), 'NATIVE-ARM\n');
});

test('cpm-planner/install.sh atomically replaces an existing installed binary', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'NEW-BUILD\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  fs.writeFileSync(path.join(dest, 'cpm-planner'), 'OLD-BUILD\n', { mode: 0o755 });
  run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
  });
  assert.equal(fs.readFileSync(path.join(dest, 'cpm-planner'), 'utf8'), 'NEW-BUILD\n');
});

test('cpm-planner/install.sh refuses an asset whose checksum does not match', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'PAYLOAD\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  fs.writeFileSync(
    path.join(fixture.releaseDir, 'checksums.sha256'),
    `${'0'.repeat(64)}  cpm-planner-x86_64-unknown-linux-gnu.tar.gz\n`,
  );
  const dest = tempDir(t);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
  });
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh fails on an unsupported operating system instead of compiling', () => {
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), '--print-target'], {
    env: { PRAXEC_OS: 'freebsd', PRAXEC_ARCH: 'x86_64' },
  });
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh fails on an unsupported CPU architecture instead of compiling', () => {
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), '--print-target'], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'ppc64le' },
  });
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh rejects a tar archive containing parent-directory traversal', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'X\n' },
  ], ({ releaseDir }) => {
    execFileSync('python3', ['-c', `
import io, sys, tarfile
with tarfile.open(sys.argv[1], 'w:gz') as t:
    data = b'escaped'
    info = tarfile.TarInfo('../escape.txt')
    info.size = len(data)
    t.addfile(info, io.BytesIO(data))
`, path.join(releaseDir, 'cpm-planner-x86_64-unknown-linux-gnu.tar.gz')]);
  });
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
  });
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh rejects a tar archive containing a symlink entry', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'X\n' },
  ], ({ releaseDir }) => {
    execFileSync('python3', ['-c', `
import sys, tarfile
with tarfile.open(sys.argv[1], 'w:gz') as t:
    info = tarfile.TarInfo('link')
    info.type = tarfile.SYMTYPE
    info.linkname = '/etc/passwd'
    t.addfile(info)
`, path.join(releaseDir, 'cpm-planner-x86_64-unknown-linux-gnu.tar.gz')]);
  });
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
  });
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh resolves native arm64 when macOS runs under Rosetta', (t) => {
  const fakeBin = tempDir(t);
  fs.writeFileSync(path.join(fakeBin, 'uname'), '#!/bin/sh\ncase "$1" in -m) echo x86_64;; *) echo Darwin;; esac\n', { mode: 0o755 });
  fs.writeFileSync(path.join(fakeBin, 'sysctl'), '#!/bin/sh\necho 1\n', { mode: 0o755 });
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), '--print-target'], {
    env: { PATH: `${fakeBin}:${process.env.PATH}`, PRAXEC_OS: '', PRAXEC_ARCH: '' },
  });
  assert.equal(r.stdout.trim(), 'aarch64-apple-darwin');
});

test('cpm-planner/install.sh refuses a non-https base URL unless explicitly overridden', (t) => {
  const dest = tempDir(t);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), '--base-url', 'http://example.invalid/releases', '--version', TAG, '--install-dir', dest], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64', PRAXEC_ALLOW_INSECURE: '' },
  });
  assert.match(r.stderr, /non-https/);
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh fails when HOME is unset and no install dir is given', () => {
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), '--dry-run', '--base-url', 'https://example.invalid/releases'], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64', HOME: '' },
  });
  assert.match(r.stderr, /HOME is not set/);
  assert.notEqual(r.status, 0);
});

test('cpm-planner/install.sh leaves no temp files behind after an upgrade', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'NEW\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  fs.writeFileSync(path.join(dest, 'cpm-planner'), 'OLD\n', { mode: 0o755 });
  run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
  });
  assert.deepEqual(fs.readdirSync(dest), ['cpm-planner']);
});

test('cpm-planner/install.sh prints the exact PATH line when the install dir is not on PATH', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'X\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest)], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64' },
  });
  assert.ok(r.stderr.includes(`export PATH="${dest}:$PATH"`), r.stderr);
});

test('cpm-planner/install.sh --add-to-path appends to the shell rc once', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'x86_64-unknown-linux-gnu', payload: 'X\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  const home = tempDir(t);
  const env = { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64', HOME: home, SHELL: '/bin/bash' };
  for (let i = 0; i < 2; i++) {
    run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest), '--add-to-path'], { env });
  }
  const rc = fs.readFileSync(path.join(home, '.bashrc'), 'utf8');
  assert.equal(rc.split(`export PATH="${dest}:$PATH"`).length - 1, 1);
});

test('cpm-planner/install.sh --add-to-path without HOME fails with a clear message', (t) => {
  const dest = tempDir(t);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), '--add-to-path', '--install-dir', dest, '--version', TAG], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64', HOME: '' },
  });
  assert.match(r.stderr, /HOME is not set/);
  assert.doesNotMatch(r.stderr, /unbound variable/);
  assert.notEqual(r.status, 0);
});

function installWithShell(t, shell, setup) {
  const fixture = makeReleaseFixture('cpm-planner', [{ target: 'x86_64-unknown-linux-gnu', payload: 'X\n' }]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  const home = tempDir(t);
  if (setup) setup(home);
  const r = run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest), '--add-to-path'], {
    env: { PRAXEC_OS: 'linux', PRAXEC_ARCH: 'x86_64', HOME: home, SHELL: shell },
  });
  return { r, home, dest };
}

test('cpm-planner/install.sh --add-to-path prefers an existing ~/.bash_profile for bash', (t) => {
  const { home, dest } = installWithShell(t, '/bin/bash', (h) => fs.writeFileSync(path.join(h, '.bash_profile'), '# mine\n'));
  assert.ok(fs.readFileSync(path.join(home, '.bash_profile'), 'utf8').includes(`export PATH="${dest}:$PATH"`));
});

test('cpm-planner/install.sh --add-to-path uses ~/.bash_profile for bash on macOS', (t) => {
  const fixture = makeReleaseFixture('cpm-planner', [
    { target: 'aarch64-apple-darwin', payload: 'X\n' },
  ]);
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  const dest = tempDir(t);
  const home = tempDir(t);
  run('sh', [scriptPath('cpm-planner', 'install.sh'), ...installArgs(fixture, dest), '--add-to-path'], {
    env: { PRAXEC_OS: 'darwin', PRAXEC_ARCH: 'aarch64', HOME: home, SHELL: '/bin/bash' },
  });
  assert.ok(fs.readFileSync(path.join(home, '.bash_profile'), 'utf8').includes(`export PATH="${dest}:$PATH"`));
});

test('cpm-planner/install.sh --add-to-path prints fish_add_path for fish and writes no rc file', (t) => {
  const { r, home, dest } = installWithShell(t, '/usr/bin/fish');
  assert.ok(r.stderr.includes(`fish_add_path ${dest}`), r.stderr);
  assert.deepEqual(fs.readdirSync(home), []);
});

const PWSH = spawnSync('pwsh', ['-NoProfile', '-Command', '1']).status === 0;
test('cpm-planner/install.ps1 resolves the native windows/x86_64 target', { skip: !PWSH }, () => {
  const r = run('pwsh', ['-NoProfile', '-File', scriptPath('cpm-planner', 'install.ps1'), '-PrintTarget'], {
    env: { PRAXEC_OS: 'windows', PRAXEC_ARCH: 'x86_64' },
  });
  assert.equal(r.stdout.trim(), 'x86_64-pc-windows-msvc');
});

test('cpm-planner/install.ps1 refuses a non-https base URL unless overridden', { skip: !PWSH }, () => {
  const r = run('pwsh', ['-NoProfile', '-File', scriptPath('cpm-planner', 'install.ps1'), '-DryRun'], {
    env: { PRAXEC_OS: 'windows', PRAXEC_ARCH: 'x86_64', PRAXEC_BASE_URL: 'http://example.invalid/r', PRAXEC_ALLOW_INSECURE: '' },
  });
  assert.notEqual(r.status, 0);
});

// ---------------------------------------------------------------------------
// release-manifest.sh: completeness and metadata
// ---------------------------------------------------------------------------
function makeManifestFixture(bin, outName = 'out') {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'praxec-manifest-'));
  const dist = path.join(root, 'dist');
  const out = path.join(root, outName);
  fs.mkdirSync(dist, { recursive: true });
  for (const [target, ext] of SIX_TARGETS) {
    fs.writeFileSync(path.join(dist, `${bin}-${target}.${ext}`), `payload ${target}\n`);
  }
  return { root, dist, out };
}

function runManifest(fixture, bin) {
  return run('sh', [
    path.join(ROOT, 'release-manifest.sh'),
    '--assets-dir', fixture.dist,
    '--out-dir', fixture.out,
    '--version', '1.2.3',
    '--source-sha', 'abcdef0123456789',
    '--binary', bin,
  ]);
}

test('release-manifest.sh emits one manifest entry for every native target', (t) => {
  const fixture = makeManifestFixture('cpm-planner');
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  runManifest(fixture, 'cpm-planner');
  const manifest = JSON.parse(fs.readFileSync(path.join(fixture.out, 'release-manifest.json'), 'utf8'));
  assert.equal(manifest.targets.length, 6);
});

test('release-manifest.sh lists every packaged asset in checksums.sha256', (t) => {
  const fixture = makeManifestFixture('cpm-planner');
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  runManifest(fixture, 'cpm-planner');
  const lines = fs.readFileSync(path.join(fixture.out, 'checksums.sha256'), 'utf8').trim().split('\n');
  assert.equal(lines.length, 6);
});

test('release-manifest.sh records the requested version', (t) => {
  const fixture = makeManifestFixture('cpm-planner');
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  runManifest(fixture, 'cpm-planner');
  const manifest = JSON.parse(fs.readFileSync(path.join(fixture.out, 'release-manifest.json'), 'utf8'));
  assert.equal(manifest.version, '1.2.3');
});

test('release-manifest.sh records the source commit SHA', (t) => {
  const fixture = makeManifestFixture('cpm-planner');
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  runManifest(fixture, 'cpm-planner');
  const manifest = JSON.parse(fs.readFileSync(path.join(fixture.out, 'release-manifest.json'), 'utf8'));
  assert.equal(manifest.sourceSha, 'abcdef0123456789');
});

test('release-manifest.sh records a sha256 digest for every target', (t) => {
  const fixture = makeManifestFixture('cpm-planner');
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  runManifest(fixture, 'cpm-planner');
  const manifest = JSON.parse(fs.readFileSync(path.join(fixture.out, 'release-manifest.json'), 'utf8'));
  assert.ok(manifest.targets.every((entry) => /^sha256:[0-9a-f]{64}$/.test(entry.digest)));
});

test('release-manifest.sh refuses to emit metadata for an incomplete matrix', (t) => {
  const fixture = makeManifestFixture('cpm-planner');
  t.after(() => fs.rmSync(fixture.root, { recursive: true, force: true }));
  fs.rmSync(path.join(fixture.dist, 'cpm-planner-aarch64-pc-windows-msvc.zip'));
  const r = runManifest(fixture, 'cpm-planner');
  assert.notEqual(r.status, 0);
});

// ---------------------------------------------------------------------------
// mcp-smoke.mjs: MCP protocol handshake over stdio
// ---------------------------------------------------------------------------
const GOOD_SERVER = `#!/usr/bin/env node
import process from 'node:process';
let buf = '';
const reply = (m) => {
  if (m.method === 'initialize') {
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: m.id, result: { protocolVersion: '2024-11-05', capabilities: { tools: {} }, serverInfo: { name: 'fake', version: '1' } } }) + '\\n');
  } else if (m.method === 'tools/list') {
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: m.id, result: { tools: ['plan.submit', 'plan.status', 'plan.get'].map((name) => ({ name, description: name, inputSchema: { type: 'object' } })) } }) + '\\n');
  }
};
process.stdin.on('data', (d) => {
  buf += d.toString();
  let i;
  while ((i = buf.indexOf('\\n')) >= 0) {
    const line = buf.slice(0, i);
    buf = buf.slice(i + 1);
    if (!line.trim()) continue;
    try { const m = JSON.parse(line); if (m.id != null) reply(m); } catch {}
  }
});
`;

const EMPTY_SERVER = GOOD_SERVER.replace(
  "result: { tools: ['plan.submit', 'plan.status', 'plan.get'].map((name) => ({ name, description: name, inputSchema: { type: 'object' } })) }",
  'result: { tools: [] }',
);

function writeServer(t, source) {
  const dir = tempDir(t);
  const file = path.join(dir, 'fake-mcp.mjs');
  fs.writeFileSync(file, source, { mode: 0o755 });
  return file;
}

test('mcp-smoke.mjs accepts a packaged server that answers initialize and tools/list', (t) => {
  const server = writeServer(t, GOOD_SERVER);
  const r = run('node', [scriptPath('cpm-planner', 'mcp-smoke.mjs'), server]);
  assert.equal(r.status, 0);
});

test('mcp-smoke.mjs rejects a packaged server that advertises no tools', (t) => {
  const server = writeServer(t, EMPTY_SERVER);
  const r = run('node', [scriptPath('cpm-planner', 'mcp-smoke.mjs'), server]);
  assert.notEqual(r.status, 0);
});

// ---------------------------------------------------------------------------
// install.ps1: Windows target resolution (only when PowerShell is available)
// ---------------------------------------------------------------------------
const hasPwsh = spawnSync('pwsh', ['-NoProfile', '-Command', 'exit 0']).status === 0;

test('cpm-planner/install.ps1 resolves the native windows/arm64 target', { skip: !hasPwsh }, () => {
  const r = run('pwsh', ['-NoProfile', '-File', scriptPath('cpm-planner', 'install.ps1'), '-PrintTarget'], {
    env: { PRAXEC_OS: 'windows', PRAXEC_ARCH: 'aarch64' },
  });
  assert.equal(r.stdout.trim(), 'aarch64-pc-windows-msvc');
});

// install.ps1 header parsing: run the real function (extracted from the script) under StrictMode
// with the shapes returned by Windows PowerShell 5.1 and PowerShell 7.
function pwshHeaderCase(expr) {
  const script = path.join(ROOT, 'install.ps1');
  const code = `
Set-StrictMode -Version Latest
$ast = [System.Management.Automation.Language.Parser]::ParseFile('${script}', [ref]$null, [ref]$null)
$fn = $ast.Find({ param($n) $n -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $n.Name -eq 'Get-LocationFromHeaders' }, $true)
. ([scriptblock]::Create($fn.Extent.Text))
[string](Get-LocationFromHeaders (${expr}))`;
  return run('pwsh', ['-NoProfile', '-Command', code]).stdout.trim();
}
const LOC = 'https://github.com/o/r/releases/tag/v1.2.3';
test('install.ps1 Get-LocationFromHeaders reads a dictionary-style (PS 5.1 WebHeaderCollection) object', { skip: !PWSH }, () => {
  assert.equal(pwshHeaderCase(`& { $h = New-Object System.Net.WebHeaderCollection; $h.Add('Location','${LOC}'); ,$h }`), LOC);
});
test('install.ps1 Get-LocationFromHeaders reads a property-style (PS 7) object with a Uri', { skip: !PWSH }, () => {
  assert.equal(pwshHeaderCase(`[pscustomobject]@{ Location = [uri]'${LOC}' }`), LOC);
});
test('install.ps1 Get-LocationFromHeaders reads a string[] value and tolerates a missing header', { skip: !PWSH }, () => {
  assert.equal(pwshHeaderCase(`@{ Location = @('${LOC}') }`), LOC);
  assert.equal(pwshHeaderCase(`@{ Other = 'x' }`), '');
});
