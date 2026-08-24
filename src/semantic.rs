use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use model2vec_rs::model::StaticModel;

use crate::graph::extract::SymbolRow;

const MODEL: &str = "minishlab/potion-base-8M";
const SCORE_FLOOR: f32 = 0.20;
const CODE_MAX: f32 = 127.0;
const RESCORE_FLOOR: usize = 128;
const RESCORE_PER_HIT: usize = 32;

pub fn load_model() -> Result<Arc<StaticModel>, String> {
    StaticModel::from_pretrained(MODEL, None, None, None)
        .map(Arc::new)
        .map_err(|e| format!("cannot load semantic model {MODEL}: {e}"))
}

pub fn model_id() -> &'static str {
    MODEL
}

pub trait Embedder: Send + Sync {
    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

pub struct LocalModel(pub Arc<StaticModel>);

impl Embedder for LocalModel {
    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(self.0.encode(texts))
    }
}

pub struct SemanticIndex {
    embedder: Arc<dyn Embedder>,
    ids: Vec<String>,
    names: Vec<String>,
    index_by_id: HashMap<String, usize>,
    common_names: HashSet<String>,
    codes: Vec<i8>,
    bits: Vec<u64>,
    dim: usize,
    words: usize,
    scale: f32,
}

fn symbol_text(symbol: &SymbolRow) -> String {
    format!("{} {} {}", symbol.kind, symbol.qualified.replace(['_', ':', '.'], " "), symbol.file)
}

fn normalize(vector: &mut [f32]) {
    let norm: f32 = vector.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        let inverse = 1.0 / norm;
        for value in vector {
            *value *= inverse;
        }
    }
}

fn quantize_into(vector: &[f32], scale: f32, out: &mut Vec<i8>) {
    out.extend(vector.iter().map(|value| (value * scale).round().clamp(-CODE_MAX, CODE_MAX) as i8));
}

fn pack_bits_into(vector: &[f32], words: usize, out: &mut Vec<u64>) {
    let start = out.len();
    out.resize(start + words, 0);
    for (index, value) in vector.iter().enumerate() {
        if *value >= 0.0 {
            out[start + index / 64] |= 1 << (index % 64);
        }
    }
}

fn dot_i8(a: &[i8], b: &[i8]) -> i32 {
    a.iter().zip(b).map(|(x, y)| *x as i32 * *y as i32).sum()
}

fn hamming(a: &[u64], b: &[u64]) -> u32 {
    a.iter().zip(b).map(|(x, y)| (x ^ y).count_ones()).sum()
}

impl SemanticIndex {
    pub fn build(embedder: Arc<dyn Embedder>, symbols: &[SymbolRow]) -> Result<SemanticIndex, String> {
        let mut kept: Vec<&SymbolRow> = symbols
            .iter()
            .filter(|s| matches!(s.kind.as_str(), "fn" | "type" | "trait" | "heading"))
            .collect();
        let texts: Vec<String> = kept.iter().map(|s| symbol_text(s)).collect();
        let mut rows = embedder.encode(&texts)?;
        let dim = rows.first().map(|row| row.len()).unwrap_or(0);
        if dim > 0 {
            let mut fkept = Vec::with_capacity(kept.len());
            let mut frows = Vec::with_capacity(rows.len());
            for (symbol, row) in kept.iter().copied().zip(std::mem::take(&mut rows)) {
                if row.len() == dim {
                    fkept.push(symbol);
                    frows.push(row);
                }
            }
            kept = fkept;
            rows = frows;
        }
        let words = dim.div_ceil(64);
        let mut max_abs = 0.0f32;
        for row in &mut rows {
            normalize(row);
            for value in row.iter() {
                max_abs = max_abs.max(value.abs());
            }
        }
        let scale = if max_abs > 0.0 { CODE_MAX / max_abs } else { CODE_MAX };
        let mut codes = Vec::with_capacity(rows.len() * dim);
        let mut bits = Vec::with_capacity(rows.len() * words);
        for row in &rows {
            quantize_into(row, scale, &mut codes);
            pack_bits_into(row, words, &mut bits);
        }
        let ids: Vec<String> = kept.iter().map(|s| s.id.clone()).collect();
        let names: Vec<String> = kept.iter().map(|s| s.name.clone()).collect();
        let index_by_id = ids.iter().enumerate().map(|(index, id)| (id.clone(), index)).collect();
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for name in &names {
            *name_counts.entry(name.as_str()).or_insert(0) += 1;
        }
        let common_cutoff = (names.len() / 40).max(5);
        let common_names: HashSet<String> = name_counts
            .into_iter()
            .filter(|(_, count)| *count >= common_cutoff)
            .map(|(name, _)| name.to_string())
            .collect();
        Ok(SemanticIndex {
            embedder,
            ids,
            names,
            index_by_id,
            common_names,
            codes,
            bits,
            dim,
            words,
            scale,
        })
    }

