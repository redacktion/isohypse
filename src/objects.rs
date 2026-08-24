use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::tag::{full_tag, is_valid_tag};

pub struct ObjectStore {
    root: PathBuf,
}

#[derive(Default, Clone, Debug)]
pub struct SeenRanges {
    runs: Vec<(usize, usize)>,
}

impl SeenRanges {
    pub fn parse(raw: &str) -> SeenRanges {
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for token in raw.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let run = match token.split_once('-') {
                Some((a, b)) => a.parse().ok().zip(b.parse().ok()),
                None => token.parse().ok().map(|n: usize| (n, n)),
            };
            if let Some((start, end)) = run {
                runs.push((start, end.max(start)));
            }
        }
        let mut ranges = SeenRanges { runs };
        ranges.normalize();
        ranges
    }

    pub fn serialize(&self) -> String {
        self.runs
            .iter()
            .map(|(start, end)| if start == end { start.to_string() } else { format!("{start}-{end}") })
            .collect::<Vec<String>>()
            .join(",")
    }

    pub fn contains(&self, line: usize) -> bool {
        self.runs
            .binary_search_by(|(start, end)| {
                if line < *start {
                    std::cmp::Ordering::Greater
                } else if line > *end {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok()
    }

    pub fn merge_set(&mut self, lines: &BTreeSet<usize>) {
        let mut run: Option<(usize, usize)> = None;
        for &line in lines {
            match run {
                Some((start, end)) if line == end + 1 => run = Some((start, line)),
                Some(done) => {
                    self.runs.push(done);
                    run = Some((line, line));
                }
                None => run = Some((line, line)),
            }
        }
        if let Some(done) = run {
            self.runs.push(done);
        }
        self.normalize();
    }

    fn normalize(&mut self) {
        self.runs.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::with_capacity(self.runs.len());
        for &(start, end) in &self.runs {
            match merged.last_mut() {
                Some((_, last_end)) if start <= *last_end + 1 => *last_end = (*last_end).max(end),
                _ => merged.push((start, end)),
            }
        }
        self.runs = merged;
    }
}

pub enum Resolution {
    Found(String, String),
    Ambiguous(Vec<String>),
    Missing,
}

const DEFAULT_MAX_BLOB_BYTES: u64 = 1024 * 1024;

fn default_root() -> PathBuf {
    if let Ok(overridden) = std::env::var("ISOHYPSE_STORE") {
        return PathBuf::from(overridden);
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".isohypse")
}

const STORE_KEY_FILE: &str = "store.key";
const SEAL_VERSION: u8 = 1;
const PLAIN_VERSION: u8 = 0;
const RUNTIME_NAME_CTX: &str = "isohypse runtime name v1";
const RUNTIME_SEAL_CTX: &str = "isohypse runtime seal v1";
const WORKSPACES_NAME_CTX: &str = "isohypse workspaces name v1";
const WORKSPACES_SEAL_CTX: &str = "isohypse workspaces seal v1";
const SESSIONS_SEAL_CTX: &str = "isohypse sessions seal v1";
const JOURNAL_NAME_CTX: &str = "isohypse journal name v1";
const JOURNAL_SEAL_CTX: &str = "isohypse journal seal v1";
const SEEN_NAME_CTX: &str = "isohypse seen name v1";

static STORE_KEY: OnceLock<Option<[u8; 32]>> = OnceLock::new();

pub(crate) fn write_private(path: &std::path::Path, bytes: &[u8]) -> Option<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let staging = path.with_extension(format!("tmp.{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&staging)
        .ok()?;
    let written = {
        use std::io::Write;
        file.write_all(bytes).is_ok()
    };
    drop(file);
    if written && fs::rename(&staging, path).is_ok() {
        Some(())
    } else {
        let _ = fs::remove_file(&staging);
        None
    }
}

fn load_or_create_store_key() -> Option<[u8; 32]> {
    let root = default_root();
    let path = root.join(STORE_KEY_FILE);
    if let Ok(bytes) = fs::read(&path) {
        if bytes.len() == 32 {
            return bytes.try_into().ok();
        }
    }
    let _ = fs::create_dir_all(&root);
    if let Ok(bytes) = fs::read(root.join("refs").join("refs.key")) {
        if bytes.len() == 32 {
            let key: [u8; 32] = bytes.try_into().ok()?;
            write_private(&path, &key)?;
            return Some(key);
        }
    }
    let mut fresh = [0u8; 32];
    getrandom::getrandom(&mut fresh).ok()?;
    let _ = write_private(&path, &fresh);
    let _ = crate::machine::refresh();
    let bytes = fs::read(&path).ok()?;
    if bytes.len() == 32 { bytes.try_into().ok() } else { None }
}

fn store_key() -> Option<[u8; 32]> {
    *STORE_KEY.get_or_init(load_or_create_store_key)
}

pub(crate) fn opaque_name(context: &str, input: &str) -> Option<String> {
    let key = store_key()?;
    let naming = blake3::derive_key(context, &key);
    Some(blake3::keyed_hash(&naming, input.as_bytes()).to_hex().to_string())
}

fn seal_with(key: &[u8; 32], plaintext: &[u8]) -> Option<Vec<u8>> {
    let mut nonce = [0u8; 12];
    getrandom::getrandom(&mut nonce).ok()?;
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let sealed = cipher.encrypt(Nonce::from_slice(&nonce), plaintext).ok()?;
    let mut out = Vec::with_capacity(13 + sealed.len());
    out.push(SEAL_VERSION);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&sealed);
    Some(out)
}

fn open_with(key: &[u8; 32], bytes: &[u8]) -> Option<Vec<u8>> {
    let rest = bytes.strip_prefix(&[SEAL_VERSION])?;
    if rest.len() < 12 {
        return None;
    }
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher.decrypt(Nonce::from_slice(&rest[..12]), &rest[12..]).ok()
}

#[derive(Clone, Copy)]
enum Category {
    Objects,
    Caches,
    Metadata,
    Sessions,
}

fn category_for(context: &str) -> Category {
    if context.contains("objects") {
        Category::Objects
    } else if context.contains("sessions") {
        Category::Sessions
    } else if context.contains("vectors") || context.contains("refs") {
        Category::Caches
    } else {
        Category::Metadata
    }
}

fn encrypt_enabled(category: Category) -> bool {
    let config = config_for_encryption();
    match category {
        Category::Objects => config.encrypt_objects,
        Category::Caches => config.encrypt_caches,
        Category::Metadata => config.encrypt_metadata,
        Category::Sessions => config.encrypt_sessions,
    }
}

#[cfg(not(test))]
fn config_for_encryption() -> crate::setup::Config {
    crate::setup::Config::load()
}

#[cfg(test)]
thread_local! {
    static ENC_OVERRIDE: std::cell::RefCell<Option<crate::setup::Config>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn config_for_encryption() -> crate::setup::Config {
    ENC_OVERRIDE.with(|slot| slot.borrow().clone()).unwrap_or_else(crate::setup::Config::load)
}

#[cfg(test)]
fn set_enc_override(config: Option<crate::setup::Config>) {
    ENC_OVERRIDE.with(|slot| *slot.borrow_mut() = config);
}

pub(crate) fn seal_bytes(context: &str, plaintext: &[u8]) -> Option<Vec<u8>> {
    if encrypt_enabled(category_for(context)) {
        let key = store_key()?;
        seal_with(&blake3::derive_key(context, &key), plaintext)
    } else {
        let mut out = Vec::with_capacity(1 + plaintext.len());
        out.push(PLAIN_VERSION);
        out.extend_from_slice(plaintext);
        Some(out)
    }
}

pub(crate) fn open_bytes(context: &str, bytes: &[u8]) -> Option<Vec<u8>> {
    match bytes.first() {
        Some(&SEAL_VERSION) => {
            let key = store_key()?;
            open_with(&blake3::derive_key(context, &key), bytes)
        }
        Some(&PLAIN_VERSION) => {
            if encrypt_enabled(category_for(context)) {
                None
            } else {
                Some(bytes[1..].to_vec())
            }
        }
        _ => None,
    }
}

fn max_blob_bytes() -> u64 {
    std::env::var("ISOHYPSE_MAX_BLOB_BYTES")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BLOB_BYTES)
}

fn runtime_dir() -> PathBuf {
    default_root().join("runtime")
}

pub fn runtime_register(root: &std::path::Path, socket: &std::path::Path) -> Result<(), String> {
    let dir = runtime_dir();
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create runtime dir: {e}"))?;
    let key = opaque_name(RUNTIME_NAME_CTX, &root.to_string_lossy())
        .ok_or_else(|| "store key unavailable".to_string())?;
    let line = format!("{}\t{}\n", root.display(), socket.display());
    let sealed = seal_bytes(RUNTIME_SEAL_CTX, line.as_bytes())
        .ok_or_else(|| "store key unavailable".to_string())?;
    let _ = fs::remove_file(dir.join(full_tag(&root.to_string_lossy())));
    write_private(&dir.join(key), &sealed).ok_or_else(|| "cannot write runtime entry".to_string())
}

pub fn runtime_unregister(root: &std::path::Path) {
    let dir = runtime_dir();
    if let Some(key) = opaque_name(RUNTIME_NAME_CTX, &root.to_string_lossy()) {
        let _ = fs::remove_file(dir.join(key));
    }
    let _ = fs::remove_file(dir.join(full_tag(&root.to_string_lossy())));
}

fn runtime_entry_from_text(text: &str) -> Option<(PathBuf, PathBuf)> {
    let (root, socket) = text.trim().split_once('\t')?;
    if !root.starts_with('/') || !socket.starts_with('/') {
        return None;
    }
    Some((PathBuf::from(root), PathBuf::from(socket)))
}

pub fn runtime_list() -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(runtime_dir()) {
        for entry in entries.flatten() {
            let Ok(bytes) = fs::read(entry.path()) else { continue };
            let Some(plain) = open_bytes(RUNTIME_SEAL_CTX, &bytes) else { continue };
            if let Some(parsed) = runtime_entry_from_text(&String::from_utf8_lossy(&plain)) {
                out.push(parsed);
            }
        }
    }
    out
}

fn workspaces_dir() -> PathBuf {
    default_root().join("workspaces")
}

pub fn persist_workspace(root: &std::path::Path, lsp: bool, semantic: bool) {
    let dir = workspaces_dir();
    let _ = fs::create_dir_all(&dir);
    let Some(key) = opaque_name(WORKSPACES_NAME_CTX, &root.to_string_lossy()) else { return };
    let entry = format!("{}\n{}\n{}\n", root.display(), lsp, semantic);
    let Some(sealed) = seal_bytes(WORKSPACES_SEAL_CTX, entry.as_bytes()) else { return };
    let _ = fs::remove_file(dir.join(full_tag(&root.to_string_lossy())));
    let _ = write_private(&dir.join(key), &sealed);
}

pub fn forget_workspace(root: &std::path::Path) {
    let dir = workspaces_dir();
    if let Some(key) = opaque_name(WORKSPACES_NAME_CTX, &root.to_string_lossy()) {
        let _ = fs::remove_file(dir.join(key));
    }
    let _ = fs::remove_file(dir.join(full_tag(&root.to_string_lossy())));
}

pub fn touch_workspace(root: &std::path::Path) {
    let Some(key) = opaque_name(WORKSPACES_NAME_CTX, &root.to_string_lossy()) else { return };
    let path = workspaces_dir().join(key);
    if let Ok(file) = fs::OpenOptions::new().write(true).open(&path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

fn workspace_from_text(text: &str) -> Option<(PathBuf, bool, bool)> {
    let mut lines = text.lines();
    let root = lines.next()?;
    if !root.starts_with('/') {
        return None;
    }
    let lsp = lines.next() == Some("true");
    let semantic = lines.next() == Some("true");
    Some((PathBuf::from(root), lsp, semantic))
}

pub fn persisted_workspaces(ttl_secs: u64) -> Vec<(PathBuf, bool, bool)> {
    let dir = workspaces_dir();
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
            if let Ok(age) = now.duration_since(modified) {
                if age.as_secs() > ttl_secs {
                    continue;
                }
            }
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Some(plain) = open_bytes(WORKSPACES_SEAL_CTX, &bytes) else { continue };
        if let Some(parsed) = workspace_from_text(&String::from_utf8_lossy(&plain)) {
            out.push(parsed);
        }
    }
    out
}

pub(crate) fn store_dir() -> PathBuf {
    default_root()
}

pub(crate) fn objects_dir() -> PathBuf {
    default_root().join("objects")
}

pub(crate) fn journal_dir() -> PathBuf {
    default_root().join("journal")
}

pub(crate) fn workspace_entry_path(root: &std::path::Path) -> Option<PathBuf> {
    Some(workspaces_dir().join(opaque_name(WORKSPACES_NAME_CTX, &root.to_string_lossy())?))
}

pub(crate) fn workspace_entries_with_mtime() -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(workspaces_dir()) else {
        return out;
    };
    for entry in entries.flatten() {
        let Ok(modified) = entry.metadata().and_then(|m| m.modified()) else { continue };
        let Ok(bytes) = fs::read(entry.path()) else { continue };
        let text = match open_bytes(WORKSPACES_SEAL_CTX, &bytes) {
            Some(plain) => String::from_utf8_lossy(&plain).into_owned(),
            None => String::from_utf8_lossy(&bytes).into_owned(),
        };
        if let Some((root, _, _)) = workspace_from_text(&text) {
            out.push((root, modified));
        }
    }
    out
}

pub(crate) fn journal_file_info(bytes: &[u8]) -> Option<(String, Vec<String>)> {
    let text = match open_bytes(JOURNAL_SEAL_CTX, bytes) {
        Some(plain) => String::from_utf8_lossy(&plain).into_owned(),
        None => String::from_utf8_lossy(bytes).into_owned(),
    };
    let mut lines = text.lines();
    let path = lines.next()?;
    if !path.starts_with('/') {
        return None;
    }
    Some((path.to_string(), lines.map(str::to_string).collect()))
}

fn sessions_dir() -> PathBuf {
    default_root().join("sessions")
}

pub struct PersistedSession {
    pub id: String,
    pub label: String,
    pub observer: bool,
    pub scope_kinds: Vec<String>,
    pub scope_workspaces: Vec<String>,
    pub token_salt: String,
    pub token_hash: String,
}

fn encode_list(items: &[String]) -> String {
    items.join(",")
}

fn decode_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split(',').map(str::to_string).collect()
    }
}

