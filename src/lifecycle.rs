use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::objects;

pub const DEFAULT_INACTIVE_HOURS: u64 = 48;
pub const DEFAULT_COLD_DAYS: u64 = 15;
const LIFECYCLE_SEAL_CTX: &str = "isohypse lifecycle seal v1";
const LIFECYCLE_NAME_CTX: &str = "isohypse lifecycle name v1";
const ROOT_ENTRY: &str = "meta/root";

pub struct StoreConfig {
    pub inactive_hours: u64,
    pub cold_days: u64,
}

pub fn store_config() -> StoreConfig {
    let mut config = StoreConfig { inactive_hours: DEFAULT_INACTIVE_HOURS, cold_days: DEFAULT_COLD_DAYS };
    let Ok(text) = std::fs::read_to_string(objects::store_dir().join("config.json")) else {
        return config;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return config;
    };
    if let Some(hours) = value.get("inactive_hours").and_then(serde_json::Value::as_u64) {
        config.inactive_hours = hours;
    }
    if let Some(days) = value.get("cold_days").and_then(serde_json::Value::as_u64) {
        config.cold_days = days;
    }
    config
}

fn inactive_dir() -> PathBuf {
    objects::store_dir().join("inactive")
}

fn cold_dir() -> PathBuf {
    objects::store_dir().join("cold_storage")
}

fn ensure_private_dir(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    true
}

fn bundle_name(root: &Path) -> Option<String> {
    objects::opaque_name(LIFECYCLE_NAME_CTX, &root.to_string_lossy()).map(|name| format!("{name}.bundle"))
}

fn gzip(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Write;
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).ok()?;
    encoder.finish().ok()
}

fn gunzip(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::GzDecoder::new(bytes).read_to_end(&mut out).ok()?;
    Some(out)
}

fn pack_entries(entries: &[(String, Vec<u8>)]) -> Option<Vec<u8>> {
    let mut raw = Vec::new();
    for (name, data) in entries {
        raw.extend_from_slice(&(name.len() as u32).to_le_bytes());
        raw.extend_from_slice(name.as_bytes());
        raw.extend_from_slice(&(data.len() as u64).to_le_bytes());
        raw.extend_from_slice(data);
    }
    objects::seal_bytes(LIFECYCLE_SEAL_CTX, &gzip(&raw)?)
}

fn unpack_entries(sealed: &[u8]) -> Option<Vec<(String, Vec<u8>)>> {
    let raw = gunzip(&objects::open_bytes(LIFECYCLE_SEAL_CTX, sealed)?)?;
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at + 4 <= raw.len() {
        let name_len = u32::from_le_bytes([raw[at], raw[at + 1], raw[at + 2], raw[at + 3]]) as usize;
        at += 4;
        let name_end = at.checked_add(name_len)?;
        if name_end.checked_add(8)? > raw.len() {
            return None;
        }
        let name = String::from_utf8(raw[at..name_end].to_vec()).ok()?;
        at = name_end;
        let data_len = u64::from_le_bytes(raw[at..at + 8].try_into().ok()?) as usize;
        at += 8;
        let data_end = at.checked_add(data_len)?;
        if data_end > raw.len() {
            return None;
        }
        entries.push((name, raw[at..data_end].to_vec()));
        at = data_end;
    }
    Some(entries)
}

fn relative_name(store: &Path, path: &Path) -> Option<String> {
    path.strip_prefix(store).ok().map(|p| p.to_string_lossy().into_owned())
}

