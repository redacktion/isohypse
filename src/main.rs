use std::io::Read;
use std::path::PathBuf;

use isohypse::daemon;
use isohypse::patcher::Patcher;
use isohypse::prompt::FORMAT_REFERENCE;
use isohypse::render;
use serde_json::json;

const DAEMON_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

fn usage() -> ! {
    eprintln!("usage: isohypse <category> <action> [args]   (categories: context mutate verify state macro; plus multi-op, session, setup)");
    eprintln!("  context read [--outline] [--max-bytes N] <targets...>   exact bytes + tag; inline ranges a.rs:10-40,90-120");
    eprintln!("  context explore <query...> | <from> <to>   symbols, callers, blast radius; two args = call path");
    eprintln!("  context find [--name] [--any] <text...>    substring search over tracked source");
    eprintln!("  context log [path]                         versions of a file; no path = workspace changelog");
    eprintln!("  context prompt                             print the op-grammar reference");
    eprintln!("  mutate edit --validate CMD|none [--diff]   anchored edits from JSON on stdin (symbol/lines/block)");
    eprintln!("  mutate create <path>                       author a new file from stdin");
    eprintln!("  mutate undo <path> [TAG] [--recover]       restore a recorded version");
    eprintln!("  verify build                               run the workspace build");
    eprintln!("  verify check                               validate mutate.edit JSON without writing");
    eprintln!("  verify diagnose [path]                     parse and reference problems");
    eprintln!("  state status | get <tag> | put             daemon state / object store");
    eprintln!("  state up [root] [--reindex] [--foreground] start or ensure the tree (idempotent)");
    eprintln!("  state down [root] | reload | stop          lifecycle (stop is password-gated while live)");
    eprintln!("  macro save <name> | run <name> | list      named step sequences (steps JSON on stdin for save)");
    eprintln!("  multi-op [--validate CMD|none] [--diff]    ordered steps, one atomic transaction (JSON on stdin)");
    eprintln!("  session                                    NDJSON bridge (session.open / session.command / ...)");
    eprintln!("  setup [--agent] [--global|--micro] [--inactive-hours N] [--cold-days N] [--encrypt all|none|LIST]   configure the store");
    std::process::exit(2);
}