const SESSIONS_NAME_CTX: &str = "isohypse sessions name v1";

pub fn persist_session(session: &PersistedSession) {
    use std::os::unix::fs::PermissionsExt;
    let dir = sessions_dir();
    let _ = fs::create_dir_all(&dir);
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    let Some(key) = opaque_name(SESSIONS_NAME_CTX, &session.id) else { return };
    let entry = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n",
        session.id,
        session.label,
        session.observer,
        encode_list(&session.scope_kinds),
        encode_list(&session.scope_workspaces),
        session.token_salt,
        session.token_hash,
    );
    reap_stale_sessions(&dir);
    let Some(sealed) = seal_bytes(SESSIONS_SEAL_CTX, entry.as_bytes()) else { return };
    let _ = write_private(&dir.join(key), &sealed);
}

const SESSION_TTL_SECS: u64 = 3 * 24 * 60 * 60;

fn reap_stale_sessions(dir: &std::path::Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age.as_secs() > SESSION_TTL_SECS)
            .unwrap_or(false);
        if stale {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn session_from_text(text: &str) -> Option<PersistedSession> {
    let mut lines = text.lines();
    let id = lines.next()?.to_string();
    let label = lines.next()?.to_string();
    let observer = lines.next()? == "true";
    let scope_kinds = decode_list(lines.next().unwrap_or(""));
    let scope_workspaces = decode_list(lines.next().unwrap_or(""));
    let token_salt = lines.next()?.to_string();
    let token_hash = lines.next()?.to_string();
    Some(PersistedSession {
        id,
        label,
        observer,
        scope_kinds,
        scope_workspaces,
        token_salt,
        token_hash,
    })
}

fn read_session_file(path: &std::path::Path) -> Option<PersistedSession> {
    let bytes = fs::read(path).ok()?;
    let plain = open_bytes(SESSIONS_SEAL_CTX, &bytes)?;
    session_from_text(&String::from_utf8_lossy(&plain))
}

pub fn load_session(id: &str) -> Option<PersistedSession> {
    let key = opaque_name(SESSIONS_NAME_CTX, id)?;
    let session = read_session_file(&sessions_dir().join(key))?;
    if session.id != id {
        return None;
    }
    Some(session)
}

pub fn all_persisted_sessions() -> Vec<PersistedSession> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(sessions_dir()) {
        for entry in entries.flatten() {
            if let Some(session) = read_session_file(&entry.path()) {
                out.push(session);
            }
        }
    }
    out
}

