pub const FORMAT_REFERENCE: &str = r#"# isohypse — op grammar v0.1

Every action is one op object: {"op":"<category.action>","arg":<primary>, ...fields}.
Categories declare effect: context reads, mutate writes, verify validates; state,
macro, and session are domains for the daemon and store, saved sequences, and the
wire. Full spec: isohypse.schema_v0.1.md (JSON Schema in isohypse.schema_v0.1.json).

Work in one loop:
  1. context.read a file -> a [path#TAG] header and numbered N:TEXT rows. TAG
     anchors every edit to the exact bytes you saw.
  2. mutate.edit against that tag, anchored by symbol, lines, or block.
  3. verify.* steps and the multi-op validate gate prove the change. A stale
     tag is refused or safely 3-way merged, never blindly overwritten.

## Ops

  context.read / context.find / context.explore (two args = call path) /
  context.log / context.prompt
  mutate.edit / mutate.create / mutate.undo
  verify.build / verify.check / verify.diagnose   (checkpoint:true runs mid-transaction)
  state.status / state.get / state.put / state.up / state.down / state.reload / state.stop
  macro.save / macro.run / macro.list
  session.open / session.command / session.cancel / session.reload /
  session.subscribe / session.close / session.request / session.requests

CLI form: isohypse <category> <action> [arg] — op bodies (mutate.edit, multi-op,
macro.save) arrive as JSON on stdin.

## mutate.edit — anchored edits

  {"op":"mutate.edit","path":"src/tag.rs","tag":"571a44f","edits":[
    {"replace":{"symbol":"FULL_TAG_LENGTH"},"with":"pub const FULL_TAG_LENGTH: usize = 32;"},
    {"replace":{"lines":[10,14]},"with":"    let x = compute();"},
    {"replace":{"block":42},"with":"fn a() -> u8 { 42 }"},
    {"insert":{"after":"$"},"body":"fn added() {}"},
    {"delete":{"symbol":"old_helper"}},
    {"move":{"to":"src/tag2.rs"}},
    {"remove":true}
  ]}

Anchors: {"symbol":NAME} is preferred — order-independent, no line math.
{"lines":[A,B]} is inclusive, numbered from your latest read of that tag.
{"block":N} is the whole construct beginning at line N. Insert anchors:
{"before":N|"^"} and {"after":N|"$"}. Actions: replace, insert, delete, move,
remove. "with" and "body" are plain strings holding only the final content.

  mutate.create: {"op":"mutate.create","path":"c.rs","content":"..."}  (fails if it exists)
  mutate.undo:   {"op":"mutate.undo","path":"c.rs","tag":"<prefix>","recover":false}

## multi-op — the transaction envelope

Ordered steps run atomically; any failed step, failed verify, or failed validate
reverts every mutation. Any mutate.* step REQUIRES a top-level "validate": a
shell command, or the literal "none" to waive it deliberately.

  {"op":"multi-op","payload":{
    "steps":[
      {"op":"context.read","arg":"a.rs"},
      {"op":"mutate.edit","path":"a.rs","tag":"tagA","edits":[
        {"replace":{"symbol":"first"},"with":"fn first() -> u8 { 1 }"}]},
      {"op":"verify.build","checkpoint":true}
    ],
    "validate":"cargo test"
  }}

CLI: isohypse multi-op reads that payload as JSON on stdin. Results come back
grouped by effect (context / mutate / verify); execution stays in step order.

## Session — open once, run many

isohypse session speaks line-delimited JSON both ways:

  1. {"do":"session.open","label":"my-agent"}
     -> {"ch":"ready","session":"my-agent.0007","token":"...","seq":1}
     Keep the token; reconnect with
     {"do":"session.open","label":"my-agent","resume":{"session":"my-agent.0007","token":"..."}}
  2. {"do":"session.command","id":1,"op":"context.read","payload":{"arg":"src/lib.rs"}}
     -> {"ch":"ack","id":1,...} then {"ch":"result","id":1,"output":"...","json":{...}}
     You pick each id; replies echo it. Pipeline several — replies may interleave.
  3. {"do":"session.cancel","id":1}   {"do":"session.reload"}   {"do":"session.close"}
  4. {"do":"session.subscribe","kinds":["file-changed","index-updated"],"workspaces":[]}
     -> unsolicited {"ch":"event",...}; a slow reader gets {"ch":"gap","dropped":N}.

session.request files a note for other agents; session.requests lists them.
Every frame carries a global "seq": one ordered timeline across the whole swarm.

## Rules

  - arg is the ONLY primary key. Other fields are named plainly: content,
    validate, tag, recover, range, outline, max_bytes, checkpoint, name, steps.
  - Tags come from your latest read; a mutate re-tags the file and the reply
    prints the new tag. Chain onto it. Prefer symbol anchors: no line math.
  - Bundle work: one multi-op or one session, never one CLI spawn per step.
  - On any error, READ THE OUTPUT — it names the cause (stale tag, path outside
    the workspace, no daemon). Never retry the same call blind.
  - Ops act on the repo at your cwd; -w '*' fans read-only ops across all live
    workspaces. --json returns structured output.
"#;
