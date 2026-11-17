//! context.* — reads, search, explore, and retained history.
mod support;

use std::fs;

use isohypse::daemon;
use isohypse::history;
use isohypse::objects::{ObjectStore, Resolution};
use isohypse::opresult::OpResult;
use isohypse::patcher::Patcher;
use serde_json::{json, Value};
use support::{scratch_dir, tag_of, workspace};

const LONG_FN: &str = "fn short() -> u32 {\n    1\n}\n\nfn long_body() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    a + b + c + d + e + f\n}\n";

// ── reads, outline, and windows ──────────────────────────────────────────────

#[test]
fn outline_elides_a_long_body_and_refuses_edits_inside_it() {
    let root = scratch_dir("outline");
    fs::write(root.join("code.rs"), LONG_FN).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let outline = patcher.read_with("code.rs", None, None, true, None).unwrap();
    let __ok0: bool = outline.contains("5:fn long_body() -> u32 {");
    let __ok1: bool = outline.contains("6-12: … (long_body)");
    let __ok2: bool = !outline.contains("let c = 3");
    let tag = tag_of(&outline);

    let inside_elided = patcher.apply_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
        {"replace": {"lines": [8, 8]}, "with": "    let c = 30;"}
    ]}));
    let __ok3: bool = inside_elided.is_err();

    let failed: Vec<&str> = [(__ok0, "signature stays visible: {outline}"), (__ok1, "body collapses to a summary row: {outline}"), (__ok2, "elided body rows are not shown: {outline}"), (__ok3, "editing inside an elided region must refuse: {inside_elided:?}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}


// ── explore ──────────────────────────────────────────────────────────────────

#[test]
fn explore_shows_tagged_source_lines_and_callers() {
    let root = scratch_dir("explore");
    fs::write(root.join("lib.rs"), "fn greet() -> String {\n    String::from(\"hi\")\n}\n\nfn main() {\n    greet();\n}\n").unwrap();
    let context = workspace(&root);

    let explored = daemon::explore(&context.index, &context.root, None, None, "greet", &daemon::ExploreOptions::default())
        .unwrap()
        .render();
    let __ok0: bool = explored.contains("[lib.rs#");
    let __ok1: bool = explored.contains("1:fn greet() -> String {");
    let __ok2: bool = explored.contains("callers (confirmed): main");

    let failed: Vec<&str> = [(__ok0, "explore shows tagged source: {explored}"), (__ok1, "explore shows numbered lines: {explored}"), (__ok2, "explore shows callers: {explored}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn explore_caps_compacts_and_filters_by_path() {
    let root = scratch_dir("explore-options");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn alphabet() {}\n").unwrap();
    fs::write(root.join("other.rs"), "fn alpha_other() {}\n").unwrap();
    let context = workspace(&root);
    let explore = |options: daemon::ExploreOptions| {
        daemon::explore(&context.index, &context.root, None, None, "alpha", &options).unwrap().render()
    };

    let capped = explore(daemon::ExploreOptions { max: 1, ..Default::default() });
    let __ok0: bool = (capped.matches("## ").count()) == (1);

    let compact = explore(daemon::ExploreOptions { compact: true, ..Default::default() });
    let __ok1: bool = compact.contains("src/a.rs:1 fn alpha");
    let __ok2: bool = !compact.contains("[src/a.rs#");

    let scoped = explore(daemon::ExploreOptions { path_prefixes: vec!["src/".into()], ..Default::default() });
    let __ok3: bool = scoped.contains("src/a.rs");
    let __ok4: bool = !scoped.contains("other.rs");

    let failed: Vec<&str> = [(__ok0, "max=1 yields one section: {capped}"), (__ok1, "compact line: {compact}"), (__ok2, "compact omits the tag header: {compact}"), (__ok3, "{scoped}"), (__ok4, "path filter excludes other.rs: {scoped}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn explore_auto_compacts_over_a_line_budget() {
    let root = scratch_dir("explore-budget");
    let mut body = String::new();
    for f in 0..4 {
        body.push_str(&format!("fn sprawling{f}() {{\n"));
        for i in 0..120 {
            body.push_str(&format!("    let _v{i} = {i};\n"));
        }
        body.push_str("}\n");
    }
    fs::write(root.join("big.rs"), body).unwrap();
    let context = workspace(&root);

    let bounded = daemon::explore(&context.index, &context.root, None, None, "sprawling", &daemon::ExploreOptions::default())
        .unwrap()
        .render();
    let __ok0: bool = bounded.contains("compact view to bound output");
    let __ok1: bool = !bounded.contains("let _v50 = 50");

    let full = daemon::explore(&context.index, &context.root, None, None, "sprawling", &daemon::ExploreOptions { full: true, ..Default::default() })
        .unwrap()
        .render();
    let __ok2: bool = full.contains("let _v50 = 50");

    let failed: Vec<&str> = [(__ok0, "auto-compacts by default: {bounded}"), (__ok1, "compact omits the body: {bounded}"), (__ok2, "full renders the body: {full}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── retained history ─────────────────────────────────────────────────────────

/// A workspace with one recorded revision of code.rs. The journal is keyed by
/// path, so each caller needs its own workspace name.
fn recorded(name: &str) -> (std::path::PathBuf, ObjectStore, String, String) {
    let root = scratch_dir(name);
    let ident: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let source = format!("fn history_{}_{ident}() -> u32 {{\n    41\n}}\n", std::process::id());
    fs::write(root.join("code.rs"), &source).unwrap();
    let store = ObjectStore::open().unwrap();
    let full = store.put(&source).unwrap();
    let path = root.join("code.rs").to_string_lossy().into_owned();
    store.journal_record(&path, &full).unwrap();
    (root, store, full, path)
}

#[test]
fn history_lists_filters_and_pages_recorded_revisions() {
    let (root, store, full, path) = recorded("history-list");
    let other = root.with_extension("other");
    fs::create_dir_all(&other).unwrap();
    let second = store.put("fn isolated_other() {}\n").unwrap();
    store.journal_record(&other.join("other.rs").to_string_lossy(), &second).unwrap();

    let list: Value = serde_json::from_str(&history::list(&root, &json!({})).unwrap()).unwrap();
    let __ok0: bool = (list["total"]) == (1);
    let __ok1: bool = (list["revisions"][0]["full_tag"]) == (full);
    let saved = list["revisions"][0]["saved_at_unix"].as_u64().unwrap();

    let filtered: Value = serde_json::from_str(&history::list(&root, &json!({
        "arg": "code.rs", "tag": &full[..7], "since": saved, "until": saved
    })).unwrap()).unwrap();
    let __ok2: bool = (filtered["total"]) == (1);

    let absent: Value = serde_json::from_str(&history::list(&root, &json!({"since": saved + 1})).unwrap()).unwrap();
    let __ok3: bool = (absent["total"]) == (0);
    let __ok4: bool = history::read(&root, &json!({"tag": second})).is_err();

    let read = history::read(&root, &json!({"tag": &full[..7], "range": "2-2"})).unwrap();
    let __ok5: bool = read.starts_with(&format!("[{path}#{}]", &full[..7]));
    let __ok6: bool = read.contains("2:    41");
    let __ok7: bool = !read.contains("1:fn ");
    let __ok8: bool = matches!(store.resolve(&full), Resolution::Found(_, bytes) if bytes.contains("history_"));

    let failed: Vec<&str> = [(__ok0, "list[\"total\"] == 1"), (__ok1, "list[\"revisions\"][0][\"full_tag\"] == full"), (__ok2, "filtered[\"total\"] == 1"), (__ok3, "absent[\"total\"] == 0"), (__ok4, "a sibling workspace must not leak into the default scope"), (__ok5, "read.starts_with(&format!(\"[{{path}}#{{}}]\", &full[..7]))"), (__ok6, "read.contains(\"2: 41\")"), (__ok7, "a ranged read shows only the range"), (__ok8, "matches!(store.resolve(&full), Resolution::Found(_, bytes) if bytes.co")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn history_rejects_bad_filters_and_ambiguous_tags() {
    let mut failed: Vec<String> = Vec::new();
    let root = scratch_dir("history-errors");
    for input in [
        json!({"workspace": "../outside"}),
        json!({"workspace": "/tmp/../outside"}),
        json!({"arg": "../outside"}),
        json!({"tag": "xyz"}),
        json!({"since": 5, "until": 4}),
        json!({"since": "yesterday"}),
        json!({"max": 0}),
        json!({"offset": -1}),
    ] {
        { let ok: bool = history::list(&root, &input).is_err(); if !ok { failed.push("{input}".to_string()); } };
    }

    let store = ObjectStore::open().unwrap();
    let path = root.join("missing.rs").to_string_lossy().into_owned();
    store.journal_record(&path, &format!("abcd{}", "0".repeat(60))).unwrap();
    store.journal_record(&path, &format!("abcd{}", "1".repeat(60))).unwrap();

    let page: Value = serde_json::from_str(&history::list(&root, &json!({"max": 1})).unwrap()).unwrap();
    { let ok: bool = (page["total"]) == (2); if !ok { failed.push("page[\"total\"] == 2".to_string()); } };
    { let ok: bool = (page["next_offset"]) == (1); if !ok { failed.push("page[\"next_offset\"] == 1".to_string()); } };
    { let ok: bool = page["revisions"][0]["saved_at_unix"].is_null(); if !ok { failed.push("a file with no mtime reports none".to_string()); } };
    { let ok: bool = history::read(&root, &json!({"tag": "abcd"})).unwrap_err().contains("ambiguous"); if !ok { failed.push("history::read(&root, &json!({{\"tag\": \"abcd\"}})).unwrap_err().contains(\"a".to_string()); } };
    { let ok: bool = history::read(&root, &json!({"tag": format!("abcd{}", "0".repeat(60))})).unwrap_err().contains("missing or corrupt"); if !ok { failed.push("history::read(&root, &json!({{\"tag\": format!(\"abcd{{}}\", \"0\".repeat(60))}}".to_string()); } };
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}

#[test]
fn saved_reads_are_bounded_and_outline_hides_lines() {
    let mut failed: Vec<String> = Vec::new();
    let root = scratch_dir("history-bounds");
    let store = ObjectStore::open().unwrap();
    let source = (1..=140).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    let full = store.put(&source).unwrap();
    store.journal_record(&root.join("long.txt").to_string_lossy(), &full).unwrap();

    let read = history::read(&root, &json!({"tag": full})).unwrap();
    { let ok: bool = read.contains("120:line 120"); if !ok { failed.push("the default window stops at 120 lines".to_string()); } };
    { let ok: bool = !read.contains("121:line 121"); if !ok { failed.push("!read.contains(\"121:line 121\")".to_string()); } };
    for range in ["0-1", "2-1", "abc", "1-2-3"] {
        { let ok: bool = history::read(&root, &json!({"tag": full, "range": range})).is_err(); if !ok { failed.push("{range}".to_string()); } };
    }

    let source = "fn historical_outline_unique() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = a + b + c + d + e;\n}\n";
    fs::write(root.join("outline.rs"), source).unwrap();
    let full = store.put(source).unwrap();
    store.journal_record(&root.join("outline.rs").to_string_lossy(), &full).unwrap();

    let read = history::read(&root, &json!({"tag": full, "outline": true})).unwrap();
    { let ok: bool = !read.contains("let c = 3"); if !ok { failed.push("an outline hides elided lines".to_string()); } };
    let mut patcher = Patcher::new(&root).unwrap();
    { let ok: bool = patcher.apply_edits_verified(&json!({"path": "outline.rs", "tag": full,
        "edits": [{"replace": {"lines": [4, 4]}, "with": "    let c = 9;"}]}), Some("true")).is_err(); if !ok { failed.push("an outline must not mark hidden lines as seen".to_string()); } };
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}


#[test]
fn a_retained_revision_is_editable_only_after_a_read_and_a_pass() {
    let (root, _store, full, path) = recorded("history-edit");
    // Only line 2 is shown, so line 3 must stay protected.
    history::read(&root, &json!({"tag": &full[..7], "range": "2-2"})).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let __ok0: bool = patcher.apply_edits_verified(&json!({
        "path": "code.rs", "tag": &full[..7],
        "edits": [{"replace": {"lines": [3, 3]}, "with": "}"}]
    }), Some("true")).is_err();

    let edit = json!({"path": "code.rs", "tag": &full[..7],
        "edits": [{"replace": {"lines": [2, 2]}, "with": "    42"}]});
    let __ok1: bool = patcher.apply_edits_verified(&edit, Some("false")).is_err();
    let __ok2: bool = fs::read_to_string(&path).unwrap().contains("41");
    patcher.apply_edits_verified(&edit, Some("true")).unwrap();
    let __ok3: bool = fs::read_to_string(&path).unwrap().contains("42");

    let failed: Vec<&str> = [(__ok0, "unseen saved lines remain protected"), (__ok1, "patcher.apply_edits_verified(&edit, Some(\"false\")).is_err()"), (__ok2, "failed validation rolls back"), (__ok3, "fs::read_to_string(&path).unwrap().contains(\"42\")")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}
