use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::graph::extract::{extract, EdgeRow, Extraction, Receiver, SymbolRow};
use crate::graph::store::GraphStore;
use crate::normalize::normalize_to_lf;
use crate::tag::full_tag;

pub const INDEXABLE_EXTENSIONS: &[&str] = &[
    "rs", "c", "h", "cc", "cpp", "cxx", "hpp", "hh", "metal", "py", "ts", "mts", "cts", "tsx", "js",
    "jsx", "mjs", "cjs", "pyi", "md",
    "yaml", "yml", "toml", "json", "txt", "sh", "bash", "sql", "proto", "graphql",
    "css", "scss", "html", "xml", "cfg", "ini", "conf",
];

const DEFAULT_SKIPPED_DIRECTORIES: [&str; 13] = [
    ".git",
    ".isohypse",
    "target",
    "node_modules",
    "site-packages",
    "__pycache__",
    ".venv",
    "venv",
    "dist",
    "build",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
];
const IGNORE_FILES: [&str; 2] = [".isohypseignore", ".gitignore"];
const VIRTUALENV_MARKER: &str = "pyvenv.cfg";
const MAX_INDEXABLE_BYTES_ENV: &str = "ISOHYPSE_MAX_FILE_BYTES";
const DEFAULT_MAX_INDEXABLE_BYTES: u64 = 2 * 1024 * 1024;

const NAME_TIER_CANDIDATE_CAP: usize = 8;

pub struct GraphIndex {
    pub root: PathBuf,
    pub store: GraphStore,
    extractions: BTreeMap<String, (String, Extraction)>,
    /// Per file: content hash and a digest of its resolved edges. The digest is
    /// what makes the unchanged check cheap; storing the edges themselves kept a
    /// second full copy of the edge graph for no benefit.
    written: HashMap<String, (String, u64)>,
    /// Files dropped by the size cap. They are absent from every find and
    /// explore, so the count is kept and reported rather than left implicit.
    pub oversized_files: usize,
}

pub fn is_indexable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| INDEXABLE_EXTENSIONS.contains(&e))
        .unwrap_or(false)
}

// A workspace root is a project directory, never a home directory or the
// filesystem root: indexing either one walks the whole disk.
pub fn is_workspace_root(path: &Path) -> bool {
    let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if canonical.parent().is_none() {
        return false;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let home = home.canonicalize().unwrap_or(home);
        if canonical == home {
            return false;
        }
    }
    true
}

pub(crate) fn load_skipped_directories(root: &Path) -> BTreeSet<String> {
    let mut skipped: BTreeSet<String> =
        DEFAULT_SKIPPED_DIRECTORIES.iter().map(|name| name.to_string()).collect();
    for ignore_file in IGNORE_FILES {
        let Ok(text) = std::fs::read_to_string(root.join(ignore_file)) else { continue };
        for line in text.lines() {
            if let Some(directory) = skippable_directory(line) {
                skipped.insert(directory);
            }
        }
    }
    skipped
}

fn skippable_directory(line: &str) -> Option<String> {
    let entry = line.trim();
    if entry.is_empty() || entry.starts_with('#') || entry.contains(['*', '?', '[', '!']) {
        return None;
    }
    let entry = entry.trim_start_matches("./").trim_end_matches('/');
    if entry.is_empty() || entry.contains('/') {
        return None;
    }
    Some(entry.to_string())
}

pub(crate) fn is_virtualenv(dir: &Path) -> bool {
    dir.join(VIRTUALENV_MARKER).is_file()
}

pub fn max_indexable_bytes() -> u64 {
    std::env::var(MAX_INDEXABLE_BYTES_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_INDEXABLE_BYTES)
}

fn within_size_cap(path: &Path, max_bytes: u64) -> bool {
    std::fs::metadata(path).map(|meta| meta.len() <= max_bytes).unwrap_or(false)
}

fn relative_key_for(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned())
}

fn symlink_escapes_root(root: &Path, child: &Path) -> bool {
    match std::fs::symlink_metadata(child) {
        Ok(meta) if meta.file_type().is_symlink() => match std::fs::canonicalize(child) {
            Ok(real) => {
                let root_canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
                !real.starts_with(&root_canon)
            }
            Err(_) => true,
        },
        _ => false,
    }
}

