//! OpenRig terminal transport — the native Herdr client (D-N051 F8, pas 4).
//!
//! Herdr (`hc`) is an AGPL terminal multiplexer. OpenRig integrates it at
//! ARM'S LENGTH (upstream `packages/daemon/src/domain/terminal/
//! herdr-transport.ts`, header OPR.0.4.6.02 FB4): the daemon talks to
//! herdr's local unix-domain CONTROL SOCKET — a runtime IPC endpoint the
//! operator's installed herdr exposes — exactly the way it shells out to
//! tmux/cmux. It never imports, links, embeds, or vendors herdr code.
//!
//! This module is the same boundary, in Rust: it frames JSON on the socket
//! the operator's herdr owns and never spawns the `hc` CLI (there is no
//! `std::process::Command` in this file — layout, panes and workspaces go
//! through the socket JSON-RPC only, H-P4-6).
//!
//! Wire protocol (grounded in the upstream verbatim captures at
//! `research/herdr-socket-captures/herdr-phase3-*.json`):
//! - socket: `~/.config/herdr/herdr.sock` (`HERDR_SOCKET_PATH` /
//!   `HERDR_SESSION` overrides)
//! - framing: newline-delimited JSON (one object per line)
//! - envelope: request `{id,method,params}` -> success `{id,result:{type,…}}`
//!   (NOT JSON-RPC 2.0 — there is no `jsonrpc` field; some methods, e.g.
//!   `ping`, may answer with a bare `{type,…}`)

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::openrig_lineage::generate_ulid;
use crate::BoxError;

/// Default Herdr control-socket path (upstream default).
pub const HERDR_DEFAULT_SOCKET: &str = "~/.config/herdr/herdr.sock";

/// Socket round-trip timeout (the upstream CLI transport uses 5 s).
pub const HERDR_SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// The Herdr socket methods this client calls (pinned from the upstream
/// `herdr-adapter.ts`). Any other method is a design event, not a silent
/// addition.
pub const HERDR_SOCKET_METHODS: &[&str] = &[
    "ping",
    "pane.list",
    "pane.report_agent_session",
    "workspace.list",
    "workspace.create",
    "layout.apply",
    "tab.focus",
    "tab.close",
];

/// Liveness + version/protocol via the socket `ping`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrProbeResult {
    pub alive: bool,
    pub version: Option<String>,
    pub protocol: Option<u64>,
}

/// One pane as `pane.list` reports it (subset the registry judges).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HerdrPane {
    pub id: String,
    pub workspace_id: Option<String>,
    pub session: Option<String>,
    pub seat_id: Option<String>,
}

/// Resolve the Herdr control-socket path, upstream semantics
/// (`resolveHerdrSocketPath(env, session?)`): `HERDR_SOCKET_PATH` wins;
/// otherwise the default under the config home. A named session
/// (`HERDR_SESSION`) selects a per-session socket when given.
pub fn resolve_herdr_socket_path() -> PathBuf {
    if let Some(path) = std::env::var("HERDR_SOCKET_PATH").ok().filter(|v| !v.is_empty()) {
        return PathBuf::from(path);
    }
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(|home| PathBuf::from(home).join(".config"))
                .unwrap_or_else(|_| PathBuf::from(".config"))
        });
    let session = std::env::var("HERDR_SESSION").ok().filter(|v| !v.is_empty());
    match session {
        Some(name) => config_home.join("herdr").join(format!("{name}.sock")),
        None => config_home.join("herdr").join("herdr.sock"),
    }
}

/// The Herdr socket client: one connection, NDJSON request/response.
/// A client is single-threaded by construction (one connection, sequential
/// round-trips); the daemon holds one client per socket endpoint.
pub struct HerdrClient {
    reader: BufReader<UnixStream>,
    socket_path: PathBuf,
}

