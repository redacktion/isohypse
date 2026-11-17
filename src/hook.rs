//! Agent-harness hook routing, in three layers. `parse_event` and `parse_shell`
//! turn untrusted text into types. `decide` is a pure function of those types
//! and a `Probe`. `FsProbe` is the only code that touches the filesystem or the
//! daemon, so the whole decision table runs against an in-memory probe.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde_json::{json, Value};

const CALL_EXAMPLE: &str = "isohypse call <<'EOF'\n{\"steps\":[{\"op\":\"context.read\",\"arg\":\"path/to/file\"}]}\nEOF";

const SESSION_CONTEXT: &str = "This repository is served by Isohypse. Read, search and edit source with `isohypse call`; the built-in Read, Grep, Glob, Edit and Write tools are blocked here.
Send a JSON batch on stdin: isohypse call <<'EOF'
{\"steps\":[{\"op\":\"context.read\",\"arg\":\"src/main.rs\",\"range\":\"1-80\"},{\"op\":\"context.find\",\"arg\":\"parse_request\",\"paths\":[\"src\"]}]}
EOF
Reads print a tag like [src/main.rs#a1b2c3d]. An edit cites that tag and a validate command: {\"steps\":[{\"op\":\"mutate.edit\",\"path\":\"src/main.rs\",\"tag\":\"a1b2c3d\",\"edits\":[{\"replace\":{\"lines\":[10,12]},\"with\":[\"new line\"]}]}],\"validate\":\"cargo check\"}. Edit only lines you read. Send independent reads in one batch. Run `isohypse context prompt` for the full grammar.";

// ── effects ────────────────────────────────────────────────────────────────

pub trait Probe {
    fn is_live(&self, dir: &Path) -> bool;
    /// Symlinks and `..` resolved. Tolerates a path that does not exist yet.
    fn canonical(&self, path: &Path) -> PathBuf;
    fn exists(&self, path: &Path) -> bool;
    fn is_file(&self, path: &Path) -> bool;
}

pub struct FsProbe;

impl Probe for FsProbe {
    fn is_live(&self, dir: &Path) -> bool {
        crate::daemon::is_live(&crate::daemon::socket_path(dir))
    }

    fn canonical(&self, path: &Path) -> PathBuf {
        let mut clean = PathBuf::new();
        for part in path.components() {
            match part {
                Component::ParentDir => { clean.pop(); }
                Component::CurDir => {}
                other => clean.push(other.as_os_str()),
            }
        }
        let mut existing = clean.as_path();
        let mut tail = Vec::new();
        while !existing.exists() {
            let Some(parent) = existing.parent() else { return clean };
            if let Some(name) = existing.file_name() { tail.push(name.to_os_string()); }
            existing = parent;
        }
        let mut resolved = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
        resolved.extend(tail.into_iter().rev());
        resolved
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
}

// ── events ─────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Read,
    Grep,
    Glob,
    Edit,
    MultiEdit,
    Write,
    NotebookEdit,
    ApplyPatch,
}

impl Builtin {
    fn parse(name: &str) -> Option<Builtin> {
        Some(match name {
            "Read" => Builtin::Read,
            "Grep" => Builtin::Grep,
            "Glob" => Builtin::Glob,
            "Edit" => Builtin::Edit,
            "MultiEdit" => Builtin::MultiEdit,
            "Write" => Builtin::Write,
            "NotebookEdit" => Builtin::NotebookEdit,
            "apply_patch" => Builtin::ApplyPatch,
            _ => return None,
        })
    }
}

impl fmt::Display for Builtin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Builtin::ApplyPatch => write!(f, "apply_patch"),
            other => write!(f, "{other:?}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tool {
    Builtin { tool: Builtin, targets: Vec<String> },
    Shell(String),
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    SessionStart { cwd: PathBuf },
    ToolUse { cwd: PathBuf, tool: Tool },
}

pub fn parse_event(input: &str) -> Option<Event> {
    let event: Value = serde_json::from_str(input).ok()?;
    let cwd = PathBuf::from(event.get("cwd").and_then(Value::as_str)?);
    if event.get("hook_event_name").and_then(Value::as_str) == Some("SessionStart") {
        return Some(Event::SessionStart { cwd });
    }
    let name = event.get("tool_name").and_then(Value::as_str)?;
    let tool_input = event.get("tool_input");
    let tool = if name == "Bash" {
        match tool_input.and_then(|i| i.get("command")).and_then(Value::as_str) {
            Some(command) => Tool::Shell(command.to_string()),
            None => Tool::Other,
        }
    } else if let Some(tool) = Builtin::parse(name) {
        let mut targets: Vec<String> = tool_input
            .and_then(|i| i.get("file_path").or_else(|| i.get("path")).or_else(|| i.get("notebook_path")))
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect();
        if tool == Builtin::ApplyPatch {
            tool_input.into_iter().for_each(|input| patch_paths(input, &mut targets));
        }
        Tool::Builtin { tool, targets }
    } else {
        Tool::Other
    };
    Some(Event::ToolUse { cwd, tool })
}

/// File paths named by the headers of an apply_patch body, in any string field.
fn patch_paths(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => {
            const HEADERS: [&str; 4] = ["*** Add File: ", "*** Update File: ", "*** Delete File: ", "*** Move to: "];
            for line in text.lines() {
                if let Some(path) = HEADERS.iter().find_map(|h| line.strip_prefix(h)) {
                    out.push(path.trim().to_string());
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| patch_paths(v, out)),
        Value::Object(map) => map.values().for_each(|v| patch_paths(v, out)),
        _ => {}
    }
}

// ── shell syntax ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    pub words: Vec<String>,
    /// True when the segment reads the previous segment's output.
    pub piped: bool,
}

const SHELLS: [&str; 4] = ["sh", "bash", "zsh", "dash"];
const WRAPPERS: [&str; 7] = ["sudo", "command", "env", "time", "nohup", "xargs", "exec"];

fn unquote(word: &str) -> String {
    word.trim_matches(|c| c == '\'' || c == '"').to_string()
}

fn split_line(line: &str) -> Vec<Segment> {
    let mut out = Vec::new();
    let mut piped = false;
    let mut rest = line;
    loop {
        let cut = ["&&", "||", ";", "|"].iter().filter_map(|sep| rest.find(sep).map(|i| (i, *sep))).min_by_key(|(i, _)| *i);
        let (head, next) = match cut {
            Some((i, sep)) => (&rest[..i], Some((&rest[i + sep.len()..], sep == "|"))),
            None => (rest, None),
        };
        let words: Vec<String> = head.split_whitespace().map(unquote).filter(|w| !w.is_empty()).collect();
        if !words.is_empty() { out.push(Segment { words, piped }); }
        match next {
            Some((tail, pipe)) => { rest = tail; piped = pipe; }
            None => break,
        }
    }
    out
}

/// Index and name of the command a segment runs, past env assignments and wrappers.
fn command_at(words: &[String]) -> Option<(usize, String)> {
    words.iter().enumerate().find_map(|(index, word)| {
        let name = Path::new(word).file_name().and_then(|n| n.to_str()).unwrap_or(word).to_string();
        let assignment = word.contains('=') && !word.starts_with('-') && !word.contains('/');
        (!assignment && !WRAPPERS.contains(&name.as_str())).then_some((index, name))
    })
}

/// Split a shell command into segments. Heredoc bodies are data, except when a
/// shell receives them. Command substitutions become segments of their own.
pub fn parse_shell(command: &str) -> Vec<Segment> {
    let command = command.replace("$(", ";").replace(['`', ')'], ";");
    let mut out = Vec::new();
    let mut heredoc: Option<(String, bool)> = None;
    for line in command.lines() {
        if let Some((end, runs)) = &heredoc {
            if line.trim() == end { heredoc = None; continue; }
            if !*runs { continue; }
        } else if let Some(at) = line.find("<<").filter(|at| !line[*at..].starts_with("<<<")) {
            let word = line[at + 2..].trim_start_matches('-').split_whitespace().next().map(unquote).unwrap_or_default();
            if !word.is_empty() {
                let receiver = split_line(&line[..at]).iter().filter_map(|s| command_at(&s.words)).next_back().map(|(_, name)| name);
                heredoc = Some((word, receiver.is_some_and(|name| SHELLS.contains(&name.as_str()))));
            }
        }
        out.extend(split_line(&without_heredoc_operator(line)));
    }
    out
}

/// The line minus its `<<WORD` redirection, which is not an argument.
fn without_heredoc_operator(line: &str) -> String {
    let Some(at) = line.find("<<").filter(|at| !line[*at..].starts_with("<<<")) else { return line.to_string() };
    let after = line[at + 2..].trim_start_matches('-').trim_start();
    let delimiter_end = after.find(char::is_whitespace).unwrap_or(after.len());
    format!("{} {}", &line[..at], &after[delimiter_end..])
}

// ── policy ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    ReadsFile(String),
    SearchesWorkspace(String),
    ListsWorkspace,
    EditsFile(String),
    WritesViaTee,
    WritesViaRedirect,
    StopsIsohypse,
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::ReadsFile(word) => write!(f, "`{word}` reads a workspace file without a tag. Use context.read."),
            Violation::SearchesWorkspace(word) => write!(f, "`{word}` searches the workspace outside the index. Use context.find, or context.explore for a symbol."),
            Violation::ListsWorkspace => write!(f, "`find` lists the workspace outside the index. Use context.find with a path pattern."),
            Violation::EditsFile(word) => write!(f, "`{word}` edits or reads a workspace file without a tag. Use context.read, then mutate.edit."),
            Violation::WritesViaTee => write!(f, "`tee` writes a workspace file without a tag. Use mutate.create or mutate.edit."),
            Violation::WritesViaRedirect => write!(f, "A redirect writes a workspace file without a tag. Use mutate.create or mutate.edit."),
            Violation::StopsIsohypse => write!(f, "Stopping or reconfiguring Isohypse is the operator's call. Ask the user to run it."),
        }
    }
}

