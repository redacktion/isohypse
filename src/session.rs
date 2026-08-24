use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;

use serde_json::Value;

use crate::frame::{Outbound, Resume};

pub struct Session {
    pub id: String,
    pub label: String,
    pub observer: bool,
    token_salt: String,
    token_hash: String,
    pub scope_kinds: Vec<String>,
    pub scope_workspaces: Vec<String>,
    pub in_flight: HashSet<String>,
    event_tx: Option<mpsc::Sender<Outbound>>,
}

pub fn command_key(id: &Value) -> String {
    id.to_string()
}

pub struct SessionRegistry {
    seq: AtomicU64,
    counter: AtomicU64,
    sessions: Mutex<HashMap<String, Session>>,
}

impl Default for SessionRegistry {
    fn default() -> SessionRegistry {
        SessionRegistry::new()
    }
}

impl SessionRegistry {
    pub fn new() -> SessionRegistry {
        SessionRegistry {
            seq: AtomicU64::new(1),
            counter: AtomicU64::new(0),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    pub fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst)
    }

    fn random_hex(&self, bytes: usize) -> String {
        let mut buffer = vec![0u8; bytes];
        getrandom::getrandom(&mut buffer).expect("system CSPRNG unavailable");
        buffer.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn open(&self, label: &str, observer: bool, resume: Option<Resume>) -> Result<Outbound, String> {
        if let Some(resume) = resume {
            return self.resume(resume);
        }
        const MAX_SESSIONS: usize = 4096;
        if self.count() >= MAX_SESSIONS {
            return Err("session limit reached; close idle sessions before opening more".to_string());
        }
        let suffix = self.counter.fetch_add(1, Ordering::SeqCst);
        let label = sanitize_label(label);
        let id = format!("{label}.{:04x}", suffix & 0xffff);
        let token = self.random_hex(16);
        let token_salt = self.random_hex(16);
        let token_hash = hash_token(&token_salt, &token);
        let session = Session {
            id: id.clone(),
            label: label.clone(),
            observer,
            token_salt: token_salt.clone(),
            token_hash: token_hash.clone(),
            scope_kinds: Vec::new(),
            scope_workspaces: Vec::new(),
            in_flight: HashSet::new(),
            event_tx: None,
        };
        crate::objects::persist_session(&crate::objects::PersistedSession {
            id: id.clone(),
            label,
            observer,
            scope_kinds: Vec::new(),
            scope_workspaces: Vec::new(),
            token_salt,
            token_hash,
        });
        self.with_sessions(|sessions| {
            sessions.insert(id.clone(), session);
        })?;
        Ok(Outbound::Ready { session: id, token, seq: self.next_seq() })
    }

    pub fn attach_sink(&self, id: &str, tx: mpsc::Sender<Outbound>) {
        let _ = self.with_sessions(|sessions| {
            if let Some(session) = sessions.get_mut(id) {
                session.event_tx = Some(tx);
            }
        });
    }

    pub fn publish(&self, kind: &str, workspace: Option<&str>, payload: &Value) {
        crate::trace::record(None, serde_json::json!({"phase": "event", "kind": kind, "workspace": workspace}));
        let Ok(sessions) = self.sessions.lock() else {
            return;
        };
        for session in sessions.values() {
            let Some(tx) = &session.event_tx else {
                continue;
            };
            if !session.scope_kinds.iter().any(|k| k == kind) {
                continue;
            }
            if !session.scope_workspaces.is_empty() {
                let keep = matches!(workspace, Some(w) if session.scope_workspaces.iter().any(|s| s == w));
                if !keep {
                    continue;
                }
            }
            let seq = self.seq.fetch_add(1, Ordering::SeqCst);
            let event = Outbound::Event {
                seq,
                kind: kind.to_string(),
                workspace: workspace.map(String::from),
                payload: payload.clone(),
            };
            if tx.try_send(event).is_err() {
                let seq = self.seq.fetch_add(1, Ordering::SeqCst);
                let _ = tx.try_send(Outbound::Gap { seq, dropped: 1 });
            }
        }
    }

    fn resume(&self, resume: Resume) -> Result<Outbound, String> {
        let in_memory = self.with_sessions(|sessions| {
            sessions.get(&resume.session).map(|session| {
                constant_time_eq(
                    hash_token(&session.token_salt, &resume.token).as_bytes(),
                    session.token_hash.as_bytes(),
                )
            })
        })?;
        match in_memory {
            Some(true) => {
                self.with_sessions(|sessions| {
                    if let Some(session) = sessions.get_mut(&resume.session) {
                        session.in_flight.clear();
                    }
                })?;
                return Ok(Outbound::Ready { session: resume.session, token: resume.token, seq: self.next_seq() });
            }
            Some(false) => return Err("resume token does not match".to_string()),
            None => {}
        }
        let Some(persisted) = crate::objects::load_session(&resume.session) else {
            return Err(format!("no session {} to resume", resume.session));
        };
        if !constant_time_eq(
            hash_token(&persisted.token_salt, &resume.token).as_bytes(),
            persisted.token_hash.as_bytes(),
        ) {
            return Err("resume token does not match".to_string());
        }
        let session = Session {
            id: persisted.id.clone(),
            label: persisted.label,
            observer: persisted.observer,
            token_salt: persisted.token_salt,
            token_hash: persisted.token_hash,
            scope_kinds: persisted.scope_kinds,
            scope_workspaces: persisted.scope_workspaces,
            in_flight: HashSet::new(),
            event_tx: None,
        };
        self.with_sessions(|sessions| {
            sessions.insert(persisted.id.clone(), session);
        })?;
        Ok(Outbound::Ready { session: resume.session, token: resume.token, seq: self.next_seq() })
    }

    pub fn scope(&self, id: &str, kinds: Vec<String>, workspaces: Vec<String>) -> Result<(), String> {
        self.with_sessions(|sessions| {
            if let Some(session) = sessions.get_mut(id) {
                session.scope_kinds = kinds;
                session.scope_workspaces = workspaces;
                crate::objects::persist_session(&crate::objects::PersistedSession {
                    id: session.id.clone(),
                    label: session.label.clone(),
                    observer: session.observer,
                    scope_kinds: session.scope_kinds.clone(),
                    scope_workspaces: session.scope_workspaces.clone(),
                    token_salt: session.token_salt.clone(),
                    token_hash: session.token_hash.clone(),
                });
            }
        })
    }

    pub fn begin(&self, id: &str, command: &Value) -> Result<(), String> {
        self.with_sessions(|sessions| {
            if let Some(session) = sessions.get_mut(id) {
                session.in_flight.insert(command_key(command));
            }
        })
    }

    pub fn finish(&self, id: &str, command: &Value) -> Result<(), String> {
        self.with_sessions(|sessions| {
            if let Some(session) = sessions.get_mut(id) {
                session.in_flight.remove(&command_key(command));
            }
        })
    }

    pub fn in_flight(&self, id: &str) -> usize {
        self.with_sessions(|sessions| sessions.get(id).map(|s| s.in_flight.len()).unwrap_or(0))
            .unwrap_or(0)
    }

    pub fn reset_in_flight(&self, id: &str) -> usize {
        self.with_sessions(|sessions| {
            sessions
                .get_mut(id)
                .map(|session| {
                    let cleared = session.in_flight.len();
                    session.in_flight.clear();
                    cleared
                })
                .unwrap_or(0)
        })
        .unwrap_or(0)
    }

    pub fn is_observer(&self, id: &str) -> bool {
        self.with_sessions(|sessions| sessions.get(id).map(|s| s.observer).unwrap_or(false))
            .unwrap_or(false)
    }

    pub fn close(&self, id: &str) {
        let _ = self.with_sessions(|sessions| {
            sessions.remove(id);
        });
        crate::objects::forget_session(id);
    }

    pub fn count(&self) -> usize {
        self.with_sessions(|sessions| sessions.len()).unwrap_or(0)
    }

    fn with_sessions<T>(&self, f: impl FnOnce(&mut HashMap<String, Session>) -> T) -> Result<T, String> {
        let mut guard = self.sessions.lock().map_err(|_| "session registry poisoned".to_string())?;
        Ok(f(&mut guard))
    }
}

fn hash_token(salt: &str, token: &str) -> String {
    blake3::hash(format!("{salt}:{token}").as_bytes()).to_hex().to_string()
}

fn sanitize_label(label: &str) -> String {
    let cleaned: String = label
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(64)
        .collect();
    if cleaned.is_empty() {
        "session".to_string()
    } else {
        cleaned
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ready_parts(frame: Outbound) -> (String, String, u64) {
        match frame {
            Outbound::Ready { session, token, seq } => (session, token, seq),
            other => panic!("expected ready, got {other:?}"),
        }
    }

    #[test]
    fn open_mints_unique_ids_for_the_same_label() {
        let registry = SessionRegistry::new();
        let (a, _, _) = ready_parts(registry.open("codex", false, None).unwrap());
        let (b, _, _) = ready_parts(registry.open("codex", false, None).unwrap());
        assert_ne!(a, b, "same label must get distinct ids: {a} vs {b}");
        assert!(a.starts_with("codex."), "id carries the label: {a}");
        assert_eq!(registry.count(), 2);
    }

    #[test]
    fn resume_requires_the_matching_token() {
        let registry = SessionRegistry::new();
        let (id, token, _) = ready_parts(registry.open("agent", false, None).unwrap());

        let good = registry.open("agent", false, Some(Resume { session: id.clone(), token: token.clone() }));
        assert!(good.is_ok(), "correct token resumes: {good:?}");

        let bad = registry.open("agent", false, Some(Resume { session: id.clone(), token: "wrong".to_string() }));
        assert!(bad.is_err(), "wrong token is refused");

        let missing = registry.open("agent", false, Some(Resume { session: "ghost.0".to_string(), token }));
        assert!(missing.is_err(), "unknown session is refused");
    }

    #[test]
    fn sequence_is_monotonic_and_shared() {
        let registry = SessionRegistry::new();
        let first = registry.next_seq();
        let second = registry.next_seq();
        let third = registry.next_seq();
        assert!(first < second && second < third, "{first} {second} {third}");
    }

    #[test]
    fn in_flight_tracks_begin_and_finish() {
        let registry = SessionRegistry::new();
        let (id, _, _) = ready_parts(registry.open("worker", false, None).unwrap());
        registry.begin(&id, &json!(1)).unwrap();
        registry.begin(&id, &json!(2)).unwrap();
        assert_eq!(registry.in_flight(&id), 2);
        registry.finish(&id, &json!(1)).unwrap();
        assert_eq!(registry.in_flight(&id), 1);
    }
}
