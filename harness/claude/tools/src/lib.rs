//! Locate and rewrite modules inside a Bun-compiled Claude binary.
//!
//! Anthropic's own code is never vendored here: this reads modules out of an
//! installation the operator already owns, inserts the Isohypse registration, and
//! writes the result to a private, version-pinned copy.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// The tool-registry expression the patch replaces. Exactly one module carries it;
/// more than one would mean the registry moved and the patch is no longer safe.
pub const ANCHOR: &[u8] = b",bfe,...rK?[rK]:[]";
pub const REQUEST: &[u8] = b",createIsohypseTool(Tt,c,Q,K,Et),bfe,...rK?[rK]:[]";
const MARKER: &[u8] = b"\n---- Bun! ----\n";
const MODULE_HEADER_BYTES: usize = 32;
const RECORD_BYTES: usize = 52;
const NUL: u8 = 0;

pub struct Module {
    /// Offset of this module's record, for rewriting it in place.
    pub offset: usize,
    pub record: [u32; 13],
    pub relative: PathBuf,
    pub content: Vec<u8>,
}

pub struct Graph {
    pub base: usize,
    pub modules: Vec<Module>,
}

pub fn read(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

pub fn fail(error: &str) -> ExitCode {
    eprintln!("isohypse: {error}");
    ExitCode::FAILURE
}

pub fn arg_or_exit(argv: &[String], index: usize, usage: &str) -> String {
    match argv.get(index) {
        Some(value) => value.clone(),
        None => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}

fn u32_at(data: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset.checked_add(4).ok_or("module table offset overflow")?;
    let slice = data.get(offset..end).ok_or("module table out of range")?;
    Ok(u32::from_le_bytes(slice.try_into().expect("four bytes")))
}

/// Parse the embedded Bun module graph out of a compiled binary.
pub fn parse(data: &[u8]) -> Result<Graph, String> {
    let end = data
        .windows(MARKER.len())
        .rposition(|window| window == MARKER)
        .ok_or("Unsupported binary: no Bun module graph")?;
    let header = end
        .checked_sub(MODULE_HEADER_BYTES)
        .ok_or("Unsupported Bun module table")?;
    let size = u64::from_le_bytes(
        data.get(header..header + 8)
            .ok_or("Unsupported Bun module table")?
            .try_into()
            .expect("eight bytes"),
    );
    let table = u32_at(data, header + 8)? as usize;
    let length = u32_at(data, header + 12)? as usize;
    let entry = u32_at(data, header + 16)? as usize;
    if length % RECORD_BYTES != 0 {
        return Err("Unsupported Bun module table".to_string());
    }
    let base = i64::try_from(header).expect("usize fits i64") - i64::try_from(size).expect("u64 fits i64");
    if base < 0 || base as usize >= data.len() {
        return Err("Unsupported Bun module table".to_string());
    }
    let base = base as usize;
    let mut modules = Vec::new();
    for index in 0..length / RECORD_BYTES {
        let offset = base + table + index * RECORD_BYTES;
        let mut record = [0u32; 13];
        for (field, slot) in record.iter_mut().enumerate() {
            *slot = u32_at(data, offset + field * 4)?;
        }
        let name = slice(data, base, record[0], record[1])?;
        let name = std::str::from_utf8(name).map_err(|_| "module name is not UTF-8".to_string())?;
        let name = name.trim_end_matches(NUL as char);
        let relative = name
            .strip_prefix("/$bunfs/root/")
            .ok_or_else(|| format!("Unexpected module path: {name}"))?;
        let relative = PathBuf::from(relative);
        if relative.is_absolute() || relative.components().any(|part| part.as_os_str() == "..") {
            return Err("Unsafe module path".to_string());
        }
        modules.push(Module {
            offset,
            record,
            relative,
            content: slice(data, base, record[2], record[3])?.to_vec(),
        });
    }
    let _ = entry;
    Ok(Graph { base, modules })
}

fn slice(data: &[u8], base: usize, start: u32, length: u32) -> Result<&[u8], String> {
    let from = base.checked_add(start as usize).ok_or("module offset overflow")?;
    let to = from.checked_add(length as usize).ok_or("module length overflow")?;
    data.get(from..to).ok_or_else(|| "module range out of bounds".to_string())
}

/// The module whose content carries the tool-registry anchor.
pub fn anchor_index(modules: &[Module]) -> Result<usize, String> {
    let mut found = None;
    for (index, module) in modules.iter().enumerate() {
        if module.content.windows(ANCHOR.len()).any(|window| window == ANCHOR) {
            if found.is_some() {
                return Err("Native tool registry changed; launcher unchanged".to_string());
            }
            found = Some(index);
        }
    }
    let index = found.ok_or("Native tool registry changed; launcher unchanged")?;
    let count = modules[index]
        .content
        .windows(ANCHOR.len())
        .filter(|window| *window == ANCHOR)
        .count();
    if count != 1 {
        return Err("Native tool registry changed; launcher unchanged".to_string());
    }
    Ok(index)
}

pub fn sha256(path: &Path) -> Result<String, String> {
    let output = Command::new("/usr/bin/shasum")
        .args(["-a", "256"])
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

pub fn codesign(args: &[&str]) -> Result<(), String> {
    let status = Command::new("/usr/bin/codesign")
        .args(args)
        .status()
        .map_err(|e| format!("codesign could not start: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("codesign {args:?} failed with {status}"))
    }
}

/// A short, collision-resistant suffix for a temporary launcher name.
pub fn nonce() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}{:x}", std::process::id(), nanos)
}