const READERS: [&str; 7] = ["cat", "head", "tail", "less", "more", "bat", "nl"];
const SEARCHERS: [&str; 6] = ["grep", "egrep", "fgrep", "rg", "ag", "ack"];
const FILE_EDITORS: [&str; 3] = ["sed", "awk", "perl"];
const SOURCE_EXTENSIONS: [&str; 26] = [
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "c", "h", "cc", "cpp", "hpp", "rb", "php", "swift", "kt", "cs", "sh", "toml", "yaml", "yml", "md", "html", "css",
    "json",
];

struct Scope<'a> {
    probe: &'a dyn Probe,
    root: &'a Path,
    cwd: &'a Path,
}

impl Scope<'_> {
    fn resolved(&self, token: &str) -> PathBuf {
        let absolute = if Path::new(token).is_absolute() { PathBuf::from(token) } else { self.cwd.join(token) };
        self.probe.canonical(&absolute)
    }

    fn inside(&self, token: &str) -> bool {
        self.resolved(token).starts_with(self.root)
    }

    /// An argument naming an existing workspace path, or a glob inside it.
    fn names_path(&self, token: &str) -> bool {
        !token.starts_with('-')
            && !token.is_empty()
            && self.inside(token)
            && (token.contains(['*', '?', '[']) || self.probe.exists(&self.resolved(token)))
    }

    /// An existing workspace file, or a new file with a source-like extension.
    fn writes_source(&self, token: &str) -> bool {
        if token.starts_with('-') || !self.inside(token) {
            return false;
        }
        let extension = Path::new(token).extension().and_then(|e| e.to_str()).unwrap_or("");
        self.probe.is_file(&self.resolved(token)) || SOURCE_EXTENSIONS.contains(&extension)
    }
}

