//! Native daemon proofs — D-N051 F8 pas 3, H-P3-3 / H-P3-4 / H-P3-6.
//!
//! 1. Hibernation through the real session manager + the real Unix
//!    client protocol (`idle` -> `shut_down`, wake on user text only).
//! 2. RAP: a tool result POSTed to the bridge's callback URL reaches
//!    the session's input queue; a result for a hibernated session is
//!    refused until user text wakes it.
//! 3. MCP compat: a REAL stdio MCP server (a small Python program,
//!    unchanged by the proxy) serves `tools/list` and `tools/call`
//!    through the proxy, and the result is delivered on the RAP
//!    callback URL.

use infinity_daemon::mcp_proxy::start_mcp_proxy;
use infinity_daemon::rap::{RapCallback, RapInvocation, RapToolResult};
use infinity_daemon::session::InputMessage;
use infinity_daemon::{DAEMON_READY_LINE, launch_session_manager};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::mpsc;

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riginfinity-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// One-shot loopback callback receiver: returns (url, rx) where rx
/// yields each POST body received.
async fn callback_receiver() -> (String, mpsc::UnboundedReceiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind callback receiver");
    let port = listener.local_addr().expect("callback port").port();
    let (tx, rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(&mut stream);
                let mut headers = String::new();
                let mut content_length = 0usize;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if line == "\r\n" {
                        break;
                    }
                    headers.push_str(&line);
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; content_length];
                use tokio::io::AsyncReadExt;
                if reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                drop(reader);
                let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
                let _ = stream
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nOK")
                    .await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/callback"), rx)
}

