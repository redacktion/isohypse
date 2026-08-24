use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(not(unix))]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::{json, Value};
use model2vec_rs::model::StaticModel;

use crate::graph::extract::SymbolRow;
use crate::graph::resolve::GraphIndex;
use crate::graph::watch;
use crate::opresult::OpResult;
use crate::patcher::Patcher;

#[cfg(unix)]
pub const SOCKET_NAME: &str = ".isohypse.sock";
#[cfg(not(unix))]
pub const SOCKET_NAME: &str = ".isohypse.port";

#[cfg(unix)]
pub type DaemonStream = UnixStream;
#[cfg(not(unix))]
pub type DaemonStream = TcpStream;

#[cfg(unix)]
fn connect_endpoint(marker: &Path) -> std::io::Result<DaemonStream> {
    let stream = UnixStream::connect(marker)?;
    {
        use std::os::unix::io::AsRawFd;
        if !crate::trust::peer_signature_ok(stream.as_raw_fd()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "server failed code-signature verification",
            ));
        }
    }
    Ok(stream)
}

#[cfg(not(unix))]
fn connect_endpoint(marker: &Path) -> std::io::Result<DaemonStream> {
    let port: u16 = std::fs::read_to_string(marker)
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "port file unreadable"))?;
    TcpStream::connect(("127.0.0.1", port))
}
const EXPLORE_SYMBOL_CAP: usize = 8;
const EXPLORE_FILE_CAP: usize = 12;
const EXPLORE_LINE_CAP: usize = 160;
const FULL_LINE_BUDGET: usize = 400;
const BLAST_DEPTH: usize = 3;
const CROSS_LANGUAGE_FLOOR: f32 = 0.65;

static SUPERSEDED: AtomicBool = AtomicBool::new(false);
static HANDOVER_MODE: AtomicBool = AtomicBool::new(false);
static HANDOVER_SIGNAL: std::sync::Once = std::sync::Once::new();

fn staging_socket(canonical: &Path) -> PathBuf {
    PathBuf::from(format!("{}.new", canonical.display()))
}

fn signal_old_handover(canonical: &Path) {
    HANDOVER_SIGNAL.call_once(|| {
        if is_live(canonical) {
            let _ = request(canonical, &json!({"op": "handover"}));
        }
    });
}

pub fn socket_path(root: &Path) -> PathBuf {
    root.join(SOCKET_NAME)
}

pub fn session_socket_path(root: &Path) -> PathBuf {
    root.join(".isohypse.session.sock")
}

pub fn find_socket(start: &Path) -> Option<PathBuf> {
    let mut current = Some(start);
    while let Some(dir) = current {
        let candidate = socket_path(dir);
        if candidate.exists() {
            return Some(candidate);
        }
        current = dir.parent();
    }
    None
}

pub fn is_live(socket: &Path) -> bool {
    connect_endpoint(socket).is_ok()
}

#[cfg(unix)]
static CLEANUP_SOCKET: std::sync::OnceLock<std::ffi::CString> = std::sync::OnceLock::new();

#[cfg(unix)]
extern "C" fn handle_termination(_signal: libc::c_int) {
    if !SUPERSEDED.load(Ordering::SeqCst) {
        if let Some(path) = CLEANUP_SOCKET.get() {
            unsafe {
                libc::unlink(path.as_ptr());
            }
        }
    }
    unsafe {
        libc::_exit(0);
    }
}

#[cfg(unix)]
fn install_signal_cleanup(socket: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(socket.as_os_str().as_bytes()) else { return };
    if CLEANUP_SOCKET.set(path).is_err() {
        return;
    }
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::signal(signal, handle_termination as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(not(unix))]
fn install_signal_cleanup(_socket: &Path) {}

pub static JSON_OUTPUT: AtomicBool = AtomicBool::new(false);

pub fn request(socket: &Path, payload: &Value) -> Result<String, String> {
    let mut stream = {
        let mut attempt: u32 = 0;
        loop {
            match connect_endpoint(socket) {
                Ok(stream) => break stream,
                Err(e) => {
                    attempt += 1;
                    if attempt >= 6 {
                        return Err(format!("no live daemon at {} ({e}); start one with `isohypse daemon`", socket.display()));
                    }
                    let base = 50u64 * (1u64 << attempt.min(5));
                    let jitter = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| (d.subsec_nanos() % 100) as u64)
                        .unwrap_or(0);
                    std::thread::sleep(std::time::Duration::from_millis(base + jitter));
                }
            }
        }
    };
    let mut line = payload.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&response).map_err(|e| format!("malformed daemon response: {e}"))?;
    if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        if JSON_OUTPUT.load(Ordering::Relaxed) {
            Ok(serde_json::to_string_pretty(&value.get("json").cloned().unwrap_or(Value::Null)).unwrap_or_default() + "\n")
        } else {
            Ok(value.get("output").and_then(Value::as_str).unwrap_or_default().to_string())
        }
    } else {
        Err(value.get("error").and_then(Value::as_str).unwrap_or("daemon error").to_string())
    }
}

pub struct Workspace {
    pub refs: Option<crate::refs::PreciseIndex>,
    pub semantic: Arc<Mutex<Option<crate::semantic::SemanticIndex>>>,
    pub index: Arc<Mutex<GraphIndex>>,
    pub apply_queue: Arc<Mutex<()>>,
    pub analysis_cache: crate::patcher::SharedAnalysisCache,
    pub root: PathBuf,
    pub socket: PathBuf,
    pub build: Option<crate::buildspec::BuildSpec>,
    pub watcher_alive: bool,
    pub semantic_requested: bool,
    pub started: std::time::Instant,
    pub alive: Arc<AtomicBool>,
}

#[cfg(unix)]
type DaemonListener = UnixListener;
#[cfg(not(unix))]
type DaemonListener = TcpListener;

fn bind_listener(socket: &Path) -> Result<DaemonListener, String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let listener = UnixListener::bind(socket)
            .map_err(|e| format!("cannot bind {}: {e}", socket.display()))?;
        let _ = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600));
        Ok(listener)
    }
    #[cfg(not(unix))]
    {
        let listener =
            TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("cannot bind loopback: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        std::fs::write(socket, port.to_string())
            .map_err(|e| format!("cannot write {}: {e}", socket.display()))?;
        Ok(listener)
    }
}

const MAX_REQUEST_BYTES: u64 = 8 * 1024 * 1024;

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_uid_ok(fd: std::os::unix::io::RawFd) -> bool {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return false;
    }
    cred.uid == unsafe { libc::geteuid() }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn peer_uid_ok(fd: std::os::unix::io::RawFd) -> bool {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return false;
    }
    uid == unsafe { libc::geteuid() }
}

pub fn peer_ok(fd: std::os::unix::io::RawFd) -> bool {
    peer_uid_ok(fd) && crate::trust::peer_signature_ok(fd)
}

fn verify_account_password(password: &str) -> bool {
    let Ok(user) = std::env::var("USER") else { return false };
    use std::io::Write;
    let Ok(mut child) = std::process::Command::new("/usr/bin/dscl")
        .args([".", "-authonly", &user])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(password.as_bytes());
        let _ = stdin.write_all(b"\n");
    }
    child.wait().map(|status| status.success()).unwrap_or(false)
}

pub fn stop_authorized(payload: &Value) -> Result<(), String> {
    let live = crate::objects::runtime_list()
        .into_iter()
        .filter(|(_, socket)| is_live(socket))
        .count();
    if live == 0 {
        return Ok(());
    }
    let password = payload.get("password").and_then(Value::as_str).unwrap_or_default();
    if password.is_empty() {
        return Err(format!(
            "{live} live workspace(s) in use; stopping requires the operator account password"
        ));
    }
    if verify_account_password(password) {
        Ok(())
    } else {
        Err("authorization failed; the daemon is still running".to_string())
    }
}

pub fn is_mutating_op(op: &str) -> bool {
    op.starts_with("mutate.")
        || op == "multi-op"
        || matches!(
            op,
            "macro.save" | "macro.run" | "state.put" | "state.stop" | "state.reload"
                | "verify.build" | "verify.diagnose" | "session.request"
        )
}

pub struct Registry {
    workspaces: RwLock<BTreeMap<PathBuf, Arc<Workspace>>>,
    lsp: bool,
    semantic: bool,
    sessions: Arc<crate::session::SessionRegistry>,
}

impl Registry {
    pub fn new(lsp: bool, semantic: bool) -> Arc<Registry> {
        Arc::new(Registry {
            workspaces: RwLock::new(BTreeMap::new()),
            lsp,
            semantic,
            sessions: Arc::new(crate::session::SessionRegistry::new()),
        })
    }


    pub fn get(&self, root: &Path) -> Option<Arc<Workspace>> {
        self.workspaces.read().ok()?.get(root).cloned()
    }

    pub fn snapshot(&self) -> Vec<Arc<Workspace>> {
        self.workspaces
            .read()
            .map(|map| map.values().cloned().collect())
            .unwrap_or_default()
    }

    fn insert(&self, workspace: Arc<Workspace>) {
        if let Ok(mut map) = self.workspaces.write() {
            map.insert(workspace.root.clone(), workspace);
        }
    }
}

pub fn build_workspace(registry: &Arc<Registry>, root: PathBuf) -> Result<Arc<Workspace>, String> {
    crate::lifecycle::restore(&root);
    crate::machine::ensure()?;
    crate::objects::migrate_store()?;
    let socket = socket_path(&root);
    let build = crate::buildspec::detect(&root);
    let mut index = GraphIndex::open(&root);
    let summary = index.full_index();
    println!(
        "indexed {} files ({} extracted, {} unreadable), {} symbols",
        summary.files, summary.extracted, summary.skipped, summary.symbols
    );
    let index = Arc::new(Mutex::new(index));
    let watcher = watch::spawn(Arc::clone(&index), root.clone(), Arc::clone(&registry.sessions), root.to_string_lossy().into_owned());
    let watcher_alive = watcher.is_ok();
    if let Ok(active) = watcher {
        std::mem::forget(active);
    }
    let refs = if registry.lsp {
        eprintln!("precise refs: stack-graph overlay building for python/javascript/typescript/java");
        Some(crate::refs::PreciseIndex::spawn(root.clone(), Arc::clone(&registry.sessions), root.to_string_lossy().into_owned()))
    } else {
        None
    };
    let semantic: Arc<Mutex<Option<crate::semantic::SemanticIndex>>> = Arc::new(Mutex::new(None));
    if registry.semantic {
        let slot = Arc::clone(&semantic);
        let index_for_semantic = Arc::clone(&index);
        let root_for_semantic = root.clone();
        std::thread::spawn(move || {
            let symbols = match index_for_semantic.lock() {
                Ok(index) => index.store.all_symbols(),
                Err(_) => return,
            };
            let embedder = std::sync::Arc::new(RemoteEmbedder::new(&root_for_semantic));
            match crate::semantic::SemanticIndex::build(embedder, &symbols) {
                Ok(built) => {
                    eprintln!("semantic: {} symbols embedded", built.len());
                    if let Ok(mut slot) = slot.lock() {
                        *slot = Some(built);
                    }
                }
                Err(e) => eprintln!("semantic: unavailable ({e})"),
            }
        });
    }
    let workspace = Arc::new(Workspace {
        refs,
        semantic,
        index,
        apply_queue: Arc::new(Mutex::new(())),
        analysis_cache: crate::patcher::SharedAnalysisCache::default(),
        root,
        socket,
        build,
        watcher_alive,
        semantic_requested: registry.semantic,
        started: std::time::Instant::now(),
        alive: Arc::new(AtomicBool::new(true)),
    });
    registry.insert(Arc::clone(&workspace));
    spawn_session_listener(registry, &workspace);
    crate::objects::persist_workspace(&workspace.root, registry.lsp, registry.semantic);
    Ok(workspace)
}

