// Explicit live check: load with pi -e, then invoke /pisohypse-grant-check.
import assert from 'node:assert/strict';
import { existsSync, readFileSync, rmSync } from 'node:fs';
import { join } from 'node:path';
import type { ExtensionAPI } from '@earendil-works/pi-coding-agent';
import pisohypse from './pisohypse.ts';

export default function (pi: ExtensionAPI) {
  pi.registerCommand('pisohypse-grant-check', {
    description: 'Test all grants against this live workspace using one temporary file',
    handler: async (_args, ctx) => {
      let command: any, tool: any, shutdown: any;
      let choice = 'Deny', confirmations = 0, selections = 0;
      const messages: string[] = [];
      pisohypse({
        registerCommand: (_name: string, definition: any) => { command = definition; },
        registerTool: (definition: any) => { tool = definition; },
        on: (_event: string, handler: any) => { shutdown = handler; },
      } as unknown as ExtensionAPI);
      const testContext = { ...ctx, hasUI: true, ui: { ...ctx.ui,
        select: async (title: string, options: string[]) => {
          assert.match(title, /NOT sandboxed/);
          assert.ok(options.includes('Full read/write + validation for this session'));
          selections++; return choice;
        },
        confirm: async () => { confirmations++; return false; },
        notify: (text: string) => { messages.push(text); },
      } };
      const path = `harness/pisohypse-grant-check-${process.pid}.rs`;
      const original = 'fn grant_check() -> u8 {\n    1\n}\n';
      const create = { steps: [{ op: 'mutate.create', path, content: original }], validate: `test -f '${path}'` };
      const call = (input: unknown) => tool.execute('grant-check', input, undefined, undefined, testContext);
      let created = false;
      assert.ok(!existsSync(join(ctx.cwd, path)));
      try {
        await assert.rejects(command.handler('connect', { ...testContext, hasUI: false }), /interactive authorization UI/);
        for (choice of ['Deny', 'unexpected grant']) {
          await command.handler('connect', testContext);
          await assert.rejects(call(create), /authorize this workspace/);
        }
        choice = 'Read only';
        await command.handler('connect', testContext);
        await assert.rejects(call(create), /Read-only/);
        assert.equal(confirmations, 0);
        await command.handler('disconnect', testContext);
        choice = 'Full read/write + validation for this session';
        await command.handler('connect', testContext);
        const selected = selections;
        await call(create); created = true;
        const source = await call({ steps: [{ op: 'context.read', arg: path }] });
        const tag = source.content[0].text.match(/#([a-f0-9]{7,64})\]/)[1];
        const edit = { steps: [{ op: 'mutate.edit', path, tag, edits: [{ replace: { lines: [2, 2] }, with: '    2' }] }], validate: 'false' };
        await assert.rejects(call(edit), /reverted|rollback/);
        assert.equal(readFileSync(join(ctx.cwd, path), 'utf8'), original);
        await call({ ...edit, validate: `test -f '${path}'` });
        assert.match(readFileSync(join(ctx.cwd, path), 'utf8'), /    2/);
        assert.equal(confirmations, 0, 'full-session edits must not prompt again');
        assert.equal(selections, selected);
        await command.handler('status', testContext);
        assert.ok(messages.at(-1)?.includes(choice));
        await shutdown();
        await assert.rejects(call(create), /authorize this workspace/);
        choice = 'Read + ask before each edit and validation command';
        await command.handler('connect', testContext);
        await assert.rejects(call(create), /not authorized/);
        assert.equal(confirmations, 1, 'full grant must not leak into a new ask-per-edit session');
        ctx.ui.notify('PISOHYPSE_GRANTS_PASS: full-session edits without prompts, rollback, read-only, per-edit, revocation', 'info');
      } finally {
        await shutdown();
        if (created) rmSync(join(ctx.cwd, path));
      }
    },
  });
}
