# Shell harness

Any agent that can run shell commands can use Isohypse through two CLI commands. No MCP, no client patching, no runtime. Both commands ship in the `isohypse` binary, so they work with every client version.

## `isohypse call`

Runs one batch in a fresh session and prints the result.

```bash
isohypse call <<'EOF'
{"steps":[{"op":"context.read","arg":"src/main.rs","range":"1-80"}]}
EOF
```

- Input is a JSON batch on stdin, or as the first argument. The batch uses the same `steps` and `validate` fields as `session.multiop`. A bare step object and an array of steps are accepted too.
- `with`, `body`, and `content` accept an array of lines, so code needs no `\n` escapes: `"with":["fn a() {","}"]`.
- Batches over 8 MiB are refused. A call that gets no reply for 15 minutes reports the outcome as unconfirmed.
- A single step without `validate` runs as one command. Anything else runs as an atomic multi-op.
- The grammar is loaded for the session automatically.
- Exit status 0 on success. On failure the error goes to stderr and the exit status is 1.
- Tags come from reads and live in the workspace store, so an edit in a later `isohypse call` can cite the tag from an earlier one.
- The daemon must be up: `isohypse state up .`.

Per-repo permissions apply as usual. See [Permissions per repository](../../README.md#permissions-per-repository).

## `isohypse hook`

Answers a harness PreToolUse event on stdin. Inside a repo served by a live Isohypse daemon it denies `Read`, `Grep`, `Glob`, `Edit`, `MultiEdit`, `Write` and `NotebookEdit`, and tells the agent to use `isohypse call`. Outside a served repo, and for paths outside the workspace, it allows the call.

## Claude Code

Load the plugin in [../claude/plugin](../claude/plugin):

```bash
claude --plugin-dir /absolute/path/to/isohypse/harness/claude/plugin
```

Or add the hook to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "isohypse hook" }] }
    ],
    "PreToolUse": [
      {
        "matcher": "Read|Grep|Glob|Edit|MultiEdit|Write|NotebookEdit|Bash",
        "hooks": [{ "type": "command", "command": "isohypse hook" }]
      }
    ]
  }
}
```

The agent then uses the Bash tool for `isohypse call`. Allow it without a prompt with the permission rule `Bash(isohypse:*)`.

## Codex and other agents

Put this in `AGENTS.md` (or the agent's equivalent instructions file):

```text
This repo is served by Isohypse. Read, search and edit through the shell:
run `isohypse context prompt` once, then send batches with
`isohypse call <<'EOF' {"steps":[...]} EOF`. Every edit cites the tag from a
read and carries a validate command.
```

Agents without a PreToolUse hook follow the instruction. Restrict what they may do with a [per-repo profile](../../README.md#permissions-per-repository).

## Test

```bash
cargo test --test call
```
