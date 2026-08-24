use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::OnceLock;

use serde_json::Value;

const MAX_MERGED_BYTES: u64 = 16 * 1024 * 1024;

pub fn rotate_if_large(path: &Path, max_bytes: u64) {
    let too_large = std::fs::metadata(path).map(|meta| meta.len() > max_bytes).unwrap_or(false);
    if !too_large {
        return;
    }
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".prev");
    let _ = std::fs::rename(path, PathBuf::from(rotated));
}

struct TraceMsg {
    session: Option<String>,
    entry: Value,
}

static SENDER: OnceLock<Sender<TraceMsg>> = OnceLock::new();

fn store_root() -> PathBuf {
    std::env::var("ISOHYPSE_STORE").map(PathBuf::from).unwrap_or_else(|_| {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".isohypse")
    })
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn append(path: &Path, line: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = file.write_all(line.as_bytes());
    }
}

pub fn record(session: Option<&str>, entry: Value) {
    let sender = SENDER.get_or_init(|| {
        let (tx, rx) = channel::<TraceMsg>();
        std::thread::spawn(move || {
            let dir = store_root().join("trace");
            let _ = std::fs::create_dir_all(&dir);
            let merged = dir.join("merged.jsonl");
            while let Ok(msg) = rx.recv() {
                let mut line = msg.entry.to_string();
                line.push('\n');
                rotate_if_large(&merged, MAX_MERGED_BYTES);
                append(&merged, &line);
                if let Some(session) = &msg.session {
                    append(&dir.join(format!("{}.jsonl", sanitize(session))), &line);
                }
            }
        });
        tx
    });
    let _ = sender.send(TraceMsg { session: session.map(String::from), entry });
}

pub fn archive(session: &str) {
    let dir = store_root().join("trace");
    let src = dir.join(format!("{}.jsonl", sanitize(session)));
    let Ok(data) = std::fs::read(&src) else {
        return;
    };
    if data.is_empty() {
        let _ = std::fs::remove_file(&src);
        return;
    }
    let archive_dir = dir.join("archive");
    let _ = std::fs::create_dir_all(&archive_dir);
    let dest = archive_dir.join(format!("{}.jsonl.gz", sanitize(session)));
    if let Ok(file) = std::fs::File::create(&dest) {
        let mut encoder = flate2::write::GzEncoder::new(file, flate2::Compression::best());
        if encoder.write_all(&data).and_then(|_| encoder.finish().map(|_| ())).is_ok() {
            let _ = std::fs::remove_file(&src);
        }
    }
}
