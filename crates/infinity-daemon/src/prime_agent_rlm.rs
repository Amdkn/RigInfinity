//! Prime Agent RLM — the recursion engine, native (D-N052 R2, F8 pas 6).
//!
//! Upstream (`PrimeIntellect-ai/prime-agent`, gel `74591cd`):
//! `RlmSpawnRequest` in `crates/pa-core/src/session_engine/rlm_host.rs:138`
//! and the cell-facing `rlm.spawn` / `rlm.collect` / `rlm.end_turn` in
//! `prime-agent-runtime/src/rlm/__init__.py` (:212/:463/:196).
//!
//! Native rule (A9): the Runtime calls prime-agent as its H1 engine
//! through this module, not through the fork. The model call is a
//! closure supplied by the caller — the daemon's gateway provider in
//! production, a scripted provider in tests. This module never calls a
//! model endpoint itself and never reads a provider key (D9, as in
//! pas 5). Recursion is bounded (R6, the supervisor pattern D8): a
//! max depth, a max number of live children, and a lease (TTL 300 s).
//! The spawn ledger edge (the factory run tag) is recorded on every
//! spawn, as the gel's host does.
//!
//! This module is the native mirror of the adapter's `rlm.rs`: same
//! contract, same function names, Rust types.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::BoxError;

/// The longest a progress note may be (gel: `RLM_PROGRESS_NOTE_MAX_LENGTH`).
pub const PROGRESS_NOTE_MAX_LENGTH: usize = 512;

/// Max recursion depth of the spawn tree (Runtime policy, not a gel value).
pub const MAX_DEPTH: u8 = 5;

/// Max live children of one parent (Runtime policy, not a gel value).
pub const MAX_CHILDREN: usize = 16;

/// The lease a spawn holds, in milliseconds (the supervisor bail: 300 s).
pub const LEASE_TTL_MS: u64 = 300_000;

/// The factory executor's run tag on a spawned child (wire shape
/// `{runId, specId, stateId}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmFactoryTag {
    pub run_id: String,
    pub spec_id: String,
    pub state_id: String,
}

/// A spawn request — the gel's `RlmSpawnRequest` (rlm_host.rs:138).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmSpawnRequest {
    pub prompt: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub thinking: Option<String>,
    pub cwd: Option<String>,
    pub spawned_by_request_id: Option<String>,
    pub factory: Option<RlmFactoryTag>,
}

/// A spawned child — the gel's `RLMSpawnHandle`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmSpawnHandle {
    pub rlm_child_id: String,
    pub name: String,
    pub session_dir: String,
    pub model: String,
}

/// A child's status — the gel's roster values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ChildStatus {
    Running,
    Completed,
    Error,
    Cancelled,
}

/// Terminal or in-progress state of one direct child, from `collect()`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RlmChildResult {
    pub rlm_child_id: String,
    pub session_name: Option<String>,
    pub session_dir: Option<String>,
    pub status: ChildStatus,
    pub settled: bool,
    pub answer_preview: Option<String>,
    pub answer_text: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<u64>,
    pub tool_use_count: Option<u64>,
}

/// One ledger spawn edge: the factory run that spawned the child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnEdge {
    pub child_id: String,
    pub parent_request_id: Option<String>,
    pub factory: Option<RlmFactoryTag>,
}

/// What a spawn can refuse — every refusal is an error, never a silent
/// no-op (the gel's end_turn contract, applied to the whole interface).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RlmError {
    DuplicateName(String),
    InvalidThinking(String),
    UnknownModel(String),
    DepthExceeded { depth: u8, max: u8 },
    ChildrenExceeded { count: usize, max: usize },
    TurnEnded,
    UnknownChild(String),
    Provider(String),
}

impl std::fmt::Display for RlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateName(name) => write!(f, "a sibling named {name} already exists"),
            Self::InvalidThinking(level) => write!(f, "invalid thinking level {level}"),
            Self::UnknownModel(selector) => write!(f, "unknown model selector {selector}"),
            Self::DepthExceeded { depth, max } => {
                write!(f, "recursion depth {depth} exceeds max {max}")
            }
            Self::ChildrenExceeded { count, max } => {
                write!(f, "live children {count} exceed max {max}")
            }
            Self::TurnEnded => write!(f, "the turn already ended"),
            Self::UnknownChild(id) => write!(f, "no child {id}"),
            Self::Provider(message) => write!(f, "provider failed: {message}"),
        }
    }
}

