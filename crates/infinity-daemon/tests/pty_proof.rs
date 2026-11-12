//! Native PTY proof — D-N051 F8 pas 8 (H-P8-4, §D4).
//!
//! The daemon holds the PTY function without the herdr-core server:
//! it opens a session via the native Herdr transport
//! (`openrig_terminal.rs`, NDJSON over a Unix socket) and executes a
//! command with a receipt. The Herdr side here is a LOCAL STUB that
//! speaks the same NDJSON protocol (`{id, method, params}` → `
//! {id, result}`) — it models the server running the command (an
//! `echo <arg>` command returns `<arg>` as its output) and it does
//! NOT spawn a real shell: the proof is on the daemon side of the
//! protocol (session open + command + receipt round-trip), not on a
//! shell. The real Herdr server of the operator speaks the same
//! protocol on the same socket path shape; the stub answers the
//! pinned methods only (`HERDR_SOCKET_METHODS`).
//!
//! What this proves: after the herdr-core `hc` server is retired,
//! the single daemon still opens a PTY session and gets a receipt
//! for a command through the native transport — no `hc` process is
//! involved anywhere in this test.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread;

use infinity_daemon::openrig_terminal::HerdrClient;
use serde_json::{Value, json};

/// One stub Herdr server: answers the pinned NDJSON methods.
/// `echo <arg>` commands return `<arg>` as `output` — the stub's
/// model of the server running the command in the pane's PTY.
fn serve_stub(listener: UnixListener) {
    let Ok((stream, _)) = listener.accept() else {
        return;
    };
    let mut reader = BufReader::new(stream.try_clone().expect("clone stub stream"));
    let mut writer = stream;
    loop {
        let mut line = String::new();
        let Ok(read) = reader.read_line(&mut line) else {
            break;
        };
        if read == 0 {
            break;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => break,
        };
        let id = request.get("id").cloned().unwrap_or(json!(null));
        let method = request
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_owned();
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let result = match method.as_str() {
            "ping" => json!({"version": "stub-0.1.0", "protocol": 1}),
            "workspace.create" => json!({
                "id": "ws-pas8",
                "label": params.get("label").and_then(|v| v.as_str()).unwrap_or(""),
                "focus": false,
            }),
            "pane.list" => json!({"panes": [{
                "id": "pane-pas8",
                "workspace_id": "ws-pas8",
                "session": "pty-pas8",
                "seat_id": null,
            }]}),
            "pane.report_agent_session" => {
                let session = params.get("session").cloned().unwrap_or(json!({}));
                let command = session
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_owned();
                // Stub model of the server running the command in
                // the pane: `echo <arg>` yields `<arg>` as output.
                let output = command.strip_prefix("echo ").unwrap_or("").to_owned();
                json!({
                    "ok": true,
                    "pane_id": params.get("pane_id").and_then(|v| v.as_str()).unwrap_or(""),
                    "command": command,
                    "output": output,
                    "receipt": "pas8-pty-ok",
                })
            }
            _ => json!({"error": format!("stub: unknown method {method}")}),
        };
        let response = serde_json::to_string(&json!({"id": id, "result": result}))
            .expect("serialize stub response");
        if writer.write_all(response.as_bytes()).is_err()
            || writer.write_all(b"\n").is_err()
            || writer.flush().is_err()
        {
            break;
        }
    }
}

fn stub_socket_path() -> PathBuf {
    std::env::temp_dir().join(format!("riginfinity-pty-stub-{}.sock", std::process::id()))
}

#[test]
fn daemon_opens_pty_session_and_executes_command_with_receipt() {
    let socket_path = stub_socket_path();
    let _ = std::fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind stub herdr socket");
    let handle = thread::spawn(move || serve_stub(listener));

    let mut client = HerdrClient::connect(&socket_path).expect("connect to stub herdr socket");
    assert_eq!(client.socket_path(), socket_path.as_path());

    // 1. Liveness over the native transport.
    let probe = client.ping().expect("ping the stub");
    assert!(probe.alive);
    assert_eq!(probe.version.as_deref(), Some("stub-0.1.0"));

    // 2. Open the PTY session: a workspace, then its pane.
    let workspace = client
        .create_workspace("pas8-pty")
        .expect("create the PTY workspace");
    assert_eq!(
        workspace.get("id").and_then(|v| v.as_str()),
        Some("ws-pas8")
    );
    let panes = client.list_panes("ws-pas8").expect("list the panes");
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].id, "pane-pas8");
    assert_eq!(panes[0].session.as_deref(), Some("pty-pas8"));

    // 3. Execute a command in the pane, with a receipt.
    let receipt = client
        .report_agent_session(
            "pane-pas8",
            json!({"command": "echo pas8-pty-ok", "session": "pty-pas8"}),
        )
        .expect("report the agent session (command + receipt)");
    assert_eq!(receipt.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        receipt.get("command").and_then(|v| v.as_str()),
        Some("echo pas8-pty-ok")
    );
    assert_eq!(
        receipt.get("output").and_then(|v| v.as_str()),
        Some("pas8-pty-ok")
    );
    assert_eq!(
        receipt.get("receipt").and_then(|v| v.as_str()),
        Some("pas8-pty-ok")
    );

    drop(client);
    let _ = std::fs::remove_file(&socket_path);
    handle.join().expect("stub server thread");
}
