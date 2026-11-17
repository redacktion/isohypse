//! store.* — the object store and the tags the rest of the tool addresses files by.
mod support;

use isohypse::objects::{ObjectStore, Resolution};
use isohypse::tag::display_tag;
use support::scratch_dir;

#[test]
fn a_tag_resolves_by_full_value_and_by_unique_prefix() {
    scratch_dir("store-resolve");
    let store = ObjectStore::open().unwrap();
    let full = store.put("fn alpha() {}\n").unwrap();

    match store.resolve(&full[..8]) {
        Resolution::Found(found, content) => {
            assert_eq!(found, full, "a prefix resolves to the full tag");
            assert_eq!(content, "fn alpha() {}\n");
        }
        _ => panic!("a prefix should resolve to the stored object"),
    }
    assert!(matches!(store.resolve(&full), Resolution::Found(_, _)));
    assert!(full.starts_with(&display_tag("fn alpha() {}\n")), "display_tag is the short form of the stored tag");
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
