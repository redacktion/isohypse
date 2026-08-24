mod common;

use std::fs;
use std::sync::{Arc, Mutex};

use common::scratch_dir;
use isohypse::daemon::{self, Workspace};
use isohypse::graph::resolve::GraphIndex;
use isohypse::opresult::OpResult;
use isohypse::patcher::Patcher;
use serde_json::json;

#[test]
fn socket_round_trip_serves_explore_status_apply_and_path() {
    let root = scratch_dir("daemon");
    fs::write(
        root.join("lib.rs"),
        "fn greet() -> String {\n    String::from(\"hi\")\n}\n\nfn main() {\n    greet();\n}\n",
    )
    .unwrap();

    let mut index = GraphIndex::open(&root);
    index.full_index();
    let context = Arc::new(Workspace {
        refs: None,
        semantic: std::sync::Arc::new(Mutex::new(None)),
        index: Arc::new(Mutex::new(index)),
        apply_queue: Arc::new(Mutex::new(())),
        analysis_cache: isohypse::patcher::SharedAnalysisCache::default(),
        root: root.clone(),
        socket: daemon::socket_path(&root),
        watcher_alive: false,
        semantic_requested: false,
        started: std::time::Instant::now(),
        build: None,
        alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    });

    let explored = daemon::explore(
        &context.index,
        &context.root,
        None,
        None,
        "greet",
        &daemon::ExploreOptions::default(),
    )
    .unwrap()
    .render();
    assert!(explored.contains("[lib.rs#"), "explore shows tagged source: {explored}");
    assert!(explored.contains("1:fn greet() -> String {"), "explore shows numbered lines: {explored}");
    assert!(explored.contains("callers (confirmed): main"), "explore shows callers: {explored}");

    let server_context = Arc::clone(&context);
    let registry = daemon::Registry::new(false, false);
    let server_registry = Arc::clone(&registry);
    let server_root = root.clone();
    let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let socket = daemon::socket_path(&server_root);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        ready_sender.send(()).unwrap();
        for _ in 0..3 {
            let (stream, _) = listener.accept().unwrap();
            daemon::serve_one(stream, &server_context, &server_registry);
        }
    });
    ready_receiver.recv().unwrap();
    let socket = daemon::find_socket(&root.join("nested")).unwrap_or_else(|| daemon::find_socket(&root).unwrap());

    let status = daemon::request(&socket, &json!({"op": "state.status"})).unwrap();
    assert!(status.contains("indexed files: 1"), "{status}");

    let path = daemon::request(&socket, &json!({"op": "context.explore", "arg": "main", "to": "greet"})).unwrap();
    assert!(path.contains("main -> greet") || path.contains("main -(direct) -> greet") || path.contains("main -("), "{path}");

    let mut patcher = Patcher::new(&root).unwrap();
    let read = patcher.read("lib.rs", None, None).unwrap();
    let tag = read.lines().next().unwrap().rsplit('#').next().unwrap().trim_end_matches(']').to_string();
    let applied = daemon::request(
        &socket,
        &json!({"op": "mutate.edit", "path": "lib.rs", "tag": tag, "validate": "none",
                "edits": [{"replace": {"lines": [2, 2]}, "with": "    String::from(\"hello\")"}]}),
    )
    .unwrap();
    assert!(applied.contains("updated [lib.rs#"), "{applied}");
    assert!(fs::read_to_string(root.join("lib.rs")).unwrap().contains("hello"));
    handle.join().unwrap();
}

#[test]
fn explore_options_cap_compact_and_path_filter() {
    let root = scratch_dir("daemon-options");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn alphabet() {}\n").unwrap();
    fs::write(root.join("other.rs"), "fn alpha_other() {}\n").unwrap();
    let mut index = GraphIndex::open(&root);
    index.full_index();
    let index = Arc::new(Mutex::new(index));

    let capped = daemon::explore(
        &index,
        &root,
        None,
        None,
        "alpha",
        &daemon::ExploreOptions { max: 1, ..Default::default() },
    )
    .unwrap()
    .render();
    assert_eq!(capped.matches("## ").count(), 1, "max=1 yields one section: {capped}");

    let compact = daemon::explore(
        &index,
        &root,
        None,
        None,
        "alpha",
        &daemon::ExploreOptions { compact: true, ..Default::default() },
    )
    .unwrap()
    .render();
    assert!(compact.contains("src/a.rs:1 fn alpha"), "compact line: {compact}");
    assert!(!compact.contains("[src/a.rs#"), "compact omits tag header: {compact}");

    let scoped = daemon::explore(
        &index,
        &root,
        None,
        None,
        "alpha",
        &daemon::ExploreOptions { path_prefixes: vec!["src/".into()], ..Default::default() },
    )
    .unwrap()
    .render();
    assert!(scoped.contains("src/a.rs"), "{scoped}");
    assert!(!scoped.contains("other.rs"), "path filter excludes other.rs: {scoped}");
}

