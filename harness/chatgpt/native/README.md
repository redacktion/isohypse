# Native Isohypse for Codex

The native app-server extension registers and executes `isohypse` inside Codex.
It works across desktop and remote connections, including resumed threads,
without MCP or client-supplied tool declarations. Each thread owns a persistent
signed Isohypse session bound to its canonical workspace.

Confirmed mutations appear as native file-change cards with diffs, retained in
thread history. Isohypse still performs the writes, fingerprint checks, and
validation. Failed or unconfirmed transactions do not produce completed edit
cards. Diffs describe managed mutation steps, not arbitrary validation-command
side effects; payloads over 8 MiB are omitted with a notice. This requires both
the updated daemon and native Codex build.

## Install

Native session opening verifies the signed executable, then starts the exact
harness-assigned workspace through `isohypse state up` when both sockets are
absent. Existing sockets are reused; partial startup gets a bounded readiness
wait. Symlinks, non-socket entries, authentication failures, and mismatched
workspace identities are refused. No parent-workspace fallback or automatic
replacement of stale sockets occurs. Startup has a 30-second command deadline
and a 30-second socket-readiness deadline. The authenticated session handshake
still verifies the worker's exact canonical root before any context operation.
Workspace workers remain managed by the existing supervisor after a thread ends.
The stdio-only bridge and Pi keep their existing startup behavior.

1. Build the matching Codex runtime (approximately 10 minutes on the first build):

   ```sh
   sh harness/chatgpt/codex-cli/build.sh
   ```

2. Install the runtime and adapter:

   ```sh
   node harness/chatgpt/native/install.mjs --allow-edits --native-binary \
     "$HOME/.isohypse/codex-native/out/codex"
   ```

3. Quit and reopen ChatGPT on the host Mac. Resume your task and ask:
   **Use isohypse to read README.md.**

The installer writes to `~/Library/Application Support/Isohypse/codex-adapter`
and installs `~/Library/LaunchAgents/com.isohypse.codex-adapter.plist`, which sets
`CODEX_CLI_PATH` for normal app launches. The app bundle stays unchanged.
Unrelated CLI commands use the original app binary. Reinstallation preserves
an existing native runtime unless a replacement is supplied. The installer
requires its version to match the app's binary; an app upgrade requires a
matching native build. Older installations that copied the app’s helper must
be upgraded with `--native-binary` pointing at the newly built bundle; missing
companions or notices cause installation to stop before updating files.

`--allow-edits` grants writes and validation shell execution with the daemon's
OS privileges, outside Codex's shell sandbox. Omit it for read-only access.
Each task uses its own workspace. `--workspace /absolute/project` optionally
restricts the installation; `--binary /absolute/isohypse` selects the signed
executable. Tool arguments cannot grant permissions or change the workspace.

Inspect installation with `node harness/chatgpt/native/install.mjs --status`.
Disable future loading with `node harness/chatgpt/native/install.mjs --disable`, then
restart the app. Installed files are retained.

## Automatic rebuild

An app update replaces the Codex runtime inside the app, so the native runtime must be rebuilt for the new version. Installing with `--native-binary` sets this up.

- A second LaunchAgent, `com.isohypse.codex-rebuild`, runs `autorebuild.mjs` at login and every 30 minutes.
- `autorebuild.mjs` compares `--version` of the app's runtime with the installed native runtime. On a difference it runs `codex-cli/build.sh` for the app's version (at low priority), then runs `install.mjs` with the same settings as the current install. A macOS notification reports the start and the result.
- The wrapper checks the same versions on every `app-server` launch. On a mismatch it starts the rebuild and passes the app through to its own runtime, so the app never runs a mismatched native build. Isohypse and SnapCompaction are unavailable until the rebuild finishes and the app restarts.
- A failed rebuild for a version is not retried for 6 hours. `node autorebuild.mjs --force` retries at once. State is in `~/.isohypse/codex-native/rebuild-state.json`, the agent log in `autorebuild.log`, and the build output in `build.log`, all in the same folder.
- The install remembers the SnapCompaction directory from `--snapcompax` or `SNAPCOMPAX_DIR`. The runtime location comes from `app-runtime.mjs`, which checks `Contents/Resources/codex-cli/bin/codex` and then `Contents/Resources/codex`.
- `--disable` also unloads the rebuild agent.
- Test: `node harness/chatgpt/native/test-autorebuild.mjs`.

## Tool calls

```json
{"steps":[
  {"op":"context.prompt"},
  {"op":"context.read","arg":"README.md","range":"1-20"}
]}
```

The static batch grammar is shared with [Pi](../pi/README.md):
`context.read/find/explore/log/prompt`, `mutate.edit/create`, tagged anchors,
workspace-relative paths, and an explicit `validate` command for mutations.
Raw jq, removals, traversal, and escaping live-read symlinks are rejected.
The native worker caps model-visible text at 24 KiB.

