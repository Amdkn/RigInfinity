# Amdkn/RigInfinity — Fusion Souveraine OpenRig × Infinity (Paradigme Go/TS -> Rust)

Ce dépôt unifie en un seul moteur déterministe pour **A'Space OS V5** :
1. **`crates/infinity-daemon/src/` (Origine `hydro-project/infinity`)** : Orchestration déterministe en Rust (`TypedThreadId`, protocole RAP, `mcp_proxy.rs`, `memory_store.rs`, Ownership & Borrowing exclusif des worktrees).
2. **`packages/daemon/` & `demo/` (Origine `mvschwarz/openrig`)** : Standard Quality Control des **578 Holons** (`context-packs-src`, `policies` leak-guard, `specs`, handoff cross-host tracé).
3. **`security-harness/` (`reviewer-security`)** : Triage autonome OWASP Top 10 déclenché par Dependabot, CodeQL et Secret Scanning avec isolation en worktree `sec-<id>` et PR prioritaire `factory:security`.
