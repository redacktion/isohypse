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

    let __ok0: bool = (edges_into(&index, "bark", "::").get("Dog::speak")) == (Some(&vec!["direct".to_string()]));
    let __ok1: bool = (edges_into(&index, "bark", "::").get("free")) == (Some(&vec!["direct".to_string()]));

    let dog_speak = edges_into(&index, "Dog::speak", "::");
    let __ok2: bool = (dog_speak.get("pet")) == (Some(&vec!["typed".to_string()]));
    let __ok3: bool = (dog_speak.get("hear")) == (Some(&vec!["dyn".to_string()]));

    let cat_speak = edges_into(&index, "Cat::speak", "::");
    let __ok4: bool = (cat_speak.get("hear")) == (Some(&vec!["dyn".to_string()]));
    let __ok5: bool = !cat_speak.contains_key("pet");

    let robot_speak = edges_into(&index, "Robot::speak", "::");
    let __ok6: bool = !robot_speak.contains_key("hear");

    let __ok7: bool = (edges_into(&index, "Speak::speak", "::").get("hear")) == (Some(&vec!["dyn".to_string()]));

    let failed: Vec<&str> = [(__ok0, "edges_into(&index, \"bark\", \"::\").get(\"Dog::speak\") == Some(&vec![\"dire"), (__ok1, "edges_into(&index, \"bark\", \"::\").get(\"free\") == Some(&vec![\"direct\".to"), (__ok2, "a typed receiver binds Dog::speak: {dog_speak:?}"), (__ok3, "a dyn receiver fans out to Dog::speak: {dog_speak:?}"), (__ok4, "dyn fan-out reaches every implementor: {cat_speak:?}"), (__ok5, "a typed receiver must not fan out: {cat_speak:?}"), (__ok6, "Robot never impls Speak, so dyn fan-out must not reach it: {robot_speak:?}"), (__ok7, "edges_into(&index, \"Speak::speak\", \"::\").get(\"hear\") == Some(&vec![\"dy")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
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
    let __ok0: bool = (callers.confirmed.len()) == (1);
    let __ok1: bool = (callers.dynamic.len()) == (1);
    let __ok2: bool = (callers.dynamic[0].1) == ("Speak");

    let failed: Vec<&str> = [(__ok0, "exact-tier callers: {:?}"), (__ok1, "dynamic callers: {:?}"), (__ok2, "callers.dynamic[0].1 == \"Speak\"")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

#[test]
fn a_typescript_call_resolves_to_the_same_tiers() {
    let index = indexed("tiers-typescript", "zoo.ts", TS_ZOO);

    let bark = edges_into(&index, "bark", ".");
    let __ok0: bool = (bark.get("Dog.speak")) == (Some(&vec!["direct".to_string()]));
    let __ok1: bool = (bark.get("free")) == (Some(&vec!["direct".to_string()]));

    let dog_speak = edges_into(&index, "Dog.speak", ".");
    let __ok2: bool = (dog_speak.get("pet")) == (Some(&vec!["typed".to_string()]));

    let callee = index
        .store
        .symbols_matching("speak", 32)
        .into_iter()
        .find(|s| s.qualified == "Dog.speak")
        .unwrap();
    let __ok3: bool = (index.callers_of(&callee.id).confirmed.len()) == (1);

    let failed: Vec<&str> = [(__ok0, "{bark:?}"), (__ok1, "{bark:?}"), (__ok2, "a typed receiver must bind Dog.speak: {dog_speak:?}"), (__ok3, "index.callers_of(&callee.id).confirmed.len() == 1")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
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
    let __ok0: bool = (summary.oversized) == (1);
    let __ok1: bool = (summary.files) == (1);
    let __ok2: bool = (index.oversized_files) == (1);

    let (status, _) = support::dispatcher(&root).dispatch("state.status", None, &serde_json::json!({})).unwrap();
    let __ok3: bool = status.contains("over the");
    let __ok4: bool = status.contains("1 file(s)");

    let failed: Vec<&str> = [(__ok0, "one file is over the cap"), (__ok1, "the small file is still indexed"), (__ok2, "the count stays on the index"), (__ok3, "status names the cap: {status}"), (__ok4, "status counts what was dropped: {status}")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
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
    let __ok0: bool = !dump.is_empty();
    let __ok1: bool = dump.contains("Oracle.ask");
    let __ok2: bool = dump.contains("scale");
    let __ok3: bool = dump.contains("notes.md#Nested");
    let __ok4: bool = (dump) == (second.store.dump());

    let failed: Vec<&str> = [(__ok0, "!dump.is_empty()"), (__ok1, "python classes are indexed"), (__ok2, "metal functions are indexed"), (__ok3, "markdown headings are indexed"), (__ok4, "the dump is deterministic")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}
