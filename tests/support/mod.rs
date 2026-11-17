//! Shared fixtures for every isohypse suite. A feature file starts with
//! mod support; and uses four entry points: scratch_dir, workspace,
//! dispatcher, and socket_server. Nothing else belongs here.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Once};

use isohypse::daemon::{self, Workspace};
use isohypse::graph::resolve::GraphIndex;
use isohypse::patcher::Patcher;
use serde_json::Value;

static STORE: Once = Once::new();

/// A clean workspace directory. The first call also points ISOHYPSE_STORE at a
/// per-process store, so no suite can read another suite's tags or journal.
pub fn scratch_dir(name: &str) -> PathBuf {
    STORE.call_once(|| {
        let store = std::env::temp_dir().join(format!("isohypse-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&store);
        unsafe { std::env::set_var("ISOHYPSE_STORE", &store) };
    });
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("isohypse-test-{name}-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A workspace with its graph already indexed.
pub fn workspace(root: &Path) -> Arc<Workspace> {
    let mut index = GraphIndex::open(root);
    index.full_index();
    Arc::new(Workspace {
        refs: None,
        semantic: Arc::new(Mutex::new(None)),
        index: Arc::new(Mutex::new(index)),
        apply_queue: Arc::new(Mutex::new(())),
        analysis_cache: isohypse::patcher::SharedAnalysisCache::default(),
        root: root.to_path_buf(),
        socket: daemon::socket_path(root),
        watcher_alive: false,
        semantic_requested: false,
        reindexing: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        started: std::time::Instant::now(),
        build: None,
        alive: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        enforce: Default::default(),
    })
}

/// The dispatcher a session or a socket request runs against.
pub fn dispatcher(root: &Path) -> daemon::SessionDispatcher {
    daemon::SessionDispatcher { context: workspace(root), registry: daemon::Registry::new(false, false) }
}

/// The tag from the first line of a read or an edit report.
pub fn tag_of(output: &str) -> String {
    output.lines().next().unwrap().rsplit('#').next().unwrap().trim_end_matches(']').to_string()
}

/// Read a path and return its tag.
pub fn read_tag(patcher: &mut Patcher, path: &str) -> String {
    let output = patcher.read(path, None, None).unwrap();
    tag_of(&output)
}

/// A daemon serving a bounded number of frames on the workspace socket.
pub fn socket_server(root: &Path, requests: usize) -> std::thread::JoinHandle<()> {
    let context = workspace(root);
    let registry = daemon::Registry::new(false, false);
    let root = root.to_path_buf();
    let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        daemon::ensure_run_dir(&root).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(daemon::socket_path(&root)).unwrap();
        ready_sender.send(()).unwrap();
        for _ in 0..requests {
            let (stream, _) = listener.accept().unwrap();
            daemon::serve_one(stream, &context, &registry);
        }
    });
    ready_receiver.recv().unwrap();
    handle
}

/// One request to a running socket server, returning the rendered output.
pub fn request(root: &Path, payload: &Value) -> String {
    let socket = daemon::find_socket(root).expect("no daemon socket for this workspace");
    daemon::request(&socket, payload).unwrap()
}