fn cwd() -> PathBuf {
    std::env::current_dir().expect("current directory")
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
    if live == 0 {
        return Ok(None);
    }
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
        "context.read" => &["--outline", "--max-bytes"],
        "context.find" => &["--name", "--workspace", "-w", "--max", "--path", "--any"],
        "context.explore" => &["--workspace", "-w", "--max", "--path", "--full"],
        "mutate.edit" | "multi-op" => &["--validate", "--diff", "--workspace", "-w"],
        "mutate.undo" => &["--recover"],
        "daemon" | "state.reload" => &["--no-lsp", "--no-semantic"],
        "state.up" => &["--no-lsp", "--no-semantic", "--reindex", "--foreground"],
        "worker" => &["--lsp", "--semantic"],
        "services" => &["--semantic"],
        "state.down" | "state.status" | "verify.build" | "verify.diagnose" => &["--workspace", "-w"],
        "mutate.create" | "verify.check" | "context.log" | "context.prompt" | "state.get"
        | "state.put" | "macro.save" | "macro.run" | "macro.list" | "session" | "state.stop" => &[],
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
        "context.read" => "usage: isohypse context read [--outline] [--max-bytes N] <targets...>\n  print files as [PATH#TAG] headers plus N:TEXT rows.\n  a target is a path, optionally with inline ranges: src/a.rs:10-40,90-120.\n  --outline collapses bodies to signatures; --max-bytes caps output (capped lines stay unseen).",
        "context.find" => "usage: isohypse context find [flags] <text...>\n  substring search over tracked source (needs the daemon). smart-case.\n  --name matches file paths; --any = each argument its own pattern; --path PREFIX restricts; --max N caps.",
        "context.explore" => "usage: isohypse context explore [flags] <query...>\n  symbols with source, callers, and blast radius (needs the daemon).\n  two bare arguments are treated as <from> <to> and return the shortest call path.\n  --path PREFIX restrict; --max N cap; --full full tagged source.",
        "context.log" => "usage: isohypse context log [path]\n  recorded versions of a file; with no path, the workspace changelog (newest first).",
        "context.prompt" => "usage: isohypse context prompt\n  print the op-grammar reference (isohypse.schema_v0.1.md is the full spec).",
        "mutate.edit" => "usage: isohypse mutate edit --validate CMD|none [--diff] [json-file]\n  anchored edits from JSON on stdin (or a file): {\"path\",\"tag\",\"edits\":[...]} or an array of those.\n  anchors: {\"symbol\":name} | {\"lines\":[a,b]} | {\"block\":N}; actions: replace/insert/delete/move/remove.\n  --validate CMD runs after writing and reverts everything on failure; --validate none waives it.",
        "mutate.create" => "usage: isohypse mutate create <path>\n  author a new file from stdin and record a tag; fails if it already exists.",
        "mutate.undo" => "usage: isohypse mutate undo <path> [TAG] [--recover]\n  restore the previous recorded version, a specific TAG prefix, or the last parse-valid with --recover.",
        "verify.build" => "usage: isohypse verify build [--workspace SEL]\n  run the repo's detected build, or a .isohypse.build override.",
        "verify.check" => "usage: isohypse verify check [json-file]\n  validate mutate.edit JSON end to end without writing; prints each file's diff.",
        "verify.diagnose" => "usage: isohypse verify diagnose [path]\n  tree-sitter parse errors for a file plus the workspace build result.",
        "state.status" => "usage: isohypse state status\n  daemon root, index counts, watcher, refs, semantic state, uptime.",
        "state.up" => "usage: isohypse state up [root] [--reindex] [--foreground] [--no-lsp] [--no-semantic]\n  start the tree detached (idempotent). --reindex rebuilds the live workspace index; --foreground serves in this terminal.",
        "state.down" => "usage: isohypse state down [root]\n  remove a workspace's worker; the tree keeps serving the rest.",
        "state.reload" => "usage: isohypse state reload [--no-lsp] [--no-semantic]\n  gap-free handover of the whole tree onto a freshly built binary.",
        "state.stop" => "usage: isohypse state stop\n  stop the tree; requires account-password authorization while workspaces are live.",
        "state.get" => "usage: isohypse state get <tag>\n  fetch stored content by tag.",
        "state.put" => "usage: isohypse state put\n  store stdin content and get a tag back.",
        "macro.save" => "usage: isohypse macro save <name>\n  save a steps array (JSON on stdin) under a name.",
        "macro.run" => "usage: isohypse macro run <name>\n  replay a saved macro in this workspace.",
        "macro.list" => "usage: isohypse macro list\n  list saved macros.",
        "multi-op" => "usage: isohypse multi-op [--validate CMD|none] [--diff] [steps-file]\n  ordered steps as one atomic transaction; JSON on stdin when no file is given (needs the daemon).\n  input: an array of op objects, or {\"steps\":[...],\"validate\":...}. Any mutate.* step requires validate.\n  a step may set \"checkpoint\":true to run validate right after it; any failure reverts every mutation.",
        "session" => "usage: isohypse session\n  open the NDJSON bridge; speak session.open / session.command / session.cancel / session.reload / session.subscribe / session.request / session.requests / session.close.",
        _ => return None,
    })
}

fn send_payload(payload: serde_json::Value) -> Result<String, String> {
    if payload.get("workspace").and_then(|w| w.as_str()) == Some("*") {
        let control = daemon::services_reachable()
            .ok_or_else(|| "no services daemon for * fan-out; start the daemon".to_string())?;
        let mut forward = payload.clone();
        if let serde_json::Value::Object(map) = &mut forward {
            map.remove("workspace");
        }
        return daemon::request(&control, &json!({"op": "fanout", "forward": forward}));
    }
    let socket = daemon::find_socket(&cwd()).ok_or_else(|| {
        "no live daemon found from here upward; start one with `isohypse daemon` at the project root".to_string()
    })?;
    daemon::request(&socket, &payload)
}

