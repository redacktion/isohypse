use crate::apply::LineEdit;

const CONTEXT: usize = 3;

pub fn unified_diff_from_edits(path: &str, old: &str, new: &str, edits: &[LineEdit]) -> String {
    if edits.is_empty() {
        return format!("--- {path}\n+++ {path}\n");
    }
    let (old_lines, _) = crate::normalize::borrowed_lines(old);
    let (new_lines, _) = crate::normalize::borrowed_lines(new);
    let mut out = String::with_capacity(256 + edits.iter().map(|e| e.new_bytes + e.old_removed * 40).sum::<usize>());
    out.push_str(&format!("--- {path}\n+++ {path}\n"));

    let mut groups: Vec<Vec<&LineEdit>> = Vec::new();
    for edit in edits {
        match groups.last_mut() {
            Some(group) => {
                let previous = group.last().unwrap();
                let previous_end = previous.old_start + previous.old_removed;
                if edit.old_start <= previous_end + 2 * CONTEXT {
                    group.push(edit);
                } else {
                    groups.push(vec![edit]);
                }
            }
            None => groups.push(vec![edit]),
        }
    }

    let mut delta_before: i64 = 0;
    for group in &groups {
        let group_start_delta = delta_before;
        let first = group.first().unwrap();
        let last = group.last().unwrap();
        let hunk_old_start = first.old_start.saturating_sub(CONTEXT).max(1);
        let last_old_end = last.old_start + last.old_removed;
        let hunk_old_end = (last_old_end + CONTEXT - 1).min(old_lines.len());
        let mut body = String::new();
        let mut old_count = 0usize;
        let mut new_count = 0usize;
        let mut cursor = hunk_old_start;
        for edit in group {
            while cursor < edit.old_start {
                if cursor <= old_lines.len() {
                    body.push(' ');
                    body.push_str(old_lines[cursor - 1]);
                    body.push('\n');
                    old_count += 1;
                    new_count += 1;
                }
                cursor += 1;
            }
            for offset in 0..edit.old_removed {
                body.push('-');
                body.push_str(old_lines[edit.old_start - 1 + offset]);
                body.push('\n');
                old_count += 1;
            }
            let new_start = (edit.old_start as i64 + delta_before) as usize;
            for offset in 0..edit.new_count {
                body.push('+');
                body.push_str(new_lines[new_start - 1 + offset]);
                body.push('\n');
                new_count += 1;
            }
            delta_before += edit.new_count as i64 - edit.old_removed as i64;
            cursor = edit.old_start + edit.old_removed;
        }
        while cursor <= hunk_old_end {
            body.push(' ');
            body.push_str(old_lines[cursor - 1]);
            body.push('\n');
            old_count += 1;
            new_count += 1;
            cursor += 1;
        }
        let hunk_new_start = (hunk_old_start as i64 + group_start_delta).max(1) as usize;
        out.push_str(&format!("@@ -{hunk_old_start},{old_count} +{hunk_new_start},{new_count} @@\n"));
        out.push_str(&body);
    }
    out
}

pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let patch = diffy::create_patch(old, new);
    let mut out = format!("--- {path}\n+++ {path}\n");
    let rendered = patch.to_string();
    let body = rendered
        .lines()
        .skip_while(|line| line.starts_with("--- ") || line.starts_with("+++ "))
        .collect::<Vec<&str>>()
        .join("\n");
    out.push_str(&body);
    if !body.ends_with('\n') {
        out.push('\n');
    }
    out
}
