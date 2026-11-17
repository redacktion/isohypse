import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, readFileSync, readdirSync, existsSync, lstatSync, rmSync, realpathSync } from 'node:fs';
import { execFileSync, spawn } from 'node:child_process';
import { join, resolve } from 'node:path';
import { SessionAdapter } from '../../session.mjs';

const executable = realpathSync(process.argv[2]);
const codex = realpathSync(process.argv[3]);
const parent = realpathSync(mkdtempSync(join(process.cwd(), '.iso-start-')));
const root = join(parent, 'workspace');
mkdirSync(root);
const priorStore = process.env.ISOHYPSE_STORE;
process.env.ISOHYPSE_STORE = join(parent, 'store');
const supervisor = spawn(executable, ['daemon', parent, '--no-lsp', '--no-semantic'], { detached: true, stdio: ['ignore', 'ignore', 'pipe'] });
let diagnostic = '';
supervisor.stderr.on('data', chunk => diagnostic = (diagnostic + chunk).slice(-2000));
const stopped = new Promise(resolve => supervisor.once('exit', resolve));
for (let n = 0; n < 100 && !existsSync(join(parent, 'store/supervisor.sock')); n++) await new Promise(resolve => setTimeout(resolve, 100));

try {
  await assert.rejects(SessionAdapter.open({ workspace: root, executable: '/bin/echo', autoStart: true }), /identifier is/);
  assert.equal(existsSync(join(root, '.isohypse.sock')), false);
  // A workspace holds none of our state now: nothing in the tree is inspected,
  // replaced, or trusted, and a file that merely carries one of our old names is
  // left exactly where the operator put it.
  writeFileSync(join(root, '.isohypse.sock'), 'not isohypse');
  mkdirSync(join(root, 'harness/pi'), { recursive: true });
  writeFileSync(join(root, 'harness/pi/manifest.json'), '{"name":"pisohypse","fixture":"independent startup workspace"}\n');
  // A live ancestor exists, but the native call must start and read THIS root.

  execFileSync(process.execPath, [resolve('harness/chatgpt/native/test-live.mjs'), root, executable, codex,
    '--native', '--native-ws', '--write-enabled', '--interrupt-native',
    ...process.argv.slice(4)], { stdio: 'inherit', timeout: 120000 });
  assert.equal(readFileSync(join(root, '.isohypse.sock'), 'utf8'), 'not isohypse');
  assert.equal(readdirSync(root).filter(name => name.startsWith('.isohypse')).length, 1, 'only the operator file');
  const runDirs = readdirSync(join(parent, 'store', 'run'));
  assert.equal(runDirs.length, 1, 'one run directory per workspace');
  const runDir = join(parent, 'store', 'run', runDirs[0]);
  assert.equal(realpathSync(join(runDir, 'workspace')), realpathSync(root));
  assert.ok(lstatSync(join(runDir, 'daemon.sock')).isSocket());
  assert.ok(lstatSync(join(runDir, 'session.sock')).isSocket());
  const firstSocket = lstatSync(join(runDir, 'daemon.sock')).ino;
  const session = await SessionAdapter.open({ workspace: root, executable, autoStart: true });
  try {
    const result = await session.run({ steps: [{ op: 'context.read', arg: 'harness/pi/manifest.json' }] });
    assert.match(result.output, /independent startup workspace/);
    assert.equal(lstatSync(join(runDir, 'daemon.sock')).ino, firstSocket, 'an answering daemon is reused, not restarted');
  } finally { await session.close(); }
  console.log('STARTUP PASS: native missing-workspace startup, exact-root read, live reuse, sockets outside the workspace, operator files untouched.');
} finally {
  if (diagnostic) console.error(diagnostic);
  supervisor.kill('SIGTERM');
  await stopped;
  if (priorStore === undefined) delete process.env.ISOHYPSE_STORE; else process.env.ISOHYPSE_STORE = priorStore;
  rmSync(parent, { recursive: true, force: true });
}