#[test]
fn explore_auto_compacts_over_line_budget() {
    let root = scratch_dir("daemon-budget");
    let mut body = String::new();
    for f in 0..4 {
        body.push_str(&format!("fn sprawling{f}() {{\n"));
        for i in 0..120 {
            body.push_str(&format!("    let _v{i} = {i};\n"));
        }
        body.push_str("}\n");
    }
    fs::write(root.join("big.rs"), body).unwrap();
    let mut index = GraphIndex::open(&root);
    index.full_index();
    let index = Arc::new(Mutex::new(index));

    let bounded = daemon::explore(&index, &root, None, None, "sprawling", &daemon::ExploreOptions::default()).unwrap().render();
    assert!(bounded.contains("compact view to bound output"), "auto-compacts by default: {bounded}");
    assert!(!bounded.contains("let _v50 = 50"), "compact omits the body: {bounded}");

    let full = daemon::explore(
        &index,
        &root,
        None,
        None,
        "sprawling",
        &daemon::ExploreOptions { full: true, ..Default::default() },
    )
    .unwrap()
    .render();
    assert!(full.contains("let _v50 = 50"), "--full renders the body: {full}");
}

fn workspace_fixture(root: &std::path::Path) -> Arc<Workspace> {
    let mut index = GraphIndex::open(root);
    index.full_index();
    Arc::new(Workspace {
        refs: None,
        semantic: std::sync::Arc::new(Mutex::new(None)),
        index: Arc::new(Mutex::new(index)),
        apply_queue: Arc::new(Mutex::new(())),
        analysis_cache: isohypse::patcher::SharedAnalysisCache::default(),
        root: root.to_path_buf(),
        socket: daemon::socket_path(root),
        watcher_alive: false,
        semantic_requested: false,
        started: std::time::Instant::now(),
        build: None,
        alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
    })
}

fn serve_n(
    context: Arc<Workspace>,
    registry: Arc<daemon::Registry>,
    root: std::path::PathBuf,
    n: usize,
) -> std::thread::JoinHandle<()> {
    let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let socket = daemon::socket_path(&root);
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        ready_sender.send(()).unwrap();
        for _ in 0..n {
            let (stream, _) = listener.accept().unwrap();
            daemon::serve_one(stream, &context, &registry);
        }
    });
    ready_receiver.recv().unwrap();
    handle
}

fn tag_of(patcher: &mut Patcher, path: &str) -> String {
    let read = patcher.read(path, None, None).unwrap();
    read.lines().next().unwrap().rsplit('#').next().unwrap().trim_end_matches(']').to_string()
}

#[test]
fn multi_op_is_atomic_across_steps() {
    let root = scratch_dir("daemon-multi-op");
    fs::write(root.join("a.rs"), "fn a() -> u8 {\n    1\n}\n").unwrap();
    fs::write(root.join("b.rs"), "fn b() -> u8 {\n    2\n}\n").unwrap();
    let context = workspace_fixture(&root);
    let registry = daemon::Registry::new(false, false);
    let handle = serve_n(Arc::clone(&context), Arc::clone(&registry), root.clone(), 3);
    let socket = daemon::find_socket(&root).unwrap();

    let mut patcher = Patcher::new(&root).unwrap();
    let tag_a = tag_of(&mut patcher, "a.rs");
    let tag_b = tag_of(&mut patcher, "b.rs");
    let ok = daemon::request(
        &socket,
        &json!({"op": "multi-op", "validate": "none", "steps": [
            {"op": "mutate.edit", "path": "a.rs", "tag": tag_a,
             "edits": [{"replace": {"lines": [2, 2]}, "with": "    11"}]},
            {"op": "mutate.edit", "path": "b.rs", "tag": tag_b,
             "edits": [{"replace": {"lines": [2, 2]}, "with": "    22"}]}
        ]}),
    )
    .unwrap();
    assert!(ok.contains("2 step(s), transactional"), "{ok}");
    assert!(fs::read_to_string(root.join("a.rs")).unwrap().contains("11"));
    assert!(fs::read_to_string(root.join("b.rs")).unwrap().contains("22"));

    let tag_a2 = tag_of(&mut patcher, "a.rs");
    let err = daemon::request(
        &socket,
        &json!({"op": "multi-op", "validate": "none", "steps": [
            {"op": "mutate.edit", "path": "a.rs", "tag": tag_a2,
             "edits": [{"replace": {"lines": [2, 2]}, "with": "    99"}]},
            {"op": "mutate.edit", "path": "b.rs", "tag": "deadbeefdeadbeef",
             "edits": [{"replace": {"lines": [2, 2]}, "with": "    99"}]}
        ]}),
    )
    .unwrap_err();
    assert!(err.contains("reverted"), "rollback message: {err}");
    let a_after = fs::read_to_string(root.join("a.rs")).unwrap();
    assert!(a_after.contains("11") && !a_after.contains("99"), "step 1 rolled back: {a_after}");

    let tag_a3 = tag_of(&mut patcher, "a.rs");
    let verr = daemon::request(
        &socket,
        &json!({"op": "multi-op", "validate": "exit 1", "steps": [
            {"op": "mutate.edit", "path": "a.rs", "tag": tag_a3,
             "edits": [{"replace": {"lines": [2, 2]}, "with": "    77"}]}
        ]}),
    )
    .unwrap_err();
    assert!(verr.contains("verify"), "verify failure surfaced: {verr}");
    assert!(!fs::read_to_string(root.join("a.rs")).unwrap().contains("77"), "verify failure rolled back");

    handle.join().unwrap();
}

