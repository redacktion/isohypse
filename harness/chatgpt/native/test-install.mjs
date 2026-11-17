import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, rmSync, mkdirSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { installFiles } from './install.mjs';

const temp = mkdtempSync(join(tmpdir(), "isohypse install's "));
try {
  const root = join(temp, 'adapter'), plist = join(temp, 'agent.plist');
  const config = { workspace: process.cwd(), executable: process.execPath, codex: process.execPath, allowEdits: false };
  const wrapper = installFiles({ root, plist, config });
  assert.match(execFileSync(wrapper, ['--version'], { encoding: 'utf8' }), /^v\d/);
  execFileSync('/usr/bin/plutil', ['-lint', plist]);
  assert.deepEqual(JSON.parse(readFileSync(join(root, 'codex/config.json'), 'utf8')), config);
  installFiles({ root, plist, config: { ...config, allowEdits: true }, previousOverride: wrapper });
  assert.equal(JSON.parse(readFileSync(join(root, 'codex/config.json'), 'utf8')).allowEdits, true);
  assert.equal(JSON.parse(readFileSync(join(root, 'installation.json'), 'utf8')).previousOverride, null);
  const bundle = join(temp, 'source-build'); mkdirSync(bundle);
  const nativeBinary = join(bundle, 'codex'); writeFileSync(nativeBinary, 'native fixture');
  const beforeNative = readFileSync(join(root, 'codex/config.json'), 'utf8');
  assert.throws(() => installFiles({ root, plist, config, nativeBinary }), /Missing source-build companion/);
  assert.equal(readFileSync(join(root, 'codex/config.json'), 'utf8'), beforeNative);
  for (const name of ['codex-code-mode-host', 'LICENSE', 'NOTICE', 'MODIFICATIONS']) writeFileSync(join(bundle, name), `source-built ${name}`);
  installFiles({ root, plist, config, nativeBinary });
  for (const name of ['codex-code-mode-host', 'LICENSE', 'NOTICE', 'MODIFICATIONS']) assert.equal(readFileSync(join(root, 'bin', name), 'utf8'), `source-built ${name}`);
  const unrelated = join(temp, 'unrelated'); mkdirSync(unrelated); writeFileSync(join(unrelated, 'keep'), 'original');
  assert.throws(() => installFiles({ root: unrelated, plist, config }), /unowned/);
  assert.equal(readFileSync(join(unrelated, 'keep'), 'utf8'), 'original');
  console.log('Installer checks passed: paths with spaces/quotes, executable passthrough, valid LaunchAgent, repeat installation, ownership protection, source-built host and notice packaging');
} finally { rmSync(temp, { recursive: true, force: true }); }
