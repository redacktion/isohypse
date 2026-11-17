//! Build the executables Codex needs, skipping work the upstream release already did.
//!
//! Only `codex` carries the Isohypse patch. The code-mode host is unmodified
//! upstream source and the sole consumer of the `v8` crate, the heaviest
//! dependency in the tree, so by default we build `codex` alone and take the
//! published code-mode host binary. Set ISOHYPSE_BUILD_CODE_MODE_HOST=1 to compile
//! it locally instead, which reintroduces the V8 fetch and build.
//!
//! Usage: compile <extracted-source>

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use codex_tools::{arg_or_exit, fail};

/// Read the V8 cargo environment from the upstream release's own Python package.
///
/// That resolution is upstream's to own; reimplementing its V8 pinning here would
/// have to track every Codex bump, so the shim stays at this one boundary and
/// reports KEY=VALUE lines this binary applies before running cargo.
fn v8_environment(source: &Path) -> Result<Vec<(String, String)>, String> {
    let shim = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../v8-env.py");
    let output = Command::new("python3")
        .arg(&shim)
        .arg(source)
        .env("CODEX_REPO_ROOT", source)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", shim.display()))?;
    if !output.status.success() {
        return Err(format!(
            "V8 environment resolution failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.split_once('=')
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .ok_or_else(|| format!("malformed V8 environment line: {line}"))
        })
        .collect()
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let source = PathBuf::from(arg_or_exit(&argv, 1, "usage: compile <extracted-source>"))
        .canonicalize()
        .map_err(|e| format!("cannot resolve source: {e}"))?;
    let build_host = std::env::var("ISOHYPSE_BUILD_CODE_MODE_HOST").as_deref() == Ok("1");

    let mut command = Command::new("cargo");
    command.args(["build", "--locked", "--release"]);
    if build_host {
        for (key, value) in v8_environment(&source)? {
            command.env(key, value);
        }
        command.args(["-p", "codex-code-mode-host", "--bin", "codex-code-mode-host"]);
    }
    command.args(["-p", "codex-cli", "--bin", "codex", "-j", "6"]);
    command.current_dir(source.join("codex-rs"));
    command.env("CODEX_REPO_ROOT", &source);
    let status = command.status().map_err(|e| format!("cargo build could not start: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed with {status}"));
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}