fn collect_files(
    root: &Path,
    dir: &Path,
    skipped: &BTreeSet<String>,
    max_bytes: u64,
    oversized: &mut usize,
    out: &mut Vec<PathBuf>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        let Some(name) = child.file_name().and_then(|n| n.to_str()) else { continue };
        let is_symlink = std::fs::symlink_metadata(&child).map(|meta| meta.file_type().is_symlink()).unwrap_or(true);
        if is_symlink || symlink_escapes_root(root, &child) {
            continue;
        }
        if child.is_dir() {
            if !skipped.contains(name) && !is_virtualenv(&child) {
                collect_files(root, &child, skipped, max_bytes, oversized, out);
            }
        } else if is_indexable(&child) {
            if within_size_cap(&child, max_bytes) {
                out.push(child);
            } else {
                *oversized += 1;
            }
        }
    }
}

impl GraphIndex {
    pub fn open(root: impl Into<PathBuf>) -> GraphIndex {
        GraphIndex {
            root: root.into(),
            store: GraphStore::new(),
            extractions: BTreeMap::new(),
            written: HashMap::new(),
            oversized_files: 0,
        }
    }

    fn relative_key(&self, path: &Path) -> String {
        relative_key_for(&self.root, path)
    }

    pub fn full_index(&mut self) -> IndexSummary {
        self.full_index_cancelable(None).unwrap_or_else(|_| IndexSummary {
            files: self.extractions.len(),
            extracted: 0,
            skipped: 0,
            oversized: self.oversized_files,
            symbols: self.store.symbol_count(),
        })
    }

    pub fn full_index_cancelable(&mut self, cancelled: Option<&std::sync::atomic::AtomicBool>) -> Result<IndexSummary, String> {
        let mut files = Vec::new();
        let mut oversized = 0usize;
        let skipped = load_skipped_directories(&self.root);
        collect_files(&self.root, &self.root.clone(), &skipped, max_indexable_bytes(), &mut oversized, &mut files);
        self.oversized_files = oversized;
        let root = self.root.clone();
        let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).max(1);
        let cancel = || cancelled.is_some_and(|token| token.load(std::sync::atomic::Ordering::Acquire));

        // Pass 1 — extract a batch, hand its symbols to the store, and drop the
        // extraction. Holding every file's call sites and type references at once
        // was the resident peak: gigabytes on a large workspace, and it is transient
        // data, not retained state.
        let known: BTreeMap<String, String> = self
            .extractions
            .iter()
            .map(|(key, (hash, _))| (key.clone(), hash.clone()))
            .collect();
        let mut changed: Vec<String> = Vec::new();
        let mut unreadable_keys: Vec<String> = Vec::new();
        for batch in files.chunks(workers * 4) {
            if cancel() {
                return Err("reindex cancelled".to_string());
            }
            let results: Vec<(String, Option<(String, Extraction)>, bool)> = std::thread::scope(|scope| {
                let handles: Vec<_> = batch
                    .iter()
                    .map(|file| {
                        let root = &root;
                        let known = &known;
                        scope.spawn(move || {
                            let key = relative_key_for(root, file);
                            let Ok(raw) = std::fs::read_to_string(file) else {
                                return (key, None, true);
                            };
                            let text = normalize_to_lf(&raw);
                            let hash = full_tag(&text);
                            if known.get(&key).map(|current| current == &hash).unwrap_or(false) {
                                return (key, None, false);
                            }
                            let extraction = extract(&key, &text);
                            (key, Some((hash, extraction)), false)
                        })
                    })
                    .collect();
                handles.into_iter().filter_map(|handle| handle.join().ok()).collect()
            });
            for (key, extracted, unreadable) in results {
                if unreadable {
                    unreadable_keys.push(key);
                    continue;
                }
                if let Some((hash, extraction)) = extracted {
                    self.store.replace_file(&key, &hash, &extraction.symbols, &[]);
                    // Keep impls: the trait-implementation table is global, everything
                    // else the extraction carries is per-file and recomputed in pass 2.
                    self.extractions
                        .insert(key.clone(), (hash, Extraction { impls: extraction.impls, ..Extraction::default() }));
                    changed.push(key);
                }
            }
        }

