use std::collections::HashMap;
use xxhash_rust::xxh3::Xxh3Builder;

pub type FnIndex = HashMap<String, u64, Xxh3Builder>;

use tree_sitter::{Node, Parser};
use xxhash_rust::xxh3::xxh3_64;

use crate::blocks::{lang_for, ts_language, Lang};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolRow {
    pub id: String,
    pub name: String,
    pub qualified: String,
    pub kind: String,
    pub file: String,
    pub start_line: usize,
    pub end_line: usize,
    pub arity: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeRow {
    pub src: String,
    pub dst: String,
    pub kind: String,
    pub confidence: String,
    pub detail: String,
}

#[derive(Clone, Debug)]
pub enum Receiver {
    Free,
    Path(String),
    Typed(String),
    DynTrait(String),
    Unknown,
}

#[derive(Clone, Debug)]
pub struct CallSite {
    pub caller: String,
    pub callee_name: String,
    pub arity: usize,
    pub receiver: Receiver,
}

#[derive(Clone, Debug)]
pub struct ImplRow {
    pub type_name: String,
    pub trait_name: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Extraction {
    pub symbols: Vec<SymbolRow>,
    pub calls: Vec<CallSite>,
    pub impls: Vec<ImplRow>,
    pub method_edges: Vec<(String, String)>,
    pub type_refs: Vec<(String, String)>,
}

pub fn fn_signatures(path: &str, text: &str, tree: &tree_sitter::Tree) -> FnIndex {
    let Some(lang) = lang_for(path, text) else { return FnIndex::with_hasher(Xxh3Builder::new()) };
    if lang == Lang::Markdown {
        return FnIndex::with_hasher(Xxh3Builder::new());
    }
    let separator = if dot_separated(lang) { "." } else { "::" };
    let mut out = FnIndex::with_hasher(Xxh3Builder::new());
    let mut containers: Vec<String> = Vec::new();
    walk_signatures(tree.root_node(), text, separator, &mut containers, &mut out);
    out
}

fn walk_signatures(node: Node<'_>, text: &str, separator: &str, containers: &mut Vec<String>, out: &mut FnIndex) {
    match node.kind() {
        "function_item" | "function_signature_item" | "function_definition" | "function_declaration"
        | "generator_function_declaration" | "method_definition" => {
            let name = node
                .child_by_field_name("name")
                .map(|n| node_text(n, text).to_string())
                .or_else(|| declarator_identifier(node, text));
            if let Some(name) = name {
                let qualified = if containers.is_empty() {
                    name
                } else {
                    let mut qualified = containers.join(separator);
                    qualified.push_str(separator);
                    qualified.push_str(&name);
                    qualified
                };
                out.insert(qualified, xxh3_64(node_text(node, text).as_bytes()));
            }
            return;
        }
        "impl_item" => {
            if let Some(type_node) = node.child_by_field_name("type") {
                if let Some(body) = node.child_by_field_name("body") {
                    containers.push(strip_generics(node_text(type_node, text)));
                    let mut cursor = body.walk();
                    for child in body.children(&mut cursor) {
                        walk_signatures(child, text, separator, containers, out);
                    }
                    containers.pop();
                }
            }
            return;
        }
        "trait_item" | "mod_item" | "class_definition" | "class_specifier" | "struct_specifier"
        | "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
            if let Some(name) = node.child_by_field_name("name").map(|n| node_text(n, text).to_string()) {
                containers.push(name);
                let mut cursor = node.walk();
                for child in node.children(&mut cursor) {
                    walk_signatures(child, text, separator, containers, out);
                }
                containers.pop();
                return;
            }
        }
        _ => {}
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_signatures(child, text, separator, containers, out);
    }
}


const FUNCTION_KINDS: [&str; 6] = [
    "function_item",
    "function_signature_item",
    "function_definition",
    "function_declaration",
    "generator_function_declaration",
    "method_definition",
];
const CONTAINER_KINDS: [&str; 8] = [
    "trait_item",
    "mod_item",
    "class_definition",
    "class_specifier",
    "struct_specifier",
    "class_declaration",
    "abstract_class_declaration",
    "interface_declaration",
];

pub fn single_function_delta(
    edited_old: &tree_sitter::Tree,
    new_tree: &tree_sitter::Tree,
    path: &str,
    new_text: &str,
    old_index: &FnIndex,
) -> Option<(FnIndex, Vec<String>)> {
    let lang = lang_for(path, new_text)?;
    if lang == Lang::Markdown {
        return None;
    }
    let separator = if dot_separated(lang) { "." } else { "::" };
    let ranges: Vec<tree_sitter::Range> = edited_old.changed_ranges(new_tree).collect();
    if ranges.is_empty() {
        return Some((old_index.clone(), Vec::new()));
    }
    let mut function_node: Option<Node<'_>> = None;
    for range in &ranges {
        let node = new_tree
            .root_node()
            .descendant_for_byte_range(range.start_byte, range.end_byte.min(new_text.len().saturating_sub(1).max(range.start_byte)))?;
        let mut cursor = Some(node);
        let mut enclosing: Option<Node<'_>> = None;
        while let Some(current) = cursor {
            if FUNCTION_KINDS.contains(&current.kind()) {
                enclosing = Some(current);
            }
            cursor = current.parent();
        }
        let enclosing = enclosing?;
        match function_node {
            Some(existing) if existing.id() == enclosing.id() => {}
            Some(_) => return None,
            None => function_node = Some(enclosing),
        }
    }
    let function_node = function_node?;
    let name = function_node
        .child_by_field_name("name")
        .map(|n| node_text(n, new_text).to_string())
        .or_else(|| declarator_identifier(function_node, new_text))?;
    let mut containers: Vec<String> = Vec::new();
    let mut ancestor = function_node.parent();
    while let Some(current) = ancestor {
        if current.kind() == "impl_item" {
            containers.push(strip_generics(node_text(current.child_by_field_name("type")?, new_text)));
        } else if CONTAINER_KINDS.contains(&current.kind()) {
            containers.push(node_text(current.child_by_field_name("name")?, new_text).to_string());
        } else if FUNCTION_KINDS.contains(&current.kind()) {
            return None;
        }
        ancestor = current.parent();
    }
    containers.reverse();
    let qualified = if containers.is_empty() {
        name
    } else {
        let mut qualified = containers.join(separator);
        qualified.push_str(separator);
        qualified.push_str(&name);
        qualified
    };
    let old_hash = *old_index.get(&qualified)?;
    let new_hash = xxh3_64(node_text(function_node, new_text).as_bytes());
    let mut updated = old_index.clone();
    updated.insert(qualified.clone(), new_hash);
    let structural = if old_hash == new_hash {
        Vec::new()
    } else {
        vec![format!("fn modified: {qualified}")]
    };
    Some((updated, structural))
}

fn declarator_identifier(node: Node<'_>, text: &str) -> Option<String> {
    let mut declarator = node.child_by_field_name("declarator")?;
    loop {
        match declarator.kind() {
            "function_declarator" | "pointer_declarator" | "reference_declarator" => {
                declarator = declarator.child_by_field_name("declarator").or_else(|| {
                    let mut cursor = declarator.walk();
                    let named: Vec<Node<'_>> = declarator.children(&mut cursor).filter(|c| c.is_named()).collect();
                    named.into_iter().next()
                })?;
            }
            _ => return Some(node_text(declarator, text).to_string()),
        }
    }
}

pub fn structural_diff_indexed(old_index: &FnIndex, new_index: &FnIndex) -> Vec<String> {
    let mut out = Vec::new();
    let mut added: Vec<&String> = new_index.keys().filter(|k| !old_index.contains_key(*k)).collect();
    let mut removed: Vec<&String> = old_index.keys().filter(|k| !new_index.contains_key(*k)).collect();
    let mut modified: Vec<&String> = new_index
        .iter()
        .filter(|(k, hash)| old_index.get(*k).map(|old| old != *hash).unwrap_or(false))
        .map(|(k, _)| k)
        .collect();
    added.sort();
    removed.sort();
    modified.sort();
    if !added.is_empty() {
        out.push(format!("fn added: {}", added.iter().map(|q| q.as_str()).collect::<Vec<&str>>().join(", ")));
    }
    if !removed.is_empty() {
        out.push(format!("fn removed: {}", removed.iter().map(|q| q.as_str()).collect::<Vec<&str>>().join(", ")));
    }
    if !modified.is_empty() {
        out.push(format!("fn modified: {}", modified.iter().map(|q| q.as_str()).collect::<Vec<&str>>().join(", ")));
    }
    out
}

pub fn symbol_id(file: &str, qualified: &str, kind: &str) -> String {
    format!("{:016x}", xxh3_64(format!("{file}|{qualified}|{kind}").as_bytes()))
}

fn node_text<'a>(node: Node<'_>, source: &'a str) -> &'a str {
    &source[node.byte_range()]
}

fn strip_generics(type_text: &str) -> String {
    let base = type_text.split('<').next().unwrap_or(type_text);
    base.trim().trim_start_matches("r#").to_string()
}

fn strip_reference_wrappers(type_text: &str) -> &str {
    let mut text = type_text.trim();
    loop {
        let next = text
            .strip_prefix('&')
            .map(str::trim_start)
            .map(|t| t.strip_prefix("mut ").unwrap_or(t))
            .unwrap_or(text);
        if next == text {
            return text;
        }
        text = next.trim();
    }
}

pub fn dyn_trait_of(type_text: &str) -> Option<String> {
    let inner = strip_reference_wrappers(type_text);
    let inner = ["Box<", "Arc<", "Rc<"]
        .iter()
        .find_map(|wrapper| inner.strip_prefix(wrapper).and_then(|rest| rest.strip_suffix('>')))
        .unwrap_or(inner);
    let inner = strip_reference_wrappers(inner);
    let trait_part = inner.strip_prefix("dyn ")?;
    Some(strip_generics(trait_part.split('+').next().unwrap_or(trait_part)))
}

pub fn extract(path: &str, text: &str) -> Extraction {
    match lang_for(path, text) {
        Some(Lang::Markdown) => extract_markdown(path, text),
        Some(lang) => {
            let Some(language) = ts_language(lang) else { return Extraction::default() };
            let mut parser = Parser::new();
            if parser.set_language(&language).is_err() {
                return Extraction::default();
            }
            let Some(tree) = parser.parse(text, None) else { return Extraction::default() };
            let mut state = ExtractState {
                path,
                text,
                lang,
                extraction: Extraction::default(),
                file_symbol: file_symbol(path, text),
            };
            state.extraction.symbols.push(state.file_symbol.clone());
            walk(&mut state, tree.root_node(), &Scope::default());
            state.extraction
        }
        None => {
            let mut extraction = Extraction::default();
            extraction.symbols.push(file_symbol(path, text));
            extraction
        }
    }
}

fn file_symbol(path: &str, text: &str) -> SymbolRow {
    let lines = text.split('\n').count();
    SymbolRow {
        id: symbol_id(path, path, "file"),
        name: path.rsplit('/').next().unwrap_or(path).to_string(),
        qualified: path.to_string(),
        kind: "file".to_string(),
        file: path.to_string(),
        start_line: 1,
        end_line: lines.max(1),
        arity: -1,
    }
}

fn extract_markdown(path: &str, text: &str) -> Extraction {
    let mut extraction = Extraction::default();
    extraction.symbols.push(file_symbol(path, text));
    let lines: Vec<&str> = text.split('\n').collect();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        let hashes = trimmed.bytes().take_while(|b| *b == b'#').count();
        if hashes == 0 || hashes > 6 || !trimmed[hashes..].starts_with(' ') {
            continue;
        }
        let title = trimmed[hashes..].trim().to_string();
        let qualified = format!("{path}#{title}");
        extraction.symbols.push(SymbolRow {
            id: symbol_id(path, &qualified, "heading"),
            name: title,
            qualified,
            kind: "heading".to_string(),
            file: path.to_string(),
            start_line: index + 1,
            end_line: index + 1,
            arity: -1,
        });
    }
    extraction
}