fn daemon_call(op: &str, query: Option<&str>, workspace: Option<&str>) -> Result<String, String> {
    let mut payload = json!({"op": op});
    if let Some(query) = query {
        payload["arg"] = json!(query);
    }
    if let Some(workspace) = workspace {
        payload["workspace"] = json!(workspace);
    }
    send_payload(payload)
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
    let pump = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let _ = std::io::copy(&mut stdin, &mut to_socket);
        let _ = to_socket.shutdown(std::net::Shutdown::Write);
    });
    let mut stdout = std::io::stdout();
    let _ = std::io::copy(&mut from_socket, &mut stdout);
    let _ = pump.join();
    Ok(String::new())
}

fn pull_workspace(args: &[String]) -> (Option<String>, Vec<String>) {
    let mut selector = None;
    let mut rest = Vec::new();
    let mut iter = args.iter();
    while let Some(argument) = iter.next() {
        if argument == "--workspace" || argument == "-w" {
            selector = iter.next().cloned();
        } else {
            rest.push(argument.clone());
        }
    }
    (selector, rest)
}

fn read_input_argument(args: &[String], position: usize) -> Result<String, String> {
    match args.get(position) {
        Some(file) => std::fs::read_to_string(file).map_err(|e| e.to_string()),
        None => {
            let mut buffer = String::new();
            std::io::stdin()
                .read_to_string(&mut buffer)
                .map(|_| buffer)
                .map_err(|e| e.to_string())
        }
    }
}

fn explore_payload(args: &[String]) -> Option<serde_json::Value> {
    let mut workspace: Option<String> = None;
    let mut max: Option<u64> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut full = false;
    let mut query_parts: Vec<String> = Vec::new();
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--workspace" | "-w" => workspace = rest.next().cloned(),
            "--max" => max = rest.next().and_then(|value| value.parse().ok()),
            "--path" => {
                if let Some(prefix) = rest.next() {
                    paths.push(prefix.clone());
                }
            }
            "--full" => full = true,
            other => query_parts.push(other.to_string()),
        }
    }
    if query_parts.is_empty() {
        return None;
    }
    let mut payload = if query_parts.len() == 2 {
        json!({ "op": "context.explore", "arg": query_parts[0], "to": query_parts[1] })
    } else {
        json!({ "op": "context.explore", "arg": query_parts.join(" "), "compact": !full, "full": full })
    };
    if let Some(max) = max {
        payload["max"] = json!(max);
    }
    if !paths.is_empty() {
        payload["paths"] = json!(paths);
    }
    if let Some(workspace) = workspace {
        payload["workspace"] = json!(workspace);
    }
    Some(payload)
}

fn split_read_target(token: &str) -> Option<(&str, &str)> {
    let (path, ranges) = token.rsplit_once(':')?;
    if path.is_empty() || ranges.is_empty() {
        return None;
    }
    if !ranges.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b',') {
        return None;
    }
    if !ranges.bytes().any(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((path, ranges))
}

type ReadTargets = Vec<(String, Vec<(Option<usize>, Option<usize>)>)>;

fn append_ranges(
    ranges: &mut Vec<(Option<usize>, Option<usize>)>,
    ranges_part: &str,
    token: &str,
) -> Result<(), String> {
    for piece in ranges_part.split(',') {
        let piece = piece.trim();
        let range = match piece.split_once('-') {
            Some((a, b)) => a.parse::<usize>().ok().zip(b.parse::<usize>().ok()),
            None => piece.parse::<usize>().ok().map(|line| (line, line)),
        };
        let Some((start, end)) = range else {
            return Err(format!(
                "bad range {piece:?} in {token:?}; use FILE:START-END[,START-END...] or FILE:LINE"
            ));
        };
        ranges.push((Some(start), Some(end)));
    }
    Ok(())
}

