import { spawn, spawnSync, execFile } from 'node:child_process';
import { realpathSync, readFileSync } from 'node:fs';
import { isAbsolute, join, relative, sep } from 'node:path';
import { homedir } from 'node:os';
import { promisify } from 'node:util';
import { setTimeout as delay } from 'node:timers/promises';

const runFile = promisify(execFile);
const workspaceStarts = new Map();
const STARTUP_WAIT_MS = 120000;
// Sockets live under the isohypse home now, so a harness cannot count them — and
// a socket file was never evidence anyway, since a SIGKILL leaves one behind.
// Ask the binary for the live endpoint and take a non-zero exit as "not running".
export function liveSocket(workspace, executable) {
  const probe = spawnSync(executable, ['state', 'socket', workspace], { encoding: 'utf8', timeout: 15000 });
  if (probe.status === 0) return (probe.stdout ?? '').trim();
  const noise = String(probe.stderr ?? '').trim();
  // A build from before the run-directory layout has no state socket command at
  // all. Say that once, plainly, instead of spending the readiness window on it.
  if (/unknown command|not a recognised|usage:/i.test(noise))
    throw new Error('This Isohypse build cannot answer state socket ('
      + noise + '); point connect at a current binary');
  return '';
}
// Test seam for lifecycle mechanics; production verifies the binary in open first.
export async function ensureWorkspace(workspace, executable) {
  if (liveSocket(workspace, executable)) return;
  let starting = workspaceStarts.get(workspace);
  if (!starting) {
    starting = (async () => {
      // Nothing answers: start it. A daemon another client is already starting is
      // left alone rather than replaced, and a socket file is never treated as
      // evidence that authorization succeeded.
      if (!liveSocket(workspace, executable)) {
        try {
          await runFile(executable, ['state', 'up', workspace], { cwd: workspace, timeout: 30000, maxBuffer: 65536 });
        } catch (error) {
          throw new Error(`Isohypse workspace startup failed; not retried: ${error.stderr || error.message}`);
        }
      }
      const deadline = Date.now() + STARTUP_WAIT_MS;
      while (!liveSocket(workspace, executable)) {
        if (Date.now() >= deadline) {
          const noise = String(spawnSync(executable, ['state', 'socket', workspace], { encoding: 'utf8', timeout: 15000 }).stderr ?? '').trim();
          throw new Error('Isohypse did not answer on the workspace socket within ' + STARTUP_WAIT_MS / 1000 + ' seconds; inspect startup before retrying' + (noise ? ' (' + noise + ')' : ''));
        }
        await delay(100);
      }
    })();
    workspaceStarts.set(workspace, starting);
  }
  try { await starting; } finally {
    if (workspaceStarts.get(workspace) === starting) workspaceStarts.delete(workspace);
  }
}

// Kept just under the daemon's MAX_REQUEST_BYTES (8 MiB) so the daemon's own
// limit is the one that speaks, not a client cap the model cannot see.
const MAX_BATCH_BYTES = 8 * 1024 * 1024 - 65536;
const fields = {
  'context.read': ['arg', 'range', 'outline', 'tag', 'workspace'],
  'context.log': ['arg', 'workspace', 'tag', 'since', 'until', 'max', 'offset'],
  'context.find': ['arg', 'paths', 'langs', 'max', 'literal', 'name'],
  'context.explore': ['arg', 'paths', 'langs', 'max', 'literal', 'compact', 'full'],
  'context.prompt': [],
  'mutate.edit': ['path', 'tag', 'edits', 'checkpoint'],
  'mutate.create': ['path', 'content', 'checkpoint'],
  'mutate.delete': ['path', 'checkpoint'],
};
function object(value, keys) {
  if (!value || typeof value !== 'object' || Array.isArray(value)
      || Object.keys(value).some(key => !keys.includes(key))) {
    throw new Error(`Expected object with only: ${keys.join(', ')}`);
  }
}
function text(value) {
  if (typeof value !== 'string' || value.includes('\0')) throw new Error('Expected text without NUL');
}
function localPath(workspace, path) {
  text(path);
  if (!path || isAbsolute(path) || path.split(/[\\/]/).includes('..')) throw new Error('Use a workspace-relative path without ..');
  const absolute = join(workspace, path);
  let probe = absolute;
  for (;;) {
    try {
      const resolved = realpathSync(probe);
      const rel = relative(workspace, resolved);
      if (rel === '..' || rel.startsWith(`..${sep}`) || isAbsolute(rel)) throw new Error('Path escapes workspace through a symlink');
      return join(resolved, relative(probe, absolute));
    } catch (error) {
      if (error.code !== 'ENOENT') throw error;
      const parent = join(probe, '..');
      if (parent === probe) throw error;
      probe = parent;
    }
  }
}
function anchor(value, insert = false) {
  object(value, insert ? ['before', 'after'] : ['symbol', 'lines', 'block']);
  const keys = Object.keys(value);
  if (keys.length !== 1) throw new Error('Use exactly one anchor');
  const key = keys[0], at = value[key];
  const line = n => Number.isSafeInteger(n) && n > 0;
  if (insert ? !(line(at) || at === '^' || at === '$')
      : key === 'symbol' ? !(typeof at === 'string' && at.length > 0)
      : key === 'block' ? !line(at)
      : !(Array.isArray(at) && at.length === 2 && at.every(line) && at[0] <= at[1])) {
    throw new Error('Invalid edit anchor');
  }
}
const REQUIRED_CODESIGN_IDENTIFIER = 'isohypse';

