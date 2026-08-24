mod common;

use std::fs;

use common::scratch_dir;
use isohypse::patcher::Patcher;
use serde_json::json;

const LONG_FN: &str = "fn short() -> u32 {\n    1\n}\n\nfn long_body() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n    let e = 5;\n    let f = 6;\n    a + b + c + d + e + f\n}\n";

fn tag_of(output: &str) -> String {
    output.lines().next().unwrap().rsplit('#').next().unwrap().trim_end_matches(']').to_string()
}

#[test]
fn feature_surface_end_to_end() {
    let root = scratch_dir("features");
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

    let full_read = patcher.read("code.rs", None, None).unwrap();
    let tag = tag_of(&full_read);

    let checked = patcher.check_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "    11"}
    ]})).unwrap();
    assert!(checked.sections[0].preview.contains("+    11"));
    assert_eq!(fs::read_to_string(root.join("code.rs")).unwrap(), LONG_FN, "check must not write");

    patcher.apply_edits(&json!({"path": "code.rs", "tag": tag, "edits": [
        {"replace": {"lines": [2, 2]}, "with": "    11"}
    ]})).unwrap();
    assert!(fs::read_to_string(root.join("code.rs")).unwrap().contains("    11"));

    let log = patcher.log("code.rs").unwrap();
    assert!(log.lines().count() >= 2, "journal records both versions: {log}");
    assert!(log.contains("<- live"), "log marks the live version: {log}");

    let undone = patcher.undo("code.rs").unwrap();
    assert!(undone.contains("restored [code.rs#"), "{undone}");
    assert_eq!(fs::read_to_string(root.join("code.rs")).unwrap(), LONG_FN, "undo restores the prior version");
    let root = scratch_dir("guard");
    fs::write(root.join("g.rs"), "fn alpha() -> u32 {\n    1\n}\n\nfn beta() -> u32 {\n    2\n}\n").unwrap();
    let mut patcher = Patcher::new(&root).unwrap();
    let read = patcher.read("g.rs", None, None).unwrap();
    let tag = tag_of(&read);

    let breaks_syntax = patcher.apply_edits(&json!({"path": "g.rs", "tag": tag, "edits": [
        {"replace": {"lines": [3, 3]}, "with": "    1 +"}
    ]}));
    assert!(breaks_syntax.is_err(), "an edit that breaks parsing must refuse: {breaks_syntax:?}");
    assert!(breaks_syntax.unwrap_err().contains("would not parse"), "refusal names the guard");
    assert!(fs::read_to_string(root.join("g.rs")).unwrap().contains("fn beta"), "nothing written");

    let report = patcher
        .apply_edits(&json!({"path": "g.rs", "tag": tag, "edits": [
            {"replace": {"symbol": "beta"}, "with": "fn gamma() -> u32 {\n    3\n}"}
        ]}))
        .unwrap();
    let structural = report.sections[0].structural.join("\n");
    assert!(structural.contains("fn added: gamma"), "{structural}");
    assert!(structural.contains("fn removed: beta"), "{structural}");
    assert!(!structural.contains("alpha"), "untouched functions stay out of the summary: {structural}");

    let before = fs::read_to_string(root.join("g.rs")).unwrap();
    let read = patcher.read("g.rs", None, None).unwrap();
    let tag = tag_of(&read);
    let reverted = patcher.apply_edits_verified(
        &json!({"path": "g.rs", "tag": tag, "edits": [
            {"replace": {"lines": [2, 2]}, "with": "    9"}
        ]}),
        Some("exit 3"),
    );
    assert!(reverted.is_err(), "failed validate must error: {reverted:?}");
    assert!(reverted.unwrap_err().contains("reverted"), "error names the revert");
    assert_eq!(fs::read_to_string(root.join("g.rs")).unwrap(), before, "validate failure restores the pre-apply file");

    let kept = patcher
        .apply_edits_verified(
            &json!({"path": "g.rs", "tag": tag, "edits": [
                {"replace": {"lines": [2, 2]}, "with": "    9"}
            ]}),
            Some("true"),
        )
        .unwrap();
    assert_eq!(kept.sections[0].op, "update");
    assert!(fs::read_to_string(root.join("g.rs")).unwrap().contains("    9"), "passing validate keeps the write");
}