#[derive(Clone, Default)]
struct Scope {
    container: Vec<String>,
    impl_type: Option<String>,
    caller: Option<String>,
    locals: HashMap<String, String>,
    dyn_bounds: HashMap<String, String>,
}

struct ExtractState<'a> {
    path: &'a str,
    text: &'a str,
    lang: Lang,
    extraction: Extraction,
    file_symbol: SymbolRow,
}

impl ExtractState<'_> {
    fn push_symbol(&mut self, name: &str, qualified: &str, kind: &str, node: Node<'_>) -> String {
        let id = symbol_id(self.path, qualified, kind);
        let arity = if kind == "fn" { fn_arity(node) } else { -1 };
        self.extraction.symbols.push(SymbolRow {
            id: id.clone(),
            name: name.to_string(),
            qualified: qualified.to_string(),
            kind: kind.to_string(),
            file: self.path.to_string(),
            start_line: node.start_position().row + 1,
            end_line: node.end_position().row + 1,
            arity,
        });
        id
    }

    fn caller_or_file(&self, scope: &Scope) -> String {
        scope.caller.clone().unwrap_or_else(|| self.file_symbol.id.clone())
    }
}

fn fn_arity(node: Node<'_>) -> i64 {
    let Some(params) = node.child_by_field_name("parameters") else { return -1 };
    let mut cursor = params.walk();
    params.children(&mut cursor).filter(|c| c.is_named()).count() as i64
}

