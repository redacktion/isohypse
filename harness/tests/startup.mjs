import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, readFileSync, existsSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { ensureWorkspace } from '../session.mjs';

// Sockets live under the isohypse home now, so a harness cannot look for files.
// The fake binary below answers `state socket` exactly as the real one does: a
// path and exit 0 when the daemon is live, exit 1 when nothing answers.
const root = mkdtempSync(join(tmpdir(), "iso-start-test's "));
const executable = join(root, 'starter');
const receipt = join(root, 'receipt');
const liveFile = join(root, 'live');
const socketPath = join(root, 'run', 'daemon.sock');
let timer;
try {
  writeFileSync(executable, `#!${process.execPath}\nconst fs = require('node:fs');\nconst args = process.argv.slice(2);\nif (args[0] === 'state' && args[1] === 'socket') {\n  if (fs.existsSync(${JSON.stringify(liveFile)})) { process.stdout.write(${JSON.stringify(socketPath)} + '\\n'); process.exit(0); }\n  process.exit(1);\n}\nfs.writeFileSync(${JSON.stringify(receipt)}, JSON.stringify(args));\n`, { mode: 0o700 });
  // Readiness is published only after the start command is recorded, so a second
  // concurrent call must join the first start rather than issue its own.
  let publish;
  const published = new Promise((resolve) => { publish = () => {
    if (!existsSync(receipt)) { timer = setTimeout(publish, 10); return; }
    writeFileSync(liveFile, '');
    resolve();
  }; });
  timer = setTimeout(publish, 10);
  await Promise.all([ensureWorkspace(root, executable), ensureWorkspace(root, executable), published]);
  assert.deepEqual(JSON.parse(readFileSync(receipt, 'utf8')), ['state', 'up', root]);
  rmSync(receipt);
  await ensureWorkspace(root, executable);
  assert.equal(existsSync(receipt), false, 'a daemon that answers the probe is never started again');
  console.log('STARTUP UNIT PASS: exact argv, shared startup wait, live-probe reuse.');
} finally {
  clearTimeout(timer);
  rmSync(root, { recursive: true, force: true });
}
