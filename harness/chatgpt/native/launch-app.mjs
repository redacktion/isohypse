#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import { realpathSync, accessSync, constants } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { parseArgs } from 'node:util';
import { findRuntime } from './app-runtime.mjs';

const { values } = parseArgs({ options: {
  workspace: { type: 'string' }, binary: { type: 'string' }, app: { type: 'string', default: '/Applications/ChatGPT.app' },
  'allow-edits': { type: 'boolean', default: false }, print: { type: 'boolean', default: false }, help: { type: 'boolean' },
} });
if (values.help) {
  console.log('node harness/chatgpt/native/launch-app.mjs --workspace /absolute/repo [--binary /signed/isohypse] [--allow-edits] [--print]\nLaunch the app with a non-MCP Isohypse native tool in new threads for this workspace. Read-only by default. --allow-edits grants edits and validation shell execution with daemon OS privileges. Quit the app yourself before launching; this command never terminates it. --print only displays the launch configuration.');
} else {
  try {
    if (process.platform !== 'darwin') throw new Error('This app launcher requires macOS');
    if (!values.workspace?.startsWith('/')) throw new Error('--workspace requires an absolute path');
    const app = realpathSync(values.app);
    const executable = realpathSync(values.binary ?? join(homedir(), 'bin', 'isohypse'));
    const cli = fileURLToPath(new URL('./codex', import.meta.url));
    accessSync(cli, constants.X_OK);
    const env = {
      CODEX_CLI_PATH: cli,
      ISOHYPSE_NODE: process.execPath,
      ISOHYPSE_WORKSPACE: realpathSync(values.workspace),
      ISOHYPSE_BINARY: executable,
      ISOHYPSE_CODEX_REAL: findRuntime(app) ?? (() => { throw new Error('No Codex runtime found in the app'); })(),
      ISOHYPSE_ALLOW_EDITS: values['allow-edits'] ? '1' : '0',
    };
    if (values.print) console.log(JSON.stringify({ app, env, activation: 'Quit the app, launch with these settings, then start a new thread in this workspace.' }, null, 2));
    else {
      const processes = execFileSync('/bin/ps', ['-axo', 'comm='], { encoding: 'utf8' });
      if (processes.split('\n').some(command => command.trim().startsWith(resolve(app, 'Contents/MacOS') + '/'))) {
        throw new Error('App is running. Quit it, then rerun this launcher. Existing threads cannot acquire a new dynamic tool; start a new thread after launch.');
      }
      execFileSync('/usr/bin/open', ['-a', app, ...Object.entries(env).flatMap(([key, value]) => ['--env', `${key}=${value}`])], { stdio: 'inherit' });
      console.log(`App launch requested with native Isohypse (${values['allow-edits'] ? 'read/write + validation' : 'read-only'}). Start a NEW thread in ${env.ISOHYPSE_WORKSPACE}.`);
    }
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
