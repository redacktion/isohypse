# Isohypse for Codex CLI

Builds a Codex CLI binary that registers `isohypse` as a native tool through the
app-server extension registry, with no MCP server and no client-supplied tool
declaration. The ChatGPT app adapter that consumes this build lives in
`../native/`.

## Build

```sh
sh harness/chatgpt/codex-cli/build.sh
```

The script fetches the pinned Codex release, verifies its SHA-256 against
`pins.json`, applies the extension, and compiles `codex` plus
`codex-code-mode-host`. The first build takes about ten minutes; later builds
reuse the extracted source and target directory under
`${ISOHYPSE_BUILD_CACHE:-$HOME/.isohypse/codex-native}`.

## What the extension changes

- `app-server/src/lib.rs` and `extensions.rs` register the Isohypse extension.
- `app-server/src/isohypse.rs` and `isohypse_tests.rs` are added here.
- `tools/src/tool_call.rs` and `core/src/tools/handlers/extension_tools.rs`
  expose canonical, persisted file-change items through the turn-item emitter.
- `Cargo.lock` first-party package versions are normalized to the release.

`MODIFICATIONS.in` carries the notice that ships with the built runtime.

## Provenance

`fetch` downloads the official OpenAI Codex release source and verifies it
against `pins.json`; a version with no recorded digest is fetched
trust-on-first-use and its digest is recorded. The `prepare` step refuses a source
tree whose version does not match, so an extension can never be applied to the
wrong release. The integration is not an official OpenAI product or endorsement.

## Scope

This directory builds the Isohypse tool integration only. Related Codex patches
that are not part of that integration live in their own repositories; see
`snapcompax` for SnapCompaction.
