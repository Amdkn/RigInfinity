//! HTTP callback routing for RAP tool results — native port of
//! `crates/infinity-daemon/src/rap_callback.rs` (gel `e1a3e4b`).
//!
//! The bridge decodes callbacks, forwards agent inputs straight into
//! the session manager's input queue, and hands display-only view
//! updates to the view channel. Admission is owned by the session
//! manager: its thread-existence and stopped-thread checks refuse
//! events for unknown or user-stopped sessions before any driver
//! spawns — so a tool result for a hibernated agent waits (it is
//! delivered when the agent wakes on user text), and a callback for an
//! unknown thread is dropped.

use std::sync::Arc;

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::mpsc;

use crate::http::serve_http1;
use crate::rap::RapCallback;
use crate::session::{InputMessage, SessionManager, SharedSessionManager};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A display-only view update, forwarded to subscribers.
#[derive(Debug, Clone)]
pub struct RapView {
    pub view_type: String,
    pub group_id: String,
    pub content: serde_json::Value,
}

/// The RAP callback bridge: a loopback-only HTTP listener whose URL is
/// handed to RAP tool servers as their callback URL.
pub struct RapCallbackBridge {
    listener: TcpListener,
    port: u16,
}

impl RapCallbackBridge {
    /// Bind the callback listener on loopback (OS-assigned port).
    pub async fn bind() -> Result<Self, BoxError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    pub fn callback_url(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Start the callback accept loop, forwarding into the session
    /// manager. Returns the view-update channel; the server task runs
    /// until the runtime shuts down.
    pub fn serve_into(
        self,
        session_manager: SharedSessionManager,
    ) -> (
        mpsc::UnboundedReceiver<RapView>,
        tokio::task::JoinHandle<()>,
    ) {
        let (view_tx, view_rx) = mpsc::unbounded_channel::<RapView>();
        let listener = self.listener;
        let task = tokio::spawn(serve_http1(
            listener,
            Arc::new(move |req: Request<Incoming>| {
                let manager = session_manager.clone();
                let view_tx = view_tx.clone();
                async move { handle_callback(req, manager, view_tx).await }
            }),
        ));
        (view_rx, task)
    }
}

async fn handle_callback(
    req: Request<Incoming>,
    manager: SharedSessionManager,
    view_tx: mpsc::UnboundedSender<RapView>,
) -> Response<Full<Bytes>> {
    if req.method() != hyper::Method::POST {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }
    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => return text_response(StatusCode::BAD_REQUEST, &format!("bad body: {e}")),
    };
    let callback: RapCallback = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => return text_response(StatusCode::BAD_REQUEST, &format!("bad json: {e}")),
    };
    match callback {
        RapCallback::ToolResult(result) => {
            // Admission lives in the session manager: a result for a
            // hibernated agent is refused here and waits for wake; the
            // bridge never spawns a driver itself.
            let accepted = {
                let mgr = manager.lock().await;
                mgr.send_input(InputMessage::ToolResult {
                    thread_id: result.group_id.clone(),
                    result,
                })
                .await
            };
            if !accepted {
                tracing::info!("RAP tool result refused (unknown or stopped thread)");
            }
        }
        RapCallback::ViewUpdate(update) => {
            // Display-only: forwarded to the view channel; the
            // `serve_callbacks` loop below persists + broadcasts it.
            // It never enters agent history.
            let _ = view_tx.send(RapView {
                view_type: update.view_type,
                group_id: update.group_id,
                content: update.content,
            });
        }
        RapCallback::SubscriptionEvent(event) => {
            let _ = view_tx.send(RapView {
                view_type: "subscription".into(),
                group_id: event.group_id,
                content: serde_json::json!({"text": event.text}),
            });
        }
        RapCallback::UserChoice(choice) => {
            let _ = view_tx.send(RapView {
                view_type: "user_choice".into(),
                group_id: choice.group_id,
                content: serde_json::json!({"prompt": choice.prompt, "choices": choice.choices}),
            });
        }
        RapCallback::OAuth(oauth) => {
            let _ = view_tx.send(RapView {
                view_type: "oauth".into(),
                group_id: oauth.group_id,
                content: serde_json::json!({"auth_url": oauth.auth_url}),
            });
        }
    }
    text_response(StatusCode::OK, "OK")
}

fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("bug: failed to build HTTP response")
}

/// Start the callback accept loop for an already-built [`SessionManager`]
/// (whose `callback_url` must match the bridge's). Agent inputs flow
/// into the session manager's input sender; view updates are persisted
/// and broadcast to subscribers.
pub fn serve_callbacks(
    bridge: RapCallbackBridge,
    session_manager: SessionManager,
) -> SharedSessionManager {
    let shared: SharedSessionManager = Arc::new(tokio::sync::Mutex::new(session_manager));
    let (mut views, _callback_server_task) = bridge.serve_into(shared.clone());
    let sm = shared.clone();
    tokio::task::spawn(async move {
        while let Some(update) = views.recv().await {
            tracing::info!(
                "RAP view: type={} group={}",
                update.view_type,
                update.group_id
            );
            let manager = sm.lock().await;
            manager
                .handle_view_update(&update.group_id, &update.view_type, update.content)
                .await;
        }
    });
    shared
}
