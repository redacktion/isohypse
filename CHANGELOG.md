# Changelog

All notable changes to Isohypse are documented here.

## [0.3.0] - 2026-11-17

### Added

- `validate_sandbox` config (macOS): `workspace_write` and
  `workspace_write_no_network` confine validation commands with `sandbox-exec`.
- `isohypse call` runs one batch in a fresh session from stdin. `isohypse hook` briefs the agent at session start and denies Read, Grep, Glob, Edit, Write, and equivalent shell commands in a served workspace. Both need no runtime and work with any client version. A Claude Code plugin in `harness/claude/plugin` wires the hook.
- One-time batching hint after three consecutive single-step read-only requests.
- `checkpoint` accepted on mutate steps in the shared session adapter and the
  Claude, ChatGPT, and Pi harness schemas.

### Changed

- Op grammar is schema v0.3. `label` is optional and prints in the multi-op
  step header and the JSON result.
- Stack-graph crates (`isohypse-sg*`, `isohypse-ts-sg`, `isohypse-tsg`,
  `isohypse-lsp-pos`, `isohypse-se`) come from crates.io. `vendor/` is removed.

### Fixed

- Hook: `find /usr -name x` no longer counts `x` as a workspace path, `kill $(pgrep isohypse)` is caught, and `<<EOF` is no longer read as a command argument.
- `isohypse call` joins a text array given on a bare step (`content`, `with`, `body`).
- Unit and integration tests are one assertion each, with per-test temp directories and macro names, so parallel runs no longer collide.
- Allow patterns match exactly. `context.read` no longer allows `context.readx` or `context.read.evil`. A trailing dot (`context.`) opts into the whole subtree.
- Enforcement state is a typed one-way machine (`src/policy.rs`). The per-session copy that was never set is removed.
- Enforcement profiles with an allowlist no longer block every `session.multiop`. Each step is checked against the policy, including steps inside macros.
- Narrowing enforcement now refuses dropping a `denied` entry, so a deny-only profile cannot widen to `full`.
- A corrupt persisted enforcement file locks the workspace to `state.status` instead of removing enforcement.
- A stored enforcement policy with a missing key or a wrong-typed field counts as corrupt and locks the workspace down. It no longer restores as a weaker policy.
- `isohypse state stop` clears persisted workspace enforcement, as the setup screen and docs state.
- Setup step 4 help text is left-aligned, clipped to the box, and lists the sandbox setting.
- Concurrent sessions recording seen lines for the same tag no longer corrupt
  the record.
- Pi validate-gate check no longer depends on a fixed workspace path.

## [0.1.0] - 2026-09-02

This entry starts the project changelog at the current release.

### Added

- Persistent NDJSON sessions with resumable state, command acknowledgements,
  cancellation, reload handling, subscriptions, and session notes.
- Cooperative cancellation for in-process operations, queued-command aborts,
  rollback-aware cancelled transactions, and hard termination of validation
  child processes.
- Atomic `multi-op` execution with ordered steps, required validation, optional
  checkpoints, compact effect summaries, and replay-oriented failure details.
- Inline jq construction for compound operations, evaluated inside the binary.
- Streaming context reads using NDJSON chunks with path, tag, line bounds,
  sequence, text, and EOF metadata.
- Persistent workspace context budgets with spacing, rolling limits, escalating
  cooldowns, session accounting, and setup-TUI configuration.
- Workspace migration and reindex lifecycle support, generation-aware reloads,
  pending-state recovery, and safer daemon/socket handoff behavior.
- Read-only release update checking through `isohypse update check`.

### Changed

- Context operations now require precise paths, ranges, symbols, or literal
  patterns and return actionable guidance when batching is required.
- Mutation handling now verifies tags and anchors against current file content,
  preserves rollback safety, and records durable change metadata.
- Large-read workflows now favor explicit ranges, outlines, multiple targets,
  or streaming rather than output-size tuning.
- Error responses now identify the failing layer, operation, step, target,
  expected/current content, and a replayable command when available.
- The model-facing prompt, schema reference, README workflow, and setup
  documentation now describe sessions, batching, validation, checkpoints,
  streaming, budgets, and current daemon behavior.

### Fixed

- Reindex requests now use the live workspace root and report their lifecycle
  state correctly.
- Session budget usage survives resume and daemon reload instead of resetting.
- Reload and connection-close paths now clear in-flight command state and
  signal active work before teardown.
- Validation timeouts terminate spawned child processes and restore staged
  changes safely.
- File indexing skips unreadable and symlink entries that could escape the
  workspace or recurse through it.
- Temporary object and staging paths avoid collisions between concurrent
  processes.
- Metadata and manifest updates recover from interrupted writes.
- Rust warnings and stale operation-surface documentation were cleaned up.

