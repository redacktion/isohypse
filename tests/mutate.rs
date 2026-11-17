//! mutate.* — anchored edits, authoring, deletion, undo, and rollback.
mod support;

use std::fs;

use isohypse::edit::compile;
use isohypse::patch::Hunk;
use isohypse::patcher::Patcher;
use isohypse::serve::Dispatcher;
use isohypse::tag::display_tag;
use serde_json::json;
use support::{dispatcher, read_tag, scratch_dir};

const GREET: &str = "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)\ngreet(\"world\")\n";
const TWO_FNS: &str = "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n";

/// A workspace holding greet.py, other.py, and PLAN.md, ready for editing.
fn playground(name: &str) -> (std::path::PathBuf, Patcher) {
    let root = scratch_dir(name);
    fs::write(root.join("greet.py"), GREET).unwrap();
    fs::write(root.join("other.py"), "print(\"other\")\n").unwrap();
    fs::write(root.join("PLAN.md"), "# Plan\n- first\n").unwrap();
    let patcher = Patcher::new(&root).unwrap();
    (root, patcher)
}

// ── anchors ──────────────────────────────────────────────────────────────────

#[test]
fn lines_can_be_replaced_and_the_file_moved() {
    let (root, mut patcher) = playground("move");
    let tag = read_tag(&mut patcher, "greet.py");

    let report = patcher
        .apply_edits(&json!({"path": "greet.py", "tag": tag, "edits": [
            {"replace": {"lines": [1, 3]}, "with": "def greet(name):\n    print(f\"Hi, {name}\")"},
            {"move": {"to": "lib/greet.py"}}
        ]}))
        .unwrap();
    let __ok0: bool = (report.sections[0].op) == ("move");
    let __ok1: bool = !root.join("greet.py").exists();
    let __ok2: bool = (fs::read_to_string(root.join("lib/greet.py")).unwrap()) == ("def greet(name):\n    print(f\"Hi, {name}\")\ngreet(\"world\")\n");

    let failed: Vec<&str> = [(__ok0, "report.sections[0].op == \"move\""), (__ok1, "the source is gone after a move"), (__ok2, "fs::read_to_string(root.join(\"lib/greet.py\")).unwrap() == \"def greet(n")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn inserts_land_at_a_line_and_at_a_file_boundary() {
    let (root, mut patcher) = playground("insert");

    let plan_tag = read_tag(&mut patcher, "PLAN.md");
    patcher
        .apply_edits(&json!({"path": "PLAN.md", "tag": plan_tag, "edits": [
            {"insert": {"after": 2}, "body": "- task\n  - nested task"}
        ]}))
        .unwrap();
    let __ok0: bool = (fs::read_to_string(root.join("PLAN.md")).unwrap()) == ("# Plan\n- first\n- task\n  - nested task\n");

    let other_tag = read_tag(&mut patcher, "other.py");
    patcher
        .apply_edits(&json!({"path": "other.py", "tag": other_tag, "edits": [
            {"insert": {"before": "^"}, "body": "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)"}
        ]}))
        .unwrap();
    let __ok1: bool = (fs::read_to_string(root.join("other.py")).unwrap()) == ("def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)\nprint(\"other\")\n");

    let failed: Vec<&str> = [(__ok0, "fs::read_to_string(root.join(\"PLAN.md\")).unwrap() == \"# Plan\\n- first\\"), (__ok1, "fs::read_to_string(root.join(\"other.py\")).unwrap() == \"def greet(name)")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn symbols_and_blocks_resolve_to_the_whole_definition() {
    let (root, mut patcher) = playground("symbols");
    fs::write(root.join("block.py"), GREET).unwrap();

    let tag = read_tag(&mut patcher, "block.py");
    patcher
        .apply_edits(&json!({"path": "block.py", "tag": tag, "edits": [
            {"replace": {"symbol": "greet"}, "with": "def greet(name):\n    print(f\"Hello, {name}\")"}
        ]}))
        .unwrap();
    let __ok0: bool = (fs::read_to_string(root.join("block.py")).unwrap()) == ("def greet(name):\n    print(f\"Hello, {name}\")\ngreet(\"world\")\n");

    fs::write(root.join("svc.py"), "@cache\ndef load(key):\n    return db[key]\n").unwrap();
    let tag = read_tag(&mut patcher, "svc.py");
    patcher
        .apply_edits(&json!({"path": "svc.py", "tag": tag, "edits": [
            {"replace": {"block": 1}, "with": "@cache\ndef load(key):\n    return store[key]"}
        ]}))
        .unwrap();
    let __ok1: bool = (fs::read_to_string(root.join("svc.py")).unwrap()) == ("@cache\ndef load(key):\n    return store[key]\n");

    let failed: Vec<&str> = [(__ok0, "fs::read_to_string(root.join(\"block.py\")).unwrap() == \"def greet(name)"), (__ok1, "fs::read_to_string(root.join(\"svc.py\")).unwrap() == \"@cache\\ndef load(")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn an_anchor_that_cannot_resolve_is_refused() {
    let mut failed: Vec<String> = Vec::new();
    let root = scratch_dir("anchor-refusals");
    fs::write(root.join("a.py"), "one\ntwo\nthree\nfour\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "a.py");
    let cases = [
        (json!({"path": "a.py", "tag": tag, "edits": [{"frobnicate": true}]}), "unknown actions"),
        (json!({"path": "a.py", "tag": tag, "edits": [{"replace": {"lines": [0, 2]}, "with": "x"}]}), "zero line anchors"),
        (json!({"path": "a.py", "edits": [{"replace": {"lines": [1, 1]}, "with": "x"}]}), "a missing tag"),
        (json!({"path": "a.py", "tag": tag, "edits": [{"delete": {"symbol": "nope"}}]}), "an unknown symbol"),
        (json!({"path": "a.py", "tag": tag, "edits": [
            {"replace": {"lines": [1, 2]}, "with": "x"},
            {"replace": {"lines": [2, 3]}, "with": "y"}
        ]}), "overlapping ranges"),
        (json!({"path": "a.py", "tag": tag, "edits": [{"delete": {"lines": [1, 1]}}, {"remove": true}]}), "remove combined with edits"),
    ];
    for (op, label) in cases {
        let outcome = patcher.apply_edits(&op);
        { let ok: bool = outcome.is_err(); if !ok { failed.push(format!("{label} must be rejected: {outcome:?}")); } };
    }
    { let ok: bool = (fs::read_to_string(root.join("a.py")).unwrap()) == ("one\ntwo\nthree\nfour\n"); if !ok { failed.push("a refused edit writes nothing".to_string()); } };
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}

#[test]
fn a_stale_tag_is_recovered_when_it_can_be_and_refused_when_it_diverges() {
    let root = scratch_dir("stale");
    fs::write(root.join("s.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "s.txt");

    let unknown = patcher.apply_edits(&json!({"path": "s.txt", "tag": "feedfacefeedface", "edits": [
        {"replace": {"lines": [1, 1]}, "with": "ALPHA"}
    ]}));
    let __ok0: bool = unknown.is_err();

    fs::write(root.join("s.txt"), "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    let _ = read_tag(&mut patcher, "s.txt"); // record the grown file; the cited tag stays the old one
    let report = patcher
        .apply_edits(&json!({"path": "s.txt", "tag": tag, "edits": [
            {"replace": {"lines": [2, 2]}, "with": "BETA"}
        ]}))
        .unwrap();
    let __ok1: bool = report.sections[0].recovered;
    let __ok2: bool = (fs::read_to_string(root.join("s.txt")).unwrap()) == ("alpha\nBETA\ngamma\ndelta\nepsilon\n");

    let stale = read_tag(&mut patcher, "s.txt");
    fs::write(root.join("s.txt"), "alpha\nCHANGED\ngamma\ndelta\nepsilon\n").unwrap();
    let conflict = patcher.apply_edits(&json!({"path": "s.txt", "tag": stale, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "REWRITTEN"}
    ]}));
    let __ok3: bool = conflict.is_err();

    let failed: Vec<&str> = [(__ok0, "an unstored tag must refuse: {unknown:?}"), (__ok1, "an appended tail is recoverable"), (__ok2, "fs::read_to_string(root.join(\"s.txt\")).unwrap() == \"alpha\\nBETA\\ngamma"), (__ok3, "a diverged edit region must refuse: {conflict:?}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn the_next_tag_matches_the_file_on_disk() {
    let root = scratch_dir("next-tag");
    fs::write(root.join("t.txt"), "one\ntwo\nthree\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "t.txt");

    let report = patcher
        .apply_edits(&json!({"path": "t.txt", "tag": tag, "edits": [{"replace": {"lines": [2, 2]}, "with": "TWO"}]}))
        .unwrap();
    let new_tag = report.sections[0].new_tag.clone().unwrap();
    let __ok0: bool = (new_tag) == (display_tag(&fs::read_to_string(root.join("t.txt")).unwrap()));
    let __ok1: bool = (read_tag(&mut patcher, "t.txt")) == (new_tag);

    let followup = patcher
        .apply_edits(&json!({"path": "t.txt", "tag": new_tag, "edits": [{"insert": {"after": "$"}, "body": "four"}]}))
        .unwrap();
    let __ok2: bool = (fs::read_to_string(root.join("t.txt")).unwrap()) == ("one\nTWO\nthree\nfour\n");
    let __ok3: bool = followup.sections[0].preview.contains("+four");

    let failed: Vec<&str> = [(__ok0, "new_tag == display_tag(&fs::read_to_string(root.join(\"t.txt\")).unwrap("), (__ok1, "read_tag(&mut patcher, \"t.txt\") == new_tag"), (__ok2, "fs::read_to_string(root.join(\"t.txt\")).unwrap() == \"one\\nTWO\\nthree\\nf"), (__ok3, "followup.sections[0].preview.contains(\"+four\")")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn an_edit_that_breaks_parsing_is_refused_and_nothing_is_written() {
    let root = scratch_dir("parse-guard");
    fs::write(root.join("g.rs"), TWO_FNS).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "g.rs");

    let breaks = patcher.apply_edits(&json!({"path": "g.rs", "tag": tag, "edits": [
        {"replace": {"lines": [3, 3]}, "with": "    1 +"}
    ]}));
    let __ok0: bool = breaks.is_err();
    let err = breaks.unwrap_err();
    let __ok1: bool = err.contains("would not parse");
    let __ok2: bool = fs::read_to_string(root.join("g.rs")).unwrap().contains("fn beta");

    let failed: Vec<&str> = [(__ok0, "an edit that breaks parsing must refuse: {breaks:?}"), (__ok1, "the refusal names the guard: {err}"), (__ok2, "nothing was written")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_structural_change_names_only_the_functions_it_touched() {
    let root = scratch_dir("structural");
    fs::write(root.join("g.rs"), TWO_FNS).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "g.rs");

    let report = patcher
        .apply_edits(&json!({"path": "g.rs", "tag": tag, "edits": [
            {"replace": {"symbol": "beta"}, "with": "fn gamma() -> u32 {\n    3\n}"}
        ]}))
        .unwrap();
    let structural = report.sections[0].structural.join("\n");
    let __ok0: bool = structural.contains("fn added: gamma");
    let __ok1: bool = structural.contains("fn removed: beta");
    let __ok2: bool = !structural.contains("alpha");

    let failed: Vec<&str> = [(__ok0, "{structural}"), (__ok1, "{structural}"), (__ok2, "untouched functions stay out of the summary: {structural}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── authoring ────────────────────────────────────────────────────────────────


#[test]
fn create_refuses_a_file_that_already_exists() {
    let (root, mut patcher) = playground("create-existing");
    let error = patcher.create("greet.py", "replacement").unwrap_err();
    let __ok0: bool = error.contains("already exists");
    let __ok1: bool = error.contains("mutate.edit");
    let __ok2: bool = (fs::read_to_string(root.join("greet.py")).unwrap()) == (GREET);

    let failed: Vec<&str> = [(__ok0, "{error}"), (__ok1, "the refusal names the way to change it: {error}"), (__ok2, "nothing was overwritten")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── deletion ─────────────────────────────────────────────────────────────────


#[test]
fn delete_refuses_a_missing_file_and_a_directory() {
    let root = scratch_dir("delete-refusals");
    fs::create_dir_all(root.join("dir")).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let missing = patcher.delete("absent.txt").unwrap_err();
    let __ok0: bool = missing.contains("does not exist");
    let dir = patcher.delete("dir").unwrap_err();
    let __ok1: bool = dir.contains("is a directory");

    let failed: Vec<&str> = [(__ok0, "{missing}"), (__ok1, "{dir}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_failed_multi_op_puts_a_deleted_file_back() {
    let root = scratch_dir("delete-rollback");
    fs::write(root.join("a.rs"), TWO_FNS).unwrap();
    fs::write(root.join("doomed.txt"), "gone soon\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "a.rs");

    let error = dispatcher(&root)
        .dispatch(
            "multi-op",
            None,
            &json!({"op": "multi-op", "validate": "none", "steps": [
                {"op": "mutate.delete", "path": "doomed.txt"},
                {"op": "mutate.edit", "path": "a.rs", "tag": tag,
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    99"}]},
                {"op": "mutate.edit", "path": "a.rs", "tag": "deadbeefdeadbeef",
                 "edits": [{"replace": {"lines": [2, 2]}, "with": "    99"}]}
            ]}),
        )
        .unwrap_err();
    let __ok0: bool = error.contains("reverted");
    let __ok1: bool = (fs::read_to_string(root.join("doomed.txt")).unwrap()) == ("gone soon\n");
    let __ok2: bool = !fs::read_to_string(root.join("a.rs")).unwrap().contains("99");

    let failed: Vec<&str> = [(__ok0, "rollback message: {error}"), (__ok1, "the deleted file came back"), (__ok2, "the edit rolled back too")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── undo and validation ──────────────────────────────────────────────────────

#[test]
fn undo_restores_the_prior_version_and_the_log_marks_live() {
    let root = scratch_dir("undo");
    fs::write(root.join("code.rs"), TWO_FNS).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let tag = read_tag(&mut patcher, "code.rs");
    let checked = patcher
        .check_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
            {"replace": {"lines": [2, 2]}, "with": "    11"}
        ]}))
        .unwrap();
    let __ok0: bool = checked.sections[0].preview.contains("+    11");
    let __ok1: bool = (fs::read_to_string(root.join("code.rs")).unwrap()) == (TWO_FNS);

    let tag = read_tag(&mut patcher, "code.rs");
    patcher
        .apply_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
            {"replace": {"lines": [2, 2]}, "with": "    11"}
        ]}))
        .unwrap();
    let __ok2: bool = fs::read_to_string(root.join("code.rs")).unwrap().contains("    11");

    let log = patcher.log("code.rs").unwrap();
    let __ok3: bool = log.lines().count() >= 2;
    let __ok4: bool = log.contains("<- live");

    let undone = patcher.undo("code.rs").unwrap();
    let __ok5: bool = undone.contains("restored [code.rs#");
    let __ok6: bool = (fs::read_to_string(root.join("code.rs")).unwrap()) == (TWO_FNS);

    let failed: Vec<&str> = [(__ok0, "checked.sections[0].preview.contains(\"+ 11\")"), (__ok1, "check must not write"), (__ok2, "fs::read_to_string(root.join(\"code.rs\")).unwrap().contains(\" 11\")"), (__ok3, "the journal records both versions: {log}"), (__ok4, "the log marks the live version: {log}"), (__ok5, "{undone}"), (__ok6, "undo restores the prior version")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_failed_validation_rolls_back_and_a_pass_keeps_the_write() {
    let root = scratch_dir("validate");
    fs::write(root.join("g.rs"), TWO_FNS).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let before = fs::read_to_string(root.join("g.rs")).unwrap();
    let tag = read_tag(&mut patcher, "g.rs");

    let reverted = patcher.apply_edits_verified(
        &json!({"path": "g.rs", "tag": tag, "edits": [{"replace": {"lines": [2, 2]}, "with": "    9"}]}),
        Some("exit 3"),
    );
    let __ok0: bool = reverted.is_err();
    let __ok1: bool = reverted.unwrap_err().contains("reverted");
    let __ok2: bool = (fs::read_to_string(root.join("g.rs")).unwrap()) == (before);

    let kept = patcher
        .apply_edits_verified(
            &json!({"path": "g.rs", "tag": tag, "edits": [{"replace": {"lines": [2, 2]}, "with": "    9"}]}),
            Some("true"),
        )
        .unwrap();
    let __ok3: bool = (kept.sections[0].op) == ("update");
    let __ok4: bool = fs::read_to_string(root.join("g.rs")).unwrap().contains("    9");

    let failed: Vec<&str> = [(__ok0, "a failed validate must error: {reverted:?}"), (__ok1, "the error names the revert"), (__ok2, "a failed validate restores the pre-apply file"), (__ok3, "kept.sections[0].op == \"update\""), (__ok4, "a passing validate keeps the write")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

// ── compile: an anchor becomes a hunk ────────────────────────────────────────

fn rust_fixture(_: &str) -> Result<String, String> {
    Ok("fn alpha() -> u8 {\n    1\n}\n\nfn beta() -> u8 {\n    2\n}\n".to_string())
}



#[test]
fn array_of_ops_compiles_to_multiple_sections() {
    let op = json!([
        {"op":"mutate.edit","path":"a.rs","tag":"abc1234","edits":[
            {"insert":{"after":"$"},"body":"fn tail() {}"}]},
        {"op":"mutate.edit","path":"b.rs","tag":"def5678","edits":[
            {"replace":{"lines":[1,2]},"with":"replaced"}]}
    ]);
    let mut reader = |p: &str| rust_fixture(p);
    let patch = compile(&op, &mut reader).expect("compiles");
    let __ok0: bool = (patch.sections.len()) == (2);
    let __ok1: bool = matches!(patch.sections[1].hunks[0].hunk, Hunk::Replace { .. });

    let failed: Vec<&str> = [(__ok0, "patch.sections.len() == 2"), (__ok1, "matches!(patch.sections[1].hunks[0].hunk, Hunk::Replace {{ .. }})")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn mutating_a_new_file_by_symbol_is_refused() {
    let op = json!({"op":"mutate.edit","path":"c.rs","tag":"new","edits":[
        {"replace":{"symbol":"alpha"},"with":"x"}]});
    let mut reader = |p: &str| rust_fixture(p);
    assert!(compile(&op, &mut reader).is_err());
}