pub fn forget_session(id: &str) {
    if let Some(key) = opaque_name(SESSIONS_NAME_CTX, id) {
        let _ = fs::remove_file(sessions_dir().join(key));
    }
    let _ = fs::remove_file(sessions_dir().join(full_tag(id)));
}

const OBJECTS_SEAL_CTX: &str = "isohypse objects seal v1";

pub(crate) fn opaque_key(context: &str, input: &str) -> Option<[u8; 32]> {
    let key = store_key()?;
    let naming = blake3::derive_key(context, &key);
    Some(*blake3::keyed_hash(&naming, input.as_bytes()).as_bytes())
}

fn encode_object(content: &str) -> Option<Vec<u8>> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    {
        use std::io::Write;
        encoder.write_all(content.as_bytes()).ok()?;
    }
    let compressed = encoder.finish().ok()?;
    seal_bytes(OBJECTS_SEAL_CTX, &compressed)
}

fn decode_object(bytes: &[u8]) -> Option<String> {
    let compressed = open_bytes(OBJECTS_SEAL_CTX, bytes)?;
    let mut out = Vec::new();
    {
        use std::io::Read;
        GzDecoder::new(compressed.as_slice()).read_to_end(&mut out).ok()?;
    }
    String::from_utf8(out).ok()
}

