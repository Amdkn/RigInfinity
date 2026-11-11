//! Prime Agent refine — the Continual Harness, native (D-N052 R2, F8 pas 6).
//!
//! Upstream (`PrimeIntellect-ai/prime-agent`, gel `74591cd`):
//! `plan_refinement` / `apply_refinement_plan` / `rollback_proposal`
//! in `crates/pa-core/src/refinement/executor.rs` (:194/:304) and
//! `planner.rs` (:505), the skill `skills/refine/SKILL.md`
//! (`refine.run(instructions, global_=False)` schedules refinement
//! at turn end; `refine.status()` reports pending / in_flight), and
//! `review_auto_refine` (executor.rs:363), the automatic review gate.
//!
//! Native rule: the harness store is the Runtime's JSON file. A plan
//! never mutates: the store is re-read from disk before applying (the
//! planning call can take seconds — the gel's rule). Scope is Local
//! by default; Global only on explicit request. A refused update is
//! annulled by rollback: the harness returns to the previous state,
//! via the recorded before-snapshots. The store file is the single
//! truth; there is no second copy.
//!
//! This module is the native mirror of the adapter's `refine.rs`:
//! same contract, same function names, Rust types, plus the durable
//! JSON store.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::BoxError;

/// A harness entry kind — the gel's `RefinementKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefinementKind {
    Prompt,
    Memory,
    Skill,
    Subagent,
    Factory,
}

/// An edit action — the gel's `RefinementAction`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefinementAction {
    Create,
    Update,
    Delete,
}

/// The scope of a refinement — the gel's `HarnessScope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HarnessScope {
    Local,
    Global,
}

/// One editable continual harness entry — the gel's `HarnessEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessEntry {
    pub id: String,
    pub kind: RefinementKind,
    pub title: String,
    pub content: String,
    pub scope: HarnessScope,
}

/// One planned edit — the gel's `RefinementEdit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinementEdit {
    pub action: RefinementAction,
    pub kind: RefinementKind,
    pub id: Option<String>,
    pub title: String,
    pub content: String,
}

/// A planned refinement — the gel's `RefinementProposal`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinementProposal {
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub edits: Vec<RefinementEdit>,
}

/// One applied (or failed) edit with before/after snapshots — the
/// gel's `AppliedRefinementEdit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppliedRefinementEdit {
    pub action: RefinementAction,
    pub kind: RefinementKind,
    pub id: String,
    pub applied: bool,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// The result of an applied refinement — the gel's `RefinementResult`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefinementResult {
    pub id: String,
    pub summary: String,
    pub rationale: String,
    pub expected_outcome: String,
    pub applied_edits: Vec<AppliedRefinementEdit>,
    pub rollback_of: Option<String>,
    pub scope: Option<HarnessScope>,
}

/// A planned refinement awaiting application — the gel's
/// `RefinementPlan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementPlan {
    pub proposal: RefinementProposal,
    pub id: String,
    pub rollback_of: Option<String>,
    pub rollback_scope: Option<HarnessScope>,
}

/// Options for planning a refinement — the gel's `RefineOptions`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RefineOptions {
    pub global: bool,
    pub instructions: Option<String>,
    pub rollback_id: Option<String>,
}

/// What planning a refinement can refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefineError {
    UnknownRollback(String),
    Proposer(String),
}

impl std::fmt::Display for RefineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownRollback(id) => write!(f, "Refinement {id} not found"),
            Self::Proposer(message) => write!(f, "proposer failed: {message}"),
        }
    }
}

impl std::error::Error for RefineError {}

/// The harness store: entries by id, backed by one JSON file.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct HarnessState {
    #[serde(default)]
    pub entries: HashMap<String, HarnessEntry>,
}

