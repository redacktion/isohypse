import assert from 'node:assert/strict';
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { decide, installArguments } from './autorebuild.mjs';
import { findRuntime, locateRuntime, runtimeVersion } from './app-runtime.mjs';
import { nativeMatchesRuntime } from './cli.mjs';
import { rebuildAgent } from './install.mjs';

const temp = mkdtempSync(join(tmpdir(), 'isohypse-autorebuild-'));
const fake = (path, version) => { writeFileSync(path, `#!/bin/sh\necho "codex-cli ${version}"\n`); chmodSync(path, 0o755); };
try {
  const now = 1_000_000_000;
  assert.equal(decide({ runtime: null, native: 'a' }), 'no-runtime');
  assert.equal(decide({ runtime: 'codex-cli 1', native: 'codex-cli 1' }), 'current');
  assert.equal(decide({ runtime: 'codex-cli 2', native: 'codex-cli 1' }), 'rebuild');
  const failed = { version: 'codex-cli 2', status: 'failed', at: now - 1000 };
  assert.equal(decide({ runtime: 'codex-cli 2', native: 'codex-cli 1', state: failed, now }), 'backoff');
  assert.equal(decide({ runtime: 'codex-cli 2', native: 'codex-cli 1', state: failed, now, force: true }), 'rebuild');
  assert.equal(decide({ runtime: 'codex-cli 2', native: 'codex-cli 1', state: { ...failed, at: now - 7 * 3600_000 }, now }), 'rebuild');
  assert.equal(decide({ runtime: 'codex-cli 3', native: 'codex-cli 1', state: failed, now }), 'rebuild');

  const config = { allowEdits: true, workspace: '/w', rebuild: { installer: '/i.mjs', snapcompax: '/s' } };
  assert.deepEqual(installArguments({ config, runtimePath: '/app/codex', nativeBinary: '/out/codex' }),
    ['/i.mjs', '--native-binary', '/out/codex', '--codex', '/app/codex', '--allow-edits', '--workspace', '/w', '--snapcompax', '/s']);
  assert.deepEqual(installArguments({ config: { rebuild: { installer: '/i.mjs' } }, runtimePath: '/app/codex', nativeBinary: '/out/codex' }),
    ['/i.mjs', '--native-binary', '/out/codex', '--codex', '/app/codex']);

  const app = join(temp, 'Fake.app');
  mkdirSync(join(app, 'Contents/Resources/codex-cli/bin'), { recursive: true });
  assert.equal(findRuntime(app), null);
  fake(join(app, 'Contents/Resources/codex'), '1.0.0');
  assert.match(findRuntime(app), /Resources\/codex$/);
  fake(join(app, 'Contents/Resources/codex-cli/bin/codex'), '2.0.0');
  assert.match(findRuntime(app), /codex-cli\/bin\/codex$/);
  assert.equal(locateRuntime(join(temp, 'missing')), findRuntime());

  const native = join(temp, 'native'), runtime = join(temp, 'runtime');
  fake(native, '2.0.0'); fake(runtime, '2.0.0');
  assert.equal(runtimeVersion(native), 'codex-cli 2.0.0');
  assert.equal(nativeMatchesRuntime(native, runtime), true);
  fake(runtime, '2.1.0');
  assert.equal(nativeMatchesRuntime(native, runtime), false);
  assert.equal(nativeMatchesRuntime(join(temp, 'absent'), runtime), false);

  const plist = join(temp, 'rebuild.plist');
  writeFileSync(plist, rebuildAgent({ root: join(temp, "root with 'quote"), node: process.execPath, log: join(temp, 'log') }));
  execFileSync('/usr/bin/plutil', ['-lint', plist]);
  console.log('Auto-rebuild checks passed: decisions and backoff, install arguments, runtime discovery across app layouts, version match gate, valid LaunchAgent');
} finally { rmSync(temp, { recursive: true, force: true }); }
