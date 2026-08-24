use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::apply::{apply_section, BlockResolution, SectionApply};
use crate::normalize::{detect_line_ending, normalize_to_lf, restore_line_endings, split_lines, strip_bom, LineEnding};
use crate::objects::{ObjectStore, Resolution};
use crate::patch::{FileOp, Patch};
use crate::preview::unified_diff;
use crate::recovery::{replay_onto_live, Recovery};
use crate::tag::{full_tag, tag_matches, DISPLAY_TAG_LENGTH};

#[derive(Debug)]
pub struct SectionResult {
    pub path: String,
    pub op: String,
    pub dest: Option<String>,
    pub new_tag: Option<String>,
    pub previous_tag: String,
    pub first_changed_line: Option<usize>,
    pub shifts: Vec<(usize, i64)>,
    pub preview: String,
    pub warnings: Vec<String>,
    pub block_resolutions: Vec<BlockResolution>,
    pub structural: Vec<String>,
    pub recovered: bool,
}

#[derive(Debug)]
pub struct ApplyReport {
    pub sections: Vec<SectionResult>,
    pub warnings: Vec<String>,
}

pub enum MultiOpAction {
    Applied(ApplyReport),
    Created(String),
}

#[derive(Default)]
pub struct MultiOpLedger {
    pub actions: Vec<MultiOpAction>,
}

pub type SharedAnalysisCache = std::sync::Arc<std::sync::Mutex<HashMap<String, std::sync::Arc<Analysis>>>>;

pub struct Patcher {
    root: PathBuf,
    canonical_root: PathBuf,
    pub objects: ObjectStore,
    analysis_cache: SharedAnalysisCache,
}

pub struct Analysis {
    first_error: Option<usize>,
    functions: crate::graph::extract::FnIndex,
    tree: tree_sitter::Tree,
}

const ANALYSIS_CACHE_CAP: usize = 64;

fn line_byte_starts(text: &str) -> Vec<usize> {
    let mut starts = Vec::with_capacity(text.len() / 24 + 2);
    starts.push(0);
    for (offset, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(offset + 1);
        }
    }
    starts
}

fn edited_old_tree(
    old_tree: &tree_sitter::Tree,
    old_text: &str,
    edits: &[crate::apply::LineEdit],
) -> Option<tree_sitter::Tree> {
    if !old_text.ends_with('\n') || edits.is_empty() {
        return None;
    }
    let starts = line_byte_starts(old_text);
    let mut tree = old_tree.clone();
    let mut byte_delta: i64 = 0;
    let mut row_delta: i64 = 0;
    for edit in edits {
        let start_original = *starts.get(edit.old_start - 1)?;
        let old_end_original = *starts.get(edit.old_start - 1 + edit.old_removed)?;
        let start_byte = usize::try_from(start_original as i64 + byte_delta).ok()?;
        let old_end_byte = usize::try_from(old_end_original as i64 + byte_delta).ok()?;
        let new_end_byte = start_byte + edit.new_bytes;
        let start_row = usize::try_from(edit.old_start as i64 - 1 + row_delta).ok()?;
        tree.edit(&tree_sitter::InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position: tree_sitter::Point { row: start_row, column: 0 },
            old_end_position: tree_sitter::Point { row: start_row + edit.old_removed, column: 0 },
            new_end_position: tree_sitter::Point { row: start_row + edit.new_count, column: 0 },
        });
        byte_delta += edit.new_bytes as i64 - (old_end_original - start_original) as i64;
        row_delta += edit.new_count as i64 - edit.old_removed as i64;
    }
    Some(tree)
}

fn write_nofollow(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let stem = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    let unique = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".isohypse.tmp.{}.{}.{}", std::process::id(), unique, stem));
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    if let Err(e) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Ok(meta) = fs::symlink_metadata(path) {
        if meta.file_type().is_file() {
            let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(meta.permissions().mode()));
        }
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn write_shaped(path: &Path, text: &str, shape: &FileShape) -> std::io::Result<()> {
    if shape.bom.is_empty() && shape.ending == LineEnding::Lf {
        return write_nofollow(path, text.as_bytes());
    }
    write_nofollow(path, format!("{}{}", shape.bom, restore_line_endings(text, shape.ending)).as_bytes())
}

fn build_analysis(key: &str, text: &str, tree: tree_sitter::Tree) -> Analysis {
    Analysis {
        first_error: crate::blocks::first_parse_error_in(&tree),
        functions: crate::graph::extract::fn_signatures(key, text, &tree),
        tree,
    }
}

struct FileShape {
    bom: String,
    ending: LineEnding,
}

enum StagedOp {
    Write { text: String },
    Move { dest: String, text: String },
    Remove,
}

struct StagedWrite {
    path: String,
    op: StagedOp,
    shape: FileShape,
    base_tag: Option<String>,
}

impl Patcher {
    pub fn new(root: impl Into<PathBuf>) -> Result<Patcher, String> {
        Patcher::with_shared_cache(root, SharedAnalysisCache::default())
    }

    pub fn with_shared_cache(root: impl Into<PathBuf>, cache: SharedAnalysisCache) -> Result<Patcher, String> {
        let root = root.into();
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        Ok(Patcher {
            root,
            canonical_root,
            objects: ObjectStore::open()?,
            analysis_cache: cache,
        })
    }

