# Changelog

All notable changes to Isohypse are documented here.

## [0.3.1] - 2026-11-17

### Added

- Codex plugin in `harness/codex/plugin`, with a local marketplace manifest.
- `isohypse hook` handles Codex `apply_patch`.

### Changed

- Removed the version-pinned native Claude Code and Codex adapters. Use the plugins.

## [0.3.0] - 2026-11-17

### Added

- `isohypse call` and `isohypse hook`, plus a Claude Code plugin.
- `validate_sandbox` config (macOS).
- Batching hint after three single-step reads.
- `checkpoint` on mutate steps in the harness schemas.

### Changed

- Breaking: enforcement allow patterns match exactly. A trailing dot (`context.`) allows the subtree.
- Grammar is schema v0.3. `label` is optional.
- Stack-graph crates come from crates.io. `vendor/` is removed.

### Fixed

- Enforcement no longer blocks every `session.multiop`. Each step is checked.
- Narrowing enforcement cannot drop a `denied` entry.
- A corrupt stored enforcement policy locks the workspace down.
- `state stop` clears persisted enforcement.
- Seen-lines race between sessions sharing a tag.
- `hex_decode` panic on non-ASCII input.

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

