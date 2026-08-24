use std::path::Path;
use std::process::Command;
use std::time::Instant;

#[derive(Clone)]
pub struct BuildSpec {
    pub command: String,
    pub source: &'static str,
}

const OVERRIDE_FILE: &str = ".isohypse.build";
const HEAD_LINES: usize = 12;
const TAIL_LINES: usize = 40;

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
    let started = Instant::now();
    let result = Command::new("sh")
        .arg("-c")
        .arg(&spec.command)
        .current_dir(root)
        .output();
    let seconds = started.elapsed().as_secs_f64();
    match result {
        Ok(out) => {
            let mut combined = String::from_utf8_lossy(&out.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&out.stderr));
            BuildOutcome {
                command: spec.command.clone(),
                source: spec.source,
                ok: out.status.success(),
                code: out.status.code(),
                seconds,
                output: head_tail(&combined),
            }
        }
        Err(e) => BuildOutcome {
            command: spec.command.clone(),
            source: spec.source,
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