    fn analysis_for(
        &mut self,
        key: &str,
        text: &str,
        tag: &str,
        reuse: Option<(&Analysis, &str, &[crate::apply::LineEdit])>,
    ) -> Option<std::sync::Arc<Analysis>> {
        let cache_key = format!("{key}#{tag}");
        if let Some(hit) = self.analysis_cache.lock().ok()?.get(&cache_key) {
            return Some(hit.clone());
        }
        let mut fast_functions: Option<crate::graph::extract::FnIndex> = None;
        let tree = reuse
            .and_then(|(old, old_text, edits)| {
                let edited = edited_old_tree(&old.tree, old_text, edits)?;
                let reparsed = crate::blocks::reparse_with(key, text, &edited)?;
                if reparsed.root_node().end_byte() != text.len() || reparsed.root_node().has_error() {
                    return None;
                }
                if old.first_error.is_none() {
                    if let Some((functions, _)) =
                        crate::graph::extract::single_function_delta(&edited, &reparsed, key, text, &old.functions)
                    {
                        fast_functions = Some(functions);
                    }
                }
                Some(reparsed)
            })
            .or_else(|| crate::blocks::parse_tree(key, text))?;
        let analysis = std::sync::Arc::new(match fast_functions {
            Some(functions) => Analysis {
                first_error: crate::blocks::first_parse_error_in(&tree),
                functions,
                tree,
            },
            None => build_analysis(key, text, tree),
        });
        if let Ok(mut cache) = self.analysis_cache.lock() {
            if cache.len() >= ANALYSIS_CACHE_CAP {
                cache.clear();
            }
            cache.insert(cache_key, std::sync::Arc::clone(&analysis));
        }
        Some(analysis)
    }

    fn analyses_for_change(
        &mut self,
        key: &str,
        old_text: &str,
        old_tag: &str,
        new_text: &str,
        new_tag: &str,
        edits: &[crate::apply::LineEdit],
    ) -> (Option<std::sync::Arc<Analysis>>, Option<std::sync::Arc<Analysis>>) {
        let old = self.analysis_for(key, old_text, old_tag, None);
        let new = self.analysis_for(
            key,
            new_text,
            new_tag,
            old.as_deref().map(|old| (old, old_text, edits)),
        );
        (old, new)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn absolute(&self, path: &str) -> PathBuf {
        let candidate = Path::new(path);
        if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        }
    }

    fn canonical_key(&self, path: &str) -> String {
        let absolute = self.absolute(path);
        match absolute.strip_prefix(&self.root) {
            Ok(relative) => relative.to_string_lossy().into_owned(),
            Err(_) => absolute.to_string_lossy().into_owned(),
        }
    }

    fn guard(&self, path: &str) -> Result<(), String> {
        ensure_within(&self.canonical_root, path)
    }

    pub fn read(&mut self, path: &str, start: Option<usize>, end: Option<usize>) -> Result<String, String> {
        self.read_with(path, start, end, false, None)
    }

    pub fn read_with(
        &mut self,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
        outline: bool,
        max_bytes: Option<usize>,
    ) -> Result<String, String> {
        use std::fmt::Write as _;
        self.guard(path)?;
        let key = self.canonical_key(path);
        let raw = fs::read_to_string(self.absolute(path)).map_err(|e| format!("cannot read {key}: {e}"))?;
        let stripped = strip_bom(&raw);
        let normalized = crate::normalize::normalize_to_lf_cow(stripped.text);
        let (lines, _) = crate::normalize::borrowed_lines(&normalized);
        let total = lines.len();
        let first = start.unwrap_or(1).max(1);
        let last = end.unwrap_or(total).min(total);
        let elisions = if outline {
            crate::blocks::outline_elisions(&key, &normalized)
        } else {
            Vec::new()
        };
        let elided: BTreeSet<usize> = elisions.iter().flat_map(|e| e.first..=e.last).collect();
        let full = self.objects.put(&normalized)?;
        self.objects.journal_record(&self.absolute(path).to_string_lossy(), &full)?;
        let mut out = String::with_capacity(normalized.len() + total * 8 + 64);
        let _ = writeln!(out, "[{key}#{}]", &full[..DISPLAY_TAG_LENGTH]);
        let mut shown: BTreeSet<usize> = BTreeSet::new();
        let mut capped_at: Option<usize> = None;
        let mut line_number = first;
        while line_number <= last && line_number <= total {
            if max_bytes.map(|cap| out.len() >= cap).unwrap_or(false) {
                capped_at = Some(line_number);
                break;
            }
            if let Some(elision) = elisions.iter().find(|e| line_number >= e.first && line_number <= e.last) {
                let visible_last = elision.last.min(last).min(total);
                let _ = writeln!(out, "{line_number}-{visible_last}: … ({})", elision.label);
                line_number = elision.last + 1;
                continue;
            }
            if !elided.contains(&line_number) {
                let _ = write!(out, "{line_number}:");
                out.push_str(lines[line_number - 1]);
                out.push('\n');
                shown.insert(line_number);
            }
            line_number += 1;
        }
        self.objects.record_seen(&full, &shown)?;
        if let Some(stopped) = capped_at {
            out.push_str(&format!(
                "(capped at {} bytes before line {stopped}; lines {stopped}..{last} are unseen — read that range before editing it)\n",
                max_bytes.unwrap_or(0)
            ));
        }
        if total == 0 {
            out.push_str("(empty file)\n");
        } else if first > 1 || last < total {
            out.push_str(&format!("(showing {first}..{last} of {total} lines)\n"));
        }
        if outline && !elisions.is_empty() {
            out.push_str("(outline: elided regions are unseen; read the range before editing inside one)\n");
        }
        Ok(out)
    }

