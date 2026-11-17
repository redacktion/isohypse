use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

pub use crate::setup::SandboxMode;

#[derive(Clone)]
pub struct BuildSpec {
    pub command: String,
    pub source: &'static str,
}

const OVERRIDE_FILE: &str = ".isohypse.build";
const HEAD_LINES: usize = 12;
const TAIL_LINES: usize = 40;
const BUILD_TIMEOUT_SECONDS: u64 = 120;

pub fn detect(root: &Path) -> Option<BuildSpec> {
    if std::env::var_os("ISOHYPSE_ALLOW_BUILD_FILE").is_some() {
        if let Ok(raw) = std::fs::read_to_string(root.join(OVERRIDE_FILE)) {
            let command = raw.trim().to_string();
            if !command.is_empty() {
                return Some(BuildSpec { command, source: "override" });
            }
        }
    }
    let marker = |name: &str| root.join(name).exists();
    let command = if marker("Cargo.toml") {
        "cargo build"
    } else if marker("go.mod") {
        "go build ./..."
    } else if marker("Makefile") {
        "make"
    } else if marker("package.json") {
        "npm run build"
    } else if marker("pyproject.toml") || marker("setup.py") {
        "python -m compileall -q ."
    } else {
        return None;
    };
    Some(BuildSpec { command: command.to_string(), source: "detected" })
}

pub struct BuildOutcome {
    pub command: String,
    pub source: &'static str,
    pub ok: bool,
    pub code: Option<i32>,
    pub seconds: f64,
    pub output: String,
}

pub fn head_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= HEAD_LINES + TAIL_LINES {
        return text.to_string();
    }
    let head = lines[..HEAD_LINES].join("\n");
    let tail = lines[lines.len() - TAIL_LINES..].join("\n");
    let elided = lines.len() - HEAD_LINES - TAIL_LINES;
    format!("{head}\n... {elided} lines elided ...\n{tail}")
}

pub fn run(root: &Path, spec: &BuildSpec) -> BuildOutcome {
    run_command(root, &spec.command, spec.source)
}

pub fn run_command(root: &Path, command: &str, source: &'static str) -> BuildOutcome {
    run_command_in(root, command, source, None, SandboxMode::Off)
}

pub fn run_command_with_cancel(root: &Path, command: &str, source: &'static str, cancelled: Option<&std::sync::atomic::AtomicBool>) -> BuildOutcome {
    run_command_in(root, command, source, cancelled, crate::setup::Config::load().validate_sandbox)
}

const SANDBOX_PROFILE: &str = r#"(version 1)
(allow default)
(deny file-write*)
(allow file-write*
  (subpath (param "ROOT"))
  (subpath (param "TMP"))
  (subpath "/private/tmp")
  (subpath "/private/var/folders")
  (subpath (string-append (param "HOME") "/.cargo"))
  (subpath (string-append (param "HOME") "/.cache"))
  (subpath (string-append (param "HOME") "/.npm"))
  (literal "/dev/null")
  (literal "/dev/tty")
  (literal "/dev/dtracehelper"))
"#;

const SANDBOX_NO_NETWORK: &str = "(deny network*)
";

#[derive(Debug, PartialEq, Eq)]
pub enum SandboxError {
    UnsupportedPlatform,
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::UnsupportedPlatform => write!(f, "validate_sandbox requires macOS sandbox-exec; set it to \"off\" on this platform"),
        }
    }
}

pub struct SandboxPaths<'a> {
    pub root: &'a Path,
    pub tmp: &'a Path,
    pub home: &'a str,
}