fn qualified_name(scope: &Scope, name: &str, separator: &str) -> String {
    if scope.container.is_empty() {
        name.to_string()
    } else {
        format!("{}{}{}", scope.container.join(separator), separator, name)
    }
}

fn dot_separated(lang: Lang) -> bool {
    matches!(lang, Lang::Python | Lang::TypeScript | Lang::Tsx | Lang::JavaScript)
}

fn walk(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    match state.lang {
        Lang::Rust => walk_rust(state, node, scope),
        Lang::Cpp => walk_cpp(state, node, scope),
        Lang::Python => walk_python(state, node, scope),
        Lang::TypeScript | Lang::Tsx | Lang::JavaScript => walk_typescript(state, node, scope),
        Lang::Markdown => {}
    }
}

fn walk_children(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    let mut cursor = node.walk();
    let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
    for child in children {
        walk(state, child, scope);
    }
}

fn child_name<'a>(node: Node<'_>, field: &str, source: &'a str) -> Option<&'a str> {
    node.child_by_field_name(field).map(|n| node_text(n, source))
}

fn rust_collect_params(state: &ExtractState<'_>, node: Node<'_>, scope: &mut Scope) {
    let Some(params) = node.child_by_field_name("parameters") else { return };
    let mut cursor = params.walk();
    for param in params.children(&mut cursor) {
        if param.kind() != "parameter" {
            continue;
        }
        let Some(pattern) = param.child_by_field_name("pattern") else { continue };
        let Some(type_node) = param.child_by_field_name("type") else { continue };
        if pattern.kind() != "identifier" {
            continue;
        }
        let name = node_text(pattern, state.text).to_string();
        let type_text = node_text(type_node, state.text).to_string();
        scope.locals.insert(name, type_text);
    }
}