    pub fn read_multi(
        &mut self,
        path: &str,
        ranges: &[(Option<usize>, Option<usize>)],
        outline: bool,
        max_bytes: Option<usize>,
    ) -> Result<String, String> {
        use std::fmt::Write as _;
        if ranges.len() <= 1 {
            let (start, end) = ranges.first().copied().unwrap_or((None, None));
            return self.read_with(path, start, end, outline, max_bytes);
        }
        self.guard(path)?;
        let key = self.canonical_key(path);
        let raw = fs::read_to_string(self.absolute(path)).map_err(|e| format!("cannot read {key}: {e}"))?;
        let stripped = strip_bom(&raw);
        let normalized = crate::normalize::normalize_to_lf_cow(stripped.text);
        let (lines, _) = crate::normalize::borrowed_lines(&normalized);
        let total = lines.len();
        let elisions = if outline {
            crate::blocks::outline_elisions(&key, &normalized)
        } else {
            Vec::new()
        };
        let elided: BTreeSet<usize> = elisions.iter().flat_map(|e| e.first..=e.last).collect();
        let bounds: Vec<(usize, usize)> = ranges
            .iter()
            .map(|(start, end)| (start.unwrap_or(1).max(1), end.unwrap_or(total).min(total)))
            .collect();
        let full = self.objects.put(&normalized)?;
        self.objects.journal_record(&self.absolute(path).to_string_lossy(), &full)?;
        let mut out = String::with_capacity(normalized.len() + 64);
        let _ = writeln!(out, "[{key}#{}]", &full[..DISPLAY_TAG_LENGTH]);
        let mut shown: BTreeSet<usize> = BTreeSet::new();
        let mut capped_at: Option<usize> = None;
        'ranges: for (index, (first, last)) in bounds.iter().enumerate() {
            if index > 0 {
                out.push_str("  ⋮\n");
            }
            let mut line_number = *first;
            while line_number <= *last && line_number <= total {
                if max_bytes.map(|cap| out.len() >= cap).unwrap_or(false) {
                    capped_at = Some(line_number);
                    break 'ranges;
                }
                if let Some(elision) = elisions.iter().find(|e| line_number >= e.first && line_number <= e.last) {
                    let visible_last = elision.last.min(*last).min(total);
                    let _ = writeln!(out, "{line_number}-{visible_last}: … ({})", elision.label);
                    line_number = elision.last + 1;
                    continue;
                }
                if !elided.contains(&line_number) {
                    let _ = write!(out, "{line_number}:");
                    out.push_str(lines[line_number - 1]);
                    out.push('\n');
                    shown.insert(line_number);
                }
                line_number += 1;
            }
        }
        self.objects.record_seen(&full, &shown)?;
        if let Some(stopped) = capped_at {
            out.push_str(&format!(
                "(capped at {} bytes before line {stopped}; the remaining requested lines are unseen — read them before editing)\n",
                max_bytes.unwrap_or(0)
            ));
        }
        if total == 0 {
            out.push_str("(empty file)\n");
        } else {
            out.push_str(&format!("(showing {} ranges of {total} lines)\n", bounds.len()));
        }
        Ok(out)
    }

    pub fn log(&self, path: &str) -> Result<String, String> {
        self.guard(path)?;
        let key = self.canonical_key(path);
        let abs = self.absolute(path).to_string_lossy().into_owned();
        let entries = self.objects.journal_entries(&abs);
        if entries.is_empty() {
            return Ok(format!("no recorded history for {key}\n"));
        }
        let live_full = fs::read_to_string(self.absolute(path))
            .ok()
            .map(|raw| full_tag(&normalize_to_lf(strip_bom(&raw).text)));
        let mut out = String::new();
        for (position, entry) in entries.iter().enumerate() {
            let marker = if live_full.as_deref() == Some(entry) { "  <- live" } else { "" };
            out.push_str(&format!("{position}: #{}{marker}\n", &entry[..DISPLAY_TAG_LENGTH]));
        }
        Ok(out)
    }

    pub fn changelog(&self) -> Result<String, String> {
        let entries = self.objects.journal_overview(&[&self.canonical_root, &self.root]);
        if entries.is_empty() {
            return Ok(format!("no recorded changes under {}\n", self.root.display()));
        }
        let canonical_prefix = format!("{}/", self.canonical_root.display());
        let plain_prefix = format!("{}/", self.root.display());
        let mut out = format!("recent changes under {} (newest first):\n", self.root.display());
        for entry in entries.iter().take(20) {
            let relative = entry
                .path
                .strip_prefix(&canonical_prefix)
                .or_else(|| entry.path.strip_prefix(&plain_prefix))
                .unwrap_or(entry.path.as_str());
            let live = fs::read_to_string(&entry.path)
                .ok()
                .map(|raw| full_tag(&normalize_to_lf(strip_bom(&raw).text)));
            let latest = entry.tags.last().cloned().unwrap_or_default();
            let marker = if live.as_deref() == Some(latest.as_str()) {
                "  (live)"
            } else {
                " (diverged or deleted)"
            };
            let chain = entry
                .tags
                .iter()
                .rev()
                .take(3)
                .map(|tag| format!("#{}", &tag[..DISPLAY_TAG_LENGTH.min(tag.len())]))
                .collect::<Vec<String>>()
                .join(" <- ");
            out.push_str(&format!("  {relative}  {} version(s)  {chain}{marker}\n", entry.tags.len()));
        }
        if entries.len() > 20 {
            out.push_str(&format!("({} more files with history)\n", entries.len() - 20));
        }
        Ok(out)
    }

    pub fn undo(&mut self, path: &str) -> Result<String, String> {
        self.undo_with(path, None, false)
    }

    pub fn undo_with(&mut self, path: &str, tag: Option<&str>, recover: bool) -> Result<String, String> {
        self.guard(path)?;
        let key = self.canonical_key(path);
        let abs = self.absolute(path);
        let abs_key = abs.to_string_lossy().into_owned();
        let raw = fs::read_to_string(&abs).map_err(|e| format!("cannot read {key}: {e}"))?;
        let bom = strip_bom(&raw);
        let ending = detect_line_ending(bom.text);
        let bom_prefix = bom.bom.to_string();
        let live_full = full_tag(&normalize_to_lf(bom.text));
        let entries = self.objects.journal_entries(&abs_key);
        let target: String = if let Some(tag) = tag {
            entries
                .iter()
                .rev()
                .find(|entry| crate::tag::tag_matches(tag, entry))
                .cloned()
                .ok_or_else(|| format!("no recorded version of {key} matches #{tag}"))?
        } else if recover {
            let mut chosen = None;
            for entry in entries.iter().rev() {
                if let Resolution::Found(_, content) = self.objects.resolve(entry) {
                    if crate::blocks::all_parse_errors(&key, &content).is_empty() {
                        chosen = Some(entry.clone());
                        break;
                    }
                }
            }
            chosen.ok_or_else(|| format!("no parse-valid recorded version of {key} found"))?
        } else {
            entries
                .iter()
                .rev()
                .find(|entry| **entry != live_full)
                .cloned()
                .ok_or_else(|| format!("no earlier version of {key} is recorded"))?
        };
        let Resolution::Found(full, content) = self.objects.resolve(&target) else {
            return Err(format!("recorded version #{} is no longer stored", &target[..DISPLAY_TAG_LENGTH.min(target.len())]));
        };
        let restored = format!("{bom_prefix}{}", restore_line_endings(&content, ending));
        write_nofollow(&abs, restored.as_bytes()).map_err(|e| format!("cannot write {key}: {e}"))?;
        self.objects.journal_record(&abs_key, &full)?;
        let all: BTreeSet<usize> = (1..=split_lines(&content).lines.len()).collect();
        self.objects.record_seen(&full, &all)?;
        Ok(format!(
            "restored [{key}#{}] (was #{})\n",
            &full[..DISPLAY_TAG_LENGTH],
            &live_full[..DISPLAY_TAG_LENGTH]
        ))
    }

    pub fn create(&mut self, path: &str, content: &str) -> Result<String, String> {
        self.guard(path)?;
        let key = self.canonical_key(path);
        let absolute = self.absolute(path);
        if absolute.exists() {
            return Err(format!(
                "[{key}] already exists; mutate.create only authors new files — change it with mutate.edit"
            ));
        }
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let normalized = normalize_to_lf(strip_bom(content).text);
        let body = if normalized.is_empty() || normalized.ends_with('\n') {
            normalized
        } else {
            format!("{normalized}\n")
        };
        write_nofollow(&absolute, body.as_bytes()).map_err(|e| format!("cannot write {key}: {e}"))?;
        self.record_applied(&key, &body)?;
        let full = full_tag(&body);
        Ok(format!("created [{key}#{}]\n", &full[..DISPLAY_TAG_LENGTH]))
    }


    pub fn compile_edit_op(&mut self, op: &serde_json::Value) -> Result<Patch, String> {
        let root = self.root.clone();
        let canonical_root = self.canonical_root.clone();
        crate::edit::compile(op, &mut |path: &str| {
            ensure_within(&canonical_root, path)?;
            std::fs::read_to_string(root.join(path)).map_err(|e| format!("cannot read {path}: {e}"))
        })
    }

    pub fn apply_edits(&mut self, op: &serde_json::Value) -> Result<ApplyReport, String> {
        let patch = self.compile_edit_op(op)?;
        self.apply_inner(patch, true)
    }

    pub fn check_edits(&mut self, op: &serde_json::Value) -> Result<ApplyReport, String> {
        let patch = self.compile_edit_op(op)?;
        self.apply_inner(patch, false)
    }

    pub fn apply_edits_verified(&mut self, op: &serde_json::Value, verify: Option<&str>) -> Result<ApplyReport, String> {
        let patch = self.compile_edit_op(op)?;
        self.apply_patch_verified(patch, verify)
    }

    pub fn apply_patch_verified(&mut self, patch: Patch, verify: Option<&str>) -> Result<ApplyReport, String> {
        let report = self.apply_inner(patch, true)?;
        let Some(command) = verify else { return Ok(report) };
        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(&self.root)
            .output()
            .map_err(|e| format!("verify command failed to start: {e}"))?;
        if output.status.success() {
            return Ok(report);
        }
        let mut detail = String::from_utf8_lossy(&output.stdout).into_owned();
        detail.push_str(&String::from_utf8_lossy(&output.stderr));
        self.revert(&report)?;
        Err(format!(
            "verify command `{command}` failed; every section was reverted to its pre-apply version.\n--- verify output ---\n{}",
            crate::buildspec::head_tail(&detail)
        ))
    }

    fn revert(&mut self, report: &ApplyReport) -> Result<(), String> {
        self.revert_sections(&report.sections)
    }

    fn revert_sections(&mut self, sections: &[SectionResult]) -> Result<(), String> {
        for section in sections.iter().rev() {
            if section.op == "create" {
                let _ = fs::remove_file(self.absolute(&section.path));
                continue;
            }
            let Resolution::Found(_, content) = self.objects.resolve(&section.previous_tag) else {
                return Err(format!(
                    "cannot revert {}: its pre-apply version is not in the store",
                    section.path
                ));
            };
            if let Some(dest) = &section.dest {
                let _ = fs::remove_file(self.absolute(dest));
            }
            let absolute = self.absolute(&section.path);
            if let Some(parent) = absolute.parent() {
                let _ = fs::create_dir_all(parent);
            }
            write_nofollow(&absolute, content.as_bytes()).map_err(|e| format!("cannot revert {}: {e}", section.path))?;
            self.record_applied(&section.path, &content)?;
        }
        Ok(())
    }

    pub fn revert_ledger(&mut self, ledger: &MultiOpLedger) -> Result<(), String> {
        for action in ledger.actions.iter().rev() {
            match action {
                MultiOpAction::Applied(report) => self.revert(report)?,
                MultiOpAction::Created(path) => {
                    let _ = fs::remove_file(self.absolute(path));
                }
            }
        }
        Ok(())
    }

    fn apply_inner(&mut self, patch: Patch, write: bool) -> Result<ApplyReport, String> {
        let timing = std::env::var_os("ISOHYPSE_TIMING").is_some();
        let started = std::time::Instant::now();
        let mark = |label: &str, from: &mut std::time::Instant| {
            if timing {
                eprintln!("timing {label}: {:.0} us", from.elapsed().as_micros());
            }
            *from = std::time::Instant::now();
        };
        let mut clock = std::time::Instant::now();
        let mut working: HashMap<String, String> = HashMap::new();
        let mut shapes: HashMap<String, FileShape> = HashMap::new();
        let mut staged: Vec<StagedWrite> = Vec::new();
        let mut freed_in_patch: BTreeSet<String> = BTreeSet::new();
        let mut results: Vec<SectionResult> = Vec::new();
        let mut report_warnings = patch.warnings.clone();

        for section in &patch.sections {
            self.guard(&section.path)?;
            if let Some(FileOp::Mv(dest)) = &section.file_op {
                self.guard(dest)?;
                let dest_key = self.canonical_key(dest);
                let occupied = working.contains_key(&dest_key)
                    || (!freed_in_patch.contains(&dest_key) && self.absolute(dest).exists());
                if occupied {
                    return Err(format!(
                        "MV target [{dest_key}] already exists; MV never overwrites — REM it first or pick a new path"
                    ));
                }
            }
            let key = self.canonical_key(&section.path);
            let cited_is_new = section.tag.as_deref() == Some("new");
            if cited_is_new && section.file_op.is_some() {
                return Err(format!(
                    "[{key}#new] cannot combine file creation with REM or MV; create it first, then move it in a later patch"
                ));
            }
            let live_normalized = match working.get(&key) {
                Some(text) => text.clone(),
                None if cited_is_new => {
                    if self.absolute(&section.path).exists() {
                        return Err(format!(
                            "[{key}#new] refused: the file already exists; read it and cite its real tag"
                        ));
                    }
                    shapes.insert(key.clone(), FileShape { bom: String::new(), ending: LineEnding::Lf });
                    String::new()
                }
                None => {
                    let raw = fs::read_to_string(self.absolute(&section.path)).map_err(|_| {
                        format!(
                            "[{key}] does not exist on disk; cite tag \"new\" in mutate.edit, or author it with mutate.create"
                        )
                    })?;
                    let bom = strip_bom(&raw);
                    let ending = detect_line_ending(bom.text);
                    shapes.insert(key.clone(), FileShape { bom: bom.bom.to_string(), ending });
                    normalize_to_lf(bom.text)
                }
            };
            let shape = shapes
                .get(&key)
                .map(|s| FileShape { bom: s.bom.clone(), ending: s.ending })
                .unwrap_or(FileShape { bom: String::new(), ending: LineEnding::Lf });

            let live_full = full_tag(&live_normalized);
            let cited = section.tag.clone().unwrap_or_default();
            let mut recovered = false;
            let mut section_warnings: Vec<String> = Vec::new();

            mark("load+hash", &mut clock);
            let seen = self.objects.seen_lines(&live_full);
            let applied = if cited_is_new || (tag_matches(&cited, &live_full) && seen.is_some()) {
                apply_section(section, &live_normalized, seen.as_ref())
                    .map_err(|e| format!("[{key}#{cited}] {e}"))?
            } else {
                let (snapshot_full, snapshot_text) = match self.objects.resolve(&cited) {
                    Resolution::Found(full, content) => (full, content),
                    Resolution::Ambiguous(candidates) => {
                        return Err(format!(
                            "[{key}#{cited}] is ambiguous across {} stored objects; cite more characters of the tag: {}",
                            candidates.len(),
                            candidates
                                .iter()
                                .map(|c| c[..DISPLAY_TAG_LENGTH.min(c.len())].to_string())
                                .collect::<Vec<String>>()
                                .join(", ")
                        ))
                    }
                    Resolution::Missing => {
                        return Err(format!(
                            "[{key}#{cited}] is stale: the live file is #{} and no stored object matches that tag. Re-read {key} and re-issue the patch",
                            &live_full[..DISPLAY_TAG_LENGTH]
                        ))
                    }
                };
                let seen = self.objects.seen_lines(&snapshot_full);
                let base_apply = apply_section(section, &snapshot_text, seen.as_ref())
                    .map_err(|e| format!("[{key}#{cited}] {e}"))?;
                if snapshot_text == live_normalized {
                    base_apply
                } else {
                    match replay_onto_live(&snapshot_text, &live_normalized, &base_apply.text) {
                        Recovery::Merged(merged) => {
                            recovered = true;
                            section_warnings.push(format!(
                                "[{key}#{cited}] was stale; the edit was replayed onto the stored snapshot and 3-way merged onto the live #{} content",
                                &live_full[..DISPLAY_TAG_LENGTH]
                            ));
                            SectionApply {
                                text: merged,
                                first_changed_line: base_apply.first_changed_line,
                                warnings: base_apply.warnings,
                                block_resolutions: base_apply.block_resolutions,
                                line_edits: Vec::new(),
                            }
                        }
                        Recovery::Conflict(region) => {
                            return Err(format!(
                                "[{key}#{cited}] is stale and the live file (#{}) diverged where the patch edits; 3-way merge conflicted:\n{region}Re-read {key} and re-issue the patch",
                                &live_full[..DISPLAY_TAG_LENGTH]
                            ));
                        }
                    }
                }
            };

            mark("apply-section", &mut clock);
            section_warnings.extend(applied.warnings.iter().cloned());
            let new_text = applied.text;
            let (op_name, dest) = match &section.file_op {
                Some(FileOp::Rem) => ("delete".to_string(), None),
                Some(FileOp::Mv(dest)) => ("move".to_string(), Some(self.canonical_key(dest))),
                None if cited_is_new => ("create".to_string(), None),
                None => ("update".to_string(), None),
            };

            let new_full = full_tag(&new_text);
            let mut structural = Vec::new();
            if op_name != "delete" {
                let (old_analysis, new_analysis) =
                    self.analyses_for_change(&key, &live_normalized, &live_full, &new_text, &new_full, &applied.line_edits);
                if let (Some(old), Some(new)) = (&old_analysis, &new_analysis) {
                    if old.first_error.is_none() {
                        if let Some(anchor_line) = new.first_error {
                            let error_line = nearest_error_line(&key, &new_text, &applied.line_edits, anchor_line);
                            let context = patched_context(&new_text, error_line);
                            let imbalance = delimiter_imbalances(&live_normalized, &new_text, &applied.line_edits);
                            return Err(format!(
                                "[{key}#{cited}] refused: the file parsed cleanly before this patch and would not parse after it (first parse error near line {error_line}). The patched result there would be:\n{context}{imbalance}Fix the patch body; nothing was written"
                            ));
                        }
                    }
                    structural = crate::graph::extract::structural_diff_indexed(&old.functions, &new.functions);
                }
            }

            mark("analysis", &mut clock);
            let preview = match op_name.as_str() {
                "delete" => format!("--- {key}\n+++ /dev/null\n"),
                _ if !recovered && !applied.line_edits.is_empty() => {
                    crate::preview::unified_diff_from_edits(&key, &live_normalized, &new_text, &applied.line_edits)
                }
                _ => unified_diff(&key, &live_normalized, &new_text),
            };
            mark("preview", &mut clock);
            let new_tag = match op_name.as_str() {
                "delete" => None,
                _ => Some(new_full[..DISPLAY_TAG_LENGTH].to_string()),
            };

            match &section.file_op {
                Some(FileOp::Rem) => {
                    working.remove(&key);
                    freed_in_patch.insert(key.clone());
                    staged.push(StagedWrite { path: key.clone(), op: StagedOp::Remove, shape, base_tag: Some(live_full.clone()) });
                }
                Some(FileOp::Mv(dest)) => {
                    let dest_key = self.canonical_key(dest);
                    working.remove(&key);
                    freed_in_patch.insert(key.clone());
                    freed_in_patch.remove(&dest_key);
                    working.insert(dest_key.clone(), new_text.clone());
                    if let Some(shape) = shapes.remove(&key) {
                        shapes.insert(dest_key.clone(), shape);
                    }
                    staged.push(StagedWrite {
                        path: key.clone(),
                        op: StagedOp::Move { dest: dest_key, text: new_text.clone() },
                        shape,
                        base_tag: Some(live_full.clone()),
                    });
                }
                None => {
                    working.insert(key.clone(), new_text.clone());
                    staged.push(StagedWrite { path: key.clone(), op: StagedOp::Write { text: new_text.clone() }, shape, base_tag: Some(live_full.clone()) });
                }
            }

            results.push(SectionResult {
                path: key,
                op: op_name,
                dest,
                new_tag,
                previous_tag: live_full,
                first_changed_line: applied.first_changed_line,
                shifts: line_shift_map(&applied.line_edits),
                preview,
                warnings: section_warnings,
                block_resolutions: applied.block_resolutions,
                structural,
                recovered,
            });
        }

        if write {
            for (index, staged_write) in staged.iter().enumerate() {
                if let Err(error) = self.commit_staged(staged_write) {
                    let revert_note = match self.revert_sections(&results[..index]) {
                        Ok(()) if index > 0 => format!("; the {index} already-written section(s) were reverted"),
                        Ok(()) => String::new(),
                        Err(revert_error) => {
                            format!("; REVERT OF EARLIER SECTIONS ALSO FAILED: {revert_error}")
                        }
                    };
                    return Err(format!("{error}{revert_note}"));
                }
            }
        }
        mark("writes", &mut clock);
        if timing {
            eprintln!("timing total: {:.0} us", started.elapsed().as_micros());
        }
        for result in &results {
            report_warnings.extend(result.warnings.iter().cloned());
        }
        report_warnings.dedup();
        Ok(ApplyReport { sections: results, warnings: report_warnings })
    }

    fn current_disk_tag(path: &Path) -> String {
        match fs::read_to_string(path) {
            Ok(raw) => {
                let bom = strip_bom(&raw);
                full_tag(&normalize_to_lf(bom.text))
            }
            Err(_) => full_tag(""),
        }
    }

    fn verify_unchanged(path: &Path, base_tag: Option<&str>) -> Result<(), String> {
        let Some(base) = base_tag else { return Ok(()) };
        if Self::current_disk_tag(path) == base {
            Ok(())
        } else {
            Err(format!(
                "{} changed on disk since it was read; re-read the current version and retry",
                path.display()
            ))
        }
    }

    fn commit_staged(&mut self, staged_write: &StagedWrite) -> Result<(), String> {
        match &staged_write.op {
            StagedOp::Write { text } => {
                ensure_within(&self.canonical_root, &staged_write.path)?;
                let target = self.absolute(&staged_write.path);
                Self::verify_unchanged(&target, staged_write.base_tag.as_deref())?;
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                let (written, recorded) = std::thread::scope(|scope| {
                    let writer = scope.spawn(|| write_shaped(&target, text, &staged_write.shape));
                    let recorded = self.record_applied(&staged_write.path, text);
                    (writer.join().expect("write thread panicked"), recorded)
                });
                written.map_err(|e| format!("cannot write {}: {e}", staged_write.path))?;
                recorded
            }
            StagedOp::Move { dest, text } => {
                ensure_within(&self.canonical_root, &staged_write.path)?;
                ensure_within(&self.canonical_root, dest)?;
                Self::verify_unchanged(&self.absolute(&staged_write.path), staged_write.base_tag.as_deref())?;
                let dest_abs = self.absolute(dest);
                if let Some(parent) = dest_abs.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
                }
                write_shaped(&dest_abs, text, &staged_write.shape)
                    .map_err(|e| format!("cannot write {dest}: {e}"))?;
                if let Err(error) = fs::remove_file(self.absolute(&staged_write.path)) {
                    let _ = fs::remove_file(&dest_abs);
                    return Err(format!("cannot remove {}: {error}", staged_write.path));
                }
                self.record_applied(dest, text)
            }
            StagedOp::Remove => {
                ensure_within(&self.canonical_root, &staged_write.path)?;
                Self::verify_unchanged(&self.absolute(&staged_write.path), staged_write.base_tag.as_deref())?;
                fs::remove_file(self.absolute(&staged_write.path))
                    .map_err(|e| format!("cannot remove {}: {e}", staged_write.path))
            }
        }
    }

    fn record_applied(&self, path: &str, text: &str) -> Result<(), String> {
        let full = self.objects.put(text)?;
        let all: BTreeSet<usize> = (1..=split_lines(text).lines.len()).collect();
        self.objects.record_seen(&full, &all)?;
        self.objects.journal_record(&self.absolute(path).to_string_lossy(), &full)
    }
}

