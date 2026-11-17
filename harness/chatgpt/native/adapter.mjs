import { realpathSync, statSync, mkdtempSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { SessionAdapter, reloadRuntime } from '../../session.mjs';

const string = { type: 'string' };
const boolean = { type: 'boolean' };
const strings = { type: 'array', items: string, maxItems: 32 };

export const tool = {
  type: 'function', name: 'isohypse', deferLoading: false,
  description: 'Read, find, explore, and edit source through one persistent Isohypse session. Call context.prompt first: the daemon refuses every other operation until the session has loaded that grammar. context.log lists saved revisions; context.read with tag retrieves saved source. Mutations require validation and a host write grant. After a disconnect or reload, call session.reconnect alone, then call context.prompt again and inspect affected files; no replay. Prefer bounded ranges.',
  inputSchema: {
    type: 'object', additionalProperties: false, required: ['steps'],
    properties: {
      steps: {
        type: 'array', minItems: 1, maxItems: 32,
        items: {
          type: 'object', additionalProperties: false, required: ['op'],
          properties: {
            op: { enum: ['context.read', 'context.find', 'context.explore', 'context.log', 'context.prompt', 'mutate.edit', 'mutate.create', 'mutate.delete', 'session.reconnect', 'session.reload'], type: 'string' },
            label: string, arg: string, range: string, outline: boolean,
            paths: strings, langs: strings, max: { type: 'integer', minimum: 1, maximum: 200 },
            literal: boolean, name: boolean, compact: boolean, full: boolean, checkpoint: boolean,
            path: string, tag: string, content: string,
            workspace: { type: 'string', description: 'Absolute recorded workspace; saved history only. Does not change the live workspace or write grant.' },
            since: { type: 'integer', minimum: 0, description: 'Inclusive saved-object mtime in Unix seconds; not the original edit time.' },
            until: { type: 'integer', minimum: 0 },
            offset: { type: 'integer', minimum: 0 },
            edits: { type: 'array', minItems: 1, maxItems: 100, items: {
              type: 'object', additionalProperties: false, properties: {
                replace: { type: 'object', properties: {
                  lines: { type: 'array', items: { type: 'integer', minimum: 1 }, minItems: 2, maxItems: 2 },
                  symbol: string, block: { type: 'integer', minimum: 1 },
                }, additionalProperties: false },
                with: string, body: string,
                insert: { type: 'object', properties: {
                  before: { anyOf: [{ type: 'integer', minimum: 1 }, { enum: ['^', '$'], type: 'string' }] },
                  after: { anyOf: [{ type: 'integer', minimum: 1 }, { enum: ['^', '$'], type: 'string' }] },
                }, additionalProperties: false },
                delete: { type: 'object', properties: {
                  lines: { type: 'array', items: { type: 'integer', minimum: 1 }, minItems: 2, maxItems: 2 },
                  symbol: string, block: { type: 'integer', minimum: 1 },
                }, additionalProperties: false },
              },
            } },
          },
        },
      },
      validate: { type: 'string', description: 'Required for mutations. Shell command executed with daemon OS privileges; the workspace is not a shell sandbox.' },
    },
  },
};

const response = (success, text) => ({ success, contentItems: [{ type: 'inputText', text }] });
function output(text) {
  if (typeof text !== 'string') throw new Error('Malformed Isohypse result: missing output');
  const lines = text.split('\n');
  if (Buffer.byteLength(text) <= 50 * 1024 && lines.length <= 2000) return text;
  const file = join(mkdtempSync(join(tmpdir(), 'codex-isohypse-')), 'output.txt');
  writeFileSync(file, text, { mode: 0o600 });
  return `${Buffer.from(lines.slice(0, 2000).join('\n')).subarray(0, 50 * 1024).toString('utf8')}\n[Truncated; full output: ${file}. Use a narrower range.]`;
}

// Host-owned authority: neither tool arguments nor model output can grant writes.
// Construct one instance per app-server connection. No MCP transport is involved.
export class CodexAdapter {
  #threads = new Map();
  #closed = false;
  #closing = new Set();
  #close;
  constructor({ workspace, executable, allowEdits = false, mutationReports = false, open = options => SessionAdapter.open(options), authorizeReload = () => false, reload = reloadRuntime }) {
    if (typeof allowEdits !== 'boolean') throw new Error('allowEdits must be boolean');
    this.workspace = workspace == null ? undefined : realpathSync(workspace);
    this.executable = realpathSync(executable);
    this.allowEdits = allowEdits;
    this.mutationReports = mutationReports;
    this.open = open; // Dependency injection for transport tests only.
    this.authorizeReload = authorizeReload;
    this.reload = reload;
  }
  bind(threadId, workspace = this.workspace) {
    if (this.#closed) throw new Error('Adapter closed');
    if (typeof threadId !== 'string' || !threadId) throw new Error('Missing thread ID');
    // The workspace is the thread's own cwd, so any repo the operator opens a
    // thread in works with no connect step and no approval. The configured
    // workspace is only the default when the host names none.
    if (workspace == null) throw new Error('Thread has no workspace; start it through the adapter');
    workspace = realpathSync(workspace);
    if (!statSync(workspace).isDirectory()) throw new Error('Isohypse workspace must be a directory: ' + workspace);
    if (this.#threads.has(threadId)) {
      if (this.workspaceFor(threadId) !== workspace) throw new Error('Cannot rebind a thread to another workspace');
      return;
    }
    if (this.#threads.size >= 16) throw new Error('At most 16 Isohypse threads per connection');
    this.#threads.set(threadId, { workspace, seen: new Set(), active: new Map(), session: undefined });
  }
  has(threadId) { return this.#threads.has(threadId); }
  workspaceFor(threadId) { return this.#threads.get(threadId)?.workspace; }
  owns(params) { return params?.tool === tool.name && params.namespace == null; }
  async call(params) {
    try {
      if (this.#closed || !this.owns(params)) throw new Error('Tool not owned by this adapter');
      const thread = this.#threads.get(params.threadId);
      if (!thread) throw new Error('Thread has no Isohypse workspace grant; start a new thread through the adapter');
      if (typeof params.callId !== 'string' || !params.callId || typeof params.turnId !== 'string' || !params.turnId) throw new Error('Missing call or turn ID');
      if (thread.seen.has(params.callId)) throw new Error('Duplicate tool call; not replayed');
      if (thread.seen.size >= 4096) throw new Error('Session call limit reached; start a new thread');
      thread.seen.add(params.callId);
      const batch = structuredClone(params.arguments);
      if (thread.reconnecting) throw new Error('Session reconnect in progress; wait for its result');
      if (batch?.steps?.some(step => step.op === 'session.reload')) {
        if (Object.keys(batch).length !== 1 || batch.steps.length !== 1 || Object.keys(batch.steps[0]).length !== 1) {
          throw new Error('session.reload must be the only step, with no other fields or validation');
        }
        if (!this.allowEdits || !await this.authorizeReload()) throw new Error('Native reload requires the host maintenance grant and write grant');
        if (thread.reconnecting || thread.active.size) throw new Error('Cannot reload while a call or reconnect is active');
        thread.reconnecting = true;
        try {
          if (thread.session) await (await thread.session).close();
          const result = await this.reload({ workspace: thread.workspace, executable: this.executable });
          return response(true, result + '\nSend session.reconnect alone before reading or editing; no calls were replayed.');
        } finally { thread.reconnecting = false; }
      }
      if (batch?.steps?.some(step => step.op === 'session.reconnect')) {
        if (Object.keys(batch).length !== 1 || batch.steps.length !== 1 || Object.keys(batch.steps[0]).length !== 1) {
          throw new Error('session.reconnect must be the only step, with no other fields or validation');
        }
        if (thread.active.size) throw new Error('Cannot reconnect while a call is active; wait for its outcome');
        thread.reconnecting = true;
        try {
          let uncertain = false;
          if (thread.session) {
            try { await (await thread.session).close(); } catch { uncertain = true; }
          }
          if (this.#closed || this.#threads.get(params.threadId) !== thread) throw new Error('Thread grant closed during reconnect');
          if (realpathSync(thread.workspace) !== thread.workspace) throw new Error('Workspace identity changed; reconnect refused');
          // Keep failed opens cached. Only another explicit reconnect may replace one.
          thread.session = this.open({ workspace: thread.workspace, executable: this.executable, allowEdits: this.allowEdits });
          await thread.session;
          if (this.#closed || this.#threads.get(params.threadId) !== thread) throw new Error('Thread grant closed during reconnect');
          return response(true, `Isohypse reconnected to ${thread.workspace}. Nothing was replayed. Read current files before editing.${uncertain ? ' Prior session outcome remains unconfirmed; inspect affected files before retrying any mutation.' : ''}`);
        } finally { thread.reconnecting = false; }
      }
      const controller = new AbortController();
      thread.active.set(params.callId, { controller, turnId: params.turnId });
      try {
        // Keep a rejected open promise: reconnecting/replaying must be explicit.
        thread.session ??= this.open({ workspace: thread.workspace, executable: this.executable, allowEdits: this.allowEdits });
        const session = await thread.session;
        controller.signal.throwIfAborted();
        const result = await session.run(batch, {
          signal: controller.signal, authorize: async () => this.allowEdits,
        });
        return { ...response(true, output(result.output)), ...(this.mutationReports ? { mutations: result.json?.mutate ?? [] } : {}) };
      } finally { thread.active.delete(params.callId); }
    } catch (error) { return response(false, error.message); }
  }
  cancel(threadId, turnId) {
    const thread = this.#threads.get(threadId);
    for (const { controller, turnId: activeTurn } of thread?.active.values() ?? []) {
      if (turnId === undefined || activeTurn === turnId) controller.abort();
    }
  }
  unbind(threadId) {
    const thread = this.#threads.get(threadId);
    if (!thread) return Promise.resolve();
    this.cancel(threadId);
    this.#threads.delete(threadId);
    const closing = (async () => {
      // Failed opens never yielded a transport owned by this adapter.
      if (thread.session) await (await thread.session.catch(() => undefined))?.close();
    })();
    this.#closing.add(closing);
    closing.finally(() => this.#closing.delete(closing)).catch(() => {});
    return closing;
  }
  close() {
    if (this.#close) return this.#close;
    this.#closed = true;
    const pending = [...this.#closing, ...[...this.#threads.keys()].map(id => this.unbind(id))];
    this.#close = Promise.allSettled(pending).then(results => {
      const failures = results.filter(r => r.status === 'rejected').map(r => r.reason);
      if (failures.length) throw new AggregateError(failures, 'Isohypse close failed; outcome unconfirmed; inspect before retrying');
    });
    return this.#close;
  }
}