impl HerdrClient {
    /// Connect to a Herdr control socket (default: `resolve_herdr_socket_path()`).
    pub fn connect(socket_path: &Path) -> Result<Self, BoxError> {
        let stream = UnixStream::connect(socket_path)
            .map_err(|e| format!("cannot connect to herdr socket {}: {e}", socket_path.display()))?;
        stream.set_read_timeout(Some(HERDR_SOCKET_TIMEOUT))?;
        stream.set_write_timeout(Some(HERDR_SOCKET_TIMEOUT))?;
        Ok(Self { reader: BufReader::new(stream), socket_path: socket_path.to_path_buf() })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// One socket round-trip: send `{id,method,params}`, resolve the
    /// matching response body. A response carrying `error` is an error; a
    /// response with no `result` and no `type` is an error; a bare
    /// `{type,…}` answer (ping) is the result itself.
    pub fn request(&mut self, method: &str, params: Value) -> Result<Value, BoxError> {
        if !HERDR_SOCKET_METHODS.contains(&method) {
            return Err(format!("herdr method {method:?} is not in the pinned set").into());
        }
        let id = generate_ulid();
        let line = serde_json::to_string(&json!({ "id": id, "method": method, "params": params }))?;
        self.reader.get_mut().write_all(line.as_bytes())?;
        self.reader.get_mut().write_all(b"\n")?;
        self.reader.get_mut().flush()?;
        let mut response_line = String::new();
        let read = self.reader.read_line(&mut response_line)?;
        if read == 0 {
            return Err("herdr socket closed without an answer".into());
        }
        let response: Value = serde_json::from_str(&response_line)
            .map_err(|e| format!("herdr answered unparsable JSON: {e}"))?;
        if let Some(error) = response.get("error") {
            return Err(format!("herdr error for {method}: {error}").into());
        }
        if let Some(result) = response.get("result") {
            return Ok(result.clone());
        }
        if response.get("type").is_some() {
            return Ok(response);
        }
        Err(format!("herdr answered without a result for {method}: {response_line}").into())
    }

    /// Liveness + version/protocol via the socket `ping`.
    pub fn ping(&mut self) -> Result<HerdrProbeResult, BoxError> {
        let result = self.request("ping", json!({}))?;
        Ok(HerdrProbeResult {
            alive: true,
            version: result.get("version").and_then(|v| v.as_str()).map(str::to_string),
            protocol: result.get("protocol").and_then(|v| v.as_u64()),
        })
    }

    /// List the workspaces herdr knows (`workspace.list`).
    pub fn list_workspaces(&mut self) -> Result<Vec<Value>, BoxError> {
        let result = self.request("workspace.list", json!({}))?;
        let workspaces = result
            .get("workspaces")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(workspaces)
    }

    /// Create a workspace (`workspace.create`, unfocused).
    pub fn create_workspace(&mut self, label: &str) -> Result<Value, BoxError> {
        self.request("workspace.create", json!({ "focus": false, "label": label }))
    }

    /// List the panes of a workspace (`pane.list`).
    pub fn list_panes(&mut self, workspace_id: &str) -> Result<Vec<HerdrPane>, BoxError> {
        let result = self.request("pane.list", json!({ "workspace_id": workspace_id }))?;
        let mut panes = Vec::new();
        if let Some(list) = result.get("panes").and_then(|v| v.as_array()) {
            for pane in list {
                let id = pane.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                if id.is_empty() {
                    continue;
                }
                panes.push(HerdrPane {
                    id,
                    workspace_id: pane.get("workspace_id").and_then(|v| v.as_str()).map(str::to_string),
                    session: pane.get("session").and_then(|v| v.as_str()).map(str::to_string),
                    seat_id: pane.get("seat_id").and_then(|v| v.as_str()).map(str::to_string),
                });
            }
        }
        Ok(panes)
    }

    /// Apply a layout to a workspace (`layout.apply`).
    pub fn apply_layout(&mut self, workspace_id: &str, layout: Value) -> Result<Value, BoxError> {
        self.request("layout.apply", json!({ "workspace_id": workspace_id, "layout": layout }))
    }

    /// Focus a tab (`tab.focus`).
    pub fn focus_tab(&mut self, tab_id: &str) -> Result<Value, BoxError> {
        self.request("tab.focus", json!({ "tab_id": tab_id }))
    }

    /// Close a tab (`tab.close`).
    pub fn close_tab(&mut self, tab_id: &str) -> Result<Value, BoxError> {
        self.request("tab.close", json!({ "tab_id": tab_id }))
    }

    /// Report an agent session on a pane (`pane.report_agent_session`).
    pub fn report_agent_session(&mut self, pane_id: &str, session: Value) -> Result<Value, BoxError> {
        self.request(
            "pane.report_agent_session",
            json!({ "pane_id": pane_id, "session": session }),
        )
    }
}

/// The seat ↔ Herdr workspace registry: which rig seat lives in which
/// Herdr workspace. The daemon registers a seat when it launches it and
/// unregisters it when the seat is torn down; anything Herdr reports that
/// is not registered is an orphan (H-P4-3).
#[derive(Debug, Default)]
pub struct HerdrSessionRegistry {
    seats: std::collections::HashMap<String, String>,
}

impl HerdrSessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a seat in a Herdr workspace.
    pub fn register_seat(&mut self, seat_id: &str, workspace_label: &str) {
        self.seats.insert(seat_id.to_string(), workspace_label.to_string());
    }

    /// Unregister a seat (torn down).
    pub fn unregister_seat(&mut self, seat_id: &str) {
        self.seats.remove(seat_id);
    }

