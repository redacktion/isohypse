//! Validation sandbox. `sandbox_argv` is pure and tested everywhere. The
//! behavior tests run only on macOS, where sandbox-exec exists.

use std::path::Path;

use isohypse::buildspec::{sandbox_argv, SandboxError, SandboxMode, SandboxPaths};

fn paths() -> SandboxPaths<'static> {
    SandboxPaths { root: Path::new("/repo"), tmp: Path::new("/tmp"), home: "/home/u" }
}



#[test]
fn a_sandbox_mode_off_macos_is_refused() {
    let got: Vec<Result<Vec<String>, SandboxError>> = [SandboxMode::WorkspaceWrite, SandboxMode::WorkspaceWriteNoNetwork].iter().map(|m| sandbox_argv(*m, false, &paths(), "ls")).collect();
    assert_eq!(got, vec![Err(SandboxError::UnsupportedPlatform), Err(SandboxError::UnsupportedPlatform)]);
}

#[cfg(target_os = "macos")]
mod macos {
    use std::fs;

    use std::net::TcpListener;

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
    fn workspace_write_refuses_a_write_outside_the_root() {
        let f = fixture("outside");
        assert!(!ok(&f, &format!("echo x > {}", f.outside.display()), SandboxMode::WorkspaceWrite));
    }



    fn connects(mode: SandboxMode, name: &str) -> bool {
        let f = fixture(name);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        ok(&f, &format!("/usr/bin/nc -z 127.0.0.1 {port}"), mode)
    }


    #[test]
    fn no_network_refuses_a_local_tcp_connection() {
        assert!(!connects(SandboxMode::WorkspaceWriteNoNetwork, "tcp-refused"));
    }
}
