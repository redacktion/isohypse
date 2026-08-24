mod common;

use std::collections::HashMap;
use std::fs;

use common::scratch_dir;
use isohypse::graph::resolve::GraphIndex;

const FIXTURE: &str = r#"trait Speak {
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

fn edge_confidences(index: &GraphIndex, callee_qualified: &str) -> HashMap<String, Vec<String>> {
    let callee = index
        .store
        .symbols_matching(callee_qualified.rsplit("::").next().unwrap(), 32)
        .into_iter()
        .find(|s| s.qualified == callee_qualified && s.kind == "fn")
        .unwrap_or_else(|| panic!("missing symbol {callee_qualified}"));
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

const TS_FIXTURE: &str = r#"class Dog {
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

#[test]
fn graph_typescript_exact_tier() {
    let root = scratch_dir("typescript");
    fs::write(root.join("zoo.ts"), TS_FIXTURE).unwrap();
    let mut index = GraphIndex::open(&root);
    index.full_index();

    let find = |qualified: &str| {
        index
            .store
            .symbols_matching(qualified.rsplit('.').next().unwrap(), 32)
            .into_iter()
            .find(|s| s.qualified == qualified && s.kind == "fn")
            .unwrap_or_else(|| panic!("missing {qualified}"))
    };
    let conf = |callee: &str| {
        let id = find(callee).id;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for edge in index.store.edges_into(&id) {
            if edge.kind == "call" {
                let caller = index.store.symbol_by_id(&edge.src).unwrap();
                out.entry(caller.qualified).or_default().push(edge.confidence);
            }
        }
        out
    };

    let bark = conf("bark");
    assert_eq!(bark.get("Dog.speak"), Some(&vec!["direct".to_string()]), "{bark:?}");
    assert_eq!(bark.get("free"), Some(&vec!["direct".to_string()]), "{bark:?}");

    let dog_speak = conf("Dog.speak");
    assert_eq!(dog_speak.get("pet"), Some(&vec!["typed".to_string()]), "typed receiver must bind Dog.speak: {dog_speak:?}");

    let callers = index.callers_of(&find("Dog.speak").id);
    assert_eq!(callers.confirmed.len(), 1, "exact-tier callers: {:?}", callers.confirmed);
}

#[test]
fn graph_dispatch_and_determinism() {
    let root = scratch_dir("tiers");
    fs::write(root.join("zoo.rs"), FIXTURE).unwrap();
    let mut index = GraphIndex::open(&root);
    index.full_index();

    let bark_edges = edge_confidences(&index, "bark");
    assert_eq!(bark_edges.get("Dog::speak"), Some(&vec!["direct".to_string()]));
    assert_eq!(bark_edges.get("free"), Some(&vec!["direct".to_string()]));

    let dog_speak = edge_confidences(&index, "Dog::speak");
    assert_eq!(dog_speak.get("pet"), Some(&vec!["typed".to_string()]), "typed receiver must bind to Dog::speak: {dog_speak:?}");
    assert_eq!(dog_speak.get("hear"), Some(&vec!["dyn".to_string()]), "dyn receiver must fan out to Dog::speak: {dog_speak:?}");

    let cat_speak = edge_confidences(&index, "Cat::speak");
    assert_eq!(cat_speak.get("hear"), Some(&vec!["dyn".to_string()]), "dyn fan-out must reach every implementor: {cat_speak:?}");
    assert!(!cat_speak.contains_key("pet"), "a typed receiver must not fan out: {cat_speak:?}");

    let robot_speak = edge_confidences(&index, "Robot::speak");
    assert!(!robot_speak.contains_key("hear"), "Robot never impls Speak; dyn fan-out must not reach it: {robot_speak:?}");

    let trait_speak = edge_confidences(&index, "Speak::speak");
    assert_eq!(trait_speak.get("hear"), Some(&vec!["dyn".to_string()]));

    let dog_callers = index.callers_of(
        &index
            .store
            .symbols_matching("speak", 32)
            .into_iter()
            .find(|s| s.qualified == "Dog::speak")
            .unwrap()
            .id,
    );
    assert_eq!(dog_callers.confirmed.len(), 1);
    assert_eq!(dog_callers.dynamic.len(), 1);
    assert_eq!(dog_callers.dynamic[0].1, "Speak");
    let root = scratch_dir("determinism");
    fs::write(root.join("zoo.rs"), FIXTURE).unwrap();
    fs::write(root.join("notes.md"), "# Top\n\n## Nested\nbody\n").unwrap();
    fs::write(root.join("kernel.metal"), "float scale(float x) { return x * 2.0f; }\n").unwrap();
    fs::write(root.join("legacy.py"), "class Oracle:\n    def ask(self):\n        return probe()\n\ndef probe():\n    return 1\n").unwrap();

    let mut first = GraphIndex::open(&root);
    first.full_index();
    let mut second = GraphIndex::open(&root);
    second.full_index();
    let first_dump = first.store.dump();
    assert!(!first_dump.is_empty());
    assert!(first_dump.contains("Oracle.ask"));
    assert!(first_dump.contains("scale"));
    assert!(first_dump.contains("notes.md#Nested"));
    assert_eq!(first_dump, second.store.dump());
}