fn spawn_session_listener(registry: &Arc<Registry>, workspace: &Arc<Workspace>) {
    let registry = Arc::clone(registry);
    let workspace = Arc::clone(workspace);
    std::thread::spawn(move || {
        let path = session_socket_path(&workspace.root);
        if !HANDOVER_MODE.load(Ordering::SeqCst) {
            let _ = std::fs::remove_file(&path);
        }
        let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(runtime) => runtime,
            Err(error) => {
                eprintln!("session listener: runtime failed: {error}");
                return;
            }
        };
        runtime.block_on(async move {
            let handover = HANDOVER_MODE.load(Ordering::SeqCst);
            let bind_path = if handover { staging_socket(&path) } else { path.clone() };
            let _ = std::fs::remove_file(&bind_path);
            let listener = match tokio::net::UnixListener::bind(&bind_path) {
                Ok(listener) => listener,
                Err(error) => {
                    eprintln!("session listener: bind {} failed: {error}", bind_path.display());
                    return;
                }
            };
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&bind_path, std::fs::Permissions::from_mode(0o600));
            }
            if handover {
                if let Err(error) = std::fs::rename(&bind_path, &path) {
                    eprintln!("session listener: promote {} failed: {error}", path.display());
                    return;
                }
            }
            let dispatcher = Arc::new(SessionDispatcher {
                registry: Arc::clone(&registry),
                context: Arc::clone(&workspace),
            });
            crate::serve::serve(listener, Arc::clone(&registry.sessions), dispatcher).await;
        });
    });
}

fn serve_workspace(registry: Arc<Registry>, workspace: Arc<Workspace>) -> Result<(), String> {
    let listener = if HANDOVER_MODE.load(Ordering::SeqCst) {
        let staging = staging_socket(&workspace.socket);
        let _ = std::fs::remove_file(&staging);
        let bound = bind_listener(&staging)?;
        signal_old_handover(&workspace.socket);
        std::fs::rename(&staging, &workspace.socket)
            .map_err(|e| format!("cannot promote {}: {e}", workspace.socket.display()))?;
        bound
    } else {
        bind_listener(&workspace.socket)?
    };
    println!("serving on {}", workspace.socket.display());
    for stream in listener.incoming() {
        if !workspace.alive.load(Ordering::Relaxed) {
            break;
        }
        let Ok(stream) = stream else { continue };
        if !workspace.alive.load(Ordering::Relaxed) {
            break;
        }
        let registry = Arc::clone(&registry);
        let workspace = Arc::clone(&workspace);
        std::thread::spawn(move || serve_one(stream, &workspace, &registry));
    }
    if !SUPERSEDED.load(Ordering::SeqCst) {
        let _ = std::fs::remove_file(&workspace.socket);
    }
    Ok(())
}

fn supervisor_socket() -> PathBuf {
    let base = std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    });
    let _ = std::fs::create_dir_all(&base);
    base.join("supervisor.sock")
}

pub fn supervisor_reachable() -> Option<PathBuf> {
    let control = supervisor_socket();
    if is_live(&control) {
        Some(control)
    } else {
        None
    }
}

fn services_socket() -> PathBuf {
    let base = std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    });
    let _ = std::fs::create_dir_all(&base);
    base.join("services.sock")
}

pub fn services_reachable() -> Option<PathBuf> {
    let socket = services_socket();
    if is_live(&socket) {
        Some(socket)
    } else {
        None
    }
}

const EMBED_CHUNK: usize = 512;

pub struct RemoteEmbedder {
    socket: PathBuf,
    pack: Option<PathBuf>,
    cache: Mutex<Option<std::collections::HashMap<[u8; 32], Vec<f32>>>>,
}

impl RemoteEmbedder {
    pub fn new(root: &std::path::Path) -> RemoteEmbedder {
        let pack = crate::objects::opaque_name(VECTORS_NAME_CTX, &root.to_string_lossy())
            .map(|name| vectors_dir().join(format!("{name}.pack")));
        RemoteEmbedder { socket: services_socket(), pack, cache: Mutex::new(None) }
    }
}

impl crate::semantic::Embedder for RemoteEmbedder {
    fn encode(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut result: Vec<Vec<f32>> = vec![Vec::new(); texts.len()];
        let mut misses: Vec<usize> = Vec::new();
        let mut cache = self.cache.lock().map_err(|_| "vector cache poisoned".to_string())?;
        if cache.is_none() {
            *cache = Some(self.pack.as_deref().map(load_pack).unwrap_or_default());
        }
        let map = cache.as_mut().ok_or_else(|| "vector cache unavailable".to_string())?;
        let keys: Vec<Option<[u8; 32]>> =
            texts.iter().map(|text| crate::objects::opaque_key(VECTORS_NAME_CTX, text)).collect();
        for (index, key) in keys.iter().enumerate() {
            match key.as_ref().and_then(|k| map.get(k).cloned()) {
                Some(row) => result[index] = row,
                None => misses.push(index),
            }
        }
        if misses.is_empty() {
            return Ok(result);
        }
        let chunks: Vec<&[usize]> = misses.chunks(EMBED_CHUNK).collect();
        let socket = self.socket.clone();
        type EmbeddedChunk = Vec<(usize, Vec<f32>)>;
        let embedded: Vec<Result<EmbeddedChunk, String>> = std::thread::scope(|scope| {
            let handles: Vec<_> = chunks
                .iter()
                .map(|indices| {
                    let socket = socket.clone();
                    scope.spawn(move || {
                        let chunk: Vec<&String> = indices.iter().map(|&i| &texts[i]).collect();
                        let response = request_json(&socket, &json!({"op": "embed", "texts": chunk}))?;
                        let hexes = response
                            .get("vectors_hex")
                            .and_then(Value::as_array)
                            .ok_or_else(|| "embed service returned no vectors".to_string())?;
                        let mut out = Vec::with_capacity(indices.len());
                        for (slot, hex) in indices.iter().zip(hexes) {
                            out.push((*slot, decode_f32_hex(hex.as_str().unwrap_or(""))));
                        }
                        Ok(out)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap_or_else(|_| Err("embed thread panicked".to_string())))
                .collect()
        });
        let mut appended: Vec<u8> = Vec::new();
        for chunk_result in embedded {
            for (index, row) in chunk_result? {
                if !row.is_empty()
                    && row.len() * 4 <= MAX_VECTOR_BYTES
                    && row.iter().all(|value| value.is_finite())
                {
                    if let Some(key) = keys[index] {
                        appended.extend_from_slice(&key);
                        appended.extend_from_slice(&((row.len() * 4) as u32).to_le_bytes());
                        for value in &row {
                            appended.extend_from_slice(&value.to_le_bytes());
                        }
                        map.insert(key, row.clone());
                    }
                }
                result[index] = row;
            }
        }
        if !appended.is_empty() {
            if let Some(path) = self.pack.as_deref() {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Some(sealed) = crate::objects::seal_bytes(VECTORS_SEAL_CTX, &appended) {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let mut framed = Vec::with_capacity(4 + sealed.len());
                    framed.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
                    framed.extend_from_slice(&sealed);
                    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).mode(0o600).open(path) {
                        let _ = file.write_all(&framed);
                    }
                }
            }
        }
        Ok(result)
    }
}

const VECTOR_TTL_SECS: u64 = 48 * 60 * 60;
const VECTOR_SWEEP_SECS: u64 = 60 * 60;
const MAX_VECTOR_BYTES: usize = 64 * 1024;

const VECTORS_NAME_CTX: &str = "isohypse vectors name v1";
const VECTORS_SEAL_CTX: &str = "isohypse vectors seal v1";

fn vectors_root() -> PathBuf {
    let base = std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    });
    base.join("vectors")
}

fn vectors_dir() -> PathBuf {
    let version = blake3::hash(crate::semantic::model_id().as_bytes()).to_hex().to_string();
    vectors_root().join(&version[..8])
}

fn parse_pack_chunk(bytes: &[u8], map: &mut std::collections::HashMap<[u8; 32], Vec<f32>>) {
    let mut at = 0usize;
    while at + 36 <= bytes.len() {
        let mut key = [0u8; 32];
        key.copy_from_slice(&bytes[at..at + 32]);
        let len = u32::from_le_bytes([bytes[at + 32], bytes[at + 33], bytes[at + 34], bytes[at + 35]]) as usize;
        at += 36;
        if len == 0 || len % 4 != 0 || len > MAX_VECTOR_BYTES || at + len > bytes.len() {
            break;
        }
        let row: Vec<f32> = bytes[at..at + len]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        at += len;
        if row.iter().all(|value| value.is_finite()) {
            map.insert(key, row);
        }
    }
}

fn load_pack(path: &std::path::Path) -> std::collections::HashMap<[u8; 32], Vec<f32>> {
    let mut map = std::collections::HashMap::new();
    let Ok(bytes) = std::fs::read(path) else { return map };
    let mut at = 0usize;
    while at + 4 <= bytes.len() {
        let frame_len = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        at += 4;
        let Some(end) = at.checked_add(frame_len) else { break };
        if frame_len == 0 || end > bytes.len() {
            break;
        }
        if let Some(chunk) = crate::objects::open_bytes(VECTORS_SEAL_CTX, &bytes[at..end]) {
            parse_pack_chunk(&chunk, &mut map);
        }
        at = end;
    }
    map
}

fn reap_vectors(ttl_secs: u64) -> usize {
    let now = std::time::SystemTime::now();
    let mut removed = 0;
    let Ok(versions) = std::fs::read_dir(vectors_root()) else {
        return 0;
    };
    for version in versions.flatten() {
        let Ok(entries) = std::fs::read_dir(version.path()) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if std::fs::remove_dir_all(&path).is_ok() {
                    removed += 1;
                }
                continue;
            }
            let stale = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .map(|age| age.as_secs() > ttl_secs)
                .unwrap_or(false);
            if stale && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

fn encode_f32_hex(vector: &[f32]) -> String {
    let mut out = String::with_capacity(vector.len() * 8);
    for value in vector {
        for byte in value.to_le_bytes() {
            out.push_str(&format!("{byte:02x}"));
        }
    }
    out
}

fn decode_f32_hex(text: &str) -> Vec<f32> {
    let bytes: Vec<u8> = (0..text.len() / 2)
        .filter_map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
        .collect();
    bytes
        .chunks_exact(4)
        .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

fn request_json(socket: &Path, payload: &Value) -> Result<Value, String> {
    let mut stream = None;
    for attempt in 0..15u32 {
        match connect_endpoint(socket) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(200 * u64::from(attempt + 1))),
        }
    }
    let mut stream = stream.ok_or_else(|| format!("cannot reach {}", socket.display()))?;
    let mut line = payload.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response).map_err(|e| e.to_string())?;
    let value: Value = serde_json::from_str(&response).map_err(|e| format!("malformed response: {e}"))?;
    if value.get("ok").and_then(Value::as_bool).unwrap_or(false) {
        Ok(value)
    } else {
        Err(value.get("error").and_then(Value::as_str).unwrap_or("service error").to_string())
    }
}

fn scrub_injection_env(command: &mut std::process::Command) {
    for var in [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
    ] {
        command.env_remove(var);
    }
}

fn spawn_services(supervisor: &Arc<Supervisor>) {
    let mut command = std::process::Command::new(&supervisor.exe);
    command.arg("services");
    scrub_injection_env(&mut command);
    command.env_remove("ISOHYPSE_HANDOVER");
    if HANDOVER_MODE.load(Ordering::SeqCst) {
        command.env("ISOHYPSE_HANDOVER", "1");
    }
    if supervisor.semantic {
        command.arg("--semantic");
    }
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            eprintln!("supervisor: cannot spawn services: {error}");
            return;
        }
    };
    let supervisor = Arc::clone(supervisor);
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
        if supervisor.shutting_down.load(Ordering::SeqCst) {
            return;
        }
        eprintln!("supervisor: services exited; restarting");
        std::thread::sleep(std::time::Duration::from_millis(300));
        spawn_services(&supervisor);
    });
}

#[cfg(unix)]
extern "C" fn reap_group(_signal: libc::c_int) {
    if let Some(path) = CLEANUP_SOCKET.get() {
        unsafe {
            libc::unlink(path.as_ptr());
        }
    }
    unsafe {
        libc::killpg(0, libc::SIGKILL);
        libc::_exit(0);
    }
}

#[cfg(unix)]
fn install_supervisor_signals(socket: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(socket.as_os_str().as_bytes()) else {
        return;
    };
    let _ = CLEANUP_SOCKET.set(path);
    for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::signal(signal, reap_group as *const () as libc::sighandler_t);
        }
    }
}

#[cfg(not(unix))]
fn install_supervisor_signals(_socket: &Path) {}

fn signal_old_supervisor_retire() {
    let control = supervisor_socket();
    if is_live(&control) {
        let _ = request(&control, &json!({"op": "retire"}));
    }
}

