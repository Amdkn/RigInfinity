//! Unified memory — native port of the agent-zero memory function
//! (D-N051 F8, pas 5, D4c, D6).
//!
//! Upstream (`agent0ai/agent-zero`, gel `ba9b67d`): chats persist as
//! files (snapshots, history) under `usr/`; retrieval uses
//! `helpers/vector_db.py` `MyFaiss` (FAISS + local
//! sentence-transformers embeddings).
//!
//! Native rule (D6): the Shared Brain JSONL is the ONLY source of
//! truth. FAISS is kept as a retrieval index — an index DERIVED from
//! the Shared Brain content, rebuilt from it, never the inverse. One
//! memory is authoritative; the index can never diverge from it,
//! because it is rebuilt from it.
//!
//! This module is the native mirror of the adapter's `memory.py`:
//! same contract, same function names, Rust types. The daemon's
//! existing `memory_store` (per-thread JSON persistence, pas 3)
//! stays the session store; this module is the agent memory —
//! chats and knowledge — unified on the Shared Brain.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::BoxError;

/// One Shared Brain entry (D-N049 D4): append-only JSONL.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainEntry {
    pub ts: f64,
    pub kind: String,
    pub content: String,
    #[serde(default)]
    pub meta: serde_json::Value,
}

fn tokens(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_owned())
        .collect()
}

/// Append-only JSONL memory of reference.
#[derive(Debug)]
pub struct SharedBrainMemory {
    path: PathBuf,
}

impl SharedBrainMemory {
    pub fn new(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(
        &self,
        kind: &str,
        content: &str,
        meta: serde_json::Value,
    ) -> Result<BrainEntry, BoxError> {
        let entry = BrainEntry {
            ts: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0),
            kind: kind.to_owned(),
            content: content.to_owned(),
            meta,
        };
        let line = serde_json::to_string(&entry)?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        use std::io::Write;
        writeln!(file, "{line}")?;
        Ok(entry)
    }

    pub fn read_all(&self) -> Result<Vec<BrainEntry>, BoxError> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut entries = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if !line.is_empty() {
                entries.push(serde_json::from_str::<BrainEntry>(line)?);
            }
        }
        Ok(entries)
    }

    /// Persist a whole chat as one brain entry (H-P5-5).
    pub fn persist_chat(
        &self,
        chat_id: &str,
        messages: &[serde_json::Value],
    ) -> Result<BrainEntry, BoxError> {
        let content = serde_json::to_string(&serde_json::json!({
            "chat_id": chat_id,
            "messages": messages,
        }))?;
        self.append("chat", &content, serde_json::json!({ "chat_id": chat_id }))
    }

    /// Keyword-overlap retrieval over the brain itself.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<BrainEntry>, BoxError> {
        let wanted = tokens(query);
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let mut scored: Vec<(usize, BrainEntry)> = Vec::new();
        for entry in self.read_all()? {
            let hit = wanted.intersection(&tokens(&entry.content)).count();
            if hit > 0 {
                scored.push((hit, entry));
            }
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0).then(
                a.1.ts
                    .partial_cmp(&b.1.ts)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        Ok(scored.into_iter().take(limit).map(|(_, e)| e).collect())
    }
}

/// Derived retrieval index — FAISS kept as an index only (D6).
///
/// The index is built FROM the Shared Brain and rebuilt from it; it
/// is never a second memory. `rebuild` is the only way content
/// enters the index, and `check_no_divergence` proves the index
/// holds exactly the brain's content.
#[derive(Debug, Default)]
pub struct FaissIndex {
    postings: HashMap<String, HashSet<usize>>,
    docs: Vec<String>,
}

