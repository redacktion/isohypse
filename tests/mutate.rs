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
    assert_eq!(report.sections[0].op, "move");
    assert!(!root.join("greet.py").exists(), "the source is gone after a move");
    assert_eq!(
        fs::read_to_string(root.join("lib/greet.py")).unwrap(),
        "def greet(name):\n    print(f\"Hi, {name}\")\ngreet(\"world\")\n"
    );
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
    assert_eq!(fs::read_to_string(root.join("PLAN.md")).unwrap(), "# Plan\n- first\n- task\n  - nested task\n");

    let other_tag = read_tag(&mut patcher, "other.py");
    patcher
        .apply_edits(&json!({"path": "other.py", "tag": other_tag, "edits": [
            {"insert": {"before": "^"}, "body": "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)"}
        ]}))
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("other.py")).unwrap(),
        "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)\nprint(\"other\")\n"
    );
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
    assert_eq!(fs::read_to_string(root.join("block.py")).unwrap(), "def greet(name):\n    print(f\"Hello, {name}\")\ngreet(\"world\")\n");

    fs::write(root.join("svc.py"), "@cache\ndef load(key):\n    return db[key]\n").unwrap();
    let tag = read_tag(&mut patcher, "svc.py");
    patcher
        .apply_edits(&json!({"path": "svc.py", "tag": tag, "edits": [
            {"replace": {"block": 1}, "with": "@cache\ndef load(key):\n    return store[key]"}
        ]}))
        .unwrap();
    assert_eq!(fs::read_to_string(root.join("svc.py")).unwrap(), "@cache\ndef load(key):\n    return store[key]\n");
}

#[test]
fn an_anchor_that_cannot_resolve_is_refused() {
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
        assert!(outcome.is_err(), "{label} must be rejected: {outcome:?}");
    }
    assert_eq!(fs::read_to_string(root.join("a.py")).unwrap(), "one\ntwo\nthree\nfour\n", "a refused edit writes nothing");
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
    assert!(unknown.is_err(), "an unstored tag must refuse: {unknown:?}");

    fs::write(root.join("s.txt"), "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    let _ = read_tag(&mut patcher, "s.txt"); // record the grown file; the cited tag stays the old one
    let report = patcher
        .apply_edits(&json!({"path": "s.txt", "tag": tag, "edits": [
            {"replace": {"lines": [2, 2]}, "with": "BETA"}
        ]}))
        .unwrap();
    assert!(report.sections[0].recovered, "an appended tail is recoverable");
    assert_eq!(fs::read_to_string(root.join("s.txt")).unwrap(), "alpha\nBETA\ngamma\ndelta\nepsilon\n");

    let stale = read_tag(&mut patcher, "s.txt");
    fs::write(root.join("s.txt"), "alpha\nCHANGED\ngamma\ndelta\nepsilon\n").unwrap();
    let conflict = patcher.apply_edits(&json!({"path": "s.txt", "tag": stale, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "REWRITTEN"}
    ]}));
    assert!(conflict.is_err(), "a diverged edit region must refuse: {conflict:?}");
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
    assert_eq!(new_tag, display_tag(&fs::read_to_string(root.join("t.txt")).unwrap()));
    assert_eq!(read_tag(&mut patcher, "t.txt"), new_tag);

    let followup = patcher
        .apply_edits(&json!({"path": "t.txt", "tag": new_tag, "edits": [{"insert": {"after": "$"}, "body": "four"}]}))
        .unwrap();
    assert_eq!(fs::read_to_string(root.join("t.txt")).unwrap(), "one\nTWO\nthree\nfour\n");
    assert!(followup.sections[0].preview.contains("+four"));
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
    assert!(breaks.is_err(), "an edit that breaks parsing must refuse: {breaks:?}");
    let err = breaks.unwrap_err();
    assert!(err.contains("would not parse"), "the refusal names the guard: {err}");
    assert!(fs::read_to_string(root.join("g.rs")).unwrap().contains("fn beta"), "nothing was written");
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
    assert!(structural.contains("fn added: gamma"), "{structural}");
    assert!(structural.contains("fn removed: beta"), "{structural}");
    assert!(!structural.contains("alpha"), "untouched functions stay out of the summary: {structural}");
}

// ── authoring ────────────────────────────────────────────────────────────────

#[test]
fn create_authors_a_new_file_and_normalises_bom_and_line_endings() {
    let root = scratch_dir("create");
    let mut patcher = Patcher::new(&root).unwrap();

    let summary = patcher.create("c.txt", "\u{feff}created\r\n").unwrap();
    assert!(summary.contains("created [c.txt#"), "{summary}");
    assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), "created\n", "BOM dropped, CRLF normalised, trailing newline kept");
}

#[test]
fn create_refuses_a_file_that_already_exists() {
    let (root, mut patcher) = playground("create-existing");
    let error = patcher.create("greet.py", "replacement").unwrap_err();
    assert!(error.contains("already exists"), "{error}");
    assert!(error.contains("mutate.edit"), "the refusal names the way to change it: {error}");
    assert_eq!(fs::read_to_string(root.join("greet.py")).unwrap(), GREET, "nothing was overwritten");
}

