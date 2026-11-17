import { execFileSync } from 'node:child_process';
import { existsSync, realpathSync } from 'node:fs';
import { join } from 'node:path';

export const APP = '/Applications/ChatGPT.app';
const RELATIVE_RUNTIMES = ['Contents/Resources/codex-cli/bin/codex', 'Contents/Resources/codex'];
const VERSION_TIMEOUT_MS = 20000;

export function findRuntime(app = APP) {
  for (const relative of RELATIVE_RUNTIMES) {
    const candidate = join(app, relative);
    if (existsSync(candidate)) return realpathSync(candidate);
  }
  return null;
}

export function locateRuntime(preferred) {
  if (preferred && existsSync(preferred)) return realpathSync(preferred);
  return findRuntime();
}

export function runtimeVersion(path) {
  try {
    return execFileSync(path, ['--version'], { encoding: 'utf8', timeout: VERSION_TIMEOUT_MS, stdio: ['ignore', 'pipe', 'ignore'] }).trim() || null;
  } catch {
    return null;
  }
}