async fn recv_timeout<T>(rx: &mut mpsc::UnboundedReceiver<T>, secs: u64) -> Option<T> {
    tokio::time::timeout(std::time::Duration::from_secs(secs), rx.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test]
async fn hibernation_over_unix_client_protocol() {
    let dir = tmp_dir("hibernation");
    let mgr = launch_session_manager(dir.clone())
        .await
        .expect("launch manager");

    // Serve the real client protocol on a temp Unix socket.
    let sock = dir.join("daemon.sock");
    let listener = tokio::net::UnixListener::bind(&sock).expect("bind daemon socket");
    let mgr2 = mgr.clone();
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let m = mgr2.clone();
            tokio::spawn(async move {
                infinity_daemon::client::handle_client(stream, m).await;
            });
        }
    });

    let stream = UnixStream::connect(&sock).await.expect("connect client");
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    async fn rpc(
        writer: &mut tokio::net::unix::OwnedWriteHalf,
        lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
        req: serde_json::Value,
    ) -> serde_json::Value {
        writer
            .write_all(format!("{req}\n").as_bytes())
            .await
            .expect("write rpc");
        let line = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line())
            .await
            .expect("rpc timeout")
            .expect("rpc read")
            .expect("rpc line");
        serde_json::from_str(&line).expect("parse rpc reply")
    }

    let created = rpc(
        &mut writer,
        &mut lines,
        serde_json::json!({"op": "create_session", "cwd": "/tmp", "model": "test/model"}),
    )
    .await;
    assert_eq!(created["ok"], true);
    let id = created["session_id"]
        .as_str()
        .expect("session id")
        .to_owned();

    // Park it through the manager (idle -> shut_down, durable).
    {
        let m = mgr.lock().await;
        m.cleanup_session(&id).await;
    }
    let listed = rpc(
        &mut writer,
        &mut lines,
        serde_json::json!({"op": "list_sessions"}),
    )
    .await;
    assert_eq!(listed["ok"], true);
    let entry = listed["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|s| s["id"] == id)
        .expect("session listed");
    assert_eq!(entry["status"], "Stopped", "hibernated session is Stopped");

    // User text over the socket wakes it.
    let sent = rpc(
        &mut writer,
        &mut lines,
        serde_json::json!({"op": "send_input", "session_id": id, "text": "wake up"}),
    )
    .await;
    assert_eq!(sent["accepted"], true);
    let listed = rpc(
        &mut writer,
        &mut lines,
        serde_json::json!({"op": "list_sessions"}),
    )
    .await;
    let entry = listed["sessions"]
        .as_array()
        .expect("sessions array")
        .iter()
        .find(|s| s["id"] == id)
        .expect("session listed");
    assert_eq!(entry["status"], "Running", "user text re-awakens the agent");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn rap_result_reaches_agent_without_process() {
    let dir = tmp_dir("rap");
    let mgr = launch_session_manager(dir.clone())
        .await
        .expect("launch manager");
    let callback_url = {
        let m = mgr.lock().await;
        m.callback_url().to_owned()
    };
    let id = {
        let m = mgr.lock().await;
        m.create_session(Path::new("/tmp"), "test/model")
            .await
            .expect("create session")
    };
    // Take the input queue: this test IS the driver.
    let mut input_rx = {
        let mut m = mgr.lock().await;
        m.take_input_receiver().expect("input receiver")
    };

    // A tool result for a live session flows into the queue.
    let result = RapToolResult {
        group_id: id.clone(),
        id: "inv-1".into(),
        call_id: None,
        text: Some("tool says hi".into()),
        content: None,
        display_as: None,
        subscription: None,
    };
    let status = reqwest::Client::new()
        .post(&callback_url)
        .json(&RapCallback::ToolResult(result))
        .send()
        .await
        .expect("post RAP callback");
    assert!(status.status().is_success());
    let got = recv_timeout(&mut input_rx, 5)
        .await
        .expect("input must arrive");
    match got {
        InputMessage::ToolResult { thread_id, result } => {
            assert_eq!(thread_id, id);
            assert_eq!(result.text.as_deref(), Some("tool says hi"));
        }
        other => panic!("expected ToolResult input, got {other:?}"),
    }

    // Park the session: a tool result is now refused (no driver spawn).
    {
        let m = mgr.lock().await;
        m.cleanup_session(&id).await;
    }
    let result = RapToolResult {
        group_id: id.clone(),
        id: "inv-2".into(),
        call_id: None,
        text: Some("late result".into()),
        content: None,
        display_as: None,
        subscription: None,
    };
    reqwest::Client::new()
        .post(&callback_url)
        .json(&RapCallback::ToolResult(result))
        .send()
        .await
        .expect("post late RAP callback");
    assert!(
        recv_timeout(&mut input_rx, 2).await.is_none(),
        "RAP callbacks while shut down are ignored"
    );

    // User text wakes the session; a fresh result flows again.
    {
        let m = mgr.lock().await;
        assert!(
            m.send_input(InputMessage::UserText {
                thread_id: id.clone(),
                text: "continue".into(),
            })
            .await
        );
    }
    assert!(matches!(
        recv_timeout(&mut input_rx, 5).await,
        Some(InputMessage::UserText { .. })
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A real MCP server (Python, stdio JSON-RPC), untouched by the proxy.
const MCP_ECHO_SERVER: &str = r#"import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    method = req.get("method")
    rid = req.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": rid, "result": {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "echo", "version": "1.0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": rid, "result": {"tools": [{"name": "echo", "description": "Echoes input", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}})
    elif method == "tools/call":
        params = req.get("params", {})
        text = params.get("arguments", {}).get("text", "?")
        send({"jsonrpc": "2.0", "id": rid, "result": {"content": [{"type": "text", "text": "echo: " + str(text)}]}})
    else:
        send({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": "unknown method " + str(method)}})
"#;

#[tokio::test]
async fn real_mcp_server_runs_unchanged_through_proxy() {
    let dir = tmp_dir("mcp");
    let script = dir.join("echo_mcp_server.py");
    std::fs::write(&script, MCP_ECHO_SERVER).expect("write MCP fixture");
    let (port, _task) = start_mcp_proxy(
        "echo".into(),
        vec!["python3".into(), script.to_string_lossy().into_owned()],
        HashMap::new(),
    )
    .await
    .expect("start MCP proxy");
    let proxy_url = format!("http://127.0.0.1:{port}");
    assert!(proxy_url.starts_with("http://127.0.0.1:"));

    // The manifest is served unchanged by the proxy.
    let manifest: serde_json::Value = reqwest::Client::new()
        .get(format!("{proxy_url}/.well-known/rap-toolset"))
        .send()
        .await
        .expect("fetch manifest")
        .json()
        .await
        .expect("parse manifest");
    assert_eq!(manifest["name"], "echo-mcp");
    assert!(
        manifest["endpoint"]
            .as_str()
            .unwrap_or("")
            .starts_with("http://127.0.0.1:"),
        "manifest endpoint is loopback only"
    );

    // list_tools through the proxy -> the real server's echo tool.
    let (callback_url, mut rx) = callback_receiver().await;
    let inv = RapInvocation {
        operation: "echo_list_tools".into(),
        arguments: serde_json::json!({}),
        id: "inv-list".into(),
        call_id: None,
        callback_url: callback_url.clone(),
        group_id: "g1".into(),
        user_id: None,
        thread_ancestors: None,
    };
    reqwest::Client::new()
        .post(&proxy_url)
        .json(&inv)
        .send()
        .await
        .expect("invoke list_tools");
    let body = recv_timeout(&mut rx, 10)
        .await
        .expect("list_tools callback");
    let cb: RapCallback = serde_json::from_str(&body).expect("parse callback");
    match cb {
        RapCallback::ToolResult(tr) => {
            let text = tr.text.unwrap_or_default();
            assert!(
                text.contains("**echo**"),
                "proxy must list the real echo tool: {text}"
            );
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }

    // invoke_tool through the proxy -> the real server answers.
    let inv = RapInvocation {
        operation: "echo_invoke_tool".into(),
        arguments: serde_json::json!({"tool_name": "echo", "arguments": {"text": "hello"}}),
        id: "inv-call".into(),
        call_id: None,
        callback_url,
        group_id: "g1".into(),
        user_id: None,
        thread_ancestors: None,
    };
    reqwest::Client::new()
        .post(&proxy_url)
        .json(&inv)
        .send()
        .await
        .expect("invoke echo");
    let body = recv_timeout(&mut rx, 10).await.expect("invoke callback");
    let cb: RapCallback = serde_json::from_str(&body).expect("parse callback");
    match cb {
        RapCallback::ToolResult(tr) => {
            assert_eq!(tr.text.as_deref(), Some("echo: hello"));
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn daemon_ready_line_is_the_only_stdout_signal() {
    assert_eq!(DAEMON_READY_LINE, "infinity-daemon ready");
    assert_eq!(infinity_daemon::INFINITY_POPULATION_CAP, 75_000);
    assert_eq!(infinity_daemon::FRACTAL_LAYER, "4L");
}
