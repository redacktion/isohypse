#!/usr/bin/env node
import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, writeFileSync, mkdirSync, renameSync, realpathSync, chmodSync, copyFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { randomUUID } from 'node:crypto';
import { parseArgs } from 'node:util';
import { tool } from './adapter.mjs';
import { locateRuntime } from './app-runtime.mjs';

const label = 'com.isohypse.codex-adapter';
const rebuildLabel = 'com.isohypse.codex-rebuild';
const REBUILD_INTERVAL_SECONDS = 1800;
const quote = value => `'${value.replaceAll("'", "'\\''")}'`;
const xml = value => value.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;').replaceAll("'", '&apos;');
function atomic(path, content, mode = 0o600) {
  const temp = `${path}.${randomUUID()}.tmp`;
  writeFileSync(temp, content, { mode, flag: 'wx' });
  renameSync(temp, path);
  chmodSync(path, mode);
}

// File preparation is separate from launchctl so it can be tested without
// modifying the user's login environment. Only known adapter files are written.
export function rebuildAgent({ root, node, log = join(homedir(), '.isohypse/codex-native/autorebuild.log') }) {
  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>${rebuildLabel}</string>
<key>ProgramArguments</key><array><string>${xml(node)}</string><string>${xml(join(root, 'codex', 'autorebuild.mjs'))}</string></array>
<key>RunAtLoad</key><true/>
<key>StartInterval</key><integer>${REBUILD_INTERVAL_SECONDS}</integer>
<key>ProcessType</key><string>Background</string>
<key>LowPriorityIO</key><true/>
<key>StandardOutPath</key><string>${xml(log)}</string>
<key>StandardErrorPath</key><string>${xml(log)}</string>
</dict></plist>\n`;
}

export function installFiles({ root, plist, config, node = process.execPath, previousOverride = null, nativeBinary, rebuildPlist = null }) {
  const marker = join(root, 'installation.json');
  if (existsSync(root) && !existsSync(marker)) throw new Error(`Refusing an unowned installation directory: ${root}`);
  const previous = existsSync(marker) ? JSON.parse(readFileSync(marker, 'utf8')) : null;
  if (previous && previous.id !== label) throw new Error('Installation directory belongs to another owner');
  if (existsSync(plist) && !previous) throw new Error(`Refusing an existing LaunchAgent: ${plist}`);
  // Validate the complete source-built bundle before changing installed files.
  const nativeFiles = ['codex-code-mode-host', 'LICENSE', 'NOTICE', 'MODIFICATIONS'];
  if (nativeBinary) for (const name of nativeFiles) {
    if (!existsSync(join(nativeBinary, '..', name))) throw new Error(`Missing source-build companion ${name}; run native/build.sh first`);
  }
  mkdirSync(root, { recursive: true, mode: 0o700 });
  mkdirSync(join(root, 'codex'), { recursive: true, mode: 0o700 });
  const source = fileURLToPath(new URL('.', import.meta.url));
  for (const name of ['adapter.mjs', 'proxy.mjs', 'cli.mjs', 'native-worker.mjs', 'app-runtime.mjs', 'autorebuild.mjs']) {
    // Installed bundles keep session.mjs one level above the harness files.
    const text = readFileSync(join(source, name), 'utf8').replaceAll("'../../session.mjs'", "'../session.mjs'");
    atomic(join(root, 'codex', name), text);
  }
  atomic(join(root, 'session.mjs'), readFileSync(join(source, '../../session.mjs')));
  if (nativeBinary) {
    const bin = join(root, 'bin');
    mkdirSync(bin, { recursive: true, mode: 0o700 });
    const target = join(bin, 'codex');
    const temp = target + '.' + randomUUID();
    copyFileSync(nativeBinary, temp); chmodSync(temp, 0o700); renameSync(temp, target);
    for (const name of nativeFiles) {
      const companion = join(nativeBinary, '..', name);
      const companionTemp = join(bin, name + '.' + randomUUID());
      copyFileSync(companion, companionTemp);
      chmodSync(companionTemp, name === 'codex-code-mode-host' ? 0o700 : 0o600);
      renameSync(companionTemp, join(bin, name));
    }
    config = { ...config, nativeBinary: target, nativeConfig: join(root, 'codex/native-config.json') };
    atomic(join(root, 'codex/tool-schema.json'), JSON.stringify(tool));
    atomic(config.nativeConfig, JSON.stringify({ node, worker: join(root, 'codex/native-worker.mjs'),
      adapterConfig: join(root, 'codex/config.json'), schema: join(root, 'codex/tool-schema.json') }, null, 2));
  }
  atomic(join(root, 'codex', 'config.json'), JSON.stringify(config, null, 2) + '\n');
  const wrapper = join(root, 'codex', 'codex');
  atomic(wrapper, `#!/bin/sh\nexec ${quote(node)} ${quote(join(root, 'codex', 'cli.mjs'))} "$@"\n`, 0o700);
  const agent = `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>Label</key><string>${label}</string>
<key>ProgramArguments</key><array><string>/bin/launchctl</string><string>setenv</string><string>CODEX_CLI_PATH</string><string>${xml(wrapper)}</string></array>
<key>RunAtLoad</key><true/>
</dict></plist>\n`;
  atomic(plist, agent);
  if (rebuildPlist) atomic(rebuildPlist, rebuildAgent({ root, node }));
  atomic(marker, JSON.stringify({ id: label, wrapper, plist, ...(rebuildPlist ? { rebuildPlist } : {}), previousOverride: previous ? previous.previousOverride : previousOverride }, null, 2) + '\n');
  return wrapper;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const { values } = parseArgs({ options: {
      workspace: { type: 'string' }, binary: { type: 'string' }, codex: { type: 'string' },
      'allow-edits': { type: 'boolean', default: false }, disable: { type: 'boolean' }, status: { type: 'boolean' },
      'native-binary': { type: 'string' }, snapcompax: { type: 'string' },
    } });
    if (process.platform !== 'darwin') throw new Error('Codex desktop installation requires macOS');
    const root = join(homedir(), 'Library/Application Support/Isohypse/codex-adapter');
    const plist = join(homedir(), 'Library/LaunchAgents', `${label}.plist`);
    const domain = `gui/${process.getuid()}`;
    const launchctl = (...args) => execFileSync('/bin/launchctl', args, { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'] }).trim();
    const current = launchctl('getenv', 'CODEX_CLI_PATH') || null;
    const marker = join(root, 'installation.json');
    if (values.status || values.disable) {
      if (!existsSync(marker)) throw new Error('Isohypse Codex adapter is not installed');
      const installed = JSON.parse(readFileSync(marker, 'utf8'));
      if (installed.id !== label) throw new Error('Invalid installation owner');
      if (values.disable) {
        try { launchctl('bootout', `${domain}/${label}`); } catch { /* may already be unloaded */ }
        launchctl('disable', `${domain}/${label}`);
        try { launchctl('bootout', `${domain}/${rebuildLabel}`); } catch { /* may already be unloaded */ }
        try { launchctl('disable', `${domain}/${rebuildLabel}`); } catch { /* not installed */ }
        if (current === installed.wrapper) {
          if (installed.previousOverride) launchctl('setenv', 'CODEX_CLI_PATH', installed.previousOverride);
          else launchctl('unsetenv', 'CODEX_CLI_PATH');
        }
        console.log('Adapter disabled for future app launches. Installed files retained; restart the app to unload the current adapter.');
      } else console.log(JSON.stringify({ installed: true, configuredForNextLaunch: current === installed.wrapper,
        wrapper: installed.wrapper, ...JSON.parse(readFileSync(join(root, 'codex/config.json'), 'utf8')) }, null, 2));
    } else {
      if (values.workspace !== undefined && !values.workspace.startsWith('/')) throw new Error('--workspace requires an absolute path');
      const expected = join(root, 'codex/codex');
      if (current && current !== expected) throw new Error(`CODEX_CLI_PATH already points to ${current}; refusing to replace another integration`);
      const config = { scope: values.workspace ? 'single-workspace' : 'per-task-workspace',
        ...(values.workspace ? { workspace: realpathSync(values.workspace) } : {}),
        executable: realpathSync(values.binary ?? join(homedir(), 'bin/isohypse')),
        codex: realpathSync(values.codex ?? locateRuntime() ?? (() => { throw new Error('No Codex runtime found in the ChatGPT app; pass --codex'); })()), allowEdits: values['allow-edits'] };
      const priorConfig = existsSync(marker) ? JSON.parse(readFileSync(join(root, 'codex/config.json'), 'utf8')) : {};
      const snapcompax = values.snapcompax ?? process.env.SNAPCOMPAX_DIR ?? priorConfig.rebuild?.snapcompax;
      config.rebuild = { build: fileURLToPath(new URL('../codex-cli/build.sh', import.meta.url)), installer: fileURLToPath(import.meta.url),
        ...(snapcompax ? { snapcompax: realpathSync(snapcompax) } : {}) };
      const nativeBinary = values['native-binary'] ? realpathSync(values['native-binary']) : priorConfig.nativeBinary;
      // Function-scoped: used both by the version gate below and by the summary printed
      // after installation, so it must not live inside the conditional block.
      const version = path => execFileSync(path, ['--version'], { encoding: 'utf8' }).trim();
      if (nativeBinary) {
        // The binary must match the runtime the app launches, which is what it replaces.
        // Comparing against a previously installed native runtime instead would pin the
        // installation to the first version ever installed and refuse every app upgrade.
        const runtimeVersion = version(config.codex);
        const candidateVersion = version(nativeBinary);
        if (candidateVersion !== runtimeVersion) {
          throw new Error('Native binary is ' + candidateVersion + ' but the active Codex runtime is ' + runtimeVersion);
        }
        if (priorConfig.nativeBinary && existsSync(priorConfig.nativeBinary)) {
          const installedVersion = version(realpathSync(priorConfig.nativeBinary));
          if (installedVersion !== runtimeVersion) {
            console.log('Replacing installed native runtime ' + installedVersion + ' (runtime is ' + runtimeVersion + ').');
          }
        }
      }
      mkdirSync(join(homedir(), 'Library/LaunchAgents'), { recursive: true });
      const rebuildPlist = join(homedir(), 'Library/LaunchAgents', `${rebuildLabel}.plist`);
      const priorRebuildPlist = existsSync(rebuildPlist) ? readFileSync(rebuildPlist, 'utf8') : null;
      const wrapper = installFiles({ root, plist, config, previousOverride: current, nativeBinary, rebuildPlist: nativeBinary ? rebuildPlist : null });
      execFileSync('/usr/bin/plutil', ['-lint', plist], { stdio: 'pipe' });
      try { launchctl('bootout', `${domain}/${label}`); } catch { /* first installation */ }
      launchctl('enable', `${domain}/${label}`);
      launchctl('bootstrap', domain, plist);
      if (nativeBinary) {
        const loaded = (() => { try { launchctl('print', `${domain}/${rebuildLabel}`); return true; } catch { return false; } })();
        if (!loaded || readFileSync(rebuildPlist, 'utf8') !== priorRebuildPlist) {
          execFileSync('/usr/bin/plutil', ['-lint', rebuildPlist], { stdio: 'pipe' });
          try { launchctl('bootout', `${domain}/${rebuildLabel}`); } catch { /* first installation */ }
          launchctl('enable', `${domain}/${rebuildLabel}`);
          launchctl('bootstrap', domain, rebuildPlist);
        }
      }
      launchctl('setenv', 'CODEX_CLI_PATH', wrapper);
      if (launchctl('getenv', 'CODEX_CLI_PATH') !== wrapper) throw new Error('Launch environment readback failed');
      const isohypseAccess = config.allowEdits ? 'read/write + validation' : 'read-only';
      const lines = [
        `Installed for normal Codex app launches (runtime ${version(config.codex)}):`,
        `  Isohypse ${nativeBinary ? 'tool ' : 'bridge'}    ${isohypseAccess}: ${config.workspace ?? 'each task uses its own workspace'}`,
      ];
      if (nativeBinary && readFileSync(join(nativeBinary, '..', 'MODIFICATIONS'), 'utf8').includes('SnapCompaction')) {
        lines.push('  SnapCompaction  PNG transcript replacement: prior context survives verbatim');
      }
      lines.push(
        'Restart the app once.',
        nativeBinary
          ? 'Existing threads can be resumed.'
          : 'Start a new desktop stdio thread; remote connections require --native-binary.',
        'No special launch command is needed.',
      );
      console.log(lines.join('\n'));
    }
  } catch (error) { console.error(error.message); process.exitCode = 1; }
}
