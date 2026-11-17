//! Hook routing against an in-memory probe: no disk, no daemon.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use isohypse::hook::{respond, Probe};
use serde_json::{json, Value};

const ROOT: &str = "/repo";

struct Fake {
    live: Option<PathBuf>,
    files: BTreeSet<PathBuf>,
    dirs: BTreeSet<PathBuf>,
}

fn fake() -> Fake {
    Fake {
        live: Some(PathBuf::from(ROOT)),
        files: ["/repo/lib.rs", "/repo/src/a.rs", "/outside/x.txt"].iter().map(PathBuf::from).collect(),
        dirs: ["/repo", "/repo/src", "/outside"].iter().map(PathBuf::from).collect(),
    }
}

impl Probe for Fake {
    fn is_live(&self, dir: &Path) -> bool {
        self.live.as_deref() == Some(dir)
    }
    fn canonical(&self, path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for part in path.components() {
            match part {
                Component::ParentDir => { out.pop(); }
                Component::CurDir => {}
                other => out.push(other.as_os_str()),
            }
        }
        match out.strip_prefix("/alias") {
            Ok(rest) => Path::new(ROOT).join(rest),
            Err(_) => out,
        }
    }
    fn exists(&self, path: &Path) -> bool {
        self.files.contains(path) || self.dirs.contains(path)
    }
    fn is_file(&self, path: &Path) -> bool {
        self.files.contains(path)
    }
}

fn denied(probe: &Fake, input: &str) -> bool {
    respond(input, false, probe).contains("\"permissionDecision\":\"deny\"")
}

fn tool_event(tool: &str, cwd: &str, input: Value) -> String {
    json!({"tool_name": tool, "cwd": cwd, "tool_input": input}).to_string()
}

fn patch(text: &str) -> String {
    tool_event("apply_patch", ROOT, json!({"input": text}))
}

fn bash(command: &str) -> String {
    tool_event("Bash", ROOT, json!({"command": command}))
}

fn file(tool: &str, path: &str) -> String {
    tool_event(tool, ROOT, json!({"file_path": path}))
}

fn verdicts(inputs: &[(&str, String)]) -> Vec<(String, bool)> {
    let probe = fake();
    inputs.iter().map(|(name, input)| (name.to_string(), denied(&probe, input))).collect()
}

fn expect(rows: &[(&str, bool)]) -> Vec<(String, bool)> {
    rows.iter().map(|(name, deny)| (name.to_string(), *deny)).collect()
}

#[test]
fn builtin_tools_inside_the_workspace_are_denied() {
    let got = verdicts(&[
        ("Read", file("Read", "/repo/lib.rs")),
        ("Edit", file("Edit", "/repo/lib.rs")),
        ("MultiEdit", file("MultiEdit", "/repo/lib.rs")),
        ("Write new file", file("Write", "/repo/new.rs")),
        ("NotebookEdit", tool_event("NotebookEdit", ROOT, json!({"notebook_path": "/repo/n.ipynb"}))),
        ("Grep without path", tool_event("Grep", ROOT, json!({"pattern": "a"}))),
        ("Glob without path", tool_event("Glob", ROOT, json!({"pattern": "*.rs"}))),
        ("Read through a symlink alias", file("Read", "/alias/lib.rs")),
        ("Read with a relative path", file("Read", "src/a.rs")),
        ("apply_patch updating a workspace file", patch("*** Begin Patch\n*** Update File: /repo/lib.rs\n@@\n-a\n+b\n*** End Patch")),
        ("apply_patch adding a relative file", patch("*** Begin Patch\n*** Add File: src/new.rs\n+x\n*** End Patch")),
        ("apply_patch naming one file outside and one inside", patch("*** Update File: /outside/x.txt\n*** Update File: /repo/lib.rs")),
        ("apply_patch with no recognizable path", patch("garbage")),
    ]);
    assert_eq!(got, expect(&[
        ("Read", true), ("Edit", true), ("MultiEdit", true), ("Write new file", true), ("NotebookEdit", true),
        ("Grep without path", true), ("Glob without path", true), ("Read through a symlink alias", true), ("Read with a relative path", true), ("apply_patch updating a workspace file", true), ("apply_patch adding a relative file", true),
        ("apply_patch naming one file outside and one inside", true), ("apply_patch with no recognizable path", true),
    ]));
}

#[test]
fn builtin_tools_outside_the_workspace_are_allowed() {
    let got = verdicts(&[
        ("absolute outside", file("Read", "/outside/x.txt")),
        ("parent escape", tool_event("Read", "/repo/src", json!({"file_path": "../../outside/x.txt"}))),
        ("unserved cwd", tool_event("Read", "/elsewhere", json!({"file_path": "/elsewhere/a"}))),
        ("apply_patch outside the workspace", patch("*** Begin Patch\n*** Update File: /outside/x.txt\n@@\n-a\n+b\n*** End Patch")),
    ]);
    assert_eq!(got, expect(&[("absolute outside", false), ("parent escape", false), ("unserved cwd", false), ("apply_patch outside the workspace", false)]));
}

#[test]
fn shell_reads_of_workspace_files_are_denied() {
    let got = verdicts(&[
        ("cat", bash("cat lib.rs")),
        ("head", bash("head -n 5 lib.rs")),
        ("cat then pipe", bash("cat lib.rs | head")),
        ("nl", bash("nl src/a.rs")),
        ("env prefix", bash("FOO=1 cat lib.rs")),
        ("xargs wrapper", bash("ls | xargs cat lib.rs")),
    ]);
    assert_eq!(got, expect(&[("cat", true), ("head", true), ("cat then pipe", true), ("nl", true), ("env prefix", true), ("xargs wrapper", true)]));
}