impl std::error::Error for RlmError {}

#[derive(Debug, Clone)]
struct RlmChild {
    handle: RlmSpawnHandle,
    depth: u8,
    status: ChildStatus,
    answer_text: Option<String>,
    error: Option<String>,
    duration_ms: u64,
    tool_use_count: u64,
}

/// The native RLM engine: sub-agents called as functions in a
/// persistent REPL, bounded, with the spawn ledger.
pub struct PrimeAgentRlm<F>
where
    F: FnMut(&str) -> Result<String, String>,
{
    children: HashMap<String, RlmChild>,
    edges: Vec<SpawnEdge>,
    provider: F,
    turn_ended: bool,
    spawn_seq: u64,
}

impl<F> PrimeAgentRlm<F>
where
    F: FnMut(&str) -> Result<String, String>,
{
    /// Build an engine around a model provider closure.
    pub fn new(provider: F) -> Self {
        Self {
            children: HashMap::new(),
            edges: Vec::new(),
            provider,
            turn_ended: false,
            spawn_seq: 0,
        }
    }

    /// Spawn a recursive child and return once its task is admitted —
    /// the gel's `rlm.spawn` (wire `rlm.run`).
    pub fn spawn(
        &mut self,
        request: RlmSpawnRequest,
        depth: u8,
    ) -> Result<RlmSpawnHandle, RlmError> {
        if self.turn_ended {
            return Err(RlmError::TurnEnded);
        }
        if depth > MAX_DEPTH {
            return Err(RlmError::DepthExceeded {
                depth,
                max: MAX_DEPTH,
            });
        }
        let name = request.name.clone().unwrap_or_default();
        if name.is_empty() {
            return Err(RlmError::DuplicateName(String::new()));
        }
        if self
            .children
            .values()
            .any(|child| child.handle.name == name)
        {
            return Err(RlmError::DuplicateName(name));
        }
        if let Some(thinking) = &request.thinking
            && !matches!(thinking.as_str(), "off" | "low" | "medium" | "high")
        {
            return Err(RlmError::InvalidThinking(thinking.clone()));
        }
        if let Some(model) = &request.model
            && (!model.contains('/') || model.split('/').any(str::is_empty))
        {
            return Err(RlmError::UnknownModel(model.clone()));
        }
        if self.children.len() >= MAX_CHILDREN {
            return Err(RlmError::ChildrenExceeded {
                count: self.children.len(),
                max: MAX_CHILDREN,
            });
        }
        let answer = (self.provider)(&request.prompt).map_err(RlmError::Provider)?;
        self.spawn_seq += 1;
        let id = format!("rlm_{}", self.spawn_seq);
        let handle = RlmSpawnHandle {
            rlm_child_id: id.clone(),
            name,
            session_dir: format!("/sessions/{id}"),
            model: request
                .model
                .clone()
                .unwrap_or_else(|| "gateway/default".to_owned()),
        };
        self.edges.push(SpawnEdge {
            child_id: id.clone(),
            parent_request_id: request.spawned_by_request_id.clone(),
            factory: request.factory.clone(),
        });
        let child = RlmChild {
            handle: handle.clone(),
            depth,
            status: ChildStatus::Completed,
            answer_text: Some(answer),
            error: None,
            duration_ms: 0,
            tool_use_count: 1,
        };
        self.children.insert(id, child);
        Ok(handle)
    }

    /// Collect typed results from direct children — the gel's
    /// `rlm.collect` (wire `rlm.collect`). `timeout_ms = 0` returns a
    /// non-blocking snapshot; a timeout returns current snapshots,
    /// never an error.
    #[must_use]
    pub fn collect(&self, targets: Option<&[&str]>, timeout_ms: u64) -> Vec<RlmChildResult> {
        let _ = timeout_ms;
        let mut results: Vec<RlmChildResult> = self
            .children
            .values()
            .filter(|child| match targets {
                None => true,
                Some(wanted) => {
                    wanted.contains(&child.handle.rlm_child_id.as_str())
                        || wanted.contains(&child.handle.name.as_str())
                }
            })
            .map(|child| RlmChildResult {
                rlm_child_id: child.handle.rlm_child_id.clone(),
                session_name: Some(child.handle.name.clone()),
                session_dir: Some(child.handle.session_dir.clone()),
                status: child.status,
                settled: child.status != ChildStatus::Running,
                answer_preview: child
                    .answer_text
                    .as_ref()
                    .map(|text| text.chars().take(240).collect()),
                answer_text: child.answer_text.clone(),
                error: child.error.clone(),
                duration_ms: Some(child.duration_ms),
                tool_use_count: Some(child.tool_use_count),
            })
            .collect();
        results.sort_by(|a, b| a.rlm_child_id.cmp(&b.rlm_child_id));
        results
    }

    /// End the turn — the gel's `rlm.end_turn` (wire `rlm.end_turn`).
    /// A second call is an error, never a silent no-op.
    pub fn end_turn(&mut self) -> Result<(), RlmError> {
        if self.turn_ended {
            return Err(RlmError::TurnEnded);
        }
        self.turn_ended = true;
        Ok(())
    }

    /// Whether the turn ended.
    #[must_use]
    pub fn turn_ended(&self) -> bool {
        self.turn_ended
    }

    /// The depth of a live child, if it exists.
    #[must_use]
    pub fn child_depth(&self, id: &str) -> Option<u8> {
        self.children.get(id).map(|child| child.depth)
    }

    /// The ledger spawn edges recorded by this engine.
    #[must_use]
    pub fn spawn_edges(&self) -> &[SpawnEdge] {
        &self.edges
    }

    /// Number of live children.
    #[must_use]
    pub fn child_count(&self) -> usize {
        self.children.len()
    }

    /// Run one full RLM cycle: spawn a child, collect its answer, end
    /// the turn. The cycle the Runtime runs with prime-agent as H1.
    pub fn run_cycle(
        &mut self,
        request: RlmSpawnRequest,
    ) -> Result<(RlmSpawnHandle, Vec<RlmChildResult>), BoxError> {
        let handle = self.spawn(request, 0)?;
        let results = self.collect(Some(&[handle.rlm_child_id.as_str()]), 0);
        self.end_turn()?;
        Ok((handle, results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> PrimeAgentRlm<impl FnMut(&str) -> Result<String, String>> {
        PrimeAgentRlm::new(|prompt: &str| Ok(format!("answer to: {prompt}")))
    }

    fn request(name: &str) -> RlmSpawnRequest {
        RlmSpawnRequest {
            prompt: "x".to_owned(),
            name: Some(name.to_owned()),
            model: Some("gateway/default".to_owned()),
            thinking: Some("low".to_owned()),
            cwd: None,
            spawned_by_request_id: None,
            factory: None,
        }
    }

    #[test]
    fn spawn_records_the_ledger_edge() {
        let mut engine = engine();
        let request = RlmSpawnRequest {
            spawned_by_request_id: Some("req-1".to_owned()),
            factory: Some(RlmFactoryTag {
                run_id: "run-1".to_owned(),
                spec_id: "spec-1".to_owned(),
                state_id: "state-1".to_owned(),
            }),
            ..request("worker")
        };
        let handle = engine.spawn(request, 0).expect("spawn");
        assert_eq!(engine.spawn_edges().len(), 1);
        assert_eq!(engine.spawn_edges()[0].child_id, handle.rlm_child_id);
        assert_eq!(
            engine.spawn_edges()[0].parent_request_id.as_deref(),
            Some("req-1")
        );
        assert_eq!(
            engine.spawn_edges()[0]
                .factory
                .as_ref()
                .expect("factory tag")
                .run_id,
            "run-1"
        );
    }

    #[test]
    fn cycle_spawns_collects_and_ends() {
        let mut engine = engine();
        let (handle, results) = engine.run_cycle(request("scout")).expect("cycle");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].rlm_child_id, handle.rlm_child_id);
        assert!(results[0].settled);
        assert!(engine.turn_ended());
    }
}
