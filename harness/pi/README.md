# pisohypse

Non-MCP Pi extension over an adapter-owned `isohypse session --stdio` process.
The harness-neutral core is `../session.mjs`; Pi owns tool registration, user
approval, file-mutation queues, output limits, and teardown.

The signed CLI retains Isohypse's existing authenticated Unix-socket transport.

## Install and authorize

Review `manifest.json`, `pisohypse.ts`, and `../session.mjs` before loading.
An extension runs with Pi's OS privileges; this adapter is not a sandbox.
Installation alone grants no workspace access and starts no process.

For a one-invocation install:

```sh
pi -e /absolute/path/to/isohypse/harness/pi/pisohypse.ts
```

For persistent installation, explicitly add that absolute path to Pi's extension
settings, or place a loader in `~/.pi/agent/extensions/pisohypse.ts`:

```ts
export { default } from '/absolute/path/to/isohypse/harness/pi/pisohypse.ts';
```

Run `/reload`, then `/pisohypse connect`. `connect` always authorizes the
directory Pi is running in; after `connect` you may give a mode and an absolute
binary path, and nothing else. To work in a workspace you are not sitting in, use
`/pisohypse-remote <directory> [read|read-write|all] [absolute binary path]`.
Directories are `~`-expanded, resolved against the cwd, and `realpath`ed, so a
workspace reached through a symlink resolves to the root the daemon keyed its
sockets on. Both commands start the workspace daemon when it is not already
running — an unregistered directory gets its worker and index here, which is the
point of naming it — and the adapter then checks the authenticated worker's
`state.status` root against that canonical workspace.

Each connected workspace gets a slot: `local` for the cwd and the default, then
`remote1`, `remote2` … in the order they were connected. `/pisohypse status`
lists them, and `disconnect [slot]` revokes one or all — on either command, so
`/pisohypse-remote disconnect remote1` drops a remote slot without leaving the
workspace it is not sitting in. `status` works on both too. The tool
addresses a slot by name with its `target` field — never by path, so a tool call
carries no workspace name and an unconnected slot is refused. The default binary
is `~/bin/isohypse`; it must be the certificate-signed Isohypse build (macOS
codesign identifier `isohypse`, strict verification, no ad-hoc signature) and the
adapter fails closed before spawning anything else. The extension never builds,
signs, or reloads a daemon, changes signing, installs a binary, or widens
enforcement. `session.reconnect` may start a workspace daemon when its socket is
gone, under the scope already authorized.

The prompt identifies the binary and canonical workspace. Choose **Read only**,
**Read + ask before each edit and validation command**, or **Full read/write +
validation for this session**. Full read/write asks once at connect, not on every
edit, and includes validation shell execution. Denial is the default without a UI. A grant survives a reload: the transports are rebuilt but the session is the
same one, so the workspaces are re-opened automatically, by slot name. Grants
expire on /pisohypse disconnect, session switch, fork, or exit. The record is
sealed at rest in `~/.pi/pisohypse-grants.json` (AES-256-GCM, key in a 0600 file);
`PI_PISOHYPSE_PERSIST=0` turns persistence off. `/pisohypse status`
reports the session; `/pisohypse disconnect` revokes access and drains it.

## Tool

```json
{"steps":[
  {"op":"context.explore","arg":"build_and_reload"},
  {"op":"context.read","arg":"src/main.rs","range":"213-247"}
]}
```

Read `context.prompt` for the underlying tagged-anchor grammar. This adapter
accepts a strict static subset: context.read/find/explore/prompt and
mutate.edit/create. Edits support replace/insert/delete anchors, not file moves
or removal. Unknown operations/fields, workspace overrides, macros, raw jq,
absolute paths, parent traversal, and escaping symlinks are rejected.
`paths` (plural) is the search path-filter field, not `path`.

Every mutation batch still requires a nonempty `validate` shell command. In
ask-per-edit mode, a confirmation shows the exact batch; full read/write mode
authorizes edits and validation for the session without further prompts.
**Validation runs with daemon OS privileges; a workspace cwd is not a
shell/network/filesystem sandbox.** The connect prompt discloses this authority.
The approved snapshot is dispatched unchanged as a bound JSON value decoded by
daemon-evaluated jq. Static read grants never evaluate model-supplied jq.

Calls serialize through one session and edit batches share Pi's canonical
per-file mutation queue with its built-in tools. Daemon tags and transactions
remain authoritative; operation errors are tool errors, not successful output.
Failed validation retains Isohypse's rollback semantics and dirty baselines.

## Lifecycle and limits

Ordinary command failures leave the session usable. Cancellation sends
`session.cancel` and then closes/drains the session: the cancellation ACK alone
is not proof that rollback finished. Even after a successful drain, work may have
committed before cancellation won the race; inspect before retrying. A 30-second
drain timeout reports an
unconfirmed outcome; inspect the workspace before reconnecting. Requests have
a ten-minute timeout that follows the same cancellation path.

No write replay is automatic. Reconnect is a tool call, not operator work: send
`{"steps":[{"op":"session.reconnect"}]}` as the only step. The extension reuses
the scope already authorized for this session and starts the workspace daemon
when its socket is gone, so agents recover without a human. Reconcile failed
writes first, and call `context.prompt` again in the new session. Resume tokens
are neither returned to the model nor persisted by the adapter.

Batches allow 32 steps and 1 MiB of request JSON; responses cap at 16 MiB.
Pi output truncates at 50 KiB/2000 lines, retaining full text in a private temp
file when needed. Use bounded reads instead of retrieving huge outputs.

## Checks

```sh
node harness/tests/session.mjs
node harness/tests/session.mjs --live "$PWD" "$HOME/bin/isohypse"
cargo check --all-targets --release
```

The live check uses only the already-running workspace session worker. It creates
one PID-scoped test file under `harness/`, exercises tagged edits and rollback,
and removes only that file. It never installs/reloads or kills a daemon.