pub fn run_services(eager: bool) -> Result<(), String> {
    let handover = std::env::var_os("ISOHYPSE_HANDOVER").is_some();
    let socket = services_socket();
    if !handover && is_live(&socket) {
        return Err(format!("a services daemon is already live on {}", socket.display()));
    }
    if !handover {
        let _ = std::fs::remove_file(&socket);
    }
    let model_slot: Arc<Mutex<Option<Arc<StaticModel>>>> = Arc::new(Mutex::new(None));
    if eager {
        let slot = Arc::clone(&model_slot);
        std::thread::spawn(move || {
            let _ = services_model(&slot);
        });
    }
    std::thread::spawn(|| {
        let (converted, saved, reaped) = crate::objects::compact_objects();
        if converted > 0 || reaped > 0 {
            eprintln!(
                "services: compacted {converted} object(s), saved {} KiB, reaped {reaped} dead entr(ies)",
                saved / 1024
            );
        }
        loop {
            let removed = reap_vectors(VECTOR_TTL_SECS);
            if removed > 0 {
                eprintln!("services: reaped {removed} stale vector(s)");
            }
            let (archived, demoted) = crate::lifecycle::sweep(&|root: &std::path::Path| is_live(&socket_path(root)));
            if archived > 0 || demoted > 0 {
                eprintln!("services: lifecycle archived {archived} repo(s), moved {demoted} to cold storage");
            }
            std::thread::sleep(std::time::Duration::from_secs(VECTOR_SWEEP_SECS));
        }
    });
    install_signal_cleanup(&socket);
    spawn_orphan_guard(socket.clone());
    let listener = if handover {
        let staging = staging_socket(&socket);
        let _ = std::fs::remove_file(&staging);
        let bound = bind_listener(&staging)?;
        std::fs::rename(&staging, &socket)
            .map_err(|e| format!("cannot promote services socket: {e}"))?;
        bound
    } else {
        bind_listener(&socket)?
    };
    println!("services on {}", socket.display());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let model_slot = Arc::clone(&model_slot);
        std::thread::spawn(move || serve_services_one(stream, &model_slot));
    }
    Ok(())
}

fn serve_services_one(stream: DaemonStream, model_slot: &Arc<Mutex<Option<Arc<StaticModel>>>>) {
    {
        use std::os::unix::io::AsRawFd;
        if !peer_ok(stream.as_raw_fd()) {
            return;
        }
    }
    use std::io::Read;
    let mut reader = BufReader::new((&stream).take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<Value>(&line) {
        Ok(payload) => {
            let op = payload.get("op").and_then(Value::as_str).unwrap_or_default();
            match op {
                "embed" => {
                    let texts: Vec<String> = payload
                        .get("texts")
                        .and_then(Value::as_array)
                        .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    match services_model(model_slot) {
                        Ok(model) => {
                            let vectors_hex: Vec<String> =
                                model.encode(&texts).iter().map(|row| encode_f32_hex(row)).collect();
                            json!({"ok": true, "vectors_hex": vectors_hex})
                        }
                        Err(error) => json!({"ok": false, "error": error}),
                    }
                }
                "fanout" => fanout_workers(&payload),
                "status" => json!({"ok": true, "output": "services: model loaded\n"}),
                other => json!({"ok": false, "error": format!("unknown services op {other}")}),
            }
        }
        Err(e) => json!({"ok": false, "error": format!("malformed request: {e}")}),
    };
    let mut out = response.to_string();
    out.push('\n');
    let _ = (&stream).write_all(out.as_bytes());
}

fn services_model(slot: &Arc<Mutex<Option<Arc<StaticModel>>>>) -> Result<Arc<StaticModel>, String> {
    let mut guard = slot.lock().map_err(|_| "model lock poisoned".to_string())?;
    if let Some(model) = guard.as_ref() {
        return Ok(Arc::clone(model));
    }
    let model = crate::semantic::load_model()?;
    *guard = Some(Arc::clone(&model));
    Ok(model)
}

fn fanout_workers(payload: &Value) -> Value {
    let forward = payload.get("forward").cloned().unwrap_or_else(|| json!({}));
    let forward_op = forward.get("op").and_then(Value::as_str).unwrap_or_default();
    if !matches!(
        forward_op,
        "read" | "find" | "explore" | "path" | "status" | "log" | "fetch"
    ) {
        return json!({"ok": false, "error": format!("fan-out (-w '*') allows only read-only ops, not {forward_op:?}")});
    }
    let mut targets: Vec<(PathBuf, PathBuf)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (root, socket) in crate::objects::runtime_list() {
        let canonical = std::fs::canonicalize(&socket).unwrap_or_else(|_| socket.clone());
        if !seen.insert(canonical) {
            continue;
        }
        if is_live(&socket) {
            targets.push((root, socket));
        }
    }
    let results: Vec<(PathBuf, Result<Value, String>)> = std::thread::scope(|scope| {
        let forward = &forward;
        let handles: Vec<_> = targets
            .iter()
            .map(|(root, socket)| scope.spawn(move || (root.clone(), request_json(socket, forward))))
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap_or_else(|_| (PathBuf::new(), Err("fanout thread panicked".to_string()))))
            .collect()
    });
    let mut merged = String::new();
    let mut items = Vec::new();
    for (root, result) in results {
        match result {
            Ok(value) => {
                let out = value.get("output").and_then(Value::as_str).unwrap_or("");
                merged.push_str(&format!("=== {} ===\n{out}", root.display()));
                if !merged.ends_with('\n') {
                    merged.push('\n');
                }
                items.push(json!({
                    "workspace": root.to_string_lossy(),
                    "json": value.get("json").cloned().unwrap_or(Value::Null),
                }));
            }
            Err(error) => {
                merged.push_str(&format!("=== {} ===\n(error: {error})\n", root.display()));
            }
        }
    }
    json!({"ok": true, "output": merged, "json": {"op": "fanout", "workspaces": items}})
}

struct Supervisor {
    exe: PathBuf,
    lsp: bool,
    semantic: bool,
    shutting_down: AtomicBool,
    workers: Mutex<std::collections::HashMap<PathBuf, u32>>,
    crashes: Mutex<std::collections::HashMap<PathBuf, (u32, std::time::Instant)>>,
}

impl Supervisor {
    fn spawn_worker(self: &Arc<Self>, root: PathBuf, handover: bool) {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let mut command = std::process::Command::new(&self.exe);
        command.arg("worker").arg(&root);
        if self.lsp {
            command.arg("--lsp");
        }
        if self.semantic {
            command.arg("--semantic");
        }
        scrub_injection_env(&mut command);
        command.env_remove("ISOHYPSE_HANDOVER");
        if handover {
            command.env("ISOHYPSE_HANDOVER", "1");
        }
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                eprintln!("supervisor: cannot spawn worker for {}: {error}", root.display());
                return;
            }
        };
        let pid = child.id();
        if let Ok(mut workers) = self.workers.lock() {
            workers.insert(root.clone(), pid);
        }
        let supervisor = Arc::clone(self);
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            let still_ours = supervisor
                .workers
                .lock()
                .ok()
                .and_then(|workers| workers.get(&root).copied())
                == Some(pid);
            if !still_ours || supervisor.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            if let Ok(mut workers) = supervisor.workers.lock() {
                if workers.get(&root).copied() == Some(pid) {
                    workers.remove(&root);
                }
            }
            if supervisor.crash_looping(&root) {
                eprintln!("supervisor: worker for {} is crash-looping; not restarting", root.display());
                return;
            }
            eprintln!("supervisor: worker for {} exited; restarting", root.display());
            std::thread::sleep(std::time::Duration::from_millis(300));
            supervisor.spawn_worker(root, false);
        });
    }

    fn crash_looping(&self, root: &Path) -> bool {
        let Ok(mut crashes) = self.crashes.lock() else {
            return false;
        };
        let now = std::time::Instant::now();
        let entry = crashes.entry(root.to_path_buf()).or_insert((0, now));
        if now.duration_since(entry.1).as_secs() > 10 {
            *entry = (1, now);
            false
        } else {
            entry.0 += 1;
            entry.1 = now;
            entry.0 > 5
        }
    }

    fn kill_worker(&self, root: &Path) -> bool {
        let pid = self.workers.lock().ok().and_then(|mut workers| workers.remove(root));
        match pid {
            Some(pid) => {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGTERM);
                }
                true
            }
            None => false,
        }
    }

    fn stop_all(&self) -> Vec<u32> {
        self.shutting_down.store(true, Ordering::SeqCst);
        let pids: Vec<u32> = self
            .workers
            .lock()
            .map(|workers| workers.values().copied().collect())
            .unwrap_or_default();
        for pid in &pids {
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGTERM);
            }
        }
        pids
    }

    fn roots(&self) -> Vec<PathBuf> {
        self.workers.lock().map(|workers| workers.keys().cloned().collect()).unwrap_or_default()
    }
}

fn spawn_orphan_guard(socket: PathBuf) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(3));
        if unsafe { libc::getppid() } == 1 {
            let _ = std::fs::remove_file(&socket);
            std::process::exit(0);
        }
    });
}

pub fn run_worker(root: PathBuf, lsp_enabled: bool, semantic_enabled: bool) -> Result<(), String> {
    let handover = std::env::var_os("ISOHYPSE_HANDOVER").is_some();
    HANDOVER_MODE.store(handover, Ordering::SeqCst);
    let socket = socket_path(&root);
    if !handover && connect_endpoint(&socket).is_ok() {
        return Err(format!("a worker is already live on {}", socket.display()));
    }
    if !handover {
        let _ = std::fs::remove_file(&socket);
    }
    let registry = Registry::new(lsp_enabled, semantic_enabled);
    let workspace = build_workspace(&registry, root)?;
    install_signal_cleanup(&workspace.socket);
    let _ = crate::objects::runtime_register(&workspace.root, &workspace.socket);
    spawn_orphan_guard(workspace.socket.clone());
    serve_workspace(Arc::clone(&registry), workspace)?;
    Ok(())
}

pub fn run_supervisor(root: PathBuf, lsp_enabled: bool, semantic_enabled: bool) -> Result<(), String> {
    let handover = std::env::var_os("ISOHYPSE_HANDOVER").is_some();
    HANDOVER_MODE.store(handover, Ordering::SeqCst);
    let control = supervisor_socket();
    if !handover && is_live(&control) {
        return Err(format!("a supervisor is already live on {}", control.display()));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let supervisor = Arc::new(Supervisor {
        exe,
        lsp: lsp_enabled,
        semantic: semantic_enabled,
        shutting_down: AtomicBool::new(false),
        workers: Mutex::new(std::collections::HashMap::new()),
        crashes: Mutex::new(std::collections::HashMap::new()),
    });
    install_supervisor_signals(&control);
    if handover {
        signal_old_supervisor_retire();
    } else {
        let _ = std::fs::remove_file(&control);
    }
    spawn_services(&supervisor);
    let mut roots: Vec<PathBuf> = vec![std::fs::canonicalize(&root).unwrap_or(root)];
    for (persisted_root, _lsp, _semantic) in crate::objects::persisted_workspaces(3 * 24 * 60 * 60) {
        if !persisted_root.exists() {
            crate::objects::forget_workspace(&persisted_root);
            continue;
        }
        let canonical = std::fs::canonicalize(&persisted_root).unwrap_or(persisted_root);
        if !roots.contains(&canonical) {
            roots.push(canonical);
        }
    }
    for workspace_root in roots {
        supervisor.spawn_worker(workspace_root, handover);
    }
    let listener = if handover {
        let staging = staging_socket(&control);
        let _ = std::fs::remove_file(&staging);
        let bound = bind_listener(&staging)?;
        std::fs::rename(&staging, &control)
            .map_err(|e| format!("cannot promote supervisor socket: {e}"))?;
        bound
    } else {
        bind_listener(&control)?
    };
    HANDOVER_MODE.store(false, Ordering::SeqCst);
    println!("supervisor on {}", control.display());
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let supervisor = Arc::clone(&supervisor);
        std::thread::spawn(move || serve_supervisor_one(stream, &supervisor));
    }
    Ok(())
}

