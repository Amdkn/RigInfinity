//! Native agent-zero proofs — D-N051 F8 pas 5.
//!
//! 1. The Agent loop runs without the fork: a stub model drives a
//!    tool call, then an explicit `break_loop` response (H-P5-2).
//! 2. The provider fails over against a REAL local loopback HTTP
//!    stub: the stub answers 500 for the first link
//!    (`freellmapi/auto`) and 200 for the second
//!    (`claude-sonnet-4-5`); the switch is recorded (H-P5-4).
//! 3. Memory persists a chat in the Shared Brain, the FAISS index
//!    rebuilds from it, and no divergence is found (H-P5-5).
//! 4. A scheduled task executes and proves itself by receipt; a
//!    task definition carrying `cron` is refused (H-P5-6, D7).

use std::collections::HashMap;
use std::path::PathBuf;

use infinity_daemon::agent::{AgentLoop, ChatMessage, LoopState, ToolTable};
use infinity_daemon::job::{JobLoop, TaskKind, TaskSpec};
use infinity_daemon::memory::{FaissIndex, SharedBrainMemory, check_no_divergence};
use infinity_daemon::provider::{GATEWAY_URL, GatewayProvider, MODELPOOL_CHAIN};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riginfinity-az-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

#[tokio::test]
async fn loop_runs_without_the_fork() {
    let mut agent = AgentLoop::new(0);
    let mut tools: ToolTable = HashMap::new();
    tools.insert(
        "echo".to_owned(),
        Box::new(|args| {
            Ok(args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned())
        }),
    );
    let mut n = 0;
    let result = agent.turn(
        "say hello",
        &mut move |_messages: &[ChatMessage]| {
            n += 1;
            if n == 1 {
                "```json\n{\"tool\": \"echo\", \"args\": {\"text\": \"hello\"}}\n```".to_owned()
            } else {
                "```json\n{\"tool\": \"break_loop\", \"args\": {\"response\": \"done: hello\"}}\n```"
                    .to_owned()
            }
        },
        &tools,
    );
    assert_eq!(result, "done: hello");
    assert_eq!(agent.state, LoopState::Done);
    // No fork module path in the persisted snapshot.
    assert!(!agent.snapshot().to_string().contains("agent.py"));
}

/// A loopback HTTP stub: 500 for the first link, 200 for the rest.
/// Each request gets its own connection (`Connection: close`).
async fn gateway_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway stub");
    let port = listener.local_addr().expect("stub port").port();
    let endpoint = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let handle = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                // Read headers.
                loop {
                    match stream.read(&mut tmp).await {
                        Ok(0) => return,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);
                            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => return,
                    }
                }
                let text = String::from_utf8_lossy(&buf);
                let content_length: usize = text
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse().ok())
                    })
                    .unwrap_or(0);
                let header_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(text.len());
                let mut body = buf[header_end..].to_vec();
                while body.len() < content_length {
                    match stream.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => body.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
                let model = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v["model"].as_str().map(|s| s.to_owned()))
                    .unwrap_or_default();
                let response = if model == "freellmapi/auto" {
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_owned()
                } else {
                    let payload = serde_json::json!({
                        "choices": [{"message": {"content": format!("answer from {model}")}}]
                    });
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        payload.to_string().len(),
                        payload
                    )
                };
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    (endpoint, handle)
}

#[tokio::test]
async fn provider_fails_over_against_local_stub() {
    let (endpoint, _handle) = gateway_stub().await;
    assert!(endpoint.starts_with("http://127.0.0.1:"));
    let mut provider = GatewayProvider::new(&endpoint, MODELPOOL_CHAIN);
    let result = provider
        .chat(&[ChatMessage {
            role: "user".to_owned(),
            content: "hi".to_owned(),
        }])
        .await
        .expect("second link answers");
    assert_eq!(result.model, "claude-sonnet-4-5");
    assert_eq!(result.link, 1);
    assert_eq!(result.content, "answer from claude-sonnet-4-5");
    assert_eq!(provider.switches.len(), 1);
    assert_eq!(provider.switches[0].from, "freellmapi/auto");
    assert_eq!(provider.switches[0].to, "claude-sonnet-4-5");
    // The gateway constant itself is the loopback default (D9).
    assert_eq!(GATEWAY_URL, "http://127.0.0.1:31415/v1/chat/completions");
}

#[test]
fn memory_persist_rebuild_no_divergence() {
    let dir = tmp_dir("memory");
    let brain = SharedBrainMemory::new(dir.join("memory.jsonl"));
    brain
        .persist_chat(
            "chat-1",
            &[
                serde_json::json!({"role": "user", "content": "the runtime runs without the original"}),
                serde_json::json!({"role": "assistant", "content": "noted"}),
            ],
        )
        .expect("persist chat");
    let mut index = FaissIndex::new();
    assert_eq!(index.rebuild(&brain).expect("rebuild index"), 1);
    assert_eq!(
        index.search("runtime", 10),
        vec![brain.read_all().expect("read brain")[0].content.clone()]
    );
    assert!(check_no_divergence(&brain, &index).expect("divergence check"));
}

#[test]
fn scheduled_task_proves_by_receipt_and_cron_is_refused() {
    let dir = tmp_dir("job");
    let mut job = JobLoop::new(dir.join("ledger.jsonl"));
    let task = job
        .create_task(TaskSpec {
            kind: TaskKind::Scheduled,
            name: "nightly check".to_owned(),
            payload: serde_json::json!({"check": "presence"}),
            run_at: Some(1000),
            handler: "echo".to_owned(),
        })
        .expect("create task");
    let receipts = job.run_due(1000).expect("run due tasks");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].task_id, task.task_id);
    assert_eq!(receipts[0].executed_at, 1000);
    assert_eq!(job.read_receipts().expect("read receipts").len(), 1);

    // A task definition carrying `cron` is refused at the spec.
    let raw = r#"{"kind": "scheduled", "name": "nope", "cron": "*/30 * * * *"}"#;
    assert!(serde_json::from_str::<TaskSpec>(raw).is_err());
}

#[test]
fn modelpool_chain_is_the_pas2_chain() {
    assert_eq!(
        MODELPOOL_CHAIN,
        [
            "freellmapi/auto",
            "claude-sonnet-4-5",
            "qwen3.8-max",
            "gemini-3.8-flash",
            "deepseek-v4-flash",
        ]
    );
    // The provider carries no key: the only key surface is the sentinel.
    assert_eq!(GatewayProvider::get_api_key("openai"), "None");
}
