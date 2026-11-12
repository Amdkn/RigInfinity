//! Prime Agent runtime surface — the Python REPL, re-pointed (D-N052 R2).
//!
//! A2: the runtime Python (`prime-agent-runtime/`, uv,
//! `pyproject.toml`) is the REPL / the kernel Python — the persistent
//! REPL where the agent's code runs (`python_repl`, `ipython`, venv).
//! The fusion composes the Rust (the H1 engine); the Python runtime
//! is the surface of the REPL, re-pointed at the single daemon — as
//! the Python server of agent-zero was re-pointed at pas 5 — and
//! never a second daemon (H-F8-2).
//!
//! This module is the native mirror of the adapter's
//! `runtime_surface.rs`: the same contract, as data, checked —
//! never launched.

/// The Python runtime directory in the fork (the surface).
pub const PYTHON_RUNTIME_DIR: &str = "prime-agent-runtime";

/// The REPL entry module (the cell-facing `rlm` namespace).
pub const REPL_ENTRY: &str = "prime-agent-runtime/src/rlm/__init__.py";

/// The kernel bootstrap module (the host-request lane).
pub const KERNEL_BOOTSTRAP: &str = "prime-agent-runtime/src/rlm/repl.py";

/// Environment variables that decide the surface's behaviour (the
/// gel's own switches — named here so the re-point carries them).
pub const SURFACE_ENV_VARS: &[&str] = &["PI_OFFLINE", "DO_NOT_TRACK", "PRIME_AGENT_TELEMETRY"];

/// What the REPL surface is re-pointed at: the single daemon.
pub const REPOINT_TARGET: &str = "RigInfinity (the single daemon)";

/// The re-pointed REPL surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSurface {
    pub python_runtime_dir: &'static str,
    pub repl_entry: &'static str,
    pub repoint_target: &'static str,
    pub second_daemon: bool,
}

impl RuntimeSurface {
    /// The surface as re-pointed by R2 (A2).
    #[must_use]
    pub fn repointed() -> Self {
        Self {
            python_runtime_dir: PYTHON_RUNTIME_DIR,
            repl_entry: REPL_ENTRY,
            repoint_target: REPOINT_TARGET,
            second_daemon: false,
        }
    }

    /// Check the re-point: the surface points at the single daemon
    /// and no second daemon is configured. Any other shape is an
    /// error, never a silent re-point.
    pub fn check(&self) -> Result<(), SurfaceError> {
        if self.second_daemon {
            return Err(SurfaceError::SecondDaemon);
        }
        if self.repoint_target != REPOINT_TARGET {
            return Err(SurfaceError::WrongTarget(self.repoint_target.to_owned()));
        }
        if self.python_runtime_dir != PYTHON_RUNTIME_DIR || self.repl_entry != REPL_ENTRY {
            return Err(SurfaceError::WrongPaths);
        }
        Ok(())
    }

    /// Whether the surface spawns a second daemon (never).
    #[must_use]
    pub fn spawns_second_daemon(&self) -> bool {
        false
    }
}

/// What a surface check can refuse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SurfaceError {
    SecondDaemon,
    WrongTarget(String),
    WrongPaths,
}

impl std::fmt::Display for SurfaceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SecondDaemon => write!(f, "the surface configures a second daemon"),
            Self::WrongTarget(target) => {
                write!(f, "the surface points at {target}, not the single daemon")
            }
            Self::WrongPaths => write!(f, "the surface paths do not match the fork"),
        }
    }
}

impl std::error::Error for SurfaceError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_repointed_surface_checks() {
        let surface = RuntimeSurface::repointed();
        assert!(surface.check().is_ok());
        assert!(!surface.spawns_second_daemon());
    }

    #[test]
    fn a_second_daemon_is_refused() {
        let surface = RuntimeSurface {
            second_daemon: true,
            ..RuntimeSurface::repointed()
        };
        assert_eq!(surface.check(), Err(SurfaceError::SecondDaemon));
    }
}