fn safe_entry_name(name: &str) -> bool {
    if name.starts_with('/') || name.contains("..") {
        return false;
    }
    ["workspaces/", "refs/", "vectors/", "journal/", "objects/"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

pub fn archive_repo(root: &Path) -> Result<(), String> {
    let name = bundle_name(root).ok_or_else(|| "store key unavailable".to_string())?;
    let store = objects::store_dir();
    let mut entries: Vec<(String, Vec<u8>)> =
        vec![(ROOT_ENTRY.to_string(), root.to_string_lossy().into_owned().into_bytes())];
    let mut removals: Vec<PathBuf> = Vec::new();
    let mut own_tags: HashSet<String> = HashSet::new();
    let mut foreign_tags: HashSet<String> = HashSet::new();
    let root_prefix = format!("{}/", root.display());
    if let Ok(dir) = std::fs::read_dir(objects::journal_dir()) {
        for entry in dir.flatten() {
            let path = entry.path();
            let Ok(bytes) = std::fs::read(&path) else { continue };
            let Some((file_path, tags)) = objects::journal_file_info(&bytes) else { continue };
            if file_path.starts_with(&root_prefix) {
                own_tags.extend(tags);
                if let Some(entry_name) = relative_name(&store, &path) {
                    entries.push((entry_name, bytes));
                    removals.push(path);
                }
            } else {
                foreign_tags.extend(tags);
            }
        }
    }
    if let Some(path) = objects::workspace_entry_path(root) {
        if let (Ok(bytes), Some(entry_name)) = (std::fs::read(&path), relative_name(&store, &path)) {
            entries.push((entry_name, bytes));
        }
    }
    if let Some(path) = crate::refs::cache_path(root) {
        if let (Ok(bytes), Some(entry_name)) = (std::fs::read(&path), relative_name(&store, &path)) {
            entries.push((entry_name, bytes));
            removals.push(path);
        }
    }
    for path in crate::daemon::vector_packs(root) {
        if let (Ok(bytes), Some(entry_name)) = (std::fs::read(&path), relative_name(&store, &path)) {
            entries.push((entry_name, bytes));
            removals.push(path);
        }
    }
    for tag in &own_tags {
        if tag.len() < 4 {
            continue;
        }
        let blob = objects::objects_dir().join(&tag[..2]).join(&tag[2..]);
        let Ok(bytes) = std::fs::read(&blob) else { continue };
        if let Some(entry_name) = relative_name(&store, &blob) {
            entries.push((entry_name, bytes));
        }
        if !foreign_tags.contains(tag) {
            removals.push(blob);
            removals.push(store.join("seen").join(tag));
        }
    }
    let sealed = pack_entries(&entries).ok_or_else(|| "store key unavailable".to_string())?;
    let dir = inactive_dir();
    if !ensure_private_dir(&dir) {
        return Err("cannot create inactive dir".to_string());
    }
    objects::write_private(&dir.join(&name), &sealed)
        .ok_or_else(|| "cannot write lifecycle bundle".to_string())?;
    for path in removals {
        let _ = std::fs::remove_file(path);
    }
    objects::forget_workspace(root);
    objects::runtime_unregister(root);
    Ok(())
}

fn demote(bundle: &Path) -> Result<(), String> {
    let bytes = std::fs::read(bundle).map_err(|e| e.to_string())?;
    let entries = unpack_entries(&bytes).ok_or_else(|| "unreadable bundle".to_string())?;
    let kept: Vec<(String, Vec<u8>)> = entries
        .into_iter()
        .filter(|(name, _)| name == ROOT_ENTRY || name.starts_with("journal/") || name.starts_with("objects/"))
        .collect();
    let sealed = pack_entries(&kept).ok_or_else(|| "store key unavailable".to_string())?;
    let dir = cold_dir();
    if !ensure_private_dir(&dir) {
        return Err("cannot create cold storage dir".to_string());
    }
    let name = bundle.file_name().ok_or_else(|| "bundle has no name".to_string())?;
    objects::write_private(&dir.join(name), &sealed)
        .ok_or_else(|| "cannot write cold bundle".to_string())?;
    std::fs::remove_file(bundle).map_err(|e| e.to_string())
}

pub fn restore(root: &Path) -> bool {
    let Some(name) = bundle_name(root) else { return false };
    let Some(bundle) = [inactive_dir().join(&name), cold_dir().join(&name)]
        .into_iter()
        .find(|path| path.is_file())
    else {
        return false;
    };
    let Ok(bytes) = std::fs::read(&bundle) else { return false };
    let Some(entries) = unpack_entries(&bytes) else { return false };
    let store = objects::store_dir();
    let mut restored = false;
    for (entry_name, data) in entries {
        if entry_name == ROOT_ENTRY || !safe_entry_name(&entry_name) {
            continue;
        }
        let target = store.join(&entry_name);
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if objects::write_private(&target, &data).is_some() {
            restored = true;
        }
    }
    if restored {
        let _ = std::fs::remove_file(&bundle);
        eprintln!("lifecycle: restored archived state for {}", root.display());
    }
    restored
}

pub fn sweep(is_root_live: &dyn Fn(&Path) -> bool) -> (usize, usize) {
    let config = store_config();
    let now = SystemTime::now();
    let mut archived = 0;
    let mut demoted = 0;
    for (root, modified) in objects::workspace_entries_with_mtime() {
        let idle = now
            .duration_since(modified)
            .map(|age| age.as_secs() > config.inactive_hours * 3600)
            .unwrap_or(false);
        if idle && !is_root_live(&root) && archive_repo(&root).is_ok() {
            archived += 1;
        }
    }
    if let Ok(dir) = std::fs::read_dir(inactive_dir()) {
        for entry in dir.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .map(|age| age.as_secs() > config.cold_days * 86_400)
                .unwrap_or(false);
            if stale && demote(&entry.path()).is_ok() {
                demoted += 1;
            }
        }
    }
    (archived, demoted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_restore_round_trip() {
        let root = std::env::temp_dir().join(format!("iso-lifecycle-{}", std::process::id()));
        let file_key = format!("{}/src/main.rs", root.display());
        let store = crate::objects::ObjectStore::open().expect("store");
        let tag = store.put("fn lifecycle_probe() {}").expect("put");
        store.journal_record(&file_key, &tag).expect("journal");
        crate::objects::persist_workspace(&root, true, true);
        archive_repo(&root).expect("archive");
        assert!(store.journal_entries(&file_key).is_empty());
        assert!(restore(&root));
        assert_eq!(store.journal_entries(&file_key), vec![tag.clone()]);
        let resolved = matches!(
            store.resolve(&tag),
            crate::objects::Resolution::Found(_, content) if content == "fn lifecycle_probe() {}"
        );
        assert!(resolved);
        crate::objects::forget_workspace(&root);
    }
}