// Fail closed before any transport spawn: the child must be the certificate-
// signed Isohypse binary with the required identifier. No production bypass
// exists; tests construct the transport directly instead (see the class).
function verifyCodesign(executable) {
  if (process.platform !== 'darwin') {
    throw new Error(`pisohypse requires macOS codesign verification; refusing to start '${executable}' on ${process.platform}`);
  }
  // Pi's cwd can be a directory that no longer exists, and a spawn from a dead
  // cwd fails before codesign runs. Probe from home so the answer is about the
  // binary, and say plainly when the probe itself could not run.
  const options = { encoding: 'utf8', timeout: 10000, maxBuffer: 65536, cwd: homedir() };
  const verify = spawnSync('/usr/bin/codesign', ['--verify', '--strict', '--', executable], options);
  if (verify.error || verify.status === undefined || verify.status === null) {
    throw new Error(`Cannot run codesign for '${executable}': ${verify.error?.message ?? 'no exit status'}; refusing to start.`);
  }
  if (verify.status !== 0) {
    throw new Error(`Isohypse binary failed strict codesign verification (${(verify.stderr || verify.stdout || '').trim() || 'exit ' + verify.status}); refusing to start. Install a certificate-signed isohypse binary.`);
  }
  const display = spawnSync('/usr/bin/codesign', ['--display', '--verbose=2', '--', executable], options);
  if (display.error || display.status === undefined || display.status === null) {
    throw new Error(`Cannot inspect the signing identity of '${executable}': ${display.error?.message ?? 'no exit status'}; refusing to start.`);
  }
  if (display.status !== 0) throw new Error('Cannot inspect Isohypse signing identity; refusing to start');
  const info = `${display.stdout ?? ''}\n${display.stderr ?? ''}`;
  const identifier = info.match(/^Identifier=(.*)$/m)?.[1]?.trim();
  if (identifier !== REQUIRED_CODESIGN_IDENTIFIER) {
    throw new Error(`Isohypse binary identifier is '${identifier ?? 'missing'}', expected '${REQUIRED_CODESIGN_IDENTIFIER}'; refusing to start.`);
  }
  if (/Signature=adhoc|flags=0x[0-9a-f]*\(adhoc\)/.test(info) || !/^Authority=.+$/m.test(info)) {
    throw new Error('Isohypse binary is ad-hoc signed; a certificate-signed build is required; refusing to start.');
  }
}