fn segment_violation(scope: &Scope, segment: &Segment) -> Option<Violation> {
    let (at, word) = command_at(&segment.words)?;
    let args = &segment.words[at + 1..];
    let any_path = || args.iter().any(|a| scope.names_path(a));
    match word.as_str() {
        "isohypse" => {
            let sub = (args.first().map(String::as_str), args.get(1).map(String::as_str));
            matches!(sub, (Some("state"), Some("down" | "stop")) | (Some("setup"), _)).then_some(Violation::StopsIsohypse)
        }
        w if SHELLS.contains(&w) => {
            let flag = args.iter().position(|a| a == "-c")?;
            shell_violation(scope, &parse_shell(&args[flag + 1..].join(" ")))
        }
        "git" if args.iter().find(|a| !a.starts_with('-')).is_some_and(|a| a == "grep") => Some(Violation::SearchesWorkspace("git grep".to_string())),
        w if READERS.contains(&w) => any_path().then(|| Violation::ReadsFile(word.clone())),
        w if SEARCHERS.contains(&w) && !segment.piped => {
            let operands: Vec<&String> = args.iter().filter(|a| !a.starts_with('-')).collect();
            let searches = operands.len() < 2 || operands.iter().skip(1).any(|a| scope.names_path(a));
            searches.then(|| Violation::SearchesWorkspace(word.clone()))
        }
        "find" if !segment.piped => {
            let paths: Vec<&String> = args.iter().take_while(|a| !a.starts_with(['-', '!', '(', ')'])).collect();
            (paths.is_empty() || paths.iter().any(|a| scope.inside(a))).then_some(Violation::ListsWorkspace)
        }
        w if FILE_EDITORS.contains(&w) => any_path().then(|| Violation::EditsFile(word.clone())),
        "tee" => args.iter().any(|a| scope.writes_source(a)).then_some(Violation::WritesViaTee),
        _ => None,
    }
}

