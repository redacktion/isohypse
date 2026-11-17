//! Workspace enforcement wired through the daemon. The policy and state-machine
//! rules are covered by tests/independent_enforcement.rs.

mod support;

use std::fs;

use isohypse::daemon::load_enforce;
use isohypse::policy::{EnforceRequest, Enforcement, Policy};
use isohypse::serve::Dispatcher;
use serde_json::json;
use support::{dispatcher, scratch_dir, workspace};

fn builtin(name: &str) -> Policy {
    Policy::builtin(name).unwrap()
}

// ── workspace integration ──────────────────────────────────────────────────

#[test]
fn workspace_multi_op_read_step_passes_under_readonly() {
    let root = scratch_dir("enforce-multiop-read");
    fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    let d = dispatcher(&root);
    d.context.set_enforce(EnforceRequest::Activate(builtin("readonly"))).unwrap();
    let result = d.dispatch("multi-op", None, &json!({"op": "multi-op", "steps": [{"op": "context.read", "arg": "a.rs"}]}));
    assert!(result.is_ok());
}

#[test]
fn workspace_multi_op_mutate_step_is_refused_under_readonly() {
    let root = scratch_dir("enforce-multiop-write");
    fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
    let d = dispatcher(&root);
    d.context.set_enforce(EnforceRequest::Activate(builtin("readonly"))).unwrap();
    let result = d.dispatch("multi-op", None, &json!({"op": "multi-op", "validate": "none", "steps": [
        {"op": "context.read", "arg": "a.rs"},
        {"op": "mutate.create", "path": "b.rs", "content": "fn b() {}\n"}
    ]}));
    assert!(result.is_err());
}

#[test]
fn workspace_refused_multi_op_writes_nothing() {
    let root = scratch_dir("enforce-multiop-nowrite");
    let d = dispatcher(&root);
    d.context.set_enforce(EnforceRequest::Activate(builtin("readonly"))).unwrap();
    let _ = d.dispatch("multi-op", None, &json!({"op": "multi-op", "validate": "none", "steps": [{"op": "mutate.create", "path": "b.rs", "content": "x\n"}]}));
    assert!(!root.join("b.rs").exists());
}

#[test]
fn workspace_activation_persists_and_reloads() {
    let root = scratch_dir("enforce-persist");
    workspace(&root).set_enforce(EnforceRequest::Activate(builtin("readonly"))).unwrap();
    assert_eq!(load_enforce(&root), Enforcement::Active(builtin("readonly")));
}

#[test]
fn workspace_without_a_stored_file_loads_off() {
    let root = scratch_dir("enforce-absent");
    assert_eq!(load_enforce(&root), Enforcement::Off);
}

#[test]
fn clearing_a_directory_removes_its_policy_files() {
    let dir = scratch_dir("enforce-clear-dir");
    fs::write(dir.join("a.json"), "{}").unwrap();
    fs::write(dir.join("keep.txt"), "x").unwrap();
    isohypse::daemon::clear_enforcement_in(&dir);
    let mut left: Vec<String> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    left.sort();
    assert_eq!(left, vec!["keep.txt".to_string()]);
}

#[test]
fn workspace_disable_is_refused_after_activation() {
    let root = scratch_dir("enforce-nodisable");
    let ws = workspace(&root);
    ws.set_enforce(EnforceRequest::Activate(builtin("readonly"))).unwrap();
    assert!(ws.set_enforce(EnforceRequest::Disable).is_err());
}

#[test]
fn workspace_refused_change_is_not_persisted() {
    let root = scratch_dir("enforce-refused-persist");
    let ws = workspace(&root);
    ws.set_enforce(EnforceRequest::Activate(builtin("readonly"))).unwrap();
    let _ = ws.set_enforce(EnforceRequest::Activate(builtin("edit")));
    assert_eq!(load_enforce(&root), Enforcement::Active(builtin("readonly")));
}