List retained revisions with `context.log`: optional `workspace` (absolute
recorded root), `arg` (exact relative path), `tag`, `since/until` (inclusive
Unix seconds), `max`, and `offset`. Time filters use saved-object modification
times, not inferred edit dates. Follow `next_offset` for another page.

Read saved bytes with `context.read`, `tag`, and an optional `workspace`,
`arg`, `range`, or `outline`. History workspace selection never changes the
live workspace or write grant. Retrieval preserves original tags and records
only displayed lines as seen. It never restores files or appends journal entries.

Sessions serialize calls and close on thread shutdown. A turn abort preserves an
idle session; an interrupted request closes and poisons its transport. A failed
or interrupted transport is never replayed automatically. Cancellation can
arrive after a write commits; inspect before retrying. Resuming a thread creates
a fresh runtime under the installation's current grant. Isohypse remains the
authority for tags, transactions, and rollback. Other Codex tools retain their
own execution paths and do not share Isohypse's locks.

After a disconnect or daemon reload, reconnect in the same task:

```json
{"steps":[{"op":"session.reconnect"}]}
```

Send this alone, without validation or edits. The harness replaces the session
under the same workspace and write grant, rechecks the signed executable, and
retains duplicate-call protection. It never replays an earlier call or treats
an uncertain mutation as rolled back. Read affected files before editing again.

For host-authorized maintenance in the Isohypse source workspace, send
`{"steps":[{"op":"session.reload"}]}` alone. It invokes the signed runtime's
build/sign/handoff path directly, without a shell or raw socket bypass.
The installed host config must grant `allowReloadFor` to this exact thread ID
and allow writes. This grant is checked on every reload and defaults to denied;
remove it after maintenance. Reconnect and inspect files after the handoff.

## Build provenance

`native/build.sh` downloads the official Codex `rust-v0.153.4` release source,
checks SHA-256 `74d988c0e154aad2b8d0cca4e950fc97fe2a29ff5ebe3b0070cce6d949c9a307`,
and applies `codex-cli`'s `prepare` step. Only app-server registration and the new
extension module are changed; source-less workspace lockfile versions are
normalized to the release version. No external dependency versions change.
The build uses Rust's release profile with optimization level 1 and no LTO.
Both the runtime and code-mode host are built from that public source release.
The build uses upstream’s helper to fetch and checksum-verify its matching V8
archive and bindings; `V8_FROM_SOURCE=1` selects upstream’s V8 source-build path.
The installer takes both from the same build directory and never copies the
app's code-mode host. LICENSE, NOTICE, and MODIFICATIONS accompany the installed
binaries; patched upstream files carry modification notices.

The upstream Apache-2.0 license and NOTICE are retained under `native/upstream`.
These cover upstream Codex; redistribution must also satisfy applicable bundled
dependency licenses. App-specific launching remains optional integration code,
not a claim of OpenAI endorsement or approval under the app's terms.

## Verification

```sh
node harness/chatgpt/native/test-adapter.mjs
node harness/chatgpt/native/test-install.mjs
node harness/chatgpt/native/test-live.mjs "$PWD" "$HOME/bin/isohypse" \
  "$HOME/Library/Application Support/Isohypse/codex-adapter/bin/codex" \
  --native --native-ws --all-workspaces --write-enabled --resume-native --file-changes \
  --installed-path "$HOME/Library/Application Support/Isohypse/codex-adapter/codex/codex"
```

The live test uses the installed launcher and a direct WebSocket client named
`codex_chatgpt_ios_remote`, with a deterministic localhost model fixture and
no external inference. It verifies server-owned registration, signed tagged
reads, writes with validation, file-change notifications and persisted diffs,
resumed threads, workspace isolation, and another client-owned tool. It removes
its own write fixture. This exercises the remote
connection architecture; actual mobile activation still requires the host app
to restart with the installed runtime.

Add `--interrupt-native` to the live command to interrupt model generation after
a completed Isohypse call and verify another read in the same loaded thread.

`node harness/chatgpt/native/test-startup.mjs "$HOME/bin/isohypse" /path/to/native/codex`
verifies native startup under a live parent workspace, exact-root reads, socket
reuse, and rejection of unsigned/wrong executables and unsafe socket entries.
It stops and removes only its own temporary workspace.

Native Rust tests cover idle-turn interruption, worker reuse, grant closure, and cancellation without
consuming a late response, including explicit reconnect without replay. Run in the prepared Codex source with
`just test --release -p codex-app-server --lib isohypse -j 6`.

## Stdio-only alternative

`proxy.mjs` remains available for hosts that own a desktop stdio connection.
Installing without `--native-binary` on a fresh installation selects this bridge.
It adds `thread/start.dynamicTools` and handles `item/tool/call`, but separate
remote connections bypass it and existing threads need new declarations.
Use the native runtime above for desktop plus remote access.
