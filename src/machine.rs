use std::path::{Path, PathBuf};

fn store_root() -> PathBuf {
    std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    })
}

fn key_path() -> PathBuf {
    store_root().join("identity").join("machine.key")
}

fn manifest_path() -> PathBuf {
    store_root().join("manifest.json")
}

fn sig_path() -> PathBuf {
    store_root().join("manifest.sig")
}

const KEY_FILES: [&str; 3] = ["store.key", "refs/refs.key", "config.json"];

fn manifest_body_at(root: &Path) -> String {
    let mut lines = vec![
        "isohypse-manifest v1".to_string(),
        format!("storeroot {}", root.display()),
    ];
    for name in KEY_FILES {
        if let Ok(bytes) = std::fs::read(root.join(name)) {
            lines.push(format!("key {name} {}", blake3::hash(&bytes).to_hex()));
        }
    }
    lines.sort();
    let mut body = lines.join("\n");
    body.push('\n');
    body
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len() / 2)
        .map(|i| u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).ok())
        .collect()
}

pub fn refresh() -> Option<()> {
    let body = manifest_body_at(&store_root());
    let signature = isohypse_se::sign(&key_path(), body.as_bytes())?;
    crate::objects::write_private(&manifest_path(), body.as_bytes())?;
    crate::objects::write_private(&sig_path(), hex_encode(&signature).as_bytes())?;
    Some(())
}

pub fn check() -> Result<(), String> {
    if !isohypse_se::available() {
        return Err("Secure Enclave unavailable on this machine".to_string());
    }
    let body = std::fs::read(manifest_path()).map_err(|_| "store manifest missing".to_string())?;
    let sig_text = std::fs::read_to_string(sig_path()).map_err(|_| "manifest signature missing".to_string())?;
    let signature = hex_decode(sig_text.trim()).ok_or("manifest signature is malformed")?;
    if !isohypse_se::verify(&key_path(), &body, &signature) {
        return Err("manifest signature does not verify (possible planted or foreign store)".to_string());
    }
    let recorded = String::from_utf8_lossy(&body);
    for line in recorded.lines() {
        let Some(rest) = line.strip_prefix("key ") else { continue };
        let mut parts = rest.splitn(2, ' ');
        let (Some(name), Some(recorded_hash)) = (parts.next(), parts.next()) else { continue };
        if let Ok(bytes) = std::fs::read(store_root().join(name)) {
            if blake3::hash(&bytes).to_hex().to_string() != recorded_hash {
                return Err(format!("{name} changed since the store manifest was signed"));
            }
        }
    }
    Ok(())
}

fn latched_reason() -> Option<String> {
    crate::setup::Config::load().agent_disabled_reason
}

fn quarantine() {
    let root = store_root();
    let dest = root.join("quarantine");
    let _ = std::fs::create_dir_all(&dest);
    for name in ["manifest.json", "manifest.sig"] {
        let src = root.join(name);
        if src.exists() {
            let _ = std::fs::rename(&src, dest.join(format!("{name}.{}", std::process::id())));
        }
    }
}

fn react(reason: &str) {
    let mut config = crate::setup::Config::load();
    config.mode = "micro".to_string();
    config.agent_disabled_reason = Some(reason.to_string());
    let _ = config.save();
    quarantine();
    eprintln!(
        "machine: MASQUERADE SIGNAL — {reason}. Manifest quarantined, fell back to micro mode, agent mode disabled. Re-enable with `isohypse setup --reenable-agent` after review."
    );
}

pub fn reenable() -> Result<String, String> {
    let user = std::env::var("USER").map_err(|_| "cannot determine current user".to_string())?;
    eprintln!("Re-enabling agent mode after a masquerade signal. Authorize with your account password.");
    let status = std::process::Command::new("/usr/bin/dscl")
        .args([".", "-authonly", &user])
        .status()
        .map_err(|e| format!("cannot run authorization: {e}"))?;
    if !status.success() {
        return Err("authorization failed; agent mode stays disabled".to_string());
    }
    let mut config = crate::setup::Config::load();
    config.agent_disabled_reason = None;
    config.save()?;
    refresh();
    Ok("agent mode re-enabled; store manifest re-signed for the current store\n".to_string())
}

pub fn ensure() -> Result<(), String> {
    if let Some(reason) = latched_reason() {
        return Err(format!(
            "agent mode disabled: {reason}; re-enable with `isohypse setup --reenable-agent`"
        ));
    }
    if !isohypse_se::available() {
        use std::sync::Once;
        static WARN: Once = Once::new();
        WARN.call_once(|| {
            eprintln!("machine: Secure Enclave unavailable; store-integrity verification is disabled on this machine.");
        });
        return Ok(());
    }
    let manifest_missing = std::fs::metadata(manifest_path()).is_err();
    let store_established = std::fs::metadata(store_root().join("store.key")).is_ok();
    if manifest_missing {
        if store_established {
            let reason = "store manifest missing while the store already exists".to_string();
            react(&reason);
            return Err(reason);
        }
        refresh();
        return Ok(());
    }
    if let Err(reason) = check() {
        react(&reason);
        return Err(reason);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_lists_present_key_files_sorted() {
        let dir = std::env::temp_dir().join(format!("iso-manifest-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("store.key"), [7u8; 32]).unwrap();
        let body = manifest_body_at(&dir);
        assert!(body.starts_with("isohypse-manifest v1\n"));
        assert!(body.contains("key store.key "));
        assert!(!body.contains("refs/refs.key"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_round_trip() {
        let bytes = [0u8, 1, 15, 16, 200, 255];
        assert_eq!(hex_decode(&hex_encode(&bytes)).unwrap(), bytes.to_vec());
    }
}
