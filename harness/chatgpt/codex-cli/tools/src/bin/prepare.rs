//! Apply the native extension to an extracted, matching Codex release.
//!
//! Usage: prepare <extracted-source> <version>

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use codex_tools::{arg_or_exit, fail, insert_once, patch_items, read, write, EXTENSION_NOTICE};

/// Pin source-less workspace entries to the release version.
///
/// The release tag bumps workspace.version while its lockfile still records
/// first-party packages as 0.0.0. Only workspace entries are touched; dependency
/// versions and checksums remain pinned.
fn normalize_lock(lock: &Path, version: &str) -> Result<(), String> {
    let notice = format!("# Modified by Isohypse: normalize first-party package versions to {version}.");
    let text = read(lock)?;
    let mut blocks: Vec<String> = Vec::new();
    for (index, block) in text.split("[[package]]").enumerate() {
        if index == 0 {
            blocks.push(block.to_string());
            continue;
        }
        let source_less = block.lines().any(|line| line.starts_with("name = "))
            && !block.lines().any(|line| line.starts_with("source = "));
        blocks.push(if source_less {
            block.replacen("version = \"0.0.0\"", &format!("version = \"{version}\""), 1)
        } else {
            block.to_string()
        });
    }
    let joined = blocks.join("[[package]]");
    let text = if joined.contains(&notice) {
        joined
    } else {
        format!("{notice}\n{joined}")
    };
    write(lock, &text)
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let usage = "usage: prepare <extracted-source> <version>";
    let source = PathBuf::from(arg_or_exit(&argv, 1, usage))
        .canonicalize()
        .map_err(|e| format!("cannot resolve source: {e}"))?;
    let version = arg_or_exit(&argv, 2, usage);
    let native = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");

    let manifest = read(&source.join("codex-rs/Cargo.toml"))?;
    if !manifest.contains(&format!("version = \"{version}\"")) {
        return Err(format!("Expected Codex {version} source; refusing a different release"));
    }

    let lock = source.join("codex-rs/Cargo.lock");
    normalize_lock(&lock, &version)?;

    let lib = source.join("codex-rs/app-server/src/lib.rs");
    let extensions = source.join("codex-rs/app-server/src/extensions.rs");
    // One pass only: a second run must not append another declaration.
    if !read(&lib)?.contains(EXTENSION_NOTICE) {
        insert_once(&lib, "mod extensions;", "mod isohypse;", "app-server lib")?;
        insert_once(
            &extensions,
            "    codex_history_notes_extension::install(&mut builder, auth_manager.clone());",
            "    crate::isohypse::install(&mut builder);",
            "extensions",
        )?;
    }
    for path in [&lib, &extensions] {
        let text = read(path)?;
        if !text.contains(EXTENSION_NOTICE) {
            write(path, &format!("{EXTENSION_NOTICE}\n{text}"))?;
        }
    }

    patch_items(&source)?;

    let status = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .current_dir(source.join("codex-rs"))
        .stdout(std::process::Stdio::null())
        .status()
        .map_err(|e| format!("cargo metadata could not start: {e}"))?;
    if !status.success() {
        return Err(format!("cargo metadata failed with {status}"));
    }
    // Resolution rewrites the lockfile without our notice. The build uses
    // --locked, so skipping this step aborts it outright.
    normalize_lock(&lock, &version)?;

    for name in ["isohypse.rs", "isohypse_tests.rs"] {
        let from = native.join(name);
        let to = source.join("codex-rs/app-server/src").join(name);
        std::fs::copy(&from, &to)
            .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
    }
    // CommandExecutionItem gained model_context in 0.155; drop it for an older tree.
    if !read(&source.join("codex-rs/protocol/src/items.rs"))?
        .contains("pub model_context: Option<ModelInvocationContext>,")
    {
        let target = source.join("codex-rs/app-server/src/isohypse.rs");
        let text = read(&target)?;
        let stripped = text.replace("            model_context: None,\n", "");
        if stripped != text {
            write(&target, &stripped)?;
        }
    }
    println!("Native extension applied to Codex {version} app-server");
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}
