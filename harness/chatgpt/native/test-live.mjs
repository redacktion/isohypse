// Real Codex app-server -> native dynamic tool -> signed live Isohypse ->
// Codex function_call_output. Only the model is a deterministic localhost fixture.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createServer as createSocketServer } from 'node:net';
import { mkdtempSync, writeFileSync, rmSync, existsSync, readFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import { join, resolve } from 'node:path';
import { tmpdir } from 'node:os';
import { PassThrough } from 'node:stream';
import { spawn, execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { serve } from './proxy.mjs';
import { installFiles } from './install.mjs';
import { tool } from './adapter.mjs';

const [workspace, executable, codex] = process.argv.slice(2).map(path => resolve(path));
if (!workspace || !executable || !codex) throw new Error('Usage: node harness/chatgpt/native/test-live.mjs /workspace /signed/isohypse /codex');
const fixtureHome = mkdtempSync(join(tmpdir(), 'isohypse-codex-live-'));
const requests = [];
const deniedPath = `harness/codex-denied-${randomUUID()}.txt`;
const writeEnabled = process.argv.includes('--write-enabled');
const allWorkspaces = process.argv.includes('--all-workspaces');
const native = process.argv.includes('--native');
const nativeWs = process.argv.includes('--native-ws');
const interruptNative = process.argv.includes('--interrupt-native');
const reconnectNative = process.argv.includes('--reconnect-native');
const resumeNative = process.argv.includes('--resume-native') || interruptNative || reconnectNative;
// Sockets live under the isohypse home, so "did this workspace start" is a
// liveness question, answered by the binary rather than by looking for a file.
const workspaceAnswers = dir => {
  try { execFileSync(executable, ['state', 'socket', dir], { timeout: 15000, stdio: 'ignore' }); return true; }
  catch { return false; }
};
const server = createServer(async (req, res) => {
  try {
    let body = '';
    for await (const chunk of req) body += chunk;
    requests.push(JSON.parse(body));
    const id = `response-${requests.length}`;
    if (interruptNative && requests.length === 4) {
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write(`event: response.created\ndata: ${JSON.stringify({ type: 'response.created', response: { id } })}\n\n`);
      return; // Hold model generation after all native calls have completed.
    }
    const item = requests.length === 1 ? {
      type: 'function_call', call_id: 'isohypse-live-read', name: 'isohypse',
      // A session refuses every operation until it has loaded the grammar, so the
      // first batch carries context.prompt ahead of the read.
      arguments: JSON.stringify({ steps: [{ op: 'context.prompt' }, { op: 'context.read', arg: 'harness/pi/manifest.json' }] }),
    } : requests.length === 2 ? {
      type: 'function_call', call_id: 'isohypse-denied-write', name: 'isohypse',
      arguments: JSON.stringify({ steps: [{ op: 'mutate.create', path: deniedPath, content: 'native write grant fixture\n' }], validate: `test -f '${deniedPath}'` }),
    } : requests.length === 3 ? {
      type: 'function_call', call_id: 'other-host-tool', name: 'host_echo', arguments: '{}',
    } : reconnectNative && requests.length === 5 ? {
      type: 'function_call', call_id: 'resumed-native-reconnect', name: 'isohypse',
      arguments: JSON.stringify({ steps: [{ op: 'session.reconnect' }] }),
    } : reconnectNative && requests.length === 6 ? {
      type: 'function_call', call_id: 'resumed-native-read', name: 'isohypse',
      arguments: JSON.stringify({ steps: [{ op: 'context.prompt' }, { op: 'context.read', arg: 'harness/pi/manifest.json' }] }),
    } : !reconnectNative && (requests.length === 5 || (resumeNative && requests.length === 7)) ? {
      type: 'function_call', call_id: resumeNative && requests.length === 5 ? 'resumed-native-read' : 'second-workspace-read', name: 'isohypse',
      arguments: JSON.stringify({ steps: [{ op: 'context.prompt' }, { op: 'context.read', arg: 'harness/pi/manifest.json' }] }),
    } : { type: 'message', role: 'assistant', id: 'finished', content: [{ type: 'output_text', text: 'Native Isohypse call completed.' }] };
    res.writeHead(200, { 'content-type': 'text/event-stream' });
    for (const event of [
      { type: 'response.created', response: { id } },
      { type: 'response.output_item.done', item },
      { type: 'response.completed', response: { id, usage: { input_tokens: 0, output_tokens: 0, total_tokens: 0 } } },
    ]) res.write(`event: ${event.type}\ndata: ${JSON.stringify(event)}\n\n`);
    res.end();
  } catch (error) { res.writeHead(500); res.end(error.message); }
});
await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
writeFileSync(join(fixtureHome, 'config.toml'), `
model = "mock-model"
model_provider = "fixture"
approval_policy = "never"
sandbox_mode = "read-only"
[model_providers.fixture]
name = "Local deterministic tool fixture"
base_url = "http://127.0.0.1:${server.address().port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
`);
const input = new PassThrough(), output = new PassThrough(), diagnostics = new PassThrough();
let stderr = '', buffer = '', nextId = 0;
const waiters = new Map(), events = [];
let ownedThread;
diagnostics.on('data', chunk => { stderr = (stderr + chunk).slice(-12000); });
output.setEncoding('utf8').on('data', chunk => {
  buffer += chunk;
  let end;
  while ((end = buffer.indexOf('\n')) >= 0) {
    const message = JSON.parse(buffer.slice(0, end)); buffer = buffer.slice(end + 1);
    if (message.method === 'item/tool/call' && message.params.tool === 'host_echo') {
      input.write(`${JSON.stringify({ id: message.id, result: { success: true, contentItems: [{ type: 'inputText', text: 'host passthrough verified' }] } })}\n`);
    }
    if (message.method) events.push(message);
    else if (waiters.has(message.id)) {
      const { resolve, reject, timer } = waiters.get(message.id);
      clearTimeout(timer); waiters.delete(message.id);
      if (message.error) reject(new Error(JSON.stringify(message.error)));
      else resolve(message.result);
    }
  }
});
const rpc = (method, params) => new Promise((resolve, reject) => {
  const id = ++nextId;
  const timer = setTimeout(() => { waiters.delete(id); reject(new Error(`${method} timed out\n${stderr}`)); }, 20000);
  waiters.set(id, { resolve, reject, timer });
  input.write(`${JSON.stringify({ id, method, params })}\n`);
});
const serverEnv = { ...process.env, CODEX_HOME: fixtureHome };
if (native) {
  const adapterConfig = join(fixtureHome, 'adapter.json');
  const schema = join(fixtureHome, 'schema.json');
  writeFileSync(adapterConfig, JSON.stringify({ executable, allowEdits: writeEnabled }));
  writeFileSync(schema, JSON.stringify(tool));
  serverEnv.ISOHYPSE_NATIVE_CONFIG = join(fixtureHome, 'native.json');
  writeFileSync(serverEnv.ISOHYPSE_NATIVE_CONFIG, JSON.stringify({ node: process.execPath,
    worker: fileURLToPath(new URL('./native-worker.mjs', import.meta.url)), adapterConfig, schema }));
}
const installedPathIndex = process.argv.indexOf('--installed-path');
const installedPath = installedPathIndex >= 0 ? process.argv[installedPathIndex + 1] : undefined;
const installed = process.argv.includes('--installed') || installedPath !== undefined;
const running = native ? (() => {
  // No proxy: simulate the remote client connecting directly to app-server.
  // The request below declares host_echo only. Isohypse must come from the server.
  return (async () => {
    let port;
    if (nativeWs) {
      const reserve = createSocketServer();
      await new Promise(resolve => reserve.listen(0, '127.0.0.1', resolve));
      port = reserve.address().port;
      await new Promise(resolve => reserve.close(resolve));
    }
    if (installedPath) for (const key of Object.keys(serverEnv)) if (key.startsWith('ISOHYPSE_')) delete serverEnv[key];
    const child = spawn(installedPath ?? codex, ['app-server', ...(nativeWs ? ['--listen', `ws://127.0.0.1:${port}`] : ['--stdio'])], { cwd: workspace, env: serverEnv, stdio: ['pipe', 'pipe', 'pipe'] });
    child.stderr.pipe(diagnostics);
    let stopping = false;
    const done = new Promise((resolve, reject) => {
      child.once('error', reject);
      child.once('close', code => code === 0 || stopping ? resolve() : reject(new Error(`Native server exited ${code}: ${stderr}`)));
    });
    if (nativeWs) {
      let socket;
      for (let tries = 0; tries < 100 && !socket; tries++) {
        socket = await new Promise(resolve => {
          const connection = new WebSocket(`ws://127.0.0.1:${port}`);
          connection.addEventListener('open', () => resolve(connection), { once: true });
          connection.addEventListener('error', () => resolve(null), { once: true });
        });
        if (!socket) await new Promise(resolve => setTimeout(resolve, 100));
      }
      if (!socket) { child.kill('SIGTERM'); throw new Error(`WebSocket server unavailable: ${stderr}`); }
      socket.addEventListener('message', event => output.write(event.data + '\n'));
      input.setEncoding('utf8');
      let tail = '';
      input.on('data', chunk => { tail += chunk; let end; while ((end = tail.indexOf('\n')) >= 0) { socket.send(tail.slice(0, end)); tail = tail.slice(end + 1); } });
      input.on('end', () => { stopping = true; socket.close(); child.kill('SIGTERM'); });
    } else { input.pipe(child.stdin); child.stdout.pipe(output); }
    return done;
  })();
})() : process.argv.includes('--wrapper') || installed ? (() => {
  const wrapper = installedPath ?? (installed ? installFiles({ root: join(fixtureHome, 'adapter'), plist: join(fixtureHome, 'agent.plist'),
    config: { ...(allWorkspaces ? {} : { workspace }), executable, codex, allowEdits: false } }) : fileURLToPath(new URL('./codex', import.meta.url)));
  const env = { ...serverEnv, ISOHYPSE_NODE: process.execPath, ISOHYPSE_WORKSPACE: workspace,
    ISOHYPSE_BINARY: executable, ISOHYPSE_CODEX_REAL: codex, ISOHYPSE_ALLOW_EDITS: '0' };
  if (installed) for (const key of Object.keys(env)) if (key.startsWith('ISOHYPSE_')) delete env[key];
  if (allWorkspaces) delete env.ISOHYPSE_WORKSPACE;
  const child = spawn(wrapper, ['-c', 'features.code_mode_host=true', 'app-server'], {
    cwd: workspace, env,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  input.pipe(child.stdin); child.stdout.pipe(output); child.stderr.pipe(diagnostics);
  return new Promise((resolve, reject) => {
    child.once('error', reject);
    child.once('close', code => code === 0 ? resolve() : reject(new Error(`Wrapper exited ${code}: ${stderr}`)));
  });
})() : serve({ workspace: allWorkspaces ? undefined : workspace, executable, codex, input, output, diagnostics, serverEnv });
running.catch(() => {});
try {
  await rpc('initialize', { clientInfo: { name: native ? 'codex_chatgpt_ios_remote' : 'isohypse_native_test', version: '1.0.0' }, capabilities: { experimentalApi: true } });
  input.write(`${JSON.stringify({ method: 'initialized' })}\n`);
  const { thread } = await rpc('thread/start', { cwd: workspace, ephemeral: !resumeNative,
    dynamicTools: [{ type: 'function', name: 'host_echo', description: 'Test host passthrough', inputSchema: { type: 'object', properties: {}, additionalProperties: false } }],
  });
  ownedThread = thread.id;
  if (!native) await assert.rejects(rpc('turn/start', { threadId: thread.id, cwd: tmpdir(), input: [] }), /Cannot change cwd/);
  const { turn: firstTurn } = await rpc('turn/start', { threadId: thread.id, input: [{ type: 'text', text: 'Run the Isohypse read.', text_elements: [] }] });
  if (interruptNative) {
    const deadline = Date.now() + 75000;
    while (requests.length < 4 && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 50));
    assert.equal(requests.length, 4, 'native calls finished before interrupt');
    await rpc('turn/interrupt', { threadId: thread.id, turnId: firstTurn.id });
  }
  const deadline = Date.now() + 75000;
  while (!events.some(e => e.method === 'turn/completed') && Date.now() < deadline) {
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert.ok(events.some(e => e.method === 'turn/completed'), `Turn did not complete: ${JSON.stringify(events.slice(-4))}\n${stderr}`);
  assert.ok(requests[0]?.tools.some(t => t.name === 'isohypse'), 'native function registered in model request');
  const result = requests.flatMap(r => r.input ?? []).find(i => i.type === 'function_call_output' && i.call_id === 'isohypse-live-read');
  assert.ok(result, `Missing native tool output: ${JSON.stringify(events.slice(-5))}`);
  assert.match(JSON.stringify(result.output), /harness\/pi\/manifest.json#[a-f0-9]+/);
  assert.match(JSON.stringify(result.output), /pisohypse/);
  const denied = requests.flatMap(r => r.input ?? []).find(i => i.type === 'function_call_output' && i.call_id === 'isohypse-denied-write');
  if (writeEnabled) {
    assert.ok(denied, 'native write result must reach Codex');
    assert.equal(readFileSync(join(workspace, deniedPath), 'utf8'), 'native write grant fixture\n');
  } else {
    assert.match(JSON.stringify(denied?.output), /Read-only grant rejects mutations/);
    assert.equal(existsSync(join(workspace, deniedPath)), false, 'read-only native call must not write');
  }
  if (process.argv.includes('--file-changes')) {
    const changes = events.filter(e => e.method === 'item/completed' && e.params.item.type === 'fileChange');
    assert.equal(changes.length, writeEnabled ? 1 : 0, 'only confirmed writes produce file-change cards');
    if (writeEnabled) {
      assert.equal(changes[0].params.item.status, 'completed');
      assert.equal(changes[0].params.item.changes[0].path, join(workspace, deniedPath));
      assert.match(changes[0].params.item.changes[0].diff, /native write grant fixture/);
      assert.ok(events.some(e => e.method === 'item/started' && e.params.item.id === changes[0].params.item.id));
    }
    console.log('LIVE PASS: canonical file-change notifications match confirmed mutations.');
  }
  assert.equal(events.filter(e => e.method === 'item/tool/call' && e.params.tool === 'isohypse').length, 0, 'adapter consumes its own callback');
  assert.equal(events.filter(e => e.method === 'item/tool/call' && e.params.tool === 'host_echo').length, 1, 'other native callbacks pass through');
  assert.ok(requests.flatMap(r => r.input ?? []).some(i => i.type === 'function_call_output' && i.call_id === 'other-host-tool' && JSON.stringify(i.output).includes('host passthrough verified')));
  console.log('LIVE PASS: Codex registered isohypse, dispatched a native tool call, and received a tagged read from the signed live daemon. Local model fixture; no external inference.');
  if (writeEnabled) console.log('LIVE PASS: installed write grant created the fixture through the native tool and ran validation.');
  if (resumeNative) {
    if (!interruptNative && !reconnectNative) {
      await rpc('thread/unsubscribe', { threadId: thread.id });
      const resumed = await rpc('thread/resume', { threadId: thread.id });
      if (writeEnabled && process.argv.includes('--file-changes')) {
        assert.ok(resumed.thread.turns.flatMap(turn => turn.items).some(item =>
          item.type === 'fileChange' && item.changes.some(change => change.path === join(workspace, deniedPath))),
          'file-change card survives thread reload');
        console.log('LIVE PASS: file-change card retained in resumed history.');
      }
    }
    const { turn } = await rpc('turn/start', { threadId: thread.id, input: [{ type: 'text', text: 'Read again after resume.', text_elements: [] }] });
    const deadline = Date.now() + 10000;
    while (!events.some(e => e.method === 'turn/completed' && e.params.turn.id === turn.id) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 50));
    const result = requests.flatMap(r => r.input ?? []).find(i => i.type === 'function_call_output' && i.call_id === 'resumed-native-read');
    assert.match(JSON.stringify(result?.output), /manifest.json#[a-f0-9]+/);
    if (reconnectNative) {
      const reconnected = requests.flatMap(r => r.input ?? []).find(i => i.type === 'function_call_output' && i.call_id === 'resumed-native-reconnect');
      assert.match(JSON.stringify(reconnected?.output), /Isohypse reconnected/);
      console.log('LIVE PASS: explicit native reconnect and fresh tagged read in the same loaded thread.');
    }
    if (!reconnectNative) console.log(interruptNative ? 'LIVE PASS: native read succeeds in the same thread after a turn interrupt, without reopening the thread.' : 'LIVE PASS: server-owned Isohypse works after thread resume without a new client tool declaration.');
  }
  if (allWorkspaces) {
    const { thread: second } = await rpc('thread/start', { cwd: fixtureHome, ephemeral: true });
    await rpc('turn/start', { threadId: second.id, input: [{ type: 'text', text: 'Read using this workspace.', text_elements: [] }] });
    const deadline = Date.now() + 10000;
    while (!events.some(e => e.method === 'turn/completed' && e.params.threadId === second.id) && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 50));
    assert.ok(requests[resumeNative ? 6 : 4]?.tools.some(t => t.name === 'isohypse'), 'second project also receives native tool');
    const result = requests.flatMap(r => r.input ?? []).find(i => i.type === 'function_call_output' && i.call_id === 'second-workspace-read');
    assert.match(JSON.stringify(result?.output), native ? /not found|No such file|does not exist|ENOENT/i : /ENOENT|not running/);
    if (!native) assert.ok(JSON.stringify(result.output).includes(fixtureHome), 'missing daemon error names second workspace, never the first');
    else assert.ok(workspaceAnswers(fixtureHome), 'native opening started second workspace');
    assert.doesNotMatch(JSON.stringify(result.output), /manifest.json#[a-f0-9]+/);
    console.log('LIVE PASS: a second project receives isohypse and targets its own workspace; missing file does not fall back to the first project.');
  }
  await rpc('thread/unsubscribe', { threadId: thread.id });
} finally {
  if (ownedThread) await rpc('thread/unsubscribe', { threadId: ownedThread }).catch(() => {});
  input.end();
  try { await running; }
  finally {
    for (const { timer } of waiters.values()) clearTimeout(timer);
    await new Promise(resolve => server.close(resolve));
    if (native && allWorkspaces && workspaceAnswers(fixtureHome)) execFileSync(executable, ['state', 'down', fixtureHome], { timeout: 15000 });
    if (writeEnabled && existsSync(join(workspace, deniedPath)) && readFileSync(join(workspace, deniedPath), 'utf8') === 'native write grant fixture\n') rmSync(join(workspace, deniedPath));
    rmSync(fixtureHome, { recursive: true, force: true, maxRetries: 5, retryDelay: 100 });
  }
}