        let skipped_files = unreadable_keys.len();
        for key in unreadable_keys {
            self.extractions.remove(&key);
            self.written.remove(&key);
            self.store.remove_file(&key);
        }
        let discovered: BTreeSet<String> = files.iter().map(|file| relative_key_for(&self.root, file)).collect();
        let stale: Vec<String> = self.extractions.keys().filter(|key| !discovered.contains(*key)).cloned().collect();
        for key in stale {
            self.extractions.remove(&key);
            self.written.remove(&key);
            self.store.remove_file(&key);
        }

        // Pass 2 — the symbol table is now complete, so re-extract each changed file
        // in batches and resolve only that batch's edges against it.
        let extracted = changed.len();
        for batch in changed.chunks(workers * 4) {
            if cancel() {
                return Err("reindex cancelled".to_string());
            }
            let mut targets: BTreeSet<String> = BTreeSet::new();
            for key in batch {
                let Ok(raw) = std::fs::read_to_string(self.root.join(key)) else { continue };
                let text = normalize_to_lf(&raw);
                let extraction = extract(key, &text);
                if let Some(entry) = self.extractions.get_mut(key) {
                    entry.1.calls = extraction.calls;
                    entry.1.type_refs = extraction.type_refs;
                    entry.1.method_edges = extraction.method_edges;
                    entry.1.impls = extraction.impls;
                }
                targets.insert(key.clone());
            }
            self.write_all(&targets);
            self.drop_edge_inputs();
        }

