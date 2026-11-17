# Claude Code native adapter

Install into the regular `claude` command:

```sh
cargo run --release --quiet --manifest-path harness/claude/tools/Cargo.toml --bin install -- --allow-edits
claude
```

The installer patches the verified 2.1.263 executable's tool registry, retains
its original Bun runtime and module layout, and signs the local copy ad hoc.
It tests writes and resume before switching `~/.local/bin/claude`. Adapter
files live in Application Support; the checkout and extracted cache are not
needed at runtime. The original executable remains available for rollback:

```sh
cargo run --release --quiet --manifest-path harness/claude/tools/Cargo.toml --bin install -- --rollback
```

The install-time write grant still requires Claude's tool permission.
`ISOHYPSE_ALLOW_EDITS=0 claude` disables writes. Claude updates can replace
the launcher; newer binary versions require a reviewed adapter update. The
installer rejects unknown binary fingerprints instead of patching blindly.

## Development launcher

Run `./harness/claude/claude-isohypse --isohypse-allow-edits` from the Isohypse checkout, or use
the launcher's absolute path from another workspace. Ask Claude to use
`isohypse` to read `README.md`. The workspace must already have a live
Isohypse session endpoint.

This local copy of Claude Code 2.1.263 registers `isohypse` directly in the
built-in tool table. Its handler calls the shared signed SessionAdapter,
without MCP or Bash tool dispatch. Calls have native tool IDs and the
display name `Isohypse`. Claude's permission checks still apply.

The adapter supports reads, searches, exploration, prompts, validated writes,
and explicit `session.reconnect`. Omit `--isohypse-allow-edits` for read-only
access. The write grant comes from the launcher; Claude's tool permission is
also required. Plan mode refuses mutations. Validation runs with daemon privileges.

Resume normally with Claude's `--resume` option. The adapter opens a fresh
authenticated transport for the same task and canonical workspace. Reread
affected files before editing; old calls are never replayed. A private journal
under the Claude config directory rejects duplicate call IDs across process
restarts, including calls with unconfirmed outcomes. It stores the workspace
and hashed call IDs, not source contents or credentials. Claude shutdown drains
the adapter through its native cleanup registry.

Subagents and the interactive terminal rendering still need further verification.
This is an internal, version-specific integration, not an official plugin API.

The development launcher leaves the installed Claude executable unchanged. Extracted Anthropic code stays
in a private local cache and must not be committed or distributed. This
prototype does not establish redistribution or licensing permission.

Prepare a fresh private copy:

```sh
cargo run --release --quiet --manifest-path harness/claude/tools/Cargo.toml --bin prepare -- \
  "$HOME/.local/share/claude/versions/2.1.263" \
  "$HOME/Library/Caches/Isohypse/claude-native/2.1.263-native"
```

The extractor refuses an existing destination or a changed registry anchor.
It records the input executable's SHA256 in the cache's `provenance.txt`.
`ISOHYPSE_CLAUDE_ENTRY` overrides the launcher entry path;
`ISOHYPSE_EXECUTABLE` selects the signed Isohypse executable.

Verify against the actual Claude runtime with a loopback mock model:

```sh
ISOHYPSE_TEST_RESUME=1 \
CLAUDE_NATIVE_ENTRY="$HOME/Library/Caches/Isohypse/claude-native/2.1.263-native/cli" \
  cargo run --release --quiet --manifest-path harness/claude/tools/Cargo.toml --bin test-native
```

The probe checks native creation, process restart with Claude resume, duplicate
call rejection, a fresh tagged read, and an edit after resume.
It uses a temporary Claude config and fake local API credential. No paid
model request is made. Claude's reported cost is synthetic accounting.

Binary investigation also found REPL `registerTool()`. In this build it
registers VM functions but does not publish the returned `newTools` into
the next model request. The plugin `$.tool.register` route uses an internal
MCP registrar. Neither route satisfies this prototype's native non-MCP target.
