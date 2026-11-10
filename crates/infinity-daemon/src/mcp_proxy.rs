//! In-process RAP server that exposes an MCP client over RAP — native
//! port of `crates/infinity-daemon/src/mcp_proxy.rs` (gel `e1a3e4b`)
//! plus the MCP client it serves (`infinity-mcp-bridge`).
//!
//! The bridge owns the adapter contract (tool names, descriptions,
//! schemas, dispatch); this module only serves the RAP manifest and
//! delivers results to the invocation's callback URL. The proxy binds
//! loopback only (`127.0.0.1:0`, D6).
//!
//! Adapter operations (upstream `McpOperation::parse`):
//! `{name}_list_tools` lists the server's tools, `{name}_invoke_tool`
//! calls one (`{"tool_name", "arguments"}`); anything else is
//! formatted as `MCP tool error: unknown operation: …` so the model
//! receives the same error shape from every adapter surface.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response, StatusCode};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::http::serve_http1;
use crate::rap::{
    DisplaySegment, RapCallback, RapInvocation, RapToolResult, ToolDef, ToolsetManifest,
};

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// One MCP transport (a way to speak JSON-RPC to an MCP server).
#[async_trait]
pub trait McpTransport: Send {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError>;
}

pub type McpTransportFactory = Arc<
    dyn Fn() -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Box<dyn McpTransport>, BoxError>> + Send>,
        > + Send
        + Sync,
>;

/// JSON-RPC over the child process's stdio (newline-delimited).
pub struct StdioMcpTransport {
    stdin: tokio::process::ChildStdin,
    stdout_lines: tokio::io::Lines<tokio::io::BufReader<tokio::process::ChildStdout>>,
    next_id: u64,
    initialized: bool,
}

impl StdioMcpTransport {
    pub async fn new(command: &[String], env: &HashMap<String, String>) -> Result<Self, BoxError> {
        let (program, args) = command
            .split_first()
            .ok_or("stdio MCP transport needs a command")?;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .envs(env)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().ok_or("stdio MCP server has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .ok_or("stdio MCP server has no stdout")?;
        let stdout_lines = tokio::io::AsyncBufReadExt::lines(tokio::io::BufReader::new(stdout));
        Ok(Self {
            stdin,
            stdout_lines,
            next_id: 1,
            initialized: false,
        })
    }

    async fn send_raw(&mut self, payload: &serde_json::Value) -> Result<(), BoxError> {
        use tokio::io::AsyncWriteExt;
        let line = serde_json::to_string(payload)?;
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.write_all(b"\n").await?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn read_response(&mut self, id: u64) -> Result<serde_json::Value, BoxError> {
        loop {
            let line = self
                .stdout_lines
                .next_line()
                .await?
                .ok_or("stdio MCP server closed stdout")?;
            if line.trim().is_empty() {
                continue;
            }
            let value: serde_json::Value = serde_json::from_str(&line)?;
            // Notifications (no id) are skipped.
            if value.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                if let Some(err) = value.get("error") {
                    return Err(format!("MCP error: {err}").into());
                }
                return Ok(value
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
        }
    }
}

#[async_trait]
impl McpTransport for StdioMcpTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        if !self.initialized {
            let id = self.next_id;
            self.next_id += 1;
            self.send_raw(&serde_json::json!({
                "jsonrpc": "2.0", "id": id, "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "riginfinity-daemon", "version": "0.2.0"}
                }
            }))
            .await?;
            let _ = self.read_response(id).await?;
            self.send_raw(&serde_json::json!({
                "jsonrpc": "2.0", "method": "notifications/initialized"
            }))
            .await?;
            self.initialized = true;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.send_raw(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method,
            "params": params.unwrap_or(serde_json::json!({}))
        }))
        .await?;
        self.read_response(id).await
    }
}

/// JSON-RPC over Streamable HTTP.
pub struct HttpMcpTransport {
    url: String,
    headers: HashMap<String, String>,
    client: reqwest::Client,
    next_id: u64,
    initialized: bool,
}

impl HttpMcpTransport {
    pub async fn new(url: String, headers: HashMap<String, String>) -> Result<Self, BoxError> {
        Ok(Self {
            url,
            headers,
            client: reqwest::Client::new(),
            next_id: 1,
            initialized: false,
        })
    }
}

#[async_trait]
impl McpTransport for HttpMcpTransport {
    async fn request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        if !self.initialized {
            let mut req = self.client.post(&self.url).json(&serde_json::json!({
                "jsonrpc": "2.0", "id": self.next_id, "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "riginfinity-daemon", "version": "0.2.0"}
                }
            }));
            for (k, v) in &self.headers {
                req = req.header(k, v);
            }
            self.next_id += 1;
            let _ = req.send().await?;
            self.initialized = true;
        }
        let mut req = self.client.post(&self.url).json(&serde_json::json!({
            "jsonrpc": "2.0", "id": self.next_id, "method": method,
            "params": params.unwrap_or(serde_json::json!({}))
        }));
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        self.next_id += 1;
        let resp: serde_json::Value = req.send().await?.json().await?;
        if let Some(err) = resp.get("error") {
            return Err(format!("MCP error: {err}").into());
        }
        Ok(resp
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

/// The MCP client: a lazy transport plus the adapter contract.
pub struct McpClient {
    name: Arc<str>,
    factory: McpTransportFactory,
    transport: Arc<Mutex<Option<Box<dyn McpTransport>>>>,
}

impl Clone for McpClient {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            factory: self.factory.clone(),
            transport: self.transport.clone(),
        }
    }
}

