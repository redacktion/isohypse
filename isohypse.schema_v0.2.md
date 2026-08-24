# isohypse schema v0.2

The operation grammar for isohypse. Every action an agent takes is an **op object**. Ops group into categories by their effect on the world, and a transaction (`multi-op`) is a list of op objects the daemon runs atomically.

This file is the source of truth for the op surface. It is a draft (`v0.2`); shapes may change until `v1`. See the Changelog at the end for what moved between versions.

## Core concepts

**Op object.** One operation is one JSON object: `{"op":"<category.action>", ...fields}`. The `op` name carries its category as a prefix (`context.`, `mutate.`, `verify.`, `state.`, `macro.`, `session.`), so a step self-declares what it does and the daemon can enforce and group on it. The primary argument is `arg`; other inputs are named fields.

**Label.** Every op object — a lone op and every `multi-op` step — MUST carry a non-empty `label`, a short segment title. An unlabeled op is rejected. The daemon prints the label as the section header above that op's output, so a batch of reads or edits segments itself with no shell scaffolding.

**Effect categories.** `context` reads, `mutate` writes, `verify` validates. These three describe effect on the workspace. `state`, `macro`, and `session` are domains that act on the daemon, the macro store, and the connection.

**Tag invariant.** Every mutate carries the `tag` returned by the read that saw that file. If the file changed since, the tag no longer matches and the write is refused or three-way merged — never a blind clobber.

**Validate gate.** A `multi-op` containing any `mutate.*` step MUST declare a top-level `validate`, either a shell command to run or the literal `"none"`. This forces every change to state how it is verified.

## context — read and ingest

Read-only. Gathers information; never changes anything. Needs no `validate`.

```
context.read      read a file, whole or by range/symbol
context.find      text search across tracked source
context.explore   symbols, callers, blast radius; two args = call path between them
context.log       recorded version history of a file
context.prompt    this schema reference
```

```json
{"op":"context.read","arg":"src/tag.rs","range":"1-40","label":"tag constants"}
{"op":"context.explore","arg":"full_tag","label":"full_tag callers"}
```

## mutate — writes

Changes files. In a `multi-op`, any mutate obliges a top-level `validate`. `mutate.edit` is anchored — you address the target by `symbol`, `lines`, or `block`, never by a hand-encoded patch string.

```
mutate.edit     replace / insert / delete / move / remove at a typed anchor
mutate.create   author a new file (fails if it exists)
mutate.undo     restore a prior recorded version
```

Anchor an edit by symbol (preferred — order-independent, no line math):

```json
{"op":"mutate.edit","path":"src/tag.rs","tag":"571a44f","label":"widen tag length","edits":[
  {"replace":{"symbol":"FULL_TAG_LENGTH"},
   "with":"pub const FULL_TAG_LENGTH: usize = 32;"}
]}
```

Other anchors and actions (the fragments inside `edits`, not full ops, so they carry no label):

```json
{"replace":{"lines":[10,14]},"with":"    let x = compute();\n    return x;"}
{"replace":{"block":42},"with":"fn a() -> u8 { 42 }"}
{"insert":{"after":"$"},"body":"\n#[cfg(test)]\nmod tests {}"}
{"delete":{"symbol":"old_helper"}}
{"move":{"to":"src/tag2.rs"}}
{"remove":true}
```

Create:

```json
{"op":"mutate.create","path":"src/new.rs","content":"fn c() {}\n","label":"add new module"}
```

## verify — in-tool validation

Runs a check as a step. Mark `checkpoint:true` to run it mid-transaction; a failure reverts the whole `multi-op`.

```
verify.build      run the workspace's detected build
verify.check      validate a proposed edit end to end without writing
verify.diagnose   report parse errors in a file
```

```json
{"op":"verify.build","checkpoint":true,"label":"build check"}
```

## state — daemon, store, lifecycle

Introspection, the content-addressed object store, and daemon lifecycle.

```
state.status    daemon and index state
state.get       fetch a stored blob by tag
state.put       store a blob, returns its tag
state.up        start / ensure the daemon (flag: reindex)
state.down      remove a workspace from the live tree
state.reload    gap-free handover onto a fresh binary
state.stop      stop the tree (password-gated when workspaces are live)
```

`worker`, `services`, and `daemon` are internal spawn targets the supervisor invokes; they are not part of the user surface.

```json
{"op":"state.status","label":"daemon state"}
{"op":"state.put","content":"handle payload","label":"stash payload"}
{"op":"state.up","path":"/repo","label":"start /repo"}
```

## macro — named replayable step sequences

