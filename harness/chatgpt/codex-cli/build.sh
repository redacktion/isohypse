#!/bin/sh
set -eu
native_script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

# ---------------------------------------------------------------------------
# Target version
#
# install.mjs refuses a native binary whose --version differs from the runtime the
# app launches, so the version is detected from the installed app rather than
# hardcoded. Override with ISOHYPSE_CODEX_VERSION when building ahead of the app.
# ---------------------------------------------------------------------------
native_app_binary="${ISOHYPSE_APP_CODEX:-}"
if [ -z "$native_app_binary" ]; then
  for candidate in /Applications/ChatGPT.app/Contents/Resources/codex-cli/bin/codex /Applications/ChatGPT.app/Contents/Resources/codex; do
    if [ -x "$candidate" ]; then native_app_binary="$candidate"; break; fi
  done
fi
native_version="${ISOHYPSE_CODEX_VERSION:-}"
if [ -z "$native_version" ]; then
  if [ -x "$native_app_binary" ]; then
    native_version=$("$native_app_binary" --version 2>/dev/null | awk '{print $NF}')
    printf 'Detected installed Codex runtime: %s\n' "${native_version:-unknown}"
  else
    echo "No installed Codex runtime found; set ISOHYPSE_APP_CODEX or ISOHYPSE_CODEX_VERSION" >&2
    exit 1
  fi
fi
if [ -z "$native_version" ]; then
  echo "Could not determine the Codex version to build" >&2
  exit 1
fi

# Keep the extracted source and cargo target outside ~/Library/Caches. macOS may
# purge that directory at any time, which previously discarded the warm target
# directory and forced a full release rebuild after every native-source edit.
native_build_root="${ISOHYPSE_BUILD_CACHE:-$HOME/.isohypse/codex-native}"
native_source="$native_build_root/codex-rust-v$native_version"
native_archive="$native_build_root/$native_version.tar.gz"
native_out="$native_build_root/out"
mkdir -p "$native_build_root"

cargo run --release --quiet --manifest-path "$native_script_dir/tools/Cargo.toml" --bin fetch -- "$native_version" "$native_build_root"
if [ ! -d "$native_source" ]; then tar -xzf "$native_archive" -C "$native_build_root"; fi
cargo run --release --quiet --manifest-path "$native_script_dir/tools/Cargo.toml" --bin prepare -- "$native_source" "$native_version"

if [ -n "${SNAPCOMPAX_DIR:-}" ]; then
  sh "$SNAPCOMPAX_DIR/apply.sh" "$native_source"
  ( cd "$native_source/codex-rs" && cargo metadata --format-version 1 >/dev/null )
fi

# target/ reaches several GB and cargo reuses little of it across Codex version
# bumps, because fingerprints are keyed on source content. sccache is keyed on
# compiler and source as well, but its entries survive both a version bump and a
# sweep, so the permanent cache stays bounded instead of growing per release.
if command -v sccache >/dev/null 2>&1; then
  export RUSTC_WRAPPER="${RUSTC_WRAPPER:-sccache}"
  export SCCACHE_CACHE_SIZE="${SCCACHE_CACHE_SIZE:-5G}"
  sccache --start-server >/dev/null 2>&1 || true
else
  echo 'note: sccache not installed; this build and the next one will both be cold' >&2
fi

cd "$native_source/codex-rs"
export CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_LTO=false
export CARGO_PROFILE_RELEASE_OPT_LEVEL=1 CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16
cargo run --release --quiet --manifest-path "$native_script_dir/tools/Cargo.toml" --bin compile -- "$native_source"

native_release="$native_source/codex-rs/target/release"

# The Isohypse patch never touches the code-mode host (see MODIFICATIONS), and that
# host is the only consumer of the v8 crate -- the heaviest item in the tree. Take
# upstream's published binary instead of rebuilding V8, unless asked to compile it.
if [ "${ISOHYPSE_BUILD_CODE_MODE_HOST:-0}" != "1" ]; then
  case "$(uname -m)" in
    arm64) native_host_target=aarch64-apple-darwin ;;
    *)     echo 'No pinned code-mode host for this architecture; set ISOHYPSE_BUILD_CODE_MODE_HOST=1' >&2
           exit 1 ;;
  esac
  native_host_archive="$native_build_root/codex-code-mode-host-$native_host_target-$native_version.tar.gz"
  tar -xzf "$native_host_archive" -C "$native_release"
  mv "$native_release/codex-code-mode-host-$native_host_target" "$native_release/codex-code-mode-host"
  chmod 755 "$native_release/codex-code-mode-host"
fi

cp "$native_source/LICENSE" "$native_release/LICENSE"
cp "$native_source/NOTICE" "$native_release/NOTICE"
cargo run --release --quiet --manifest-path "$native_script_dir/tools/Cargo.toml" --bin modifications -- \
  "$native_script_dir/MODIFICATIONS.in" "$native_release/MODIFICATIONS" "$native_version"
if [ -n "${SNAPCOMPAX_DIR:-}" ]; then
  cat >> "$native_release/MODIFICATIONS" <<'EOF'

Modified by the SnapCompaction project (snapcompax):
- core/src/snapcompact.rs and core/src/snapcompact_render.rs are added.
- An eligible compaction on an image-capable model replaces the transcript with
  PNG-rendered pages, so prior context survives verbatim instead of paraphrased.
- The ab_glyph workspace dependency is added for glyph rasterization.
Methodology by the oh-mi-pi developer; port by the snapcompax project.
EOF
fi

# Publish outside target/ so the build directory can be swept without losing the
# installed runtime. install.mjs expects the companions to sit beside codex.
mkdir -p "$native_out"
for name in codex codex-code-mode-host LICENSE NOTICE MODIFICATIONS; do
  cp "$native_release/$name" "$native_out/$name"
done
chmod 755 "$native_out/codex" "$native_out/codex-code-mode-host"

if [ "${ISOHYPSE_KEEP_TARGET:-0}" != "1" ]; then
  native_target="$native_source/codex-rs/target"
  # Guarded: only ever removes the cargo target directory this script itself created.
  case "$native_target" in
    "$native_build_root"/codex-rust-*/codex-rs/target) rm -rf "$native_target" ;;
    *) echo "refusing to remove unexpected path: $native_target" >&2 ;;
  esac
fi

printf '\nNative Codex runtime: %s\n' "$native_out/codex"
"$native_out/codex" --version
printf 'Install with:\n  node %s/../../install.mjs --allow-edits --native-binary "%s"\n' \
  "$native_script_dir" "$native_out/codex"