#[test]
fn session_command_runs_a_real_op_end_to_end() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let root = scratch_dir("daemon-session");
    fs::write(root.join("lib.rs"), "fn a() {}\n").unwrap();
    let context = workspace_fixture(&root);
    let registry = daemon::Registry::new(false, false);
    let dispatcher = std::sync::Arc::new(daemon::SessionDispatcher {
        registry: std::sync::Arc::clone(&registry),
        context: std::sync::Arc::clone(&context),
    });
    let sessions = std::sync::Arc::new(isohypse::session::SessionRegistry::new());

    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async move {
        let path = std::env::temp_dir().join(format!("isohypse-sess-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        tokio::spawn(isohypse::serve::serve(listener, sessions, dispatcher));

        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (read_half, mut write_half) = stream.into_split();
        let mut lines = BufReader::new(read_half).lines();

        write_half.write_all(b"{\"do\":\"session.open\",\"label\":\"t\"}\n").await.unwrap();
        let ready: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ready["ch"], "ready");

        write_half.write_all(b"{\"do\":\"session.command\",\"id\":1,\"op\":\"state.status\"}\n").await.unwrap();
        let ack: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ack["ch"], "ack");
        let result: serde_json::Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(result["ch"], "result", "{result}");
        assert!(result["output"].as_str().unwrap().contains("indexed files"), "{result}");

        let _ = std::fs::remove_file(&path);
    });
}

#[test]
fn store_and_fetch_round_trip_by_tag() {
    let root = scratch_dir("daemon-handles");
    fs::write(root.join("x.rs"), "fn x() {}\n").unwrap();
    let context = workspace_fixture(&root);
    let registry = daemon::Registry::new(false, false);
    let handle = serve_n(Arc::clone(&context), Arc::clone(&registry), root.clone(), 2);
    let socket = daemon::find_socket(&root).unwrap();

    let stored = daemon::request(&socket, &json!({"op": "state.put", "arg": "handle payload"})).unwrap();
    let tag = stored.split('#').nth(1).unwrap().split(']').next().unwrap().to_string();
    let fetched = daemon::request(&socket, &json!({"op": "state.get", "arg": tag})).unwrap();
    assert_eq!(fetched.trim_end(), "handle payload");

    handle.join().unwrap();
}

#[test]
fn diagnose_reports_parse_errors() {
    let root = scratch_dir("daemon-diagnose");
    fs::write(root.join("broken.rs"), "fn a() {\n    let x = \n}\n}\n").unwrap();
    let context = workspace_fixture(&root);
    let registry = daemon::Registry::new(false, false);
    let handle = serve_n(Arc::clone(&context), Arc::clone(&registry), root.clone(), 1);
    let socket = daemon::find_socket(&root).unwrap();

    let out = daemon::request(&socket, &json!({"op": "verify.diagnose", "arg": "broken.rs"})).unwrap();
    assert!(out.contains("broken.rs:"), "parse tier locates the error: {out}");

    handle.join().unwrap();
}

#[test]
fn macro_save_and_run_executes_steps() {
    let root = scratch_dir("daemon-macro");
    fs::write(root.join("m.rs"), "fn m() {}\n").unwrap();
    let context = workspace_fixture(&root);
    let registry = daemon::Registry::new(false, false);
    let handle = serve_n(Arc::clone(&context), Arc::clone(&registry), root.clone(), 2);
    let socket = daemon::find_socket(&root).unwrap();
    let name = format!("test-macro-{}", std::process::id());

    let saved = daemon::request(&socket, &json!({"op": "macro.save", "name": name, "steps": [{"op": "state.status"}]})).unwrap();
    assert!(saved.contains("saved"), "{saved}");
    let ran = daemon::request(&socket, &json!({"op": "macro.run", "name": name})).unwrap();
    assert!(ran.contains("1 step(s)"), "{ran}");

    if let Ok(store) = std::env::var("ISOHYPSE_STORE") {
        let _ = std::fs::remove_file(std::path::PathBuf::from(store).join("macros").join(format!("{name}.json")));
    }
    handle.join().unwrap();
}
