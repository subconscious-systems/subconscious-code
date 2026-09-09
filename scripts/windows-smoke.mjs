// Real Windows executable + real PowerShell, with a local mock model only.
// Run: node scripts/windows-smoke.mjs path/to/marathon.exe
import assert from 'node:assert/strict';
import fs from 'node:fs/promises';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';

assert.equal(process.platform, 'win32', 'Run this smoke test on native Windows');
const binary = path.resolve(process.argv[2] || 'dist/marathon.exe');
const profile = await fs.mkdtemp(path.join(os.tmpdir(), 'sc-windows-smoke-'));
await fs.mkdir(path.join(profile, '.sc'));
await fs.writeFile(path.join(profile, '.sc', 'AGENTS.md'), 'WINDOWS_PROFILE_MEMORY_MARKER');
let requests = 0;
let serverError;
const server = http.createServer(async (req, res) => {
  try {
    assert.equal(req.url, '/v1/chat/completions');
    assert.equal(req.headers.authorization, 'Bearer dummy-windows-smoke-key');
    let body = '';
    for await (const chunk of req) body += chunk;
    const request = JSON.parse(body);
    const names = request.tools.map(tool => tool.function.name);
    assert.match(JSON.stringify(request.messages.filter(message => message.role === 'system')), /WINDOWS_PROFILE_MEMORY_MARKER/);
    assert(names.includes('PowerShell'));
    assert(!names.includes('Bash'));
    assert(request.stream);
    requests++;
    res.setHeader('Content-Type', 'text/event-stream');
    const send = (delta, finish_reason = null) => res.write(`data: ${JSON.stringify({
      id: 'windows-smoke', object: 'chat.completion.chunk', created: 1, model: request.model,
      choices: [{ index: 0, delta, finish_reason }],
    })}\n\n`);
    if (requests === 1) {
      send({ role: 'assistant', tool_calls: [{ index: 0, id: 'powershell-smoke', type: 'function', function: {
        name: 'PowerShell', arguments: JSON.stringify({ command: "if ($env:SC_API_KEY) { throw 'API key leaked to shell' }; Write-Output 'WINDOWS_SC_EXEC_OK'" }),
      } }] });
      send({}, 'tool_calls');
    } else {
      assert.equal(requests, 2, 'Unexpected extra model request');
      const result = request.messages.find(message => message.role === 'tool' && message.tool_call_id === 'powershell-smoke');
      assert(result, 'Tool answer missing from next request');
      assert.match(JSON.stringify(result.content), /WINDOWS_SC_EXEC_OK/);
      assert.match(JSON.stringify(result.content), /exit: 0/);
      send({ role: 'assistant', content: 'WINDOWS_SC_E2E_OK' });
      send({}, 'stop');
    }
    res.end('data: [DONE]\n\n');
  } catch (error) {
    serverError = error;
    res.writeHead(500).end('mock assertion failed');
  }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const env = { ...process.env, USERPROFILE: profile, SC_API_KEY: 'dummy-windows-smoke-key',
  SC_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`, SC_MODEL: 'windows-smoke',
  SC_DLR_ENABLED: 'false', SC_REQUEST_GZIP: 'false', SC_DANGEROUS: '1', SC_MAX_RETRIES: '0',
  SC_WINDOWS_SHELL: 'powershell',
};
// A Windows user profile must suffice without a Unix HOME variable.
for (const key of Object.keys(env)) if (key.toUpperCase() === 'HOME') delete env[key];
try {
  const result = await new Promise((resolve, reject) => {
    const child = spawn(binary, ['--dangerously-skip-permissions', '-p', 'Exercise the Windows shell.'], {
      cwd: profile, env, stdio: ['ignore', 'pipe', 'pipe'],
    });
    let stdout = '', stderr = '';
    child.stdout.on('data', chunk => { stdout += chunk; });
    child.stderr.on('data', chunk => { stderr += chunk; });
    const timer = setTimeout(() => { child.kill(); reject(new Error('Windows smoke timed out')); }, 60000);
    child.once('error', error => { clearTimeout(timer); reject(error); });
    child.once('close', code => { clearTimeout(timer); resolve({ code, stdout, stderr }); });
  });
  if (serverError) throw serverError;
  assert.equal(result.code, 0, result.stderr);
  assert.match(result.stdout, /WINDOWS_SC_E2E_OK/);
  assert.equal(requests, 2);
  // Headless runs are intentionally ephemeral; global memory verifies profile lookup.
  console.log('WINDOWS_SC_E2E_PASSED: native PowerShell, secret filtering, tool replay, USERPROFILE memory');
} finally {
  server.closeAllConnections();
  await new Promise(resolve => server.close(resolve));
  await fs.rm(profile, { recursive: true, force: true });
}
