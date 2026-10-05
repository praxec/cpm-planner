#!/usr/bin/env node
// mcp-smoke.mjs — verify a packaged MCP stdio server answers the protocol
// handshake (`initialize`) and advertises tools (`tools/list`).
//
// Uses only Node builtins; makes no API calls, loads no models, and changes no
// production state. Exits non-zero if either step fails or times out.
import { spawn } from 'node:child_process';

const bin = process.argv[2];
if (!bin) {
  console.error('usage: mcp-smoke.mjs <path-to-mcp-binary>');
  process.exit(2);
}

const child = spawn(bin, [], { stdio: ['pipe', 'pipe', 'pipe'] });
const pending = new Map();
let buffer = '';
let stderrText = '';
child.stderr.on('data', (d) => { stderrText += d.toString(); });

child.on('exit', (code, signal) => {
  for (const { reject } of pending.values()) {
    reject(new Error(`server exited early (code=${code} signal=${signal}) ${stderrText.trim()}`));
  }
  pending.clear();
});

child.stdout.on('data', (chunk) => {
  buffer += chunk.toString();
  let idx;
  while ((idx = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    let msg;
    try { msg = JSON.parse(line); } catch { continue; }
    if (msg.id != null && pending.has(msg.id)) {
      const entry = pending.get(msg.id);
      pending.delete(msg.id);
      entry.resolve(msg);
    }
  }
});

function request(id, method, params) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`timed out waiting for ${method}`));
    }, 15000);
    pending.set(id, {
      resolve: (m) => { clearTimeout(timer); resolve(m); },
      reject: (e) => { clearTimeout(timer); reject(e); },
    });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n');
  });
}

function notify(method, params) {
  child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method, params }) + '\n');
}

try {
  const init = await request(1, 'initialize', {
    protocolVersion: '2024-11-05',
    capabilities: {},
    clientInfo: { name: 'praxec-release-smoke', version: '1.0.0' },
  });
  if (init.error) throw new Error('initialize returned error: ' + JSON.stringify(init.error));
  if (!init.result) throw new Error('initialize returned no result');

  notify('notifications/initialized');

  const list = await request(2, 'tools/list', {});
  if (list.error) throw new Error('tools/list returned error: ' + JSON.stringify(list.error));
  const tools = list.result && list.result.tools;
  if (!Array.isArray(tools) || tools.length === 0) {
    throw new Error('tools/list advertised no tools');
  }

  child.kill();
  console.log(`mcp-smoke: ok (${tools.length} tools) via ${bin}`);
  process.exit(0);
} catch (err) {
  child.kill();
  console.error('mcp-smoke: ' + err.message);
  process.exit(1);
}