pub fn compact_objects() -> (usize, u64, usize) {
    if store_key().is_none() {
        return (0, 0, 0);
    }
    let mut reaped = 0usize;
    let objects = default_root().join("objects");
    if let Ok(buckets) = fs::read_dir(&objects) {
        for bucket in buckets.flatten() {
            let Ok(files) = fs::read_dir(bucket.path()) else { continue };
            for file in files.flatten() {
                let path = file.path();
                let name = file.file_name().to_string_lossy().into_owned();
                if name.contains(".tmp") {
                    let _ = fs::remove_file(&path);
                    continue;
                }
                let Ok(bytes) = fs::read(&path) else { continue };
                if decode_object(&bytes).is_none() && fs::remove_file(&path).is_ok() {
                    reaped += 1;
                }
            }
        }
    }
    let seen = default_root().join("seen");
    if let Ok(entries) = fs::read_dir(&seen) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dead = name.len() != crate::tag::FULL_TAG_LENGTH
                || !name.bytes().all(|b| b.is_ascii_hexdigit());
            if dead && fs::remove_file(entry.path()).is_ok() {
                reaped += 1;
            }
        }
    }
    (0, 0, reaped)
}

const STORE_SCHEMA_VERSION: u32 = 1;

fn state_path() -> PathBuf {
    default_root().join("state.json")
}

