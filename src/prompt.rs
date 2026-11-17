pub const HELP: &str = "Isohypse — read, explore, and edit through one persistent session.

Load the grammar first: call context.prompt. Until a session has read it, the
daemon refuses every other operation.
Read files to obtain tags; anchor edits to those tags.
Batch related operations and provide a validation command for edits.
List saved revisions with context.log; read one with context.read and its tag.
Use the returned tag for the next edit. Inspect failures before retrying.
After a disconnect or daemon reload, send session.reconnect alone, then reread
affected files. Codex and Pi both accept it, and neither needs operator action.

The harness owns workspace selection, authentication, and session lifetime.
";

pub const FORMAT_REFERENCE: &str = r#"# Isohypse

Use one persistent session for the task. The harness owns workspace selection,
authentication, and session lifetime. Send batches to the isohypse tool.

## Start here

Load the grammar before anything else:

{"steps":[{"op":"context.prompt"}]}

A session refuses every other operation until it has read the grammar. The
response documents tags, anchors, ranges, batching, and validation. Read it once
instead of guessing field names.


## Read and explore

{"steps":[
  {"op":"context.read","arg":"src/main.rs","range":"1-80"},
  {"op":"context.find","arg":"parse_request","paths":["src"],"max":10},
  {"op":"context.explore","arg":"parse_request","compact":true}
]}

- context.read: arg is a file; range is inclusive; outline:true shows structure. Add tag for saved source.
- context.find: arg is one pattern; paths/langs narrow scope; literal:true matches text literally.
- context.explore: arg is a symbol query; paths/langs narrow scope; compact/full control detail.
- context.prompt: returns this reference.

Use workspace-relative paths. Read only the ranges needed. Batch independent
lookups; wait for results before sending operations that depend on them.
Reads return [path#TAG] and numbered lines. Keep the tag with the bytes read.

## Saved history

{"steps":[
  {"op":"context.log","arg":"src/main.rs","max":20},
  {"op":"context.read","arg":"src/main.rs","tag":"TAG_FROM_HISTORY","range":"1-80"}
]}

context.log accepts optional arg (exact recorded path), workspace (absolute
history root), tag (prefix), since/until (inclusive Unix seconds), max (1–200),
and offset. Use next_offset for another page. Time filters use saved-object mtime,
not original edit time; old journals do not store per-revision timestamps.
With a time filter, entries without storage timestamps are excluded.

context.read with tag returns saved, hash-verified source and its original path.
arg is optional when the tag identifies one path in that workspace. Default
history reads show at most 120 lines. Only displayed lines count as seen for edits.
workspace selects retained history only; it never changes live reads or write
authority. Historical reads never restore files or append to the journal.
Read the current target before planning restoration; tagged edits and validation
remain required.


## Edit and validate

{"steps":[
  {"op":"mutate.edit","path":"src/main.rs","tag":"TAG_FROM_READ","edits":[
    {"replace":{"lines":[10,12]},"with":"replacement text"}
  ]}
],"validate":"cargo check"}

Each edit has one action:
- replace: anchor by {"symbol":"name"}, {"lines":[first,last]}, or {"block":start}; supply with.
- insert: anchor by {"before":line} or {"after":line}; supply body. Use "^"/"$" for file boundaries.
- delete: anchor by symbol, inclusive lines, or block.

mutate.create uses path and content; it refuses an existing file.
mutate.delete uses path alone; it refuses a directory and a file the object
store cannot hold, because the removed text is the only baseline a revert can
restore from.
Tags must come from a read. Edit only the region read; prefer symbol anchors
when replacing a whole definition. Successful edits return the next tag.

Mutations require the host's write grant and a nonempty validation command.
A failed batch or validation rolls back its managed edits. Validation runs with
the daemon's OS privileges; arbitrary shell side effects are not transactional.

## Failures

Stale tags are refused or merged against the recorded baseline. Inspect the
result before continuing. After cancellation or a transport failure, inspect
current files before submitting a new change; never replay an uncertain edit.
Acknowledge means accepted, not completed. Use the matching result or error.

After a disconnect or daemon reload, send this standalone batch:

{"steps":[{"op":"session.reconnect"}]}

Do not combine reconnect with other steps or validation. Codex and Pi both
accept it, and the agent runs it directly rather than asking the operator. Pi
keeps the scope already authorized for that session and opens a fresh socket,
starting the workspace daemon if the old one is gone. It opens a new session
under the same workspace and grant; it never replays earlier calls. The new
session has not read the grammar, so call context.prompt again. Afterward read
affected files before editing. An unconfirmed mutation stays unconfirmed until
inspected; reconnect does not undo it.

In the Isohypse source workspace, session.reload alone builds, signs, and
hot-reloads the daemon. It requires a separate host maintenance grant plus the
write grant; tool arguments cannot grant either. Reconnect afterward.

The tool accepts static steps and validate. It does not expose raw jq, workspace
switching, file moves, file removal, or other lifecycle commands. Do not invent fields.
"#;