fn rust_collect_generic_bounds(state: &ExtractState<'_>, node: Node<'_>, scope: &mut Scope) {
    let mut record = |param_name: &str, bound_text: &str| {
        scope.dyn_bounds.insert(param_name.to_string(), strip_generics(bound_text));
    };
    if let Some(generics) = node.child_by_field_name("type_parameters") {
        let mut cursor = generics.walk();
        for param in generics.children(&mut cursor) {
            if param.kind() != "constrained_type_parameter" {
                continue;
            }
            let Some(left) = param.child_by_field_name("left") else { continue };
            let Some(bounds) = param.child_by_field_name("bounds") else { continue };
            let mut bound_cursor = bounds.walk();
            for bound in bounds.children(&mut bound_cursor) {
                if bound.is_named() {
                    record(node_text(left, state.text), node_text(bound, state.text));
                    break;
                }
            }
        }
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() != "where_clause" {
            continue;
        }
        let mut where_cursor = child.walk();
        for predicate in child.children(&mut where_cursor) {
            if predicate.kind() != "where_predicate" {
                continue;
            }
            let Some(left) = predicate.child_by_field_name("left") else { continue };
            let Some(bounds) = predicate.child_by_field_name("bounds") else { continue };
            let mut bound_cursor = bounds.walk();
            for bound in bounds.children(&mut bound_cursor) {
                if bound.is_named() {
                    record(node_text(left, state.text), node_text(bound, state.text));
                    break;
                }
            }
        }
    }
}