pub fn ensure_within(root: &Path, path: &str) -> Result<(), String> {
    let candidate = Path::new(path);
    let mut depth: i64 = 0;
    for component in candidate.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                return Err(format!("path {path:?} is absolute; use a path relative to the workspace root {}", root.display()));
            }
            std::path::Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!("path {path:?} is outside the workspace root {}", root.display()));
                }
            }
            std::path::Component::CurDir => {}
            std::path::Component::Normal(_) => depth += 1,
        }
    }
    let absolute = root.join(candidate);
    let mut probe = absolute.as_path();
    loop {
        if probe.symlink_metadata().is_ok() {
            let real = std::fs::canonicalize(probe)
                .map_err(|e| format!("cannot resolve {path:?}: {e}"))?;
            if !real.starts_with(root) {
                return Err(format!("path {path:?} escapes the workspace root {} through a symlink", root.display()));
            }
            return Ok(());
        }
        match probe.parent() {
            Some(parent) => probe = parent,
            None => return Ok(()),
        }
    }
}

fn patched_context(text: &str, error_line: usize) -> String {
    let (lines, _) = crate::normalize::borrowed_lines(text);
    if lines.is_empty() {
        return String::new();
    }
    let first = error_line.saturating_sub(3).max(1);
    let last = (error_line + 3).min(lines.len());
    let mut out = String::new();
    for number in first..=last {
        out.push_str(&format!("  {number}:{}\n", lines[number - 1]));
    }
    out
}

