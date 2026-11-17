use std::path::PathBuf;
use std::process::Command;

use isohypse::daemon;
use isohypse::prompt::{FORMAT_REFERENCE, HELP};
use serde_json::json;

const DAEMON_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

fn usage() -> ! {
    eprint!("{HELP}");
    std::process::exit(2);
}

fn cwd() -> PathBuf {
    std::env::current_dir().expect("current directory")
}

/// Canonicalize a workspace root, tolerating one that has just been deleted.
/// /tmp/x and /private/tmp/x are one root to the supervisor, and a worker that
/// outlives its directory still has to be addressable and stoppable.
fn resolve_root(path: &std::path::Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }
    match (path.parent().and_then(|p| std::fs::canonicalize(p).ok()), path.file_name()) {
        (Some(parent), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

fn read_hidden(prompt: &str) -> Result<String, String> {
    use std::io::{BufRead, Write};
    use std::os::unix::io::AsRawFd;
    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    let fd = std::io::stdin().as_raw_fd();
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut term) } != 0 {
        return Err("cannot access the terminal for password entry".to_string());
    }
    let saved = term;
    term.c_lflag &= !libc::ECHO;
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &term);
    }
    let mut password = String::new();
    let result = std::io::stdin().lock().read_line(&mut password);
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &saved);
    }
    eprintln!();
    result.map_err(|e| e.to_string())?;
    Ok(password.trim_end_matches(['\n', '\r']).to_string())
}

fn authorize_stop() -> Result<Option<String>, String> {
    let live = isohypse::objects::runtime_list()
        .into_iter()
        .filter(|(_, socket)| daemon::is_live(socket))
        .count();

    use std::os::unix::io::AsRawFd;
    let interactive = unsafe { libc::isatty(std::io::stdin().as_raw_fd()) } == 1;
    if !interactive {
        return Err(format!(
            "{live} live workspace(s) are in use; stopping needs interactive authorization at a terminal — agents cannot stop the daemon"
        ));
    }
    eprintln!("Stopping {live} live workspace(s) would cut off every connected agent.");
    let password = read_hidden("Authorize with your account password: ")?;
    if password.is_empty() {
        return Err("no password entered; the daemon is still running".to_string());
    }
    Ok(Some(password))
}

fn allowed_flags(command: &str) -> Option<&'static [&'static str]> {
    Some(match command {
        "daemon" | "state.reload" | "build" => &["--no-lsp", "--no-semantic"],
        "state.up" => &["--no-lsp", "--no-semantic", "--reindex", "--foreground"],
        "worker" => &["--lsp", "--semantic"],
        "services" => &["--semantic"],
        "state.down" | "state.status" | "state.socket" | "state.stop" | "context.prompt" | "update.check" => &[],
        "session" => &["--stdio"],
        "call" | "hook" => &[],
        "setup" => &["--agent", "--global", "--micro", "--inactive-hours", "--cold-days", "--encrypt", "--reenable-agent"],
        _ => return None,
    })
}

fn reject_unknown_flags(args: &[String], allowed: &'static [&'static str]) -> Result<(), String> {
    for argument in args {
        if argument.starts_with('-') && !allowed.contains(&argument.as_str()) {
            return Err(if allowed.is_empty() {
                format!("unknown flag {argument}; this command takes no flags")
            } else {
                format!("unknown flag {argument}; valid flags: {}", allowed.join(", "))
            });
        }
    }
    Ok(())
}