fn redirect_violation(scope: &Scope, segments: &[Segment]) -> Option<Violation> {
    let mut targets = segments.iter().flat_map(|segment| {
        segment.words.iter().enumerate().filter_map(|(i, word)| {
            if word == ">" || word == ">>" {
                segment.words.get(i + 1).map(String::as_str)
            } else {
                word.strip_prefix(">>").or_else(|| word.strip_prefix('>')).filter(|t| !t.is_empty() && !t.starts_with('&'))
            }
        })
    });
    targets.any(|t| !t.starts_with("/dev/") && scope.writes_source(t)).then_some(Violation::WritesViaRedirect)
}

/// A kill-family command anywhere in a command that names isohypse anywhere.
fn kills_isohypse(segments: &[Segment]) -> bool {
    let kills = segments.iter().any(|s| command_at(&s.words).is_some_and(|(_, name)| matches!(name.as_str(), "kill" | "pkill" | "killall")));
    kills && segments.iter().flat_map(|s| s.words.iter()).any(|w| w.contains("isohypse"))
}

fn shell_violation(scope: &Scope, segments: &[Segment]) -> Option<Violation> {
    if kills_isohypse(segments) {
        return Some(Violation::StopsIsohypse);
    }
    segments.iter().find_map(|segment| segment_violation(scope, segment)).or_else(|| redirect_violation(scope, segments))
}

// ── decisions ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Brief(String),
    Deny(String),
}

impl Verdict {
    /// The JSON the harness reads. Empty for Allow.
    pub fn render(&self) -> String {
        match self {
            Verdict::Allow => String::new(),
            Verdict::Brief(context) => json!({"hookSpecificOutput": {"hookEventName": "SessionStart", "additionalContext": context}}).to_string(),
            Verdict::Deny(reason) => json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "deny", "permissionDecisionReason": reason}}).to_string(),
        }
    }
}

fn live_root(probe: &dyn Probe, cwd: &Path) -> Option<PathBuf> {
    probe.canonical(cwd).ancestors().find(|dir| probe.is_live(dir)).map(Path::to_path_buf)
}

pub fn decide(event: &Event, probe: &dyn Probe) -> Verdict {
    let (Event::SessionStart { cwd } | Event::ToolUse { cwd, .. }) = event;
    let Some(root) = live_root(probe, cwd) else { return Verdict::Allow };
    let cwd = probe.canonical(cwd);
    let scope = Scope { probe, root: &root, cwd: &cwd };
    match event {
        Event::SessionStart { .. } => Verdict::Brief(SESSION_CONTEXT.to_string()),
        Event::ToolUse { tool: Tool::Other, .. } => Verdict::Allow,
        Event::ToolUse { tool: Tool::Shell(command), .. } => match shell_violation(&scope, &parse_shell(command)) {
            Some(why) => Verdict::Deny(format!("This workspace is served by Isohypse. {why} Run `{CALL_EXAMPLE}`; `isohypse context prompt` prints the grammar.")),
            None => Verdict::Allow,
        },
        Event::ToolUse { tool: Tool::Builtin { tool, targets }, .. } => {
            if !targets.is_empty() && targets.iter().all(|path| !scope.inside(path)) {
                return Verdict::Allow;
            }
            Verdict::Deny(format!(
                "This workspace is served by Isohypse. Use it instead of {tool}: run `{CALL_EXAMPLE}`. `isohypse context prompt` prints the grammar. Edits cite the tag from a read and carry a validate command."
            ))
        }
    }
}

/// Hook entry point: stdin text to stdout text. `off` is `ISOHYPSE_HOOK=off`.
pub fn respond(input: &str, off: bool, probe: &dyn Probe) -> String {
    if off {
        return String::new();
    }
    parse_event(input).map(|event| decide(&event, probe).render()).unwrap_or_default()
}