fn serve_supervisor_one(stream: DaemonStream, supervisor: &Arc<Supervisor>) {
    {
        use std::os::unix::io::AsRawFd;
        if !peer_ok(stream.as_raw_fd()) {
            return;
        }
    }
    use std::io::Read;
    let mut reader = BufReader::new((&stream).take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<Value>(&line) {
        Ok(payload) => {
            let op = payload.get("op").and_then(Value::as_str).unwrap_or_default();
            let root_arg = || {
                payload
                    .get("root")
                    .and_then(Value::as_str)
                    .map(|raw| std::fs::canonicalize(raw).unwrap_or_else(|_| PathBuf::from(raw)))
            };
            match op {
                "stop" => {
                    if let Err(message) = stop_authorized(&payload) {
                        let body = json!({"ok": false, "error": message});
                        let _ = (&stream).write_all(format!("{body}\n").as_bytes());
                        return;
                    }
                    let _ = supervisor.stop_all();
                    let _ = (&stream).write_all(b"{\"ok\":true,\"output\":\"supervisor stopping\\n\"}\n");
                    std::thread::sleep(std::time::Duration::from_millis(400));
                    let _ = std::fs::remove_file(supervisor_socket());
                    unsafe {
                        libc::killpg(0, libc::SIGKILL);
                    }
                    std::process::exit(0);
                }
                "retire" => {
                    supervisor.shutting_down.store(true, Ordering::SeqCst);
                    let _ = (&stream).write_all(b"{\"ok\":true,\"output\":\"supervisor retiring\\n\"}\n");
                    std::thread::spawn(|| {
                        std::thread::sleep(std::time::Duration::from_secs(8));
                        unsafe {
                            libc::killpg(0, libc::SIGKILL);
                        }
                    });
                    return;
                }
                "spawn" => match root_arg() {
                    Some(root) => {
                        let running = supervisor.workers.lock().map(|w| w.contains_key(&root)).unwrap_or(false);
                        if running {
                            json!({"ok": true, "output": format!("worker already running for {}\n", root.display())})
                        } else {
                            supervisor.spawn_worker(root.clone(), false);
                            json!({"ok": true, "output": format!("worker spawned for {}\n", root.display())})
                        }
                    }
                    None => json!({"ok": false, "error": "spawn needs a root"}),
                },
                "kill" => match root_arg() {
                    Some(root) => {
                        if supervisor.kill_worker(&root) {
                            crate::objects::forget_workspace(&root);
                            json!({"ok": true, "output": format!("worker killed for {}\n", root.display())})
                        } else {
                            json!({"ok": false, "error": format!("no worker running for {}", root.display())})
                        }
                    }
                    None => json!({"ok": false, "error": "kill needs a root"}),
                },
                "reload" => {
                    for root in supervisor.roots() {
                        supervisor.spawn_worker(root, true);
                    }
                    json!({"ok": true, "output": "supervisor reloading workers\n"})
                }
                "list" | "status" => {
                    let roots = supervisor.roots();
                    let mut out = format!("supervisor: {} worker(s)\n", roots.len());
                    for root in &roots {
                        out.push_str(&format!("  {}\n", root.display()));
                    }
                    json!({"ok": true, "output": out, "json": {"workers": roots.iter().map(|r| r.to_string_lossy()).collect::<Vec<_>>()}})
                }
                other => json!({"ok": false, "error": format!("unknown supervisor op {other}")}),
            }
        }
        Err(e) => json!({"ok": false, "error": format!("malformed request: {e}")}),
    };
    let mut out = response.to_string();
    out.push('\n');
    let _ = (&stream).write_all(out.as_bytes());
}

pub fn serve_one(stream: DaemonStream, context: &Workspace, registry: &Arc<Registry>) {
    {
        use std::os::unix::io::AsRawFd;
        if !peer_ok(stream.as_raw_fd()) {
            return;
        }
    }
    use std::io::Read;
    let mut reader = BufReader::new((&stream).take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
        return;
    }
    let response = match serde_json::from_str::<Value>(&line) {
        Ok(payload) => {
            let op = payload.get("op").and_then(Value::as_str).unwrap_or_default();
            if op == "stop" {
                if let Err(message) = stop_authorized(&payload) {
                    let body = json!({"ok": false, "error": message});
                    let _ = (&stream).write_all(format!("{body}\n").as_bytes());
                    return;
                }
                let _ = (&stream).write_all(b"{\"ok\":true,\"output\":\"daemon stopping\\n\"}\n");
                for workspace in registry.snapshot() {
                    let _drained = workspace.apply_queue.lock();
                }
                for workspace in registry.snapshot() {
                    let _ = std::fs::remove_file(&workspace.socket);
                    crate::objects::runtime_unregister(&workspace.root);
                }
                let _ = std::fs::remove_file(&context.socket);
                std::process::exit(0);
            }
            if op == "handover" {
                SUPERSEDED.store(true, Ordering::SeqCst);
                let _ = (&stream).write_all(b"{\"ok\":true,\"output\":\"handover accepted\\n\"}\n");
                let registry = Arc::clone(registry);
                std::thread::spawn(move || {
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
                    loop {
                        let idle = registry.snapshot().iter().all(|ws| ws.apply_queue.try_lock().is_ok());
                        if idle || std::time::Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    std::process::exit(0);
                });
                return;
            }
            let selector = payload.get("workspace").and_then(Value::as_str);
            crate::objects::touch_workspace(&context.root);
            let outcome = dispatch_op(op, &payload, context, registry, selector);
            match outcome {
                Ok(result) => json!({"ok": true, "output": result.render(), "json": result.to_json()}),
                Err(error) => json!({"ok": false, "error": error}),
            }
        }
        Err(e) => json!({"ok": false, "error": format!("malformed request: {e}")}),
    };
    let mut out = response.to_string();
    out.push('\n');
    let _ = (&stream).write_all(out.as_bytes());
}

pub fn runtime_reachable() -> Option<PathBuf> {
    crate::objects::runtime_list()
        .into_iter()
        .map(|(_, socket)| socket)
        .find(|socket| is_live(socket))
}

fn dispatch_op(
    op: &str,
    payload: &Value,
    context: &Workspace,
    registry: &Arc<Registry>,
    selector: Option<&str>,
) -> Result<Box<dyn OpResult>, String> {
    match selector {
        None | Some("") | Some(".") => run_op(op, payload, context),
        Some("*") => fan_out(op, payload, registry).map(into_boxed),
        Some(sel) => {
            let target = resolve_workspace(registry, sel)?;
            run_op(op, payload, &target)
        }
    }
}

fn into_boxed<T: OpResult + 'static>(value: T) -> Box<dyn OpResult> {
    Box::new(value)
}

pub struct SessionDispatcher {
    pub registry: Arc<Registry>,
    pub context: Arc<Workspace>,
}

impl crate::serve::Dispatcher for SessionDispatcher {
    fn dispatch(&self, op: &str, workspace: Option<&str>, payload: &Value) -> Result<(String, Value), String> {
        crate::objects::touch_workspace(&self.context.root);
        let result = dispatch_op(op, payload, &self.context, &self.registry, workspace)?;
        Ok((result.render(), result.to_json()))
    }
}

fn arg_str(payload: &Value) -> &str {
    payload.get("arg").and_then(Value::as_str).unwrap_or_default()
}

fn run_op(op: &str, payload: &Value, ws: &Workspace) -> Result<Box<dyn OpResult>, String> {
    let query = arg_str(payload);
    match op {
        "context.explore" => {
            if let Some(to) = payload.get("to").and_then(Value::as_str) {
                return call_path(&ws.index, &format!("{query} {to}")).map(into_boxed);
            }
            let semantic_guard = ws.semantic.lock().ok();
            let semantic = semantic_guard.as_ref().and_then(|guard| guard.as_ref());
            let options = ExploreOptions::from_payload(payload);
            explore(&ws.index, &ws.root, ws.refs.as_ref(), semantic, query, &options).map(into_boxed)
        }
        "state.status" => status(ws).map(into_boxed),
        "context.find" => find(&ws.index, &ws.root, query, &FindOptions::from_payload(payload)).map(into_boxed),
        "verify.build" => {
            let spec = ws.build.as_ref().ok_or_else(|| {
                format!(
                    "no build command for {}; add a .isohypse.build file with the command",
                    ws.root.display()
                )
            })?;
            let outcome = crate::buildspec::run(&ws.root, spec);
            if outcome.ok {
                Ok(into_boxed(outcome))
            } else {
                Err(crate::buildspec::render(&outcome))
            }
        }
        "multi-op" => multi_op(ws, payload).map(into_boxed),
        "context.prompt" => Ok(into_boxed(crate::prompt::FORMAT_REFERENCE.to_string())),
        "state.up" => {
            if payload.get("reindex").and_then(Value::as_bool) != Some(true) {
                return Err("state.up runs from the CLI; in a session, pass {\"reindex\":true} to rebuild this workspace".to_string());
            }
            let summary = ws.index.lock().map_err(|_| "index poisoned".to_string())?.full_index();
            Ok(into_boxed(format!(
                "indexed {} files ({} extracted, {} unreadable), {} symbols\n",
                summary.files, summary.extracted, summary.skipped, summary.symbols
            )))
        }
        "state.get" => fetch_object(query).map(into_boxed),
        "state.put" => store_object(query).map(into_boxed),
        "verify.diagnose" => diagnose(ws, payload).map(into_boxed),
        "macro.save" => macro_save(payload).map(into_boxed),
        "macro.run" => macro_run(ws, payload).map(into_boxed),
        "macro.list" => list_macros().map(into_boxed),
        "context.read" => {
            let mut patcher = Patcher::with_shared_cache(&ws.root, Arc::clone(&ws.analysis_cache))?;
            let outline = payload.get("outline").and_then(Value::as_bool).unwrap_or(false);
            let max_bytes = payload.get("max_bytes").and_then(Value::as_u64).map(|n| n as usize);
            let (start, end) = match payload.get("range").and_then(Value::as_str) {
                Some(spec) => {
                    let mut parts = spec.split('-');
                    (
                        parts.next().and_then(|s| s.trim().parse::<usize>().ok()),
                        parts.next().and_then(|s| s.trim().parse::<usize>().ok()),
                    )
                }
                None => (None, None),
            };
            patcher.read_with(query, start, end, outline, max_bytes).map(into_boxed)
        }
        "verify.check" => {
            let mut patcher = Patcher::with_shared_cache(&ws.root, Arc::clone(&ws.analysis_cache))?;
            let target = check_target(payload);
            patcher
                .check_edits(&target)
                .map(|report| crate::render::check_report(&report))
                .map(into_boxed)
        }
        "context.log" => {
            let patcher = Patcher::with_shared_cache(&ws.root, Arc::clone(&ws.analysis_cache))?;
            if query.trim().is_empty() {
                patcher.changelog().map(into_boxed)
            } else {
                patcher.log(query).map(into_boxed)
            }
        }
        "mutate.create" => {
            let _turn = ws.apply_queue.lock().map_err(|_| "apply queue poisoned".to_string())?;
            let path = payload.get("path").and_then(Value::as_str).ok_or("mutate.create needs a path")?;
            let content = payload.get("content").and_then(Value::as_str).unwrap_or("");
            let mut patcher = Patcher::with_shared_cache(&ws.root, Arc::clone(&ws.analysis_cache))?;
            patcher.create(path, content).map(into_boxed)
        }
        "mutate.undo" => {
            let _turn = ws.apply_queue.lock().map_err(|_| "apply queue poisoned".to_string())?;
            let tag = payload.get("tag").and_then(Value::as_str);
            let recover = payload.get("recover").and_then(Value::as_bool).unwrap_or(false);
            let mut patcher = Patcher::with_shared_cache(&ws.root, Arc::clone(&ws.analysis_cache))?;
            patcher.undo_with(query, tag, recover).map(into_boxed)
        }
        "mutate.edit" => {
            let validate = mutate_validate(ws, payload)?;
            let quiet = payload.get("quiet").and_then(Value::as_bool).unwrap_or(true);
            queued_edit(ws, payload, validate.as_deref(), quiet).map(into_boxed)
        }
        _ => Err(format!("unknown op {op:?}")),
    }
}

fn check_target(payload: &Value) -> Value {
    if let Some(ops) = payload.get("ops") {
        return ops.clone();
    }
    let mut target = payload.clone();
    if let Value::Object(map) = &mut target {
        map.remove("op");
    }
    target
}

fn mutate_validate(ws: &Workspace, payload: &Value) -> Result<Option<String>, String> {
    match payload.get("validate") {
        None => Err("mutate needs \"validate\": a shell command, or \"none\" to waive it".to_string()),
        Some(Value::String(s)) if s == "none" => Ok(None),
        other => resolve_verify(ws, other),
    }
}

fn queued_edit(
    context: &Workspace,
    payload: &Value,
    validate: Option<&str>,
    quiet: bool,
) -> Result<ApplyOutcome, String> {
    let _turn = context.apply_queue.lock().map_err(|_| "apply queue poisoned".to_string())?;
    let mut patcher = Patcher::with_shared_cache(&context.root, Arc::clone(&context.analysis_cache))?;
    let target = check_target(payload);
    let report = patcher.apply_edits_verified(&target, validate)?;
    Ok(ApplyOutcome { report, quiet })
}

pub(crate) fn session_note(payload: &Value) -> Result<String, String> {
    file_request(payload)
}

pub(crate) fn session_notes() -> Result<String, String> {
    list_requests()
}

pub fn store_root() -> PathBuf {
    std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    })
}

