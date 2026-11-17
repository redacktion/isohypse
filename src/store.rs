use std::path::{Path, PathBuf};

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

fn global_config_path() -> PathBuf {
    home().join(".isohypse").join("config.json")
}

fn machine_mode() -> String {
    std::fs::read_to_string(global_config_path())
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| value.get("mode").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_else(|| "micro".to_string())
}

fn repo_root(start: &Path) -> PathBuf {
    let mut dir = start;
    loop {
        if dir.join(".git").exists() {
            return dir.to_path_buf();
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return start.to_path_buf(),
        }
    }
}

/// Point the process at a per-workspace store when the machine is unconfigured
/// or in micro mode. A machine configured global (mode=global in the fixed home
/// config) or an explicit ISOHYPSE_STORE override keeps the global store.
///
/// Micro isolation is preserved, but it lives under the isohypse home: nothing is
/// written into the workspace, and no `.gitignore` is edited to hide it.
pub fn resolve() -> Result<(), String> {
    if std::env::var_os("ISOHYPSE_STORE").is_some() {
        return Ok(());
    }
    if machine_mode() == "global" {
        return Ok(());
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo = repo_root(&cwd);
    let store = workspace_store_dir(&repo);
    std::fs::create_dir_all(&store).map_err(|e| format!("cannot create {}: {e}", store.display()))?;
    unsafe { std::env::set_var("ISOHYPSE_STORE", &store); }
    Ok(())
}

/// The micro-mode store for one workspace. Keyed by the canonical repo root, the
/// same opaque key the daemon uses for enforce policy and the warm index, so a
/// workspace has one identity wherever isohypse keeps state about it.
fn workspace_store_dir(repo: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(repo).unwrap_or_else(|_| repo.to_path_buf());
    let key = blake3::hash(canonical.to_string_lossy().as_bytes()).to_hex().to_string();
    home().join(".isohypse").join("workspaces").join(key)
}

// ── unit: store root discovery ─────────────────────────────────────────────
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_walks_up_to_git() {
        let base = std::env::temp_dir().join(format!("iso-repo-{}", crate::test_store::unique()));
        let nested = base.join("a").join("b");
        let _ = std::fs::create_dir_all(base.join(".git"));
        let _ = std::fs::create_dir_all(&nested);
        assert_eq!(repo_root(&nested), base);
        let _ = std::fs::remove_dir_all(&base);
    }

}
