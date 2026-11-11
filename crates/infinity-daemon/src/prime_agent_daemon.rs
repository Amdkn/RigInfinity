//! `pa-daemon` as a reference — deposited, never launched (D-N052 R2).
//!
//! A1: prime-agent has a daemon (`crates/pa-daemon`, README + src +
//! tests + testdata). The Runtime has a single daemon (RigInfinity —
//! F8 D8). Two daemons do not coexist in the Runtime. A daemon is not
//! a surface — it is not "re-pointed", it is launched or not launched.
//! `pa-daemon` is deposited as reference code (here, as the
//! documentation of its ACP wire interface) and never launched. The
//! single daemon (RigInfinity) is the only daemon that runs (H-F8-1).
//!
//! The reference (from `crates/pa-daemon/README.md`, at the gel):
//! ACP stdio transport — `initialize`, `session/new`,
//! `session/prompt`, `session/close`, `session/cancel`, and the
//! outgoing `session/update` notification with namespaced `_meta`
//! correlation; supervisor process (one worker per active session,
//! restart/backoff, session registry/roster); append-only session
//! store (JSONL); client attach/detach; direct-attach transport
//! (single-use tickets, 10 s TTL); worker turn loop (compaction,
//! auto-refine, goal continuation).
//!
//! This module contains no process-spawning call. The only "launch"
//! path is `DaemonRef::launch_attempt`, which returns the refusal by
//! construction. `SingleDaemon` is the guard the daemon consults: it
//! names the one daemon that runs.

/// The ACP methods the daemon serves (the wire interface, as
/// reference — not a client).
pub const ACP_METHODS: &[&str] = &[
    "initialize",
    "session/new",
    "session/prompt",
    "session/close",
    "session/cancel",
    "session/update",
];

/// The daemon as reference code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonRef {
    pub krate: &'static str,
    pub protocol: &'static str,
    pub methods: &'static [&'static str],
}

impl DaemonRef {
    /// The `pa-daemon` reference (A1).
    #[must_use]
    pub fn pa_daemon() -> Self {
        Self {
            krate: "pa-daemon",
            protocol: "ACP (Agent Client Protocol, stdio JSON-RPC)",
            methods: ACP_METHODS,
        }
    }

    /// Attempt to launch the daemon through the adapter: always
    /// refused. The reference is documentation, not a process.
    pub fn launch_attempt(&self) -> Result<(), DaemonError> {
        Err(DaemonError::NeverLaunched { krate: self.krate })
    }
}

/// What the daemon reference refuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    NeverLaunched { krate: &'static str },
}

impl std::fmt::Display for DaemonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NeverLaunched { krate } => write!(
                f,
                "{krate} is deposited as reference code and never launched; the single daemon is RigInfinity"
            ),
        }
    }
}

impl std::error::Error for DaemonError {}

/// The single-daemon guard: exactly one daemon runs in the Runtime,
/// and it is RigInfinity. Any second daemon is refused at the guard,
/// not at launch time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SingleDaemon {
    pub running: &'static str,
}

impl SingleDaemon {
    /// The guard as it stands after R2.
    #[must_use]
    pub fn riginfinity() -> Self {
        Self {
            running: "RigInfinity",
        }
    }

    /// Admit a daemon to run: only RigInfinity is admitted.
    pub fn admit(&self, name: &str) -> Result<(), DaemonError> {
        if name == self.running {
            Ok(())
        } else {
            Err(DaemonError::NeverLaunched { krate: "pa-daemon" })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_daemon_reference_cannot_be_launched() {
        let daemon = DaemonRef::pa_daemon();
        assert_eq!(
            daemon.launch_attempt(),
            Err(DaemonError::NeverLaunched { krate: "pa-daemon" })
        );
        assert_eq!(daemon.methods.len(), 6);
        assert!(daemon.methods.contains(&"session/prompt"));
    }

    #[test]
    fn the_guard_admits_only_riginfinity() {
        let guard = SingleDaemon::riginfinity();
        assert!(guard.admit("RigInfinity").is_ok());
        assert!(guard.admit("pa-daemon").is_err());
    }
}
