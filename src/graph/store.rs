use std::collections::{BTreeMap, HashMap};

use crate::graph::extract::{EdgeRow, SymbolRow};

fn contains_ignore_case(haystack: &str, lowercase_needle: &str) -> bool {
    if lowercase_needle.is_empty() {
        return true;
    }
    let needle = lowercase_needle.as_bytes();
    let bytes = haystack.as_bytes();
    if bytes.len() < needle.len() {
        return false;
    }
    bytes
        .windows(needle.len())
        .any(|window| window.iter().zip(needle).all(|(b, n)| b.to_ascii_lowercase() == *n))
}

#[derive(Default)]
pub struct GraphStore {
    symbols_by_file: BTreeMap<String, Vec<SymbolRow>>,
    edges_by_file: BTreeMap<String, Vec<EdgeRow>>,
    file_hashes: BTreeMap<String, String>,
    by_id: HashMap<String, SymbolRow>,
    by_name: BTreeMap<String, Vec<String>>,
    edges_into: HashMap<String, Vec<EdgeRow>>,
}

impl GraphStore {
    pub fn new() -> GraphStore {
        GraphStore::default()
    }

    pub fn file_hash(&self, path: &str) -> Option<String> {
        self.file_hashes.get(path).cloned()
    }

    pub fn replace_file(&mut self, path: &str, content_hash: &str, symbols: &[SymbolRow], edges: &[EdgeRow]) {
        self.drop_file_rows(path);
        self.file_hashes.insert(path.to_string(), content_hash.to_string());
        self.symbols_by_file.insert(path.to_string(), symbols.to_vec());
        self.edges_by_file.insert(path.to_string(), edges.to_vec());
        for symbol in symbols {
            self.by_id.insert(symbol.id.clone(), symbol.clone());
            self.by_name.entry(symbol.name.clone()).or_default().push(symbol.id.clone());
        }
        for edge in edges {
            self.edges_into.entry(edge.dst.clone()).or_default().push(edge.clone());
        }
    }

    pub fn remove_file(&mut self, path: &str) {
        self.drop_file_rows(path);
        self.file_hashes.remove(path);
        self.symbols_by_file.remove(path);
        self.edges_by_file.remove(path);
    }

    fn drop_file_rows(&mut self, path: &str) {
        if let Some(old_symbols) = self.symbols_by_file.get(path) {
            for symbol in old_symbols {
                self.by_id.remove(&symbol.id);
                if let Some(ids) = self.by_name.get_mut(&symbol.name) {
                    ids.retain(|id| id != &symbol.id);
                    if ids.is_empty() {
                        self.by_name.remove(&symbol.name);
                    }
                }
            }
        }
        if let Some(old_edges) = self.edges_by_file.get(path) {
            for edge in old_edges {
                if let Some(into) = self.edges_into.get_mut(&edge.dst) {
                    into.retain(|e| e != edge);
                    if into.is_empty() {
                        self.edges_into.remove(&edge.dst);
                    }
                }
            }
        }
    }

    pub fn symbols_matching(&self, term: &str, limit: usize) -> Vec<SymbolRow> {
        let mut out: Vec<SymbolRow> = Vec::new();
        if let Some(ids) = self.by_name.get(term) {
            let mut exact: Vec<&SymbolRow> = ids.iter().filter_map(|id| self.by_id.get(id)).collect();
            exact.sort_by(|a, b| (&a.file, a.start_line).cmp(&(&b.file, b.start_line)));
            out.extend(exact.into_iter().take(limit).cloned());
        }
        if out.len() < limit {
            let needle = term.to_ascii_lowercase();
            let mut fuzzy: Vec<&SymbolRow> = self
                .symbols_by_file
                .values()
                .flatten()
                .filter(|s| {
                    s.name != term
                        && (contains_ignore_case(&s.name, &needle)
                            || contains_ignore_case(&s.qualified, &needle))
                })
                .collect();
            fuzzy.sort_by(|a, b| {
                (a.name.len(), &a.file, a.start_line).cmp(&(b.name.len(), &b.file, b.start_line))
            });
            out.extend(fuzzy.into_iter().take(limit - out.len()).cloned());
        }
        out
    }

    pub fn symbol_by_id(&self, id: &str) -> Option<SymbolRow> {
        self.by_id.get(id).cloned()
    }

    pub fn enclosing_symbol(&self, file: &str, line: usize) -> Option<SymbolRow> {
        let symbols = self.symbols_by_file.get(file)?;
        let strict = symbols
            .iter()
            .filter(|s| s.kind == "fn" && s.start_line <= line && line <= s.end_line)
            .min_by_key(|s| s.end_line - s.start_line);
        if let Some(symbol) = strict {
            return Some(symbol.clone());
        }
        symbols
            .iter()
            .filter(|s| s.kind == "fn" && s.start_line <= line)
            .max_by_key(|s| s.start_line)
            .cloned()
    }

    pub fn edges_into(&self, dst: &str) -> Vec<EdgeRow> {
        self.edges_into.get(dst).cloned().unwrap_or_default()
    }

    pub fn file_count(&self) -> usize {
        self.file_hashes.len()
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols_by_file.values().map(Vec::len).sum()
    }

    pub fn all_symbols(&self) -> Vec<SymbolRow> {
        self.symbols_by_file.values().flatten().cloned().collect()
    }

    pub fn files(&self) -> impl Iterator<Item = &String> {
        self.file_hashes.keys()
    }

    pub fn all_edges(&self) -> impl Iterator<Item = &EdgeRow> {
        self.edges_by_file.values().flatten()
    }

    pub fn dump(&self) -> String {
        let mut out = String::new();
        let mut symbols: Vec<&SymbolRow> = self.symbols_by_file.values().flatten().collect();
        symbols.sort_by(|a, b| a.id.cmp(&b.id));
        for s in symbols {
            out.push_str(&format!(
                "{}|{}|{}|{}|{}|{}|{}|{}\n",
                s.id, s.name, s.qualified, s.kind, s.file, s.start_line, s.end_line, s.arity
            ));
        }
        let mut edges: Vec<&EdgeRow> = self.edges_by_file.values().flatten().collect();
        edges.sort_by(|a, b| (&a.src, &a.dst, &a.kind, &a.confidence).cmp(&(&b.src, &b.dst, &b.kind, &b.confidence)));
        for e in edges {
            out.push_str(&format!("{}|{}|{}|{}|{}\n", e.src, e.dst, e.kind, e.confidence, e.detail));
        }
        out
    }
}