fn walk_rust(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "function_item" | "function_signature_item" => {
            let Some(name) = child_name(node, "name", state.text) else { return };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, "::");
            let id = state.push_symbol(&name, &qualified, "fn", node);
            if let Some(impl_type) = &scope.impl_type {
                state.push_method_of(&id, impl_type);
            }
            let mut inner = Scope {
                container: {
                    let mut c = scope.container.clone();
                    c.push(name);
                    c
                },
                impl_type: scope.impl_type.clone(),
                caller: Some(id),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            if let Some(impl_type) = &scope.impl_type {
                inner.locals.insert("self".to_string(), impl_type.clone());
            }
            rust_collect_params(state, node, &mut inner);
            rust_collect_generic_bounds(state, node, &mut inner);
            if let Some(caller) = &inner.caller {
                let mut referenced: Vec<String> = inner
                    .locals
                    .iter()
                    .filter(|(name, _)| name.as_str() != "self")
                    .map(|(_, type_text)| {
                        dyn_trait_of(type_text)
                            .unwrap_or_else(|| strip_generics(strip_reference_wrappers(type_text)))
                    })
                    .collect();
                referenced.sort();
                referenced.dedup();
                for type_name in referenced {
                    state.extraction.type_refs.push((caller.clone(), type_name));
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_rust(state, body, &inner);
            }
        }
        "struct_item" | "enum_item" | "union_item" => {
            if let Some(name) = child_name(node, "name", state.text) {
                let qualified = qualified_name(scope, name, "::");
                state.push_symbol(name, &qualified, "type", node);
            }
        }
        "trait_item" => {
            let Some(name) = child_name(node, "name", state.text) else { return };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, "::");
            state.push_symbol(&name, &qualified, "trait", node);
            let inner = Scope {
                container: {
                    let mut c = scope.container.clone();
                    c.push(name);
                    c
                },
                impl_type: None,
                caller: scope.caller.clone(),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            walk_children(state, node, &inner);
        }
        "impl_item" => {
            let Some(type_node) = node.child_by_field_name("type") else { return };
            let type_name = strip_generics(node_text(type_node, state.text));
            let trait_name = node
                .child_by_field_name("trait")
                .map(|n| strip_generics(node_text(n, state.text)));
            state.extraction.impls.push(ImplRow { type_name: type_name.clone(), trait_name: trait_name.clone() });
            let inner = Scope {
                container: vec![type_name.clone()],
                impl_type: Some(type_name),
                caller: scope.caller.clone(),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(state, body, &inner);
            }
        }
        "const_item" | "static_item" | "type_item" | "macro_definition" => {
            if let Some(name) = child_name(node, "name", state.text) {
                let qualified = qualified_name(scope, name, "::");
                let kind = if node.kind() == "macro_definition" { "macro" } else { "const" };
                state.push_symbol(name, &qualified, kind, node);
            }
        }
        "mod_item" => {
            let Some(name) = child_name(node, "name", state.text) else { return };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, "::");
            state.push_symbol(&name, &qualified, "mod", node);
            let inner = Scope {
                container: {
                    let mut c = scope.container.clone();
                    c.push(name);
                    c
                },
                impl_type: None,
                caller: scope.caller.clone(),
                locals: scope.locals.clone(),
                dyn_bounds: scope.dyn_bounds.clone(),
            };
            walk_children(state, node, &inner);
        }
        "block" => {
            let mut sequential = scope.clone();
            let mut cursor = node.walk();
            let children: Vec<Node<'_>> = node.children(&mut cursor).collect();
            for child in children {
                if child.kind() == "let_declaration" {
                    walk_children(state, child, &sequential);
                    rust_record_let(state, child, &mut sequential);
                } else {
                    walk_rust(state, child, &sequential);
                }
            }
        }
        "call_expression" => {
            rust_record_call(state, node, scope);
            walk_children(state, node, scope);
        }
        _ => walk_children(state, node, scope),
    }
}

fn rust_record_let(state: &mut ExtractState<'_>, node: Node<'_>, scope: &mut Scope) {
    let Some(pattern) = node.child_by_field_name("pattern") else { return };
    if pattern.kind() != "identifier" {
        return;
    }
    let name = node_text(pattern, state.text).to_string();
    let type_text = if let Some(type_node) = node.child_by_field_name("type") {
        Some(node_text(type_node, state.text).to_string())
    } else {
        node.child_by_field_name("value").and_then(|v| rust_constructor_type(state, v))
    };
    let Some(type_text) = type_text else { return };
    if let Some(caller) = &scope.caller {
        let referenced =
            dyn_trait_of(&type_text).unwrap_or_else(|| strip_generics(strip_reference_wrappers(&type_text)));
        state.extraction.type_refs.push((caller.clone(), referenced));
    }
    scope.locals.insert(name, type_text);
}

fn rust_constructor_type(state: &ExtractState<'_>, value: Node<'_>) -> Option<String> {
    if value.kind() != "call_expression" {
        return None;
    }
    let function = value.child_by_field_name("function")?;
    if function.kind() != "scoped_identifier" {
        return None;
    }
    let path = node_text(function, state.text);
    let (type_part, method) = path.rsplit_once("::")?;
    if matches!(method, "new" | "default" | "with_capacity" | "from") {
        Some(strip_generics(type_part))
    } else {
        None
    }
}