fn parse_read_targets(
    tokens: &[String],
) -> Result<Option<ReadTargets>, String> {
    let mut specs = Vec::with_capacity(tokens.len());
    let mut any_ranged = false;
    for token in tokens {
        if let Some((path, ranges_part)) = split_read_target(token) {
            let mut ranges = Vec::new();
            append_ranges(&mut ranges, ranges_part, token)?;
            any_ranged = true;
            specs.push((path.to_string(), ranges));
            continue;
        }
        let range_shaped = token.bytes().all(|b| b.is_ascii_digit() || b == b'-' || b == b',')
            && token.bytes().any(|b| b.is_ascii_digit());
        match specs.last_mut() {
            Some(last) if any_ranged && range_shaped => append_ranges(&mut last.1, token, token)?,
            _ => specs.push((token.clone(), Vec::new())),
        }
    }
    if any_ranged { Ok(Some(specs)) } else { Ok(None) }
}

fn find_payload(args: &[String]) -> Option<serde_json::Value> {
    let mut name = false;
    let mut any = false;
    let mut workspace: Option<String> = None;
    let mut max: Option<u64> = None;
    let mut paths: Vec<String> = Vec::new();
    let mut query_parts: Vec<String> = Vec::new();
    let mut rest = args.iter();
    while let Some(argument) = rest.next() {
        match argument.as_str() {
            "--name" => name = true,
            "--any" => any = true,
            "--workspace" | "-w" => workspace = rest.next().cloned(),
            "--max" => max = rest.next().and_then(|value| value.parse().ok()),
            "--path" => {
                if let Some(prefix) = rest.next() {
                    paths.push(prefix.clone());
                }
            }
            other => query_parts.push(other.to_string()),
        }
    }
    if query_parts.is_empty() {
        return None;
    }
    let query = query_parts.join(" ");
    let mut payload = json!({ "op": "context.find", "arg": query, "name": name });
    if any {
        payload["patterns"] = json!(query_parts);
    }
    if let Some(max) = max {
        payload["max"] = json!(max);
    }
    if !paths.is_empty() {
        payload["paths"] = json!(paths);
    }
    if let Some(workspace) = workspace {
        payload["workspace"] = json!(workspace);
    }
    Some(payload)
}

