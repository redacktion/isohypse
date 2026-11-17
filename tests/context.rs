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
    assert!(outline.contains("5:fn long_body() -> u32 {"), "signature stays visible: {outline}");
    assert!(outline.contains("6-12: … (long_body)"), "body collapses to a summary row: {outline}");
    assert!(!outline.contains("let c = 3"), "elided body rows are not shown: {outline}");
    let tag = tag_of(&outline);

    let inside_elided = patcher.apply_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
        {"replace": {"lines": [8, 8]}, "with": "    let c = 30;"}
    ]}));
    assert!(inside_elided.is_err(), "editing inside an elided region must refuse: {inside_elided:?}");
}

#[test]
fn an_edit_outside_the_read_window_is_refused() {
    let root = scratch_dir("read-window");
    fs::write(root.join("window.txt"), "w1\nw2\nw3\nw4\nw5\nw6\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let window = patcher.read("window.txt", Some(1), Some(3)).unwrap();
    let tag = tag_of(&window);

    let outside = patcher.apply_edits(&json!({"path": "window.txt", "tag": tag, "edits": [
        {"replace": {"lines": [5, 5]}, "with": "W5"}
    ]}));
    assert!(outside.is_err(), "edits outside the read window must refuse: {outside:?}");
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
    assert!(explored.contains("[lib.rs#"), "explore shows tagged source: {explored}");
    assert!(explored.contains("1:fn greet() -> String {"), "explore shows numbered lines: {explored}");
    assert!(explored.contains("callers (confirmed): main"), "explore shows callers: {explored}");
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
    assert_eq!(capped.matches("## ").count(), 1, "max=1 yields one section: {capped}");

    let compact = explore(daemon::ExploreOptions { compact: true, ..Default::default() });
    assert!(compact.contains("src/a.rs:1 fn alpha"), "compact line: {compact}");
    assert!(!compact.contains("[src/a.rs#"), "compact omits the tag header: {compact}");

    let scoped = explore(daemon::ExploreOptions { path_prefixes: vec!["src/".into()], ..Default::default() });
    assert!(scoped.contains("src/a.rs"), "{scoped}");
    assert!(!scoped.contains("other.rs"), "path filter excludes other.rs: {scoped}");
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
    assert!(bounded.contains("compact view to bound output"), "auto-compacts by default: {bounded}");
    assert!(!bounded.contains("let _v50 = 50"), "compact omits the body: {bounded}");

    let full = daemon::explore(&context.index, &context.root, None, None, "sprawling", &daemon::ExploreOptions { full: true, ..Default::default() })
        .unwrap()
        .render();
    assert!(full.contains("let _v50 = 50"), "full renders the body: {full}");
}

// ── retained history ─────────────────────────────────────────────────────────

/// A workspace with one recorded revision of code.rs. The journal is keyed by
/// path, so each caller needs its own workspace name.
fn recorded(name: &str) -> (std::path::PathBuf, ObjectStore, String, String) {
    let root = scratch_dir(name);
    let source = format!("fn history_{}() -> u32 {{\n    41\n}}\n", std::process::id());
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
    assert_eq!(list["total"], 1);
    assert_eq!(list["revisions"][0]["full_tag"], full);
    let saved = list["revisions"][0]["saved_at_unix"].as_u64().unwrap();

    let filtered: Value = serde_json::from_str(&history::list(&root, &json!({
        "arg": "code.rs", "tag": &full[..7], "since": saved, "until": saved
    })).unwrap()).unwrap();
    assert_eq!(filtered["total"], 1);

    let absent: Value = serde_json::from_str(&history::list(&root, &json!({"since": saved + 1})).unwrap()).unwrap();
    assert_eq!(absent["total"], 0);
    assert!(history::read(&root, &json!({"tag": second})).is_err(), "a sibling workspace must not leak into the default scope");

    let read = history::read(&root, &json!({"tag": &full[..7], "range": "2-2"})).unwrap();
    assert!(read.starts_with(&format!("[{path}#{}]", &full[..7])));
    assert!(read.contains("2:    41"));
    assert!(!read.contains("1:fn "), "a ranged read shows only the range");
    assert!(matches!(store.resolve(&full), Resolution::Found(_, bytes) if bytes.contains("history_")));
}

#[test]
fn history_rejects_bad_filters_and_ambiguous_tags() {
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
        assert!(history::list(&root, &input).is_err(), "{input}");
    }

    let store = ObjectStore::open().unwrap();
    let path = root.join("missing.rs").to_string_lossy().into_owned();
    store.journal_record(&path, &format!("abcd{}", "0".repeat(60))).unwrap();
    store.journal_record(&path, &format!("abcd{}", "1".repeat(60))).unwrap();

    let page: Value = serde_json::from_str(&history::list(&root, &json!({"max": 1})).unwrap()).unwrap();
    assert_eq!(page["total"], 2);
    assert_eq!(page["next_offset"], 1);
    assert!(page["revisions"][0]["saved_at_unix"].is_null(), "a file with no mtime reports none");
    assert!(history::read(&root, &json!({"tag": "abcd"})).unwrap_err().contains("ambiguous"));
    assert!(history::read(&root, &json!({"tag": format!("abcd{}", "0".repeat(60))})).unwrap_err().contains("missing or corrupt"));
}

