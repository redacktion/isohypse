#!/usr/bin/env node
import { spawnSync } from 'node:child_process';
import { existsSync, mkdirSync, openSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { locateRuntime, runtimeVersion } from './app-runtime.mjs';

const RETRY_AFTER_MS = 6 * 60 * 60 * 1000;
const BUILD_PATH = [join(homedir(), '.cargo/bin'), '/opt/homebrew/bin', '/usr/local/bin', '/usr/bin', '/bin', '/usr/sbin', '/sbin'].join(':');

export function decide({ runtime, native, state, now = Date.now(), force = false }) {
  if (!runtime) return 'no-runtime';
  if (native === runtime) return 'current';
  if (!force && state?.version === runtime && state.status === 'failed' && now - state.at < RETRY_AFTER_MS) return 'backoff';
  return 'rebuild';
}

export function installArguments({ config, runtimePath, nativeBinary }) {
  return [
    config.rebuild.installer,
    '--native-binary', nativeBinary,
    '--codex', runtimePath,
    ...(config.allowEdits ? ['--allow-edits'] : []),
    ...(config.workspace ? ['--workspace', config.workspace] : []),
    ...(config.rebuild.snapcompax ? ['--snapcompax', config.rebuild.snapcompax] : []),
  ];
}

function readJson(path) {
  try {
    return JSON.parse(readFileSync(path, 'utf8'));
  } catch {
    return null;
  }
}

function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function takeLock(lock) {
  try {
    mkdirSync(lock);
  } catch {
    const holder = Number(readFileSync(join(lock, 'pid'), 'utf8').trim());
    if (alive(holder)) return false;
    rmSync(lock, { recursive: true, force: true });
    mkdirSync(lock);
  }
  writeFileSync(join(lock, 'pid'), String(process.pid));
  return true;
}

function notify(message) {
  spawnSync('/usr/bin/osascript', ['-e', `display notification ${JSON.stringify(message)} with title "Isohypse"`], { stdio: 'ignore' });
}

function logLine(text) {
  process.stdout.write(`${new Date().toISOString()} ${text}\n`);
}

function rebuild({ config, runtimePath, runtime, work }) {
  const log = openSync(join(work, 'build.log'), 'w');
  const token = runtime.split(/\s+/).pop();
  const env = {
    ...process.env,
    PATH: BUILD_PATH,
    HOME: homedir(),
    ISOHYPSE_CODEX_VERSION: token,
    ISOHYPSE_APP_CODEX: runtimePath,
    ...(config.rebuild.snapcompax ? { SNAPCOMPAX_DIR: config.rebuild.snapcompax } : {}),
  };
  const built = spawnSync('/usr/bin/nice', ['-n', '15', '/bin/sh', config.rebuild.build], { env, stdio: ['ignore', log, log] });
  if (built.status !== 0) return `build failed (exit ${built.status}); see ${join(work, 'build.log')}`;
  const installed = spawnSync(process.execPath, installArguments({ config, runtimePath, nativeBinary: join(work, 'out/codex') }), { env, stdio: ['ignore', log, log] });
  return installed.status === 0 ? null : `install failed (exit ${installed.status}); see ${join(work, 'build.log')}`;
}

function main() {
  const work = join(homedir(), '.isohypse/codex-native');
  const root = join(homedir(), 'Library/Application Support/Isohypse/codex-adapter');
  const config = readJson(join(root, 'codex/config.json'));
  if (!config?.nativeBinary || !config.rebuild) return logLine('not installed with a rebuild configuration');
  const runtimePath = locateRuntime(config.codex);
  const runtime = runtimePath ? runtimeVersion(runtimePath) : null;
  const statePath = join(work, 'rebuild-state.json');
  const decision = decide({ runtime, native: runtimeVersion(config.nativeBinary), state: readJson(statePath), force: process.argv.includes('--force') });
  if (decision !== 'rebuild') return logLine(`${decision}: app runtime ${runtime ?? 'not found'}`);
  mkdirSync(work, { recursive: true });
  if (!takeLock(join(work, 'rebuild.lock'))) return logLine('rebuild already running');
  try {
    logLine(`rebuilding native runtime for ${runtime}`);
    notify(`Rebuilding the native runtime for ${runtime}. Isohypse and SnapCompaction are off until it finishes.`);
    const failure = rebuild({ config, runtimePath, runtime, work });
    writeFileSync(statePath, JSON.stringify({ version: runtime, status: failure ? 'failed' : 'installed', at: Date.now(), ...(failure ? { detail: failure } : {}) }, null, 2));
    logLine(failure ?? `installed native runtime ${runtime}`);
    notify(failure ?? `Native runtime ${runtime} installed. Restart the app to use it.`);
    if (failure) process.exitCode = 1;
  } finally {
    rmSync(join(work, 'rebuild.lock'), { recursive: true, force: true });
  }
}

if (process.argv[1] && existsSync(process.argv[1]) && fileURLToPath(import.meta.url) === process.argv[1]) main();
