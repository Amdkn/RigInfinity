//! OpenRig state store — the native port of the daemon's SQLite state
//! (D-N051 F8, pas 4, D4).
//!
//! Upstream, the daemon (`@openrig/daemon` 0.7.1, gel `bd39da9`) keeps its
//! state in better-sqlite3 at `<OPENRIG_HOME>/openrig.sqlite`
//! (`OPENRIG_DB` / `RIGGED_DB` override, containment-checked —
//! `packages/daemon/src/daemon-db-path.ts`), over 146 migrations and ~50
//! tables (`packages/daemon/src/db/migrations/`).
//!
//! The native port is a faithful re-implementation of the *function* (durable
//! rig / seat / event / usage state), not a line-by-line copy of the SQL:
//! rusqlite with the same path rule, the same core tables (rigs, pods,
//! nodes, edges, bindings, sessions, events, usage samples), the same
//! ISO-8601 UTC timestamps. The remaining upstream tables (queue, inbox /
//! outbox, checkpoints, classifier leases, …) are follow-up work — the
//! schema version table records this port's coverage, and the fiche names
//! the gap instead of hiding it.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use crate::BoxError;

/// This port's schema version. The upstream daemon is at migration 146;
/// this port covers the core tables below (version 1 of the native schema).
pub const OPENRIG_STATE_SCHEMA_VERSION: i64 = 1;

/// Core tables this port implements (upstream table names, verbatim).
pub const CORE_TABLES: &[&str] = &[
    "rigs",
    "pods",
    "nodes",
    "edges",
    "bindings",
    "sessions",
    "events",
    "usage_samples",
];

const SCHEMA_SQL: &str = "
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS rigs (
  id TEXT PRIMARY KEY, name TEXT NOT NULL,
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS pods (
  id TEXT PRIMARY KEY, rig_id TEXT NOT NULL, namespace TEXT NOT NULL,
  label TEXT NOT NULL, summary TEXT, continuity_policy_json TEXT,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS nodes (
  id TEXT PRIMARY KEY, rig_id TEXT NOT NULL, logical_id TEXT NOT NULL,
  role TEXT, runtime TEXT, model TEXT, cwd TEXT, workspace TEXT,
  pod_id TEXT, agent_ref TEXT, profile TEXT, label TEXT,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS edges (
  id TEXT PRIMARY KEY, rig_id TEXT NOT NULL, source_id TEXT NOT NULL,
  target_id TEXT NOT NULL, target_ref TEXT, kind TEXT NOT NULL,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS bindings (
  id TEXT PRIMARY KEY, node_id TEXT NOT NULL,
  attachment_type TEXT, tmux_session TEXT, tmux_window TEXT, tmux_pane TEXT,
  external_session_name TEXT, cmux_workspace TEXT, cmux_surface TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS sessions (
  id TEXT PRIMARY KEY, node_id TEXT NOT NULL, session_name TEXT NOT NULL,
  status TEXT NOT NULL, resume_type TEXT, resume_token TEXT,
  restore_policy TEXT NOT NULL, last_seen_at TEXT,
  created_at TEXT NOT NULL, origin TEXT NOT NULL,
  startup_status TEXT NOT NULL, startup_completed_at TEXT
);
CREATE TABLE IF NOT EXISTS events (
  id INTEGER PRIMARY KEY AUTOINCREMENT, rig_id TEXT NOT NULL,
  kind TEXT NOT NULL, node_id TEXT, payload_json TEXT,
  created_at TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS usage_samples (
  id INTEGER PRIMARY KEY AUTOINCREMENT, seat_session TEXT,
  lane TEXT, captured_at TEXT NOT NULL,
  tokens INTEGER, cost_cents INTEGER
);
CREATE INDEX IF NOT EXISTS idx_nodes_rig ON nodes(rig_id);
CREATE INDEX IF NOT EXISTS idx_edges_rig ON edges(rig_id);
CREATE INDEX IF NOT EXISTS idx_bindings_node ON bindings(node_id);
CREATE INDEX IF NOT EXISTS idx_sessions_node ON sessions(node_id);
CREATE INDEX IF NOT EXISTS idx_events_rig ON events(rig_id);
CREATE INDEX IF NOT EXISTS idx_usage_seat ON usage_samples(seat_session, captured_at);
";

/// A rig (upstream `Rig`: id, name, createdAt, updatedAt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rig {
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub updated_at: String,
}

/// A pod (upstream `Pod`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pod {
    pub id: String,
    pub rig_id: String,
    pub namespace: String,
    pub label: String,
    pub summary: Option<String>,
    pub continuity_policy_json: Option<String>,
    pub created_at: String,
}

/// A node / seat (upstream `Node` — identity + placement columns; the
/// handover / resume-ledger columns are follow-up, see the fiche).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigNode {
    pub id: String,
    pub rig_id: String,
    pub logical_id: String,
    pub role: Option<String>,
    pub runtime: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub workspace: Option<String>,
    pub pod_id: Option<String>,
    pub agent_ref: Option<String>,
    pub profile: Option<String>,
    pub label: Option<String>,
    pub created_at: String,
}

/// An edge (upstream `Edge`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigEdge {
    pub id: String,
    pub rig_id: String,
    pub source_id: String,
    pub target_id: String,
    pub target_ref: Option<String>,
    pub kind: String,
    pub created_at: String,
}

/// A binding (upstream `Binding`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigBinding {
    pub id: String,
    pub node_id: String,
    pub attachment_type: Option<String>,
    pub tmux_session: Option<String>,
    pub tmux_window: Option<String>,
    pub tmux_pane: Option<String>,
    pub external_session_name: Option<String>,
    pub cmux_workspace: Option<String>,
    pub cmux_surface: Option<String>,
    pub updated_at: String,
}

/// A session (upstream `Session`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RigSession {
    pub id: String,
    pub node_id: String,
    pub session_name: String,
    pub status: String,
    pub resume_type: Option<String>,
    pub resume_token: Option<String>,
    pub restore_policy: String,
    pub last_seen_at: Option<String>,
    pub created_at: String,
    pub origin: String,
    pub startup_status: String,
    pub startup_completed_at: Option<String>,
}