fn subcommand_help(command: &str) -> Option<&'static str> {
    Some(match command {
        "call" => "Run one batch in a fresh session: isohypse call <<'EOF' {\"steps\":[...]} EOF. Reads the batch from stdin, or from the first argument. Prints the output, exits non-zero on failure.",
        "hook" => "Answer an agent harness PreToolUse event on stdin. Denies Read, Grep, Glob, Edit, MultiEdit, Write and NotebookEdit inside a live Isohypse workspace and points at `isohypse call`.",
        "session" => "Persistent session transport. The host opens once, matches results by request ID, and closes when finished.",
        "context.prompt" => FORMAT_REFERENCE,
        "build" => "Build, sign, and reload the runtime. Host maintenance only.",
        "state.status" => "Report workspace state.",
        "state.socket" => "Print the live workspace socket path for a root; exits non-zero when nothing answers.",
        "state.up" => "Start or join the selected workspace. Preserve its authenticated identity.",
        "state.down" => "Stop the selected workspace worker.",
        "state.reload" => "Reload through the authenticated supervisor.",
        "state.stop" => "Stop the supervisor and its workers. Requires interactive account authentication.",
        "update.check" => "Check available runtime releases.",
        "setup" => "Configure storage, identity, and access policy.",
        _ => return None,
    })
}
fn session_bridge(socket: &std::path::Path) -> Result<String, String> {
    use std::os::unix::net::UnixStream;
    let stream = UnixStream::connect(socket).map_err(|e| format!("cannot connect to session socket: {e}"))?;
    {
        use std::os::unix::io::AsRawFd;
        if !isohypse::trust::peer_signature_ok(stream.as_raw_fd()) {
            return Err("session socket failed code-signature verification".to_string());
        }
    }
    let mut to_socket = stream.try_clone().map_err(|e| e.to_string())?;
    let mut from_socket = stream;
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let _ = std::io::copy(&mut stdin, &mut to_socket);
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = std::io::stdout();
    // The server can close while stdin is still open (close/reload/disconnect).
    // Do not join a stdin reader that may remain blocked indefinitely.
    std::io::copy(&mut from_socket, &mut stdout).map_err(|e| e.to_string())?;
    Ok(String::new())
}









fn start_detached(root: PathBuf, lsp: bool, semantic: bool, handover: bool) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    start_detached_from(exe, root, lsp, semantic, handover)
}