// ── deletion ─────────────────────────────────────────────────────────────────

#[test]
fn delete_removes_a_file() {
    let root = scratch_dir("delete");
    fs::write(root.join("doomed.txt"), "gone soon\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let summary = patcher.delete("doomed.txt").unwrap();
    assert!(summary.contains("deleted [doomed.txt#"), "{summary}");
    assert!(!root.join("doomed.txt").exists());
}

#[test]
fn delete_refuses_a_missing_file_and_a_directory() {
    let root = scratch_dir("delete-refusals");
    fs::create_dir_all(root.join("dir")).unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let missing = patcher.delete("absent.txt").unwrap_err();
    assert!(missing.contains("does not exist"), "{missing}");
    let dir = patcher.delete("dir").unwrap_err();
    assert!(dir.contains("is a directory"), "{dir}");
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
    assert!(error.contains("reverted"), "rollback message: {error}");
    assert_eq!(fs::read_to_string(root.join("doomed.txt")).unwrap(), "gone soon\n", "the deleted file came back");
    assert!(!fs::read_to_string(root.join("a.rs")).unwrap().contains("99"), "the edit rolled back too");
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
    assert!(checked.sections[0].preview.contains("+    11"));
    assert_eq!(fs::read_to_string(root.join("code.rs")).unwrap(), TWO_FNS, "check must not write");

    let tag = read_tag(&mut patcher, "code.rs");
    patcher
        .apply_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
            {"replace": {"lines": [2, 2]}, "with": "    11"}
        ]}))
        .unwrap();
    assert!(fs::read_to_string(root.join("code.rs")).unwrap().contains("    11"));

    let log = patcher.log("code.rs").unwrap();
    assert!(log.lines().count() >= 2, "the journal records both versions: {log}");
    assert!(log.contains("<- live"), "the log marks the live version: {log}");

    let undone = patcher.undo("code.rs").unwrap();
    assert!(undone.contains("restored [code.rs#"), "{undone}");
    assert_eq!(fs::read_to_string(root.join("code.rs")).unwrap(), TWO_FNS, "undo restores the prior version");
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
    assert!(reverted.is_err(), "a failed validate must error: {reverted:?}");
    assert!(reverted.unwrap_err().contains("reverted"), "the error names the revert");
    assert_eq!(fs::read_to_string(root.join("g.rs")).unwrap(), before, "a failed validate restores the pre-apply file");

    let kept = patcher
        .apply_edits_verified(
            &json!({"path": "g.rs", "tag": tag, "edits": [{"replace": {"lines": [2, 2]}, "with": "    9"}]}),
            Some("true"),
        )
        .unwrap();
    assert_eq!(kept.sections[0].op, "update");
    assert!(fs::read_to_string(root.join("g.rs")).unwrap().contains("    9"), "a passing validate keeps the write");
}

// ── compile: an anchor becomes a hunk ────────────────────────────────────────

fn rust_fixture(_: &str) -> Result<String, String> {
    Ok("fn alpha() -> u8 {\n    1\n}\n\nfn beta() -> u8 {\n    2\n}\n".to_string())
}

#[test]
fn symbol_replace_compiles_to_a_block_hunk() {
    let op = json!({"op":"mutate.edit","path":"a.rs","tag":"abc1234","edits":[
        {"replace":{"symbol":"beta"},"with":"fn beta() -> u8 {\n    22\n}"}
    ]});
    let mut reader = |p: &str| rust_fixture(p);
    let patch = compile(&op, &mut reader).expect("compiles");
    assert_eq!(patch.sections.len(), 1);
    match &patch.sections[0].hunks[0].hunk {
        Hunk::ReplaceBlock { anchor, body } => {
            assert_eq!(*anchor, 5);
            assert_eq!(body.len(), 3);
        }
        other => panic!("expected ReplaceBlock, got {other:?}"),
    }
}

#[test]
fn unknown_symbol_is_refused_with_the_path() {
    let op = json!({"op":"mutate.edit","path":"a.rs","tag":"abc1234","edits":[
        {"delete":{"symbol":"gamma"}}
    ]});
    let mut reader = |p: &str| rust_fixture(p);
    let error = compile(&op, &mut reader).expect_err("refused");
    assert!(error.contains("gamma") && error.contains("a.rs"), "{error}");
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
    assert_eq!(patch.sections.len(), 2);
    assert!(matches!(patch.sections[1].hunks[0].hunk, Hunk::Replace { .. }));
}

#[test]
fn mutating_a_new_file_by_symbol_is_refused() {
    let op = json!({"op":"mutate.edit","path":"c.rs","tag":"new","edits":[
        {"replace":{"symbol":"alpha"},"with":"x"}]});
    let mut reader = |p: &str| rust_fixture(p);
    assert!(compile(&op, &mut reader).is_err());
}
