#!/usr/bin/env python3
"""Report the V8 cargo environment for an extracted Codex source tree.

The upstream release ships this resolution as its own Python package
(`scripts/codex_package`), and it pins a specific V8 build per release. Rather
than reimplement that pinning in Rust and chase every Codex bump, this shim
delegates to upstream and prints KEY=VALUE lines that the Rust `compile` bin
applies before invoking cargo.

Usage: v8-env.py <extracted-codex-source>
"""
import sys
from pathlib import Path

source = Path(sys.argv[1]).resolve()
sys.path.insert(0, str(source / "scripts"))

from codex_package.targets import TARGET_SPECS, default_target
from codex_package.v8 import resolve_codex_v8_cargo_env

for key, value in resolve_codex_v8_cargo_env(TARGET_SPECS[default_target()]).items():
    print(f"{key}={value}")
