import { readFileSync } from 'node:fs';
import { CodexAdapter } from './adapter.mjs';
import { createHash } from 'node:crypto';

// One worker per app-server thread. Its stdin belongs to the native extension,
// not to a desktop/mobile client. The signed Isohypse transport remains owned
// by CodexAdapter and retains its validation, cancellation, and drain rules.
const [workspace, configPath, threadId] = process.argv.slice(2);
const config = JSON.parse(readFileSync(configPath, 'utf8'));
const adapter = new CodexAdapter({ workspace: config.workspace, executable: config.executable, allowEdits: config.allowEdits,
  mutationReports: true,
  authorizeReload: () => JSON.parse(readFileSync(configPath, 'utf8')).allowReloadFor === threadId,
  open: async options => {
    const source = new URL('../../session.mjs', import.meta.url);
    const revision = createHash('sha256').update(readFileSync(source)).digest('hex');
    source.searchParams.set('revision', revision);
    const { SessionAdapter } = await import(source.href);
    return SessionAdapter.open({ ...options, autoStart: true });
  } });
adapter.bind(threadId, workspace);
let closing;
const close = () => closing ??= adapter.close().finally(() => process.stdin.pause());
const send = value => process.stdout.write(JSON.stringify(value) + '\n');
let buffer = '';
process.stdin.setEncoding('utf8').on('data', chunk => {
  buffer += chunk;
  // Just under the daemon's MAX_REQUEST_BYTES (8 MiB), so the daemon's own limit
  // is the one that speaks instead of a client cap the model cannot see.
  if (Buffer.byteLength(buffer) > 8 * 1024 * 1024 - 65536) {
    send({ success: false, error: 'Native request exceeds the 8 MiB limit; split it across batches' });
    void close(); return;
  }
  let end;
  while ((end = buffer.indexOf('\n')) >= 0) {
    const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
    let request;
    try { request = JSON.parse(line); } catch { send({ success: false, error: 'Invalid native request' }); continue; }
    if (request.close === true) { void close().then(() => process.exit(0), error => { console.error(error.message); process.exit(1); }); continue; }
    if (closing) { send({ success: false, error: 'Native worker closing; no replay' }); continue; }
    void adapter.call({ ...request, threadId, tool: 'isohypse', namespace: null }).then(result => {
      let text = result.contentItems.map(item => item.text).join('\n');
      // Keep each model context fragment below 10K tokens, including overhead.
      if (Buffer.byteLength(text) > 24 * 1024) text = Buffer.from(text).subarray(0, 24 * 1024).toString('utf8') + '\n[Native output truncated; request a narrower range.]';
      // UI payload is separate from the model's bounded text output.
      let mutations = result.mutations ?? [];
      if (Buffer.byteLength(JSON.stringify(mutations)) > 8 * 1024 * 1024) {
        mutations = [];
        text += '\n[Edit completed; native diff exceeds 8 MiB and was omitted.]';
      }
      send({ success: result.success, output: text, mutations });
    });
  }
});
process.stdin.on('end', () => { void close().catch(error => { console.error(error.message); process.exitCode = 1; }); });
