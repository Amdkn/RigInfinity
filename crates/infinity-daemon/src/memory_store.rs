//! Memory / session stores — native port of the durable layer of
//! `crates/infinity-daemon/src/memory_store.rs` (gel `e1a3e4b`).
//!
//! Ported: per-thread JSON-file persistence (`{dir}/{thread_id}.json`),
//! daemon-only per-thread extras (title, children, token totals,
//! timestamps, selected model), transient + persisted views, session
//! serialisation for migration, and the pending-choice state store.
//!
//! NOT ported (they live in `infinity-agent-core::stores`, which is not
//! part of this pas — providers / agent core are out of scope, D3):
//! spawn orders, ancestor-chain bookkeeping beyond the parent link,
//! compaction cutoffs, dedup sets, subscriptions. The seams are kept:
//! `ensure_root_thread`, `get_root_thread_id`, `get_open_subthreads`
//! behave as the daemon expects them to.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub struct MemoryError(pub String);
impl std::fmt::Display for MemoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for MemoryError {}

/// Daemon-only per-thread extras (upstream `ThreadExtras`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadExtras {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub total_tokens_used: usize,
    #[serde(default)]
    pub last_updated: String,
    #[serde(default)]
    pub children: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

/// One thread, persisted as `{dir}/{thread_id}.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ThreadRecord {
    #[serde(default)]
    pub messages: Vec<serde_json::Value>,
    #[serde(default)]
    pub extras: ThreadExtras,
    /// Serialised views (`view_type` -> payload), persisted with the thread.
    #[serde(default)]
    pub views: HashMap<String, serde_json::Value>,
}

/// Durable conversation store: threads as pure data on disk, loaded
/// lazily per thread, saved on change.
#[derive(Debug)]
pub struct PersistentConversationStore {
    dir: PathBuf,
    threads: HashMap<String, ThreadRecord>,
}

impl PersistentConversationStore {
    pub fn new_with_dir(dir: PathBuf) -> Self {
        Self {
            dir,
            threads: HashMap::new(),
        }
    }

    fn thread_path(&self, thread_id: &str) -> PathBuf {
        self.dir.join(format!("{thread_id}.json"))
    }

    fn load_thread(&mut self, thread_id: &str) -> &mut ThreadRecord {
        if !self.threads.contains_key(thread_id) {
            let record = std::fs::read_to_string(self.thread_path(thread_id))
                .ok()
                .and_then(|s| serde_json::from_str::<ThreadRecord>(&s).ok())
                .unwrap_or_default();
            self.threads.insert(thread_id.to_owned(), record);
        }
        self.threads
            .get_mut(thread_id)
            .expect("bug: thread just loaded")
    }

    fn save_thread(&self, thread_id: &str) {
        if let Some(record) = self.threads.get(thread_id)
            && let Err(e) = std::fs::create_dir_all(&self.dir).and_then(|()| {
                std::fs::write(
                    self.thread_path(thread_id),
                    serde_json::to_string_pretty(record)
                        .expect("bug: thread record must serialize"),
                )
            })
        {
            tracing::warn!("failed to persist thread {thread_id}: {e}");
        }
    }

    pub fn has_thread(&mut self, thread_id: &str) -> bool {
        if self.threads.contains_key(thread_id) {
            return true;
        }
        self.thread_path(thread_id).exists()
    }

    /// Ensure the root thread metadata exists (no-op when it already does).
    pub fn ensure_root_thread(&mut self, thread_id: &str) {
        self.load_thread(thread_id);
        self.save_thread(thread_id);
    }

    pub fn get_root_thread_id(&mut self, thread_id: &str) -> String {
        let mut current = thread_id.to_owned();
        loop {
            let parent = self
                .threads
                .get(&current)
                .and_then(|r| r.extras.parent.clone());
            match parent {
                Some(p) => current = p,
                None => return current,
            }
        }
    }

    pub fn get_thread_parent_id(&mut self, thread_id: &str) -> Option<String> {
        self.load_thread(thread_id).extras.parent.clone()
    }

    pub fn set_thread_title(&mut self, thread_id: &str, title: &str) {
        self.load_thread(thread_id).extras.title = Some(title.to_owned());
        self.save_thread(thread_id);
    }

    pub fn get_thread_title(&mut self, thread_id: &str) -> Option<String> {
        self.load_thread(thread_id).extras.title.clone()
    }

    pub fn set_thread_model(&mut self, thread_id: &str, model: &str) {
        self.load_thread(thread_id).extras.model = Some(model.to_owned());
        self.save_thread(thread_id);
    }

    pub fn get_thread_model(&mut self, thread_id: &str) -> Option<String> {
        self.load_thread(thread_id).extras.model.clone()
    }

    pub fn get_total_tokens_used(&mut self, thread_id: &str) -> usize {
        self.load_thread(thread_id).extras.total_tokens_used
    }

    pub fn set_total_tokens_used(&mut self, thread_id: &str, tokens: usize) {
        self.load_thread(thread_id).extras.total_tokens_used = tokens;
        self.save_thread(thread_id);
    }

    pub fn get_last_updated(&mut self, thread_id: &str) -> String {
        self.load_thread(thread_id).extras.last_updated.clone()
    }

    pub fn set_last_updated(&mut self, thread_id: &str, ts: &str) {
        self.load_thread(thread_id).extras.last_updated = ts.to_owned();
        self.save_thread(thread_id);
    }

