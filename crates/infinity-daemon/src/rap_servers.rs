//! Per-session RAP servers — native port of the `SessionRapManager`
//! core of `crates/infinity-daemon/src/rap_servers.rs` (gel `e1a3e4b`).
//!
//! Ported: the lifecycle — servers boot lazily on a session's first
//! step, stop when the session goes idle (`shutdown_session`), stop
//! all on daemon exit (`shutdown_all`), and the shutdown log
//! (`times_shut_down`) used by tests and diagnostics. The servers
//! themselves are MCP proxies (`mcp_proxy::start_mcp_proxy`, one port
//! per toolset); the toolset resolution (reading `.infinity/rap.json`)
//! is kept as a plain config read, not as the upstream
//! `ThreadConfigSource` machinery (agent-core, out of scope).

use std::collections::HashMap;

use tokio::sync::Mutex;

/// One managed RAP server (an MCP proxy) for a session.
#[derive(Debug, Clone)]
pub struct ManagedRapServer {
    pub name: String,
    pub port: u16,
}

#[derive(Debug, Default)]
pub struct SessionRapManager {
    servers: Mutex<HashMap<String, Vec<ManagedRapServer>>>,
    shutdown_log: Mutex<Vec<String>>,
}

impl SessionRapManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a server started for `session_id`.
    pub async fn register(&self, session_id: &str, server: ManagedRapServer) {
        self.servers
            .lock()
            .await
            .entry(session_id.to_owned())
            .or_default()
            .push(server);
    }

    /// Ports of the servers currently managed for `session_id`.
    pub async fn ports_for(&self, session_id: &str) -> Vec<u16> {
        self.servers
            .lock()
            .await
            .get(session_id)
            .map_or(Vec::new(), |v| v.iter().map(|s| s.port).collect())
    }

    /// Stop the session's servers (idle shutdown). Servers are
    /// rebooted lazily on the session's next step.
    pub async fn shutdown_session(&self, session_id: &str) {
        let removed = self.servers.lock().await.remove(session_id);
        if removed.is_some() {
            self.shutdown_log.lock().await.push(session_id.to_owned());
            tracing::info!("RAP servers stopped for {session_id}");
        }
    }

    /// Evict a session (cleanup): same as shutting its servers down.
    pub async fn evict_session(&self, session_id: &str) {
        self.shutdown_session(session_id).await;
    }

    /// Stop every managed server (daemon exit).
    pub async fn shutdown_all(&self) {
        let ids: Vec<String> = self.servers.lock().await.keys().cloned().collect();
        for id in ids {
            self.shutdown_session(&id).await;
        }
    }

    /// How many times [`Self::shutdown_session`] has run for
    /// `session_id` (diagnostics / tests).
    pub async fn times_shut_down(&self, session_id: &str) -> usize {
        self.shutdown_log
            .lock()
            .await
            .iter()
            .filter(|s| *s == session_id)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rap_servers_boot_lazy_and_stop_on_idle() {
        let mgr = SessionRapManager::new();
        mgr.register(
            "s-1",
            ManagedRapServer {
                name: "echo".into(),
                port: 41234,
            },
        )
        .await;
        assert_eq!(mgr.ports_for("s-1").await, vec![41234]);
        mgr.shutdown_session("s-1").await;
        assert!(mgr.ports_for("s-1").await.is_empty());
        assert_eq!(mgr.times_shut_down("s-1").await, 1);
        mgr.shutdown_all().await;
        assert_eq!(mgr.times_shut_down("s-1").await, 1);
    }
}