/// The argv that runs `command` under `mode`. Pure.
pub fn sandbox_argv(mode: SandboxMode, macos: bool, paths: &SandboxPaths, command: &str) -> Result<Vec<String>, SandboxError> {
    if mode == SandboxMode::Off {
        return Ok(vec!["sh".to_string(), "-c".to_string(), command.to_string()]);
    }
    if !macos {
        return Err(SandboxError::UnsupportedPlatform);
    }
    let mut profile = SANDBOX_PROFILE.to_string();
    if mode == SandboxMode::WorkspaceWriteNoNetwork {
        profile.push_str(SANDBOX_NO_NETWORK);
    }
    Ok(["/usr/bin/sandbox-exec".to_string(),
        "-D".to_string(), format!("ROOT={}", paths.root.display()),
        "-D".to_string(), format!("TMP={}", paths.tmp.display()),
        "-D".to_string(), format!("HOME={}", paths.home),
        "-p".to_string(), profile,
        "sh".to_string(), "-c".to_string(), command.to_string()]
    .to_vec())
}

fn shell_command(root: &Path, command: &str, sandbox: SandboxMode) -> Result<Command, SandboxError> {
    let canonical = |path: &Path| path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let (root, tmp) = (canonical(root), canonical(&std::env::temp_dir()));
    let home = std::env::var("HOME").unwrap_or_default();
    let argv = sandbox_argv(sandbox, cfg!(target_os = "macos"), &SandboxPaths { root: &root, tmp: &tmp, home: &home }, command)?;
    let mut built = Command::new(&argv[0]);
    built.args(&argv[1..]);
    Ok(built)
}

pub fn run_command_in(root: &Path, command: &str, source: &'static str, cancelled: Option<&std::sync::atomic::AtomicBool>, sandbox: SandboxMode) -> BuildOutcome {
    let started = Instant::now();
    let spawned = shell_command(root, command, sandbox)
        .map_err(|reason| std::io::Error::new(std::io::ErrorKind::Unsupported, reason.to_string()))
        .and_then(|mut cmd| cmd
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn());
    let result: Result<(Option<std::process::ExitStatus>, Vec<u8>, Vec<u8>), std::io::Error> = match spawned {
        Ok(mut child) => loop {
            match child.try_wait() {
                Ok(Some(status)) => break child.wait_with_output().map(|output| (Some(status), output.stdout, output.stderr)),
                Ok(None) if cancelled.is_some_and(|token| token.load(std::sync::atomic::Ordering::Acquire)) || started.elapsed() >= std::time::Duration::from_secs(BUILD_TIMEOUT_SECONDS) => {
                    let _ = child.kill();
                    let output = child.wait_with_output();
                    break output.map(|output| (None, output.stdout, output.stderr));
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(error) => break Err(error),
            }
        },
        Err(error) => Err(error),
    };
    let seconds = started.elapsed().as_secs_f64();
    match result {
        Ok((status, stdout, stderr)) => {
            let mut combined = String::from_utf8_lossy(&stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&stderr));
            let timed_out = status.is_none();
            if timed_out { combined.push_str("\nvalidation timed out after 120 seconds and was terminated\n"); }
            BuildOutcome {
                command: command.to_string(),
                source,
                ok: status.map(|status| status.success()).unwrap_or(false),
                code: status.and_then(|status| status.code()),
                seconds,
                output: head_tail(&combined),
            }
        }
        Err(e) => BuildOutcome {
            command: command.to_string(),
            source,
            ok: false,
            code: None,
            seconds,
            output: format!("failed to launch build: {e}"),
        },
    }
}

pub fn render(outcome: &BuildOutcome) -> String {
    let status = if outcome.ok { "ok" } else { "FAILED" };
    let code = match outcome.code {
        Some(c) => format!("exit {c}"),
        None => "no exit code".to_string(),
    };
    let mut out = format!(
        "build {status} ({}, {}, {:.2}s, {code})\n",
        outcome.command, outcome.source, outcome.seconds
    );
    if !outcome.ok && !outcome.output.trim().is_empty() {
        out.push_str(&outcome.output);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

impl crate::opresult::OpResult for BuildOutcome {
    fn render(&self) -> String {
        render(self)
    }
    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "op": "build",
            "command": self.command,
            "source": self.source,
            "ok": self.ok,
            "code": self.code,
            "seconds": self.seconds,
            "output": self.output,
        })
    }
}