        Ok(IndexSummary { files: self.extractions.len(), extracted, skipped: skipped_files, oversized, symbols: self.store.symbol_count() })
    }

    pub fn update_file(&mut self, path: &Path) {
        let key = self.relative_key(path);
        match std::fs::read_to_string(path) {
            Ok(raw) => {
                let text = normalize_to_lf(&raw);
                let hash = full_tag(&text);
                if self
                    .extractions
                    .get(&key)
                    .map(|(existing, _)| existing == &hash)
                    .unwrap_or(false)
                {
                    return;
                }
                let extraction = extract(&key, &text);
                self.store.replace_file(&key, &hash, &extraction.symbols, &[]);
                self.extractions.insert(key.clone(), (hash, Extraction { symbols: Vec::new(), ..extraction }));
            }
            Err(_) => {
                self.extractions.remove(&key);
                self.written.remove(&key);
                self.store.remove_file(&key);
            }
        }
        let targets: BTreeSet<String> = std::iter::once(key.clone()).collect();
        self.write_all(&targets);
        self.drop_edge_inputs();
    }

    /// Edge inputs — call sites, type references, method edges — are only needed
    /// while resolving that file's edges. Retaining them for every file after the
    /// edges were stored was the largest resident structure in the index.
    fn drop_edge_inputs(&mut self) {
        for (_, (_, extraction)) in self.extractions.iter_mut() {
            extraction.calls = Vec::new();
            extraction.calls.shrink_to_fit();
            extraction.type_refs = Vec::new();
            extraction.type_refs.shrink_to_fit();
            extraction.method_edges = Vec::new();
            extraction.method_edges.shrink_to_fit();
        }
    }

    /// Write resolved edges for the given files only. Other files keep the edges
    /// already in the store, which is what makes a single-file edit cheap.
    fn write_all(&mut self, targets: &BTreeSet<String>) {
        let edges_by_file = self.resolve_edges();
        let empty: Vec<EdgeRow> = Vec::new();
        let entries: Vec<(String, String)> = targets
            .iter()
            .filter_map(|path| self.extractions.get(path).map(|(hash, _)| (path.clone(), hash.clone())))
            .collect();
        for (path, hash) in entries {
            let edges = edges_by_file.get(&path).unwrap_or(&empty);
            let digest = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                edges.len().hash(&mut hasher);
                for edge in edges {
                    edge.src.hash(&mut hasher);
                    edge.dst.hash(&mut hasher);
                    edge.kind.hash(&mut hasher);
                    edge.confidence.hash(&mut hasher);
                    edge.detail.hash(&mut hasher);
                }
                hasher.finish()
            };
            let unchanged = self
                .written
                .get(&path)
                .map(|(written_hash, written_edges)| written_hash == &hash && *written_edges == digest)
                .unwrap_or(false);
            if unchanged {
                continue;
            }
            self.store.set_edges(&path, edges);
            self.written.insert(path, (hash, digest));
        }
    }

    fn resolve_edges<'a>(&'a self) -> HashMap<String, Vec<EdgeRow>> {
        let mut fn_by_name: HashMap<&'a str, Vec<&'a SymbolRow>> = HashMap::new();
        let mut by_qualified: HashMap<&'a str, Vec<&'a SymbolRow>> = HashMap::new();
        let mut fn_by_suffix: HashMap<String, Vec<&'a SymbolRow>> = HashMap::new();
        let mut symbol_file: HashMap<&'a str, &'a str> = HashMap::new();
        let mut trait_symbols: HashMap<&'a str, &'a SymbolRow> = HashMap::new();
        let mut type_symbols: HashMap<&'a str, Vec<&'a SymbolRow>> = HashMap::new();
        let mut visit = |symbol: &'a SymbolRow| {
            symbol_file.insert(&symbol.id, &symbol.file);
            by_qualified.entry(&symbol.qualified).or_default().push(symbol);
            match symbol.kind.as_str() {
                "fn" => {
                    fn_by_name.entry(&symbol.name).or_default().push(symbol);
                    let segments: Vec<&str> = symbol.qualified.rsplit("::").take(2).collect();
                    if segments.len() == 2 {
                        fn_by_suffix
                            .entry(format!("{}::{}", segments[1], segments[0]))
                            .or_default()
                            .push(symbol);
                    }
                }
                "trait" => {
                    trait_symbols.insert(&symbol.name, symbol);
                }
                "type" => type_symbols.entry(&symbol.name).or_default().push(symbol),
                _ => {}
            }
        };
        self.store.for_each_symbol(&mut visit);
        let mut trait_impls: HashMap<&'a str, Vec<&'a str>> = HashMap::new();
        for (_, extraction) in self.extractions.values() {
            for impl_row in &extraction.impls {
                if let Some(trait_name) = &impl_row.trait_name {
                    trait_impls.entry(trait_name).or_default().push(&impl_row.type_name);
                }
            }
        }
        for impls in trait_impls.values_mut() {
            impls.sort();
            impls.dedup();
        }

        let mut edges_by_file: HashMap<String, Vec<EdgeRow>> = HashMap::new();
        let mut push_edge = |src: &str, dst: &str, kind: &str, confidence: &str, detail: &str| {
            let Some(file) = symbol_file.get(src) else { return };
            edges_by_file.entry(file.to_string()).or_default().push(EdgeRow {
                src: src.to_string(),
                dst: dst.to_string(),
                kind: kind.to_string(),
                confidence: confidence.to_string(),
                detail: detail.to_string(),
            });
        };

        for (_, extraction) in self.extractions.values() {
            for (referrer, type_name) in &extraction.type_refs {
                let mut targets: Vec<&&SymbolRow> = Vec::new();
                if let Some(types) = type_symbols.get(type_name.as_str()) {
                    targets.extend(types.iter());
                }
                if let Some(trait_symbol) = trait_symbols.get(type_name.as_str()) {
                    targets.push(trait_symbol);
                }
                for target in targets {
                    push_edge(referrer, &target.id, "reference", "direct", "");
                }
            }
            for (method_id, type_qualified) in &extraction.method_edges {
                if let Some(types) = by_qualified.get(type_qualified.as_str()) {
                    for type_symbol in types {
                        if type_symbol.kind == "type" {
                            push_edge(method_id, &type_symbol.id, "method_of", "direct", "");
                        }
                    }
                }
            }
            for impl_row in &extraction.impls {
                let Some(trait_name) = &impl_row.trait_name else { continue };
                let Some(trait_symbol) = trait_symbols.get(trait_name.as_str()) else { continue };
                if let Some(types) = type_symbols.get(impl_row.type_name.as_str()) {
                    for type_symbol in types {
                        push_edge(&type_symbol.id, &trait_symbol.id, "impl_of", "direct", trait_name);
                    }
                }
            }
            for call in &extraction.calls {
                let mut resolved = false;
                match &call.receiver {
                    Receiver::Free => {
                        if let Some(candidates) = by_qualified.get(call.callee_name.as_str()) {
                            for symbol in candidates.iter().filter(|s| s.kind == "fn") {
                                push_edge(&call.caller, &symbol.id, "call", "direct", "");
                                resolved = true;
                            }
                        }
                    }
                    Receiver::Path(path) => {
                        let suffix = path.rsplit("::").take(2).collect::<Vec<&str>>();
                        let needle = if suffix.len() == 2 {
                            format!("{}::{}", suffix[1], suffix[0])
                        } else {
                            path.clone()
                        };
                        let candidates = by_qualified
                            .get(needle.as_str())
                            .map(|symbols| symbols.as_slice())
                            .or_else(|| fn_by_suffix.get(&needle).map(|symbols| symbols.as_slice()))
                            .unwrap_or(&[]);
                        for symbol in candidates.iter().filter(|s| s.kind == "fn") {
                            push_edge(&call.caller, &symbol.id, "call", "direct", "");
                            resolved = true;
                        }
                    }
                    Receiver::Typed(type_name) => {
                        for separator in ["::", "."] {
                            let needle = format!("{type_name}{separator}{}", call.callee_name);
                            if let Some(symbols) = by_qualified.get(needle.as_str()) {
                                for symbol in symbols.iter().filter(|s| s.kind == "fn") {
                                    push_edge(&call.caller, &symbol.id, "call", "typed", type_name);
                                    resolved = true;
                                }
                            }
                        }
                    }
                    Receiver::DynTrait(trait_name) => {
                        let trait_method = format!("{trait_name}::{}", call.callee_name);
                        if let Some(symbols) = by_qualified.get(trait_method.as_str()) {
                            for symbol in symbols.iter().filter(|s| s.kind == "fn") {
                                push_edge(&call.caller, &symbol.id, "call", "dyn", trait_name);
                                resolved = true;
                            }
                        }
                        if let Some(impl_types) = trait_impls.get(trait_name.as_str()) {
                            for type_name in impl_types {
                                let impl_method = format!("{type_name}::{}", call.callee_name);
                                if let Some(symbols) = by_qualified.get(impl_method.as_str()) {
                                    for symbol in symbols.iter().filter(|s| s.kind == "fn") {
                                        push_edge(&call.caller, &symbol.id, "call", "dyn", trait_name);
                                        resolved = true;
                                    }
                                }
                            }
                        }
                    }
                    Receiver::Unknown => {}
                }
                if !resolved && !call.callee_name.contains("::") {
                    if let Some(candidates) = fn_by_name.get(call.callee_name.as_str()) {
                        let mut sorted: Vec<&&SymbolRow> = candidates
                            .iter()
                            .filter(|s| s.arity < 0 || s.arity == call.arity as i64)
                            .collect();
                        sorted.sort_by(|a, b| (&a.file, a.start_line).cmp(&(&b.file, b.start_line)));
                        for symbol in sorted.into_iter().take(NAME_TIER_CANDIDATE_CAP) {
                            if symbol.id != call.caller {
                                push_edge(&call.caller, &symbol.id, "call", "name", "");
                            }
                        }
                    }
                }
            }
        }

        for edges in edges_by_file.values_mut() {
            edges.sort_by(|a, b| (&a.src, &a.dst, &a.kind, &a.confidence).cmp(&(&b.src, &b.dst, &b.kind, &b.confidence)));
            edges.dedup();
        }
        edges_by_file
    }

    pub fn call_path(&self, from: &str, to: &str) -> Option<Vec<(SymbolRow, String)>> {
        let resolve_endpoints = |name: &str| -> Vec<String> {
            self.store
                .symbols_matching(name, 16)
                .into_iter()
                .filter(|s| s.kind == "fn" && (s.name == name || s.qualified == name))
                .map(|s| s.id)
                .collect()
        };
        let sources = resolve_endpoints(from);
        let targets: BTreeSet<String> = resolve_endpoints(to).into_iter().collect();
        if sources.is_empty() || targets.is_empty() {
            return None;
        }
        let mut adjacency: BTreeMap<&str, Vec<&crate::graph::extract::EdgeRow>> = BTreeMap::new();
        for edge in self.store.all_edges() {
            if edge.kind == "call" {
                adjacency.entry(edge.src.as_str()).or_default().push(edge);
            }
        }
        let mut parents: HashMap<String, (String, String)> = HashMap::new();
        let mut frontier: Vec<String> = sources.clone();
        let mut visited: BTreeSet<String> = sources.iter().cloned().collect();
        let mut reached: Option<String> = sources.iter().find(|s| targets.contains(*s)).cloned();
        while reached.is_none() && !frontier.is_empty() {
            let mut next = Vec::new();
            for current in &frontier {
                for edge in adjacency.get(current.as_str()).into_iter().flatten() {
                    if !visited.insert(edge.dst.clone()) {
                        continue;
                    }
                    parents.insert(edge.dst.clone(), (current.clone(), edge.confidence.clone()));
                    if targets.contains(&edge.dst) {
                        reached = Some(edge.dst.clone());
                        break;
                    }
                    next.push(edge.dst.clone());
                }
                if reached.is_some() {
                    break;
                }
            }
            frontier = next;
        }
        let mut cursor = reached?;
        let mut chain: Vec<(SymbolRow, String)> = vec![(self.store.symbol_by_id(&cursor)?, String::new())];
        while let Some((parent, confidence)) = parents.get(&cursor) {
            chain.push((self.store.symbol_by_id(parent)?, confidence.clone()));
            cursor = parent.clone();
        }
        chain.reverse();
        Some(chain)
    }

    pub fn callers_of(&self, symbol_id: &str) -> CallerReport {
        let mut confirmed = Vec::new();
        let mut dynamic = Vec::new();
        let mut named = Vec::new();
        for edge in self.store.edges_into(symbol_id) {
            if edge.kind != "call" {
                continue;
            }
            let Some(source) = self.store.symbol_by_id(&edge.src) else { continue };
            match edge.confidence.as_str() {
                "direct" | "typed" => confirmed.push(source),
                "dyn" => dynamic.push((source, edge.detail)),
                _ => named.push(source),
            }
        }
        CallerReport { confirmed, dynamic, named }
    }

    pub fn blast_radius(&self, symbol_id: &str, depth: usize) -> BlastRadius {
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut frontier = vec![symbol_id.to_string()];
        let mut confirmed_files: BTreeMap<String, usize> = BTreeMap::new();
        let mut dynamic: Vec<(SymbolRow, String)> = Vec::new();
        for _ in 0..depth {
            let mut next = Vec::new();
            for id in frontier.drain(..) {
                for edge in self.store.edges_into(&id) {
                    if edge.kind != "call" && edge.kind != "method_of" {
                        continue;
                    }
                    match edge.confidence.as_str() {
                        "direct" | "typed" => {
                            if visited.insert(edge.src.clone())
                                && let Some(symbol) = self.store.symbol_by_id(&edge.src)
                            {
                                *confirmed_files.entry(symbol.file.clone()).or_insert(0) += 1;
                                next.push(edge.src);
                            }
                        }
                        "dyn" => {
                            if let Some(symbol) = self.store.symbol_by_id(&edge.src) {
                                dynamic.push((symbol, edge.detail));
                            }
                        }
                        _ => {}
                    }
                }
            }
            frontier = next;
            if frontier.is_empty() {
                break;
            }
        }
        BlastRadius { confirmed_symbols: visited.len(), confirmed_files, dynamic }
    }
}

#[derive(Debug)]
pub struct IndexSummary {
    pub files: usize,
    pub extracted: usize,
    pub skipped: usize,
    /// Indexable files left out because they exceed ISOHYPSE_MAX_FILE_BYTES.
    pub oversized: usize,
    pub symbols: usize,
}

#[derive(Debug)]
pub struct CallerReport {
    pub confirmed: Vec<SymbolRow>,
    pub dynamic: Vec<(SymbolRow, String)>,
    pub named: Vec<SymbolRow>,
}

#[derive(Debug)]
pub struct BlastRadius {
    pub confirmed_symbols: usize,
    pub confirmed_files: BTreeMap<String, usize>,
    pub dynamic: Vec<(SymbolRow, String)>,
}