#[test]
fn saved_reads_are_bounded_and_outline_hides_lines() {
    let root = scratch_dir("history-bounds");
    let store = ObjectStore::open().unwrap();
    let source = (1..=140).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
    let full = store.put(&source).unwrap();
    store.journal_record(&root.join("long.txt").to_string_lossy(), &full).unwrap();

    let read = history::read(&root, &json!({"tag": full})).unwrap();
    assert!(read.contains("120:line 120"), "the default window stops at 120 lines");
    assert!(!read.contains("121:line 121"));
    for range in ["0-1", "2-1", "abc", "1-2-3"] {
        assert!(history::read(&root, &json!({"tag": full, "range": range})).is_err(), "{range}");
    }

    let source = "fn historical_outline_unique() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = a + b + c + d + e;\n}\n";
    fs::write(root.join("outline.rs"), source).unwrap();
    let full = store.put(source).unwrap();
    store.journal_record(&root.join("outline.rs").to_string_lossy(), &full).unwrap();

    let read = history::read(&root, &json!({"tag": full, "outline": true})).unwrap();
    assert!(!read.contains("let c = 3"), "an outline hides elided lines");
    let mut patcher = Patcher::new(&root).unwrap();
    assert!(patcher.apply_edits_verified(&json!({"path": "outline.rs", "tag": full,
        "edits": [{"replace": {"lines": [4, 4]}, "with": "    let c = 9;"}]}), Some("true")).is_err(),
        "an outline must not mark hidden lines as seen");
}

#[test]
fn history_reads_never_append_and_survive_deletion() {
    let (root, store, full, path) = recorded("history-append");
    let before = store.journal_entries(&path);

    history::list(&root, &json!({})).unwrap();
    history::read(&root, &json!({"tag": &full[..7], "range": "2-2"})).unwrap();
    assert_eq!(store.journal_entries(&path), before, "listing and retrieval never append or reorder history");
    assert!(fs::read_to_string(&path).unwrap().contains("history_"), "a read never rewrites the live file");

    fs::remove_file(&path).unwrap();
    assert!(history::read(&root, &json!({"arg": "code.rs", "tag": full})).unwrap().contains("2:    41"),
        "retained source survives deletion of the live file");
}

#[test]
fn a_retained_revision_is_editable_only_after_a_read_and_a_pass() {
    let (root, _store, full, path) = recorded("history-edit");
    // Only line 2 is shown, so line 3 must stay protected.
    history::read(&root, &json!({"tag": &full[..7], "range": "2-2"})).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    assert!(patcher.apply_edits_verified(&json!({
        "path": "code.rs", "tag": &full[..7],
        "edits": [{"replace": {"lines": [3, 3]}, "with": "}"}]
    }), Some("true")).is_err(), "unseen saved lines remain protected");

    let edit = json!({"path": "code.rs", "tag": &full[..7],
        "edits": [{"replace": {"lines": [2, 2]}, "with": "    42"}]});
    assert!(patcher.apply_edits_verified(&edit, Some("false")).is_err());
    assert!(fs::read_to_string(&path).unwrap().contains("41"), "failed validation rolls back");
    patcher.apply_edits_verified(&edit, Some("true")).unwrap();
    assert!(fs::read_to_string(&path).unwrap().contains("42"));
}