fn start_detached(root: PathBuf, lsp: bool, semantic: bool, handover: bool) -> Result<String, String> {
    let socket = daemon::socket_path(&root);
    if !handover && daemon::is_live(&socket) {
        return Ok(format!("isohypse already up on {}\n", socket.display()));
    }
    if !handover {
        let _ = std::fs::remove_file(&socket);
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
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
    let pid = command.spawn().map_err(|e| e.to_string())?.id();
    for _ in 0..100 {
        if daemon::is_live(&socket) {
            return Ok(format!("isohypse up (pid {pid}) on {}\n", socket.display()));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Ok(format!("isohypse starting (pid {pid}); indexing still underway, see {}\n", log_path.display()))
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.iter().any(|a| a == "--json") {
        daemon::JSON_OUTPUT.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    let args: Vec<String> = raw.into_iter().filter(|a| a != "--json").collect();
    let Some(first) = args.first() else { usage() };
    if first != "setup" {
        isohypse::store::resolve();
    }
    if first == "-h" || first == "--help" || first == "help" {
        usage()
    }
    let (command, rest): (String, Vec<String>) = match first.as_str() {
        "context" | "mutate" | "verify" | "state" | "macro" => {
            let Some(action) = args.get(1) else { usage() };
            (format!("{first}.{action}"), args[2..].to_vec())
        }
        _ => (first.clone(), args[1..].to_vec()),
    };
    if rest.iter().any(|a| a == "-h" || a == "--help") {
        if let Some(help) = subcommand_help(&command) {
            println!("{help}");
            std::process::exit(0);
        }
        usage()
    }
    if let Some(allowed) = allowed_flags(command.as_str()) {
        if let Err(message) = reject_unknown_flags(&rest, allowed) {
            eprintln!("isohypse: {message}");
            std::process::exit(1);
        }
    }
    let outcome: Result<String, String> = match command.as_str() {
        "context.read" => {
            let mut outline = false;
            let mut max_bytes: Option<usize> = None;
            let mut positional: Vec<String> = Vec::new();
            let mut it = rest.iter();
            while let Some(argument) = it.next() {
                match argument.as_str() {
                    "--outline" => outline = true,
                    "--max-bytes" => max_bytes = it.next().and_then(|value| value.parse().ok()),
                    other => positional.push(other.to_string()),
                }
            }
            if positional.is_empty() {
                usage()
            }
            let first_target = positional[0].clone();
            match parse_read_targets(&positional) {
                Err(e) => Err(e),
                Ok(Some(specs)) => Patcher::new(cwd()).and_then(|mut patcher| {
                    let mut out = String::new();
                    let mut omitted: Vec<String> = Vec::new();
                    for (path, ranges) in &specs {
                        let remaining = match max_bytes {
                            Some(cap) if out.len() >= cap => {
                                omitted.push(path.clone());
                                continue;
                            }
                            Some(cap) => Some(cap - out.len()),
                            None => None,
                        };
                        let ranged = if ranges.is_empty() { vec![(None, None)] } else { ranges.clone() };
                        out.push_str(&patcher.read_multi(path, &ranged, outline, remaining)?);
                    }
                    if !omitted.is_empty() {
                        out.push_str(&format!(
                            "({} file(s) past the --max-bytes cap not shown: {}; read them in a follow-up call)\n",
                            omitted.len(),
                            omitted.join(", ")
                        ));
                    }
                    Ok(out)
                }),
                Ok(None) => {
                    let has_ranges = positional[1..].iter().any(|t| t.contains('-'));
                    if has_ranges {
                        let mut ranges: Vec<(Option<usize>, Option<usize>)> = Vec::new();
                        let mut parse_error: Option<String> = None;
                        for token in &positional[1..] {
                            match token.split_once('-') {
                                Some((a, b)) => match (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                                    (Ok(start), Ok(end)) => ranges.push((Some(start), Some(end))),
                                    _ => parse_error = Some(format!("bad range {token:?}; use START-END")),
                                },
                                None => parse_error = Some(format!("bad range {token:?}; use START-END")),
                            }
                        }
                        match parse_error {
                            Some(e) => Err(e),
                            None => Patcher::new(cwd())
                                .and_then(|mut patcher| patcher.read_multi(&first_target, &ranges, outline, max_bytes)),
                        }
                    } else if positional.len() >= 2 && positional[1].parse::<usize>().is_err() {
                        Patcher::new(cwd()).and_then(|mut patcher| {
                            let mut out = String::new();
                            let mut omitted: Vec<&str> = Vec::new();
                            for file in &positional {
                                let remaining = match max_bytes {
                                    Some(cap) if out.len() >= cap => {
                                        omitted.push(file.as_str());
                                        continue;
                                    }
                                    Some(cap) => Some(cap - out.len()),
                                    None => None,
                                };
                                out.push_str(&patcher.read_with(file, None, None, outline, remaining)?);
                            }
                            if !omitted.is_empty() {
                                out.push_str(&format!(
                                    "({} file(s) past the --max-bytes cap not shown: {}; read them in a follow-up call)\n",
                                    omitted.len(),
                                    omitted.join(", ")
                                ));
                            }
                            Ok(out)
                        })
                    } else if positional.len() > 3 {
                        Err(format!("unexpected argument {:?}; a single-file read takes at most [start] [end]", positional[3]))
                    } else {
                        let start = positional.get(1).and_then(|v| v.parse().ok());
                        let end = positional.get(2).and_then(|v| v.parse().ok());
                        Patcher::new(cwd())
                            .and_then(|mut patcher| patcher.read_with(&first_target, start, end, outline, max_bytes))
                    }
                }
            }
        }
        "mutate.edit" => (|| {
            let mut validate: Option<String> = None;
            let mut diff = false;
            let mut selector: Option<String> = None;
            let mut positional: Vec<String> = Vec::new();
            let mut it = rest.iter();
            while let Some(argument) = it.next() {
                match argument.as_str() {
                    "--validate" => validate = it.next().cloned(),
                    "--diff" => diff = true,
                    "--workspace" | "-w" => selector = it.next().cloned(),
                    _ => positional.push(argument.clone()),
                }
            }
            let validate = validate
                .ok_or_else(|| "mutate.edit needs --validate CMD, or --validate none to waive it".to_string())?;
            let raw = read_input_argument(&positional, 0)?;
            let parsed: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| format!("mutate.edit takes JSON: {e}"))?;
            if let Some(socket) = daemon::find_socket(&cwd()).filter(|socket| daemon::is_live(socket)) {
                let mut payload = json!({
                    "op": "mutate.edit",
                    "cwd": cwd().to_string_lossy(),
                    "quiet": !diff,
                    "validate": validate,
                });
                match &parsed {
                    serde_json::Value::Array(items) => payload["ops"] = json!(items),
                    serde_json::Value::Object(source) => {
                        if let serde_json::Value::Object(target) = &mut payload {
                            for (key, value) in source {
                                if key != "op" {
                                    target.insert(key.clone(), value.clone());
                                }
                            }
                        }
                    }
                    _ => return Err("mutate.edit takes an op object or an array of them".to_string()),
                }
                if let Some(ws) = &selector {
                    payload["workspace"] = json!(ws);
                }
                daemon::request(&socket, &payload)
            } else {
                let mut patcher = Patcher::new(cwd())?;
                let effective = if validate == "none" { None } else { Some(validate.clone()) };
                let report = patcher.apply_edits_verified(&parsed, effective.as_deref())?;
                Ok(render::apply_report(&report, !diff))
            }
        })(),
        "multi-op" => (|| {
            let mut validate: Option<String> = None;
            let mut diff = false;
            let mut selector: Option<String> = None;
            let mut positional: Vec<String> = Vec::new();
            let mut it = rest.iter();
            while let Some(argument) = it.next() {
                match argument.as_str() {
                    "--validate" => validate = it.next().cloned(),
                    "--diff" => diff = true,
                    "--workspace" | "-w" => selector = it.next().cloned(),
                    _ => positional.push(argument.clone()),
                }
            }
            let raw = read_input_argument(&positional, 0)?;
            let parsed: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| format!("multi-op steps must be JSON: {e}"))?;
            let (steps, embedded_validate) = match parsed {
                serde_json::Value::Array(items) => (items, None),
                serde_json::Value::Object(map) => {
                    let steps = map
                        .get("steps")
                        .and_then(|v| v.as_array())
                        .cloned()
                        .ok_or_else(|| "multi-op object needs a steps array".to_string())?;
                    (steps, map.get("validate").cloned())
                }
                _ => return Err("multi-op input must be a JSON array of steps or an object with a steps array".to_string()),
            };
            let socket = daemon::find_socket(&cwd()).ok_or_else(|| {
                "no live daemon found from here upward; multi-op needs a running daemon".to_string()
            })?;
            let mut payload = json!({
                "op": "multi-op",
                "steps": steps,
                "cwd": cwd().to_string_lossy(),
                "quiet": !diff,
            });
            if let Some(command) = &validate {
                payload["validate"] = json!(command);
            } else if let Some(embedded) = embedded_validate {
                payload["validate"] = embedded;
            }
            if let Some(ws) = &selector {
                payload["workspace"] = json!(ws);
            }
            daemon::request(&socket, &payload)
        })(),
        "mutate.create" => {
            let Some(path) = rest.first() else { usage() };
            let mut content = String::new();
            match std::io::stdin().read_to_string(&mut content) {
                Ok(_) => Patcher::new(cwd()).and_then(|mut patcher| patcher.create(path, &content)),
                Err(e) => Err(e.to_string()),
            }
        }
        "context.find" => match find_payload(&rest) {
            Some(payload) => send_payload(payload),
            None => usage(),
        },
        "verify.check" => read_input_argument(&rest, 0).and_then(|raw| {
            let parsed: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| format!("verify.check takes mutate.edit JSON: {e}"))?;
            let mut patcher = Patcher::new(cwd())?;
            let report = patcher.check_edits(&parsed)?;
            Ok(render::check_report(&report))
        }),
        "context.log" => match rest.first() {
            Some(path) => Patcher::new(cwd()).and_then(|patcher| patcher.log(path)),
            None => Patcher::new(cwd()).and_then(|patcher| patcher.changelog()),
        },
        "mutate.undo" => {
            let mut recover = false;
            let mut positional: Vec<String> = Vec::new();
            for argument in &rest {
                match argument.as_str() {
                    "--recover" => recover = true,
                    other => positional.push(other.to_string()),
                }
            }
            let Some(path) = positional.first().cloned() else { usage() };
            if positional.len() > 2 {
                eprintln!("isohypse: unexpected argument {:?}; undo takes <path> [TAG]", positional[2]);
                std::process::exit(1);
            }
            let tag = positional.get(1).cloned();
            Patcher::new(cwd()).and_then(|mut patcher| patcher.undo_with(&path, tag.as_deref(), recover))
        }
        "context.explore" => match explore_payload(&rest) {
            Some(payload) => send_payload(payload),
            None => usage(),
        },
        "state.status" => {
            let (selector, _) = pull_workspace(&rest);
            daemon_call("state.status", None, selector.as_deref())
        }
        "state.stop" => authorize_stop().and_then(|password| {
            let mut body = json!({"op": "stop"});
            if let Some(pw) = &password {
                body["password"] = json!(pw);
            }
            match daemon::supervisor_reachable() {
                Some(control) => daemon::request(&control, &body),
                None => match daemon::find_socket(&cwd()) {
                    Some(socket) => daemon::request(&socket, &body),
                    None => Err("no live isohypse process found".to_string()),
                },
            }
        }),
        "daemon" => {
            let lsp_enabled = !rest.iter().any(|a| a == "--no-lsp");
            let semantic_enabled = !rest.iter().any(|a| a == "--no-semantic");
            let root = rest
                .iter()
                .find(|a| !a.starts_with("--"))
                .map(PathBuf::from)
                .unwrap_or_else(cwd);
            daemon::run_supervisor(root, lsp_enabled, semantic_enabled).map(|_| String::new())
        }
        "worker" => {
            let lsp_enabled = rest.iter().any(|a| a == "--lsp");
            let semantic_enabled = rest.iter().any(|a| a == "--semantic");
            let root = rest
                .iter()
                .find(|a| *a != "--lsp" && *a != "--semantic")
                .map(PathBuf::from)
                .unwrap_or_else(cwd);
            daemon::run_worker(root, lsp_enabled, semantic_enabled).map(|_| String::new())
        }
        "services" => daemon::run_services(rest.iter().any(|a| a == "--semantic")).map(|_| String::new()),
        "state.up" => {
            let reindex = rest.iter().any(|a| a == "--reindex");
            let foreground = rest.iter().any(|a| a == "--foreground");
            let lsp_enabled = !rest.iter().any(|a| a == "--no-lsp");
            let semantic_enabled = !rest.iter().any(|a| a == "--no-semantic");
            let root = rest
                .iter()
                .find(|a| !a.starts_with("--"))
                .map(PathBuf::from)
                .unwrap_or_else(cwd);
            let root = std::fs::canonicalize(&root).unwrap_or(root);
            if reindex {
                send_payload(json!({"op": "state.up", "reindex": true}))
            } else if foreground {
                daemon::run_supervisor(root, lsp_enabled, semantic_enabled).map(|_| String::new())
            } else {
                let socket = daemon::socket_path(&root);
                if daemon::is_live(&socket) {
                    Ok(format!("isohypse already up on {}\n", socket.display()))
                } else if let Some(control) = daemon::supervisor_reachable() {
                    daemon::request(&control, &json!({"op": "spawn", "root": root.to_string_lossy()}))
                } else {
                    start_detached(root, lsp_enabled, semantic_enabled, false)
                }
            }
        }
        "verify.build" => {
            let (selector, _) = pull_workspace(&rest);
            match daemon::find_socket(&cwd()).filter(|socket| daemon::is_live(socket)) {
                Some(socket) => {
                    let mut payload = json!({"op": "verify.build"});
                    if let Some(ws) = &selector {
                        payload["workspace"] = json!(ws);
                    }
                    daemon::request(&socket, &payload)
                }
                None => match isohypse::buildspec::detect(&cwd()) {
                    Some(spec) => {
                        let outcome = isohypse::buildspec::run(&cwd(), &spec);
                        let rendered = isohypse::buildspec::render(&outcome);
                        if outcome.ok {
                            Ok(rendered)
                        } else {
                            Err(rendered)
                        }
                    }
                    None => Err("no build command detected; add a .isohypse.build file with the command".to_string()),
                },
            }
        }
        "state.down" => {
            let (_, positional) = pull_workspace(&rest);
            let root = positional.first().map(PathBuf::from).unwrap_or_else(cwd);
            let root = std::fs::canonicalize(&root).unwrap_or(root);
            match daemon::supervisor_reachable() {
                Some(control) => daemon::request(
                    &control,
                    &json!({"op": "kill", "root": root.to_string_lossy()}),
                ),
                None => Err("no live supervisor found".to_string()),
            }
        }
        "context.prompt" => Ok(FORMAT_REFERENCE.to_string()),
        "verify.diagnose" => {
            let (selector, positional) = pull_workspace(&rest);
            daemon_call("verify.diagnose", positional.first().map(String::as_str), selector.as_deref())
        }
        "state.get" => {
            let Some(tag) = rest.first() else { usage() };
            daemon_call("state.get", Some(tag), None)
        }
        "state.put" => {
            let mut content = String::new();
            std::io::stdin()
                .read_to_string(&mut content)
                .map_err(|e| e.to_string())
                .and_then(|_| daemon_call("state.put", Some(&content), None))
        }
        "macro.save" => (|| {
            let Some(name) = rest.first() else { usage() };
            let raw = read_input_argument(&rest, 1)?;
            let steps: serde_json::Value =
                serde_json::from_str(&raw).map_err(|e| format!("macro.save takes a JSON steps array: {e}"))?;
            send_payload(json!({"op": "macro.save", "name": name, "steps": steps}))
        })(),
        "macro.run" => {
            let Some(name) = rest.first() else { usage() };
            send_payload(json!({"op": "macro.run", "name": name}))
        }
        "macro.list" => daemon_call("macro.list", None, None),
        "session" => match daemon::find_socket(&cwd()).and_then(|s| s.parent().map(|p| p.join(".isohypse.session.sock"))) {
            Some(socket) => session_bridge(&socket),
            None => Err("no live daemon found from here upward".to_string()),
        },
        "setup" => isohypse::setup::run(&rest),
        "state.reload" => {
            let lsp = !rest.iter().any(|a| a == "--no-lsp");
            let semantic = !rest.iter().any(|a| a == "--no-semantic");
            let root = std::fs::canonicalize(cwd()).unwrap_or_else(|_| cwd());
            start_detached(root, lsp, semantic, true)
        }
        _ => usage(),
    };
    match outcome {
        Ok(output) => print!("{output}"),
        Err(message) => {
            eprintln!("isohypse: {message}");
            std::process::exit(1);
        }
    }
}