/// One usage sample (backs the local `/api/telemetry/usage/series` read).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageSample {
    pub id: i64,
    pub seat_session: Option<String>,
    pub lane: Option<String>,
    pub captured_at: String,
    pub tokens: Option<i64>,
    pub cost_cents: Option<i64>,
}

/// Current time as ISO 8601 UTC (`2026-11-10T00:00:00Z`), std-only.
pub fn iso_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    iso_from_unix(secs)
}

/// Format a Unix timestamp as ISO 8601 UTC (Howard Hinnant's civil
/// algorithm — no date crate needed).
pub fn iso_from_unix(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let (hour, minute, second) = (
        secs_of_day / 3_600,
        (secs_of_day % 3_600) / 60,
        secs_of_day % 60,
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Resolve the daemon's SQLite path — the upstream rule
/// (`daemon-db-path.ts`): an explicit `OPENRIG_DB` / `RIGGED_DB` wins (the
/// operator takes responsibility for that path); otherwise the db is
/// anchored under `OPENRIG_HOME`, never a bare CWD-relative filename. An
/// implicit path that escapes the resolved home is refused.
pub fn resolve_db_path(openrig_home: &Path) -> Result<PathBuf, BoxError> {
    if let Some(explicit) = std::env::var("OPENRIG_DB")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| std::env::var("RIGGED_DB").ok().filter(|v| !v.is_empty()))
    {
        return Ok(PathBuf::from(explicit));
    }
    let db_path = openrig_home.join("openrig.sqlite");
    let resolved_home =
        std::fs::canonicalize(openrig_home).unwrap_or_else(|_| openrig_home.to_path_buf());
    let resolved_db = std::fs::canonicalize(&db_path).unwrap_or_else(|_| db_path.clone());
    if !resolved_db.starts_with(&resolved_home) {
        return Err(format!(
            "refusing implicit database outside resolved OPENRIG_HOME: {} resolves to {}, outside {}",
            db_path.display(),
            resolved_db.display(),
            resolved_home.display()
        )
        .into());
    }
    Ok(db_path)
}

/// The native OpenRig state store: one SQLite file, WAL-friendly, opened
/// on demand. Mirrors the daemon's `openrig.sqlite` role in the unique
/// daemon — the openrig daemon itself no longer runs.
pub struct OpenRigStateStore {
    conn: Connection,
}

impl OpenRigStateStore {
    /// Open (or create) the store at an explicit path, applying the schema.
    pub fn open(path: &Path) -> Result<Self, BoxError> {
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    /// Open the store at the resolved daemon path for a home.
    pub fn open_for_home(openrig_home: &Path) -> Result<Self, BoxError> {
        let path = resolve_db_path(openrig_home)?;
        Self::open(&path)
    }

    /// In-memory store (tests, throwaway runs).
    pub fn open_in_memory() -> Result<Self, BoxError> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self, BoxError> {
        conn.execute_batch(SCHEMA_SQL)?;
        let version: i64 = conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?;
        if version < OPENRIG_STATE_SCHEMA_VERSION {
            conn.execute(
                "INSERT INTO schema_version (version) VALUES (?1)",
                params![OPENRIG_STATE_SCHEMA_VERSION],
            )?;
        }
        Ok(Self { conn })
    }

    pub fn schema_version(&self) -> Result<i64, BoxError> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        )?)
    }

    // -- rigs / pods / nodes / edges -------------------------------------

    pub fn create_rig(&mut self, id: &str, name: &str) -> Result<Rig, BoxError> {
        let now = iso_now();
        self.conn.execute(
            "INSERT INTO rigs (id, name, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
            params![id, name, now],
        )?;
        Ok(Rig {
            id: id.to_string(),
            name: name.to_string(),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    pub fn get_rig(&self, id: &str) -> Result<Option<Rig>, BoxError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, name, created_at, updated_at FROM rigs WHERE id = ?1")?;
        let mut rows = stmt.query(params![id])?;
        Ok(match rows.next()? {
            Some(row) => Some(Rig {
                id: row.get(0)?,
                name: row.get(1)?,
                created_at: row.get(2)?,
                updated_at: row.get(3)?,
            }),
            None => None,
        })
    }

    pub fn add_pod(&mut self, pod: &Pod) -> Result<(), BoxError> {
        self.conn.execute(
            "INSERT INTO pods (id, rig_id, namespace, label, summary, continuity_policy_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![pod.id, pod.rig_id, pod.namespace, pod.label, pod.summary, pod.continuity_policy_json, pod.created_at],
        )?;
        Ok(())
    }

    pub fn add_node(&mut self, node: &RigNode) -> Result<(), BoxError> {
        self.conn.execute(
            "INSERT INTO nodes (id, rig_id, logical_id, role, runtime, model, cwd, workspace, pod_id, agent_ref, profile, label, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![node.id, node.rig_id, node.logical_id, node.role, node.runtime, node.model, node.cwd, node.workspace, node.pod_id, node.agent_ref, node.profile, node.label, node.created_at],
        )?;
        Ok(())
    }

    pub fn nodes_of_rig(&self, rig_id: &str) -> Result<Vec<RigNode>, BoxError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, rig_id, logical_id, role, runtime, model, cwd, workspace, pod_id, agent_ref, profile, label, created_at FROM nodes WHERE rig_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![rig_id], |row| {
            Ok(RigNode {
                id: row.get(0)?,
                rig_id: row.get(1)?,
                logical_id: row.get(2)?,
                role: row.get(3)?,
                runtime: row.get(4)?,
                model: row.get(5)?,
                cwd: row.get(6)?,
                workspace: row.get(7)?,
                pod_id: row.get(8)?,
                agent_ref: row.get(9)?,
                profile: row.get(10)?,
                label: row.get(11)?,
                created_at: row.get(12)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub fn add_edge(&mut self, edge: &RigEdge) -> Result<(), BoxError> {
        self.conn.execute(
            "INSERT INTO edges (id, rig_id, source_id, target_id, target_ref, kind, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![edge.id, edge.rig_id, edge.source_id, edge.target_id, edge.target_ref, edge.kind, edge.created_at],
        )?;
        Ok(())
    }

    // -- bindings / sessions ----------------------------------------------

    pub fn set_binding(&mut self, binding: &RigBinding) -> Result<(), BoxError> {
        self.conn.execute(
            "INSERT INTO bindings (id, node_id, attachment_type, tmux_session, tmux_window, tmux_pane, external_session_name, cmux_workspace, cmux_surface, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) ON CONFLICT(id) DO UPDATE SET node_id=excluded.node_id, attachment_type=excluded.attachment_type, tmux_session=excluded.tmux_session, tmux_window=excluded.tmux_window, tmux_pane=excluded.tmux_pane, external_session_name=excluded.external_session_name, cmux_workspace=excluded.cmux_workspace, cmux_surface=excluded.cmux_surface, updated_at=excluded.updated_at",
            params![binding.id, binding.node_id, binding.attachment_type, binding.tmux_session, binding.tmux_window, binding.tmux_pane, binding.external_session_name, binding.cmux_workspace, binding.cmux_surface, binding.updated_at],
        )?;
        Ok(())
    }

    pub fn get_binding(&self, node_id: &str) -> Result<Option<RigBinding>, BoxError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, node_id, attachment_type, tmux_session, tmux_window, tmux_pane, external_session_name, cmux_workspace, cmux_surface, updated_at FROM bindings WHERE node_id = ?1",
        )?;
        let mut rows = stmt.query(params![node_id])?;
        Ok(match rows.next()? {
            Some(row) => Some(RigBinding {
                id: row.get(0)?,
                node_id: row.get(1)?,
                attachment_type: row.get(2)?,
                tmux_session: row.get(3)?,
                tmux_window: row.get(4)?,
                tmux_pane: row.get(5)?,
                external_session_name: row.get(6)?,
                cmux_workspace: row.get(7)?,
                cmux_surface: row.get(8)?,
                updated_at: row.get(9)?,
            }),
            None => None,
        })
    }

    pub fn record_session(&mut self, session: &RigSession) -> Result<(), BoxError> {
        self.conn.execute(
            "INSERT INTO sessions (id, node_id, session_name, status, resume_type, resume_token, restore_policy, last_seen_at, created_at, origin, startup_status, startup_completed_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12) ON CONFLICT(id) DO UPDATE SET status=excluded.status, resume_type=excluded.resume_type, resume_token=excluded.resume_token, restore_policy=excluded.restore_policy, last_seen_at=excluded.last_seen_at, startup_status=excluded.startup_status, startup_completed_at=excluded.startup_completed_at",
            params![session.id, session.node_id, session.session_name, session.status, session.resume_type, session.resume_token, session.restore_policy, session.last_seen_at, session.created_at, session.origin, session.startup_status, session.startup_completed_at],
        )?;
        Ok(())
    }

    // -- events / usage -----------------------------------------------------

    pub fn record_event(
        &mut self,
        rig_id: &str,
        kind: &str,
        node_id: Option<&str>,
        payload_json: &str,
    ) -> Result<i64, BoxError> {
        self.conn.execute(
            "INSERT INTO events (rig_id, kind, node_id, payload_json, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![rig_id, kind, node_id, payload_json, iso_now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn record_usage_sample(
        &mut self,
        seat_session: Option<&str>,
        lane: Option<&str>,
        tokens: Option<i64>,
        cost_cents: Option<i64>,
    ) -> Result<i64, BoxError> {
        self.conn.execute(
            "INSERT INTO usage_samples (seat_session, lane, captured_at, tokens, cost_cents) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![seat_session, lane, iso_now(), tokens, cost_cents],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The local telemetry read (`/api/telemetry/usage/series`): usage
    /// samples for a seat (or all seats), in a time window, newest last.
    /// Facts only — no account identity leaves this store.
    pub fn usage_series(
        &self,
        seat_session: Option<&str>,
        since_iso: Option<&str>,
        until_iso: Option<&str>,
        limit: i64,
    ) -> Result<Vec<UsageSample>, BoxError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, seat_session, lane, captured_at, tokens, cost_cents FROM usage_samples
             WHERE (?1 IS NULL OR seat_session = ?1)
               AND (?2 IS NULL OR captured_at >= ?2)
               AND (?3 IS NULL OR captured_at <= ?3)
             ORDER BY captured_at ASC, id ASC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![seat_session, since_iso, until_iso, limit], |row| {
            Ok(UsageSample {
                id: row.get(0)?,
                seat_session: row.get(1)?,
                lane: row.get(2)?,
                captured_at: row.get(3)?,
                tokens: row.get(4)?,
                cost_cents: row.get(5)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// H-P4-3 — orphan sessions: sessions with status `running` whose node
    /// has no binding row. An empty list means zero orphans: every live
    /// session is attached to a seat, and the cockpit shows only rig seats.
    pub fn list_orphan_sessions(&self) -> Result<Vec<RigSession>, BoxError> {
        let mut stmt = self.conn.prepare(
            "SELECT s.id, s.node_id, s.session_name, s.status, s.resume_type, s.resume_token, s.restore_policy, s.last_seen_at, s.created_at, s.origin, s.startup_status, s.startup_completed_at
             FROM sessions s LEFT JOIN bindings b ON b.node_id = s.node_id
             WHERE s.status = 'running' AND b.id IS NULL ORDER BY s.id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(RigSession {
                id: row.get(0)?,
                node_id: row.get(1)?,
                session_name: row.get(2)?,
                status: row.get(3)?,
                resume_type: row.get(4)?,
                resume_token: row.get(5)?,
                restore_policy: row.get(6)?,
                last_seen_at: row.get(7)?,
                created_at: row.get(8)?,
                origin: row.get(9)?,
                startup_status: row.get(10)?,
                startup_completed_at: row.get(11)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_rig(store: &mut OpenRigStateStore) {
        let rig = store
            .create_rig("RIG-A0-CLARA-01ARZ3NDEKTSV4RRFFQ69G5FA", "starter")
            .expect("create rig");
        assert_eq!(rig.name, "starter");
        store
            .add_pod(&Pod {
                id: "dev".to_string(),
                rig_id: rig.id.clone(),
                namespace: "dev".to_string(),
                label: "Project".to_string(),
                summary: None,
                continuity_policy_json: None,
                created_at: rig.created_at.clone(),
            })
            .expect("add pod");
        let now = rig.created_at.clone();
        for (id, runtime) in [("build", "claude-code"), ("review", "codex")] {
            store
                .add_node(&RigNode {
                    id: id.to_string(),
                    rig_id: rig.id.clone(),
                    logical_id: id.to_string(),
                    role: None,
                    runtime: Some(runtime.to_string()),
                    model: None,
                    cwd: Some(".".to_string()),
                    workspace: None,
                    pod_id: Some("dev".to_string()),
                    agent_ref: Some(format!("local:agents/{id}")),
                    profile: Some("default".to_string()),
                    label: None,
                    created_at: now.clone(),
                })
                .expect("add node");
        }
        store
            .add_edge(&RigEdge {
                id: "e1".to_string(),
                rig_id: rig.id.clone(),
                source_id: "build".to_string(),
                target_id: "review".to_string(),
                target_ref: None,
                kind: "delegates_to".to_string(),
                created_at: now.clone(),
            })
            .expect("add edge");
    }

    fn test_session(store: &mut OpenRigStateStore, node: &str) {
        store
            .record_session(&RigSession {
                id: format!("sess-{node}"),
                node_id: node.to_string(),
                session_name: format!("sess-{node}"),
                status: "running".to_string(),
                resume_type: None,
                resume_token: None,
                restore_policy: "resume".to_string(),
                last_seen_at: None,
                created_at: iso_now(),
                origin: "launched".to_string(),
                startup_status: "ready".to_string(),
                startup_completed_at: None,
            })
            .expect("record session");
    }

    #[test]
    fn iso_format_is_utc() {
        assert_eq!(iso_from_unix(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso_from_unix(1_794_348_000), "2026-11-10T22:00:00Z");
        let now = iso_now();
        assert_eq!(now.len(), 20);
        assert!(now.ends_with('Z'));
    }

    #[test]
    fn rig_round_trip_and_nodes() {
        let mut store = OpenRigStateStore::open_in_memory().expect("open memory store");
        assert_eq!(
            store.schema_version().expect("schema version"),
            OPENRIG_STATE_SCHEMA_VERSION
        );
        test_rig(&mut store);
        let rig = store
            .get_rig("RIG-A0-CLARA-01ARZ3NDEKTSV4RRFFQ69G5FA")
            .expect("get rig")
            .expect("rig present");
        assert_eq!(rig.name, "starter");
        let nodes = store.nodes_of_rig(&rig.id).expect("nodes of rig");
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].id, "build");
        assert_eq!(nodes[1].runtime.as_deref(), Some("codex"));
    }

    #[test]
    fn orphan_sessions_cleared_by_binding() {
        let mut store = OpenRigStateStore::open_in_memory().expect("open memory store");
        test_rig(&mut store);
        test_session(&mut store, "build");
        // A running session with no binding is an orphan.
        let orphans = store.list_orphan_sessions().expect("orphans");
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].node_id, "build");
        // Binding the seat clears the orphan.
        store
            .set_binding(&RigBinding {
                id: "b-build".to_string(),
                node_id: "build".to_string(),
                attachment_type: Some("external_cli".to_string()),
                tmux_session: None,
                tmux_window: None,
                tmux_pane: None,
                external_session_name: None,
                cmux_workspace: Some("ws-build".to_string()),
                cmux_surface: Some("surface-build".to_string()),
                updated_at: iso_now(),
            })
            .expect("set binding");
        assert!(store.get_binding("build").expect("get binding").is_some());
        assert!(store.list_orphan_sessions().expect("orphans").is_empty());
    }

    #[test]
    fn usage_series_filters_by_seat_and_window() {
        let mut store = OpenRigStateStore::open_in_memory().expect("open memory store");
        store
            .record_usage_sample(Some("sess-a"), Some("context"), Some(100), Some(2))
            .expect("sample");
        store
            .record_usage_sample(Some("sess-b"), Some("context"), Some(50), Some(1))
            .expect("sample");
        store
            .record_usage_sample(Some("sess-a"), Some("provider_window"), Some(10), None)
            .expect("sample");
        let all = store.usage_series(None, None, None, 10).expect("series");
        assert_eq!(all.len(), 3);
        let a = store
            .usage_series(Some("sess-a"), None, None, 10)
            .expect("series");
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].tokens, Some(100));
        let empty = store
            .usage_series(Some("sess-a"), Some("2999-01-01T00:00:00Z"), None, 10)
            .expect("series");
        assert!(empty.is_empty());
    }

    #[test]
    fn events_are_recorded_with_ids() {
        let mut store = OpenRigStateStore::open_in_memory().expect("open memory store");
        let id = store
            .record_event("RIG-1", "rig.started", None, "{\"rig\":\"RIG-1\"}")
            .expect("record event");
        assert!(id >= 1);
    }

    #[test]
    fn db_path_rule() {
        // SAFETY: tests run single-threaded per module in this crate; the env
        // var is set and cleared inside this test only.
        let home = std::env::temp_dir().join("openrig-state-test-home");
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).expect("create home");
        let path = resolve_db_path(&home).expect("resolve db path");
        assert_eq!(path, home.join("openrig.sqlite"));
        let _ = std::fs::remove_dir_all(&home);
    }
}
