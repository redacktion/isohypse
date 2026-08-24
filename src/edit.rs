use serde_json::Value;

use crate::graph::extract::extract;
use crate::patch::{FileOp, Gap, Hunk, Patch, Range, Section, SourcedHunk};

pub fn compile(
    op: &Value,
    read_text: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Patch, String> {
    let ops: Vec<&Value> = match op {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![op],
        _ => return Err("mutate.edit takes an op object or an array of them".to_string()),
    };
    if ops.is_empty() {
        return Err("mutate.edit takes at least one op object".to_string());
    }
    let mut sections = Vec::new();
    for (index, item) in ops.iter().enumerate() {
        sections.push(compile_section(item, index, read_text)?);
    }
    Ok(Patch { sections, warnings: Vec::new() })
}

enum Anchor {
    Symbol(String),
    Lines(Range),
    Block(usize),
}

fn compile_section(
    op: &Value,
    index: usize,
    read_text: &mut dyn FnMut(&str) -> Result<String, String>,
) -> Result<Section, String> {
    if let Some(name) = op.get("op").and_then(Value::as_str) {
        if name != "mutate.edit" {
            return Err(format!("edit {}: op must be \"mutate.edit\", got {name:?}", index + 1));
        }
    }
    let path = op
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("edit {}: \"path\" is required", index + 1))?
        .to_string();
    let tag = op
        .get("tag")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            format!("edit {}: \"tag\" is required (from your latest read; \"new\" creates the file)", index + 1)
        })?
        .to_string();
    let edits = op
        .get("edits")
        .and_then(Value::as_array)
        .filter(|list| !list.is_empty())
        .ok_or_else(|| format!("edit {}: \"edits\" must be a non-empty array", index + 1))?;
    let mut symbols: Option<Vec<(String, String, usize)>> = None;
    let mut hunks: Vec<SourcedHunk> = Vec::new();
    let mut file_op: Option<FileOp> = None;
    for (at, edit) in edits.iter().enumerate() {
        let line = at + 1;
        if let Some(target) = edit.get("replace") {
            let body = body_lines(edit, "with", line)?;
            let hunk = match anchor(target, line)? {
                Anchor::Symbol(name) => Hunk::ReplaceBlock {
                    anchor: symbol_line(&path, &tag, &name, &mut symbols, read_text, line)?,
                    body,
                },
                Anchor::Lines(range) => Hunk::Replace { range, body },
                Anchor::Block(n) => Hunk::ReplaceBlock { anchor: n, body },
            };
            hunks.push(SourcedHunk { hunk, line });
        } else if let Some(target) = edit.get("insert") {
            let body = body_lines(edit, "body", line)?;
            let gap = insert_gap(target, line)?;
            hunks.push(SourcedHunk { hunk: Hunk::Insert { gap, body }, line });
        } else if let Some(target) = edit.get("delete") {
            let hunk = match anchor(target, line)? {
                Anchor::Symbol(name) => Hunk::CutBlock {
                    anchor: symbol_line(&path, &tag, &name, &mut symbols, read_text, line)?,
                },
                Anchor::Lines(range) => Hunk::Cut { range },
                Anchor::Block(n) => Hunk::CutBlock { anchor: n },
            };
            hunks.push(SourcedHunk { hunk, line });
        } else if let Some(target) = edit.get("move") {
            let dest = target
                .get("to")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("edit {line}: move needs {{\"to\": \"path\"}}"))?;
            set_file_op(&mut file_op, FileOp::Mv(dest.to_string()), line)?;
        } else if edit.get("remove").and_then(Value::as_bool) == Some(true) {
            set_file_op(&mut file_op, FileOp::Rem, line)?;
        } else {
            return Err(format!(
                "edit {line}: unknown action; use replace, insert, delete, move, or remove"
            ));
        }
    }
    if matches!(file_op, Some(FileOp::Rem)) && !hunks.is_empty() {
        return Err(format!(
            "edit {}: remove cannot combine with other edits on the same file",
            index + 1
        ));
    }
    Ok(Section { path, tag: Some(tag), hunks, file_op, header_line: index + 1 })
}

fn anchor(target: &Value, line: usize) -> Result<Anchor, String> {
    if let Some(name) = target.get("symbol").and_then(Value::as_str) {
        return Ok(Anchor::Symbol(name.to_string()));
    }
    if let Some(lines) = target.get("lines").and_then(Value::as_array) {
        let (Some(a), Some(b)) =
            (lines.first().and_then(Value::as_u64), lines.get(1).and_then(Value::as_u64))
        else {
            return Err(format!("edit {line}: lines anchor is [start, end]"));
        };
        if a == 0 || b < a {
            return Err(format!("edit {line}: lines anchor is [start, end] with 1 <= start <= end"));
        }
        return Ok(Anchor::Lines(Range { start: a as usize, end: b as usize }));
    }
    if let Some(n) = target.get("block").and_then(Value::as_u64) {
        if n == 0 {
            return Err(format!("edit {line}: block anchor is a 1-based line number"));
        }
        return Ok(Anchor::Block(n as usize));
    }
    Err(format!(
        "edit {line}: anchor must be {{\"symbol\":name}}, {{\"lines\":[a,b]}}, or {{\"block\":N}}"
    ))
}

