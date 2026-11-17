import { existsSync, realpathSync, mkdtempSync, statSync, writeFileSync } from 'node:fs';
import { homedir, tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { join, resolve } from 'node:path';
import { Type } from 'typebox';
import { StringEnum } from '@earendil-works/pi-ai';
import { type ExtensionAPI, truncateHead, withFileMutationQueue } from '@earendil-works/pi-coding-agent';
// The transport lives in session.mjs, which changes with the daemon's on-disk
// layout. A Pi process caches every module it imports for its whole lifetime, so
// reach it through its modification time: a /reload then loads the current file
// instead of the copy this process cached before the layout changed.
// Load heartbeat. A module that throws while loading registers nothing, which is
// indistinguishable from "the commands are gone". Every stage leaves a line, so the
// answer is readable instead of guessed.
const { appendFileSync } = await import('node:fs');
const LOAD_LOG = (): string => join(process.env.HOME || homedir(), '.pi', 'pisohypse-load.log');
const beat = (stage: string) => { try { appendFileSync(LOAD_LOG(), new Date().toISOString() + ' ' + stage + String.fromCharCode(10)); } catch {} };
beat('module start');
let SessionAdapter: any;
try {
  const sessionUrl = new URL('../session.mjs', import.meta.url);
  sessionUrl.searchParams.set('v', String(statSync(fileURLToPath(sessionUrl)).mtimeMs));
  ({ SessionAdapter } = await import(sessionUrl.href));
  beat('transport imported through its mtime');
} catch (error) {
  beat('mtime import failed: ' + String(error));
  ({ SessionAdapter } = await import('../session.mjs'));
  beat('transport imported plainly');
}

// A grant survives a reload: same process, same session, so re-authorizing is
// operator work that buys nothing. Sealed at rest, cleared by /pisohypse disconnect.
const { readFileSync } = await import('node:fs');   // writeFileSync is already imported above
const { createCipheriv, createDecipheriv, randomBytes } = await import('node:crypto');
// os.homedir() is fixed at process start, so a runtime HOME override (how the
// harness tests sandbox themselves) is only visible through process.env.
const piHome = (): string => process.env.HOME || homedir();
const GRANT_KEY = (): string => join(piHome(), '.pi', 'pisohypse.key');
const GRANT_FILE = (): string => join(piHome(), '.pi', 'pisohypse-grants.json');
const grantKey = (): Buffer | null => {
  try {
    const existing = readFileSync(GRANT_KEY(), 'utf8').trim();
    if (existing) return Buffer.from(existing, 'base64');
  } catch {}
  try {
    const fresh = randomBytes(32).toString('base64');
    writeFileSync(GRANT_KEY(), fresh, { mode: 0o600 });
    return Buffer.from(fresh, 'base64');
  } catch { return null; }
};
const sealGrants = (text: string): string | null => {
  const k = grantKey();
  if (!k) return null;
  const iv = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', k, iv);
  const data = Buffer.concat([cipher.update(text, 'utf8'), cipher.final()]);
  return JSON.stringify({ v: 1, iv: iv.toString('base64'), tag: cipher.getAuthTag().toString('base64'), data: data.toString('base64') });
};
const readGrants = (): Record<string, any> => {
  try {
    const parsed = JSON.parse(readFileSync(GRANT_FILE(), 'utf8'));
    if (!parsed.v) return parsed;
    const k = grantKey();
    if (!k) return {};
    const decipher = createDecipheriv('aes-256-gcm', k, Buffer.from(parsed.iv, 'base64'));
    decipher.setAuthTag(Buffer.from(parsed.tag, 'base64'));
    return JSON.parse(Buffer.concat([decipher.update(Buffer.from(parsed.data, 'base64')), decipher.final()]).toString('utf8'));
  } catch { return {}; }
};
const writeGrants = (all: Record<string, any>) => {
  if (process.env.PI_PISOHYPSE_PERSIST === '0') return;
  const sealed = sealGrants(JSON.stringify(all));
  if (sealed) { try { writeFileSync(GRANT_FILE(), sealed, { mode: 0o600 }); } catch {} }
};
const sessionIdOf = (ctx: any): string => {
  try { return String(ctx?.sessionManager?.getSessionId?.() ?? ''); } catch { return ''; }
};

// Two commands, one session, N workspaces.
//
//   /pisohypse connect [read|read-write|all] [binary]
//       Always the directory Pi is running in. Nothing else.
//
//   /pisohypse-remote <directory> [read|read-write|all] [binary]
//       A workspace somewhere else, named explicitly. An unregistered directory
//       gets its daemon started, because the operator had to type the command.
//
// Each connected workspace gets a slot name: local for the cwd, remote1, remote2
// ... in the order they were connected. The tool addresses one of them by slot,
// never by path, so a tool call carries no workspace name, and a slot the
// operator never connected is refused.
type Connection = { adapter: SessionAdapter; access: string; executable: string; label: string; workspace: string };

const CHOICES = [
  'Read only',
  'Read + ask before each edit and validation command',
  'Full read/write + validation for this session',
];
const FULL = CHOICES[2];
const CONTROLS = ['connect', 'disconnect', 'status'];
const USE = 'Use /pisohypse connect [read|read-write|all] [absolute binary path], /pisohypse-remote <directory> [read|read-write|all] [absolute binary path], disconnect [slot], or status';

export default function pisohypse(pi: ExtensionAPI) {
  const connections = new Map<string, Connection>();
  let localSlot = '';
  let connecting = false;
  let generation = 0;

  const shut = async (connection: Connection) => { await connection.adapter.close().catch(() => {}); };
  const disconnectAll = async () => {
    generation++;
    const all = [...connections.values()];
    connections.clear();
    localSlot = '';
    await Promise.all(all.map(shut));
  };
  // Slots are named by what the operator asked for: one local, then remote1, 2...
  const slotFor = (remote: boolean) => {
    if (!remote) return 'local';
    for (let n = 1; ; n += 1) {
      const label = 'remote' + n;
      if (![...connections.values()].some((c) => c.label === label)) return label;
    }
  };
  const expand = (token: string) =>
    token === '~' ? homedir() : token.startsWith('~/') ? join(homedir(), token.slice(2)) : token;

  // A token is classified by what it is on disk, so order does not matter, and
  // tilde and relative paths resolve against the cwd Pi is running in.
  // Directories are realpath'd: a workspace reached through a symlink has to land
  // on the root the daemon keyed its sockets on.
  const parse = (raw: string, needsDirectory: boolean) => {
    let mode = '';
    let directory = '';
    let binary = '';
    for (const token of raw.split(/\s+/).filter((t) => t && t !== '-')) {
      if (token === 'read' || token === 'read-write' || token === 'all') {
        if (mode) throw new Error('Give one mode, not two (' + mode + ' and ' + token + '). ' + USE);
        mode = token;
        continue;
      }
      const target = resolve(process.cwd(), expand(token));
      if (CONTROLS.includes(token.toLowerCase()))
        throw new Error(token + ' is a command, not a path. ' + USE);
      let stats;
      try { stats = statSync(target); } catch { throw new Error('No such file or directory: ' + target); }
      if (stats.isDirectory()) {
        if (directory) throw new Error('Two directories given (' + directory + ' and ' + target + '); one workspace per command. ' + USE);
        directory = realpathSync(target);
      } else {
        if (binary) throw new Error('Two binary paths given (' + binary + ' and ' + target + '). ' + USE);
        binary = realpathSync(target);
      }
    }
    if (needsDirectory && !directory) throw new Error('Use /pisohypse-remote <directory> [read|read-write|all] [absolute binary path]');
    if (!needsDirectory && directory) throw new Error(directory + ' is a directory; use /pisohypse-remote ' + directory + ' for another workspace, or /pisohypse connect for the cwd');
    return { mode, directory, binary };
  };

  pi.on('session_shutdown', async () => { await disconnectAll(); });

  // A reload rebuilds this runtime: the transports are gone, the session is not.
  // Re-open what the operator authorized, by slot name, so target: "remote1" keeps
  // meaning what it meant.
  pi.on('session_start', async (event: any, ctx: any) => {
    if (event?.reason !== 'reload') return;
    const grantId = sessionIdOf(ctx);
    if (!grantId) return;
    const saved = readGrants()[grantId];
    if (!saved?.slots?.length) return;
    const dropped: string[] = [];
    for (const slot of saved.slots) {
      try {
        // A workspace that is gone, or a grant whose binary no longer verifies,
        // can never restore. Name it once and stop retrying it on every reload.
        if (!existsSync(slot.workspace)) throw new Error('workspace no longer exists: ' + slot.workspace);
        const opened = await SessionAdapter.open({ workspace: slot.workspace, executable: slot.executable, allowEdits: slot.grant !== CHOICES[0], autoStart: true });
        connections.set(opened.workspace, { adapter: opened, access: slot.grant, executable: slot.executable, label: slot.label, workspace: opened.workspace });
        if (slot.label === 'local') localSlot = 'local';
      } catch (error) {
        dropped.push(slot.label);
        ctx.ui?.notify('pisohypse: could not restore ' + slot.label + ' (' + slot.workspace + '): ' + String(error), 'warning');
      }
    }
    if (dropped.length) {
      const all = readGrants();
      const held = all[grantId];
      if (held?.slots) {
        held.slots = held.slots.filter((slot: any) => !dropped.includes(slot.label));
        if (!held.slots.length) delete all[grantId];
        writeGrants(all);
      }
      ctx.ui?.notify('pisohypse: dropped ' + dropped.join(', ') + ' from this grant; not retried on reload', 'info');
    }
    if (connections.size) ctx.ui?.notify('pisohypse restored after reload: ' + [...connections.values()].map((c) => c.label).join(', '), 'info');
  });

  const connect = (remote: boolean) => async (args: string, ctx: any) => {
    const rawArgs = args.trim();
    // A phone-created session has no UI, so a full grant is the only thing it can
    // ask for; the companion sets PI_COMPANION_ACCESS once it sees the command.
    const companionAll = process.env.PI_COMPANION === '1'
      && (remote ? /(?:^|\s)all(?:\s|$)/.test(rawArgs) : /^connect\s+all(?:\s|$)/.test(rawArgs));
    if (!ctx.hasUI && !companionAll)
      throw new Error('pisohypse requires an interactive authorization UI (or a full grant via pi-companion)');
    // Controls are read from the first token whatever its case, so a control word
    // is never resolved against the cwd as a path.
    const words = rawArgs.toLowerCase().split(/\s+/).filter(Boolean);
    if (words[0] === 'disconnect' && !words[1]) {
      await disconnectAll();
      const grantId = sessionIdOf(ctx);
      if (grantId) { const all = readGrants(); delete all[grantId]; writeGrants(all); }
      ctx.ui.notify('pisohypse disconnected; grants revoked, and nothing will be restored on reload', 'info');
      return;
    }
    if (words[0] === 'disconnect') {
      const label = words[1];
      const hit = [...connections.values()].find((c) => c.label === label);
      if (!hit) throw new Error('Not connected: ' + label + '. Connected: ' + list());
      connections.delete(hit.workspace);
      if (localSlot === hit.label) localSlot = '';
      await shut(hit);
      ctx.ui.notify('pisohypse disconnected ' + hit.label + ' (' + hit.workspace + ')', 'info');
      return;
    }
    if (words[0] === 'status') {
      const all = [...connections.values()];
      ctx.ui?.notify(all.length
        ? all.map((c) => c.label + (c.label === localSlot ? ' (default)' : '') + '  ' + c.workspace + ' — ' + c.access + ' (' + c.adapter.session + ')').join('\n')
        : 'pisohypse disconnected', 'info');
      return;
    }
    if (!remote && rawArgs && !/^connect(?:\s|$)/.test(rawArgs)) throw new Error(USE);
    if (remote && /^connect\b/.test(rawArgs)) throw new Error('/pisohypse-remote takes a directory, not the word connect. ' + USE);
    if (connecting) throw new Error('Already connecting');
    if (!ctx.isProjectTrusted()) throw new Error('Trust this project in Pi before granting Isohypse access');
    const body = remote ? rawArgs : rawArgs.replace(/^connect\s*/, '').trim();
    const { mode, directory, binary } = parse(body, remote);
    const workspace = directory || realpathSync(ctx.cwd);
    if (connections.has(workspace)) throw new Error('Already connected as ' + connections.get(workspace)!.label + '; disconnect it first');
    const executable = binary || realpathSync(join(homedir(), 'bin', 'isohypse'));
    const requested = mode === 'read' ? CHOICES[0] : mode === 'read-write' ? CHOICES[1] : mode === 'all' ? FULL : undefined;
    const grant = requested ?? await ctx.ui.select(
      'pisohypse: ' + workspace + '\nBinary: ' + executable
      + '\nFull read/write also authorizes validation shell commands without further prompts.'
      + '\nValidation runs with daemon OS permissions, NOT sandboxed by this workspace.'
      + '\nGrant for this Pi session only?', [...CHOICES, 'Deny']);
    if (!CHOICES.includes(grant ?? '')) return;
    connecting = true;
    const expectedGeneration = generation;
    try {
      // A workspace with no daemon is started here, which is the point of naming
      // it: the operator typed this command, so its socket may come up.
      const opened = await SessionAdapter.open({ workspace, executable, allowEdits: grant !== CHOICES[0], autoStart: true });
      if (generation !== expectedGeneration) { await opened.close().catch(() => {}); return; }
      const label = slotFor(remote);
      connections.set(opened.workspace, { adapter: opened, access: grant, executable, label, workspace: opened.workspace });
      if (!remote) localSlot = label;
      const grantId = sessionIdOf(ctx);
      if (grantId) {
        const all = readGrants();
        all[grantId] = { at: new Date().toISOString(), slots: [...connections.values()].map((c) => ({ label: c.label, workspace: c.workspace, grant: c.access, executable: c.executable })) };
        writeGrants(all);
      }
      ctx.ui.notify('pisohypse connected: ' + label + ' — ' + opened.session + ' — ' + grant
        + '\nWorkspace: ' + opened.workspace + (remote ? ' (use target "' + label + '" to address it)' : ' (local; the default target)')
        + '. No daemon policy was widened.', 'info');
    } finally { connecting = false; }
  };
  const list = () => {
    const all = [...connections.values()];
    return all.length ? all.map((c) => c.label).join(', ') : 'nothing';
  };

  beat('registering commands');
  pi.registerCommand('pisohypse', {
    description: 'connect [read|read-write|all] [absolute binary path], disconnect [slot], or status; workspace-scoped Isohypse authorization for the cwd (slot: local)',
    handler: connect(false),
  });
  pi.registerCommand('pisohypse-remote', {
    description: 'connect another workspace by directory: <directory> [read|read-write|all] [absolute binary path]; starts its daemon when it is not running (slot: remote1, remote2, ...)',
    handler: connect(true),
  });

  // The model cannot run /pisohypse status, and the tool schema is static, so the
  // live slot list is appended to the system prompt on every turn. Without it a
  // remote workspace would only be discoverable by making a call that fails.
  pi.on('before_agent_start', async (event: any) => {
    if (existsSync(join(homedir(), '.pi', 'pisohypse-off'))) return;
    const all = [...connections.values()];
    if (!all.length) return;
    const lines = all
      .map((c) => '  ' + c.label + ' = ' + c.workspace + (c.label === localSlot ? ' (the default)' : '') + ' — ' + c.access)
      .join('\n');
    return {
      systemPrompt: String(event?.systemPrompt ?? '')
        + '\npisohypse slots — pass one by NAME as the pisohypse tool target, never a path; omit target for the default:\n'
        + lines + '\n',
    };
  });
  const NOT_AUTHORIZED = 'pisohypse has no authorized workspace in this session. Do not retry. Tell the operator to run /pisohypse connect (or /pisohypse-remote <directory>), and use ctx tools or built-in file tools for the read meanwhile.';
  const pick = (target: string | undefined): Connection => {
    const all = [...connections.values()];
    if (!all.length) throw new Error(NOT_AUTHORIZED);
    if (!target) {
      const fallback = localSlot || (all.length === 1 ? all[0].label : '');
      const chosen = all.find((c) => c.label === fallback);
      if (chosen) return chosen;
      throw new Error('Connected: ' + list() + '. Pass target to choose one.');
    }
    const hit = all.find((c) => c.label === target.toLowerCase());
    if (!hit) throw new Error('Not connected: ' + target + '. Connected: ' + list());
    return hit;
  };

  const GRAMMAR_SUMMARY = [
    'Run ordered Isohypse steps in one persistent, authorized workspace session. The grammar is already loaded; do not call context.prompt.',
    'Steps are strict: each op accepts ONLY its listed fields (plus label). Unknown fields are rejected.',
    'context.read {arg: file, range: "10-80", outline, tag, workspace} returns [path#TAG] and numbered lines.',
    'context.find {arg: one pattern, paths: [dirs], langs, max 1-200, literal, name}.',
    'context.explore {arg: symbol query, paths, langs, max 1-200, literal, compact, full}.',
    'context.log {arg: recorded path, workspace, tag, since, until, max 1-200, offset} lists saved revisions; context.read with tag reads one.',
    'mutate.edit {path, tag: TAG from a read, edits: [..]}; mutate.create {path, content} for new files; mutate.delete {path}. Mutations need a top-level validate shell command.',
    'Edit objects, one action each: {replace: {symbol: "name"} | {lines: [a,b]} | {block: n}, with: "text"}; {insert: {before: n} | {after: n} (n may be "^" or "$"), body: "text"}; {delete: {symbol} | {lines} | {block}}.',
    'Paths are workspace-relative, no ..; read/find/explore/log use arg, never path/query/pattern. session.reconnect must be the only step.',
    'Example: {"steps":[{"op":"context.find","arg":"parse_request","paths":["src"],"max":10},{"op":"context.read","arg":"src/main.rs","range":"1-80"}]}',
    'Edit example: {"steps":[{"op":"mutate.edit","path":"src/main.rs","tag":"TAG","edits":[{"replace":{"lines":[10,12]},"with":"new text"}]}],"validate":"cargo check"}',
    'target picks a connected slot (local default, remote1, remote2), never a path. Full read/write grants skip per-edit prompts; ask-per-edit confirms each batch. Output limited to 50 KiB/2000 lines; prefer ranges.',
  ].join(' ');

  beat('registering tool');
  pi.registerTool({
    name: 'pisohypse',
    label: 'pisohypse',
    description: GRAMMAR_SUMMARY,
    promptSnippet: 'Workspace-scoped Isohypse reads, exploration, tagged edits, and transactional validation',
    promptGuidelines: [
      'pisohypse: the grammar is already loaded; start with the work. Never send context.prompt first. Sent alone it returns the full reference, sent beside other steps it is dropped.',
      'pisohypse: use ONLY the ops and fields listed in the tool description. context.read/log/find/explore take arg (never path, query, pattern); paths (plural) narrows find/explore; max is 1-200. There is no context.edit/write/apply/help op: edits are mutate.edit with a tag from a read.',
      'pisohypse: batch dependent steps. An ACK is not completion; never blindly replay a failed mutation.',
      'pisohypse: omit target to work in the cwd; pass target only with a slot name from the pisohypse slots list (local, remote1, remote2 ...). Never pass a path as target.',
      'pisohypse: when it reports no authorization or no slot, stop retrying, tell the operator to run /pisohypse connect (cwd) or /pisohypse-remote <directory>, and use ctx tools or built-in file tools for that read meanwhile.',
      'pisohypse: after a disconnect or daemon reload, send session.reconnect as the only step. It reuses the scope already authorized; do not ask the operator to reconnect.',
    ],
    parameters: Type.Object({
      target: Type.Optional(Type.String({ description: 'Connected slot to run against: local (the cwd, the default) or remote1, remote2, as /pisohypse status lists them. Never a path; an unconnected slot is refused.' })),
      steps: Type.Array(Type.Object({
        op: StringEnum(['context.read', 'context.find', 'context.explore', 'context.log', 'context.prompt', 'mutate.edit', 'mutate.create', 'mutate.delete', 'session.reconnect'] as const),
        label: Type.Optional(Type.String()), arg: Type.Optional(Type.String()),
        range: Type.Optional(Type.String()), outline: Type.Optional(Type.Boolean()),
        paths: Type.Optional(Type.Array(Type.String())), langs: Type.Optional(Type.Array(Type.String())),
        max: Type.Optional(Type.Integer({ minimum: 1, maximum: 200 })),
        literal: Type.Optional(Type.Boolean()), name: Type.Optional(Type.Boolean()), checkpoint: Type.Optional(Type.Boolean()),
        compact: Type.Optional(Type.Boolean()), full: Type.Optional(Type.Boolean()),
        path: Type.Optional(Type.String()), tag: Type.Optional(Type.String()),
        workspace: Type.Optional(Type.String()),
        since: Type.Optional(Type.Integer({ minimum: 0 })), until: Type.Optional(Type.Integer({ minimum: 0 })),
        offset: Type.Optional(Type.Integer({ minimum: 0 })),
        content: Type.Optional(Type.String()), edits: Type.Optional(Type.Array(Type.Unknown())),
      }, { additionalProperties: false }), { minItems: 1, maxItems: 32 }),
      validate: Type.Optional(Type.String({ description: 'Required for mutations: exact shell command; runs with daemon OS permissions, NOT sandboxed by cwd. Authorized by the full read/write session grant or per-batch confirmation.' })),
    }, { additionalProperties: false }),
    async execute(_id, input, signal, _update, ctx) {
      // Reconnect is standalone: the previous transports are gone, so it must not
      // be dispatched through them. Every connected slot is reopened under the
      // scope already granted; nothing is replayed.
      if (input.steps.length === 1 && input.steps[0].op === 'session.reconnect') {
        if (Object.keys(input).length !== 1) throw new Error('session.reconnect must be the only field in the batch');
        if (!connections.size) throw new Error(NOT_AUTHORIZED);
        generation++;
        const reopened: string[] = [];
        for (const [key, connection] of [...connections.entries()]) {
          connections.delete(key);
          await connection.adapter.close().catch(() => {});
          const opened = await SessionAdapter.open({
            workspace: connection.workspace, executable: connection.executable,
            allowEdits: connection.access !== CHOICES[0], autoStart: true,
          });
          connections.set(opened.workspace, { ...connection, adapter: opened, workspace: opened.workspace });
          if (localSlot === connection.label) localSlot = connection.label;
          reopened.push(connection.label);
        }
        return {
          content: [{ type: 'text', text: 'pisohypse reconnected ' + reopened.join(', ') + '; your authorized scope is unchanged and nothing was replayed. The grammar reloads automatically; read affected files before editing.' }],
          details: { slots: reopened },
        };
      }

      if (!connections.size && process.env.PI_COMPANION_ACCESS === 'all') {
        const workspace = realpathSync(ctx.cwd);
        const executable = realpathSync(join(homedir(), 'bin', 'isohypse'));
        const opened = await SessionAdapter.open({ workspace, executable, allowEdits: true });
        connections.set(opened.workspace, { adapter: opened, access: FULL, executable, label: 'local', workspace: opened.workspace });
        localSlot = 'local';
      }
      const current = pick(input.target);
      const sessionWrites = current.access === FULL;
      // prepareBatch validates the batch object strictly, so the slot selector is
      // dropped before the batch reaches the session.
      const { target: _slot, ...batch } = input as { target?: string } & Record<string, unknown>;
      const grammarRefused = (text: string) => /call context\.prompt first/i.test(text);
      // A strict-schema rejection is a naming mistake, so name the legal fields.
      const schemaHint = (message: string) => /additional propert|unexpected|invalid|schema|expected object/i.test(message)
        ? message + '\nPer op: context.read {arg, range, outline, tag, workspace}; context.find {arg, paths, langs, max, literal, name}; context.explore {arg, paths, langs, max, literal, compact, full}; context.log {arg, workspace, tag, since, until, max, offset}; mutate.edit {path, tag, edits, checkpoint}; mutate.create {path, content, checkpoint}; mutate.delete {path, checkpoint}. Reads use arg, never path/query/pattern.'
        : message;
      const loadGrammar = () => current.adapter.run({ steps: [{ op: 'context.prompt' }] }, { signal }).then((r: { output: string }) => r.output).catch(() => '');
      const runBatch = (next: Record<string, unknown>) => current.adapter.run(next, {
        signal,
        authorize: async (preview: string) => sessionWrites || (ctx.hasUI && await ctx.ui.confirm('pisohypse: approve these edits AND validation shell execution?',
          'Slot: ' + current.label + '\nValidation runs with daemon OS permissions, not a filesystem/network sandbox. Approve only a command you trust.\n\n' + preview, { signal })),
        lock: async (paths: string[], run: () => Promise<unknown>) => {
          const nextIndex = (index: number): Promise<unknown> => index === paths.length ? run() : withFileMutationQueue(paths[index], () => nextIndex(index + 1));
          return nextIndex(0);
        },
      });
      // Load the grammar here and discard its reply, so the model neither calls
      // context.prompt nor pays for the ~2 KB it returns. A grammar refusal means
      // the daemon ran nothing, so retrying the same batch cannot replay a mutation.
      const steps = (batch.steps as Array<{ op: string }>).filter((step) => step.op !== 'context.prompt');
      if (!steps.length) {
        const reference = await loadGrammar();
        return { content: [{ type: 'text', text: reference || 'grammar loaded' }], details: { slot: current.label, session: current.adapter.session, slots: [...connections.values()].map((c) => c.label + '=' + c.workspace) } };
      }
      const nextBatch = { ...batch, steps };
      let result;
      try {
        result = await runBatch(nextBatch);
      } catch (error) {
        const message = String((error as Error)?.message ?? error);
        if (!grammarRefused(message)) throw new Error(schemaHint(message));
        await loadGrammar();
        result = await runBatch(nextBatch);
      }
      if (grammarRefused(result.output)) {
        await loadGrammar();
        result = await runBatch(nextBatch);
      }
      const truncated = truncateHead(result.output);
      let text = truncated.content;
      if (truncated.truncated) {
        const directory = mkdtempSync(join(tmpdir(), 'pisohypse-'));
        const path = join(directory, 'output.txt');
        writeFileSync(path, result.output, { mode: 0o600 });
        text += '\n[Truncated; full output: ' + path + '. Prefer a narrower pisohypse range.]';
      }
      return { content: [{ type: 'text', text }], details: { slot: current.label, session: current.adapter.session, sequence: result.seq, slots: [...connections.values()].map((c) => c.label + '=' + c.workspace) } };
    },
  });
  beat('loaded: commands pisohypse + pisohypse-remote, tool pisohypse');
}
