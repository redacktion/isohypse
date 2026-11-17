#!/usr/bin/env node
import { spawn } from 'node:child_process';
import { realpathSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';
import { CodexAdapter, tool } from './adapter.mjs';

// A transparent stdio app-server host bridge. Unrelated RPC traffic and tool
// callbacks remain the caller's responsibility, including its approval UI.
export async function serve({ workspace, executable, codex, allowEdits = false,
  input = process.stdin, output = process.stdout, diagnostics = process.stderr,
  serverArgs = ['app-server', '--stdio'], serverEnv = process.env,
  adapter = new CodexAdapter({ workspace, executable, allowEdits }),
}) {
  const child = spawn(codex, serverArgs, { cwd: adapter.workspace, env: serverEnv, stdio: ['pipe', 'pipe', 'pipe'], shell: false });
  const pending = new Map();
  let ending = false;
  const send = message => child.stdin.write(`${JSON.stringify(message)}\n`);
  const emit = message => output.write(`${JSON.stringify(message)}\n`);
  const reject = (id, message) => emit({ id, error: { code: -32602, message } });
  const done = new Promise((resolveDone, rejectDone) => {
    let failure;
    async function shutdown(error) {
      if (ending) return;
      ending = true;
      failure = error;
      input.removeListener('data', onInput);
      try { await adapter.close(); } catch (error) { failure ??= error; }
      child.stdin.end();
      const timer = setTimeout(() => child.kill('SIGKILL'), 5000);
      timer.unref();
      child.once('close', () => clearTimeout(timer));
    }
    function onHost(message) {
      if (ending) return;
      if (message.method === 'initialize') {
        message.params ??= {};
        message.params.capabilities = { ...message.params.capabilities, experimentalApi: true };
      }
      if (message.method === 'thread/start') {
        let cwd;
        try {
          const requested = message.params?.cwd ?? adapter.workspace;
          cwd = requested == null ? undefined : realpathSync(requested);
        }
        catch (error) { reject(message.id, error.message); return; }
        // Any directory the host started the thread in, not only the configured
        // one: a repo the operator opened works with no connect step, and a
        // workspace with no daemon gets one started by the transport.
        if (cwd) {
          const existing = message.params?.dynamicTools ?? [];
          if (!Array.isArray(existing) || existing.some(t => t.name === tool.name)) {
            reject(message.id, 'Isohypse tool name is already registered or dynamicTools is invalid'); return;
          }
          // The server rejects mixed legacy/canonical specs. Preserve the
          // caller's format; a flat legacy function is supported by Codex too.
          const spec = existing.some(t => t.type === undefined) ? (({ type, ...rest }) => rest)(tool) : tool;
          message.params = { ...message.params, ...(cwd ? { cwd } : {}), dynamicTools: [...existing, spec] };
          pending.set(message.id, { bind: true, cwd });
        }
      }
      const threadId = message.params?.threadId;
      if (adapter.has(threadId)) {
        if (message.method === 'turn/start' && message.params.cwd != null) {
          try {
            if (realpathSync(message.params.cwd) !== adapter.workspaceFor(threadId)) throw new Error('Cannot change cwd of an Isohypse-bound thread');
          } catch (error) { reject(message.id, error.message); return; }
        }
        if (message.method === 'turn/interrupt') adapter.cancel(threadId, message.params.turnId);
        if (['thread/archive', 'thread/unsubscribe'].includes(message.method)) {
          pending.set(message.id, { unbind: threadId });
        }
      }
      send(message);
    }
    async function onServer(message) {
      if (message.method === 'item/tool/call' && adapter.owns(message.params)) {
        const result = await adapter.call(message.params);
        if (!child.stdin.destroyed && !child.stdin.writableEnded) send({ id: message.id, result });
        return;
      }
      if (pending.has(message.id) && !message.method) {
        const action = pending.get(message.id);
        pending.delete(message.id);
        if (!message.error) {
          if (action.bind) {
            const actual = realpathSync(message.result.thread.cwd);
            if (action.cwd && actual !== action.cwd) throw new Error('Codex returned a different thread workspace; grant refused');
            adapter.bind(message.result.thread.id, actual);
          }
          else await adapter.unbind(action.unbind);
        }
      }
      emit(message);
    }
    function reader(handle) {
      let buffer = '';
      return chunk => {
        buffer += chunk;
        // Bound an unterminated frame; each parsed frame has the same ceiling.
        let end;
        while ((end = buffer.indexOf('\n')) >= 0) {
          const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
          if (Buffer.byteLength(line) > 16 * 1024 * 1024) { void shutdown(new Error('RPC frame exceeds 16 MiB')); return; }
          if (!line.trim()) continue;
          try { Promise.resolve(handle(JSON.parse(line))).catch(shutdown); }
          catch (error) { void shutdown(error); return; }
        }
        if (Buffer.byteLength(buffer) > 16 * 1024 * 1024) void shutdown(new Error('RPC frame exceeds 16 MiB'));
      };
    }
    const onInput = reader(onHost);
    input.setEncoding('utf8'); child.stdout.setEncoding('utf8');
    input.on('data', onInput);
    child.stdout.on('data', reader(onServer));
    child.stderr.on('data', chunk => diagnostics.write(chunk));
    input.once('end', () => { void shutdown(); });
    input.once('error', shutdown); child.stdin.on('error', shutdown);
    child.once('error', shutdown);
    child.once('close', async code => {
      input.removeListener('data', onInput);
      input.pause();
      try { await adapter.close(); } catch (error) { failure ??= error; }
      if (failure || code !== 0) rejectDone(failure ?? new Error(`Codex app-server exited ${code}`));
      else resolveDone();
    });
    // Signal handlers are removed when this connection completes.
    const stop = () => { void shutdown(); };
    process.once('SIGINT', stop); process.once('SIGTERM', stop);
    child.once('close', () => {
      process.removeListener('SIGINT', stop); process.removeListener('SIGTERM', stop);
    });
  });
  return done;
}

if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  const { values } = parseArgs({ options: {
    workspace: { type: 'string' }, binary: { type: 'string' }, codex: { type: 'string' },
    'allow-edits': { type: 'boolean', default: false }, help: { type: 'boolean' },
  } });
  if (values.help) {
    console.log('node harness/chatgpt/native/proxy.mjs --workspace /absolute/repo --binary /absolute/isohypse --codex /absolute/codex [--allow-edits]\nStdio app-server bridge. Read-only by default. --allow-edits grants edits and validation shell execution with daemon OS privileges for this connection. Configure the host to launch this command instead of codex app-server.');
  } else {
    try {
      if (values.workspace !== undefined && !values.workspace.startsWith('/')) throw new Error('--workspace requires an absolute path');
      for (const key of ['binary', 'codex']) {
        if (!values[key]?.startsWith('/')) throw new Error(`--${key} requires an absolute path`);
      }
      await serve({ workspace: values.workspace, executable: values.binary, codex: values.codex, allowEdits: values['allow-edits'] });
    } catch (error) { console.error(error.message); process.exitCode = 1; }
  }
}