fn macro_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains('/') || name.contains("..") {
        return Err(format!("invalid macro name {name:?}"));
    }
    Ok(store_root().join("macros").join(format!("{name}.json")))
}

fn macro_save(payload: &Value) -> Result<String, String> {
    let name = payload.get("name").and_then(Value::as_str).ok_or("macro.save needs a name")?;
    let steps = payload.get("steps").ok_or("macro.save needs a steps array")?;
    let count = steps.as_array().ok_or("steps must be an array")?.len();
    let path = macro_path(name)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(steps).unwrap_or_default())
        .map_err(|e| format!("cannot write macro {name}: {e}"))?;
    Ok(format!("macro {name} saved ({count} steps)\n"))
}

fn macro_run(ws: &Workspace, payload: &Value) -> Result<MultiOpResult, String> {
    let name = payload.get("name").and_then(Value::as_str).ok_or("macro.run needs a name")?;
    let path = macro_path(name)?;
    let raw = std::fs::read_to_string(&path).map_err(|_| format!("no macro named {name}"))?;
    let steps: Value = serde_json::from_str(&raw).map_err(|e| format!("macro {name} is corrupt: {e}"))?;
    let mut steps_payload = json!({ "steps": steps });
    steps_payload["cwd"] = json!(ws.root.to_string_lossy());
    if let Some(quiet) = payload.get("quiet") {
        steps_payload["quiet"] = quiet.clone();
    }
    if let Some(validate) = payload.get("validate") {
        steps_payload["validate"] = validate.clone();
    }
    multi_op(ws, &steps_payload)
}

fn list_macros() -> Result<String, String> {
    let dir = store_root().join("macros");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Ok("no macros saved\n".to_string()),
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(|stem| stem.to_string())
        })
        .collect();
    names.sort();
    if names.is_empty() {
        return Ok("no macros saved\n".to_string());
    }
    Ok(format!("{}\n", names.join("\n")))
}

fn requests_path() -> PathBuf {
    let root = std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    });
    root.join("requests.jsonl")
}

fn file_request(payload: &Value) -> Result<String, String> {
    let message = arg_str(payload).trim();
    if message.is_empty() {
        return Err("request needs a message".to_string());
    }
    if message.len() > 64 * 1024 {
        return Err("request message too large".to_string());
    }
    let scope = payload.get("scope").and_then(Value::as_str).unwrap_or("session");
    if !matches!(scope, "session" | "persistent" | "architectural") {
        return Err(format!("scope must be session, persistent, or architectural, got {scope:?}"));
    }
    let entry = json!({
        "scope": scope,
        "severity": payload.get("severity").and_then(Value::as_str).unwrap_or("normal"),
        "message": message,
        "rationale": payload.get("rationale").and_then(Value::as_str),
        "patch": payload.get("patch").and_then(Value::as_str),
    });
    let path = requests_path();
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1024 * 1024 {
            return Err("requests log is full; process and clear the queue".to_string());
        }
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let mut line = entry.to_string();
    line.push('\n');
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot open requests log: {e}"))?;
    file.write_all(line.as_bytes()).map_err(|e| format!("cannot write request: {e}"))?;
    Ok(format!("request filed ({scope})\n"))
}

fn list_requests() -> Result<String, String> {
    let path = requests_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return Ok("no requests filed\n".to_string()),
    };
    let mut out = String::new();
    let mut count = 0;
    for line in raw.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        count += 1;
        out.push_str(&format!(
            "[{}] {}\n",
            entry.get("scope").and_then(Value::as_str).unwrap_or("session"),
            entry.get("message").and_then(Value::as_str).unwrap_or_default()
        ));
    }
    Ok(format!("{count} request(s)\n{out}"))
}

struct DiagnoseResult {
    parse_errors: Vec<(String, usize, String)>,
    compiler: Option<(bool, String)>,
}

