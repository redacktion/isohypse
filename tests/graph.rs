//! graph.* — indexing and call resolution.
mod support;

use std::collections::HashMap;
use std::fs;

use isohypse::graph::resolve::GraphIndex;
use isohypse::serve::Dispatcher;
use support::scratch_dir;

const RUST_ZOO: &str = r#"trait Speak {
    fn speak(&self) -> String;
}

struct Dog;
struct Cat;
struct Robot;

impl Speak for Dog {
    fn speak(&self) -> String {
        bark()
    }
}

impl Speak for Cat {
    fn speak(&self) -> String {
        String::from("meow")
    }
}

impl Robot {
    fn speak(&self) -> String {
        String::from("beep")
    }
}

fn bark() -> String {
    String::from("woof")
}

fn hear(animal: &dyn Speak) -> String {
    animal.speak()
}

fn pet() -> String {
    let dog: Dog = Dog;
    dog.speak()
}

fn free() -> String {
    bark()
}
"#;

const TS_ZOO: &str = r#"class Dog {
    speak(): string {
        return bark();
    }
}

function bark(): string {
    return "woof";
}

function pet(dog: Dog): string {
    return dog.speak();
}

function free(): string {
    return bark();
}
"#;

/// Every call edge into a qualified symbol, grouped by caller, with confidence.
fn edges_into(index: &GraphIndex, qualified: &str, sep: &str) -> HashMap<String, Vec<String>> {
    let short = qualified.rsplit(sep).next().unwrap();
    let callee = index
        .store
        .symbols_matching(short, 32)
        .into_iter()
        .find(|s| s.qualified == qualified && s.kind == "fn")
        .unwrap_or_else(|| panic!("missing symbol {qualified}"));
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for edge in index.store.edges_into(&callee.id) {
        if edge.kind != "call" {
            continue;
        }
        let caller = index.store.symbol_by_id(&edge.src).unwrap();
        out.entry(caller.qualified).or_default().push(edge.confidence);
    }
    out
}

fn indexed(name: &str, file: &str, source: &str) -> GraphIndex {
    let root = scratch_dir(name);
    fs::write(root.join(file), source).unwrap();
    let mut index = GraphIndex::open(&root);
    index.full_index();
    index
}

// ── resolution tiers ─────────────────────────────────────────────────────────

#[test]
fn a_rust_call_is_direct_typed_or_dyn_by_receiver() {
    let index = indexed("tiers-rust", "zoo.rs", RUST_ZOO);

    assert_eq!(edges_into(&index, "bark", "::").get("Dog::speak"), Some(&vec!["direct".to_string()]));
    assert_eq!(edges_into(&index, "bark", "::").get("free"), Some(&vec!["direct".to_string()]));

    let dog_speak = edges_into(&index, "Dog::speak", "::");
    assert_eq!(dog_speak.get("pet"), Some(&vec!["typed".to_string()]), "a typed receiver binds Dog::speak: {dog_speak:?}");
    assert_eq!(dog_speak.get("hear"), Some(&vec!["dyn".to_string()]), "a dyn receiver fans out to Dog::speak: {dog_speak:?}");

    let cat_speak = edges_into(&index, "Cat::speak", "::");
    assert_eq!(cat_speak.get("hear"), Some(&vec!["dyn".to_string()]), "dyn fan-out reaches every implementor: {cat_speak:?}");
    assert!(!cat_speak.contains_key("pet"), "a typed receiver must not fan out: {cat_speak:?}");

    let robot_speak = edges_into(&index, "Robot::speak", "::");
    assert!(!robot_speak.contains_key("hear"), "Robot never impls Speak, so dyn fan-out must not reach it: {robot_speak:?}");

    assert_eq!(edges_into(&index, "Speak::speak", "::").get("hear"), Some(&vec!["dyn".to_string()]));
}

#[test]
fn callers_split_into_confirmed_and_dynamic() {
    let index = indexed("tiers-callers", "zoo.rs", RUST_ZOO);
    let dog_speak = index
        .store
        .symbols_matching("speak", 32)
        .into_iter()
        .find(|s| s.qualified == "Dog::speak")
        .unwrap();

    let callers = index.callers_of(&dog_speak.id);
    assert_eq!(callers.confirmed.len(), 1, "exact-tier callers: {:?}", callers.confirmed);
    assert_eq!(callers.dynamic.len(), 1, "dynamic callers: {:?}", callers.dynamic);
    assert_eq!(callers.dynamic[0].1, "Speak");
}

#[test]
fn a_typescript_call_resolves_to_the_same_tiers() {
    let index = indexed("tiers-typescript", "zoo.ts", TS_ZOO);

    let bark = edges_into(&index, "bark", ".");
    assert_eq!(bark.get("Dog.speak"), Some(&vec!["direct".to_string()]), "{bark:?}");
    assert_eq!(bark.get("free"), Some(&vec!["direct".to_string()]), "{bark:?}");

    let dog_speak = edges_into(&index, "Dog.speak", ".");
    assert_eq!(dog_speak.get("pet"), Some(&vec!["typed".to_string()]), "a typed receiver must bind Dog.speak: {dog_speak:?}");

    let callee = index
        .store
        .symbols_matching("speak", 32)
        .into_iter()
        .find(|s| s.qualified == "Dog.speak")
        .unwrap();
    assert_eq!(index.callers_of(&callee.id).confirmed.len(), 1);
}

// ── determinism ──────────────────────────────────────────────────────────────

// ── the size cap ─────────────────────────────────────────────────────────────

#[test]
fn a_file_over_the_index_cap_is_counted_and_reported_in_status() {
    let root = scratch_dir("size-cap");
    fs::write(root.join("small.rs"), "fn small() {}\n").unwrap();
    fs::write(root.join("big.rs"), format!("fn big() {{}}\n{}", "// padding\n".repeat(200_000))).unwrap();

    let mut index = GraphIndex::open(&root);
    let summary = index.full_index();
    assert_eq!(summary.oversized, 1, "one file is over the cap");
    assert_eq!(summary.files, 1, "the small file is still indexed");
    assert_eq!(index.oversized_files, 1, "the count stays on the index");

    let (status, _) = support::dispatcher(&root).dispatch("state.status", None, &serde_json::json!({})).unwrap();
    assert!(status.contains("over the"), "status names the cap: {status}");
    assert!(status.contains("1 file(s)"), "status counts what was dropped: {status}");
}

#[test]
fn two_indexes_of_one_tree_dump_identically_across_languages() {
    let root = scratch_dir("determinism");
    fs::write(root.join("zoo.rs"), RUST_ZOO).unwrap();
    fs::write(root.join("notes.md"), "# Top\n\n## Nested\nbody\n").unwrap();
    fs::write(root.join("kernel.metal"), "float scale(float x) { return x * 2.0f; }\n").unwrap();
    fs::write(root.join("legacy.py"), "class Oracle:\n    def ask(self):\n        return probe()\n\ndef probe():\n    return 1\n").unwrap();

    let mut first = GraphIndex::open(&root);
    first.full_index();
    let mut second = GraphIndex::open(&root);
    second.full_index();

    let dump = first.store.dump();
    assert!(!dump.is_empty());
    assert!(dump.contains("Oracle.ask"), "python classes are indexed");
    assert!(dump.contains("scale"), "metal functions are indexed");
    assert!(dump.contains("notes.md#Nested"), "markdown headings are indexed");
    assert_eq!(dump, second.store.dump(), "the dump is deterministic");
}
