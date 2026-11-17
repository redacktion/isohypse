#[cfg(not(unix))]
compile_error!("isohypse requires a Unix platform: its socket security model depends on Unix domain sockets and peer-uid checks");


pub mod normalize;
pub mod tag;
pub mod objects;
pub mod history;
pub mod patch;
pub mod blocks;
pub mod apply;
pub mod edit;
pub mod recovery;
pub mod preview;
pub mod patcher;
pub mod graph;
pub mod daemon;
pub mod prompt;
pub mod render;
pub mod refs;
pub mod semantic;
pub mod buildspec;
pub mod opresult;
pub mod frame;
pub mod session;
pub mod serve;
pub mod trace;
pub mod lifecycle;
pub mod trust;
pub mod setup;
pub mod machine;
pub mod store;
pub mod jq;
pub mod call;
pub mod hook;
pub mod policy;
pub mod hint;
pub mod output;

/// Unit tests must never touch the operator's real store. A tag miss in
/// `ObjectStore::resolve` falls back to decrypting every stored object, so a
/// real store makes a unit run both slow and destructive. `default_root` calls
/// this, which is the single funnel for the store path.
#[cfg(test)]
pub(crate) mod test_store {
    use std::sync::Once;

    static ONCE: Once = Once::new();

    /// A name fragment unique to one call in one process.
    pub fn unique() -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        format!("{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn isolate() {
        ONCE.call_once(|| {
            let dir = std::env::temp_dir().join(format!("isohypse-unit-store-{}", crate::test_store::unique()));
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::create_dir_all(&dir);
            unsafe { std::env::set_var("ISOHYPSE_STORE", &dir) };
        });
    }
}