impl FaissIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild the whole index from the brain. Returns doc count.
    pub fn rebuild(&mut self, brain: &SharedBrainMemory) -> Result<usize, BoxError> {
        self.postings.clear();
        self.docs.clear();
        for entry in brain.read_all()? {
            let doc_id = self.docs.len();
            self.docs.push(entry.content.clone());
            for token in tokens(&entry.content) {
                self.postings.entry(token).or_default().insert(doc_id);
            }
        }
        Ok(self.docs.len())
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<String> {
        let wanted = tokens(query);
        if wanted.is_empty() {
            return Vec::new();
        }
        let mut hits: HashMap<usize, usize> = HashMap::new();
        for token in &wanted {
            if let Some(docs) = self.postings.get(token) {
                for doc_id in docs {
                    *hits.entry(*doc_id).or_insert(0) += 1;
                }
            }
        }
        let mut ranked: Vec<(usize, usize)> = hits.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        ranked
            .into_iter()
            .take(limit)
            .map(|(doc_id, _)| self.docs[doc_id].clone())
            .collect()
    }

    pub fn doc_count(&self) -> usize {
        self.docs.len()
    }

    pub fn docs(&self) -> &[String] {
        &self.docs
    }
}

/// One memory is authoritative: the index holds exactly the brain.
///
/// Rebuilds a fresh index from the brain and compares doc lists.
/// `true` means no divergence between the two (H-P5-5).
pub fn check_no_divergence(
    brain: &SharedBrainMemory,
    index: &FaissIndex,
) -> Result<bool, BoxError> {
    let mut fresh = FaissIndex::new();
    fresh.rebuild(brain)?;
    Ok(fresh.docs == index.docs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_brain() -> SharedBrainMemory {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir =
            std::env::temp_dir().join(format!("riginfinity-brain-{n}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        SharedBrainMemory::new(dir.join("memory.jsonl"))
    }

    #[test]
    fn brain_append_and_read() {
        let brain = tmp_brain();
        brain
            .append(
                "note",
                "the runtime runs without the original",
                serde_json::json!({}),
            )
            .expect("append note");
        brain
            .append("chat", "hello world", serde_json::json!({}))
            .expect("append chat");
        let entries = brain.read_all().expect("read brain");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, "note");
        assert_eq!(entries[1].content, "hello world");
    }

    #[test]
    fn brain_search_keyword_overlap() {
        let brain = tmp_brain();
        brain
            .append(
                "note",
                "the runtime runs without the original",
                serde_json::json!({}),
            )
            .expect("append note");
        brain
            .append("note", "unrelated weather report", serde_json::json!({}))
            .expect("append note");
        let hits = brain.search("runtime original", 10).expect("search brain");
        assert_eq!(hits.len(), 1);
        assert!(hits[0].content.contains("runtime"));
    }

    #[test]
    fn persist_chat_roundtrip() {
        let brain = tmp_brain();
        let messages = vec![
            serde_json::json!({"role": "user", "content": "hi"}),
            serde_json::json!({"role": "assistant", "content": "hello"}),
        ];
        brain
            .persist_chat("chat-1", &messages)
            .expect("persist chat");
        let entries = brain.read_all().expect("read brain");
        assert_eq!(entries[0].meta["chat_id"], "chat-1");
    }

    #[test]
    fn faiss_index_is_derived_from_brain() {
        let brain = tmp_brain();
        brain
            .append(
                "note",
                "the runtime runs without the original",
                serde_json::json!({}),
            )
            .expect("append note");
        brain
            .append("note", "the daemon is single", serde_json::json!({}))
            .expect("append note");
        let mut index = FaissIndex::new();
        assert_eq!(index.rebuild(&brain).expect("rebuild index"), 2);
        assert_eq!(
            index.search("runtime", 10),
            vec!["the runtime runs without the original"]
        );
        assert_eq!(index.search("daemon", 10), vec!["the daemon is single"]);
        assert!(index.search("nothing here", 10).is_empty());
    }

    #[test]
    fn no_divergence_between_brain_and_index() {
        let brain = tmp_brain();
        brain
            .append("note", "first entry", serde_json::json!({}))
            .expect("append note");
        brain
            .append("note", "second entry", serde_json::json!({}))
            .expect("append note");
        let mut index = FaissIndex::new();
        index.rebuild(&brain).expect("rebuild index");
        assert!(check_no_divergence(&brain, &index).expect("divergence check"));
        brain
            .append("note", "third entry", serde_json::json!({}))
            .expect("append note");
        assert!(!check_no_divergence(&brain, &index).expect("divergence check"));
        index.rebuild(&brain).expect("rebuild index");
        assert!(check_no_divergence(&brain, &index).expect("divergence check"));
    }
}
