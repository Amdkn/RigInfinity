//! RigInfinity Deterministic Rust Daemon Core (A'Space OS V5)
//! Merges hydro-project/infinity RAP session engine with OpenRig 578-Holon Quality Control.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolonExecutionState {
    Dormant,
    ClaimedExclusive { borrower_holon: String, fencing_token: u64 },
    ExecutingGoldfish { worktree: String },
    VerifyingOutsideJudge,
    HaltedByBeth,
}

#[derive(Debug, Clone)]
pub struct RigInfinitySession {
    pub root_thread_id: String,
    pub area_code: String,
    pub fractal_layer: String,
    pub state: HolonExecutionState,
}