impl McpClient {
    pub fn new(name: impl Into<String>, factory: McpTransportFactory) -> Self {
        Self {
            name: Arc::from(name.into()),
            factory,
            transport: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a lazy stdio MCP client.
    pub fn stdio(
        name: impl Into<String>,
        command: Vec<String>,
        env: HashMap<String, String>,
    ) -> Self {
        Self::new(
            name,
            Arc::new(move || {
                let command = command.clone();
                let env = env.clone();
                Box::pin(async move {
                    Ok(Box::new(StdioMcpTransport::new(&command, &env).await?)
                        as Box<dyn McpTransport>)
                })
            }),
        )
    }

    /// Create a lazy Streamable HTTP MCP client.
    pub fn http(name: impl Into<String>, url: String, headers: HashMap<String, String>) -> Self {
        Self::new(
            name,
            Arc::new(move || {
                let url = url.clone();
                let headers = headers.clone();
                Box::pin(async move {
                    Ok(Box::new(HttpMcpTransport::new(url, headers).await?)
                        as Box<dyn McpTransport>)
                })
            }),
        )
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    async fn transport(
        &self,
    ) -> Result<tokio::sync::MutexGuard<'_, Option<Box<dyn McpTransport>>>, BoxError> {
        let mut guard = self.transport.lock().await;
        if guard.is_none() {
            let t = (self.factory)().await?;
            *guard = Some(t);
        }
        Ok(guard)
    }

    async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, BoxError> {
        let mut guard = self.transport().await?;
        let t = guard.as_mut().expect("bug: transport just built");
        t.request(method, params).await
    }

    /// The server's tools, via `tools/list`.
    pub async fn list_tools(&self) -> Result<Vec<ToolDef>, BoxError> {
        let result = self.call("tools/list", None).await?;
        let tools = result
            .get("tools")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for t in tools {
            out.push(serde_json::from_value::<ToolDef>(t)?);
        }
        Ok(out)
    }

    /// The adapter toolset descriptor (`{name}-mcp`, two adapter tools).
    pub fn describe_toolset(&self) -> McpToolsetDescriptor {
        let name = self.name().to_owned();
        McpToolsetDescriptor {
            name: format!("{name}-mcp"),
            description: format!(
                "MCP server proxy for {name}. Use {name}_list_tools to discover available tools, then {name}_invoke_tool to call them."
            ),
            tools: vec![
                ToolDef {
                    name: format!("{name}_list_tools"),
                    description: "List the tools of the MCP server.".into(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                ToolDef {
                    name: format!("{name}_invoke_tool"),
                    description: "Invoke a tool of the MCP server.".into(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "tool_name": {"type": "string"},
                            "arguments": {"type": "object"}
                        },
                        "required": ["tool_name"]
                    }),
                },
            ],
        }
    }

    /// Execute one adapter tool call and format the outcome for the model.
    pub async fn dispatch(
        &self,
        tool_name: &str,
        arguments: &serde_json::Value,
    ) -> (String, Option<Vec<DisplaySegment>>) {
        let result = if tool_name.ends_with("_list_tools") {
            self.list_tools()
                .await
                .map(|tools| {
                    let mut text = String::new();
                    for t in tools {
                        text.push_str(&format!("- **{}**: {}\n", t.name, t.description));
                    }
                    let display = vec![DisplaySegment::Text(text.clone())];
                    (text, Some(display))
                })
                .map_err(|e| e.to_string())
        } else if tool_name.ends_with("_invoke_tool") {
            let target = arguments
                .get("tool_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let target_arguments = arguments
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            self.invoke_tool(&target, target_arguments)
                .await
                .map(|text| (text, None))
                .map_err(|e| e.to_string())
        } else {
            Err(format!("unknown operation: {tool_name}"))
        };
        result.unwrap_or_else(|error| (format!("MCP tool error: {error}"), None))
    }

    async fn invoke_tool(
        &self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String, BoxError> {
        let result = self
            .call(
                "tools/call",
                Some(serde_json::json!({"name": tool_name, "arguments": arguments})),
            )
            .await?;
        let mut text = String::new();
        if let Some(content) = result.get("content").and_then(serde_json::Value::as_array) {
            for item in content {
                if let Some(t) = item.get("text").and_then(serde_json::Value::as_str) {
                    text.push_str(t);
                }
            }
        }
        if text.is_empty() {
            text = serde_json::to_string(&result)?;
        }
        Ok(text)
    }
}

/// The adapter toolset descriptor served in the RAP manifest.
#[derive(Debug, Clone)]
pub struct McpToolsetDescriptor {
    pub name: String,
    pub description: String,
    pub tools: Vec<ToolDef>,
}

/// `true` iff the host of `url` is loopback (127.0.0.0/8, ::1 or
/// `localhost`). Used to refuse exfiltration via a caller-supplied
/// RAP callback URL (R-3).
fn callback_url_is_loopback(url: &str) -> bool {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    let host_port = after_scheme.split('/').next().unwrap_or("");
    let host_port = host_port.rsplit_once('@').map_or(host_port, |(_, h)| h);
    // Bracketed IPv6 (`[::1]:port`) — splitting on ':' first would cut it.
    let host = if let Some(bracketed) = host_port.strip_prefix('[') {
        bracketed.split(']').next().unwrap_or("")
    } else {
        host_port.split(':').next().unwrap_or("")
    };
    host == "localhost"
        || host == "::1"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn json_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("bug: failed to build HTTP response")
}

fn text_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("bug: failed to build HTTP response")
}

async fn handle(req: Request<Incoming>, client: McpClient, port: u16) -> Response<Full<Bytes>> {
    let path = req.uri().path().to_owned();
    let method = req.method().clone();

    if method == hyper::Method::GET && path.contains(".well-known/rap-toolset") {
        let descriptor = client.describe_toolset();
        let manifest = ToolsetManifest {
            name: descriptor.name,
            description: Some(descriptor.description),
            endpoint: format!("http://127.0.0.1:{port}"),
            tools: descriptor.tools,
            needs_migration: false,
        };
        let manifest =
            serde_json::to_string(&manifest).expect("bug: failed to serialize RAP manifest");
        return json_response(StatusCode::OK, &manifest);
    }

    if method != hyper::Method::POST {
        return text_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }
    let body = match req.into_body().collect().await {
        Ok(body) => body.to_bytes(),
        Err(error) => return text_response(StatusCode::BAD_REQUEST, &format!("bad body: {error}")),
    };
    let invocation: RapInvocation = match serde_json::from_slice(&body) {
        Ok(invocation) => invocation,
        Err(error) => return text_response(StatusCode::BAD_REQUEST, &format!("bad json: {error}")),
    };

    tokio::spawn(async move {
        // R-3 (audit): the callback URL comes from the caller — refuse
        // any non-loopback target before posting (upstream does not).
        if !callback_url_is_loopback(&invocation.callback_url) {
            tracing::error!(
                "Refused non-loopback RAP callback URL: {}",
                invocation.callback_url
            );
            return;
        }
        let (text, display_as) = client
            .dispatch(&invocation.operation, &invocation.arguments)
            .await;
        let callback = serde_json::to_string(&RapCallback::ToolResult(RapToolResult {
            group_id: invocation.group_id,
            id: invocation.id,
            call_id: invocation.call_id,
            text: Some(text),
            content: None,
            display_as,
            subscription: None,
        }))
        .expect("bug: failed to serialize RAP callback");

        match reqwest::Client::new()
            .post(&invocation.callback_url)
            .header("content-type", "application/json")
            .body(callback)
            .send()
            .await
        {
            Ok(response) if !response.status().is_success() => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                tracing::error!("Callback rejected MCP result (HTTP {status}): {body}");
            }
            Err(error) => tracing::warn!("Failed to send MCP result to callback: {error}"),
            _ => {}
        }
    });

    text_response(StatusCode::OK, "OK")
}

/// Start an MCP proxy RAP server for a stdio subprocess.
pub async fn start_mcp_proxy(
    name: String,
    command: Vec<String>,
    env: HashMap<String, String>,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    start_client_proxy(McpClient::stdio(name, command, env)).await
}

/// Start an MCP proxy RAP server for a Streamable HTTP server.
pub async fn start_http_mcp_proxy(
    name: String,
    url: String,
    headers: HashMap<String, String>,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    start_client_proxy(McpClient::http(name, url, headers)).await
}

/// Start a proxy from a custom MCP transport factory.
pub async fn start_proxy_server(
    name: String,
    factory: McpTransportFactory,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    start_client_proxy(McpClient::new(name, factory)).await
}

async fn start_client_proxy(
    client: McpClient,
) -> Result<(u16, tokio::task::JoinHandle<()>), BoxError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(serve_http1(
        listener,
        Arc::new(move |req: Request<Incoming>| {
            let client = client.clone();
            async move { handle(req, client, port).await }
        }),
    ));
    tracing::info!("MCP proxy RAP server listening on port {port}");
    Ok((port, task))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_url_must_be_loopback() {
        assert!(callback_url_is_loopback("http://127.0.0.1:41234/callback"));
        assert!(callback_url_is_loopback("http://localhost:9/callback"));
        assert!(callback_url_is_loopback("http://[::1]:9/callback"));
        assert!(!callback_url_is_loopback("http://0.0.0.0:9/callback"));
        assert!(!callback_url_is_loopback("http://192.168.1.10:9/callback"));
        assert!(!callback_url_is_loopback("https://example.com/callback"));
    }
}
