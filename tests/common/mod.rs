use std::fs;
use std::path::PathBuf;
use std::sync::Once;

static STORE: Once = Once::new();

pub fn scratch_dir(name: &str) -> PathBuf {
    STORE.call_once(|| {
        let store = std::env::temp_dir().join(format!("isohypse-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&store);
        unsafe { std::env::set_var("ISOHYPSE_STORE", &store) };
    });
    let dir = std::env::temp_dir().join(format!("isohypse-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}