    pub fn workspace_of(&self, seat_id: &str) -> Option<&str> {
        self.seats.get(seat_id).map(String::as_str)
    }

    /// H-P4-3 — orphan panes: panes reported by Herdr that are not
    /// attached to any registered seat. A pane is attached when its
    /// `seat_id` (or its `session` name) matches a registered seat.
    pub fn list_orphan_panes(&self, panes: &[HerdrPane]) -> Vec<String> {
        let mut orphans = Vec::new();
        for pane in panes {
            let keys = [pane.seat_id.as_deref(), pane.session.as_deref()];
            let attached = keys
                .iter()
                .flatten()
                .any(|key| self.seats.contains_key(*key));
            if !attached {
                orphans.push(pane.id.clone());
            }
        }
        orphans
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;

    /// A mock herdr: answers `ping` and `workspace.list` with canned
    /// NDJSON, then closes. The listener is bound in the calling thread
    /// (so the socket exists before the client connects); the accept loop
    /// runs on a thread. The client talks to it over a real Unix socket —
    /// no herdr binary needed.
    fn mock_herdr(socket_path: &Path) -> std::thread::JoinHandle<()> {
        let listener = UnixListener::bind(socket_path).expect("bind mock socket");
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut reader = BufReader::new(stream.try_clone().expect("clone stream"));
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let request: Value = serde_json::from_str(&line).unwrap_or(json!({}));
                let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let id = request.get("id").cloned().unwrap_or(json!("0"));
                let answer = match method {
                    "ping" => json!({ "id": id, "result": { "type": "pong", "version": "0.7.1", "protocol": 1 } }),
                    "workspace.list" => {
                        json!({ "id": id, "result": { "type": "workspaces", "workspaces": [{ "id": "ws-1", "label": "build" }] } })
                    }
                    _ => json!({ "id": id, "error": format!("unknown method {method}") }),
                };
                let text = serde_json::to_string(&answer).expect("serialize answer");
                stream.write_all(text.as_bytes()).expect("write answer");
                stream.write_all(b"\n").expect("write newline");
                stream.flush().expect("flush");
            }
        })
    }

    /// A fresh temp socket path per test — the tests run in parallel, so
    /// the path must be unique per test (a shared path races on bind).
    fn test_socket_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("openrig-herdr-test-{}-{name}.sock", std::process::id()))
    }

    #[test]
    fn socket_round_trip_ping_and_list() {
        let path = test_socket_path("round-trip");
        let _ = std::fs::remove_file(&path);
        let handle = mock_herdr(&path);
        let mut client = HerdrClient::connect(&path).expect("connect to mock herdr");
        let probe = client.ping().expect("ping");
        assert!(probe.alive);
        assert_eq!(probe.version.as_deref(), Some("0.7.1"));
        assert_eq!(probe.protocol, Some(1));
        let workspaces = client.list_workspaces().expect("list workspaces");
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].get("label").and_then(|v| v.as_str()), Some("build"));
        drop(client);
        handle.join().expect("mock thread ends");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn unknown_method_is_an_error() {
        let path = test_socket_path("unknown-method");
        let _ = std::fs::remove_file(&path);
        let handle = mock_herdr(&path);
        let mut client = HerdrClient::connect(&path).expect("connect to mock herdr");
        let err = client.request("workspace.create", json!({})).expect_err("mock answers error");
        assert!(err.to_string().contains("unknown method"), "{err}");
        drop(client);
        handle.join().expect("mock thread ends");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn registry_finds_orphans() {
        let mut registry = HerdrSessionRegistry::new();
        registry.register_seat("build", "ws-build");
        registry.register_seat("review", "ws-review");
        let panes = vec![
            HerdrPane { id: "pane-1".to_string(), workspace_id: Some("ws-build".to_string()), session: None, seat_id: Some("build".to_string()) },
            HerdrPane { id: "pane-2".to_string(), workspace_id: Some("ws-review".to_string()), session: Some("review".to_string()), seat_id: None },
            HerdrPane { id: "pane-3".to_string(), workspace_id: None, session: None, seat_id: Some("ghost".to_string()) },
        ];
        assert_eq!(registry.list_orphan_panes(&panes), vec!["pane-3".to_string()]);
        registry.unregister_seat("build");
        assert_eq!(registry.list_orphan_panes(&panes), vec!["pane-1".to_string(), "pane-3".to_string()]);
    }

    #[test]
    fn socket_path_resolution() {
        // SAFETY: env vars are process-global; this test only reads them.
        let path = resolve_herdr_socket_path();
        let text = path.to_string_lossy();
        assert!(text.ends_with("herdr.sock") || text.ends_with(".sock"), "{text}");
    }
}
