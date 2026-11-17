//! Read-only views over retained journals and source objects. No migration or replay.
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::Serialize;
use serde_json::{Value, json};

use crate::objects::{ObjectStore, Resolution};
use crate::tag::{DISPLAY_TAG_LENGTH, FULL_TAG_LENGTH, is_valid_tag, tag_matches};

#[derive(Clone, Serialize)]
struct Revision {
    path: String,
    tag: String,
    full_tag: String,
    revision: usize,
    saved_at_unix: Option<u64>,
    journal_updated_at_unix: Option<u64>,
}

struct Query {
    workspace: PathBuf,
    roots: Vec<PathBuf>,
    path: Option<PathBuf>,
    tag: Option<String>,
    since: Option<u64>,
    until: Option<u64>,
    offset: usize,
    max: usize,
}

fn text<'a>(value: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match value.get(key) {
        None => Ok(None),
        Some(Value::String(s)) if !s.is_empty() && !s.contains('\0') => Ok(Some(s)),
        _ => Err(format!("{key} must be nonempty text")),
    }
}

fn number(value: &Value, key: &str) -> Result<Option<u64>, String> {
    value.get(key).map(|v| v.as_u64().filter(|n| *n <= 9_007_199_254_740_991)
        .ok_or_else(|| format!("{key} must be a nonnegative safe integer"))).transpose()
}

fn clean_path(path: &Path, absolute: bool) -> bool {
    path.is_absolute() == absolute
        && path.to_str().is_some_and(|s| !s.contains('\0') && !s.split('/').any(|part| part == "." || part == ".."))
        && path.components().all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
        && path.components().any(|c| matches!(c, Component::Normal(_)))
}

impl Query {
    fn parse(root: &Path, value: &Value) -> Result<Self, String> {
        let workspace = text(value, "workspace")?.map(PathBuf::from).unwrap_or_else(|| root.to_path_buf());
        if !clean_path(&workspace, true) {
            return Err("workspace must be an absolute history root without . or ..".into());
        }
        let mut roots = vec![workspace.clone()];
        if let Ok(canonical) = workspace.canonicalize() {
            if canonical != workspace { roots.push(canonical); }
        }
        let path = text(value, "arg")?.map(PathBuf::from);
        if path.as_ref().is_some_and(|p| !clean_path(p, false)) {
            return Err("arg must be a workspace-relative recorded path without . or ..".into());
        }
        let tag = text(value, "tag")?.map(str::to_ascii_lowercase);
        if tag.as_ref().is_some_and(|tag| !is_valid_tag(tag)) {
            return Err("tag must contain 4–64 hexadecimal characters".into());
        }
        let since = number(value, "since")?;
        let until = number(value, "until")?;
        if since.zip(until).is_some_and(|(a, b)| a > b) {
            return Err("since must not exceed until".into());
        }
        let max = number(value, "max")?.unwrap_or(20);
        if !(1..=200).contains(&max) { return Err("max must be 1–200".into()); }
        Ok(Self {
            workspace, roots, path, tag, since, until,
            offset: number(value, "offset")?.unwrap_or(0) as usize, max: max as usize,
        })
    }

    fn revisions(&self, store: &ObjectStore) -> Vec<Revision> {
        let roots: Vec<&Path> = self.roots.iter().map(PathBuf::as_path).collect();
        let mut out = Vec::new();
        for file in store.journal_overview(&roots) {
            let absolute = Path::new(&file.path);
            let Some(relative) = self.roots.iter().find_map(|r| absolute.strip_prefix(r).ok()) else { continue };
            if !clean_path(relative, false) || self.path.as_ref().is_some_and(|p| p != relative) {
                continue;
            }
            for (revision, full_tag) in file.tags.into_iter().enumerate() {
                if full_tag.len() != FULL_TAG_LENGTH || !is_valid_tag(&full_tag)
                    || self.tag.as_ref().is_some_and(|tag| !tag_matches(tag, &full_tag)) {
                    continue;
                }
                let saved_at_unix = store.saved_at_unix(&full_tag);
                if self.since.is_some() || self.until.is_some() {
                    let Some(time) = saved_at_unix else { continue };
                    if self.since.is_some_and(|since| time < since)
                        || self.until.is_some_and(|until| time > until) { continue; }
                }
                out.push(Revision {
                    path: file.path.clone(), tag: full_tag[..DISPLAY_TAG_LENGTH].to_string(),
                    full_tag, revision, saved_at_unix,
                    journal_updated_at_unix: file.modified.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()),
                });
            }
        }
        out.sort_by(|a, b| b.saved_at_unix.cmp(&a.saved_at_unix)
            .then_with(|| a.path.cmp(&b.path)).then_with(|| b.revision.cmp(&a.revision)));
        out
    }
}

