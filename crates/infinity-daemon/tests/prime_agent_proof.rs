//! Native prime-agent proofs — D-N052 R2 (= F8 pas 6).
//!
//! 1. Spawn returns via the native engine: a recursive sub-agent is
//!    spawned as a function in the persistent REPL, and it returns
//!    (`rlm.spawn` + `rlm.collect`, H-R3).
//! 2. Refine applies session-local, persisted in the durable JSON
//!    harness store (`/refine`, H-R3).
//! 3. A refused update is annulled: the harness returns to the
//!    previous state, on disk (rollback, H-R3).
//! 4. The Runtime cycle with prime-agent as H1 runs against a REAL
//!    loopback HTTP gateway stub: spawn → collect → refine from the
//!    answer → end_turn (H-F8-4, R2-5).
//! 5. A single daemon runs: `pa-daemon` is a reference, never
//!    launched (H-F8-1); the Python runtime is a surface re-pointed
//!    at the single daemon (H-F8-2).
//! 6. Telemetry zero: no analytics endpoint URL in the native
//!    prime-agent modules (H-M4).

use std::io::{Read, Write};
use std::path::PathBuf;

use infinity_daemon::prime_agent_daemon::{DaemonRef, SingleDaemon};
use infinity_daemon::prime_agent_refine::{
    HarnessScope, HarnessState, HarnessStore, RefineOptions, RefinementAction, RefinementEdit,
    RefinementKind, RefinementProposal, apply_refinement_plan, plan_refinement,
};
use infinity_daemon::prime_agent_rlm::{ChildStatus, PrimeAgentRlm, RlmSpawnRequest};
use infinity_daemon::prime_agent_runtime::RuntimeSurface;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn tmp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("riginfinity-pa-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn scripted_provider(prompt: &str) -> Result<String, String> {
    Ok(format!("done: {prompt}"))
}

fn spawn_request(name: &str) -> RlmSpawnRequest {
    RlmSpawnRequest {
        prompt: "summarise the harness state".to_owned(),
        name: Some(name.to_owned()),
        model: Some("gateway/default".to_owned()),
        thinking: Some("low".to_owned()),
        cwd: None,
        spawned_by_request_id: None,
        factory: None,
    }
}

fn proposer(instructions: &str) -> Result<RefinementProposal, String> {
    Ok(RefinementProposal {
        summary: format!("Apply: {instructions}"),
        rationale: "evidence: the child reported the pattern twice".to_owned(),
        expected_outcome: "one memory entry created, session-local".to_owned(),
        edits: vec![RefinementEdit {
            action: RefinementAction::Create,
            kind: RefinementKind::Memory,
            id: None,
            title: instructions.to_owned(),
            content: instructions.to_owned(),
        }],
    })
}

#[test]
fn spawn_returns() {
    let mut engine = PrimeAgentRlm::new(scripted_provider);
    let handle = engine.spawn(spawn_request("summariser"), 0).expect("spawn");
    let results = engine.collect(None, 0);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].rlm_child_id, handle.rlm_child_id);
    assert!(results[0].settled);
    assert_eq!(results[0].status, ChildStatus::Completed);
    assert_eq!(
        results[0].answer_text.as_deref(),
        Some("done: summarise the harness state")
    );
    assert_eq!(engine.spawn_edges().len(), 1);
}

#[test]
fn refine_applies_session_local_on_disk() {
    let dir = tmp_dir("refine");
    let store = HarnessStore::new(&dir);
    let mut state = store.load().expect("load empty store");
    let plan = plan_refinement(
        &[],
        &RefineOptions {
            global: false,
            instructions: Some("check git status before committing".to_owned()),
            rollback_id: None,
        },
        &proposer,
    )
    .expect("plan");
    let result = apply_refinement_plan(&mut state, plan, &RefineOptions::default(), None);
    assert_eq!(result.scope, Some(HarnessScope::Local));
    store.save(&state).expect("save");
    let reloaded = store.load().expect("reload");
    assert_eq!(reloaded.len(), 1);
    assert_eq!(
        reloaded
            .get("check-git-status-before-committing")
            .expect("entry")
            .scope,
        HarnessScope::Local
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn refused_update_rolls_back_on_disk() {
    let dir = tmp_dir("rollback");
    let store = HarnessStore::new(&dir);
    let mut state = store.load().expect("load empty store");
    let plan = plan_refinement(
        &[],
        &RefineOptions {
            global: false,
            instructions: Some("check git status before committing".to_owned()),
            rollback_id: None,
        },
        &proposer,
    )
    .expect("plan");
    let applied = apply_refinement_plan(&mut state, plan, &RefineOptions::default(), None);
    store.save(&state).expect("save applied");
    assert_eq!(store.load().expect("reload").len(), 1);
    let history = vec![applied];
    let rollback = plan_refinement(
        &history,
        &RefineOptions {
            global: false,
            instructions: None,
            rollback_id: Some(history[0].id.clone()),
        },
        &proposer,
    )
    .expect("rollback plan");
    let undone = apply_refinement_plan(&mut state, rollback, &RefineOptions::default(), None);
    assert!(undone.applied_edits.iter().all(|edit| edit.applied));
    store.save(&state).expect("save rolled back");
    assert_eq!(store.load().expect("reload").len(), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A loopback HTTP gateway stub: one POST, one canned chat completion.
async fn gateway_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind gateway stub");
    let port = listener.local_addr().expect("stub port").port();
    let endpoint = format!("http://127.0.0.1:{port}/v1/chat/completions");
    let handle = tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let Ok(n) = stream.read(&mut tmp).await else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "done: summarise the harness state" } }]
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
    });
    (endpoint, handle)
}

