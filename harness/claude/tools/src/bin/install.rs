//! Install a local native Claude adapter without rebundling Claude's runtime.
//!
//! Usage: install [--allow-edits] [--source <binary>] [--rollback]

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use claude_tools::{
    anchor_index, codesign, fail, nonce, parse, read, sha256, ANCHOR, REQUEST,
};

/// The one Claude build this adapter has been reviewed against.
const SUPPORTED_SHA256: &str = "ef5d2909c8af49f31ab6d5487e90316777bc2fac170adfe8160716caa8aaf4f9";
const SUPPORTED_VERSION: &str = "2.1.263 (Claude Code)";
const RELEASE_PREFIX: &str = "2.1.263-";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

/// Point the launcher at `target` in one rename, so no window leaves it dangling.
fn activate(launcher: &Path, target: &Path) -> Result<(), String> {
    let temporary = launcher.with_file_name(format!("claude.isohypse-{}", nonce()));
    let _ = std::fs::remove_file(&temporary);
    std::os::unix::fs::symlink(target, &temporary)
        .map_err(|e| format!("cannot create {}: {e}", temporary.display()))?;
    std::fs::rename(&temporary, launcher)
        .map_err(|e| format!("cannot activate {}: {e}", launcher.display()))
}

fn make_release_dir(root: &Path) -> Result<PathBuf, String> {
    for _ in 0..64 {
        let candidate = root.join(format!("{RELEASE_PREFIX}{}", &nonce()[..6.min(nonce().len())]));
        match std::fs::create_dir(&candidate) {
            Ok(()) => {
                let _ = std::fs::set_permissions(
                    &candidate,
                    std::os::unix::fs::PermissionsExt::from_mode(0o700),
                );
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("cannot create release directory: {e}")),
        }
    }
    Err("cannot allocate a unique release directory".to_string())
}

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let allow_edits = argv.iter().any(|arg| arg == "--allow-edits");
    let rollback = argv.iter().any(|arg| arg == "--rollback");
    let source_arg = argv
        .iter()
        .position(|arg| arg == "--source")
        .and_then(|index| argv.get(index + 1))
        .map(PathBuf::from);

    let root = home().join("Library/Application Support/Isohypse/claude-adapter");
    let launcher = home().join(".local/bin/claude");
    let receipt_path = root.join("install.json");
    let previous: Option<serde_json::Value> = if receipt_path.exists() {
        let text = std::fs::read_to_string(&receipt_path)
            .map_err(|e| format!("cannot read receipt: {e}"))?;
        Some(serde_json::from_str(&text).map_err(|e| format!("cannot parse receipt: {e}"))?)
    } else {
        None
    };
    let receipt_field = |value: &serde_json::Value, key: &str| -> Option<String> {
        value.get(key).and_then(serde_json::Value::as_str).map(str::to_string)
    };

    if rollback {
        let previous = previous.ok_or("No Isohypse installation receipt")?;
        let original = PathBuf::from(receipt_field(&previous, "original").unwrap_or_default());
        if !original.is_file() {
            return Err("Original executable missing; launcher unchanged".to_string());
        }
        activate(&launcher, &original)?;
        println!("Restored {} -> {}", launcher.display(), original.display());
        return Ok(());
    }

    let current = launcher
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {e}", launcher.display()))?;
    let source = match source_arg {
        Some(path) => path,
        None => match &previous {
            Some(previous)
                if receipt_field(previous, "binary").as_deref() == current.to_str() =>
            {
                PathBuf::from(receipt_field(previous, "original").unwrap_or_default())
            }
            _ => current.clone(),
        },
    };

    let mut data = read(&source)?;
    let digest = sha256(&source)?;
    if digest != SUPPORTED_SHA256 {
        return Err(format!(
            "Unsupported Claude binary. Expected verified {RELEASE_PREFIX}; launcher unchanged. Review the new module graph before updating this pin."
        ));
    }
    codesign(&["--verify", "--strict", &source.display().to_string()])?;

    let graph = parse(&data)?;
    let anchor = anchor_index(&graph.modules)?;
    let module = &graph.modules[anchor];
    let record = module.record;

    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let release = make_release_dir(&root)?;
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    std::fs::create_dir_all(release.join("claude"))
        .map_err(|e| format!("cannot create release/claude: {e}"))?;
    std::fs::create_dir_all(release.join("bin"))
        .map_err(|e| format!("cannot create release/bin: {e}"))?;
    for (from, to) in [
        (here.join("native.mjs"), release.join("claude/native.mjs")),
        (here.join("../session.mjs"), release.join("session.mjs")),
        (here.join("adapter.mjs"), release.join("claude/adapter.mjs")),
    ] {
        std::fs::copy(&from, &to)
            .map_err(|e| format!("cannot copy {} to {}: {e}", from.display(), to.display()))?;
    }
    let bridge = release.join("claude/registration.mjs");
    let allow = serde_json::json!({ "allowEdits": allow_edits }).to_string();
    std::fs::write(
        &bridge,
        format!(
            "import {{createIsohypseTool as create}} from \"./native.mjs\";\nexport function createIsohypseTool(...args) {{ return create(...args, {allow}); }}\n"
        ),
    )
    .map_err(|e| format!("cannot write registration bridge: {e}"))?;

    // Reuse this module's now-unused bytecode allocation. All other modules,
    // Mach-O offsets, embedded assets, and original source notices are preserved.
    let head = format!(
        "// Modified locally: native Isohypse registration.\nimport {{createIsohypseTool}} from \"{}\";\n",
        bridge.display()
    );
    let mut patched = head.into_bytes();
    patched.extend_from_slice(&replace_all(&module.content, ANCHOR, REQUEST));
    if patched.len() > record[7] as usize {
        return Err("Insufficient bytecode allocation; launcher unchanged".to_string());
    }
    let start = graph.base + record[6] as usize;
    data.get_mut(start..start + patched.len())
        .ok_or("patch range out of bounds")?
        .copy_from_slice(&patched);
    let mut updated = record;
    updated[2] = record[6];
    updated[3] = patched.len() as u32;
    updated[6] = 0;
    updated[7] = 0;
    updated[8] = 0;
    updated[9] = 0;
    for (field, value) in updated.iter().enumerate() {
        let at = module.offset + field * 4;
        data.get_mut(at..at + 4)
            .ok_or("module record out of bounds")?
            .copy_from_slice(&value.to_le_bytes());
    }

    let binary = release.join("bin/claude");
    std::fs::write(&binary, &data).map_err(|e| format!("cannot write {}: {e}", binary.display()))?;
    std::fs::set_permissions(&binary, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .map_err(|e| format!("cannot chmod {}: {e}", binary.display()))?;
    codesign(&["--force", "--sign", "-", &binary.display().to_string()])?;
    codesign(&["--verify", "--strict", &binary.display().to_string()])?;

    let version = String::from_utf8(
        Command::new(&binary)
            .arg("--version")
            .output()
            .map_err(|e| format!("cannot run the patched binary: {e}"))?
            .stdout,
    )
    .map_err(|_| "version output is not UTF-8".to_string())?;
    let version = version.trim().to_string();
    if version != SUPPORTED_VERSION {
        return Err(format!("Unexpected version: {version}; launcher unchanged"));
    }

    // Exercise signed sessions and actual native write/resume before switching.
    let log = std::fs::File::create(release.join("verification.log"))
        .map_err(|e| format!("cannot open verification log: {e}"))?;
    // The verifier ships beside this binary, so a cargo-built installer and a
    // copied-out installer both find it without a Python runtime in the loop.
    let verifier = std::env::current_exe()
        .map_err(|e| format!("cannot locate the installed verifier: {e}"))?
        .with_file_name("test-native");
    let status = Command::new(&verifier)
        .env("ISOHYPSE_TEST_RESUME", "1")
        .env("CLAUDE_NATIVE_ENTRY", "binary")
        .env("CLAUDE_NATIVE_BINARY", &binary)
        .stdout(Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(Stdio::from(log))
        .status()
        .map_err(|e| format!("cannot run {}: {e}", verifier.display()))?;
    if !status.success() {
        return Err(format!("Native verification failed ({status}); launcher unchanged"));
    }

    let receipt = serde_json::json!({
        "original": source.canonicalize().unwrap_or_else(|_| source.clone()).display().to_string(),
        "sourceSHA256": digest,
        "binary": binary.display().to_string(),
        "allowEdits": allow_edits,
        "version": version,
    });
    let text = serde_json::to_string_pretty(&receipt).map_err(|e| e.to_string())? + "\n";
    std::fs::write(release.join("install.json"), &text).map_err(|e| e.to_string())?;
    let staging = root.join("install.json.new");
    std::fs::write(&staging, &text).map_err(|e| e.to_string())?;
    std::fs::rename(&staging, &receipt_path).map_err(|e| format!("cannot record receipt: {e}"))?;
    activate(&launcher, &binary)?;
    println!(
        "Installed: {} -> {}\nOriginal preserved: {}\nVerified native writes, permission gates, and resume.",
        launcher.display(),
        binary.display(),
        source.display()
    );
    Ok(())
}

fn replace_all(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut index = 0;
    while index < haystack.len() {
        if haystack[index..].starts_with(needle) {
            out.extend_from_slice(replacement);
            index += needle.len();
        } else {
            out.push(haystack[index]);
            index += 1;
        }
    }
    out
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail(&error),
    }
}
