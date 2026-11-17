import { spawn } from 'node:child_process';
import { realpathSync, readFileSync, existsSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { serve } from './proxy.mjs';
import { locateRuntime, runtimeVersion } from './app-runtime.mjs';

export function isStdioServer(args, { requireStdio = true } = {}) {
  // Skip global options with values before identifying the command.
  const valued = new Set(['-c', '--config', '--enable', '--disable', '--remote', '--remote-auth-token-env', '-i', '--image', '-m', '--model', '--local-provider', '-p', '--profile', '-s', '--sandbox', '-C', '--cd', '--add-dir', '-a', '--ask-for-approval']);
  let command = -1;
  for (let index = 0; index < args.length; index++) {
    if (valued.has(args[index])) { index++; continue; }
    if (args[index].startsWith('-')) continue;
    command = index; break;
  }
  if (args[command] !== 'app-server') return false;
  const tail = args.slice(command + 1);
  // CLI protocol generators and daemon management are ordinary passthroughs.
  if (['daemon', 'proxy', 'generate-ts', 'generate-json-schema', 'help'].includes(tail[0]) || tail.includes('--help') || tail.includes('-h')) return false;
  for (let index = 0; index < tail.length; index++) {
    const value = tail[index] === '--listen' ? tail[index + 1] : tail[index].startsWith('--listen=') ? tail[index].slice(9) : undefined;
    if (requireStdio && value !== undefined && value !== 'stdio://') throw new Error('Isohypse Codex bridge supports stdio app-server connections only');
  }
  return true;
}

export function nativeMatchesRuntime(nativeBinary, codex) {
  const native = runtimeVersion(nativeBinary);
  return native !== null && native === runtimeVersion(codex);
}

function startRebuild() {
  const script = fileURLToPath(new URL('./autorebuild.mjs', import.meta.url));
  if (!existsSync(script)) return;
  spawn(process.execPath, [script], { detached: true, stdio: 'ignore' }).unref();
}

if (process.argv[1] && realpathSync(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const configPath = new URL('./config.json', import.meta.url);
    const config = existsSync(configPath) ? JSON.parse(readFileSync(configPath, 'utf8')) : {};
    const codex = process.env.ISOHYPSE_CODEX_REAL ? realpathSync(process.env.ISOHYPSE_CODEX_REAL) : locateRuntime(config.codex);
    if (!codex) throw new Error('No Codex runtime found in the ChatGPT app');
    if (codex === realpathSync(fileURLToPath(new URL('./codex', import.meta.url)))) throw new Error('Codex executable points back to the Isohypse wrapper');
    const args = process.argv.slice(2);
    const passthrough = () => {
      const child = spawn(codex, args, { stdio: 'inherit', shell: false });
      for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(signal, () => child.kill(signal));
      child.once('error', error => { console.error(error.message); process.exitCode = 1; });
      child.once('exit', code => { process.exitCode = code ?? 1; });
    };
    if (isStdioServer(args, { requireStdio: !config.nativeBinary }) && config.nativeBinary) {
      if (nativeMatchesRuntime(config.nativeBinary, codex)) {
        const child = spawn(config.nativeBinary, args, { stdio: 'inherit', shell: false,
          env: { ...process.env, ISOHYPSE_NATIVE_CONFIG: config.nativeConfig } });
        for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) process.on(signal, () => child.kill(signal));
        child.once('error', error => { console.error(error.message); process.exitCode = 1; });
        child.once('exit', code => { process.exitCode = code ?? 1; });
      } else {
        startRebuild();
        passthrough();
      }
    } else if (isStdioServer(args)) {
      const grant = process.env.ISOHYPSE_ALLOW_EDITS ?? (config.allowEdits === true ? '1' : '0');
      if (!['0', '1'].includes(grant)) throw new Error('ISOHYPSE_ALLOW_EDITS must be 0 or 1');
      await serve({ workspace: process.env.ISOHYPSE_WORKSPACE ?? config.workspace, executable: process.env.ISOHYPSE_BINARY ?? config.executable,
        codex, allowEdits: grant === '1', serverArgs: args });
    } else passthrough();
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