    pub fn is_common_name(&self, id: &str) -> bool {
        self.index_by_id
            .get(id)
            .map(|&index| self.common_names.contains(&self.names[index]))
            .unwrap_or(false)
    }

    pub fn relatedness(&self, a: &str, b: &str) -> Option<f32> {
        if self.dim == 0 {
            return None;
        }
        let index_a = *self.index_by_id.get(a)?;
        let index_b = *self.index_by_id.get(b)?;
        let dot = dot_i8(self.code_row(index_a), self.code_row(index_b));
        Some(dot as f32 / (self.scale * self.scale))
    }

    pub fn neighbors(&self, id: &str, top_k: usize) -> Vec<(String, f32)> {
        if self.dim == 0 {
            return Vec::new();
        }
        let Some(&origin) = self.index_by_id.get(id) else { return Vec::new() };
        let dequant = 1.0 / (self.scale * self.scale);
        let query_code = self.code_row(origin);
        let mut scored: Vec<(usize, i32)> = self
            .codes
            .chunks_exact(self.dim)
            .enumerate()
            .filter(|(index, _)| *index != origin)
            .map(|(index, code)| (index, dot_i8(query_code, code)))
            .collect();
        if top_k >= 1 && scored.len() > top_k {
            scored.select_nth_unstable_by(top_k - 1, |a, b| b.1.cmp(&a.1));
            scored.truncate(top_k);
        }
        scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        scored.into_iter().map(|(index, score)| (self.ids[index].clone(), score as f32 * dequant)).collect()
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    fn code_row(&self, index: usize) -> &[i8] {
        &self.codes[index * self.dim..(index + 1) * self.dim]
    }

    fn shortlist(&self, query_bits: &[u64], pool: usize) -> Vec<usize> {
        let count = self.ids.len();
        let mut ranked: Vec<(usize, u32)> = (0..count)
            .map(|index| (index, hamming(query_bits, &self.bits[index * self.words..(index + 1) * self.words])))
            .collect();
        ranked.select_nth_unstable_by(pool - 1, |a, b| a.1.cmp(&b.1));
        ranked.truncate(pool);
        ranked.into_iter().map(|(index, _)| index).collect()
    }

    pub fn query(&self, text: &str, top_k: usize) -> Vec<(String, f32)> {
        if self.dim == 0 {
            return Vec::new();
        }
        let Ok(query_embeddings) = self.embedder.encode(&[text.to_string()]) else { return Vec::new() };
        let Some(mut query) = query_embeddings.into_iter().next() else { return Vec::new() };
        normalize(&mut query);
        let mut query_code = Vec::with_capacity(self.dim);
        quantize_into(&query, self.scale, &mut query_code);
        let floor_code = (SCORE_FLOOR * self.scale * self.scale) as i32;
        let dequant = 1.0 / (self.scale * self.scale);

        let count = self.ids.len();
        let pool = (top_k * RESCORE_PER_HIT).max(RESCORE_FLOOR);
        let mut scored: Vec<(usize, i32)> = if pool < count {
            let mut query_bits = Vec::with_capacity(self.words);
            pack_bits_into(&query, self.words, &mut query_bits);
            self.shortlist(&query_bits, pool)
                .into_iter()
                .map(|index| (index, dot_i8(&query_code, self.code_row(index))))
                .filter(|(_, score)| *score >= floor_code)
                .collect()
        } else {
            self.codes
                .chunks_exact(self.dim)
                .enumerate()
                .map(|(index, code)| (index, dot_i8(&query_code, code)))
                .filter(|(_, score)| *score >= floor_code)
                .collect()
        };

        if top_k >= 1 && scored.len() > top_k {
            scored.select_nth_unstable_by(top_k - 1, |a, b| b.1.cmp(&a.1));
            scored.truncate(top_k);
        }
        scored.sort_by_key(|entry| std::cmp::Reverse(entry.1));
        scored
            .into_iter()
            .map(|(index, score)| (self.ids[index].clone(), score as f32 * dequant))
            .collect()
    }
}