impl OpResult for DiagnoseResult {
    fn render(&self) -> String {
        let mut out = format!("parse: {} problem(s)\n", self.parse_errors.len());
        for (file, line, kind) in &self.parse_errors {
            out.push_str(&format!("  {file}:{line}: {kind}\n"));
        }
        match &self.compiler {
            Some((ok, output)) => {
                out.push_str(&format!("compiler: {}\n", if *ok { "clean" } else { "errors" }));
                if !ok {
                    out.push_str(output);
                    if !output.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
            None => out.push_str("compiler: no build command\n"),
        }
        out
    }
    fn to_json(&self) -> Value {
        json!({
            "op": "diagnose",
            "parse": self.parse_errors.iter().map(|(file, line, kind)| json!({
                "file": file, "line": line, "kind": kind,
            })).collect::<Vec<_>>(),
            "compiler": self.compiler.as_ref().map(|(ok, output)| json!({ "ok": ok, "output": output })),
        })
    }
}

fn diagnose(ws: &Workspace, payload: &Value) -> Result<DiagnoseResult, String> {
    let path = arg_str(payload).trim();
    let mut parse_errors = Vec::new();
    if !path.is_empty() {
        crate::patcher::ensure_within(&std::fs::canonicalize(&ws.root).unwrap_or_else(|_| ws.root.clone()), path)?;
        let absolute = ws.root.join(path);
        let text = std::fs::read_to_string(&absolute).map_err(|e| format!("cannot read {path}: {e}"))?;
        for (line, kind) in crate::blocks::all_parse_errors(path, &text) {
            parse_errors.push((path.to_string(), line, kind));
        }
    }
    let compiler = ws.build.as_ref().map(|spec| {
        let outcome = crate::buildspec::run(&ws.root, spec);
        (outcome.ok, crate::buildspec::render(&outcome))
    });
    Ok(DiagnoseResult { parse_errors, compiler })
}

fn fetch_object(tag: &str) -> Result<String, String> {
    if tag.trim().is_empty() {
        return Err("fetch needs a tag".to_string());
    }
    let store = crate::objects::ObjectStore::open()?;
    match store.resolve(tag) {
        crate::objects::Resolution::Found(_, content) => Ok(content),
        crate::objects::Resolution::Ambiguous(candidates) => {
            Err(format!("tag {tag} is ambiguous across {} stored objects", candidates.len()))
        }
        crate::objects::Resolution::Missing => Err(format!("no object stored for tag {tag}")),
    }
}

fn store_object(content: &str) -> Result<String, String> {
    let store = crate::objects::ObjectStore::open()?;
    let full = store.put(content)?;
    Ok(format!("stored [#{full}]\n"))
}

fn resolve_verify(ws: &Workspace, field: Option<&Value>) -> Result<Option<String>, String> {
    let from_build = || {
        ws.build
            .as_ref()
            .map(|spec| Some(spec.command.clone()))
            .ok_or_else(|| "no build command to verify against; add a .isohypse.build file".to_string())
    };
    match field {
        None | Some(Value::Bool(false)) | Some(Value::Null) => Ok(None),
        Some(Value::Bool(true)) => from_build(),
        Some(Value::String(s)) if s.trim().is_empty() => from_build(),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(other) => Err(format!("verify must be a string or bool, got {other}")),
    }
}

fn resolve_workspace(registry: &Arc<Registry>, selector: &str) -> Result<Arc<Workspace>, String> {
    let target = Path::new(selector);
    let workspaces = registry.snapshot();
    if let Some(exact) = workspaces.iter().find(|ws| ws.root == target) {
        return Ok(Arc::clone(exact));
    }
    let matches: Vec<&Arc<Workspace>> = workspaces
        .iter()
        .filter(|ws| {
            ws.root.ends_with(target)
                || ws.root.file_name() == Some(std::ffi::OsStr::new(selector))
        })
        .collect();
    match matches.as_slice() {
        [single] => Ok(Arc::clone(single)),
        [] => Err(format!(
            "no workspace matches {selector:?}; live roots: {}",
            join_roots(workspaces.iter())
        )),
        _ => Err(format!(
            "workspace {selector:?} is ambiguous across: {}",
            join_roots(matches.into_iter())
        )),
    }
}

fn join_roots<'a>(workspaces: impl Iterator<Item = &'a Arc<Workspace>>) -> String {
    workspaces
        .map(|ws| ws.root.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn body_digest(body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

fn fan_out(op: &str, payload: &Value, registry: &Arc<Registry>) -> Result<FanOutResult, String> {
    if op.starts_with("mutate.") || op == "multi-op" {
        return Err(format!("{op} cannot target all workspaces; name a single one"));
    }
    let workspaces = registry.snapshot();
    if workspaces.is_empty() {
        return Err("no workspaces registered".to_string());
    }
    type FanResults = Vec<(PathBuf, Result<Box<dyn OpResult>, String>)>;
    let results: FanResults = workspaces
        .iter()
        .map(|ws| (ws.root.clone(), run_op(op, payload, ws)))
        .collect();
    let any_items = results
        .iter()
        .any(|(_, r)| r.as_ref().map(|b| b.fan_items().is_some()).unwrap_or(false));
    if any_items {
        let mut order: Vec<String> = Vec::new();
        let mut by_tag: BTreeMap<String, MergedItem> = BTreeMap::new();
        for (root, result) in &results {
            let Ok(boxed) = result else { continue };
            let Some(items) = boxed.fan_items() else { continue };
            for item in items {
                by_tag
                    .entry(item.tag.clone())
                    .and_modify(|existing| existing.workspaces.push(root.clone()))
                    .or_insert_with(|| {
                        order.push(item.tag.clone());
                        MergedItem {
                            render: item.render.clone(),
                            json: item.json.clone(),
                            workspaces: vec![root.clone()],
                        }
                    });
            }
        }
        let items = order
            .into_iter()
            .filter_map(|tag| by_tag.remove(&tag))
            .collect();
        return Ok(FanOutResult::Merged(items));
    }
    let mut entries = Vec::new();
    let mut seen: BTreeMap<u64, PathBuf> = BTreeMap::new();
    for (root, result) in results {
        let entry = match result {
            Ok(boxed) => {
                let body = boxed.render();
                let digest = body_digest(&body);
                let identical_to = seen.get(&digest).cloned();
                if identical_to.is_none() {
                    seen.insert(digest, root.clone());
                }
                FanEntry {
                    root,
                    body,
                    json: boxed.to_json(),
                    identical_to,
                    error: false,
                }
            }
            Err(e) => FanEntry {
                root,
                body: format!("(error: {e})"),
                json: json!({ "error": e }),
                identical_to: None,
                error: true,
            },
        };
        entries.push(entry);
    }
    Ok(FanOutResult::Blocks(entries))
}

struct MergedItem {
    render: String,
    json: Value,
    workspaces: Vec<PathBuf>,
}

struct FanEntry {
    root: PathBuf,
    body: String,
    json: Value,
    identical_to: Option<PathBuf>,
    error: bool,
}

enum FanOutResult {
    Blocks(Vec<FanEntry>),
    Merged(Vec<MergedItem>),
}

fn workspace_label(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string())
}

impl OpResult for FanOutResult {
    fn render(&self) -> String {
        let mut out = String::new();
        match self {
            FanOutResult::Merged(items) => {
                for item in items {
                    out.push_str(&item.render);
                    let labels = item
                        .workspaces
                        .iter()
                        .map(|r| workspace_label(r))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("  [in: {labels}]\n"));
                }
            }
            FanOutResult::Blocks(entries) => {
                for entry in entries {
                    match &entry.identical_to {
                        Some(prior) => out.push_str(&format!(
                            "== workspace {} == identical to {}\n",
                            entry.root.display(),
                            prior.display()
                        )),
                        None => {
                            out.push_str(&format!("== workspace {} ==\n", entry.root.display()));
                            out.push_str(&entry.body);
                            if !entry.body.ends_with('\n') {
                                out.push('\n');
                            }
                        }
                    }
                }
            }
        }
        out
    }
    fn to_json(&self) -> Value {
        match self {
            FanOutResult::Merged(items) => json!({
                "op": "fan_out",
                "mode": "merged",
                "items": items.iter().map(|item| json!({
                    "workspaces": item.workspaces.iter().map(|r| r.display().to_string()).collect::<Vec<_>>(),
                    "result": item.json,
                })).collect::<Vec<_>>(),
            }),
            FanOutResult::Blocks(entries) => json!({
                "op": "fan_out",
                "mode": "blocks",
                "workspaces": entries.iter().map(|entry| json!({
                    "root": entry.root.display().to_string(),
                    "identical_to": entry.identical_to.as_ref().map(|p| p.display().to_string()),
                    "error": entry.error,
                    "result": entry.json,
                })).collect::<Vec<_>>(),
            }),
        }
    }
}

fn status(context: &Workspace) -> Result<StatusResult, String> {
    let index = context.index.lock().map_err(|_| "index lock poisoned".to_string())?;
    let refs_state = match &context.refs {
        Some(index) if index.ready() => "warm",
        Some(_) => "loading",
        None => "off",
    };
    let semantic_state = if !context.semantic_requested {
        "off".to_string()
    } else {
        match context.semantic.lock().ok().and_then(|guard| guard.as_ref().map(|s| s.len())) {
            Some(count) => format!("warm ({count} embedded)"),
            None => "loading".to_string(),
        }
    };
    let state = if (context.semantic_requested && semantic_state == "loading") || refs_state == "loading" {
        "indexing"
    } else {
        "ready"
    };
    Ok(StatusResult {
        root: context.root.clone(),
        state: state.to_string(),
        files: index.store.file_count(),
        symbols: index.store.symbol_count(),
        watcher_live: context.watcher_alive,
        refs_state: refs_state.to_string(),
        semantic_state,
        uptime: context.started.elapsed().as_secs(),
    })
}

struct StatusResult {
    root: PathBuf,
    state: String,
    files: usize,
    symbols: usize,
    watcher_live: bool,
    refs_state: String,
    semantic_state: String,
    uptime: u64,
}

impl OpResult for StatusResult {
    fn render(&self) -> String {
        format!(
            "root: {}\nstate: {}\nindexed files: {}\nsymbols: {}\nwatcher: {}\napply queue: serialized in-daemon\nverified refs: {}\nsemantic: {}\nuptime: {}s\n",
            self.root.display(),
            self.state,
            self.files,
            self.symbols,
            if self.watcher_live { "live" } else { "off" },
            self.refs_state,
            self.semantic_state,
            self.uptime
        )
    }
    fn to_json(&self) -> Value {
        json!({
            "op": "status",
            "root": self.root.display().to_string(),
            "state": self.state,
            "files": self.files,
            "symbols": self.symbols,
            "watcher": self.watcher_live,
            "refs": self.refs_state,
            "semantic": self.semantic_state,
            "uptime_secs": self.uptime,
        })
    }
}

struct ApplyOutcome {
    report: crate::patcher::ApplyReport,
    quiet: bool,
}

impl OpResult for ApplyOutcome {
    fn render(&self) -> String {
        crate::render::apply_report(&self.report, self.quiet)
    }
    fn to_json(&self) -> Value {
        apply_report_json(&self.report)
    }
}

fn apply_report_json(report: &crate::patcher::ApplyReport) -> Value {
    json!({
        "op": "mutate.edit",
        "sections": report.sections.iter().map(|s| json!({
            "path": s.path,
            "op": s.op,
            "new_tag": s.new_tag,
            "dest": s.dest,
            "first_changed_line": s.first_changed_line,
            "shifts": s.shifts.iter().map(|(after, delta)| json!({"after_line": after, "delta": delta})).collect::<Vec<_>>(),
            "recovered": s.recovered,
            "structural": s.structural,
        })).collect::<Vec<_>>(),
        "warnings": report.warnings,
    })
}

fn run_verify(cwd: &Path, command: &str) -> Result<(), String> {
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("verify command failed to start: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut detail = String::from_utf8_lossy(&output.stdout).into_owned();
    detail.push_str(&String::from_utf8_lossy(&output.stderr));
    Err(format!("verify `{command}` failed:\n{}", crate::buildspec::head_tail(&detail)))
}

fn rollback_multi_op(
    patcher: &mut Patcher,
    ledger: &crate::patcher::MultiOpLedger,
    step: usize,
    reason: &str,
) -> String {
    let reverted = ledger.actions.len();
    match patcher.revert_ledger(ledger) {
        Ok(()) => format!(
            "multi-op failed at step {}: {reason}; {reverted} prior mutation(s) reverted",
            step + 1
        ),
        Err(e) => format!(
            "multi-op failed at step {}: {reason}; ROLLBACK ALSO FAILED: {e}",
            step + 1
        ),
    }
}

struct MultiOpStepOutcome {
    index: usize,
    op: String,
    summary: String,
    json: Value,
}

struct MultiOpResult {
    items: Vec<MultiOpStepOutcome>,
    transactional: bool,
}

impl OpResult for MultiOpResult {
    fn render(&self) -> String {
        let kind = if self.transactional { "transactional" } else { "read-only" };
        let mut out = format!("multi-op: {} step(s), {kind}\n", self.items.len());
        for item in &self.items {
            out.push_str(&format!("-- step {} ({})\n", item.index + 1, item.op));
            out.push_str(&item.summary);
            if !item.summary.ends_with('\n') {
                out.push('\n');
            }
        }
        out
    }
    fn to_json(&self) -> Value {
        let grouped = |effect: &str| {
            self.items
                .iter()
                .filter(|i| effect_of(&i.op) == effect)
                .map(|i| json!({"index": i.index, "op": i.op, "json": i.json}))
                .collect::<Vec<_>>()
        };
        json!({
            "op": "multi-op",
            "transactional": self.transactional,
            "context": grouped("context"),
            "mutate": grouped("mutate"),
            "verify": grouped("verify"),
            "other": grouped("other"),
        })
    }
}

fn effect_of(op: &str) -> &'static str {
    if op.starts_with("context.") {
        "context"
    } else if op.starts_with("mutate.") {
        "mutate"
    } else if op.starts_with("verify.") {
        "verify"
    } else {
        "other"
    }
}

fn multi_op(context: &Workspace, payload: &Value) -> Result<MultiOpResult, String> {
    let steps = payload
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| "multi-op needs a steps array".to_string())?;
    if steps.is_empty() {
        return Err("multi-op has no steps".to_string());
    }
    for step in steps {
        if let Some(sel) = step.get("workspace").and_then(Value::as_str) {
            if !sel.is_empty() && sel != "." {
                return Err(
                    "multi-op steps cannot target other workspaces yet; run the multi-op in the target workspace".to_string(),
                );
            }
        }
        if step.get("op").and_then(Value::as_str) == Some("mutate.undo") {
            return Err("mutate.undo cannot run inside a multi-op; run it as its own op".to_string());
        }
    }
    let quiet = payload.get("quiet").and_then(Value::as_bool).unwrap_or(false);
    let mutating = steps.iter().any(|s| {
        s.get("op").and_then(Value::as_str).map(|o| o.starts_with("mutate.")).unwrap_or(false)
    });
    let validate_field = payload.get("validate");
    if mutating && validate_field.is_none() {
        return Err(
            "multi-op has mutate steps but no top-level \"validate\" (a command or \"none\"); declare how this change is verified".to_string(),
        );
    }
    let validate = match validate_field.and_then(Value::as_str) {
        Some("none") => None,
        _ => resolve_verify(context, validate_field)?,
    };
    if !mutating {
        let mut items = Vec::new();
        for (index, step) in steps.iter().enumerate() {
            let op = step.get("op").and_then(Value::as_str).unwrap_or_default();
            let result = run_op(op, step, context)?;
            items.push(MultiOpStepOutcome {
                index,
                op: op.to_string(),
                summary: result.render(),
                json: result.to_json(),
            });
        }
        return Ok(MultiOpResult { items, transactional: false });
    }
    let _turn = context.apply_queue.lock().map_err(|_| "apply queue poisoned".to_string())?;
    let caller_cwd = context.root.clone();
    let mut patcher = Patcher::with_shared_cache(&caller_cwd, Arc::clone(&context.analysis_cache))?;
    let mut ledger = crate::patcher::MultiOpLedger::default();
    let mut items: Vec<MultiOpStepOutcome> = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let op = step.get("op").and_then(Value::as_str).unwrap_or_default();
        let step_outcome: Result<(String, Value), String> = match op {
            "mutate.edit" => {
                let target = check_target(step);
                patcher.apply_edits(&target).map(|report| {
                    let summary = crate::render::apply_report(&report, quiet);
                    let json = apply_report_json(&report);
                    ledger.actions.push(crate::patcher::MultiOpAction::Applied(report));
                    (summary, json)
                })
            }
            "mutate.create" => match step.get("path").and_then(Value::as_str) {
                Some(path) => {
                    let content = step.get("content").and_then(Value::as_str).unwrap_or_default();
                    patcher.create(path, content).map(|summary| {
                        ledger.actions.push(crate::patcher::MultiOpAction::Created(path.to_string()));
                        (summary, json!({ "op": "mutate.create", "path": path }))
                    })
                }
                None => Err("mutate.create step needs a path (arg)".to_string()),
            },
            _ => run_op(op, step, context).map(|result| (result.render(), result.to_json())),
        };
        match step_outcome {
            Ok((summary, json)) => items.push(MultiOpStepOutcome {
                index,
                op: op.to_string(),
                summary,
                json,
            }),
            Err(reason) => return Err(rollback_multi_op(&mut patcher, &ledger, index, &reason)),
        }
        let checkpoint = step.get("checkpoint").and_then(Value::as_bool).unwrap_or(false);
        if checkpoint {
            if let Some(command) = validate.as_deref() {
                if let Err(reason) = run_verify(&caller_cwd, command) {
                    return Err(rollback_multi_op(&mut patcher, &ledger, index, &reason));
                }
            }
        }
    }
    if let Some(command) = validate.as_deref() {
        if let Err(reason) = run_verify(&caller_cwd, command) {
            return Err(rollback_multi_op(&mut patcher, &ledger, steps.len() - 1, &reason));
        }
    }
    Ok(MultiOpResult { items, transactional: true })
}

fn call_path(index: &Arc<Mutex<GraphIndex>>, query: &str) -> Result<PathResult, String> {
    let mut parts = query.split_whitespace();
    let (Some(from), Some(to)) = (parts.next(), parts.next()) else {
        return Err("path needs two symbol names: `isohypse path <from> <to>`".to_string());
    };
    let index = index.lock().map_err(|_| "index lock poisoned".to_string())?;
    match index.call_path(from, to) {
        Some(chain) => {
            let steps = chain
                .iter()
                .map(|(symbol, confidence)| PathStep {
                    qualified: symbol.qualified.clone(),
                    file: symbol.file.clone(),
                    line: symbol.start_line,
                    confidence: confidence.clone(),
                })
                .collect();
            Ok(PathResult { steps })
        }
        None => match index.call_path(to, from) {
            Some(_) => Err(format!("no call path {from} -> {to}; the reverse direction {to} -> {from} exists")),
            None => Err(format!("no call path between {from} and {to} in either direction")),
        },
    }
}

struct PathStep {
    qualified: String,
    file: String,
    line: usize,
    confidence: String,
}

struct PathResult {
    steps: Vec<PathStep>,
}

impl OpResult for PathResult {
    fn render(&self) -> String {
        let mut out = String::new();
        for (position, step) in self.steps.iter().enumerate() {
            if position > 0 {
                out.push_str(" -> ");
            }
            out.push_str(&step.qualified);
            if !step.confidence.is_empty() {
                out.push_str(&format!(" -({})", step.confidence));
            }
        }
        out.push('\n');
        for step in &self.steps {
            out.push_str(&format!("  {} at {}:{}\n", step.qualified, step.file, step.line));
        }
        out
    }
    fn to_json(&self) -> Value {
        json!({
            "op": "path",
            "chain": self.steps.iter().map(|s| json!({
                "qualified": s.qualified,
                "file": s.file,
                "line": s.line,
                "confidence": s.confidence,
            })).collect::<Vec<_>>(),
        })
    }
}

const FIND_MATCH_CAP: usize = 200;

pub struct FindOptions {
    pub name: bool,
    pub max: usize,
    pub path_prefixes: Vec<String>,
    pub langs: Vec<String>,
    pub patterns: Vec<String>,
}

