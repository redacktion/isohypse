use std::collections::BTreeSet;
use std::collections::HashMap;

use crate::blocks::{resolve_block, BlockSpan};
use crate::patch::{Gap, Hunk, Range, Section, SourcedHunk};

#[derive(Clone, Copy, Debug)]
pub struct BlockResolution {
    pub anchor_line: usize,
    pub start: usize,
    pub end: usize,
}

#[derive(Debug)]
pub struct SectionApply {
    pub text: String,
    pub first_changed_line: Option<usize>,
    pub warnings: Vec<String>,
    pub block_resolutions: Vec<BlockResolution>,
    pub line_edits: Vec<LineEdit>,
}

#[derive(Clone, Copy, Debug)]
pub struct LineEdit {
    pub old_start: usize,
    pub old_removed: usize,
    pub new_count: usize,
    pub new_bytes: usize,
}

#[derive(Debug)]
pub struct ApplyError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

impl std::error::Error for ApplyError {}

fn err(line: usize, message: impl Into<String>) -> ApplyError {
    ApplyError { line, message: message.into() }
}

enum ConcreteOp {
    Replace { range: Range, body: Vec<String> },
    Insert { gap: Gap, body: Vec<String> },
    Delete { range: Range },
}

struct SourcedOp {
    op: ConcreteOp,
    line: usize,
}

fn resolve_section_block(
    path: &str,
    text: &str,
    anchor: usize,
    op_line: usize,
    op_name: &str,
) -> Result<BlockSpan, ApplyError> {
    let span = resolve_block(path, text, anchor).ok_or_else(|| {
        err(op_line, format!(
            "`{op_name} {anchor}*` did not resolve: no syntactic block begins on line {anchor}. Anchor the block's opening line, or use a plain line range"
        ))
    })?;
    if span.start == span.end {
        return Err(err(op_line, format!(
            "line {anchor} is a single statement, not the opening line of a multi-line block; use the plain op on line {anchor} instead of `{anchor}*`"
        )));
    }
    Ok(span)
}

pub fn apply_section(
    section: &Section,
    text: &str,
    seen_lines: Option<&crate::objects::SeenRanges>,
) -> Result<SectionApply, ApplyError> {
    let warnings = Vec::new();
    let mut block_resolutions = Vec::new();
    let mut ops: Vec<SourcedOp> = Vec::new();

    for SourcedHunk { hunk, line } in &section.hunks {
        let line = *line;
        match hunk {
            Hunk::Replace { range, body } => {
                ops.push(SourcedOp { op: ConcreteOp::Replace { range: *range, body: body.clone() }, line });
            }
            Hunk::ReplaceBlock { anchor, body } => {
                let span = resolve_section_block(&section.path, text, *anchor, line, "replace")?;
                block_resolutions.push(BlockResolution { anchor_line: *anchor, start: span.start, end: span.end });
                let range = Range { start: span.start, end: span.end };
                ops.push(SourcedOp { op: ConcreteOp::Replace { range, body: body.clone() }, line });
            }
            Hunk::Insert { gap, body } => {
                ops.push(SourcedOp { op: ConcreteOp::Insert { gap: gap.clone(), body: body.clone() }, line });
            }
            Hunk::Cut { range } => {
                slice_range(text, *range, line, "delete")?;
                ops.push(SourcedOp { op: ConcreteOp::Delete { range: *range }, line });
            }
            Hunk::CutBlock { anchor } => {
                let span = resolve_section_block(&section.path, text, *anchor, line, "delete")?;
                block_resolutions.push(BlockResolution { anchor_line: *anchor, start: span.start, end: span.end });
                let range = Range { start: span.start, end: span.end };
                slice_range(text, range, line, "delete")?;
                ops.push(SourcedOp { op: ConcreteOp::Delete { range }, line });
            }
        }
    }

    materialize(text, &ops, seen_lines, warnings, block_resolutions)
}

fn slice_range(text: &str, range: Range, op_line: usize, description: &str) -> Result<Vec<String>, ApplyError> {
    let (lines, _) = crate::normalize::borrowed_lines(text);
    if range.start < 1 || range.end > lines.len() {
        return Err(err(op_line, format!(
            "`{description}` is out of range (file has {} lines)",
            lines.len()
        )));
    }
    Ok(lines[range.start - 1..range.end].iter().map(|l| l.to_string()).collect())
}

fn touched_original_lines(op: &ConcreteOp) -> Vec<usize> {
    match op {
        ConcreteOp::Replace { range, .. } | ConcreteOp::Delete { range } => (range.start..=range.end).collect(),
        ConcreteOp::Insert { gap, .. } => match gap {
            Gap::Before(line) | Gap::After(line) => vec![*line],
            Gap::Bof | Gap::Eof => Vec::new(),
        },
    }
}

