import assert from 'node:assert/strict';
import { mkdtempSync, writeFileSync, rmSync, symlinkSync, readFileSync, realpathSync, copyFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createServer } from 'node:net';
import { SessionAdapter, prepareBatch } from '../session.mjs';

const read = arg => ({ steps: [{ op: 'context.read', arg }] });
const root = realpathSync(mkdtempSync(join(tmpdir(), 'pisohypse-test-')));
const markers = [createServer(), createServer()];
for (const [index, name] of ['.isohypse.sock', '.isohypse.session.sock'].entries()) {
  await new Promise((resolve, reject) => markers[index].once('error', reject).listen(join(root, name), resolve));
}
const sessions = [];
try {
  writeFileSync(join(root, 'a.rs'), 'fn original() {}\n');
  symlinkSync(join(root, 'a.rs'), join(root, 'alias.rs'));
  symlinkSync(tmpdir(), join(root, 'outside'));
  const mutation = { steps: [{ op: 'mutate.edit', path: 'a.rs', tag: '1234567', edits: [{ replace: { lines: [1, 1] }, with: 'fn changed() {}' }] }], validate: 'true' };
  const aliases = structuredClone(mutation);
  aliases.steps.push({ ...aliases.steps[0], path: 'alias.rs' });
  assert.equal(prepareBatch(root, aliases).paths.length, 1, 'canonical lock keys must deduplicate symlink aliases');
  for (const input of [read('../outside'), read('/etc/passwd'), read('outside/no-such-file'),
    { steps: [{ op: 'state.stop' }] }, { steps: [{ op: 'macro.run', name: 'oops' }] },
    { steps: [{ op: 'context.read', arg: 'a.rs', workspace: '*' }] },
    { steps: [{ op: 'context.read', arg: 'a.rs', ops: [] }] },
    { ...read('a.rs'), validate: 'touch surprise' }, { ...read('a.rs'), expr: 'evil' },
    { ...mutation, validate: 'none' }, { steps: [] },
    { steps: [{ op: 'mutate.edit', path: 'a.rs', tag: '1234567', edits: [{ move: { to: '../outside' } }] }], validate: 'true' },
  ]) assert.throws(() => prepareBatch(root, input));
  const historical = { steps: [{ op: 'context.read', workspace: '/archived/workspace', tag: '88a1a2c', range: '1-80' }] };
  assert.deepEqual(prepareBatch(root, historical).batch, historical);
  assert.deepEqual(prepareBatch(root, historical).paths, [], 'saved reads never acquire write authority');
  assert.doesNotThrow(() => prepareBatch(root, { steps: [{ op: 'context.read', arg: 'a.rs', range: '1-2' }] }));
  assert.doesNotThrow(() => prepareBatch(root, { steps: [{ op: 'context.log', workspace: '/archived/workspace', since: 1, until: 2, max: 5, offset: 0 }] }));
  for (const step of [
    { op: 'context.log', workspace: '/archive/../other' },
    { op: 'context.log', since: 3, until: 2 },
    { op: 'context.log', offset: -1 },
    { op: 'context.read', tag: 'not-a-tag' },
    { op: 'context.read', tag: '88a1a2c', arg: '../outside' },
    { op: 'context.read', tag: '88a1a2c', range: '0-4' },
  ]) assert.throws(() => prepareBatch(root, { steps: [step] }));

  writeFileSync(join(root, 'session'), `
    const { createInterface } = require('node:readline');
    const send = frame => process.stdout.write(JSON.stringify(frame) + '\\n');
    let seq = 0, closed = false;
    const jobs = [];
    const lines = createInterface({ input: process.stdin });
    lines.on('line', line => {
      const frame = JSON.parse(line);
      if (frame.do === 'session.open') send({ ch: 'ready', session: 'same-session', token: 'secret', seq: ++seq });
      if (frame.do === 'session.command' && frame.op === 'state.status') send({ ch: 'result', id: frame.id, json: {root: require('node:fs').existsSync('wrong-root') ? '/wrong' : process.cwd()}, seq: ++seq });
      if (frame.do === 'session.command' && frame.op !== 'state.status') {
        if (frame.workspace !== undefined) throw new Error('History must not change session routing');
        send({ ch: 'result', id: frame.id, output: JSON.stringify({ steps: [{ op: frame.op, ...frame.payload }] }), seq: ++seq });
      }
      if (frame.do === 'session.multiop') {
        const batch = JSON.parse(frame.bind.batch);
        const arg = batch.steps[0].arg;
        send({ ch: 'ack', id: frame.id, seq: ++seq });
        if (arg === 'disconnect') return process.exit(0);
        jobs.push(new Promise(resolve => setTimeout(() => {
          send(arg === 'fail' ? { ch: 'error', id: frame.id, message: 'expected failure', seq: ++seq }
            : { ch: 'result', id: frame.id, output: JSON.stringify(batch), seq: ++seq });
          resolve();
        }, arg === 'slow' ? 200 : 5)));
      }
      if (frame.do === 'session.cancel') send({ ch: 'cancelled', id: frame.id, seq: ++seq });
      if (frame.do === 'session.close' && !closed) {
        closed = true;
        Promise.all(jobs).then(() => { send({ ch: 'snapshot', state: { closed: true }, seq: ++seq }); process.exit(0); });
      }
    });
  `);
  // Test seam: the constructor skips codesign verification; production code
  // must go through SessionAdapter.open, which always verifies.
  const open = async allowEdits => {
    const s = new SessionAdapter(root, process.execPath, allowEdits);
    await s.ready;
    sessions.push(s); return s;
  };
  if (process.platform === 'darwin') {
    // The fake transport runs under Node: reject on ad-hoc signature or
    // wrong identifier before any spawn.
    await assert.rejects(SessionAdapter.open({ workspace: root, executable: process.execPath, allowEdits: false }), /ad-hoc signed|identifier is/);
    await assert.rejects(SessionAdapter.open({ workspace: root, executable: '/bin/echo', allowEdits: false }), /identifier is/);
    const fixture = join(root, 'unsigned');
    copyFileSync('/bin/echo', fixture);
    assert.equal(spawnSync('/usr/bin/codesign', ['--remove-signature', fixture]).status, 0);
    await assert.rejects(SessionAdapter.open({ workspace: root, executable: fixture }), /failed strict codesign/);
    assert.equal(spawnSync('/usr/bin/codesign', ['--force', '--sign', '-', '--identifier', 'isohypse', fixture]).status, 0);
    await assert.rejects(SessionAdapter.open({ workspace: root, executable: fixture }), /ad-hoc signed/);
    writeFileSync(join(root, 'codesign'), '#!/bin/sh\nprintf "Identifier=isohypse\\nAuthority=fake\\n"\n', { mode: 0o700 });
    const oldPath = process.env.PATH;
    try {
      process.env.PATH = root;
      await assert.rejects(SessionAdapter.open({ workspace: root, executable: '/bin/echo' }), /identifier is/);
    } finally {
      if (oldPath === undefined) delete process.env.PATH; else process.env.PATH = oldPath;
    }
  }
  writeFileSync(join(root, 'wrong-root'), '');
  await assert.rejects(open(false), /workspace mismatch/);
  rmSync(join(root, 'wrong-root'));
  const reader = await open(false);
  assert.equal(reader.session, 'same-session');
  const first = await reader.run(read('a.rs'));
  await assert.rejects(reader.run(read('fail')), /expected failure/);
  assert.ok((await reader.run(read('a.rs'))).seq > first.seq);
  assert.deepEqual(JSON.parse((await reader.run(historical)).output), historical);
  const historyLog = { steps: [{ op: 'context.log', workspace: '/archived/workspace', tag: '88a1a2c' }] };
  assert.deepEqual(JSON.parse((await reader.run(historyLog)).output), historyLog);
  let asked = false;
  await assert.rejects(reader.run(mutation, { authorize: () => { asked = true; return true; } }), /Read-only/);
  assert.equal(asked, false);
  const editor = await open(true);
  await assert.rejects(editor.run(mutation), /not authorized/);
  await assert.rejects(editor.run(mutation, { authorize: () => false }), /not authorized/);
  const input = structuredClone(mutation);
  const result = await editor.run(input, { authorize: preview => {
    assert.deepEqual(JSON.parse(preview), mutation);
    input.validate = 'unapproved';
    return true;
  } });
  assert.deepEqual(JSON.parse(result.output), mutation, 'dispatch must match approved snapshot');
  const aliasEdit = structuredClone(mutation);
  aliasEdit.steps[0].path = 'alias.rs';
  await assert.rejects(editor.run(aliasEdit, { authorize: () => {
    writeFileSync(join(root, 'b.rs'), 'other');
    rmSync(join(root, 'alias.rs')); symlinkSync(join(root, 'b.rs'), join(root, 'alias.rs'));
    return true;
  } }), /targets changed/);
  const slow = reader.run(read('slow'), { signal: AbortSignal.timeout(50) });
  await assert.rejects(slow, /drained and closed/);
  await assert.rejects(reader.run(read('a.rs')), /reconnect explicitly/);
  const broken = await open(false);
  await assert.rejects(broken.run(read('disconnect')), /outcome unconfirmed; no replay/);
  await assert.rejects(broken.run(read('a.rs')), /reconnect explicitly/);
  await assert.rejects(broken.close(), /outcome unconfirmed/);
  await editor.close();
  await editor.close();
  console.log('pisohypse checks passed: scope, grants, immutable approval, persistent failures, cancellation drain, disconnect/no replay');
} finally {
  await Promise.all(sessions.map(s => s.close().catch(() => {})));
  await Promise.all(markers.map(marker => new Promise(resolve => marker.close(resolve))));
  rmSync(root, { recursive: true, force: true });
}

