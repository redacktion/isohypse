// Command-surface checks for harness/pi/pisohypse.ts.
//
//   node harness/tests/pisohypse.mjs
//
// The extension imports typebox and two Pi packages at load, so this resolves
// them from the installed Pi app and copies the real files into a temp home.
// HOME points at that temp dir, so the grant file, its key, and the load log
// never touch the operator's real ones.
import assert from 'node:assert/strict';
import { createCipheriv, randomBytes } from 'node:crypto';
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, '..', '..');
const BUNDLE = '/Applications/bb.app/Contents/Resources/app.asar.unpacked/node_modules';
if (!existsSync(BUNDLE)) {
  console.log('pisohypse command checks skipped: no Pi app bundle to resolve typebox from');
  process.exit(0);
}

const home = realpathSync(mkdtempSync(join(tmpdir(), 'pisohypse-home-')));
const dir = realpathSync(mkdtempSync(join(tmpdir(), 'pisohypse-ext-')));
const realHome = process.env.HOME;
process.env.HOME = home;
mkdirSync(join(home, '.pi'), { recursive: true });

// A failed assertion must still clean up, so removal rides the exit hook.
const cleanup = () => {
  process.env.HOME = realHome;
  try { rmSync(home, { recursive: true, force: true }); } catch {}
  try { rmSync(dir, { recursive: true, force: true }); } catch {}
};
process.on('exit', cleanup);

const grantsFile = join(home, '.pi', 'pisohypse-grants.json');
const keyFile = join(home, '.pi', 'pisohypse.key');
const key = randomBytes(32).toString('base64');
writeFileSync(keyFile, key, { mode: 0o600 });

mkdirSync(join(dir, 'node_modules', '@earendil-works'), { recursive: true });
symlinkSync(join(BUNDLE, 'typebox'), join(dir, 'node_modules', 'typebox'));
for (const name of ['pi-ai', 'pi-coding-agent']) {
  symlinkSync(join(BUNDLE, '@earendil-works', name), join(dir, 'node_modules', '@earendil-works', name));
}
// Mirror the repo layout: the extension reaches its transport as ../session.mjs.
mkdirSync(join(dir, 'pi'));
copyFileSync(join(REPO, 'harness', 'session.mjs'), join(dir, 'session.mjs'));
copyFileSync(join(REPO, 'harness', 'pi', 'pisohypse.ts'), join(dir, 'pi', 'pisohypse.ts'));

const seal = value => {
  const iv = randomBytes(12);
  const cipher = createCipheriv('aes-256-gcm', Buffer.from(key, 'base64'), iv);
  const data = Buffer.concat([cipher.update(JSON.stringify(value), 'utf8'), cipher.final()]);
  return JSON.stringify({ v: 1, iv: iv.toString('base64'), tag: cipher.getAuthTag().toString('base64'), data: data.toString('base64') });
};
const { default: pisohypse } = await import(join(dir, 'pi', 'pisohypse.ts'));

const commands = {};
const hooks = {};
const notes = [];
const SESSION = 'suite-session';
pisohypse({
  registerCommand: (name, definition) => { commands[name] = definition; },
  registerTool: () => {},
  on: (name, handler) => { hooks[name] = handler; },
});
const ctx = (id = SESSION) => ({
  cwd: dir,
  hasUI: true,
  isProjectTrusted: () => true,
  ui: {
    notify: (text, level) => notes.push({ text, level }),
    select: async () => 'Deny',
    confirm: async () => true,
  },
  sessionManager: { getSessionId: () => id },
});
const last = () => notes[notes.length - 1]?.text ?? '';

// ── a control word is a command, never a path ───────────────────────────────

await commands['pisohypse-remote'].handler('disconnect', ctx());
assert.match(last(), /disconnected; grants revoked/, 'disconnect is not resolved as a path: ' + last());

await commands['pisohypse-remote'].handler('DISCONNECT', ctx());
assert.match(last(), /disconnected; grants revoked/, 'a control word is case-insensitive: ' + last());

await commands['pisohypse-remote'].handler('status', ctx());
assert.equal(last(), 'pisohypse disconnected', 'status reports instead of resolving a path: ' + last());

await assert.rejects(commands['pisohypse-remote'].handler('disconnect remote1', ctx()), /Not connected: remote1/);
notes.length = 0;

// A control word in any position is refused as a path, not stat'd.
await assert.rejects(commands['pisohypse-remote'].handler(dir + ' disconnect', ctx()), /is a command, not a path/);
await assert.rejects(commands['pisohypse-remote'].handler(dir + ' status', ctx()), /is a command, not a path/);

// ── a slot that cannot restore is dropped, not retried forever ──────────────

const gone = join(dir, 'workspace-that-is-gone');
const gone2 = join(dir, 'another-gone-workspace');
writeFileSync(grantsFile, seal({
  [SESSION]: {
    at: new Date().toISOString(),
    slots: [
      { label: 'local', workspace: gone, grant: 'Full read/write + validation for this session', executable: process.execPath },
      { label: 'remote1', workspace: gone2, grant: 'Full read/write + validation for this session', executable: process.execPath },
    ],
  },
}), { mode: 0o600 });

notes.length = 0;
await hooks.session_start({ reason: 'reload' }, ctx());
assert.ok(notes.some(n => n.text.includes('could not restore local') && n.text.includes('workspace no longer exists')), JSON.stringify(notes));
assert.ok(notes.some(n => n.text.includes('dropped local, remote1')), JSON.stringify(notes));
assert.ok(!notes.some(n => n.text.includes('restored after reload')), 'nothing was restored: ' + JSON.stringify(notes));

// The dead slots are gone from the record, so the next reload is quiet.
const raw = readFileSync(grantsFile, 'utf8');
const parsed = JSON.parse(raw);
const decipher = (await import('node:crypto')).createDecipheriv('aes-256-gcm', Buffer.from(key, 'base64'), Buffer.from(parsed.iv, 'base64'));
decipher.setAuthTag(Buffer.from(parsed.tag, 'base64'));
const saved = JSON.parse(Buffer.concat([decipher.update(Buffer.from(parsed.data, 'base64')), decipher.final()]).toString('utf8'));
assert.equal(saved[SESSION], undefined, 'a grant with no restorable slot is removed: ' + JSON.stringify(saved));

notes.length = 0;
await hooks.session_start({ reason: 'reload' }, ctx());
assert.equal(notes.length, 0, 'the second reload has nothing left to retry: ' + JSON.stringify(notes));

console.log('pisohypse command checks passed: controls are never paths, dead slots are dropped');