fn edited_new_regions(edits: &[crate::apply::LineEdit]) -> Vec<(usize, usize)> {
    let mut delta: i64 = 0;
    let mut regions = Vec::with_capacity(edits.len());
    for edit in edits {
        let new_start = (edit.old_start as i64 + delta).max(1) as usize;
        let new_end = new_start + edit.new_count.saturating_sub(1);
        regions.push((new_start, new_end.max(new_start)));
        delta += edit.new_count as i64 - edit.old_removed as i64;
    }
    regions
}

fn nearest_error_line(key: &str, new_text: &str, edits: &[crate::apply::LineEdit], fallback: usize) -> usize {
    let regions = edited_new_regions(edits);
    if regions.is_empty() {
        return fallback;
    }
    let distance = |line: usize| -> usize {
        regions
            .iter()
            .map(|(start, end)| {
                if (*start..=*end).contains(&line) {
                    0
                } else {
                    line.abs_diff(*start).min(line.abs_diff(*end))
                }
            })
            .min()
            .unwrap_or(usize::MAX)
    };
    crate::blocks::all_parse_errors(key, new_text)
        .iter()
        .map(|(line, _)| *line)
        .min_by_key(|line| distance(*line))
        .unwrap_or(fallback)
}

fn delimiter_deltas(lines: &[&str]) -> [i64; 3] {
    let mut deltas = [0i64; 3];
    for line in lines {
        for byte in line.bytes() {
            match byte {
                b'{' => deltas[0] += 1,
                b'}' => deltas[0] -= 1,
                b'(' => deltas[1] += 1,
                b')' => deltas[1] -= 1,
                b'[' => deltas[2] += 1,
                b']' => deltas[2] -= 1,
                _ => {}
            }
        }
    }
    deltas
}

