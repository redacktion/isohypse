// Guards the validate-field gate: a missing validate must name itself, not report a NUL.
import assert from 'node:assert/strict';
import { mkdtempSync, realpathSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { prepareBatch } from '../session.mjs';

const workspace = realpathSync(mkdtempSync(join(tmpdir(), 'isohypse-gate-')));
writeFileSync(join(workspace, '.isohypse-probe.txt'), 'x\n');
const edit = {
  op: 'mutate.edit',
  path: '.isohypse-probe.txt',
  tag: '91e7cda',
  edits: [{ replace: { lines: [1, 1] }, with: 'ALPHA' }],
};

assert.throws(
  () => prepareBatch(workspace, { steps: [edit] }),
  /Edits require an explicit validation command/,
  'a mutation without validate names the missing validation command',
);
assert.throws(
  () => prepareBatch(workspace, { steps: [edit], validate: 'none' }),
  /Edits require an explicit validation command/,
  'validate "none" is refused too',
);
assert.ok(
  prepareBatch(workspace, { steps: [edit], validate: 'true' }).paths.length === 1,
  'a mutation with a validation command is accepted',
);
assert.throws(
  () => prepareBatch(workspace, { steps: [{ op: 'context.read', arg: '.isohypse-probe.txt' }], validate: 'true' }),
  /Read-only batches cannot run validation/,
  'a read-only batch may not carry a validation command',
);

rmSync(workspace, { recursive: true, force: true });
console.log('PASS: validate gate names itself and accepts a real command');