impl FindOptions {
    pub fn from_payload(payload: &Value) -> Self {
        FindOptions {
            name: payload.get("name").and_then(Value::as_bool).unwrap_or(false),
            max: payload
                .get("max")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0)
                .map(|n| n as usize)
                .unwrap_or(FIND_MATCH_CAP),
            path_prefixes: string_list(payload.get("paths")),
            langs: string_list(payload.get("langs")),
            patterns: string_list(payload.get("patterns")),
        }
    }

    fn passes(&self, file: &str) -> bool {
        if !self.path_prefixes.is_empty() && !self.path_prefixes.iter().any(|p| file.starts_with(p.as_str())) {
            return false;
        }
        if !self.langs.is_empty() {
            let extension = Path::new(file).extension().and_then(|e| e.to_str()).unwrap_or("");
            if !self.langs.iter().any(|lang| lang == extension) {
                return false;
            }
        }
        true
    }
}

struct FindNeedle {
    pattern: String,
    needle: String,
    sensitive: bool,
}

fn find_needles(query: &str, options: &FindOptions) -> Result<Vec<FindNeedle>, String> {
    let patterns: Vec<String> = if options.patterns.is_empty() {
        let single = query.trim();
        if single.is_empty() {
            return Err("find needs a pattern: `isohypse find <text>`".to_string());
        }
        vec![single.to_string()]
    } else {
        options
            .patterns
            .iter()
            .map(|pattern| pattern.trim().to_string())
            .filter(|pattern| !pattern.is_empty())
            .collect()
    };
    if patterns.is_empty() {
        return Err("find needs at least one pattern".to_string());
    }
    Ok(patterns
        .into_iter()
        .map(|pattern| {
            let sensitive = pattern.chars().any(|c| c.is_uppercase());
            let needle = if sensitive { pattern.clone() } else { pattern.to_ascii_lowercase() };
            FindNeedle { pattern, needle, sensitive }
        })
        .collect())
}

fn needle_matches(needle: &FindNeedle, haystack: &str) -> bool {
    if needle.sensitive {
        haystack.contains(&needle.needle)
    } else {
        haystack.to_ascii_lowercase().contains(&needle.needle)
    }
}

fn find(
    index: &Arc<Mutex<GraphIndex>>,
    root: &Path,
    query: &str,
    options: &FindOptions,
) -> Result<FindResult, String> {
    let needles = find_needles(query, options)?;
    let files: Vec<String> = {
        let locked = index.lock().map_err(|_| "index lock poisoned".to_string())?;
        locked.store.files().filter(|f| options.passes(f)).cloned().collect()
    };
    let labels = needles.iter().map(|n| n.pattern.clone()).collect::<Vec<String>>().join(", ");

    if options.name {
        let mut hits: Vec<&String> = files
            .iter()
            .filter(|file| needles.iter().any(|needle| needle_matches(needle, file)))
            .collect();
        hits.sort();
        if hits.is_empty() {
            return Ok(FindResult {
                text: format!("no tracked file path matches {labels:?}\n"),
                paths: Vec::new(),
                matches: Vec::new(),
            });
        }
        let shown = hits.len().min(options.max);
        let mut out = String::new();
        for file in hits.iter().take(shown) {
            out.push_str(file);
            out.push('\n');
        }
        if hits.len() > shown {
            out.push_str(&format!("({} more paths; narrow with --path or raise --max)\n", hits.len() - shown));
        }
        let paths = hits.iter().take(shown).map(|file| (*file).clone()).collect();
        return Ok(FindResult { text: out, paths, matches: Vec::new() });
    }

    let mut per_pattern: Vec<Vec<FindMatch>> = (0..needles.len()).map(|_| Vec::new()).collect();
    let mut truncated = vec![false; needles.len()];
    for file in &files {
        if per_pattern.iter().all(|hits| hits.len() >= options.max) {
            break;
        }
        let Ok(raw) = std::fs::read_to_string(root.join(file)) else { continue };
        for (offset, line) in raw.lines().enumerate() {
            for (slot, needle) in needles.iter().enumerate() {
                if !needle_matches(needle, line) {
                    continue;
                }
                if per_pattern[slot].len() >= options.max {
                    truncated[slot] = true;
                    continue;
                }
                per_pattern[slot].push(FindMatch {
                    file: file.clone(),
                    line: offset + 1,
                    text: line.trim_end().to_string(),
                    pattern: needle.pattern.clone(),
                });
            }
        }
    }
    let total: usize = per_pattern.iter().map(Vec::len).sum();
    if total == 0 {
        return Ok(FindResult {
            text: format!("no match for {labels:?} in {} tracked files\n", files.len()),
            paths: Vec::new(),
            matches: Vec::new(),
        });
    }
    let single = needles.len() == 1;
    let mut out = String::new();
    let mut collected: Vec<FindMatch> = Vec::new();
    for (slot, hits) in per_pattern.into_iter().enumerate() {
        if !single {
            out.push_str(&format!("== {} ==\n", needles[slot].pattern));
        }
        if hits.is_empty() {
            out.push_str("(no match)\n");
            continue;
        }
        let file_count = {
            let mut seen_files: BTreeSet<&str> = BTreeSet::new();
            for hit in &hits {
                seen_files.insert(hit.file.as_str());
            }
            seen_files.len()
        };
        for hit in &hits {
            out.push_str(&format!("{}:{}:{}\n", hit.file, hit.line, hit.text));
        }
        out.push_str(&format!("({} matches across {file_count} files)\n", hits.len()));
        if truncated[slot] {
            out.push_str(&format!("(stopped at {} matches; raise --max or narrow with --path)\n", options.max));
        }
        collected.extend(hits);
    }
    Ok(FindResult { text: out, paths: Vec::new(), matches: collected })
}

struct FindMatch {
    file: String,
    line: usize,
    text: String,
    pattern: String,
}

struct FindResult {
    text: String,
    paths: Vec<String>,
    matches: Vec<FindMatch>,
}

impl OpResult for FindResult {
    fn render(&self) -> String {
        self.text.clone()
    }
    fn to_json(&self) -> Value {
        json!({
            "op": "find",
            "paths": self.paths,
            "matches": self.matches.iter().map(|m| json!({
                "file": m.file,
                "line": m.line,
                "text": m.text,
                "pattern": m.pattern,
            })).collect::<Vec<_>>(),
        })
    }
}

fn query_terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for token in query.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == ':' || c == '.')) {
        let token = token.trim_matches(|c: char| c == ':' || c == '.');
        if token.len() < 3 {
            continue;
        }
        let owned = token.to_string();
        if !terms.contains(&owned) {
            terms.push(owned);
        }
    }
    terms.truncate(8);
    terms
}

pub struct ExploreOptions {
    pub max: usize,
    pub compact: bool,
    pub full: bool,
    pub json: bool,
    pub literal: bool,
    pub path_prefixes: Vec<String>,
    pub langs: Vec<String>,
}

impl Default for ExploreOptions {
    fn default() -> Self {
        ExploreOptions {
            max: EXPLORE_FILE_CAP,
            compact: false,
            full: false,
            json: false,
            literal: false,
            path_prefixes: Vec::new(),
            langs: Vec::new(),
        }
    }
}

impl ExploreOptions {
    pub fn from_payload(payload: &Value) -> Self {
        let mut options = ExploreOptions::default();
        if let Some(max) = payload.get("max").and_then(Value::as_u64) {
            if max > 0 {
                options.max = max as usize;
            }
        }
        options.compact = payload.get("compact").and_then(Value::as_bool).unwrap_or(false);
        options.full = payload.get("full").and_then(Value::as_bool).unwrap_or(false);
        options.json = payload.get("json").and_then(Value::as_bool).unwrap_or(false);
        options.literal = payload.get("literal").and_then(Value::as_bool).unwrap_or(false);
        options.path_prefixes = string_list(payload.get("paths"));
        options.langs = string_list(payload.get("langs"));
        options
    }