fn call_arity(node: Node<'_>) -> usize {
    node.child_by_field_name("arguments")
        .map(|args| {
            let mut cursor = args.walk();
            args.children(&mut cursor).filter(|c| c.is_named()).count()
        })
        .unwrap_or(0)
}

fn rust_record_call(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    let Some(function) = node.child_by_field_name("function") else { return };
    let caller = state.caller_or_file(scope);
    let arity = call_arity(node);
    match function.kind() {
        "identifier" => {
            state.extraction.calls.push(CallSite {
                caller,
                callee_name: node_text(function, state.text).to_string(),
                arity,
                receiver: Receiver::Free,
            });
        }
        "scoped_identifier" => {
            state.extraction.calls.push(CallSite {
                caller,
                callee_name: node_text(function, state.text).to_string(),
                arity,
                receiver: Receiver::Path(node_text(function, state.text).to_string()),
            });
        }
        "field_expression" => {
            let Some(field) = function.child_by_field_name("field") else { return };
            let method = node_text(field, state.text).to_string();
            let receiver = function
                .child_by_field_name("value")
                .map(|value| rust_receiver(state, value, scope))
                .unwrap_or(Receiver::Unknown);
            state.extraction.calls.push(CallSite { caller, callee_name: method, arity: arity + 1, receiver });
        }
        _ => {}
    }
}

fn rust_receiver(state: &ExtractState<'_>, value: Node<'_>, scope: &Scope) -> Receiver {
    match value.kind() {
        "identifier" | "self" => {
            let name = node_text(value, state.text);
            if let Some(type_text) = scope.locals.get(name) {
                if let Some(dyn_trait) = dyn_trait_of(type_text) {
                    return Receiver::DynTrait(dyn_trait);
                }
                let base = strip_generics(strip_reference_wrappers(type_text));
                if let Some(bound) = scope.dyn_bounds.get(&base) {
                    return Receiver::DynTrait(bound.clone());
                }
                return Receiver::Typed(base);
            }
            Receiver::Unknown
        }
        "field_expression" => Receiver::Unknown,
        "call_expression" => rust_constructor_type(state, value).map(Receiver::Typed).unwrap_or(Receiver::Unknown),
        _ => Receiver::Unknown,
    }
}

fn walk_cpp(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "function_definition" => {
            let name = cpp_declarator_name(state, node);
            let Some(name) = name else {
                walk_children(state, node, scope);
                return;
            };
            let qualified = qualified_name(scope, &name, "::");
            let id = state.push_symbol(&name, &qualified, "fn", node);
            let inner = Scope {
                container: scope.container.clone(),
                impl_type: scope.impl_type.clone(),
                caller: Some(id),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(state, body, &inner);
            }
        }
        "struct_specifier" | "class_specifier" | "enum_specifier" => {
            if let Some(name) = child_name(node, "name", state.text) {
                let qualified = qualified_name(scope, name, "::");
                state.push_symbol(name, &qualified, "type", node);
                let inner = Scope {
                    container: {
                        let mut c = scope.container.clone();
                        c.push(name.to_string());
                        c
                    },
                    impl_type: Some(name.to_string()),
                    caller: scope.caller.clone(),
                    locals: HashMap::new(),
                    dyn_bounds: HashMap::new(),
                };
                if let Some(body) = node.child_by_field_name("body") {
                    walk_children(state, body, &inner);
                    return;
                }
            }
            walk_children(state, node, scope);
        }
        "call_expression" => {
            if let Some(function) = node.child_by_field_name("function") {
                let caller = state.caller_or_file(scope);
                let arity = call_arity(node);
                match function.kind() {
                    "identifier" => state.extraction.calls.push(CallSite {
                        caller,
                        callee_name: node_text(function, state.text).to_string(),
                        arity,
                        receiver: Receiver::Free,
                    }),
                    "field_expression" => {
                        if let Some(field) = function.child_by_field_name("field") {
                            state.extraction.calls.push(CallSite {
                                caller,
                                callee_name: node_text(field, state.text).to_string(),
                                arity: arity + 1,
                                receiver: Receiver::Unknown,
                            });
                        }
                    }
                    "qualified_identifier" => state.extraction.calls.push(CallSite {
                        caller,
                        callee_name: node_text(function, state.text).to_string(),
                        arity,
                        receiver: Receiver::Path(node_text(function, state.text).to_string()),
                    }),
                    _ => {}
                }
            }
            walk_children(state, node, scope);
        }
        _ => walk_children(state, node, scope),
    }
}

