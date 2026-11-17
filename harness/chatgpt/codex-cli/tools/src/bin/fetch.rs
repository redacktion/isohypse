//! Fetch and verify the upstream artifacts a Codex version is built from.
//!
//! Usage: fetch <version> <dest-dir> [<darwin-target>]
//!
//! Digests live in pins.json. A version with no pin is fetched trust-on-first-use
//! and its digest is recorded, so later builds of the same version are verified.
//! Replace a recorded digest by deleting its entry and rebuilding.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use codex_tools::{arg_or_exit, fail, read};

// Left generic so non-version keys such as `_comment` survive a rewrite.
// serde_json orders object keys, so output stays stable across runs.
type Pins = serde_json::Map<String, serde_json::Value>;

fn pins_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../pins.json")
}

fn machine_target() -> Option<&'static str> {
    match std::env::consts::ARCH {
        "aarch64" => Some("aarch64-apple-darwin"),
        "x86_64" => Some("x86_64-apple-darwin"),
        _ => None,
    }
}

/// Download through the system curl, which build.sh already relies on.
fn fetch(url: &str, dst: &Path) -> Result<(), String> {
    if dst.exists() {
        return Ok(());
    }
    let status = Command::new("/usr/bin/curl")
        .args(["--fail", "--location", "--silent", "--show-error", "--max-time", "600"])
        .args(["--user-agent", "isohypse-codex-build"])
        .arg("--output")
        .arg(dst)
        .arg(url)
        .status()
        .map_err(|e| format!("curl could not start: {e}"))?;
    if !status.success() {
        let _ = std::fs::remove_file(dst);
        return Err(format!("download failed for {url} ({status})"));
    }
    Ok(())
}

fn sha256(path: &Path) -> Result<String, String> {
    let output = Command::new("/usr/bin/shasum")
        .arg("-a")
        .arg("256")
        .arg(path)
        .output()
        .map_err(|e| format!("shasum could not start: {e}"))?;
    if !output.status.success() {
        return Err(format!("cannot hash {}", path.display()));
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
        .ok_or_else(|| format!("no digest for {}", path.display()))
}

fn verify(path: &Path, pins: &mut Pins, version: &str, key: &str, label: &str) -> Result<(), String> {
    let actual = sha256(path)?;
    let entry = pins
        .entry(version.to_string())
        .or_insert_with(|| serde_json::Value::Object(serde_json::Map::new()));
    let object = entry
        .as_object_mut()
        .ok_or_else(|| format!("pins.json entry for {version} is not an object"))?;
    let already_pinned = match object.get(key).and_then(serde_json::Value::as_str) {
        None => {
            object.insert(key.to_string(), serde_json::Value::String(actual.clone()));
            false
        }
        Some(expected) if *expected != actual => {
            return Err(format!(
                "{label} checksum mismatch for {version}; refusing to build\n  expected {expected}\n  actual   {actual}"
            ));
        }
        Some(_) => true,
    };
    if already_pinned {
        println!("  {label}: verified");
    } else {
        let snapshot = serde_json::Value::Object(pins.clone());
        let text = serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())? + "\n";
        std::fs::write(pins_path(), text).map_err(|e| format!("cannot write pins.json: {e}"))?;
        println!("  {label}: no pin for {version}; recorded {actual}");
    }
    Ok(())
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let usage = "usage: fetch <version> <dest-dir> [<darwin-target>]";
    let version = arg_or_exit(&argv, 1, usage);
    let dest = PathBuf::from(arg_or_exit(&argv, 2, usage));
    let target = match argv.get(3) {
        Some(value) => value.clone(),
        None => machine_target()
            .ok_or_else(|| format!("No upstream target known for {}", std::env::consts::ARCH))?
            .to_string(),
    };
    std::fs::create_dir_all(&dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let mut pins: Pins = if pins_path().exists() {
        match serde_json::from_str::<serde_json::Value>(&read(&pins_path())?)
            .map_err(|e| format!("cannot parse pins.json: {e}"))?
        {
            serde_json::Value::Object(map) => map,
            _ => return Err("pins.json must be a JSON object".to_string()),
        }
    } else {
        Pins::new()
    };

    let source = dest.join(format!("{version}.tar.gz"));
    println!("codex {version} [{target}]");
    fetch(
        &format!("https://codeload.github.com/openai/codex/tar.gz/refs/tags/rust-v{version}"),
        &source,
    )?;
    verify(&source, &mut pins, &version, "source", "source archive")?;

    let host = dest.join(format!("codex-code-mode-host-{target}-{version}.tar.gz"));
    fetch(
        &format!(
            "https://github.com/openai/codex/releases/download/rust-v{version}/codex-code-mode-host-{target}.tar.gz"
        ),
        &host,
    )?;
    verify(&host, &mut pins, &version, "code_mode_host", "code-mode host")?;
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}