#[test]
fn shell_searches_of_the_workspace_are_denied() {
    let got = verdicts(&[
        ("rg without path", bash("rg foo")),
        ("rg in a directory", bash("rg -n foo src")),
        ("grep recursive", bash("grep -rn foo .")),
        ("git grep", bash("git grep foo")),
        ("find in cwd", bash("find . -name x")),
        ("find with no path", bash("find -name x")),
    ]);
    assert_eq!(got, expect(&[("rg without path", true), ("rg in a directory", true), ("grep recursive", true), ("git grep", true), ("find in cwd", true), ("find with no path", true)]));
}

#[test]
fn shell_writes_to_workspace_files_are_denied() {
    let got = verdicts(&[
        ("sed in place", bash("sed -i s/a/b/ lib.rs")),
        ("redirect over a file", bash("echo x > lib.rs")),
        ("redirect a new source file", bash("echo x > new.rs")),
        ("append a new source file", bash("echo x >> new.rs")),
        ("attached redirect", bash("echo x >new.rs")),
        ("tee over a file", bash("echo x | tee lib.rs")),
    ]);
    assert_eq!(got, expect(&[
        ("sed in place", true), ("redirect over a file", true), ("redirect a new source file", true), ("append a new source file", true),
        ("attached redirect", true), ("tee over a file", true),
    ]));
}

#[test]
fn shell_wrappers_do_not_hide_a_bypass() {
    let got = verdicts(&[
        ("command substitution", bash("echo $(cat lib.rs)")),
        ("backticks", bash("echo `cat lib.rs`")),
        ("bash -c", bash("bash -c 'cat lib.rs'")),
        ("shell heredoc body", bash("bash <<EOF\ncat lib.rs\nEOF")),
        ("herestring does not hide the next line", bash("cat <<< hi\ncat lib.rs")),
        ("second line", bash("echo hi\ncat lib.rs")),
        ("and chain", bash("true && cat lib.rs")),
    ]);
    assert_eq!(got, expect(&[
        ("command substitution", true), ("backticks", true), ("bash -c", true), ("shell heredoc body", true),
        ("herestring does not hide the next line", true), ("second line", true), ("and chain", true),
    ]));
}

#[test]
fn stopping_or_reconfiguring_isohypse_is_denied() {
    let got = verdicts(&[
        ("state down", bash("isohypse state down")),
        ("state stop", bash("isohypse state stop")),
        ("setup", bash("isohypse setup --micro")),
        ("pkill", bash("pkill isohypse")),
        ("killall", bash("killall isohypse")),
        ("kill by name", bash("kill $(pgrep isohypse)")),
    ]);
    assert_eq!(got, expect(&[("state down", true), ("state stop", true), ("setup", true), ("pkill", true), ("killall", true), ("kill by name", true)]));
}

#[test]
fn ordinary_shell_work_is_allowed() {
    let got = verdicts(&[
        ("heredoc text for isohypse call is data", bash("isohypse call <<'EOF'\n{\"steps\":[{\"op\":\"context.find\",\"arg\":\"cat lib.rs\"}]}\nEOF")),
        ("file outside the workspace", bash("cat /outside/x.txt")),
        ("missing file", bash("cat nope.txt")),
        ("ls", bash("ls")),
        ("log redirect", bash("cargo test > test.log")),
        ("redirect to dev null", bash("echo x > /dev/null")),
        ("stderr merge", bash("cargo build 2>&1 | tail -3")),
        ("grep filter on a pipe", bash("git log | grep fix")),
        ("find outside", bash("find /usr -name x")),
        ("isohypse call from a file", bash("isohypse call < in.json")),
        ("isohypse status", bash("isohypse state status")),
        ("empty command", bash("")),
    ]);
    assert_eq!(got, expect(&[
        ("heredoc text for isohypse call is data", false), ("file outside the workspace", false), ("missing file", false), ("ls", false),
        ("log redirect", false), ("redirect to dev null", false), ("stderr merge", false), ("grep filter on a pipe", false), ("find outside", false),
        ("isohypse call from a file", false), ("isohypse status", false), ("empty command", false),
    ]));
}

#[test]
fn unknown_tools_and_malformed_input_are_allowed() {
    let got = verdicts(&[
        ("WebFetch", tool_event("WebFetch", ROOT, json!({"url": "https://example.com"}))),
        ("Bash without a command", tool_event("Bash", ROOT, json!({}))),
        ("not json", "not json".to_string()),
        ("json without cwd", json!({"tool_name": "Read"}).to_string()),
        ("empty object", "{}".to_string()),
    ]);
    assert_eq!(got, expect(&[("WebFetch", false), ("Bash without a command", false), ("not json", false), ("json without cwd", false), ("empty object", false)]));
}


#[test]
fn the_off_switch_allows_everything() {
    let probe = fake();
    assert_eq!(respond(&file("Read", "/repo/lib.rs"), true, &probe), "");
}



#[test]
fn session_start_briefs_inside_a_served_workspace() {
    let input = json!({"hook_event_name": "SessionStart", "cwd": ROOT}).to_string();
    assert!(respond(&input, false, &fake()).contains("additionalContext"));
}




