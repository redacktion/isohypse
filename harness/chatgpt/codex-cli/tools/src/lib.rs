//! Shared patching helpers for the Codex native extension.
//!
//! Every anchor is asserted to occur exactly once, so a Codex release that moves
//! these locations fails loudly instead of patching the wrong place. Re-running
//! is a no-op, because each file is guarded by the notice it prepends.

use std::fs;
use std::path::Path;
use std::process::ExitCode;

pub const ITEMS_NOTICE: &str =
    "// Modified by Isohypse: emit canonical file-change items from native tools.";
pub const EXTENSION_NOTICE: &str = "// Modified by Isohypse: register the native session extension.";

pub fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

pub fn write(path: &Path, text: &str) -> Result<(), String> {
    fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Insert `addition` after the only occurrence of `anchor`.
pub fn insert_once(path: &Path, anchor: &str, addition: &str, label: &str) -> Result<(), String> {
    let text = read(path)?;
    // Re-running prepare must be a no-op. Without this guard every build appended
    // another copy of the module declaration and the release failed to compile.
    if text.contains(addition) {
        return Ok(());
    }
    if text.matches(anchor).count() != 1 {
        return Err(format!("expected one {label} anchor in {}", path.display()));
    }
    write(path, &text.replacen(anchor, &format!("{anchor}\n{addition}"), 1))
}

/// Enable canonical file-change items on Codex's existing tool emitter.
pub fn patch_items(source: &Path) -> Result<(), String> {
    const METHOD: &str = r#"    /// Emits a file change through the normal persisted item lifecycle.
    /// A missing status starts the item; a terminal status completes it.
    fn emit_file_change<'a>(
        &'a self,
        item: codex_protocol::items::FileChangeItem,
    ) -> TurnItemEmissionFuture<'a>;
"#;
    const COMMAND_METHOD: &str = r#"    /// Emits a command execution through the normal persisted item lifecycle.
    fn emit_command_execution<'a>(
        &'a self,
        _item: codex_protocol::items::CommandExecutionItem,
    ) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }
"#;
    const NOOP: &str = r#"    fn emit_file_change<'a>(
        &'a self,
        _item: codex_protocol::items::FileChangeItem,
    ) -> TurnItemEmissionFuture<'a> {
        Box::pin(std::future::ready(()))
    }
"#;
    const CORE: &str = r#"    fn emit_file_change<'a>(
        &'a self,
        item: codex_protocol::items::FileChangeItem,
    ) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let completed = item.status.is_some();
            let item = TurnItem::FileChange(item);
            if completed {
                session.emit_turn_item_completed(turn.as_ref(), item).await;
            } else {
                session.emit_turn_item_started(turn.as_ref(), &item).await;
            }
        })
    }
"#;
    const COMMAND_CORE: &str = r#"    fn emit_command_execution<'a>(
        &'a self,
        item: codex_protocol::items::CommandExecutionItem,
    ) -> TurnItemEmissionFuture<'a> {
        Box::pin(async move {
            let (Some(session), Some(turn)) = (self.session.upgrade(), self.turn.upgrade()) else {
                return;
            };
            let completed = item.status != codex_protocol::items::CommandExecutionStatus::InProgress;
            let item = codex_protocol::items::TurnItem::CommandExecution(item);
            if completed {
                session.emit_turn_item_completed(turn.as_ref(), item).await;
            } else {
                session.emit_turn_item_started(turn.as_ref(), &item).await;
            }
        })
    }
"#;
    const TRAIT: &str = "pub trait TurnItemEmitter: Send + Sync {";
    const NOOP_IMPL: &str = "impl TurnItemEmitter for NoopTurnItemEmitter {";
    const CORE_IMPL: &str = "impl TurnItemEmitter for CoreTurnItemEmitter {";

    let groups: [(&str, &[(&str, &str)]); 2] = [
        ("tools/src/tool_call.rs", &[(TRAIT, METHOD), (TRAIT, COMMAND_METHOD), (NOOP_IMPL, NOOP)]),
        ("core/src/tools/handlers/extension_tools.rs", &[(CORE_IMPL, CORE), (CORE_IMPL, COMMAND_CORE)]),
    ];
    for (relative, insertions) in groups {
        let path = source.join("codex-rs").join(relative);
        let text = read(&path)?;
        if text.contains(ITEMS_NOTICE) {
            continue;
        }
        let mut text = text;
        for (anchor, body) in insertions {
            if text.matches(anchor).count() != 1 {
                return Err(format!("expected one file-change emitter anchor in {relative}"));
            }
            text = text.replacen(anchor, &format!("{anchor}\n{body}"), 1);
        }
        write(&path, &format!("{ITEMS_NOTICE}\n{text}"))?;
    }
    Ok(())
}

/// Report a failure the way every bin here does, and exit unsuccessfully.
pub fn fail(error: &str) -> ExitCode {
    eprintln!("isohypse: {error}");
    ExitCode::FAILURE
}

/// Resolve one argument, or exit with the bin's usage line.
pub fn arg_or_exit(argv: &[String], index: usize, usage: &str) -> String {
    match argv.get(index) {
        Some(value) => value.clone(),
        None => {
            eprintln!("{usage}");
            std::process::exit(2);
        }
    }
}