    fn passes(&self, symbol: &SymbolRow) -> bool {
        if !self.path_prefixes.is_empty()
            && !self.path_prefixes.iter().any(|prefix| symbol.file.starts_with(prefix.as_str()))
        {
            return false;
        }
        if !self.langs.is_empty() {
            let extension = Path::new(&symbol.file).extension().and_then(|e| e.to_str()).unwrap_or("");
            if !self.langs.iter().any(|lang| lang == extension) {
                return false;
            }
        }
        true
    }
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

pub fn explore(
    index: &Arc<Mutex<GraphIndex>>,
    root: &Path,
    refs: Option<&crate::refs::PreciseIndex>,
    semantic: Option<&crate::semantic::SemanticIndex>,
    query: &str,
    options: &ExploreOptions,
) -> Result<ExploreResult, String> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Err("query carries no searchable identifiers".to_string());
    }
    let objects = crate::objects::ObjectStore::open()?;
    let mut file_cache: std::collections::HashMap<String, (String, Vec<String>)> = std::collections::HashMap::new();
    let mut seen_by_tag: std::collections::HashMap<String, BTreeSet<usize>> = std::collections::HashMap::new();
    let load_file = |file: &str,
                         cache: &mut std::collections::HashMap<String, (String, Vec<String>)>|
     -> Result<(String, Vec<String>), String> {
        if let Some(entry) = cache.get(file) {
            return Ok(entry.clone());
        }
        let raw = std::fs::read_to_string(root.join(file)).map_err(|e| format!("cannot read {file}: {e}"))?;
        let normalized = crate::normalize::normalize_to_lf(crate::normalize::strip_bom(&raw).text);
        let full = objects.put(&normalized)?;
        objects.journal_record(&root.join(file).to_string_lossy(), &full)?;
        let lines = crate::normalize::split_lines(&normalized).lines;
        cache.insert(file.to_string(), (full.clone(), lines.clone()));
        Ok((full, lines))
    };
    let locked = index.lock().map_err(|_| "index lock poisoned".to_string())?;
    let mut selected: Vec<SymbolRow> = Vec::new();
    let mut seen_ids: BTreeSet<String> = BTreeSet::new();
    let mut semantic_scores: std::collections::HashMap<String, f32> = std::collections::HashMap::new();
    for term in &terms {
        for symbol in locked.store.symbols_matching(term, EXPLORE_SYMBOL_CAP) {
            if symbol.kind == "file" || !options.passes(&symbol) {
                continue;
            }
            if seen_ids.insert(symbol.id.clone()) {
                selected.push(symbol);
            }
        }
    }
    let literal_count = selected.len();
    if !options.literal {
        if let Some(semantic) = semantic {
            if selected.len() < 3 {
                for (id, score) in semantic.query(query, 5) {
                    if let Some(symbol) = locked.store.symbol_by_id(&id) {
                        if symbol.kind != "file" && options.passes(&symbol) && seen_ids.insert(symbol.id.clone()) {
                            semantic_scores.insert(symbol.id.clone(), score);
                            selected.push(symbol);
                        }
                    }
                }
            }
        }
    }
    if selected.is_empty() {
        let definitive = options.literal || semantic.is_none();
        let tail = if definitive {
            "no such symbol in the precise index"
        } else {
            "no precise symbol, and semantic search found nothing above threshold"
        };
        return Ok(ExploreResult { text: format!("no match for {terms:?}: {tail}; try `isohypse context read <path>` or a different name\n"), hits: Vec::new() });
    }
    selected.truncate(options.max * 2);

    let mut kept: Vec<SymbolRow> = Vec::new();
    for symbol in &selected {
        if kept.len() >= options.max {
            break;
        }
        kept.push(symbol.clone());
    }

    let mut hits: Vec<ExploreHit> = Vec::new();
    for symbol in &kept {
        let (file_tag, lines) = match load_file(&symbol.file, &mut file_cache) {
            Ok(pair) => pair,
            Err(_) => continue,
        };
        let start = symbol.start_line.saturating_sub(1);
        let end = symbol.end_line.min(lines.len());
        let source = lines.get(start..end).map(|slice| slice.join("\n")).unwrap_or_default();
        let content_tag = crate::tag::full_tag(&source);
        let snippet = lines.get(start).map(|line| line.trim().to_string()).unwrap_or_default();
        let mut compact = format!(
            "{}:{} {} {}",
            symbol.file, symbol.start_line, symbol.kind, symbol.qualified
        );
        if let Some(score) = semantic_scores.get(&symbol.id) {
            compact.push_str(&format!(" ~{score:.2}"));
        }
        if !snippet.is_empty() {
            compact.push_str(&format!("  — {snippet}"));
        }
        hits.push(ExploreHit {
            qualified: symbol.qualified.clone(),
            kind: symbol.kind.clone(),
            file: symbol.file.clone(),
            start_line: symbol.start_line,
            end_line: symbol.end_line,
            content_tag,
            file_tag,
            compact,
        });
    }
    let semantic_only = literal_count == 0;
    if options.json {
        return Ok(ExploreResult { text: render_json(&locked, &kept, &semantic_scores, semantic_only), hits });
    }
    let estimated_lines: usize = kept
        .iter()
        .map(|s| (s.end_line.saturating_sub(s.start_line) + 1).min(EXPLORE_LINE_CAP))
        .sum();
    let auto_compact = !options.full && estimated_lines > FULL_LINE_BUDGET;
    if options.compact || auto_compact {
        let mut out = render_compact(&locked, root, &kept, &semantic_scores, semantic_only);
        if auto_compact && !options.compact {
            out.push_str(&format!(
                "\n({} results, ~{estimated_lines} source lines — compact view to bound output; add --full for source, or read a named symbol)\n",
                kept.len()
            ));
        }
        return Ok(ExploreResult { text: out, hits });
    }

    let mut sections: Vec<String> = Vec::new();
    let banner: Option<String> = if semantic_only {
        Some(format!(
            "note: no exact symbol matched {terms:?}; the results below are semantic guesses — verify before trusting.\n\n"
        ))
    } else {
        None
    };
    for symbol in &kept {
        let span_lines = symbol.end_line.saturating_sub(symbol.start_line) + 1;
        let end = if span_lines > EXPLORE_LINE_CAP {
            symbol.start_line + EXPLORE_LINE_CAP - 1
        } else {
            symbol.end_line
        };
        match load_file(&symbol.file, &mut file_cache) {
            Ok((full, lines)) => {
                let mut section = match semantic_scores.get(&symbol.id) {
                    Some(score) => format!("## {} ({} {}) — semantic match {score:.2}\n", symbol.qualified, symbol.kind, symbol.file),
                    None => format!("## {} ({} {})\n", symbol.qualified, symbol.kind, symbol.file),
                };
                section.push_str(&format!("[{}#{}]\n", symbol.file, &full[..crate::tag::DISPLAY_TAG_LENGTH]));
                let last = end.min(lines.len());
                let seen = seen_by_tag.entry(full).or_default();
                for line_number in symbol.start_line..=last {
                    section.push_str(&format!("{line_number}:{}\n", lines[line_number - 1]));
                    seen.insert(line_number);
                }
                if end < symbol.end_line {
                    section.push_str(&format!(
                        "(truncated at {EXPLORE_LINE_CAP} lines; symbol ends at line {})\n",
                        symbol.end_line
                    ));
                }
                sections.push(section);
            }
            Err(e) => sections.push(format!("## {} — unreadable: {e}\n", symbol.qualified)),
        }
    }
    for (full, seen) in &seen_by_tag {
        objects.record_seen(full, seen)?;
    }

    for (symbol, section) in kept.iter().zip(sections.iter_mut()) {
        if symbol.kind != "fn" && symbol.kind != "type" && symbol.kind != "trait" {
            continue;
        }
        let callers = locked.callers_of(&symbol.id);
        if !callers.confirmed.is_empty() {
            section.push_str("callers (confirmed): ");
            section.push_str(
                &callers
                    .confirmed
                    .iter()
                    .map(|c| match semantic.and_then(|s| s.relatedness(&c.id, &symbol.id)) {
                        Some(score) => format!("{} ({}:{}) ~{score:.2}", c.qualified, c.file, c.start_line),
                        None => format!("{} ({}:{})", c.qualified, c.file, c.start_line),
                    })
                    .collect::<Vec<String>>()
                    .join(", "),
            );
            section.push('\n');
        }
        if !callers.dynamic.is_empty() {
            section.push_str("dynamic candidates: ");
            section.push_str(
                &callers
                    .dynamic
                    .iter()
                    .map(|(c, trait_name)| match semantic.and_then(|s| s.relatedness(&c.id, &symbol.id)) {
                        Some(score) => format!("{} via trait {trait_name} ~{score:.2}", c.qualified),
                        None => format!("{} via trait {trait_name}", c.qualified),
                    })
                    .collect::<Vec<String>>()
                    .join(", "),
            );
            section.push('\n');
        }
        if !callers.named.is_empty() {
            let mut ranked: Vec<_> = callers
                .named
                .iter()
                .map(|c| (c, semantic.and_then(|s| s.relatedness(&c.id, &symbol.id))))
                .collect();
            ranked.sort_by(|a, b| {
                b.1.unwrap_or(f32::NEG_INFINITY)
                    .partial_cmp(&a.1.unwrap_or(f32::NEG_INFINITY))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let rendered = ranked
                .iter()
                .take(8)
                .map(|(c, score)| match score {
                    Some(value) => format!("{} ({}:{}) ~{value:.2}", c.qualified, c.file, c.start_line),
                    None => format!("{} ({}:{})", c.qualified, c.file, c.start_line),
                })
                .collect::<Vec<String>>()
                .join(", ");
            let label = if semantic.is_some() {
                "callers (name-tier, semantic-ranked)"
            } else {
                "name-tier candidates"
            };
            section.push_str(&format!("{label}: {rendered}\n"));
        }
        let blast = locked.blast_radius(&symbol.id, BLAST_DEPTH);
        if blast.confirmed_symbols > 0 {
            let files = blast
                .confirmed_files
                .iter()
                .map(|(file, count)| format!("{file} ({count})"))
                .collect::<Vec<String>>()
                .join(", ");
            section.push_str(&format!(
                "blast radius (depth {BLAST_DEPTH}, confirmed): {} symbols across {}\n",
                blast.confirmed_symbols, files
            ));
        }
        if !blast.dynamic.is_empty() {
            section.push_str(&format!(
                "blast radius (dynamic, reported separately): {}\n",
                blast
                    .dynamic
                    .iter()
                    .map(|(s, trait_name)| format!("{} via {trait_name}", s.qualified))
                    .collect::<Vec<String>>()
                    .join(", ")
            ));
        }
        if let Some(sem) = semantic {
            if !sem.is_common_name(&symbol.id) {
                let own_language = crate::blocks::lang_for_path(&symbol.file);
                let kin: Vec<String> = sem
                    .neighbors(&symbol.id, 16)
                    .into_iter()
                    .filter(|(_, score)| *score >= CROSS_LANGUAGE_FLOOR)
                    .filter_map(|(id, score)| {
                        if sem.is_common_name(&id) {
                            return None;
                        }
                        let other = locked.store.symbol_by_id(&id)?;
                        let other_language = crate::blocks::lang_for_path(&other.file);
                        if other_language.is_some() && other_language != own_language {
                            Some(format!("{} ({}) ~{score:.2}", other.qualified, other.file))
                        } else {
                            None
                        }
                    })
                    .take(5)
                    .collect();
                if !kin.is_empty() {
                    section.push_str(&format!("cross-language kin: {}\n", kin.join(", ")));
                }
            }
        }
    }
    if let Some(refs) = refs {
        for (symbol, section) in kept.iter().zip(sections.iter_mut()).take(3) {
            if symbol.kind != "fn" {
                continue;
            }
            let column = file_cache
                .get(&symbol.file)
                .and_then(|(_, lines)| lines.get(symbol.start_line - 1))
                .and_then(|line| line.find(&symbol.name))
                .unwrap_or(0);
            match refs.references(&symbol.file, symbol.start_line, column) {
                Some(references) if !references.is_empty() => {
                    section.push_str("callers (precise): ");
                    section.push_str(
                        &references
                            .iter()
                            .take(12)
                            .map(|(file, line)| match locked.store.enclosing_symbol(file, *line) {
                                Some(caller) => match semantic.and_then(|s| s.relatedness(&caller.id, &symbol.id)) {
                                    Some(score) => format!("{} ({file}:{line}) ~{score:.2}", caller.qualified),
                                    None => format!("{} ({file}:{line})", caller.qualified),
                                },
                                None => format!("{file}:{line}"),
                            })
                            .collect::<Vec<String>>()
                            .join(", "),
                    );
                    section.push('\n');
                }
                Some(_) => {}
                None => section.push_str("(precise refs: overlay warming; tree-sitter tiers shown)\n"),
            }
        }
    }
    let mut out = banner.unwrap_or_default();
    for section in sections {
        out.push_str(&section);
        out.push('\n');
    }
    Ok(ExploreResult { text: out, hits })
}

struct ExploreHit {
    qualified: String,
    kind: String,
    file: String,
    start_line: usize,
    end_line: usize,
    content_tag: String,
    file_tag: String,
    compact: String,
}

pub struct ExploreResult {
    text: String,
    hits: Vec<ExploreHit>,
}

impl OpResult for ExploreResult {
    fn render(&self) -> String {
        self.text.clone()
    }
    fn to_json(&self) -> Value {
        json!({
            "op": "explore",
            "hits": self.hits.iter().map(|h| json!({
                "qualified": h.qualified,
                "kind": h.kind,
                "file": h.file,
                "start_line": h.start_line,
                "end_line": h.end_line,
                "content_tag": h.content_tag,
                "file_tag": h.file_tag,
            })).collect::<Vec<_>>(),
        })
    }
    fn fan_items(&self) -> Option<Vec<crate::opresult::FanItem>> {
        Some(self.hits.iter().map(|h| crate::opresult::FanItem {
            tag: h.content_tag.clone(),
            render: h.compact.clone(),
            json: json!({
                "qualified": h.qualified,
                "kind": h.kind,
                "file": h.file,
                "start_line": h.start_line,
                "end_line": h.end_line,
                "content_tag": h.content_tag,
            }),
        }).collect())
    }
}

fn first_source_line(root: &Path, file: &str, line: usize) -> String {
    std::fs::read_to_string(root.join(file))
        .ok()
        .and_then(|raw| raw.lines().nth(line.saturating_sub(1)).map(str::trim).map(str::to_string))
        .unwrap_or_default()
}

fn render_compact(
    index: &GraphIndex,
    root: &Path,
    kept: &[SymbolRow],
    semantic_scores: &std::collections::HashMap<String, f32>,
    semantic_only: bool,
) -> String {
    let mut out = String::new();
    if semantic_only {
        out.push_str("note: no exact match; semantic guesses below — verify before trusting.\n");
    }
    for symbol in kept {
        let callers = index.callers_of(&symbol.id);
        let confirmed = callers.confirmed.len();
        let named = callers.named.len();
        let snippet = first_source_line(root, &symbol.file, symbol.start_line);
        out.push_str(&format!(
            "{}:{} {} {}",
            symbol.file, symbol.start_line, symbol.kind, symbol.qualified
        ));
        if let Some(score) = semantic_scores.get(&symbol.id) {
            out.push_str(&format!(" ~{score:.2}"));
        }
        if confirmed > 0 || named > 0 {
            out.push_str(&format!(" [callers {confirmed} confirmed, {named} name-tier]"));
        }
        if !snippet.is_empty() {
            out.push_str(&format!("  — {snippet}"));
        }
        out.push('\n');
    }
    out
}

fn render_json(
    index: &GraphIndex,
    kept: &[SymbolRow],
    semantic_scores: &std::collections::HashMap<String, f32>,
    semantic_only: bool,
) -> String {
    let results: Vec<Value> = kept
        .iter()
        .map(|symbol| {
            let callers = index.callers_of(&symbol.id);
            let confirmed: Vec<Value> = callers
                .confirmed
                .iter()
                .map(|c| json!({"qualified": c.qualified, "file": c.file, "line": c.start_line}))
                .collect();
            let blast = index.blast_radius(&symbol.id, BLAST_DEPTH);
            json!({
                "qualified": symbol.qualified,
                "name": symbol.name,
                "kind": symbol.kind,
                "file": symbol.file,
                "start_line": symbol.start_line,
                "end_line": symbol.end_line,
                "semantic_score": semantic_scores.get(&symbol.id),
                "callers_confirmed": confirmed,
                "blast_symbols": blast.confirmed_symbols,
            })
        })
        .collect();
    let payload = json!({"semantic_only": semantic_only, "results": results});
    let mut out = payload.to_string();
    out.push('\n');
    out
}

pub(crate) fn vector_packs(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Some(name) = crate::objects::opaque_name(VECTORS_NAME_CTX, &root.to_string_lossy()) else {
        return out;
    };
    let Ok(versions) = std::fs::read_dir(vectors_root()) else {
        return out;
    };
    for version in versions.flatten() {
        let candidate = version.path().join(format!("{name}.pack"));
        if candidate.is_file() {
            out.push(candidate);
        }
    }
    out
}