fn delimiter_imbalances(old_text: &str, new_text: &str, edits: &[crate::apply::LineEdit]) -> String {
    let (old_lines, _) = crate::normalize::borrowed_lines(old_text);
    let (new_lines, _) = crate::normalize::borrowed_lines(new_text);
    let regions = edited_new_regions(edits);
    let mut out = String::new();
    for (edit, (new_start, _)) in edits.iter().zip(regions) {
        let removed_start = edit.old_start.saturating_sub(1);
        let removed = old_lines.get(removed_start..removed_start + edit.old_removed).unwrap_or(&[]);
        let added_start = new_start.saturating_sub(1);
        let added = if edit.new_count == 0 {
            &[][..]
        } else {
            new_lines.get(added_start..added_start + edit.new_count).unwrap_or(&[])
        };
        let removed_deltas = delimiter_deltas(removed);
        let added_deltas = delimiter_deltas(added);
        for (slot, open, close) in [(0usize, '{', '}'), (1, '(', ')'), (2, '[', ']')] {
            let diff = added_deltas[slot] - removed_deltas[slot];
            if diff != 0 {
                out.push_str(&format!(
                    "note: the hunk at old line {} shifts `{open}{close}` balance by {diff:+} versus the lines it replaces\n",
                    edit.old_start
                ));
            }
        }
    }
    out
}

fn line_shift_map(edits: &[crate::apply::LineEdit]) -> Vec<(usize, i64)> {
    let mut cumulative: i64 = 0;
    let mut out: Vec<(usize, i64)> = Vec::new();
    for edit in edits {
        let delta = edit.new_count as i64 - edit.old_removed as i64;
        if delta == 0 {
            continue;
        }
        cumulative += delta;
        let after_old_line = if edit.old_removed == 0 {
            edit.old_start.saturating_sub(1)
        } else {
            edit.old_start + edit.old_removed - 1
        };
        out.push((after_old_line, cumulative));
    }
    out
}