    /// All thread ids belonging to the session rooted at `root_id`
    /// (the root plus its children, transitively).
    pub fn get_session_thread_ids(&mut self, root_id: &str) -> Vec<String> {
        let mut out = vec![root_id.to_owned()];
        let mut i = 0;
        while i < out.len() {
            let id = out[i].clone();
            let children = self.load_thread(&id).extras.children.clone();
            for child in children {
                if !out.contains(&child) {
                    out.push(child);
                }
            }
            i += 1;
        }
        out
    }

    /// Children of `root_id` that are still open (have a record on disk).
    pub fn get_open_subthreads(&mut self, root_id: &str) -> Vec<String> {
        self.get_session_thread_ids(root_id)
            .into_iter()
            .filter(|id| id != root_id)
            .filter(|id| self.has_thread(id))
            .collect()
    }

    pub fn append_message(&mut self, thread_id: &str, message: serde_json::Value) {
        self.load_thread(thread_id).messages.push(message);
        self.save_thread(thread_id);
    }

    pub fn set_view(&mut self, thread_id: &str, view_type: &str, content: serde_json::Value) {
        self.load_thread(thread_id)
            .views
            .insert(view_type.to_owned(), content);
        self.save_thread(thread_id);
    }

    pub fn get_views(&mut self, thread_id: &str) -> HashMap<String, serde_json::Value> {
        self.load_thread(thread_id).views.clone()
    }

    /// Serialise a whole session (root + children) for migration.
    pub fn serialize_session(&mut self, root_thread_id: &str) -> String {
        let ids = self.get_session_thread_ids(root_thread_id);
        let mut records = HashMap::new();
        for id in &ids {
            records.insert(id.clone(), self.load_thread(id).clone());
        }
        serde_json::to_string(&records).expect("bug: session records must serialize")
    }

    /// Import a session serialised by [`Self::serialize_session`].
    pub fn import_session(&mut self, data: &str) -> Result<(), MemoryError> {
        let records: HashMap<String, ThreadRecord> =
            serde_json::from_str(data).map_err(|e| MemoryError(e.to_string()))?;
        for (id, record) in records {
            self.threads.insert(id.clone(), record);
            self.save_thread(&id);
        }
        Ok(())
    }
}

/// A choice the agent asked the user to make, waiting for an answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingChoice {
    pub id: String,
    pub prompt: String,
    pub choices: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_url: Option<String>,
}

/// Durable state store: pending user choices per thread.
#[derive(Debug, Default)]
pub struct PersistentStateStore {
    pending: HashMap<String, Vec<PendingChoice>>,
}

impl PersistentStateStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_pending_choice(&mut self, thread_id: &str, choice: PendingChoice) {
        self.pending
            .entry(thread_id.to_owned())
            .or_default()
            .push(choice);
    }

    pub fn has_pending_choices(&self, thread_id: &str) -> bool {
        self.pending.get(thread_id).is_some_and(|v| !v.is_empty())
    }

    pub fn has_pending_choices_for_session(
        &mut self,
        session_id: &str,
        conv: &mut PersistentConversationStore,
    ) -> bool {
        conv.get_session_thread_ids(session_id)
            .iter()
            .any(|id| self.has_pending_choices(id))
    }

    pub fn pending_choice(&self, thread_id: &str, choice_id: &str) -> Option<PendingChoice> {
        self.pending
            .get(thread_id)?
            .iter()
            .find(|c| c.id == choice_id)
            .cloned()
    }

    pub fn clear_pending_choices_for_session(
        &mut self,
        session_id: &str,
        conv: &mut PersistentConversationStore,
    ) {
        for id in conv.get_session_thread_ids(session_id) {
            self.pending.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("riginfinity-memstore-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn conversation_store_persists_and_migrates() {
        let dir = tmp_dir();
        let mut store = PersistentConversationStore::new_with_dir(dir.clone());
        store.ensure_root_thread("root-1");
        store.set_thread_title("root-1", "Test session");
        store.set_thread_model("root-1", "provider1/model1");
        store.set_total_tokens_used("root-1", 42);
        store.set_last_updated("root-1", "2026-11-10T00:00:00Z");
        store.append_message(
            "root-1",
            serde_json::json!({"role": "user", "text": "hello"}),
        );
        store.set_view(
            "root-1",
            "diff",
            serde_json::json!({"path": "a.rs", "patch": "@@ -1 +1 @@\n"}),
        );
        // A second process reading the same directory sees the same data.
        let mut store2 = PersistentConversationStore::new_with_dir(dir.clone());
        assert!(store2.has_thread("root-1"));
        assert_eq!(
            store2.get_thread_title("root-1").as_deref(),
            Some("Test session")
        );
        assert_eq!(store2.get_total_tokens_used("root-1"), 42);
        assert_eq!(store2.get_last_updated("root-1"), "2026-11-10T00:00:00Z");
        assert_eq!(store2.get_views("root-1").len(), 1);
        // Migration round-trip.
        let blob = store2.serialize_session("root-1");
        let dir2 = tmp_dir();
        let mut store3 = PersistentConversationStore::new_with_dir(dir2.clone());
        store3.import_session(&blob).expect("import session");
        assert_eq!(
            store3.get_thread_title("root-1").as_deref(),
            Some("Test session")
        );
        assert_eq!(store3.get_total_tokens_used("root-1"), 42);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn state_store_pending_choices() {
        let mut state = PersistentStateStore::new();
        assert!(!state.has_pending_choices("t-1"));
        state.add_pending_choice(
            "t-1",
            PendingChoice {
                id: "c-1".into(),
                prompt: "Pick one".into(),
                choices: vec!["a".into(), "b".into()],
                response_url: None,
            },
        );
        assert!(state.has_pending_choices("t-1"));
        assert!(state.pending_choice("t-1", "c-1").is_some());
        assert!(state.pending_choice("t-1", "nope").is_none());
    }
}
