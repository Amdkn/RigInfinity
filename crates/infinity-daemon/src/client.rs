//! Local client protocol — the daemon's Unix-socket front door.
//!
//! Minimal JSON-line protocol (one request per line, one JSON reply
//! per line) over the daemon socket. This is the native equivalent of
//! the upstream `client_handler.rs` command surface, reduced to the
//! session lifecycle the daemon owns:
//!
//! * `{"op":"create_session","cwd":"…","model":"…"}`
//!   -> `{"ok":true,"session_id":"…"}` (emits `Connected` on the broadcast)
//! * `{"op":"resume_session","session_id":"…"}`
//!   -> `{"ok":true}` (emits `Connected`)
//! * `{"op":"list_sessions"}` -> `{"ok":true,"sessions":[{"id":"…","status":"Running"|"Idle"|"Stopped"|"Archived"|"WaitingForChoice"}]}`
//! * `{"op":"send_input","session_id":"…","text":"…"}`
//!   -> `{"ok":true,"accepted":true|false}` (user text; wakes a hibernated session)
//! * `{"op":"cleanup_session","session_id":"…"}`
//!   -> `{"ok":true}` (parks the session: `idle` -> `shut_down`, durable)
//!
//! The WebSocket + web-asset UI of the upstream daemon is NOT ported
//! (UI surface, out of scope for this pas — see the adaptation fiche);
//! local clients use this socket, and remote tools use RAP.

use std::path::Path;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::session::{InputMessage, SharedSessionManager};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Serve one local client connection until it closes.
pub async fn handle_client(stream: UnixStream, manager: SharedSessionManager) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let reply = dispatch(&line, &manager).await;
        let mut out = serde_json::to_string(&reply)
            .unwrap_or_else(|_| r#"{"ok":false,"error":"reply serialize"}"#.into());
        out.push('\n');
        if writer.write_all(out.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn dispatch(line: &str, manager: &SharedSessionManager) -> serde_json::Value {
    let req: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => return serde_json::json!({"ok": false, "error": format!("bad json: {e}")}),
    };
    let op = req
        .get("op")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    match op {
        "create_session" => {
            let cwd = req
                .get("cwd")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("/tmp");
            let model = req
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("default");
            let mgr = manager.lock().await;
            match mgr.create_session(Path::new(cwd), model).await {
                Ok(id) => serde_json::json!({"ok": true, "session_id": id}),
                Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
            }
        }
        "resume_session" => {
            let id = req
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mgr = manager.lock().await;
            match mgr.resume_session(id).await {
                Ok(()) => serde_json::json!({"ok": true}),
                Err(e) => serde_json::json!({"ok": false, "error": e.to_string()}),
            }
        }
        "list_sessions" => {
            let mgr = manager.lock().await;
            let sessions = mgr.list_sessions().await;
            let list: Vec<serde_json::Value> = sessions
                .into_iter()
                .map(|(id, status)| serde_json::json!({"id": id, "status": format!("{status:?}")}))
                .collect();
            serde_json::json!({"ok": true, "sessions": list})
        }
        "send_input" => {
            let id = req
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let text = req
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mgr = manager.lock().await;
            let accepted = mgr
                .send_input(InputMessage::UserText {
                    thread_id: id.to_owned(),
                    text: text.to_owned(),
                })
                .await;
            serde_json::json!({"ok": true, "accepted": accepted})
        }
        "cleanup_session" => {
            let id = req
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let mgr = manager.lock().await;
            mgr.cleanup_session(id).await;
            serde_json::json!({"ok": true})
        }
        other => serde_json::json!({"ok": false, "error": format!("unknown op: {other}")}),
    }
}