A macro is a saved list of op objects, replayed by name in whatever workspace you run it from. Deterministic, reusable building blocks.

```
macro.save   save a steps array under a name
macro.run    replay a saved macro
macro.list   list saved macros
```

```json
{"op":"macro.save","name":"lint-fix","steps":[ /* op objects */ ],"label":"save lint-fix"}
{"op":"macro.run","name":"lint-fix","label":"run lint-fix"}
```

## session — connect layer and inter-agent notes

The wire protocol for a persistent connection, plus notes agents leave each other. Open once, run many; replies stream back tagged by `id`. A carried op in `session.command` follows the op rules, so its payload needs a `label`.

```
session.open        open or resume a session; returns a token
session.command     run one op object over the session
session.cancel      cancel an in-flight command by id
session.reload      reset this session (clears stale in-flight, re-binds events)
session.subscribe   receive pushed events instead of polling
session.close       end the session
session.request     leave a note for another agent
session.requests    list outstanding notes
```

```json
{"do":"session.open","label":"my-agent"}
{"do":"session.command","id":1,"op":"context.read","payload":{"arg":"src/tag.rs","label":"tag file"}}
{"do":"session.reload"}
```

## multi-op — the transaction envelope

A `multi-op` is an ordered list of op objects run atomically: any failed step, failed verify, or errored read reverts every mutation. Steps execute in the order given; the result comes back grouped by category for tracking. Each step is an op object, so each carries its own `label`. Any `mutate.*` step requires a top-level `validate` (a command or `"none"`).

Normal edit, verified:

```json
{"op":"multi-op","payload":{
  "steps":[
    {"op":"context.read","arg":"src/tag.rs","label":"read tag"},
    {"op":"mutate.edit","path":"src/tag.rs","tag":"571a44f","label":"widen tag","edits":[
      {"replace":{"symbol":"FULL_TAG_LENGTH"},
       "with":"pub const FULL_TAG_LENGTH: usize = 32;"}]},
    {"op":"verify.build","checkpoint":true,"label":"build"}
  ],
  "validate":"cargo test"
}}
```

Multi-file, each change verified before the next runs:

```json
{"op":"multi-op","payload":{
  "steps":[
    {"op":"context.read","arg":"a.rs","label":"read a"},
    {"op":"context.read","arg":"b.rs","label":"read b"},
    {"op":"mutate.edit","path":"a.rs","tag":"tagA","label":"edit a","edits":[
      {"replace":{"symbol":"first"},"with":"fn first() -> u8 { 1 }"}]},
    {"op":"verify.build","checkpoint":true,"label":"build after a"},
    {"op":"mutate.edit","path":"b.rs","tag":"tagB","label":"edit b","edits":[
      {"replace":{"symbol":"second"},"with":"fn second() -> u8 { 2 }"}]},
    {"op":"verify.build","checkpoint":true,"label":"build after b"}
  ],
  "validate":"cargo test"
}}
```

Doc edit, no build path, explicit waiver:

```json
{"op":"multi-op","payload":{
  "steps":[
    {"op":"context.read","arg":"README.md","label":"read readme"},
    {"op":"mutate.edit","path":"README.md","tag":"c3a8b72","label":"fix line","edits":[
      {"replace":{"lines":[10,10]},"with":"updated line"}]}
  ],
  "validate":"none"
}}
```

Rejected — mutate present, no `validate`:

```json
{"ok":false,"error":"multi-op has mutate steps but no top-level \"validate\" (a command or \"none\"); declare how this change is verified"}
```

Rejected — op with no `label`:

```json
{"ok":false,"error":"op needs a \"label\" (a short segment title); every op and multi-op step must carry one"}
```

Result, grouped by category (execution stayed in order):

```json
{"ch":"result","id":1,"json":{
  "op":"multi-op","transactional":true,
  "context":[{"op":"context.read","label":"read tag","tag":"571a44f","lines":40}],
  "mutate":[{"op":"mutate.edit","label":"widen tag","tag":"9f2c1ab","edits":1}],
  "verify":[{"op":"verify.build","label":"build","ok":true}]
}}
```

## Changelog

**v0.2 (current draft).** `label` is now required on every op object and every `multi-op` step; a missing or empty `label` is rejected. The daemon renders each `label` as the section header above that op's output, so batched reads and edits segment themselves with no shell scaffolding.

**v0.1.** Initial op grammar: `category.action` names, effect prefixes (`context`/`mutate`/`verify`/`state`/`macro`/`session`), `arg` as the only primary key, the anchored `mutate.edit` schema (`symbol`/`lines`/`block`), and the `multi-op` envelope with the validate gate.