if (process.argv[2] === '--live') {
  const workspace = resolve(process.argv[3]), executable = resolve(process.argv[4]);
  const session = await SessionAdapter.open({ workspace, executable, allowEdits: true });
  const path = `harness/pisohypse-check-${process.pid}.rs`;
  const original = 'fn pisohypse_check() -> u8 {\n    1\n}\n';
  const approve = async batch => session.run(batch, { authorize: preview => JSON.stringify(JSON.parse(preview)) === JSON.stringify(batch) });
  let created = false;
  try {
    await session.run({ steps: [{ op: 'context.prompt' }] });
    const identity = session.session;
    await approve({ steps: [{ op: 'mutate.create', path, content: original }], validate: `test -f '${path}'` });
    created = true;
    const source = await session.run(read(path));
    const tag = source.output.match(/#([a-f0-9]{7,64})\]/)[1];
    const edit = { steps: [{ op: 'mutate.edit', path, tag, edits: [{ replace: { lines: [2, 2] }, with: '    2' }] }], validate: 'false' };
    await assert.rejects(approve(edit), /reverted|rollback/);
    assert.equal(readFileSync(join(workspace, path), 'utf8'), original);
    await approve({ ...edit, validate: `test -f '${path}'` });
    assert.match(readFileSync(join(workspace, path), 'utf8'), /    2/);
    await assert.rejects(approve({ ...edit, steps: [{ ...edit.steps[0], tag: 'deadbeefdeadbeef' }] }), /tag|baseline|object/);
    await session.run(read(path));
    assert.equal(session.session, identity);
    console.log('pisohypse live check passed: same session, tagged edit, failed-validation rollback, stale-tag rejection, recovery');
  } finally {
    await session.close();
    if (created) rmSync(join(workspace, path));
  }
}