fn cpp_declarator_name(state: &ExtractState<'_>, node: Node<'_>) -> Option<String> {
    let mut declarator = node.child_by_field_name("declarator")?;
    loop {
        match declarator.kind() {
            "function_declarator" => {
                declarator = declarator.child_by_field_name("declarator")?;
            }
            "pointer_declarator" | "reference_declarator" => {
                declarator = declarator.child_by_field_name("declarator").or_else(|| {
                    let mut cursor = declarator.walk();
                    let named: Vec<Node<'_>> = declarator.children(&mut cursor).filter(|c| c.is_named()).collect();
                    named.into_iter().next()
                })?;
            }
            "identifier" | "field_identifier" | "qualified_identifier" | "destructor_name" | "operator_name" => {
                return Some(node_text(declarator, state.text).to_string());
            }
            _ => return None,
        }
    }
}

fn walk_python(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "class_definition" => {
            let Some(name) = child_name(node, "name", state.text) else { return };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, ".");
            state.push_symbol(&name, &qualified, "type", node);
            let inner = Scope {
                container: {
                    let mut c = scope.container.clone();
                    c.push(name.clone());
                    c
                },
                impl_type: Some(qualified),
                caller: scope.caller.clone(),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(state, body, &inner);
            }
        }
        "function_definition" => {
            let Some(name) = child_name(node, "name", state.text) else { return };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, ".");
            let id = state.push_symbol(&name, &qualified, "fn", node);
            if let Some(impl_type) = &scope.impl_type {
                state.push_method_of(&id, impl_type);
            }
            let mut inner = Scope {
                container: {
                    let mut c = scope.container.clone();
                    c.push(name);
                    c
                },
                impl_type: scope.impl_type.clone(),
                caller: Some(id),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            if let Some(impl_type) = &scope.impl_type {
                inner.locals.insert("self".to_string(), impl_type.clone());
            }
            if let Some(params) = node.child_by_field_name("parameters") {
                let mut cursor = params.walk();
                for param in params.children(&mut cursor) {
                    if param.kind() == "typed_parameter" {
                        let mut inner_cursor = param.walk();
                        let ident = param.children(&mut inner_cursor).find(|c| c.kind() == "identifier");
                        if let (Some(ident), Some(type_node)) = (ident, param.child_by_field_name("type")) {
                            inner
                                .locals
                                .insert(node_text(ident, state.text).to_string(), node_text(type_node, state.text).to_string());
                        }
                    }
                }
            }
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(state, body, &inner);
            }
        }
        "call" => {
            if let Some(function) = node.child_by_field_name("function") {
                let caller = state.caller_or_file(scope);
                let arity = call_arity(node);
                match function.kind() {
                    "identifier" => state.extraction.calls.push(CallSite {
                        caller,
                        callee_name: node_text(function, state.text).to_string(),
                        arity,
                        receiver: Receiver::Free,
                    }),
                    "attribute" => {
                        if let Some(attribute) = function.child_by_field_name("attribute") {
                            let receiver = function
                                .child_by_field_name("object")
                                .map(|object| match object.kind() {
                                    "identifier" => {
                                        let name = node_text(object, state.text);
                                        scope
                                            .locals
                                            .get(name)
                                            .map(|t| Receiver::Typed(strip_generics(t)))
                                            .unwrap_or(Receiver::Unknown)
                                    }
                                    _ => Receiver::Unknown,
                                })
                                .unwrap_or(Receiver::Unknown);
                            state.extraction.calls.push(CallSite {
                                caller,
                                callee_name: node_text(attribute, state.text).to_string(),
                                arity: arity + 1,
                                receiver,
                            });
                        }
                    }
                    _ => {}
                }
            }
            walk_children(state, node, scope);
        }
        _ => walk_children(state, node, scope),
    }
}

