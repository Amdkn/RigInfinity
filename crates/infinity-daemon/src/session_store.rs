//! Session store — native port of
//! `crates/infinity-daemon/src/session_store.rs` (gel `e1a3e4b`).
//!
//! Same data model (`SessionEntry`: `cwd`, `shut_down`, `idle`,
//! `archived`), same serde shape, same two-step hibernation
//! (`idle` -> `shut_down`, wake on user text input only). Thread ids
//! are plain `String`s here (the upstream `ThreadId` newtype lives in
//! `infinity-agent-core`, which is not ported — providers are out of
//! scope, D3).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Hibernation state of one agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SessionStatus {
    Running,
    Idle,
    Stopped,
    Archived,
    WaitingForChoice,
}

/// One durable session: pure data between turns.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub cwd: PathBuf,
    /// When true, only user text input should re-awaken the agent.
    #[serde(default)]
    pub shut_down: bool,
    /// When true, the agent is idle (no active work).
    #[serde(default)]
    pub idle: bool,
    /// When true, the session has been migrated away and should not be displayed.
    #[serde(default)]
    pub archived: bool,
}

impl SessionEntry {
    pub fn status(&self, has_pending_choices: bool) -> SessionStatus {
        if self.archived {
            SessionStatus::Archived
        } else if has_pending_choices {
            SessionStatus::WaitingForChoice
        } else if self.idle {
            SessionStatus::Idle
        } else if self.shut_down {
            SessionStatus::Stopped
        } else {
            SessionStatus::Running
        }
    }

    /// The agent is pure data between turns: hibernating, costing nothing
    /// until the next event arrives.
    pub fn is_hibernating(&self) -> bool {
        self.shut_down && !self.archived
    }
}

/// Durable map of sessions, persisted as JSON at `path`.
#[derive(Serialize, Deserialize)]
pub struct SessionStore {
    pub sessions: HashMap<String, SessionEntry>,
    #[serde(skip)]
    path: String,
    #[serde(skip)]
    change_tx: Option<mpsc::UnboundedSender<String>>,
}

impl SessionStore {
    pub fn load(path: &str, change_tx: mpsc::UnboundedSender<String>) -> Self {
        let sessions = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| {
                // New HashMap format first.
                if let Ok(store) = serde_json::from_str::<Self>(&s) {
                    return Some(store.sessions);
                }
                // Legacy Vec format.
                #[derive(Deserialize)]
                struct LegacyEntry {
                    thread_id: String,
                }
                #[derive(Deserialize)]
                struct LegacyStore {
                    sessions: Vec<LegacyEntry>,
                }
                if let Ok(legacy) = serde_json::from_str::<LegacyStore>(&s) {
                    let map = legacy
                        .sessions
                        .into_iter()
                        .map(|e| {
                            (
                                e.thread_id,
                                SessionEntry {
                                    cwd: std::env::current_dir()
                                        .expect("bug: failed to get current directory"),
                                    shut_down: false,
                                    idle: false,
                                    archived: false,
                                },
                            )
                        })
                        .collect();
                    return Some(map);
                }
                None
            })
            .unwrap_or_default();

        Self {
            sessions,
            path: path.to_owned(),
            change_tx: Some(change_tx),
        }
    }

    pub fn save(&self) -> Result<(), BoxError> {
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, json)?;
        Ok(())
    }

    pub fn notify(&self, session_id: &str) {
        if let Some(ref tx) = self.change_tx {
            let _ = tx.send(session_id.to_owned());
        }
    }

    pub fn create(&mut self, session_id: &str, cwd: PathBuf) {
        self.sessions.insert(
            session_id.to_owned(),
            SessionEntry {
                cwd,
                shut_down: false,
                idle: false,
                archived: false,
            },
        );
        self.notify(session_id);
    }

    pub fn get_cwd(&self, session_id: &str) -> &PathBuf {
        &self
            .sessions
            .get(session_id)
            .expect("bug: session not found in store")
            .cwd
    }

    pub fn mark_shut_down(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.shut_down = true;
            self.notify(session_id);
        }
    }

    /// Clear the shut-down flag. Returns `true` iff the session was shut
    /// down — i.e. iff user text actually re-awakened it (H-P3-3).
    pub fn clear_shut_down(&mut self, session_id: &str) -> bool {
        tracing::trace!("Clearing shut down status for {session_id}");
        if let Some(entry) = self.sessions.get_mut(session_id)
            && entry.shut_down
        {
            entry.shut_down = false;
            self.notify(session_id);
            return true;
        }
        false
    }

    pub fn mark_idle(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && !entry.idle
        {
            entry.idle = true;
            self.notify(session_id);
        }
    }

    pub fn clear_idle(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id)
            && entry.idle
        {
            entry.idle = false;
            self.notify(session_id);
        }
    }

    pub fn mark_archived(&mut self, session_id: &str) {
        if let Some(entry) = self.sessions.get_mut(session_id) {
            entry.archived = true;
            self.notify(session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SessionStore {
        let (tx, _rx) = mpsc::unbounded_channel();
        SessionStore {
            sessions: HashMap::new(),
            path: String::new(),
            change_tx: Some(tx),
        }
    }

    #[test]
    fn hibernation_two_step_and_wake_on_user_text_only() {
        let mut s = store();
        s.create("t-1", PathBuf::from("/tmp"));
        let e = s.sessions.get("t-1").expect("session exists");
        assert_eq!(e.status(false), SessionStatus::Running);
        s.mark_idle("t-1");
        assert_eq!(s.sessions["t-1"].status(false), SessionStatus::Idle);
        assert!(!s.sessions["t-1"].is_hibernating());
        s.mark_shut_down("t-1");
        // Upstream precedence: `idle` is checked before `shut_down`,
        // so the status still reads Idle — but the session IS hibernating.
        assert_eq!(s.sessions["t-1"].status(false), SessionStatus::Idle);
        assert!(s.sessions["t-1"].is_hibernating());
        assert!(s.clear_shut_down("t-1"));
        assert!(!s.clear_shut_down("t-1"));
        assert_eq!(s.sessions["t-1"].status(false), SessionStatus::Idle);
    }

    #[test]
    fn session_entry_serde_round_trips_like_upstream() {
        let entry = SessionEntry {
            cwd: PathBuf::from("/tmp"),
            shut_down: true,
            idle: true,
            archived: false,
        };
        let json = serde_json::to_string(&entry).expect("serialize entry");
        let back: SessionEntry = serde_json::from_str(&json).expect("deserialize entry");
        assert_eq!(back, entry);
    }
}