/// A synchronous gateway provider closure: POST the prompt to the
/// loopback stub and read the chat completion's content.
fn gateway_provider(endpoint: &str) -> impl FnMut(&str) -> Result<String, String> {
    let endpoint = endpoint.to_owned();
    move |prompt: &str| {
        let url = endpoint.trim_start_matches("http://");
        let (host, path) = url.split_once('/').unwrap_or((url, ""));
        let body = serde_json::json!({
            "model": "gateway/default",
            "messages": [{ "role": "user", "content": prompt }],
        })
        .to_string();
        let mut stream = std::net::TcpStream::connect(host).map_err(|err| err.to_string())?;
        let request = format!(
            "POST /{path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let mut raw = String::new();
        stream
            .read_to_string(&mut raw)
            .map_err(|err| err.to_string())?;
        let (_, json) = raw.split_once("\r\n\r\n").ok_or("no HTTP body")?;
        let value: serde_json::Value = serde_json::from_str(json).map_err(|err| err.to_string())?;
        value["choices"][0]["message"]["content"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| "no content in gateway reply".to_owned())
    }
}

/// The provider closure is synchronous std I/O: it blocks the worker
/// thread while the stub task must be polled on the same runtime, so
/// this test runs on the multi-thread runtime — under the default
/// current-thread runtime the stub would never be polled and the
/// provider would block forever on `read_to_string` (found live:
/// 0 CPU, 8 min, killed).
#[tokio::test(flavor = "multi_thread")]
async fn runtime_cycle_with_h1_over_a_real_gateway_stub() {
    let (endpoint, stub) = gateway_stub().await;
    let mut engine = PrimeAgentRlm::new(gateway_provider(&endpoint));
    let (handle, results) = engine
        .run_cycle(spawn_request("scout"))
        .expect("the H1 cycle runs");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].rlm_child_id, handle.rlm_child_id);
    assert_eq!(
        results[0].answer_text.as_deref(),
        Some("done: summarise the harness state")
    );
    let mut state = HarnessState::new();
    let answer = results[0].answer_text.clone().expect("answer");
    let plan = plan_refinement(
        &[],
        &RefineOptions {
            global: false,
            instructions: Some(answer),
            rollback_id: None,
        },
        &proposer,
    )
    .expect("plan");
    let result = apply_refinement_plan(&mut state, plan, &RefineOptions::default(), None);
    assert_eq!(state.len(), 1);
    assert_eq!(result.scope, Some(HarnessScope::Local));
    assert!(engine.turn_ended());
    stub.abort();
}

#[test]
fn single_daemon_and_repointed_surface() {
    assert!(DaemonRef::pa_daemon().launch_attempt().is_err());
    let guard = SingleDaemon::riginfinity();
    assert!(guard.admit("RigInfinity").is_ok());
    assert!(guard.admit("pa-daemon").is_err());
    let surface = RuntimeSurface::repointed();
    assert!(surface.check().is_ok());
    assert!(!surface.spawns_second_daemon());
}

#[test]
fn no_telemetry_endpoint_in_the_native_modules() {
    // H-M4, structural: the native prime-agent modules carry no
    // analytics endpoint URL. The guard lives in this file (not in
    // `src/`): the guard literal must not sit in a file the guard
    // itself scans.
    let dir = format!("{}/src", env!("CARGO_MANIFEST_DIR"));
    let mut found: Vec<String> = Vec::new();
    scan(&dir, &mut found);
    assert!(
        found.is_empty(),
        "telemetry endpoint URL found in: {found:?}"
    );
}

fn scan(dir: &str, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan(&path.to_string_lossy(), found);
            continue;
        }
        let is_prime_agent = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("prime_agent"))
            && path.extension().is_some_and(|ext| ext == "rs");
        if !is_prime_agent {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path)
            && text.contains("https://api.primeintellect.ai")
        {
            found.push(path.to_string_lossy().into_owned());
        }
    }
}