fn read_state() -> serde_json::Value {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn write_state(state: &serde_json::Value) {
    if let Ok(text) = serde_json::to_string_pretty(state) {
        let _ = write_private(&state_path(), text.as_bytes());
    }
}

fn reseal_dir(dir: &std::path::Path, context: &str) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(".tmp") {
            continue;
        }
        let Ok(bytes) = fs::read(&path) else { continue };
        let Some(inner) = open_bytes(context, &bytes) else { continue };
        let Some(out) = seal_bytes(context, &inner) else { continue };
        let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
        if fs::write(&tmp, &out).is_ok() {
            let _ = fs::rename(&tmp, &path);
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }
}

fn reseal_objects(store: &ObjectStore) {
    let objects = store.root.join("objects");
    let Ok(buckets) = fs::read_dir(&objects) else { return };
    for bucket in buckets.flatten() {
        if !bucket.path().is_dir() {
            continue;
        }
        let Ok(files) = fs::read_dir(bucket.path()) else { continue };
        for file in files.flatten() {
            let path = file.path();
            let name = file.file_name().to_string_lossy().into_owned();
            if name.contains(".tmp") {
                continue;
            }
            let Ok(bytes) = fs::read(&path) else { continue };
            let Some(inner) = open_bytes(OBJECTS_SEAL_CTX, &bytes) else { continue };
            let Some(out) = seal_bytes(OBJECTS_SEAL_CTX, &inner) else { continue };
            let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
            if fs::write(&tmp, &out).is_ok() {
                let _ = fs::rename(&tmp, &path);
            } else {
                let _ = fs::remove_file(&tmp);
            }
        }
    }
}