fn walk_typescript(state: &mut ExtractState<'_>, node: Node<'_>, scope: &Scope) {
    match node.kind() {
        "class_declaration" | "abstract_class_declaration" | "interface_declaration" => {
            let Some(name) = child_name(node, "name", state.text) else {
                walk_children(state, node, scope);
                return;
            };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, ".");
            state.push_symbol(&name, &qualified, "type", node);
            let inner = Scope {
                container: {
                    let mut c = scope.container.clone();
                    c.push(name);
                    c
                },
                impl_type: Some(qualified),
                caller: scope.caller.clone(),
                locals: HashMap::new(),
                dyn_bounds: HashMap::new(),
            };
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(state, body, &inner);
            }
        }
        "function_declaration" | "generator_function_declaration" | "method_definition" => {
            let Some(name) = child_name(node, "name", state.text) else {
                walk_children(state, node, scope);
                return;
            };
            let name = name.to_string();
            let qualified = qualified_name(scope, &name, ".");
            let id = state.push_symbol(&name, &qualified, "fn", node);
            if let Some(impl_type) = &scope.impl_type {
                state.push_method_of(&id, impl_type);
            }
            let inner = ts_function_scope(state, node, scope, &name, id);
            if let Some(body) = node.child_by_field_name("body") {
                walk_children(state, body, &inner);
            }
        }
        "variable_declarator" | "public_field_definition" => {
            let bound = node.child_by_field_name("value").filter(|value| {
                matches!(value.kind(), "arrow_function" | "function_expression" | "function")
            });
            if let (Some(name_node), Some(value)) = (node.child_by_field_name("name"), bound) {
                if name_node.kind() == "identifier" || name_node.kind() == "property_identifier" {
                    let name = node_text(name_node, state.text).to_string();
                    let qualified = qualified_name(scope, &name, ".");
                    let id = state.push_symbol(&name, &qualified, "fn", value);
                    if let Some(impl_type) = &scope.impl_type {
                        state.push_method_of(&id, impl_type);
                    }
                    let inner = ts_function_scope(state, value, scope, &name, id);
                    if let Some(body) = value.child_by_field_name("body") {
                        walk_children(state, body, &inner);
                    }
                    return;
                }
            }
            walk_children(state, node, scope);
        }
        "call_expression" => {
            if let Some(function) = node.child_by_field_name("function") {
                let caller = state.caller_or_file(scope);
                let arity = call_arity(node);
                match function.kind() {
                    "identifier" => state.extraction.calls.push(CallSite {
                        caller,
                        callee_name: node_text(function, state.text).to_string(),
                        arity,
                        receiver: Receiver::Free,
                    }),
                    "member_expression" => {
                        if let Some(property) = function.child_by_field_name("property") {
                            let receiver = function
                                .child_by_field_name("object")
                                .map(|object| ts_receiver(state, object, scope))
                                .unwrap_or(Receiver::Unknown);
                            state.extraction.calls.push(CallSite {
                                caller,
                                callee_name: node_text(property, state.text).to_string(),
                                arity: arity + 1,
                                receiver,
                            });
                        }
                    }
                    _ => {}
                }
            }
            walk_children(state, node, scope);
        }
        _ => walk_children(state, node, scope),
    }
}

fn ts_function_scope(state: &ExtractState<'_>, func: Node<'_>, scope: &Scope, name: &str, id: String) -> Scope {
    let mut inner = Scope {
        container: {
            let mut c = scope.container.clone();
            c.push(name.to_string());
            c
        },
        impl_type: scope.impl_type.clone(),
        caller: Some(id),
        locals: HashMap::new(),
        dyn_bounds: HashMap::new(),
    };
    if let Some(impl_type) = &scope.impl_type {
        inner.locals.insert("this".to_string(), impl_type.clone());
    }
    if let Some(params) = func.child_by_field_name("parameters") {
        let mut cursor = params.walk();
        for param in params.children(&mut cursor) {
            if !matches!(param.kind(), "required_parameter" | "optional_parameter") {
                continue;
            }
            let (Some(pattern), Some(type_node)) =
                (param.child_by_field_name("pattern"), param.child_by_field_name("type"))
            else {
                continue;
            };
            if pattern.kind() != "identifier" {
                continue;
            }
            let ty = ts_type_text(type_node, state.text);
            if !ty.is_empty() {
                inner.locals.insert(node_text(pattern, state.text).to_string(), ty);
            }
        }
    }
    inner
}

fn ts_type_text(type_node: Node<'_>, text: &str) -> String {
    strip_generics(node_text(type_node, text).trim_start_matches(':').trim())
}

fn ts_receiver(state: &ExtractState<'_>, object: Node<'_>, scope: &Scope) -> Receiver {
    match object.kind() {
        "identifier" | "this" => scope
            .locals
            .get(node_text(object, state.text))
            .map(|t| Receiver::Typed(strip_generics(t)))
            .unwrap_or(Receiver::Unknown),
        _ => Receiver::Unknown,
    }
}

impl ExtractState<'_> {
    fn push_method_of(&mut self, method_id: &str, type_id: &str) {
        self.extraction.method_edges.push((method_id.to_string(), type_id.to_string()));
    }
}