fn insert_gap(target: &Value, line: usize) -> Result<Gap, String> {
    if let Some(before) = target.get("before") {
        return match before {
            Value::String(s) if s == "^" => Ok(Gap::Bof),
            Value::Number(n) => n
                .as_u64()
                .filter(|n| *n >= 1)
                .map(|n| Gap::Before(n as usize))
                .ok_or_else(|| format!("edit {line}: \"before\" is a line number or \"^\"")),
            _ => Err(format!("edit {line}: \"before\" is a line number or \"^\"")),
        };
    }
    if let Some(after) = target.get("after") {
        return match after {
            Value::String(s) if s == "$" => Ok(Gap::Eof),
            Value::Number(n) => n
                .as_u64()
                .filter(|n| *n >= 1)
                .map(|n| Gap::After(n as usize))
                .ok_or_else(|| format!("edit {line}: \"after\" is a line number or \"$\"")),
            _ => Err(format!("edit {line}: \"after\" is a line number or \"$\"")),
        };
    }
    Err(format!("edit {line}: insert anchor is {{\"before\":N|\"^\"}} or {{\"after\":N|\"$\"}}"))
}

fn body_lines(edit: &Value, key: &str, line: usize) -> Result<Vec<String>, String> {
    let text = edit
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("edit {line}: \"{key}\" must be a string"))?;
    if text.is_empty() {
        return Ok(Vec::new());
    }
    Ok(text.split('\n').map(str::to_string).collect())
}

fn set_file_op(slot: &mut Option<FileOp>, op: FileOp, line: usize) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("edit {line}: only one move or remove per file"));
    }
    *slot = Some(op);
    Ok(())
}

fn symbol_line(
    path: &str,
    tag: &str,
    name: &str,
    cache: &mut Option<Vec<(String, String, usize)>>,
    read_text: &mut dyn FnMut(&str) -> Result<String, String>,
    line: usize,
) -> Result<usize, String> {
    if tag == "new" {
        return Err(format!("edit {line}: a symbol anchor cannot target a file being created"));
    }
    if cache.is_none() {
        let text = read_text(path)?;
        let rows = extract(path, &text).symbols;
        *cache = Some(
            rows.into_iter()
                .filter(|row| row.kind != "file")
                .map(|row| (row.name, row.qualified, row.start_line))
                .collect(),
        );
    }
    let rows = cache.as_ref().expect("populated above");
    let mut lines: Vec<usize> = rows
        .iter()
        .filter(|(n, q, _)| n == name || q == name)
        .map(|(_, _, l)| *l)
        .collect();
    lines.sort_unstable();
    lines.dedup();
    match lines.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("edit {line}: no symbol named {name:?} in {path}")),
        many => Err(format!(
            "edit {line}: symbol {name:?} is ambiguous in {path} ({} matches at lines {many:?}); use its qualified name, or a lines or block anchor",
            many.len()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rust_fixture(_: &str) -> Result<String, String> {
        Ok("fn alpha() -> u8 {\n    1\n}\n\nfn beta() -> u8 {\n    2\n}\n".to_string())
    }

    #[test]
    fn symbol_replace_compiles_to_a_block_hunk() {
        let op = json!({"op":"mutate.edit","path":"a.rs","tag":"abc1234","edits":[
            {"replace":{"symbol":"beta"},"with":"fn beta() -> u8 {\n    22\n}"}
        ]});
        let mut reader = |p: &str| rust_fixture(p);
        let patch = compile(&op, &mut reader).expect("compiles");
        assert_eq!(patch.sections.len(), 1);
        match &patch.sections[0].hunks[0].hunk {
            Hunk::ReplaceBlock { anchor, body } => {
                assert_eq!(*anchor, 5);
                assert_eq!(body.len(), 3);
            }
            other => panic!("expected ReplaceBlock, got {other:?}"),
        }
    }

    #[test]
    fn unknown_symbol_is_refused_with_the_path() {
        let op = json!({"op":"mutate.edit","path":"a.rs","tag":"abc1234","edits":[
            {"delete":{"symbol":"gamma"}}
        ]});
        let mut reader = |p: &str| rust_fixture(p);
        let error = compile(&op, &mut reader).expect_err("refused");
        assert!(error.contains("gamma") && error.contains("a.rs"), "{error}");
    }

    #[test]
    fn array_of_ops_compiles_to_multiple_sections() {
        let op = json!([
            {"op":"mutate.edit","path":"a.rs","tag":"abc1234","edits":[
                {"insert":{"after":"$"},"body":"fn tail() {}"}]},
            {"op":"mutate.edit","path":"b.rs","tag":"def5678","edits":[
                {"replace":{"lines":[1,2]},"with":"replaced"}]}
        ]);
        let mut reader = |p: &str| rust_fixture(p);
        let patch = compile(&op, &mut reader).expect("compiles");
        assert_eq!(patch.sections.len(), 2);
        assert!(matches!(patch.sections[1].hunks[0].hunk, Hunk::Replace { .. }));
    }

    #[test]
    fn mutating_a_new_file_by_symbol_is_refused() {
        let op = json!({"op":"mutate.edit","path":"c.rs","tag":"new","edits":[
            {"replace":{"symbol":"alpha"},"with":"x"}]});
        let mut reader = |p: &str| rust_fixture(p);
        assert!(compile(&op, &mut reader).is_err());
    }
}