fn reencrypt_category(store: &ObjectStore, category: &str) {
    let root = default_root();
    match category {
        "objects" => reseal_objects(store),
        "sessions" => reseal_dir(&root.join("sessions"), SESSIONS_SEAL_CTX),
        "metadata" => {
            reseal_dir(&root.join("runtime"), RUNTIME_SEAL_CTX);
            reseal_dir(&root.join("workspaces"), WORKSPACES_SEAL_CTX);
            reseal_dir(&root.join("journal"), JOURNAL_SEAL_CTX);
        }
        "caches" => {
            let _ = fs::remove_dir_all(root.join("vectors"));
            if let Ok(entries) = fs::read_dir(root.join("refs")) {
                for entry in entries.flatten() {
                    if entry.file_name().to_string_lossy() != "refs.key" {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
        _ => {}
    }
}

pub fn migrate_store() -> Result<(), String> {
    if store_key().is_none() {
        return Ok(());
    }
    let store = ObjectStore::open()?;
    let config = crate::setup::Config::load();
    let mut state = read_state();
    let schema = state.get("schema").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    if schema < STORE_SCHEMA_VERSION {
        store.rebuild_index();
        state["schema"] = serde_json::json!(STORE_SCHEMA_VERSION);
    }
    let written = state.get("enc").cloned().unwrap_or_else(|| serde_json::json!({}));
    let categories = [
        ("objects", config.encrypt_objects),
        ("caches", config.encrypt_caches),
        ("metadata", config.encrypt_metadata),
        ("sessions", config.encrypt_sessions),
    ];
    let mut enc_state = serde_json::Map::new();
    for (name, desired) in categories {
        let previous = written.get(name).and_then(|v| v.as_bool()).unwrap_or(true);
        if previous != desired {
            reencrypt_category(&store, name);
        }
        enc_state.insert(name.to_string(), serde_json::json!(desired));
    }
    state["enc"] = serde_json::Value::Object(enc_state);
    write_state(&state);
    Ok(())
}

impl ObjectStore {
    pub fn open() -> Result<ObjectStore, String> {
        let root = default_root();
        fs::create_dir_all(root.join("objects")).map_err(|e| format!("cannot create object store: {e}"))?;
        fs::create_dir_all(root.join("seen")).map_err(|e| format!("cannot create seen store: {e}"))?;
        fs::create_dir_all(root.join("journal")).map_err(|e| format!("cannot create journal: {e}"))?;
        let _ = fs::write(root.join(".metadata_never_index"), b"");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
        }
        Ok(ObjectStore { root })
    }

    fn journal_path(&self, path_key: &str) -> Option<PathBuf> {
        Some(self.root.join("journal").join(opaque_name(JOURNAL_NAME_CTX, path_key)?))
    }


    fn journal_text(&self, path_key: &str) -> String {
        let Some(path) = self.journal_path(path_key) else { return String::new() };
        if let Ok(bytes) = fs::read(&path) {
            if let Some(plain) = open_bytes(JOURNAL_SEAL_CTX, &bytes) {
                return String::from_utf8_lossy(&plain).into_owned();
            }
        }
        String::new()
    }

    fn write_journal_text(&self, path_key: &str, text: &str) -> Result<(), String> {
        let path = self.journal_path(path_key).ok_or_else(|| "store key unavailable".to_string())?;
        let sealed = seal_bytes(JOURNAL_SEAL_CTX, text.as_bytes())
            .ok_or_else(|| "store key unavailable".to_string())?;
        write_private(&path, &sealed).ok_or_else(|| "cannot write journal".to_string())
    }

    pub fn journal_record(&self, path_key: &str, full: &str) -> Result<(), String> {
        let existing = self.journal_text(path_key);
        if existing.is_empty() {
            return self.write_journal_text(path_key, &format!("{path_key}\n{full}\n"));
        }
        if existing.lines().last() == Some(full) {
            return Ok(());
        }
        self.write_journal_text(path_key, &format!("{existing}{full}\n"))
    }

    pub fn journal_entries(&self, path_key: &str) -> Vec<String> {
        self.journal_text(path_key)
            .lines()
            .skip(1)
            .map(str::to_string)
            .collect()
    }

    fn object_path(&self, full: &str) -> Option<PathBuf> {
        const OBJECTS_NAME_CTX: &str = "isohypse objects name v1";
        let name = opaque_name(OBJECTS_NAME_CTX, full)?;
        Some(self.root.join("objects").join(&name[..2]).join(&name[2..]))
    }

    fn read_object(&self, full: &str) -> Option<String> {
        let bytes = fs::read(self.object_path(full)?).ok()?;
        decode_object(&bytes)
    }

    fn seen_path(&self, full: &str) -> Option<PathBuf> {
        Some(self.root.join("seen").join(opaque_name(SEEN_NAME_CTX, full)?))
    }
    const IDX_CTX: &'static str = "isohypse object index seal v1";

    fn index_path(&self) -> PathBuf {
        self.root.join("objects").join("index")
    }

    fn index_append(&self, full: &str) {
        let Some(key) = store_key() else { return };
        let Some(sealed) = seal_with(&blake3::derive_key(Self::IDX_CTX, &key), full.as_bytes()) else { return };
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).mode(0o600).open(self.index_path()) {
            let mut framed = Vec::with_capacity(4 + sealed.len());
            framed.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
            framed.extend_from_slice(&sealed);
            let _ = file.write_all(&framed);
        }
    }

    fn index_tags(&self) -> Vec<String> {
        let mut tags = Vec::new();
        let Some(key) = store_key() else { return tags };
        let Ok(bytes) = fs::read(self.index_path()) else { return tags };
        let mut at = 0usize;
        while at + 4 <= bytes.len() {
            let len = u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
            at += 4;
            let Some(end) = at.checked_add(len) else { break };
            if len == 0 || end > bytes.len() {
                break;
            }
            if let Some(plain) = open_with(&blake3::derive_key(Self::IDX_CTX, &key), &bytes[at..end]) {
                if let Ok(tag) = String::from_utf8(plain) {
                    tags.push(tag);
                }
            }
            at = end;
        }
        tags
    }

    fn rebuild_index(&self) {
        let Some(key) = store_key() else { return };
        let mut tags: BTreeSet<String> = BTreeSet::new();
        let objects = self.root.join("objects");
        if let Ok(buckets) = fs::read_dir(&objects) {
            for bucket in buckets.flatten() {
                if !bucket.path().is_dir() {
                    continue;
                }
                let Ok(files) = fs::read_dir(bucket.path()) else { continue };
                for file in files.flatten() {
                    let name = file.file_name().to_string_lossy().into_owned();
                    if name.contains(".tmp") {
                        continue;
                    }
                    let Ok(bytes) = fs::read(file.path()) else { continue };
                    if let Some(content) = decode_object(&bytes) {
                        tags.insert(full_tag(&content));
                    }
                }
            }
        }
        let tmp = objects.join(format!("index.tmp.{}", std::process::id()));
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let Ok(mut file) = fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&tmp) else { return };
        let mut ok = true;
        for tag in &tags {
            let Some(sealed) = seal_with(&blake3::derive_key(Self::IDX_CTX, &key), tag.as_bytes()) else { ok = false; break };
            let mut framed = Vec::with_capacity(4 + sealed.len());
            framed.extend_from_slice(&(sealed.len() as u32).to_le_bytes());
            framed.extend_from_slice(&sealed);
            if file.write_all(&framed).is_err() {
                ok = false;
                break;
            }
        }
        drop(file);
        if ok {
            let _ = fs::rename(&tmp, self.index_path());
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }


    pub fn put(&self, content: &str) -> Result<String, String> {
        let full = full_tag(content);
        if content.len() as u64 > max_blob_bytes() {
            return Ok(full);
        }
        let path = self.object_path(&full).ok_or_else(|| "store key unavailable".to_string())?;
        let matches_existing = self
            .read_object(&full)
            .map(|existing| existing == content)
            .unwrap_or(false);
        if !matches_existing {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let encoded = encode_object(content).ok_or_else(|| "store key unavailable".to_string())?;
            let staging = path.with_extension(format!("tmp.{}", std::process::id()));
            fs::write(&staging, &encoded).map_err(|e| e.to_string())?;
            fs::rename(&staging, &path).map_err(|e| e.to_string())?;
            self.index_append(&full);
        }
        Ok(full)
    }

    pub fn resolve(&self, cited: &str) -> Resolution {
        let cited = cited.to_ascii_lowercase();
        if !is_valid_tag(&cited) {
            return Resolution::Missing;
        }
        if cited.len() == crate::tag::FULL_TAG_LENGTH {
            return match self.read_object(&cited) {
                Some(content) if full_tag(&content) == cited => Resolution::Found(cited, content),
                _ => Resolution::Missing,
            };
        }
        let mut matches: Vec<(String, String)> = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for full in self.index_tags() {
            if full.starts_with(&cited) && seen.insert(full.clone()) {
                if let Some(content) = self.read_object(&full) {
                    if full_tag(&content) == full {
                        matches.push((full, content));
                    }
                }
            }
        }
        if matches.is_empty() {
            let objects = self.root.join("objects");
            if let Ok(buckets) = fs::read_dir(&objects) {
                for bucket in buckets.flatten() {
                    if !bucket.path().is_dir() {
                        continue;
                    }
                    let Ok(files) = fs::read_dir(bucket.path()) else { continue };
                    for file in files.flatten() {
                        let name = file.file_name().to_string_lossy().into_owned();
                        if name.contains(".tmp") {
                            continue;
                        }
                        let Ok(bytes) = fs::read(file.path()) else { continue };
                        let Some(content) = decode_object(&bytes) else { continue };
                        let full = full_tag(&content);
                        if full.starts_with(&cited) && seen.insert(full.clone()) {
                            matches.push((full, content));
                        }
                    }
                }
            }
        }
        matches.sort_by(|a, b| a.0.cmp(&b.0));
        match matches.len() {
            0 => Resolution::Missing,
            1 => {
                let (full, content) = matches.into_iter().next().unwrap();
                Resolution::Found(full, content)
            }
            _ => Resolution::Ambiguous(matches.into_iter().map(|(full, _)| full).collect()),
        }
    }

    pub fn record_seen(&self, full: &str, lines: &BTreeSet<usize>) -> Result<(), String> {
        let mut merged = self.seen_lines(full).unwrap_or_default();
        merged.merge_set(lines);
        let path = self.seen_path(full).ok_or_else(|| "store key unavailable".to_string())?;
        fs::write(path, merged.serialize()).map_err(|e| e.to_string())
    }

    pub fn seen_lines(&self, full: &str) -> Option<SeenRanges> {
        let raw = fs::read_to_string(self.seen_path(full)?).ok()?;
        Some(SeenRanges::parse(&raw))
    }
}

pub struct JournalFileHistory {
    pub path: String,
    pub tags: Vec<String>,
    pub modified: std::time::SystemTime,
}

impl ObjectStore {
    pub fn journal_overview(&self, roots: &[&std::path::Path]) -> Vec<JournalFileHistory> {
        let mut out = Vec::new();
        let Ok(entries) = fs::read_dir(self.root.join("journal")) else {
            return out;
        };
        let prefixes: Vec<String> = roots.iter().map(|root| format!("{}/", root.display())).collect();
        let mut seen: HashSet<String> = HashSet::new();
        for entry in entries.flatten() {
            let Ok(bytes) = fs::read(entry.path()) else { continue };
            let Some(plain) = open_bytes(JOURNAL_SEAL_CTX, &bytes) else { continue };
            let text = String::from_utf8_lossy(&plain).into_owned();
            let mut lines = text.lines();
            let Some(path) = lines.next() else { continue };
            if !seen.insert(path.to_string()) {
                continue;
            }
            if !prefixes.iter().any(|prefix| path.starts_with(prefix.as_str())) {
                continue;
            }
            let tags: Vec<String> = lines.map(str::to_string).collect();
            if tags.is_empty() {
                continue;
            }
            let modified = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            out.push(JournalFileHistory { path: path.to_string(), tags, modified });
        }
        out.sort_by_key(|entry| std::cmp::Reverse(entry.modified));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_persistence_round_trip() {
        let root = std::env::temp_dir().join(format!("iso-persist-{}", std::process::id()));
        persist_workspace(&root, true, false);
        let found = persisted_workspaces(3 * 24 * 60 * 60);
        assert!(
            found.iter().any(|(r, lsp, sem)| r == &root && *lsp && !*sem),
            "persisted entry present"
        );
        forget_workspace(&root);
        let after = persisted_workspaces(3 * 24 * 60 * 60);
        assert!(!after.iter().any(|(r, _, _)| r == &root), "forgotten entry gone");
    }

    #[test]
    fn sealed_roundtrip() {
        let key = [7u8; 32];
        let sealed = seal_with(&key, b"store metadata").unwrap();
        assert_ne!(&sealed[13..], b"store metadata".as_slice());
        assert_eq!(open_with(&key, &sealed).unwrap(), b"store metadata");
        assert!(open_with(&[8u8; 32], &sealed).is_none());
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(open_with(&key, &tampered).is_none());
    }

    fn temp_store() -> ObjectStore {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("iso-objtest-{}-{}", std::process::id(), n));
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(dir.join("objects"));
        ObjectStore { root: dir }
    }

    #[test]
    fn index_resolves_by_prefix() {
        let store = temp_store();
        let full = store.put("fn alpha() {}\n").unwrap();
        assert!(store.index_tags().contains(&full), "index missing tag");
        match store.resolve(&full[..8]) {
            Resolution::Found(found, content) => {
                assert_eq!(found, full);
                assert_eq!(content, "fn alpha() {}\n");
            }
            _ => panic!("prefix should resolve to the stored object"),
        }
        assert!(matches!(store.resolve(&full), Resolution::Found(_, _)));
    }

    #[test]
    fn rebuild_index_recovers_tags() {
        let store = temp_store();
        let one = store.put("one\n").unwrap();
        let two = store.put("two\n").unwrap();
        let _ = fs::remove_file(store.index_path());
        assert!(store.index_tags().is_empty(), "index file not cleared");
        store.rebuild_index();
        let tags = store.index_tags();
        assert!(tags.contains(&one), "missing one");
        assert!(tags.contains(&two), "missing two");
    }

    #[test]
    fn reseal_preserves_objects() {
        let store = temp_store();
        let full = store.put("secret body\n").unwrap();
        let path = store.object_path(&full).unwrap();
        let before = fs::read(&path).unwrap()[0];
        reseal_objects(&store);
        let after = fs::read(&path).unwrap()[0];
        assert_eq!(before, after, "reseal is idempotent under unchanged config");
        assert_eq!(store.read_object(&full).unwrap(), "secret body\n");
    }

    #[test]
    fn seal_format_follows_config() {
        let config = crate::setup::Config::load();
        let blob = seal_bytes(OBJECTS_SEAL_CTX, b"hello").unwrap();
        let expected = if config.encrypt_objects { SEAL_VERSION } else { PLAIN_VERSION };
        assert_eq!(blob[0], expected, "object seal format must follow config");
        assert_eq!(open_bytes(OBJECTS_SEAL_CTX, &blob).unwrap(), b"hello");
    }

    #[test]
    fn toggling_objects_encryption_reencrypts() {
        let store = temp_store();
        let on = crate::setup::Config { encrypt_objects: true, ..crate::setup::Config::default() };
        let off = crate::setup::Config { encrypt_objects: false, ..crate::setup::Config::default() };
        set_enc_override(Some(on));
        let full = store.put("secret body\n").unwrap();
        let path = store.object_path(&full).unwrap();
        assert_eq!(fs::read(&path).unwrap()[0], SEAL_VERSION, "sealed while on");
        set_enc_override(Some(off));
        reseal_objects(&store);
        assert_eq!(fs::read(&path).unwrap()[0], PLAIN_VERSION, "plaintext after toggling off");
        assert_eq!(store.read_object(&full).unwrap(), "secret body\n");
        set_enc_override(None);
    }
}
