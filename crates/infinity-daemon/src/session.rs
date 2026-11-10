//! Session manager — native port of the lifecycle core of
//! `crates/infinity-daemon/src/session/mod.rs` (gel `e1a3e4b`).
//!
//! Ported: create / resume / list / cleanup, the input queue with
//! router admission (user text may create or resume a thread; event
//! input for unknown or stopped threads is dropped), the
//! idle -> shut_down hibernation flags, wake on user text, view
//! updates, pending-choice admission.
//!
//! NOT ported (they live in `infinity-agent-core` / the provider
//! crates, out of scope, D3): the agent driver itself (model calls,
//! tool execution, compaction). The manager owns the input queue and
//! the broadcast; any driver consumes the queue via
//! [`SessionManager::take_input_receiver`] — that is the seam where a
//! provider plugs in. A session with no driver is still a durable,
//! hibernating session: pure data between turns.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, broadcast, mpsc};

use crate::memory_store::{PersistentConversationStore, PersistentStateStore};
use crate::rap::RapToolResult;
use crate::rap_servers::SessionRapManager;
use crate::session_store::{SessionStatus, SessionStore};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type SharedSessionManager = Arc<Mutex<SessionManager>>;

/// Messages the daemon broadcasts to attached clients.
#[derive(Debug, Clone)]
pub enum DaemonMessage {
    Connected {
        root_thread_id: String,
        thread_id: String,
        model_name: String,
        title: Option<String>,
        total_tokens_used: usize,
    },
    Info {
        thread_id: Option<String>,
        text: String,
    },
    ViewUpdate {
        thread_id: String,
        view_type: String,
        content: serde_json::Value,
    },
    SessionListChanged,
}

/// Input into the agent system. The router admits user text for any
/// thread (it may create or resume one); every other kind is dropped
/// for unknown or stopped threads.
#[derive(Debug, Clone)]
pub enum InputMessage {
    /// User text — the only input that re-awakens a hibernated agent.
    UserText { thread_id: String, text: String },
    /// A tool result delivered by RAP (may arrive while the agent has
    /// no process — it waits in the outbox until wake).
    ToolResult {
        thread_id: String,
        result: RapToolResult,
    },
    /// A display-only view update (never enters agent history).
    ViewUpdate {
        thread_id: String,
        view_type: String,
        content: serde_json::Value,
    },
}

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn generate_session_id() -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    let c = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("sess-{n:x}-{c}-{pid}", pid = std::process::id())
}

pub struct SessionManager {
    state_dir: PathBuf,
    callback_url: String,
    session_store: Arc<Mutex<SessionStore>>,
    conversation_store: Arc<Mutex<PersistentConversationStore>>,
    state_store: Arc<Mutex<PersistentStateStore>>,
    rap_manager: SessionRapManager,
    input_tx: mpsc::UnboundedSender<InputMessage>,
    input_rx: Option<mpsc::UnboundedReceiver<InputMessage>>,
    broadcast_tx: broadcast::Sender<DaemonMessage>,
    /// Sessions whose RAP servers were shut down (diagnostics / tests).
    shutdown_log: std::sync::Mutex<Vec<String>>,
}

impl SessionManager {
    pub async fn new(state_dir: PathBuf, callback_url: String) -> Result<Self, BoxError> {
        std::fs::create_dir_all(&state_dir)?;
        let (change_tx, mut change_rx) = mpsc::unbounded_channel::<String>();
        let sessions_path = state_dir.join("sessions.json");
        let session_store = Arc::new(Mutex::new(SessionStore::load(
            sessions_path.to_string_lossy().as_ref(),
            change_tx,
        )));
        let (input_tx, input_rx) = mpsc::unbounded_channel();
        let (broadcast_tx, _) = broadcast::channel(256);
        let conversation_store = Arc::new(Mutex::new(PersistentConversationStore::new_with_dir(
            state_dir.join("threads"),
        )));
        let state_store = Arc::new(Mutex::new(PersistentStateStore::new()));
        let rap_manager = SessionRapManager::new();
        // Forward store changes to subscribers as list-changed events.
        let btx = broadcast_tx.clone();
        tokio::spawn(async move {
            while change_rx.recv().await.is_some() {
                let _ = btx.send(DaemonMessage::SessionListChanged);
            }
        });
        Ok(Self {
            state_dir,
            callback_url,
            session_store,
            conversation_store,
            state_store,
            rap_manager,
            input_tx,
            input_rx: Some(input_rx),
            broadcast_tx,
            shutdown_log: std::sync::Mutex::new(Vec::new()),
        })
    }

    pub fn callback_url(&self) -> &str {
        &self.callback_url
    }

    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    /// The agent system's input sender, used by the RAP callback bridge
    /// to forward converted callbacks directly into the input queue.
    pub fn input_sender(&self) -> mpsc::UnboundedSender<InputMessage> {
        self.input_tx.clone()
    }