/// Launch a supervisor from an explicit image. A stateful reload must start the
/// newly built binary, which is not this process's own executable.
fn start_detached_from(exe: PathBuf, root: PathBuf, lsp: bool, semantic: bool, handover: bool) -> Result<String, String> {
    let socket = daemon::socket_path(&root);
    if !handover && daemon::is_live(&socket) {
        return Ok(format!("isohypse already up on {}\n", socket.display()));
    }
    if !handover {
        let _ = std::fs::remove_file(&socket);
    }
    let log_path = daemon::store_root().join("daemon.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
    }
    isohypse::trace::rotate_if_large(&log_path, DAEMON_LOG_MAX_BYTES);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| e.to_string())?;
    let log_err = log.try_clone().map_err(|e| e.to_string())?;
    let mut command = std::process::Command::new(exe);
    command.arg("daemon").arg(&root);
    if !lsp {
        command.arg("--no-lsp");
    }
    if !semantic {
        command.arg("--no-semantic");
    }
    for var in [
        "DYLD_INSERT_LIBRARIES",
        "DYLD_LIBRARY_PATH",
        "DYLD_FRAMEWORK_PATH",
        "LD_PRELOAD",
        "LD_LIBRARY_PATH",
    ] {
        command.env_remove(var);
    }
    if handover {
        command.env("ISOHYPSE_HANDOVER", "1");
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log))
        .stderr(std::process::Stdio::from(log_err));
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let pid = child.id();
    for _ in 0..100 {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("isohypse startup failed: process {pid} exited with {status}; see {}", log_path.display()));
        }
        if daemon::supervisor_reachable().is_some() && daemon::is_live(&socket) {
            return Ok(format!("isohypse up (pid {pid}) on {}\n", socket.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(format!("isohypse startup unconfirmed after 10 seconds (pid {pid}); inspect {} before retrying", log_path.display()))
}

fn build_and_reload(args: &[String]) -> Result<String, String> {
    let lsp = !args.iter().any(|arg| arg == "--no-lsp");
    let semantic = !args.iter().any(|arg| arg == "--no-semantic");
    let root = args.iter().find(|arg| !arg.starts_with("--")).map(PathBuf::from).unwrap_or_else(cwd);
    let root = resolve_root(&root);
    let mut out = isohypse::output::Phases::new();
    out.phase("Build");
    out.working("Compiling the release binary");
    let build = Command::new("cargo").args(["build", "--release"]).current_dir(&root).output().map_err(|error| format!("build could not start: {error}"))?;
    if !build.status.success() {
        out.fail("cargo build --release failed");
        eprint!("{}", String::from_utf8_lossy(&build.stderr));
        return Err(format!("cargo build --release failed with {}", build.status));
    }
    out.done("Release binary built.");
    let running = std::env::current_exe().map_err(|error| format!("cannot locate running binary: {error}"))?;
    let install = if running.extension().is_some_and(|ext| ext.to_string_lossy().starts_with("build-")) {
        running.with_extension("")
    } else { running };
    let built = root.join("target/release/isohypse");
    let nonce = format!("{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| e.to_string())?.as_nanos());
    let staged = install.with_extension(format!("build-{nonce}"));
    out.phase("Stage");
    out.working("Copying the binary next to the install path");
    std::fs::copy(&built, &staged).map_err(|error| format!("cannot stage {} at {}: {error}", built.display(), staged.display()))?;
    out.done("Binary staged.");
    let identity = std::env::var("ISOHYPSE_CODESIGN_IDENTITY").unwrap_or_else(|_| "isohypse-local".to_string());
    out.phase("Sign");
    out.working(&format!("Signing with {identity}"));
    let signed = Command::new("/usr/bin/codesign").args(["--force", "--sign"]).arg(&identity).args(["--identifier", "isohypse"]).arg(&staged).output().map_err(|error| format!("codesign could not start: {error}"))?;
    if !signed.status.success() {
        let _ = std::fs::remove_file(&staged);
        out.fail(&format!("codesign failed for identity {identity}"));
        eprint!("{}", String::from_utf8_lossy(&signed.stderr));
        return Err(format!("codesign failed for identity {identity}: {}", signed.status));
    }
    out.done("Binary signed.");
    // Stateful handover: install the new image, then start a replacement
    // supervisor. It warms every worker before promoting the control and
    // workspace sockets, so no client observes a cold or dead endpoint, and the
    // running supervisor retires only once that promotion is visible.
    out.phase("Install");
    out.working(&format!("Promoting the staged image to {}", install.display()));
    promote_release(&staged, &install, &nonce)?;
    out.done("Release installed.");
    out.phase("Reload");
    out.working("Starting a replacement supervisor and retiring the running one");
    daemon::supersede_supervisor();

    let output = start_detached_from(staged.clone(), root, lsp, semantic, true)?;
    out.done("Supervisor handed over.");
    out.end();
    Ok(format!("release build installed and signed with {identity}; {output}"))
}

// Running signed images must keep their vnode and a reachable path. Workers also
// retain the staged path for crash recovery. Only the public symlink is replaced.
fn promote_release(staged: &std::path::Path, install: &std::path::Path, nonce: &str) -> Result<(), String> {
    let link = install.with_extension(format!("link-{nonce}"));
    let retained = install.with_extension(format!("build-retained-{nonce}"));
    std::os::unix::fs::symlink(staged, &link).map_err(|e| e.to_string())?;
    let legacy = match std::fs::symlink_metadata(install) {
        Ok(meta) if meta.is_file() => true,
        Ok(meta) if meta.file_type().is_symlink() => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        _ => { let _ = std::fs::remove_file(&link); return Err("install target is not a file or symlink".into()); }
    };
    if legacy {
        if retained.exists() { let _ = std::fs::remove_file(&link); return Err("retained image already exists".into()); }
        if let Err(error) = std::fs::rename(install, &retained) {
            let _ = std::fs::remove_file(&link);
            return Err(error.to_string());
        }
    }
    if let Err(error) = std::fs::rename(&link, install) {
        if legacy { let _ = std::fs::rename(&retained, install); }
        let _ = std::fs::remove_file(&link);
        return Err(error.to_string());
    }
    Ok(())
}

#[test]
fn release_promotion_preserves_running_images() {
    let root = std::env::temp_dir().join(format!("isohypse-promotion-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let install = root.join("isohypse");
    let first = root.join("isohypse.build-first");
    let second = root.join("isohypse.build-second");
    std::fs::write(&install, "legacy").unwrap();
    std::fs::write(&first, "first").unwrap();
    std::fs::write(&second, "second").unwrap();
    promote_release(&first, &install, "first").unwrap();
    promote_release(&second, &install, "second").unwrap();
    let __ok0: bool = (std::fs::read_to_string(&install).unwrap()) == ("second");
    let __ok1: bool = (std::fs::read_to_string(&first).unwrap()) == ("first");
    let __ok2: bool = (std::fs::read_to_string(root.join("isohypse.build-retained-first")).unwrap()) == ("legacy");
    let __ok3: bool = (std::fs::canonicalize(&install).unwrap()) == (std::fs::canonicalize(&second).unwrap());
    std::fs::remove_dir_all(root).unwrap();

    let failed: Vec<&str> = [(__ok0, "std::fs::read_to_string(&install).unwrap() == \"second\""), (__ok1, "std::fs::read_to_string(&first).unwrap() == \"first\""), (__ok2, "std::fs::read_to_string(root.join(\"isohypse.build-retained-first\")).un"), (__ok3, "std::fs::canonicalize(&install).unwrap() == std::fs::canonicalize(&sec")].iter().filter(|(ok, _)| !ok).map(|(_, label)| *label).collect();
    assert_eq!(failed, Vec::<&str>::new(), "these checks failed");
}

fn update_check() -> Result<String, String> {
    let current = env!("CARGO_PKG_VERSION");
    let output = Command::new("/usr/bin/curl")
        .args(["--fail", "--silent", "--show-error", "--max-time", "5", "https://api.github.com/repos/redacktion/isohypse/releases/latest"])
        .output()
        .map_err(|e| format!("update check unavailable: {e}"))?;
    if !output.status.success() {
        return Err(format!("update check failed with {}", output.status));
    }
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|e| format!("update metadata was invalid: {e}"))?;
    let latest = payload.get("tag_name").and_then(|v| v.as_str()).unwrap_or("unknown");
    let url = payload.get("html_url").and_then(|v| v.as_str()).unwrap_or("");
    Ok(format!("installed: {current}\nlatest: {latest}\nrelease: {url}\n"))
}


fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "--json") {
        daemon::JSON_OUTPUT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let args: Vec<String> = raw.into_iter().filter(|a| a != "--json").collect();
    let Some(first) = args.first() else { usage() };
    if first == "-h" || first == "--help" || first == "help" { print!("{HELP}"); return; }
    if first != "setup" && first != "hook" {
        if let Err(error) = isohypse::store::resolve() { eprintln!("isohypse: {error}"); std::process::exit(1); }
    }
    let (command, rest): (String, Vec<String>) = match first.as_str() {
        "context" | "state" | "update" => {
            let Some(action) = args.get(1) else { usage() };
            (format!("{first}.{action}"), args[2..].to_vec())
        }
        _ => (first.clone(), args[1..].to_vec()),
    };
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        if let Some(help) = subcommand_help(&command) { println!("{help}"); std::process::exit(0); }
        usage()
    }
    if let Some(allowed) = allowed_flags(command.as_str()) {
        if let Err(message) = reject_unknown_flags(&rest, allowed) { eprintln!("isohypse: {message}"); std::process::exit(1); }
    }
    let outcome: Result<String, String> = match command.as_str() {
        "state.status" => {
            let socket = daemon::find_socket(&cwd()).ok_or_else(|| "no live daemon found".to_string());
            socket.and_then(|s| daemon::request(&s, &json!({"op": "state.status"})))
        }
        // Harness-facing liveness probe. A socket file is not evidence that a
        // daemon is alive, and the sockets no longer sit inside the workspace to
        // be counted, so this answers by connecting and exits non-zero otherwise.
        "state.socket" => {
            let root = rest.first().map(PathBuf::from).unwrap_or_else(cwd);
            let root = resolve_root(&root);
            let socket = daemon::socket_path(&root);
            if daemon::is_live(&socket) {
                Ok(format!("{}\n", socket.display()))
            } else {
                Err(format!("no live daemon for {} ({} does not answer)", root.display(), socket.display()))
            }
        }
        "state.stop" => authorize_stop().and_then(|password| {
            let mut body = json!({"op": "stop"});
            if let Some(pw) = &password { body["password"] = json!(pw); }
            let stopped = match daemon::supervisor_reachable() {
                Some(control) => daemon::request(&control, &body),
                None => daemon::recover_supervisor(password.as_deref().unwrap_or_default()),
            };
            if stopped.is_ok() {
                daemon::clear_all_enforcement();
            }
            stopped
        }),
        "daemon" => {
            let lsp = !rest.iter().any(|a| a == "--no-lsp");
            let semantic = !rest.iter().any(|a| a == "--no-semantic");
            let root = rest.iter().find(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or_else(cwd);
            daemon::run_supervisor(root, lsp, semantic).map(|_| String::new())
        }
        "worker" => {
            let lsp = rest.iter().any(|a| a == "--lsp");
            let semantic = rest.iter().any(|a| a == "--semantic");
            let root = rest.iter().find(|a| *a != "--lsp" && *a != "--semantic").map(PathBuf::from).unwrap_or_else(cwd);
            daemon::run_worker(root, lsp, semantic).map(|_| String::new())
        }
        "services" => daemon::run_services(rest.iter().any(|a| a == "--semantic")).map(|_| String::new()),
        "state.up" => {
            let reindex = rest.iter().any(|a| a == "--reindex");
            let foreground = rest.iter().any(|a| a == "--foreground");
            let lsp = !rest.iter().any(|a| a == "--no-lsp");
            let semantic = !rest.iter().any(|a| a == "--no-semantic");
            let root = rest.iter().find(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or_else(cwd);
            let root = resolve_root(&root);
            if reindex {
                let socket = daemon::find_socket(&cwd()).ok_or_else(|| "no live daemon found".to_string());
                socket.and_then(|s| daemon::request(&s, &json!({"op": "state.up", "reindex": true, "workspace": root.to_string_lossy()})))
            } else if foreground {
                daemon::run_supervisor(root, lsp, semantic).map(|_| String::new())
            } else {
                let socket = daemon::socket_path(&root);
                if daemon::is_live(&socket) {
                    Ok(format!("isohypse already up on {}\n", socket.display()))
                } else if let Some(control) = daemon::supervisor_reachable() {
                    daemon::request(&control, &json!({"op": "spawn", "root": root.to_string_lossy()}))
                } else {
                    start_detached(root, lsp, semantic, false)
                }
            }
        }
        "build" => build_and_reload(&rest),
        "state.down" => {
            let root = rest.first().map(PathBuf::from).unwrap_or_else(cwd);
            let root = resolve_root(&root);
            match daemon::supervisor_reachable() {
                Some(control) => daemon::request(&control, &json!({"op": "kill", "root": root.to_string_lossy()})),
                None => Err("no live supervisor found".to_string()),
            }
        }
        "update.check" => update_check(),
        "context.prompt" => Ok(FORMAT_REFERENCE.to_string()),
        "call" => {
            let input = match rest.first() {
                Some(batch) => Ok(batch.clone()),
                None => {
                    let mut text = String::new();
                    std::io::Read::read_to_string(&mut std::io::Read::take(std::io::stdin(), isohypse::call::MAX_INPUT_BYTES as u64 + 1), &mut text).map(|_| text).map_err(|e| format!("cannot read stdin: {e}"))
                }
            };
            input.and_then(|batch| match daemon::find_socket(&cwd()).and_then(|s| daemon::session_socket_beside(&s)) {
                Some(socket) => isohypse::call::run(&socket, &batch).map_err(|e| e.to_string()),
                None => Err("no live daemon found from here upward; run `isohypse state up .`".to_string()),
            })
        }
        "hook" => {
            let mut text = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut text);
            let off = std::env::var("ISOHYPSE_HOOK").is_ok_and(|v| v == "off");
            Ok(isohypse::hook::respond(&text, off, &isohypse::hook::FsProbe))
        }
        "session" => match daemon::find_socket(&cwd()).and_then(|s| daemon::session_socket_beside(&s)) {
            Some(socket) => session_bridge(&socket),
            None => Err("no live daemon found from here upward".to_string()),
        },
        "setup" => isohypse::setup::run(&rest),
        "state.reload" => {
            let lsp = !rest.iter().any(|a| a == "--no-lsp");
            let semantic = !rest.iter().any(|a| a == "--no-semantic");
            let root = std::fs::canonicalize(cwd()).unwrap_or_else(|_| cwd());
            // Stateful reload: the replacement supervisor warms every workspace
            // with this image and then takes over the sockets. The running
            // supervisor retires only once the new control socket is promoted.
            daemon::supersede_supervisor();

            start_detached(root, lsp, semantic, true)
        }
        _ => usage(),
    };
    match outcome {
        Ok(output) => print!("{output}"),
        Err(message) => { eprintln!("isohypse: {message}"); std::process::exit(1); }
    }
}
