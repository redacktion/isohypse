use tree_sitter::{Language, Node, Parser};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct BlockSpan {
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    Cpp,
    Python,
    TypeScript,
    Tsx,
    JavaScript,
    Markdown,
}

pub fn lang_for_path(path: &str) -> Option<Lang> {
    let name = path.rsplit('/').next().unwrap_or(path);
    let ext = name.rsplit('.').next().filter(|e| *e != name)?;
    match ext {
        "rs" => Some(Lang::Rust),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "metal" => Some(Lang::Cpp),
        "py" | "pyi" => Some(Lang::Python),
        "ts" | "mts" | "cts" => Some(Lang::TypeScript),
        "tsx" => Some(Lang::Tsx),
        "js" | "jsx" | "mjs" | "cjs" => Some(Lang::JavaScript),
        "md" | "markdown" => Some(Lang::Markdown),
        _ => None,
    }
}

pub fn lang_for(path: &str, text: &str) -> Option<Lang> {
    lang_for_path(path).or_else(|| lang_for_content(text))
}

fn lang_for_content(text: &str) -> Option<Lang> {
    let first_line = text.lines().next().unwrap_or("");
    if first_line.starts_with("#!") {
        if first_line.contains("python") {
            return Some(Lang::Python);
        }
        return None;
    }
    let head = &text[..text.len().min(4096)];
    let score = |needles: &[&str]| needles.iter().filter(|n| head.contains(**n)).count();
    let rust = score(&["fn ", "let ", "impl ", "pub ", "::", "-> "]);
    let python = score(&["def ", "import ", "self", "elif ", "None"]);
    let cpp = score(&["#include", "void ", "template<", "std::", "uint"]);
    let best = rust.max(python).max(cpp);
    if best < 3 {
        return None;
    }
    if best == rust {
        Some(Lang::Rust)
    } else if best == python {
        Some(Lang::Python)
    } else {
        Some(Lang::Cpp)
    }
}

pub fn ts_language(lang: Lang) -> Option<Language> {
    match lang {
        Lang::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        Lang::Cpp => Some(tree_sitter_cpp::LANGUAGE.into()),
        Lang::Python => Some(tree_sitter_python::LANGUAGE.into()),
        Lang::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        Lang::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        Lang::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        Lang::Markdown => None,
    }
}

pub fn resolve_block(path: &str, text: &str, line: usize) -> Option<BlockSpan> {
    if line < 1 {
        return None;
    }
    let lang = lang_for(path, text)?;
    if lang == Lang::Markdown {
        return resolve_markdown_section(text, line);
    }
    let language = ts_language(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    let tree = parser.parse(text, None)?;
    let root = tree.root_node();
    let target_row = line - 1;
    let mut best: Option<Node> = None;
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.start_position().row > target_row {
            continue;
        }
        if node.end_position().row >= target_row {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        if node.is_named() && node.start_position().row == target_row && node.id() != root.id() {
            let wider = match best {
                Some(current) => node.end_position().row > current.end_position().row,
                None => true,
            };
            if wider {
                best = Some(node);
            }
        }
    }
    let node = best?;
    Some(BlockSpan { start: node.start_position().row + 1, end: node.end_position().row + 1 })
}

pub fn parse_tree(path: &str, text: &str) -> Option<tree_sitter::Tree> {
    let language = ts_language(lang_for(path, text)?)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(text, None)
}

pub fn first_parse_error(path: &str, text: &str) -> Option<usize> {
    first_parse_error_in(&parse_tree(path, text)?)
}

pub fn reparse_with(path: &str, text: &str, edited: &tree_sitter::Tree) -> Option<tree_sitter::Tree> {
    let language = ts_language(lang_for(path, text)?)?;
    let mut parser = Parser::new();
    parser.set_language(&language).ok()?;
    parser.parse(text, Some(edited))
}

pub fn first_parse_error_in(tree: &tree_sitter::Tree) -> Option<usize> {
    let root = tree.root_node();
    if !root.has_error() {
        return None;
    }
    let mut stack = vec![root];
    let mut first: Option<usize> = None;
    while let Some(node) = stack.pop() {
        if !node.has_error() {
            continue;
        }
        if node.is_error() || node.is_missing() {
            let line = node.start_position().row + 1;
            first = Some(first.map_or(line, |current| current.min(line)));
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    first.or(Some(root.start_position().row + 1))
}

pub fn all_parse_errors(path: &str, text: &str) -> Vec<(usize, String)> {
    let Some(tree) = parse_tree(path, text) else {
        return Vec::new();
    };
    let root = tree.root_node();
    if !root.has_error() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if !node.has_error() && !node.is_error() && !node.is_missing() {
            continue;
        }
        if node.is_error() || node.is_missing() {
            let kind = if node.is_missing() {
                format!("missing {}", node.kind())
            } else {
                "syntax error".to_string()
            };
            out.push((node.start_position().row + 1, kind));
            continue;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    out.sort_by_key(|(line, _)| *line);
    out.dedup();
    out
}

#[derive(Clone, Debug)]
pub struct Elision {
    pub first: usize,
    pub last: usize,
    pub label: String,
}

const MIN_ELIDED_SPAN: usize = 7;

pub fn outline_elisions(path: &str, text: &str) -> Vec<Elision> {
    let Some(lang) = lang_for(path, text) else { return Vec::new() };
    let Some(language) = ts_language(lang) else { return Vec::new() };
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return Vec::new();
    }
    let Some(tree) = parser.parse(text, None) else { return Vec::new() };
    let mut out = Vec::new();
    collect_elisions(tree.root_node(), text, &mut out);
    out.sort_by_key(|e| e.first);
    out
}

fn collect_elisions(node: Node<'_>, text: &str, out: &mut Vec<Elision>) {
    let is_function = matches!(node.kind(), "function_item" | "function_definition");
    if is_function {
        let start_line = node.start_position().row + 1;
        let end_line = node.end_position().row + 1;
        if end_line - start_line + 1 >= MIN_ELIDED_SPAN {
            let label = node
                .child_by_field_name("name")
                .or_else(|| node.child_by_field_name("declarator"))
                .map(|n| text[n.byte_range()].to_string())
                .unwrap_or_else(|| "fn".to_string());
            out.push(Elision { first: start_line + 1, last: end_line - 1, label });
            return;
        }
    }
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    for child in children {
        collect_elisions(child, text, out);
    }
}

fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.starts_with(' ') || rest.is_empty() {
        Some(hashes)
    } else {
        None
    }
}

fn resolve_markdown_section(text: &str, line: usize) -> Option<BlockSpan> {
    let lines: Vec<&str> = text.split('\n').collect();
    if line > lines.len() {
        return None;
    }
    let level = heading_level(lines[line - 1])?;
    let mut end = lines.len();
    for (index, candidate) in lines.iter().enumerate().skip(line) {
        if let Some(next_level) = heading_level(candidate) {
            if next_level <= level {
                end = index;
                break;
            }
        }
    }
    while end > line && lines[end - 1].trim().is_empty() {
        end -= 1;
    }
    Some(BlockSpan { start: line, end })
}