    /// Take the input receiver once — the driver that consumes it.
    pub fn take_input_receiver(&mut self) -> Option<mpsc::UnboundedReceiver<InputMessage>> {
        self.input_rx.take()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonMessage> {
        self.broadcast_tx.subscribe()
    }

    /// Create a session: persist the entry, ensure the root thread,
    /// record the model, broadcast `Connected`.
    pub async fn create_session(&self, cwd: &Path, model: &str) -> Result<String, BoxError> {
        let session_id = generate_session_id();
        {
            let mut store = self.session_store.lock().await;
            store.create(&session_id, cwd.to_path_buf());
            store.save()?;
        }
        {
            let mut conv = self.conversation_store.lock().await;
            conv.ensure_root_thread(&session_id);
            conv.set_thread_model(&session_id, model);
            conv.set_last_updated(&session_id, &rfc3339_now());
        }
        let _ = self.broadcast_tx.send(DaemonMessage::Connected {
            root_thread_id: session_id.clone(),
            thread_id: session_id.clone(),
            model_name: model.to_owned(),
            title: None,
            total_tokens_used: 0,
        });
        Ok(session_id)
    }

    /// Resume a persisted session (does NOT boot the agent loop — that
    /// happens lazily on first user input via `send_input`).
    pub async fn resume_session(&self, session_id: &str) -> Result<(), BoxError> {
        let (title, tokens, model) = {
            let mut conv = self.conversation_store.lock().await;
            if !conv.has_thread(session_id) {
                return Err(format!("unknown session {session_id}").into());
            }
            (
                conv.get_thread_title(session_id),
                conv.get_total_tokens_used(session_id),
                conv.get_thread_model(session_id)
                    .unwrap_or_else(|| "default".to_owned()),
            )
        };
        let _ = self.broadcast_tx.send(DaemonMessage::Connected {
            root_thread_id: session_id.to_owned(),
            thread_id: session_id.to_owned(),
            model_name: model,
            title,
            total_tokens_used: tokens,
        });
        Ok(())
    }

    /// Send an input message to the agent system.
    ///
    /// Router admission (upstream rule): user text may create or resume
    /// a thread — a resulting wake clears the session's idle and stopped
    /// flags. Event-style input for unknown or stopped threads is
    /// dropped. Returns `true` iff the input was accepted.
    pub async fn send_input(&self, msg: InputMessage) -> bool {
        let thread_id = match &msg {
            InputMessage::UserText { thread_id, .. }
            | InputMessage::ToolResult { thread_id, .. }
            | InputMessage::ViewUpdate { thread_id, .. } => thread_id.clone(),
        };
        let is_user_text = matches!(msg, InputMessage::UserText { .. });
        {
            let mut store = self.session_store.lock().await;
            let known = store.sessions.contains_key(&thread_id);
            let stopped = store.sessions.get(&thread_id).is_some_and(|e| e.shut_down);
            if !known && !is_user_text {
                return false;
            }
            if stopped && !is_user_text {
                // RAP callbacks arriving while shut down are ignored.
                return false;
            }
            if is_user_text {
                // A live transition clears idle and stopped flags.
                store.clear_idle(&thread_id);
                let woke = store.clear_shut_down(&thread_id);
                if woke || known {
                    let _ = store.save();
                }
                if !known {
                    store.create(&thread_id, std::env::current_dir().unwrap_or_default());
                    let _ = store.save();
                }
            }
        }
        self.input_tx.send(msg).is_ok()
    }

    /// List sessions with their status (pending choices counted).
    pub async fn list_sessions(&self) -> Vec<(String, SessionStatus)> {
        let mut out = Vec::new();
        let store = self.session_store.lock().await;
        let mut state = self.state_store.lock().await;
        let mut conv = self.conversation_store.lock().await;
        for (id, entry) in store.sessions.iter() {
            let pending = state.has_pending_choices_for_session(id, &mut conv);
            out.push((id.clone(), entry.status(pending)));
        }
        out
    }

    /// Park a session: clear its pending choices, stop its RAP servers,
    /// mark it shut down and persist. The agent system keeps running —
    /// a thread mid-completion finishes and flushes its turn, then parks.
    pub async fn cleanup_session(&self, session_id: &str) {
        {
            let mut state = self.state_store.lock().await;
            let mut conv = self.conversation_store.lock().await;
            state.clear_pending_choices_for_session(session_id, &mut conv);
        }
        self.rap_manager.evict_session(session_id).await;
        self.shutdown_log
            .lock()
            .expect("bug: shutdown log lock poisoned")
            .push(session_id.to_owned());
        let mut store = self.session_store.lock().await;
        if store.sessions.contains_key(session_id) {
            store.mark_shut_down(session_id);
            let _ = store.save();
            tracing::info!("Cleanup complete for {session_id}");
        } else {
            tracing::warn!("Session not found: {session_id}");
        }
    }

    /// Returns true if the session has no active threads (no live driver
    /// and no pending choices): the single definition of idleness.
    pub async fn is_session_idle(&self, session_id: &str) -> bool {
        let store = self.session_store.lock().await;
        let entry = match store.sessions.get(session_id) {
            Some(e) => e,
            None => return true,
        };
        if !entry.idle && !entry.shut_down {
            return false;
        }
        let mut state = self.state_store.lock().await;
        let mut conv = self.conversation_store.lock().await;
        !state.has_pending_choices_for_session(session_id, &mut conv)
    }

    /// Persist a display-only view update and broadcast it.
    pub async fn handle_view_update(
        &self,
        group_id: &str,
        view_type: &str,
        content: serde_json::Value,
    ) {
        {
            let mut conv = self.conversation_store.lock().await;
            if conv.has_thread(group_id) {
                conv.set_view(group_id, view_type, content.clone());
            }
        }
        let _ = self.broadcast_tx.send(DaemonMessage::ViewUpdate {
            thread_id: group_id.to_owned(),
            view_type: view_type.to_owned(),
            content,
        });
    }

    /// How many times the session's RAP servers were shut down.
    pub fn times_shut_down(&self, session_id: &str) -> usize {
        self.shutdown_log
            .lock()
            .expect("bug: shutdown log lock poisoned")
            .iter()
            .filter(|s| *s == session_id)
            .count()
    }

    /// Stop every managed RAP server (daemon exit).
    pub async fn shutdown_all_rap(&self) {
        self.rap_manager.shutdown_all().await;
    }

    /// Conversation store (for the RAP manager and tests).
    pub fn conversation_store(&self) -> Arc<Mutex<PersistentConversationStore>> {
        self.conversation_store.clone()
    }

    /// State store (for the RAP manager and tests).
    pub fn state_store(&self) -> Arc<Mutex<PersistentStateStore>> {
        self.state_store.clone()
    }
}

fn rfc3339_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    // Seconds since epoch, formatted as RFC 3339 UTC without sub-second
    // precision (no chrono dependency in the native crate).
    let (y, mo, d, h, mi, s) = epoch_to_civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

fn epoch_to_civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, mi, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    (y as u64, mo, d, h, mi, s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_store::SessionStatus as StoreStatus;

    fn tmp_state_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "riginfinity-session-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[tokio::test]
    async fn hibernation_lifecycle_through_manager() {
        let dir = tmp_state_dir("hibernation");
        let mgr = SessionManager::new(dir.clone(), "http://127.0.0.1:0/cb".into())
            .await
            .expect("create manager");
        let id = mgr
            .create_session(Path::new("/tmp"), "provider1/model1")
            .await
            .expect("create session");
        // Idle, then shut down: the two-step hibernation.
        {
            let mut store = mgr.session_store.lock().await;
            store.mark_idle(&id);
            store.mark_shut_down(&id);
            let _ = store.save();
            // Upstream precedence: idle reads before shut_down, so the
            // listed status is Idle — the flags prove the hibernation.
            assert!(store.sessions[&id].is_hibernating());
            assert!(store.sessions[&id].idle);
            assert!(store.sessions[&id].shut_down);
        }
        let sessions = mgr.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].1, StoreStatus::Idle);
        assert!(mgr.is_session_idle(&id).await);
        // A tool result arriving while shut down is dropped.
        let accepted = mgr
            .send_input(InputMessage::ToolResult {
                thread_id: id.clone(),
                result: RapToolResult {
                    group_id: id.clone(),
                    id: "inv-1".into(),
                    call_id: None,
                    text: Some("late result".into()),
                    content: None,
                    display_as: None,
                    subscription: None,
                },
            })
            .await;
        assert!(!accepted, "RAP callbacks while shut down are ignored");
        // User text re-awakens.
        let accepted = mgr
            .send_input(InputMessage::UserText {
                thread_id: id.clone(),
                text: "wake up".into(),
            })
            .await;
        assert!(accepted);
        let sessions = mgr.list_sessions().await;
        assert_eq!(sessions[0].1, StoreStatus::Running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cleanup_parks_session_durably() {
        let dir = tmp_state_dir("cleanup");
        let mgr = SessionManager::new(dir.clone(), "http://127.0.0.1:0/cb".into())
            .await
            .expect("create manager");
        let id = mgr
            .create_session(Path::new("/tmp"), "provider1/model1")
            .await
            .expect("create session");
        mgr.cleanup_session(&id).await;
        assert_eq!(mgr.times_shut_down(&id), 1);
        // A fresh manager on the same directory sees the parked session.
        let mgr2 = SessionManager::new(dir.clone(), "http://127.0.0.1:0/cb".into())
            .await
            .expect("reopen manager");
        let sessions = mgr2.list_sessions().await;
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].1, StoreStatus::Stopped);
        // ... and resumes it on user text.
        assert!(
            mgr2.send_input(InputMessage::UserText {
                thread_id: id.clone(),
                text: "continue".into(),
            })
            .await
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