export async function reloadRuntime({ workspace, executable }) {
  const manifest = readFileSync(join(workspace, 'Cargo.toml'), 'utf8');
  const pkg = manifest.match(/^\[package\]\s*\n([\s\S]*?)(?=^\[|\s*$)/m);
  if (!pkg || !/^name\s*=\s*"isohypse"\s*$/m.test(pkg[1])) {
    throw new Error('Native reload requires the Isohypse source workspace');
  }
  verifyCodesign(executable);
  // Direct native argv, no shell, unsigned helper, or socket-security bypass.
  // The existing build path stages and signs before the supervisor handoff.
  const result = await runFile(executable, ['build', workspace], {
    cwd: workspace, timeout: 600000, maxBuffer: 2 * 1024 * 1024,
  });
  return result.stdout;
}


// Static JSON only: never try to authorize arbitrary jq by inspecting its text.
// Static JSON only: never try to authorize arbitrary jq by inspecting its text.
export function prepareBatch(workspace, input) {
  object(input, ['steps', 'validate']);
  const batch = JSON.parse(JSON.stringify(input));
  if (!Array.isArray(batch.steps) || batch.steps.length < 1 || batch.steps.length > 32) throw new Error('Use 1–32 ordered steps');
  const paths = new Set();
  for (const step of batch.steps) {
    const allowed = Object.hasOwn(fields, step?.op) ? fields[step.op] : undefined;
    if (!allowed) throw new Error(`Operation not granted: ${step?.op}`);
    object(step, ['op', 'label', ...allowed]);
    if (step.label !== undefined) text(step.label);
    for (const key of ['outline', 'literal', 'name', 'compact', 'full', 'checkpoint']) {
      if (step[key] !== undefined && typeof step[key] !== 'boolean') throw new Error(`${key} must be boolean`);
    }
    for (const key of ['paths', 'langs']) {
      if (step[key] !== undefined) {
        if (!Array.isArray(step[key]) || step[key].length > 32) throw new Error(`${key} must be a bounded string array`);
        step[key].forEach(text);
      }
    }
    if (step.max !== undefined && !(Number.isSafeInteger(step.max) && step.max > 0 && step.max <= 200)) throw new Error('max must be 1–200');
    const historical = step.op === 'context.log' || (step.op === 'context.read' && step.tag !== undefined);
    if (step.tag !== undefined && historical && (typeof step.tag !== 'string' || !/^[a-f0-9]{4,64}$/i.test(step.tag))) {
      throw new Error('tag must contain 4–64 hexadecimal characters');
    }
    if (step.workspace !== undefined) {
      text(step.workspace);
      if (!historical || !isAbsolute(step.workspace) || step.workspace.split(/[\\/]/).includes('..')
          || step.workspace.split(/[\\/]/).includes('.') || !step.workspace.split('/').some(Boolean)) {
        throw new Error('workspace must be an absolute saved-history root without . or ..');
      }
    }
    for (const key of ['since', 'until', 'offset']) {
      if (step[key] !== undefined && !(Number.isSafeInteger(step[key]) && step[key] >= 0)) throw new Error(`${key} must be a nonnegative safe integer`);
    }
    if (step.since !== undefined && step.until !== undefined && step.since > step.until) throw new Error('since must not exceed until');
    if (allowed.includes('arg') && (!historical || step.arg !== undefined)) {
      text(step.arg); if (!step.arg.trim()) throw new Error('arg cannot be empty');
    }
    if (historical && step.arg !== undefined && (isAbsolute(step.arg) || step.arg.split(/[\\/]/).some(part => part === '..' || part === '.'))) {
      throw new Error('arg must be a workspace-relative recorded path without . or ..');
    }
    if (step.op === 'context.read') {
      if (!historical) localPath(workspace, step.arg);
      if (step.range !== undefined && (typeof step.range !== 'string' || !/^[1-9]\d*-[1-9]\d*$/.test(step.range)
          || Number(step.range.split('-')[0]) > Number(step.range.split('-')[1]))) throw new Error('range must be start-end');
    }
    if (step.op.startsWith('mutate.')) {
      paths.add(localPath(workspace, step.path));
      if (step.op === 'mutate.create') text(step.content);
      else if (step.op !== 'mutate.delete') {
        if (typeof step.tag !== 'string' || !/^[a-f0-9]{7,64}$/.test(step.tag)) throw new Error('Use a tag from an Isohypse read');
        if (!Array.isArray(step.edits) || !step.edits.length || step.edits.length > 100) throw new Error('Use 1–100 anchored edits');
        for (const edit of step.edits) {
          object(edit, ['replace', 'with', 'insert', 'body', 'delete']);
          const actions = ['replace', 'insert', 'delete'].filter(key => Object.hasOwn(edit, key));
          if (actions.length !== 1) throw new Error('Use exactly one edit action');
          const action = actions[0];
          object(edit, action === 'replace' ? ['replace', 'with'] : action === 'insert' ? ['insert', 'body'] : ['delete']);
          anchor(edit[action], action === 'insert');
          if (action !== 'delete') text(edit[action === 'replace' ? 'with' : 'body']);
        }
      }
    }
  }
  if (paths.size) {
    if (typeof batch.validate !== 'string') throw new Error('Edits require an explicit validation command');
    text(batch.validate);
    if (!batch.validate.trim() || batch.validate.trim() === 'none') throw new Error('Edits require an explicit validation command');
  } else if (batch.validate !== undefined) throw new Error('Read-only batches cannot run validation');
  // The daemon accepts 8 MiB (MAX_REQUEST_BYTES); the old 1 MiB client cap refused
  // legitimate authoring before the daemon ever saw it.
  const batchBytes = Buffer.byteLength(JSON.stringify(batch));
  if (batchBytes > MAX_BATCH_BYTES) throw new Error('Batch is ' + batchBytes + ' bytes, over the ' + MAX_BATCH_BYTES + ' byte limit; split it across several batches');
  return { batch, paths: [...paths].sort() };
}

export class SessionAdapter {
  #child;
  #pending;
  #ready;
  #readyResolve;
  #readyReject;
  #tail = Promise.resolve();
  #id = 0;
  #state = 'opening';
  #close;
  #closeResolve;
  #closeReject;
  #closedSnapshot = false;
  #session;
  #failure;
  #workspace;
  #allowEdits;

  static async open({ workspace, executable, allowEdits = false, autoStart = false }) {
    if (!isAbsolute(workspace) || !isAbsolute(executable)) throw new Error('Workspace and executable must be absolute');
    workspace = realpathSync(workspace);
    executable = realpathSync(executable);
    // Verification precedes all lifecycle side effects. Only host-owned native
    // opening opts into startup; tool arguments and Pi do not grant it.
    verifyCodesign(executable);
    if (autoStart) await ensureWorkspace(workspace, executable);
    else if (!liveSocket(workspace, executable)) throw new Error('No live Isohypse daemon answers for this workspace');
    const adapter = new SessionAdapter(workspace, executable, allowEdits);
    await adapter.#ready;
    return adapter;
  }
  // Test seam: constructs the transport WITHOUT codesign verification.
  // Production must go through SessionAdapter.open, which always verifies.
  constructor(workspace, executable, allowEdits) {
    this.#workspace = workspace;
    this.#allowEdits = allowEdits;
    this.#ready = new Promise((resolve, reject) => { this.#readyResolve = resolve; this.#readyReject = reject; });
    this.#child = spawn(executable, ['session', '--stdio'], { cwd: workspace, stdio: ['pipe', 'pipe', 'pipe'], shell: false });
    let buffer = '', stderr = '';
    const timer = setTimeout(() => this.#fail(new Error('Isohypse session open timed out')), 10000);
    this.#ready.finally(() => clearTimeout(timer)).catch(() => {});
    this.#child.stderr.setEncoding('utf8').on('data', chunk => { stderr = (stderr + chunk).slice(-4096); });
    this.#child.stdout.setEncoding('utf8').on('data', chunk => {
      buffer += chunk;
      if (Buffer.byteLength(buffer) > 16 * 1024 * 1024) return this.#fail(new Error('Isohypse response exceeds 16 MiB; use smaller ranges'));
      let end;
      while ((end = buffer.indexOf('\n')) >= 0) {
        const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
        try { this.#frame(JSON.parse(line)); } catch (error) { this.#fail(error); return; }
      }
    });
    this.#child.on('error', error => this.#fail(error));
    this.#child.stdin.on('error', error => this.#fail(error));
    this.#child.on('close', code => {
      if (this.#closedSnapshot && code === 0) {
        this.#state = 'closed';
        this.#closeResolve?.();
      } else this.#fail(new Error(`Isohypse disconnected (${code}); outcome unconfirmed; no replay. ${stderr}`));
    });
    this.#send({ do: 'session.open', label: 'pisohypse', observer: !allowEdits });
  }
  // Test seam: awaits transport readiness for constructions that skipped
  // codesign verification. Grants no authority by itself.
  get ready() { return this.#ready; }

  get workspace() { return this.#workspace; }
  get session() { return this.#session; }
  #send(frame) { this.#child.stdin.write(`${JSON.stringify(frame)}\n`); }
  #fail(error) {
    if (this.#failure) return;
    error = new Error(`${error.message}; session transport failed; outcome unconfirmed; no replay`);
    this.#failure = error;
    this.#state = 'closed';
    this.#readyReject(error);
    this.#pending?.reject(error);
    this.#closeReject?.(error);
    // This is only our CLI transport child, never a daemon or its process group.
    this.#child.kill();
  }
  #frame(frame) {
    if (frame.ch === 'ready' && this.#state === 'opening') {
      if (typeof frame.session !== 'string') throw new Error('Malformed ready frame');
      this.#session = frame.session; // Deliberately do not expose/persist the resume token.
      this.#send({ do: 'session.command', id: 0, op: 'state.status', payload: {} });
    } else if (this.#state === 'opening' && frame.id === 0) {
      if (frame.ch === 'error') throw new Error(frame.message);
      if (frame.ch === 'result') {
        if (frame.json?.root !== this.#workspace) throw new Error('Authenticated worker workspace mismatch');
        this.#state = 'ready'; this.#readyResolve();
      }
    } else if (frame.ch === 'snapshot' && frame.state?.closed === true) {
      this.#closedSnapshot = true;
    } else if (frame.ch === 'error' && frame.id === undefined) {
      throw new Error(frame.message);
    } else if (this.#pending && frame.id === this.#pending.id && !this.#pending.aborting) {
      if (frame.ch === 'result') this.#pending.resolve(frame);
      if (frame.ch === 'error') this.#pending.reject(new Error(frame.message));
    }
  }
  run(input, { signal, authorize, lock = async (_paths, run) => run() } = {}) {
    // Snapshot before awaiting approval/queueing: the approved batch cannot change underneath us.
    const prepared = prepareBatch(this.#workspace, input);
    if (prepared.paths.length && !this.#allowEdits) return Promise.reject(new Error('Read-only grant rejects mutations'));
    const job = this.#tail.then(async () => {
      signal?.throwIfAborted();
      if (this.#state !== 'ready') throw new Error('Session unavailable; reconnect explicitly through the harness (Codex: session.reconnect as the only step). Nothing was replayed.');
      if (prepared.paths.length && (!authorize || await authorize(JSON.stringify(prepared.batch, null, 2)) !== true)) throw new Error('Edit and validation execution not authorized');
      return lock(prepared.paths, async () => {
        signal?.throwIfAborted();
        if (this.#state !== 'ready') throw new Error('Session closed before dispatch');
        const current = prepareBatch(this.#workspace, prepared.batch);
        if (JSON.stringify(current.paths) !== JSON.stringify(prepared.paths)) throw new Error('Mutation targets changed while awaiting approval/locks');
        const id = ++this.#id;
        let timer;
        const abort = () => {
          if (!this.#pending || this.#pending.aborting) return;
          this.#pending.aborting = true;
          this.#send({ do: 'session.cancel', id });
          // cancelled is only an ACK. Close drains accepted work before we report completion.
          this.close().then(() => this.#pending?.reject(new Error('Cancelled; session drained and closed. Work may already have applied; inspect before retrying.')),
            error => this.#pending?.reject(error));
        };
        try {
          return await new Promise((resolve, reject) => {
            this.#pending = { id, resolve, reject, aborting: false };
            signal?.addEventListener('abort', abort, { once: true });
            timer = setTimeout(abort, 10 * 60 * 1000);
            const [step] = prepared.batch.steps;
            if (prepared.batch.steps.length === 1 && (step.op === 'context.log' || (step.op === 'context.read' && step.tag))) {
              // History workspace is a storage filter, never a session routing grant.
              const { op, ...payload } = step;
              this.#send({ do: 'session.command', id, op, payload });
            } else {
              this.#send({ do: 'session.multiop', id, expr: '$batch | fromjson', bind: { batch: JSON.stringify(prepared.batch) } });
            }
          });
        } finally {
          clearTimeout(timer);
          signal?.removeEventListener('abort', abort);
          this.#pending = undefined;
        }
      });
    });
    this.#tail = job.catch(() => {});
    return job;
  }
  close() {
    if (this.#close) return this.#close;
    if (this.#failure) return Promise.reject(this.#failure);
    if (this.#state === 'closed') return Promise.resolve();
    this.#state = 'closing';
    this.#close = new Promise((resolve, reject) => { this.#closeResolve = resolve; this.#closeReject = reject; });
    // ponytail: 30s drain ceiling; report unconfirmed rather than pretending rollback completed.
    const timer = setTimeout(() => this.#fail(new Error('Session drain timed out; outcome unconfirmed, inspect before reconnecting')), 30000);
    this.#close.finally(() => clearTimeout(timer)).catch(() => {});
    this.#send({ do: 'session.close' });
    return this.#close;
  }
}
