//! Validation sandbox. `sandbox_argv` is pure and tested everywhere. The
//! behavior tests run only on macOS, where sandbox-exec exists.

use std::path::Path;

use isohypse::buildspec::{sandbox_argv, SandboxError, SandboxMode, SandboxPaths};

fn paths() -> SandboxPaths<'static> {
    SandboxPaths { root: Path::new("/repo"), tmp: Path::new("/tmp"), home: "/home/u" }
}

#[test]
fn off_runs_the_command_under_plain_sh() {
    assert_eq!(sandbox_argv(SandboxMode::Off, true, &paths(), "ls"), Ok(vec!["sh".to_string(), "-c".to_string(), "ls".to_string()]));
}

#[test]
fn off_works_on_every_platform() {
    assert!(sandbox_argv(SandboxMode::Off, false, &paths(), "ls").is_ok());
}

#[test]
fn a_sandbox_mode_off_macos_is_refused() {
    let got: Vec<Result<Vec<String>, SandboxError>> = [SandboxMode::WorkspaceWrite, SandboxMode::WorkspaceWriteNoNetwork].iter().map(|m| sandbox_argv(*m, false, &paths(), "ls")).collect();
    assert_eq!(got, vec![Err(SandboxError::UnsupportedPlatform), Err(SandboxError::UnsupportedPlatform)]);
}

fn profile_of(mode: SandboxMode) -> String {
    let argv = sandbox_argv(mode, true, &paths(), "ls").unwrap();
    let at = argv.iter().position(|a| a == "-p").unwrap();
    argv[at + 1].clone()
}

#[test]
fn workspace_write_profile_does_not_deny_the_network() {
    assert!(!profile_of(SandboxMode::WorkspaceWrite).contains("(deny network*)"));
}

#[test]
fn no_network_profile_denies_the_network() {
    assert!(profile_of(SandboxMode::WorkspaceWriteNoNetwork).contains("(deny network*)"));
}

#[test]
fn both_sandbox_profiles_deny_writes_by_default() {
    let got: Vec<bool> = [SandboxMode::WorkspaceWrite, SandboxMode::WorkspaceWriteNoNetwork].iter().map(|m| profile_of(*m).contains("(deny file-write*)")).collect();
    assert_eq!(got, vec![true, true]);
}

#[test]
fn sandbox_argv_passes_the_paths_as_parameters() {
    let argv = sandbox_argv(SandboxMode::WorkspaceWrite, true, &paths(), "ls").unwrap();
    let params: Vec<&String> = argv.iter().filter(|a| a.contains('=') && !a.starts_with('(')).collect();
    assert_eq!(params, vec!["ROOT=/repo", "TMP=/tmp", "HOME=/home/u"]);
}

#[test]
fn sandbox_argv_ends_with_the_command_under_sh() {
    let argv = sandbox_argv(SandboxMode::WorkspaceWrite, true, &paths(), "echo hi").unwrap();
    assert_eq!(argv[argv.len() - 3..], ["sh", "-c", "echo hi"]);
}

#[cfg(target_os = "macos")]
mod macos {
    use std::fs;

    use isohypse::buildspec::{run_command_in, SandboxMode};

    struct Fixture {
        root: std::path::PathBuf,
        outside: std::path::PathBuf,
    }

    fn fixture(name: &str) -> Fixture {
        let root = fs::canonicalize(std::env::temp_dir()).unwrap().join(format!("isohypse-sandbox-{name}-{}", std::process::id()));
        let outside = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(format!(".isohypse-sandbox-probe-{name}-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let _ = fs::remove_file(&outside);
        Fixture { root, outside }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.outside);
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn ok(f: &Fixture, command: &str, mode: SandboxMode) -> bool {
        run_command_in(&f.root, command, "test", None, mode).ok
    }

    #[test]
    fn off_allows_a_write_outside_the_root() {
        let f = fixture("off");
        assert!(ok(&f, &format!("echo x > {}", f.outside.display()), SandboxMode::Off));
    }

    #[test]
    fn workspace_write_allows_a_write_inside_the_root() {
        let f = fixture("inside");
        assert!(ok(&f, "echo x > inside.txt", SandboxMode::WorkspaceWrite));
    }

    #[test]
    fn workspace_write_refuses_a_write_outside_the_root() {
        let f = fixture("outside");
        assert!(!ok(&f, &format!("echo x > {}", f.outside.display()), SandboxMode::WorkspaceWrite));
    }

    #[test]
    fn workspace_write_leaves_the_refused_file_absent() {
        let f = fixture("absent");
        ok(&f, &format!("echo x > {}", f.outside.display()), SandboxMode::WorkspaceWrite);
        assert!(!f.outside.exists());
    }

    #[test]
    fn workspace_write_allows_reads_outside_the_root() {
        let f = fixture("read");
        assert!(ok(&f, "cat /etc/hosts > /dev/null", SandboxMode::WorkspaceWrite));
    }

    #[test]
    fn no_network_allows_a_write_inside_the_root() {
        let f = fixture("nonet");
        assert!(ok(&f, "echo x > inside.txt", SandboxMode::WorkspaceWriteNoNetwork));
    }
}
