// Claude Code's own Isohypse declaration and session adapter.
//
// Claude declares tools its own way and keeps its own call journal, so this
// harness owns its copy instead of reaching into another harness's directory.
// Claude has no host maintenance grant, which is why Isohypse's rebuild path is
// absent here rather than merely refused.
import { mkdtempSync, writeFileSync, realpathSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { SessionAdapter } from '../session.mjs';

const string = { type: 'string' };
const boolean = { type: 'boolean' };
const strings = { type: 'array', items: string, maxItems: 32 };

export const tool = {
  name: 'isohypse',
  description: 'Read, find, explore, and edit source through one persistent Isohypse session. Call context.prompt first: the daemon refuses every other operation until the session has loaded that grammar. context.log lists saved revisions; context.read with tag retrieves saved source. Mutations require validation and a host write grant. After a disconnect, call session.reconnect alone, then call context.prompt again and inspect affected files; no replay. Prefer bounded ranges.',
  inputSchema: {
    type: 'object', additionalProperties: false, required: ['steps'],
    properties: {
      steps: {
        type: 'array', minItems: 1, maxItems: 32,
        items: {
          type: 'object', additionalProperties: false, required: ['op'],
          properties: {
            op: { enum: ['context.read', 'context.find', 'context.explore', 'context.log', 'context.prompt', 'mutate.edit', 'mutate.create', 'session.reconnect'], type: 'string' },
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
  const file = join(mkdtempSync(join(tmpdir(), 'claude-isohypse-')), 'output.txt');
  writeFileSync(file, text, { mode: 0o600 });
  return `${Buffer.from(lines.slice(0, 2000).join('\n')).subarray(0, 50 * 1024).toString('utf8')}\n[Truncated; full output: ${file}. Use a narrower range.]`;
}

// One instance per Claude session. The signed Isohypse transport stays owned by
// SessionAdapter, which keeps its validation, cancellation, and drain rules.
export class ClaudeAdapter {
  #workspace;
  #executable;
  #allowEdits;
  #open;
  #threads = new Map();
  #closed = false;
  #closing = new Set();
  #close;

  constructor({ workspace, executable, allowEdits = false, open = options => SessionAdapter.open(options) }) {
    if (typeof allowEdits !== 'boolean') throw new Error('allowEdits must be boolean');
    this.#workspace = realpathSync(workspace);
    this.#executable = realpathSync(executable);
    this.#allowEdits = allowEdits;
    this.#open = open;
  }

  get workspace() { return this.#workspace; }

  bind(threadId) {
    if (this.#closed) throw new Error('Adapter closed');
    if (typeof threadId !== 'string' || !threadId) throw new Error('Missing thread ID');
    if (!this.#threads.has(threadId)) {
      this.#threads.set(threadId, { session: undefined, active: new Map(), reconnecting: false });
    }
  }

  async call({ threadId, turnId, callId, arguments: batch }) {
    try {
      if (this.#closed) throw new Error('Tool not owned by this adapter');
      const thread = this.#threads.get(threadId);
      if (!thread) throw new Error('Thread has no Isohypse workspace grant');
      if (typeof callId !== 'string' || !callId || typeof turnId !== 'string' || !turnId) throw new Error('Missing call or turn ID');
      if (thread.reconnecting) throw new Error('Session reconnect in progress; wait for its result');
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
          if (this.#closed || this.#threads.get(threadId) !== thread) throw new Error('Thread grant closed during reconnect');
          if (realpathSync(this.#workspace) !== this.#workspace) throw new Error('Workspace identity changed; reconnect refused');
          thread.session = this.#open({ workspace: this.#workspace, executable: this.#executable, allowEdits: this.#allowEdits });
          await thread.session;
          if (this.#closed || this.#threads.get(threadId) !== thread) throw new Error('Thread grant closed during reconnect');
          return response(true, `Isohypse reconnected to ${this.#workspace}. Nothing was replayed. Call context.prompt again to load the grammar, then read current files before editing.${uncertain ? ' Prior session outcome remains unconfirmed; inspect affected files before retrying any mutation.' : ''}`);
        } finally { thread.reconnecting = false; }
      }
      const controller = new AbortController();
      thread.active.set(callId, { controller, turnId });
      try {
        // Keep a rejected open promise: reconnecting and replaying stay explicit.
        thread.session ??= this.#open({ workspace: this.#workspace, executable: this.#executable, allowEdits: this.#allowEdits });
        const session = await thread.session;
        controller.signal.throwIfAborted();
        const result = await session.run(batch, { signal: controller.signal, authorize: async () => this.#allowEdits });
        return response(true, output(result.output));
      } finally { thread.active.delete(callId); }
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
