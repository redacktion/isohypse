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

fn ensure_gitignored(repo: &Path, entry: &str) {
    let path = repo.join(".gitignore");
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let bare = entry.trim_end_matches('/');
    if existing.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == entry || trimmed == bare
    }) {
        return;
    }
    let mut body = existing;
    if !body.is_empty() && !body.ends_with('\n') {
        body.push('\n');
    }
    body.push_str(entry);
    body.push('\n');
    let _ = std::fs::write(&path, body);
}

/// Point the process at a per-repo store when the machine is unconfigured or in
/// micro mode. A machine configured global (mode=global in the fixed home
/// config) or an explicit ISOHYPSE_STORE override keeps the global store.
pub fn resolve() {
    if std::env::var_os("ISOHYPSE_STORE").is_some() {
        return;
    }
    if machine_mode() == "global" {
        return;
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let repo = repo_root(&cwd);
    let store = repo.join(".isohypse-store");
    let _ = std::fs::create_dir_all(&store);
    ensure_gitignored(&repo, ".isohypse-store/");
    unsafe {
        std::env::set_var("ISOHYPSE_STORE", &store);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_root_walks_up_to_git() {
        let base = std::env::temp_dir().join(format!("iso-repo-{}", std::process::id()));
        let nested = base.join("a").join("b");
        let _ = std::fs::create_dir_all(base.join(".git"));
        let _ = std::fs::create_dir_all(&nested);
        assert_eq!(repo_root(&nested), base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn gitignore_appends_the_store_once() {
        let repo = std::env::temp_dir().join(format!("iso-gi-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&repo);
        ensure_gitignored(&repo, ".isohypse-store/");
        ensure_gitignored(&repo, ".isohypse-store/");
        let body = std::fs::read_to_string(repo.join(".gitignore")).unwrap();
        assert_eq!(body.matches(".isohypse-store").count(), 1);
        let _ = std::fs::remove_dir_all(&repo);
    }
}