fn materialize(
    text: &str,
    ops: &[SourcedOp],
    seen_lines: Option<&crate::objects::SeenRanges>,
    warnings: Vec<String>,
    block_resolutions: Vec<BlockResolution>,
) -> Result<SectionApply, ApplyError> {
    let (file_lines, source_trailing_newline) = crate::normalize::borrowed_lines(text);
    let line_count = file_lines.len();

    let mut deleted: HashMap<usize, usize> = HashMap::new();
    for sourced in ops {
        for anchor in touched_original_lines(&sourced.op) {
            if anchor < 1 || anchor > line_count {
                return Err(err(sourced.line, format!(
                    "line {anchor} does not exist (file has {line_count} lines)"
                )));
            }
            if let Some(seen) = seen_lines {
                if !seen.contains(anchor) {
                    return Err(err(sourced.line, format!(
                        "line {anchor} was not displayed under this tag; re-read the range before editing it"
                    )));
                }
            }
        }
        if let ConcreteOp::Replace { range, .. } | ConcreteOp::Delete { range } = &sourced.op {
            for anchor in range.start..=range.end {
                if let Some(previous) = deleted.insert(anchor, sourced.line) {
                    return Err(err(sourced.line, format!(
                        "line {anchor} is already targeted by the hunk on patch line {previous}; issue one hunk per range"
                    )));
                }
            }
        }
    }

    let mut before: HashMap<usize, Vec<&str>> = HashMap::new();
    let mut after: HashMap<usize, Vec<&str>> = HashMap::new();
    let mut bof: Vec<&str> = Vec::new();
    let mut eof: Vec<&str> = Vec::new();
    let mut removed: BTreeSet<usize> = BTreeSet::new();
    let mut line_edits: Vec<LineEdit> = Vec::with_capacity(ops.len());
    let mut first_changed: Option<usize> = None;
    let mut note_change = |line: usize| {
        first_changed = Some(first_changed.map_or(line, |current| current.min(line)));
    };

    for sourced in ops {
        match &sourced.op {
            ConcreteOp::Replace { range, body } => {
                before
                    .entry(range.start)
                    .or_default()
                    .extend(body.iter().map(String::as_str));
                for anchor in range.start..=range.end {
                    removed.insert(anchor);
                }
                line_edits.push(LineEdit {
                    old_start: range.start,
                    old_removed: range.end - range.start + 1,
                    new_count: body.len(),
                    new_bytes: body.iter().map(|row| row.len() + 1).sum(),
                });
                note_change(range.start);
            }
            ConcreteOp::Delete { range } => {
                for anchor in range.start..=range.end {
                    removed.insert(anchor);
                }
                line_edits.push(LineEdit {
                    old_start: range.start,
                    old_removed: range.end - range.start + 1,
                    new_count: 0,
                    new_bytes: 0,
                });
                note_change(range.start);
            }
            ConcreteOp::Insert { gap, body } => {
                let (position, insertion_start) = match gap {
                    Gap::Bof => {
                        bof.extend(body.iter().map(String::as_str));
                        (1, 1)
                    }
                    Gap::Eof => {
                        eof.extend(body.iter().map(String::as_str));
                        (line_count + 1, line_count + 1)
                    }
                    Gap::Before(line) => {
                        before.entry(*line).or_default().extend(body.iter().map(String::as_str));
                        (*line, *line)
                    }
                    Gap::After(line) => {
                        after.entry(*line).or_default().extend(body.iter().map(String::as_str));
                        (*line + 1, *line + 1)
                    }
                };
                line_edits.push(LineEdit { old_start: insertion_start, old_removed: 0, new_count: body.len(), new_bytes: body.iter().map(|row| row.len() + 1).sum() });
                note_change(position);
            }
        }
    }
    line_edits.sort_by_key(|e| e.old_start);

    let mut out = String::with_capacity(text.len() + 1024);
    let mut emitted = 0usize;
    let push_row = |out: &mut String, row: &str, emitted: &mut usize| {
        out.push_str(row);
        out.push('\n');
        *emitted += 1;
    };
    for row in &bof {
        push_row(&mut out, row, &mut emitted);
    }
    for (index, line) in file_lines.iter().enumerate() {
        let number = index + 1;
        if let Some(rows) = before.get(&number) {
            for row in rows {
                push_row(&mut out, row, &mut emitted);
            }
        }
        if !removed.contains(&number) {
            push_row(&mut out, line, &mut emitted);
        }
        if let Some(rows) = after.get(&number) {
            for row in rows {
                push_row(&mut out, row, &mut emitted);
            }
        }
    }
    for row in &eof {
        push_row(&mut out, row, &mut emitted);
    }

    let trailing_newline = source_trailing_newline || (file_lines.is_empty() && emitted > 0);
    if !trailing_newline && emitted > 0 {
        out.pop();
    }
    Ok(SectionApply {
        text: out,
        first_changed_line: first_changed,
        warnings,
        block_resolutions,
        line_edits,
    })
}
