import { ClaudeAdapter, tool } from './adapter.mjs';
import { SessionAdapter } from '../session.mjs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { mkdirSync, openSync, closeSync, writeSync, fsyncSync, realpathSync, readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';

// Reuse the harness-neutral session/grant machinery; no MCP or Bash dispatch.
const sessions = new Map();
const stateRoot = join(process.env.CLAUDE_CONFIG_DIR || join(homedir(), '.claude'), 'isohypse-sessions');
function journal(id, workspace) {
  mkdirSync(stateRoot, { recursive: true, mode: 0o700 });
  const directory = join(stateRoot, createHash('sha256').update(id).digest('hex'));
  mkdirSync(directory, { mode: 0o700, recursive: true });
  const binding = join(directory, 'workspace');
  try {
    const fd = openSync(binding, 'wx', 0o600);
    try { writeSync(fd, workspace); fsyncSync(fd); } finally { closeSync(fd); }
  } catch (error) { if (error.code !== 'EEXIST') throw error; }
  if (readFileSync(binding, 'utf8') !== workspace) throw new Error('Resumed Isohypse workspace differs from its original grant');
  return callId => {
    const path = join(directory, createHash('sha256').update(callId).digest('hex'));
    let fd;
    try { fd = openSync(path, 'wx', 0o600); }
    catch (error) {
      if (error.code === 'EEXIST') throw new Error('Duplicate tool call across resume; not replayed. Inspect files for an unconfirmed prior outcome.');
      throw error;
    }
    try { writeSync(fd, 'dispatched; inspect files if no result was received\n'); fsyncSync(fd); }
    finally { closeSync(fd); }
  };
}
let nativeTool;
export function createIsohypseTool(define, object, cwd, sessionId, registerCleanup, hostConfig = {}) {
  const allowEdits = process.env.ISOHYPSE_ALLOW_EDITS === undefined ? hostConfig.allowEdits === true : process.env.ISOHYPSE_ALLOW_EDITS === '1';
  return nativeTool ??= define({
    name: 'isohypse',
    maxResultSizeChars: 100000,
    inputSchema: object({}).passthrough(),
    inputJSONSchema: tool.inputSchema,
    async prompt() { return tool.description + ' After task resume, reread affected files before editing. Resuming opens a fresh authenticated transport; previous calls are never replayed.'; },
    async description() { return tool.description; },
    isEnabled() { return true; },
    isConcurrencySafe() { return false; },
    isReadOnly() { return false; },
    async checkPermissions(input) { return { behavior: 'ask', message: input.steps?.some(s => s.op?.startsWith('mutate.')) ? `Apply Isohypse edits and run validation: ${input.validate || '(missing)'}` : 'Run Isohypse in this workspace' }; },
    userFacingName() { return 'Isohypse'; },
    getToolUseSummary() { return null; },
    renderToolUseMessage(input) { return input.steps?.map(step => step.op).join(', ') ?? 'Isohypse'; },
    async call(input, context, _permission, message) {
      const owner = sessionId?.() ?? context.session?.id;
      if (typeof owner !== 'string' || !owner) throw new Error('Missing Claude session identity');
      const call = message.message.content.find(item => item.type === 'tool_use' && item.name === 'isohypse');
      if (!call?.id || !message.message.id) throw new Error('Missing Claude tool-call identity');
      const workspace = realpathSync(cwd());
      let state = sessions.get(owner);
      if (!state) {
        const claim = journal(owner, workspace);
        const adapter = new ClaudeAdapter({
          workspace: cwd(), executable: process.env.ISOHYPSE_EXECUTABLE || join(homedir(), 'bin/isohypse'),
          allowEdits,
          open: options => SessionAdapter.open({ ...options, autoStart: false }),
        });
        adapter.bind('claude');
        state = { adapter, workspace, claim, read: false };
        sessions.set(owner, state);
        registerCleanup?.(async () => {
          sessions.delete(owner);
          await adapter.close();
        });
      }
      if (state.workspace !== workspace) throw new Error('Workspace changed; original Isohypse grant cannot be rebound');
      const { adapter } = state;
      const mutation = input.steps?.some(step => step.op?.startsWith('mutate.'));
      if (mutation && !allowEdits) throw new Error('Writes require the host launcher --isohypse-allow-edits flag');
      if (mutation && context.getAppState?.().toolPermissionContext?.mode === 'plan') throw new Error('Isohypse mutations are disabled in plan mode');
      if (mutation && input.steps.some(step => step.op === 'mutate.edit') && !state.read) throw new Error('Read affected files in this session before editing, including after resume');
      const signal = context.abortController?.signal;
      const cancel = () => adapter.cancel('claude', message.message.id);
      signal?.throwIfAborted();
      state.claim(call.id);
      signal?.addEventListener('abort', cancel, { once: true });
      let result;
      try {
        result = await adapter.call({ threadId: 'claude', turnId: message.message.id, callId: call.id, tool: 'isohypse', arguments: input });
      } finally {
        signal?.removeEventListener('abort', cancel);
      }
      const text = result.contentItems.map(item => item.text).join('\n');
      if (!result.success) throw new Error(text);
      if (input.steps.some(step => step.op === 'session.reconnect')) state.read = false;
      if (input.steps.some(step => step.op === 'context.read')) state.read = true;
      return { data: text };
    },
    mapToolResultToToolResultBlockParam(data, id) { return { type: 'tool_result', tool_use_id: id, content: data }; },
  });
}
