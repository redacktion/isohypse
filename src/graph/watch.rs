use std::path::PathBuf;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use notify::{Config, Event, PollWatcher, RecursiveMode, Watcher};

use crate::graph::resolve::{is_indexable, GraphIndex};

pub struct GraphWatcher {
    _watcher: Box<dyn Watcher + Send>,
    pub thread: std::thread::JoinHandle<()>,
}

pub fn spawn(
    index: Arc<Mutex<GraphIndex>>,
    root: PathBuf,
    sessions: Arc<crate::session::SessionRegistry>,
    workspace: String,
) -> Result<GraphWatcher, String> {
    let (sender, receiver) = channel::<Vec<PathBuf>>();
    let handler = move |result: Result<Event, notify::Error>| {
        if let Ok(event) = result {
            let paths: Vec<PathBuf> = event.paths.into_iter().filter(|p| is_indexable(p)).collect();
            if !paths.is_empty() {
                let _ = sender.send(paths);
            }
        }
    };
    let mut watcher: Box<dyn Watcher + Send> = if std::env::var("ISOHYPSE_WATCH_POLL").is_ok() {
        Box::new(
            PollWatcher::new(handler, Config::default().with_poll_interval(Duration::from_secs(2)))
                .map_err(|e| e.to_string())?,
        )
    } else {
        Box::new(notify::recommended_watcher(handler).map_err(|e| e.to_string())?)
    };
    watcher.watch(&root, RecursiveMode::Recursive).map_err(|e| e.to_string())?;

    let thread = std::thread::spawn(move || {
        let mut pending: Vec<PathBuf> = Vec::new();
        loop {
            match receiver.recv_timeout(Duration::from_millis(300)) {
                Ok(mut paths) => pending.append(&mut paths),
                Err(RecvTimeoutError::Timeout) => {
                    if pending.is_empty() {
                        continue;
                    }
                    pending.sort();
                    pending.dedup();
                    let changed: Vec<PathBuf> = std::mem::take(&mut pending);
                    if let Ok(mut index) = index.lock() {
                        for path in &changed {
                            index.update_file(path);
                        }
                    }
                    for path in &changed {
                        let rel = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().into_owned();
                        sessions.publish("file-changed", Some(&workspace), &serde_json::json!({ "path": rel }));
                    }
                    sessions.publish("index-updated", Some(&workspace), &serde_json::json!({ "files": changed.len() }));
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    Ok(GraphWatcher { _watcher: watcher, thread })
}