impl HarnessState {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read the store from disk; an absent file is an empty store.
    pub fn load(path: &Path) -> Result<Self, BoxError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Write the store to disk (atomic: write + rename).
    pub fn save(&self, path: &Path) -> Result<(), BoxError> {
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    #[must_use]
    pub fn get(&self, id: &str) -> Option<&HarnessEntry> {
        self.entries.get(id)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

static REFINE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Mint a refinement id in the canonical `refine_<timestamp>` format.
#[must_use]
pub fn generate_refinement_id() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let seq = REFINE_SEQ.fetch_add(1, Ordering::SeqCst);
    let digits = format!("{millis:013}{seq:04}");
    format!("refine_{}", &digits[..digits.len().min(17)])
}

/// The inverse of an applied refinement — the gel's `rollback_proposal`.
#[must_use]
pub fn rollback_proposal(target: &RefinementResult) -> RefinementProposal {
    let mut edits: Vec<RefinementEdit> = Vec::new();
    for edit in target.applied_edits.iter().rev() {
        if !edit.applied {
            continue;
        }
        if let Some(before) = &edit.before {
            edits.push(RefinementEdit {
                action: if edit.after.is_some() {
                    RefinementAction::Update
                } else {
                    RefinementAction::Create
                },
                kind: edit.kind,
                id: Some(edit.id.clone()),
                title: edit.id.clone(),
                content: before.clone(),
            });
        } else {
            edits.push(RefinementEdit {
                action: RefinementAction::Delete,
                kind: edit.kind,
                id: Some(edit.id.clone()),
                title: edit.id.clone(),
                content: String::new(),
            });
        }
    }
    RefinementProposal {
        summary: format!("Rollback of {}", target.id),
        rationale: format!(
            "Annul refinement {}: the harness returns to the previous state.",
            target.id
        ),
        expected_outcome: "The harness state is the state before the rolled-back refinement."
            .to_owned(),
        edits,
    }
}

/// Produce a refinement proposal without mutating any harness state —
/// the gel's `plan_refinement`.
pub fn plan_refinement(
    history: &[RefinementResult],
    options: &RefineOptions,
    propose: &dyn Fn(&str) -> Result<RefinementProposal, String>,
) -> Result<RefinementPlan, RefineError> {
    let id = generate_refinement_id();
    if let Some(rollback_id) = &options.rollback_id {
        let Some(target) = history.iter().find(|item| &item.id == rollback_id) else {
            return Err(RefineError::UnknownRollback(rollback_id.clone()));
        };
        let scope = target.scope.unwrap_or(if options.global {
            HarnessScope::Global
        } else {
            HarnessScope::Local
        });
        return Ok(RefinementPlan {
            proposal: rollback_proposal(target),
            id,
            rollback_of: Some(target.id.clone()),
            rollback_scope: Some(scope),
        });
    }
    let instructions = options.instructions.as_deref().unwrap_or("");
    let proposal = propose(instructions).map_err(RefineError::Proposer)?;
    Ok(RefinementPlan {
        proposal,
        id,
        rollback_of: None,
        rollback_scope: None,
    })
}

/// Apply a plan to the (re-read) harness state — the gel's
/// `apply_refinement_plan`. Edits whose entry changed since the
/// baseline are rejected.
#[must_use]
pub fn apply_refinement_plan(
    state: &mut HarnessState,
    plan: RefinementPlan,
    options: &RefineOptions,
    baseline: Option<&HarnessState>,
) -> RefinementResult {
    let scope = plan.rollback_scope.unwrap_or(if options.global {
        HarnessScope::Global
    } else {
        HarnessScope::Local
    });
    let mut applied_edits: Vec<AppliedRefinementEdit> = Vec::new();
    for edit in &plan.proposal.edits {
        let id = match &edit.id {
            Some(id) => id.clone(),
            None => slug(&edit.title),
        };
        let before = state.get(&id).map(|entry| entry.content.clone());
        let changed_since_baseline = match baseline {
            Some(baseline_state) => match (baseline_state.get(&id), &before) {
                (Some(base), Some(current)) => base.content != *current,
                (Some(_), None) => true,
                (None, Some(_)) => true,
                (None, None) => false,
            },
            None => false,
        };
        if changed_since_baseline {
            applied_edits.push(AppliedRefinementEdit {
                action: edit.action,
                kind: edit.kind,
                id,
                applied: false,
                before,
                after: None,
            });
            continue;
        }
        let after = match edit.action {
            RefinementAction::Create | RefinementAction::Update => {
                state.entries.insert(
                    id.clone(),
                    HarnessEntry {
                        id: id.clone(),
                        kind: edit.kind,
                        title: edit.title.clone(),
                        content: edit.content.clone(),
                        scope,
                    },
                );
                Some(edit.content.clone())
            }
            RefinementAction::Delete => {
                state.entries.remove(&id);
                None
            }
        };
        applied_edits.push(AppliedRefinementEdit {
            action: edit.action,
            kind: edit.kind,
            id,
            applied: true,
            before,
            after,
        });
    }
    RefinementResult {
        id: plan.id,
        summary: plan.proposal.summary,
        rationale: plan.proposal.rationale,
        expected_outcome: plan.proposal.expected_outcome,
        applied_edits,
        rollback_of: plan.rollback_of,
        scope: Some(scope),
    }
}

/// Slug an entry title into an id (the gel's `slug` in planner.rs).
fn slug(raw: &str) -> String {
    let mut normalized = String::new();
    for ch in raw.trim().to_lowercase().chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() {
            normalized.push(ch);
        } else if ch == ' ' || ch == '-' || ch == '_' {
            normalized.push('-');
        }
    }
    let trimmed = normalized.trim_matches('-');
    if trimmed.is_empty() {
        "entry".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// The automatic `/refine` review gate — the gel's `review_auto_refine`
/// as a deterministic gate. The threshold is a Runtime policy, passed
/// by the caller — the module invents no product default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRefineReview {
    pub should_refine: bool,
    pub rationale: String,
    pub instructions: Option<String>,
}

/// Decide whether an automatic refine is due.
#[must_use]
pub fn review_auto_refine(
    reason: &str,
    turns_since_last_review: u32,
    min_turns: u32,
) -> AutoRefineReview {
    if turns_since_last_review >= min_turns && !reason.trim().is_empty() {
        AutoRefineReview {
            should_refine: true,
            rationale: format!(
                "Auto-refine due: {turns_since_last_review} turns since the last review ({reason})."
            ),
            instructions: Some(reason.to_owned()),
        }
    } else {
        AutoRefineReview {
            should_refine: false,
            rationale: format!(
                "No auto-refine: {turns_since_last_review} turns since the last review, threshold {min_turns}."
            ),
            instructions: None,
        }
    }
}

/// The durable harness store: one JSON file under a directory.
#[derive(Debug, Clone)]
pub struct HarnessStore {
    path: PathBuf,
}

impl HarnessStore {
    /// Open the store at `<dir>/harness.json`.
    #[must_use]
    pub fn new(dir: &Path) -> Self {
        Self {
            path: dir.join("harness.json"),
        }
    }

    /// The store's file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load the current state (re-read before every apply).
    pub fn load(&self) -> Result<HarnessState, BoxError> {
        HarnessState::load(&self.path)
    }

    /// Save the state.
    pub fn save(&self, state: &HarnessState) -> Result<(), BoxError> {
        state.save(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposer(instructions: &str) -> Result<RefinementProposal, String> {
        Ok(RefinementProposal {
            summary: format!("Apply: {instructions}"),
            rationale: "scripted proposer".to_owned(),
            expected_outcome: "one memory entry created".to_owned(),
            edits: vec![RefinementEdit {
                action: RefinementAction::Create,
                kind: RefinementKind::Memory,
                id: None,
                title: instructions.to_owned(),
                content: instructions.to_owned(),
            }],
        })
    }

    #[test]
    fn store_round_trips_through_disk() {
        let dir =
            std::env::temp_dir().join(format!("riginfinity-pa-refine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let store = HarnessStore::new(&dir);
        let mut state = store.load().expect("load empty store");
        assert!(state.is_empty());
        let plan = plan_refinement(
            &[],
            &RefineOptions {
                global: false,
                instructions: Some("check git status before committing".to_owned()),
                rollback_id: None,
            },
            &proposer,
        )
        .expect("plan");
        let result = apply_refinement_plan(&mut state, plan, &RefineOptions::default(), None);
        assert_eq!(result.scope, Some(HarnessScope::Local));
        store.save(&state).expect("save");
        let reloaded = store.load().expect("reload");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(
            reloaded
                .get("check-git-status-before-committing")
                .expect("entry")
                .scope,
            HarnessScope::Local
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rollback_restores_the_previous_state() {
        let mut state = HarnessState::new();
        let plan = plan_refinement(
            &[],
            &RefineOptions {
                global: false,
                instructions: Some("a memory".to_owned()),
                rollback_id: None,
            },
            &proposer,
        )
        .expect("plan");
        let applied = apply_refinement_plan(&mut state, plan, &RefineOptions::default(), None);
        assert_eq!(state.len(), 1);
        let history = vec![applied];
        let rollback = plan_refinement(
            &history,
            &RefineOptions {
                global: false,
                instructions: None,
                rollback_id: Some(history[0].id.clone()),
            },
            &proposer,
        )
        .expect("rollback plan");
        let undone = apply_refinement_plan(&mut state, rollback, &RefineOptions::default(), None);
        assert_eq!(state.len(), 0);
        assert!(undone.applied_edits.iter().all(|edit| edit.applied));
    }
}
