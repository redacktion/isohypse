mod common;

use std::fs;

use common::scratch_dir;
use isohypse::patcher::Patcher;
use isohypse::tag::display_tag;
use serde_json::json;

fn read_tag(patcher: &mut Patcher, path: &str) -> String {
    let output = patcher.read(path, None, None).unwrap();
    let header = output.lines().next().unwrap();
    header.rsplit('#').next().unwrap().trim_end_matches(']').to_string()
}

#[test]
fn anchored_edits_end_to_end() {
    let root = scratch_dir("examples");
    let greet = "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)\ngreet(\"world\")\n";
    fs::write(root.join("greet.py"), greet).unwrap();
    fs::write(root.join("other.py"), "print(\"other\")\n").unwrap();
    fs::write(root.join("PLAN.md"), "# Plan\n- first\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();

    let tag = read_tag(&mut patcher, "greet.py");
    let report = patcher
        .apply_edits(&json!({"path": "greet.py", "tag": tag, "edits": [
            {"replace": {"lines": [1, 3]}, "with": "def greet(name):\n    print(f\"Hi, {name}\")"},
            {"move": {"to": "lib/greet.py"}}
        ]}))
        .unwrap();
    assert_eq!(report.sections[0].op, "move");
    assert!(!root.join("greet.py").exists());
    assert_eq!(
        fs::read_to_string(root.join("lib/greet.py")).unwrap(),
        "def greet(name):\n    print(f\"Hi, {name}\")\ngreet(\"world\")\n"
    );

    let plan_tag = read_tag(&mut patcher, "PLAN.md");
    patcher
        .apply_edits(&json!({"path": "PLAN.md", "tag": plan_tag, "edits": [
            {"insert": {"after": 2}, "body": "- task\n  - nested task"}
        ]}))
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("PLAN.md")).unwrap(),
        "# Plan\n- first\n- task\n  - nested task\n"
    );

    fs::write(root.join("lib/greet.py"), greet).unwrap();
    let source_tag = read_tag(&mut patcher, "lib/greet.py");
    let other_tag = read_tag(&mut patcher, "other.py");
    let report = patcher
        .apply_edits(&json!([
            {"path": "lib/greet.py", "tag": source_tag, "edits": [{"delete": {"symbol": "greet"}}]},
            {"path": "other.py", "tag": other_tag, "edits": [
                {"insert": {"before": "^"}, "body": "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)"}
            ]}
        ]))
        .unwrap();
    assert_eq!(report.sections[0].block_resolutions[0].end, 3);
    assert_eq!(fs::read_to_string(root.join("lib/greet.py")).unwrap(), "greet(\"world\")\n");
    assert_eq!(
        fs::read_to_string(root.join("other.py")).unwrap(),
        "def greet(name):\n    msg = \"Hello, \" + name\n    print(msg)\nprint(\"other\")\n"
    );

    fs::write(root.join("block.py"), greet).unwrap();
    let block_tag = read_tag(&mut patcher, "block.py");
    patcher
        .apply_edits(&json!({"path": "block.py", "tag": block_tag, "edits": [
            {"replace": {"symbol": "greet"}, "with": "def greet(name):\n    print(f\"Hello, {name}\")"}
        ]}))
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("block.py")).unwrap(),
        "def greet(name):\n    print(f\"Hello, {name}\")\ngreet(\"world\")\n"
    );

    fs::write(root.join("svc.py"), "@cache\ndef load(key):\n    return db[key]\n").unwrap();
    let svc_tag = read_tag(&mut patcher, "svc.py");
    patcher
        .apply_edits(&json!({"path": "svc.py", "tag": svc_tag, "edits": [
            {"replace": {"block": 1}, "with": "@cache\ndef load(key):\n    return store[key]"}
        ]}))
        .unwrap();
    assert_eq!(
        fs::read_to_string(root.join("svc.py")).unwrap(),
        "@cache\ndef load(key):\n    return store[key]\n"
    );
    let root = scratch_dir("anti");
    fs::write(root.join("a.py"), "one\ntwo\nthree\nfour\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "a.py");

    let unknown_action = patcher.apply_edits(&json!({"path": "a.py", "tag": tag, "edits": [{"frobnicate": true}]}));
    assert!(unknown_action.is_err(), "unknown actions must be rejected: {unknown_action:?}");

    let bad_anchor = patcher.apply_edits(&json!({"path": "a.py", "tag": tag, "edits": [
        {"replace": {"lines": [0, 2]}, "with": "x"}
    ]}));
    assert!(bad_anchor.is_err(), "zero line anchors must be rejected: {bad_anchor:?}");

    let missing_tag = patcher.apply_edits(&json!({"path": "a.py", "edits": [
        {"replace": {"lines": [1, 1]}, "with": "x"}
    ]}));
    assert!(missing_tag.is_err(), "missing tag must be rejected: {missing_tag:?}");

    let missing_symbol = patcher.apply_edits(&json!({"path": "a.py", "tag": tag, "edits": [
        {"delete": {"symbol": "nope"}}
    ]}));
    assert!(missing_symbol.is_err(), "unknown symbols must be rejected: {missing_symbol:?}");

    let overlap = patcher.apply_edits(&json!({"path": "a.py", "tag": tag, "edits": [
        {"replace": {"lines": [1, 2]}, "with": "x"},
        {"replace": {"lines": [2, 3]}, "with": "y"}
    ]}));
    assert!(overlap.is_err(), "overlapping ranges must be rejected: {overlap:?}");

    let remove_with_edits = patcher.apply_edits(&json!({"path": "a.py", "tag": tag, "edits": [
        {"delete": {"lines": [1, 1]}},
        {"remove": true}
    ]}));
    assert!(remove_with_edits.is_err(), "remove must not combine with other edits: {remove_with_edits:?}");
    let root = scratch_dir("stale");
    fs::write(root.join("s.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "s.txt");

    let unknown = patcher.apply_edits(&json!({"path": "s.txt", "tag": "feedfacefeedface", "edits": [
        {"replace": {"lines": [1, 1]}, "with": "ALPHA"}
    ]}));
    assert!(unknown.is_err(), "an unstored tag must refuse: {unknown:?}");

    fs::write(root.join("s.txt"), "alpha\nbeta\ngamma\ndelta\nepsilon\n").unwrap();
    let report = patcher.apply_edits(&json!({"path": "s.txt", "tag": tag, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "BETA"}
    ]})).unwrap();
    assert!(report.sections[0].recovered);
    assert_eq!(
        fs::read_to_string(root.join("s.txt")).unwrap(),
        "alpha\nBETA\ngamma\ndelta\nepsilon\n"
    );

    let stale_conflict = read_tag(&mut patcher, "s.txt");
    fs::write(root.join("s.txt"), "alpha\nCHANGED\ngamma\ndelta\nepsilon\n").unwrap();
    let conflict = patcher.apply_edits(&json!({"path": "s.txt", "tag": stale_conflict, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "REWRITTEN"}
    ]}));
    assert!(conflict.is_err(), "a diverged edit region must refuse: {conflict:?}");
    let root = scratch_dir("tags");
    fs::write(root.join("t.txt"), "one\ntwo\nthree\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let tag = read_tag(&mut patcher, "t.txt");

    let report = patcher.apply_edits(&json!({"path": "t.txt", "tag": tag, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "TWO"}
    ]})).unwrap();
    let new_tag = report.sections[0].new_tag.clone().unwrap();
    let on_disk = fs::read_to_string(root.join("t.txt")).unwrap();
    assert_eq!(new_tag, display_tag(&on_disk));
    assert_eq!(read_tag(&mut patcher, "t.txt"), new_tag);

    let followup = patcher.apply_edits(&json!({"path": "t.txt", "tag": new_tag, "edits": [
        {"insert": {"after": "$"}, "body": "four"}
    ]})).unwrap();
    assert_eq!(fs::read_to_string(root.join("t.txt")).unwrap(), "one\nTWO\nthree\nfour\n");
    assert!(followup.sections[0].preview.contains("+four"));

    fs::write(root.join("window.txt"), "w1\nw2\nw3\nw4\nw5\nw6\n").unwrap();
    let window_read = patcher.read("window.txt", Some(1), Some(3)).unwrap();
    let window_tag = window_read.lines().next().unwrap().rsplit('#').next().unwrap().trim_end_matches(']').to_string();
    let outside = patcher.apply_edits(&json!({"path": "window.txt", "tag": window_tag, "edits": [
        {"replace": {"lines": [5, 5]}, "with": "W5"}
    ]}));
    assert!(outside.is_err(), "edits outside the read window must refuse: {outside:?}");
}
