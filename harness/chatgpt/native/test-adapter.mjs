import assert from 'node:assert/strict';
import { CodexAdapter, tool } from './adapter.mjs';
import { isStdioServer } from './cli.mjs';
import { mkdtempSync, realpathSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

assert.equal(isStdioServer(['-c', 'features.code_mode_host=true', 'app-server', '--analytics-default-enabled']), true);
assert.equal(isStdioServer(['--version']), false);
assert.equal(isStdioServer(['app-server', 'generate-json-schema']), false);
assert.equal(isStdioServer(['-c', 'app-server', 'exec']), false);
assert.throws(() => isStdioServer(['app-server', '--listen', 'unix://']), /stdio/);

const calls = [];
let opens = 0, closes = 0;
const adapter = new CodexAdapter({ workspace: process.cwd(), executable: process.execPath,
  open: async options => {
    opens++;
    assert.equal(options.allowEdits, false);
    return {
      async run(batch, { signal, authorize }) {
        calls.push(structuredClone(batch));
        assert.equal(await authorize(), false);
        if (batch.steps[0].arg === 'slow') {
          await new Promise((resolve, reject) => {
            signal.addEventListener('abort', () => reject(new Error('cancelled; drained')), { once: true });
          });
        }
        if (batch.steps[0].arg === 'error') throw new Error('stale tag');
        return { output: '[README.md#1234567]\n1:verified' };
      },
      async close() { closes++; },
    };
  },
});
const params = (callId, arg = 'README.md', threadId = 'one') => ({
  threadId, turnId: 'turn', callId, tool: tool.name, namespace: null,
  arguments: { steps: [{ op: 'context.read', arg }] },
});
assert.equal((await adapter.call(params('unknown'))).success, false);
adapter.bind('one'); adapter.bind('two');
assert.equal((await adapter.call({ ...params('foreign'), namespace: 'other' })).success, false);
const initial = params('first');
const first = adapter.call(initial);
initial.arguments.steps[0].arg = 'changed-after-dispatch';
assert.equal((await first).success, true);
assert.equal(calls[0].steps[0].arg, 'README.md');
assert.match((await adapter.call(params('first'))).contentItems[0].text, /not replayed/);
assert.equal((await adapter.call(params('failure', 'error'))).success, false);
assert.equal((await adapter.call(params('recovery'))).success, true);
assert.equal(opens, 1, 'normal operation failures retain one persistent transport');
assert.equal((await adapter.call(params('second-thread', 'README.md', 'two'))).success, true);
assert.equal(opens, 2, 'each thread owns its grant and transport');
const slow = adapter.call(params('cancel', 'slow'));
await new Promise(resolve => setImmediate(resolve));
adapter.cancel('two', 'turn');
adapter.cancel('one', 'unrelated-turn');
adapter.cancel('one', 'turn');
assert.match((await slow).contentItems[0].text, /cancelled/);
await adapter.unbind('one');
assert.equal((await adapter.call(params('revoked'))).success, false);
await adapter.close();
assert.equal(closes, 2);
assert.throws(() => adapter.bind('new'), /closed/);
console.log('Codex adapter checks passed: thread grants, immutable arguments, no replay, recovery, isolation, cancellation, teardown');

const secondRoot = realpathSync(mkdtempSync(join(tmpdir(), 'isohypse-workspace-')));
const openedRoots = [];
const global = new CodexAdapter({ executable: process.execPath, open: async ({ workspace }) => {
  openedRoots.push(workspace);
  return { run: async () => ({ output: workspace }), close: async () => {} };
} });
try {
  global.bind('one', process.cwd());
  global.bind('two', secondRoot);
  assert.throws(() => global.bind('one', secondRoot), /Cannot rebind/);
  assert.equal((await global.call(params('a'))).contentItems[0].text, realpathSync(process.cwd()));
  assert.equal((await global.call(params('b', 'README.md', 'two'))).contentItems[0].text, secondRoot);
  assert.deepEqual(openedRoots, [realpathSync(process.cwd()), secondRoot]);
  console.log('Workspace routing passed: two threads use distinct canonical roots and cannot rebind across projects');
} finally { await global.close(); rmSync(secondRoot, { recursive: true }); }

let reconnectOpens = 0;
const grants = [];
const reconnectAdapter = new CodexAdapter({ workspace: process.cwd(), executable: process.execPath, allowEdits: true,
  open: async options => {
    grants.push(options);
    const number = ++reconnectOpens;
    return {
      async run() {
        if (number === 1) throw new Error('Session unavailable; reconnect explicitly.');
        return { output: 'fresh session' };
      },
      async close() { if (number === 1) throw new Error('Transport lost; outcome unconfirmed'); },
    };
  },
});
reconnectAdapter.bind('one');
const reconnectParams = id => ({ ...params(id), arguments: { steps: [{ op: 'session.reconnect' }] } });
assert.equal((await reconnectAdapter.call(params('old-call'))).success, false);
assert.equal((await reconnectAdapter.call({ ...reconnectParams('mixed'), arguments: { steps: [{ op: 'session.reconnect' }, { op: 'context.prompt' }] } })).success, false);
assert.equal(reconnectOpens, 1);
const reconnected = await reconnectAdapter.call(reconnectParams('reconnect'));
assert.equal(reconnected.success, true);
assert.match(reconnected.contentItems[0].text, /outcome remains unconfirmed/);
assert.deepEqual(grants[0], grants[1], 'reconnect preserves host workspace, executable, and write grant');
assert.match((await reconnectAdapter.call(params('old-call'))).contentItems[0].text, /not replayed/);
assert.equal((await reconnectAdapter.call(params('fresh-call'))).contentItems[0].text, 'fresh session');
await reconnectAdapter.close();
assert.equal((await reconnectAdapter.call(reconnectParams('closed'))).success, false);
console.log('Reconnect passed: fresh session, same grant, explicit-only, no replay, no revival after close');

let maintenanceGrant = false, reloads = 0;
const maintenance = new CodexAdapter({ workspace: process.cwd(), executable: process.execPath, allowEdits: true,
  authorizeReload: () => maintenanceGrant,
  reload: async ({ workspace, executable }) => {
    assert.equal(workspace, realpathSync(process.cwd()));
    assert.equal(executable, realpathSync(process.execPath));
    reloads++; return 'signed handoff complete';
  },
});
maintenance.bind('one');
const reloadParams = id => ({ ...params(id), arguments: { steps: [{ op: 'session.reload' }] } });
assert.equal((await maintenance.call(reloadParams('denied'))).success, false);
maintenanceGrant = true;
assert.equal((await maintenance.call({ ...reloadParams('mixed-reload'), arguments: { steps: [{ op: 'session.reload', workspace: '/tmp' }] } })).success, false);
assert.equal((await maintenance.call(reloadParams('reload'))).success, true);
assert.equal((await maintenance.call(reloadParams('reload'))).success, false);
maintenanceGrant = false;
assert.equal((await maintenance.call(reloadParams('revoked-reload'))).success, false);
assert.equal(reloads, 1);
await maintenance.close();
console.log('Native maintenance passed: separate host grant, fixed workspace/executable, revocation and no replay');

