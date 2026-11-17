//! store.* — the object store and the tags the rest of the tool addresses files by.
mod support;

use isohypse::objects::{ObjectStore, Resolution};
use isohypse::tag::display_tag;
use support::scratch_dir;

#[test]
fn a_tag_resolves_by_full_value_and_by_unique_prefix() {
    let mut failed: Vec<String> = Vec::new();
    scratch_dir("store-resolve");
    let store = ObjectStore::open().unwrap();
    let full = store.put("fn alpha() {}\n").unwrap();

    match store.resolve(&full[..8]) {
        Resolution::Found(found, content) => {
            { let ok: bool = (found) == (full); if !ok { failed.push("a prefix resolves to the full tag".to_string()); } };
            { let ok: bool = (content) == ("fn alpha() {}\n"); if !ok { failed.push("content == \"fn alpha() {{}}\\n\"".to_string()); } };
        }
        _ => panic!("a prefix should resolve to the stored object"),
    }
    { let ok: bool = matches!(store.resolve(&full), Resolution::Found(_, _)); if !ok { failed.push("matches!(store.resolve(&full), Resolution::Found(_, _))".to_string()); } };
    { let ok: bool = full.starts_with(&display_tag("fn alpha() {}\n")); if !ok { failed.push("display_tag is the short form of the stored tag".to_string()); } };
    assert_eq!(failed, Vec::<String>::new(), "these checks failed");
}

#[test]
fn the_journal_appends_once_and_keeps_order() {
    let root = scratch_dir("store-journal");
    let store = ObjectStore::open().unwrap();
    let path = root.join("f.rs").to_string_lossy().into_owned();
    let one = store.put("one\n").unwrap();
    let two = store.put("two\n").unwrap();

    store.journal_record(&path, &one).unwrap();
    store.journal_record(&path, &two).unwrap();
    store.journal_record(&path, &two).unwrap();
    assert_eq!(store.journal_entries(&path), vec![one, two], "the same version is not recorded twice");
}