/// List retained revisions. Time filters use storage metadata, not inferred edit times.
pub fn list(root: &Path, value: &Value) -> Result<String, String> {
    let query = Query::parse(root, value)?;
    let store = ObjectStore::open()?;
    let revisions = query.revisions(&store);
    let total = revisions.len();
    let page: Vec<_> = revisions.into_iter().skip(query.offset).take(query.max).collect();
    let next = query.offset.saturating_add(page.len());
    serde_json::to_string(&json!({
        "workspace": query.workspace, "time_basis": "saved object mtime, not per-path edit time",
        "total": total, "offset": query.offset,
        "next_offset": if next < total { Some(next) } else { None },
        "revisions": page,
    })).map(|text| text + "\n").map_err(|e| e.to_string())
}

/// Retrieve a journal-bound object without touching the live file or journal.
pub fn read(root: &Path, value: &Value) -> Result<String, String> {
    let query = Query::parse(root, value)?;
    let cited = query.tag.as_deref().ok_or("saved source needs a tag")?;
    let store = ObjectStore::open()?;
    let revisions = query.revisions(&store);
    let matches: BTreeSet<_> = revisions.iter().map(|r| (r.path.clone(), r.full_tag.clone())).collect();
    if matches.is_empty() {
        return Err(format!("no recorded source matches #{cited} under {}; use context.log to locate it", query.workspace.display()));
    }
    let tags: BTreeSet<_> = matches.iter().map(|(_, tag)| tag).collect();
    if tags.len() > 1 {
        return Err(format!("tag #{cited} is ambiguous across {} revisions; use a longer tag", tags.len()));
    }
    if matches.len() > 1 {
        return Err(format!("tag #{cited} appears at {} paths; supply arg from context.log", matches.len()));
    }
    let (path, full) = matches.into_iter().next().unwrap();
    let content = match store.resolve(&full) {
        Resolution::Found(_, content) => content,
        _ => return Err(format!("journal records [{path}#{cited}], but its saved source is missing or corrupt")),
    };
    let lines: Vec<_> = content.lines().collect();
    let (first, last) = match text(value, "range")? {
        None => (1, lines.len().min(120)),
        Some(range) => {
            let (a, b) = range.split_once('-').ok_or("range must be start-end")?;
            let first: usize = a.parse().map_err(|_| "range must be start-end")?;
            let last: usize = b.parse().map_err(|_| "range must be start-end")?;
            if first == 0 || last < first { return Err("range must be start-end".into()); }
            (first, last.min(lines.len()))
        }
    };
    let outline = match value.get("outline") {
        None => false,
        Some(Value::Bool(value)) => *value,
        _ => return Err("outline must be boolean".into()),
    };
    let elisions = if outline { crate::blocks::outline_elisions(&path, &content) } else { Vec::new() };
    let mut out = format!("[{path}#{}]\n", &full[..DISPLAY_TAG_LENGTH]);
    let mut shown = BTreeSet::new();
    let mut line = first;
    while line <= last {
        if let Some(elision) = elisions.iter().find(|e| line >= e.first && line <= e.last) {
            let _ = writeln!(out, "{}-{}: … ({})", line, elision.last.min(last), elision.label);
            line = elision.last + 1;
        } else {
            let _ = writeln!(out, "{line}:{}", lines[line - 1]);
            shown.insert(line);
            line += 1;
        }
    }
    store.record_seen(&full, &shown)?;
    let _ = writeln!(out, "(saved revision; full tag {full}; showing {first}..{last} of {} lines; live source unchanged)", lines.len());
    Ok(out)
}
