//! Extract an owned Claude installation into a private, version-pinned local copy.
//!
//! Only this patcher belongs in the repository; extracted Anthropic code does not.
//!
//! Usage: prepare <binary> <destination>

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use claude_tools::{anchor_index, arg_or_exit, fail, parse, read, sha256, ANCHOR, REQUEST};

fn run() -> Result<(), String> {
    let argv: Vec<String> = std::env::args().collect();
    let usage = "usage: prepare <binary> <destination>";
    let binary = PathBuf::from(arg_or_exit(&argv, 1, usage));
    let destination = PathBuf::from(arg_or_exit(&argv, 2, usage))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(&argv[2]));
    if destination.exists() {
        return Err("Destination already exists; choose a fresh private directory".to_string());
    }

    let data = read(&binary)?;
    let graph = parse(&data)?;
    let anchor = anchor_index(&graph.modules)?;
    let factory = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../native.mjs")
        .canonicalize()
        .map_err(|e| format!("cannot resolve native.mjs: {e}"))?;
    let prefix = format!("{}/", destination.display());

    std::fs::create_dir_all(&destination)
        .map_err(|e| format!("cannot create {}: {e}", destination.display()))?;
    for (index, module) in graph.modules.iter().enumerate() {
        let mut content = module.content.clone();
        if module.relative.extension().is_some_and(|ext| ext == "js")
            || module.relative.file_name().is_some_and(|name| name == "cli")
        {
            content = replace_all(&content, b"/$bunfs/root/", prefix.as_bytes());
            if index == anchor {
                let mut head = format!("import {{createIsohypseTool}} from \"{}\";\n", factory.display()).into_bytes();
                head.extend_from_slice(&replace_all(&content, ANCHOR, REQUEST));
                content = head;
            }
        }
        let target = destination.join(&module.relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, &content).map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    }
    let resolved = binary.canonicalize().unwrap_or_else(|_| binary.clone());
    let provenance = format!(
        "Input: {}\nSHA256: {}\nLocal extraction; native Isohypse registry insertion only.\n",
        resolved.display(),
        sha256(&binary)?
    );
    std::fs::write(destination.join("provenance.txt"), provenance)
        .map_err(|e| format!("cannot write provenance: {e}"))?;
    println!("{}", destination.join(&graph.modules[0].relative).display());
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
