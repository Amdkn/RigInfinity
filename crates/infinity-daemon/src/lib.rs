//! RigInfinity Deterministic Rust Daemon Core (A'Space OS V5)
//! Merges hydro-project/infinity RAP session engine with OpenRig 578-Holon Quality Control.
//!
//! D-N051 F8 pas 3 — the daemon is no longer a stub: the four
//! functions of the upstream `infinity-daemon` (gel `e1a3e4b`) are
//! ported here as native modules (session manager, RAP callback,
//! MCP proxy, memory/session stores). Providers (bedrock, rig) and
//! the slack-bot are NOT ported (D3 OUT); the WebSocket + web-asset
//! UI is NOT ported either (UI surface — local clients use the Unix
//! socket, remote tools use RAP). The daemon is local-first: every
//! listener binds 127.0.0.1 only (D6), never 0.0.0.0.
//!
//! D-N051 F8 pas 4 — the OpenRig daemon is retired: its three functions
//! live here as native modules (`openrig_state` for the SQLite state,
//! `openrig_context_packs` for the context packs, `openrig_terminal`
//! for the Herdr socket transport, `openrig_lineage` for the ULID
//! lineage scheduler that replaces the bind's `Date.now()` spawner).
//! The Hono HTTP server is NOT ported — its routes are re-implemented
//! here, in Rust, not copied.

pub mod client;
pub mod http;
pub mod mcp_proxy;
pub mod memory_store;
pub mod openrig_context_packs;
pub mod openrig_lineage;
pub mod openrig_state;
pub mod openrig_terminal;
pub mod rap;
pub mod rap_callback;
pub mod rap_servers;
pub mod session;
pub mod session_store;

use std::path::PathBuf;

use tokio::net::UnixListener;

use crate::rap_callback::{RapCallbackBridge, serve_callbacks};
use crate::session::{SessionManager, SharedSessionManager};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HolonExecutionState {
    Dormant,
    ClaimedExclusive {
        borrower_holon: String,
        fencing_token: u64,
    },
    ExecutingGoldfish {
        worktree: String,
    },
    VerifyingOutsideJudge,
    HaltedByBeth,
}

#[derive(Debug, Clone)]
pub struct RigInfinitySession {
    pub root_thread_id: String,
    pub area_code: String,
    pub fractal_layer: String,
    pub state: HolonExecutionState,
}

/// Population cap, carried as a constant (D8). **Not measured, not
/// guaranteed**: the scheduler cap from the bind / README claim.
pub const INFINITY_POPULATION_CAP: u32 = 75_000;
/// Fractal layer the plugin declares authoritative (4L, D2).
pub const FRACTAL_LAYER: &str = "4L";

/// Line printed to stdout when `run_daemon` is called with
/// `announce_ready = true`, once initialization has succeeded and the daemon
/// is serving. It must remain the daemon's only stdout output.
pub const DAEMON_READY_LINE: &str = "infinity-daemon ready";

/// State directory: `$INFINITY_STATE_DIR`, else `~/.infinity`.
pub fn state_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("INFINITY_STATE_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    PathBuf::from(home).join(".infinity")
}

fn socket_path() -> PathBuf {
    state_dir().join("daemon.sock")
}

/// Loopback guard (D6): refuse any bind host that is not loopback.
fn require_loopback(host: &str) -> Result<(), BoxError> {
    let host = host.trim().trim_matches(['[', ']']);
    let ok = host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if ok {
        Ok(())
    } else {
        Err(format!("refused non-loopback bind {host:?}: the daemon is localhost-only (D6)").into())
    }
}

/// Launch a fully-initialized session manager: bind the RAP callback
/// listener, build the [`SessionManager`] with the listener's callback
/// URL, and start serving callbacks into it. This is the shared startup
/// path for the daemon and direct (daemonless) mode.
pub async fn launch_session_manager(state_dir: PathBuf) -> Result<SharedSessionManager, BoxError> {
    let bridge = RapCallbackBridge::bind().await?;
    let session_manager = SessionManager::new(state_dir, bridge.callback_url().to_owned()).await?;
    Ok(serve_callbacks(bridge, session_manager))
}

/// Run the daemon until it receives a shutdown signal. When
/// `announce_ready` is true, prints [`DAEMON_READY_LINE`] to stdout
/// after all initialization has succeeded.
pub async fn run_daemon(announce_ready: bool) -> Result<(), BoxError> {
    let dir = state_dir();
    std::fs::create_dir_all(&dir)?;

    // The WS bind is honoured as a loopback-only policy even though the
    // WS server itself is not ported (UI surface): a non-loopback
    // `INFINITY_WS_BIND` is refused rather than silently ignored.
    let ws_bind_addr = std::env::var("INFINITY_WS_BIND").unwrap_or_else(|_| "127.0.0.1".to_owned());
    require_loopback(&ws_bind_addr)?;

    let sock_path = socket_path();
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }
    let listener = UnixListener::bind(&sock_path)?;
    tracing::info!("daemon listening on {}", sock_path.display());

    let session_manager = launch_session_manager(dir).await?;
    tracing::info!("shared callback server started");

    if announce_ready {
        println!("{DAEMON_READY_LINE}");
    }

    let shutdown = async {
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("failed to register SIGINT handler");
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to register SIGTERM handler");
        tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        }
        tracing::info!("received shutdown signal");
    };

    tokio::select! {
        _ = async {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        let mgr = session_manager.clone();
                        tokio::task::spawn(async move {
                            client::handle_client(stream, mgr).await;
                        });
                    }
                    Err(e) => {
                        tracing::warn!("accept error: {e}");
                    }
                }
            }
        } => {}
        _ = shutdown => {}
    }

    let _ = std::fs::remove_file(&sock_path);

    // Stop every booted RAP server; sessions themselves need no teardown
    // (their state is already durable, and they resume on the next input
    // after the daemon restarts).
    {
        let mgr = session_manager.lock().await;
        mgr.shutdown_all_rap().await;
    }

    tracing::info!("daemon shut down");
    Ok(())
}
