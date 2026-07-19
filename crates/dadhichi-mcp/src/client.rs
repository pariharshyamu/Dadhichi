//! A live MCP client, transport-agnostic.
//!
//! [`McpConnection`] is a thin facade over a pluggable [`Transport`]: every
//! high-level call (`handshake`, `list_tools`, `call_tool`) funnels through a
//! single `request(method, params)` primitive, so the same protocol logic drives
//! any wire.
//!
//! Three transports ship:
//!
//! - **stdio** — a real server subprocess (or an in-memory pipe in tests) over a
//!   line-delimited JSON-RPC stream, via [`connect_stdio`](McpConnection::connect_stdio).
//! - **Streamable HTTP** — a hosted server reached by POSTing JSON-RPC to a URL,
//!   accepting either a JSON or an SSE (`text/event-stream`) reply, with the
//!   `Mcp-Session-Id` carried across calls.
//! - **WebSocket** — a persistent bidirectional JSON-RPC socket.
//!
//! The two networked transports live behind the `remote` feature so a default
//! build stays offline and dependency-light.

use crate::protocol::{
    McpClient, McpError, PromptSpec, ResourceSpec, RpcRequest, RpcResponse, ServerCapabilities,
};
use crate::tool::{Permission, ToolSpec};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, McpError>>>>>;

/// The request/response primitive every MCP transport provides. Given a method
/// and params, it returns the call's `result` (or a JSON-RPC error), taking care
/// of id correlation internally.
#[async_trait]
trait Transport: Send + Sync + std::fmt::Debug {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError>;
}

/// A connection to an MCP server over some [`Transport`].
pub struct McpConnection {
    transport: Box<dyn Transport>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection").finish_non_exhaustive()
    }
}

impl McpConnection {
    /// Build a connection over an arbitrary byte stream (stdio, an in-memory
    /// pipe), spawning the reader loop.
    pub fn new<R, W>(reader: R, writer: W) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            transport: Box::new(StreamTransport::new(reader, writer, None)),
        }
    }

    /// Launch an MCP server subprocess and speak to it over stdio.
    pub async fn connect_stdio(command: &str, args: &[&str]) -> Result<Self, McpError> {
        let owned: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        Self::connect_stdio_env(command, &owned, &[]).await
    }

    /// Launch an MCP server subprocess with an explicit environment (for API
    /// tokens and the like) and speak to it over stdio.
    pub async fn connect_stdio_env(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, McpError> {
        use std::process::Stdio;
        let command = &resolve_command(command);
        let spawn_direct = || {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                // The server's lifetime is the connection's: dropping the
                // transport must reap the subprocess, not orphan it.
                .kill_on_drop(true);
            cmd.spawn()
        };
        let mut child = match spawn_direct() {
            Ok(child) => child,
            // MCP servers are usually installed as npm/py launcher shims
            // (`npx`, `uvx`, `claude`, `gemini` are .cmd/.ps1 wrappers on
            // Windows) which CreateProcess can't start by bare name — fall
            // back to cmd.exe, which resolves PATHEXT.
            Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::NotFound => {
                let mut cmd = tokio::process::Command::new("cmd");
                cmd.arg("/C")
                    .arg(command)
                    .args(args)
                    .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                cmd.spawn().map_err(|e| McpError::Transport(e.to_string()))?
            }
            Err(err) => return Err(McpError::Transport(err.to_string())),
        };
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("no stdout".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("no stdin".into()))?;
        Ok(Self {
            transport: Box::new(StreamTransport::new(stdout, stdin, Some(child))),
        })
    }

    /// Connect to a remote MCP server by URL, choosing the transport by scheme:
    /// `http(s)://` → Streamable HTTP, `ws(s)://` → WebSocket. `headers` are sent
    /// on every request (HTTP) or on the handshake (WebSocket) — the place for an
    /// `Authorization` bearer token.
    #[cfg(feature = "remote")]
    pub async fn connect_url(url: &str, headers: &[(String, String)]) -> Result<Self, McpError> {
        if url.starts_with("ws://") || url.starts_with("wss://") {
            let transport = WsTransport::connect(url, headers).await?;
            Ok(Self {
                transport: Box::new(transport),
            })
        } else if url.starts_with("http://") || url.starts_with("https://") {
            let transport = HttpTransport::connect(url, headers)?;
            Ok(Self {
                transport: Box::new(transport),
            })
        } else {
            Err(McpError::Transport(format!(
                "unsupported MCP url scheme: {url}"
            )))
        }
    }

    /// Send a request and await its correlated response result.
    pub async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        self.transport.request(method, params).await
    }

    /// Perform the `initialize` handshake, returning negotiated capabilities.
    pub async fn handshake(&self) -> Result<ServerCapabilities, McpError> {
        let result = self
            .request(
                "initialize",
                serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "dadhichi", "version": "0.1.0" }
                }),
            )
            .await?;
        let caps = result.get("capabilities");
        Ok(ServerCapabilities {
            tools: caps.and_then(|c| c.get("tools")).is_some(),
            resources: caps.and_then(|c| c.get("resources")).is_some(),
            prompts: caps.and_then(|c| c.get("prompts")).is_some(),
        })
    }

    /// List the tools the server exposes (`tools/list`).
    pub async fn list_tools(&self) -> Result<Vec<ToolSpec>, McpError> {
        let result = self.request("tools/list", serde_json::json!({})).await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
            .into_iter()
            .filter_map(|t| {
                Some(ToolSpec {
                    name: t.get("name")?.as_str()?.to_string(),
                    description: t
                        .get("description")
                        .and_then(|d| d.as_str())
                        .unwrap_or("")
                        .to_string(),
                    input_schema: t
                        .get("inputSchema")
                        .cloned()
                        .unwrap_or(serde_json::json!({})),
                    // External MCP tools are gated behind Network by default.
                    permissions: vec![Permission::Network],
                })
            })
            .collect())
    }

    /// Invoke a tool on the server (`tools/call`).
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
        .await
    }

    /// List the readable resources the server exposes (`resources/list`).
    pub async fn list_resources(&self) -> Result<Vec<ResourceSpec>, McpError> {
        let result = self
            .request("resources/list", serde_json::json!({}))
            .await?;
        decode_array(&result, "resources")
    }

    /// Read a resource's contents by URI (`resources/read`). The returned value
    /// is the server's `contents` array (text and/or blobs).
    pub async fn read_resource(&self, uri: &str) -> Result<serde_json::Value, McpError> {
        self.request("resources/read", serde_json::json!({ "uri": uri }))
            .await
    }

    /// List the prompt templates the server exposes (`prompts/list`).
    pub async fn list_prompts(&self) -> Result<Vec<PromptSpec>, McpError> {
        let result = self.request("prompts/list", serde_json::json!({})).await?;
        decode_array(&result, "prompts")
    }

    /// Instantiate a prompt template with `arguments` (`prompts/get`). The
    /// returned value is the server's rendered `messages` (plus any description).
    pub async fn get_prompt(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        self.request(
            "prompts/get",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
        .await
    }
}

/// Decode the `key` array of an MCP list response into typed specs, skipping any
/// entry that doesn't deserialize rather than failing the whole call.
fn decode_array<T: serde::de::DeserializeOwned>(
    result: &serde_json::Value,
    key: &str,
) -> Result<Vec<T>, McpError> {
    let items = result
        .get(key)
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(items
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

#[async_trait]
impl McpClient for McpConnection {
    async fn initialize(&self) -> Result<ServerCapabilities, McpError> {
        self.handshake().await
    }

    async fn call(&self, request: RpcRequest) -> Result<RpcResponse, McpError> {
        let result = self
            .request(
                &request.method,
                request.params.unwrap_or(serde_json::Value::Null),
            )
            .await;
        Ok(match result {
            Ok(value) => RpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id,
                result: Some(value),
                error: None,
            },
            Err(McpError::Rpc { code, message }) => RpcResponse {
                jsonrpc: "2.0".into(),
                id: request.id,
                result: None,
                error: Some(crate::protocol::RpcErrorObject { code, message }),
            },
            Err(e) => return Err(e),
        })
    }
}

// ── Stream transport (stdio + in-memory pipe) ────────────────────────────────

/// A JSON-RPC transport over a line-delimited byte stream. A background reader
/// correlates responses to requests by id.
struct StreamTransport {
    writer: SharedWriter,
    pending: Pending,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
    _child: Option<tokio::process::Child>,
}

impl std::fmt::Debug for StreamTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamTransport").finish_non_exhaustive()
    }
}

impl Drop for StreamTransport {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

impl StreamTransport {
    fn new<R, W>(reader: R, writer: W, child: Option<tokio::process::Child>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = tokio::spawn(read_loop(reader, pending.clone()));
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            reader_task,
            _child: child,
        }
    }
}

#[async_trait]
impl Transport for StreamTransport {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = write_line(&self.writer, &message).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        rx.await
            .map_err(|_| McpError::Transport("connection closed".into()))?
    }
}

async fn write_line(writer: &SharedWriter, message: &serde_json::Value) -> Result<(), McpError> {
    let mut line = serde_json::to_vec(message).map_err(|e| McpError::Protocol(e.to_string()))?;
    line.push(b'\n');
    let mut guard = writer.lock().await;
    guard
        .write_all(&line)
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
    guard
        .flush()
        .await
        .map_err(|e| McpError::Transport(e.to_string()))?;
    Ok(())
}

async fn read_loop<R: AsyncRead + Unpin>(reader: R, pending: Pending) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            dispatch(value, &pending).await;
        }
    }
    // Transport closed: fail all outstanding requests.
    fail_all(&pending).await;
}

/// Correlate an incoming JSON-RPC message to its waiting request by id.
async fn dispatch(message: serde_json::Value, pending: &Pending) {
    let Some(id) = message.get("id").and_then(|v| v.as_u64()) else {
        return; // a notification — nothing awaits it
    };
    if let Some(tx) = pending.lock().await.remove(&id) {
        let _ = tx.send(outcome_from_message(&message));
    }
}

async fn fail_all(pending: &Pending) {
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(McpError::Transport("connection closed".into())));
    }
}

/// Extract the `result` from a JSON-RPC response, or map its `error` object to an
/// [`McpError::Rpc`].
fn outcome_from_message(message: &serde_json::Value) -> Result<serde_json::Value, McpError> {
    match message.get("error") {
        Some(err) => Err(McpError::Rpc {
            code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
            message: err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string(),
        }),
        None => Ok(message
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null)),
    }
}

// ── Streamable HTTP transport ────────────────────────────────────────────────

#[cfg(feature = "remote")]
#[derive(Debug)]
struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: reqwest::header::HeaderMap,
    next_id: AtomicU64,
    /// The `Mcp-Session-Id` the server hands out at `initialize`, echoed on every
    /// later request so the server can pin us to one session.
    session: Mutex<Option<String>>,
}

#[cfg(feature = "remote")]
impl HttpTransport {
    fn connect(url: &str, headers: &[(String, String)]) -> Result<Self, McpError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| McpError::Transport(e.to_string()))?;
        Self::with_client(client, url, headers)
    }

    fn with_client(
        client: reqwest::Client,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Self, McpError> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::Transport(format!("bad header name `{k}`: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| McpError::Transport(format!("bad header value for `{k}`: {e}")))?;
            map.insert(name, value);
        }
        Ok(Self {
            client,
            url: url.to_string(),
            headers: map,
            next_id: AtomicU64::new(1),
            session: Mutex::new(None),
        })
    }
}

#[cfg(feature = "remote")]
#[async_trait]
impl Transport for HttpTransport {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        use reqwest::header::{ACCEPT, CONTENT_TYPE};
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let body =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });

        let mut req = self
            .client
            .post(&self.url)
            .headers(self.headers.clone())
            .header(ACCEPT, "application/json, text/event-stream")
            .json(&body);
        if let Some(session) = self.session.lock().await.clone() {
            req = req.header("mcp-session-id", session);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session.lock().await = Some(sid.to_string());
        }

        let status = resp.status();
        let ctype = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp
            .text()
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;

        if !status.is_success() {
            return Err(McpError::Transport(format!("http {status}: {text}")));
        }

        if ctype.contains("text/event-stream") {
            parse_sse(&text, id)
        } else {
            let value: serde_json::Value =
                serde_json::from_str(&text).map_err(|e| McpError::Protocol(e.to_string()))?;
            outcome_from_message(&value)
        }
    }
}

/// Pull the JSON-RPC response matching `id` out of an SSE body (`data:` lines,
/// events separated by blank lines), falling back to the last decodable event.
#[cfg(feature = "remote")]
fn parse_sse(body: &str, id: u64) -> Result<serde_json::Value, McpError> {
    let mut data = String::new();
    let mut last: Option<serde_json::Value> = None;

    let flush = |data: &mut String, last: &mut Option<serde_json::Value>| {
        if data.is_empty() {
            return None;
        }
        let parsed = serde_json::from_str::<serde_json::Value>(data).ok();
        data.clear();
        if let Some(value) = parsed {
            if value.get("id").and_then(|i| i.as_u64()) == Some(id) {
                return Some(value);
            }
            *last = Some(value);
        }
        None
    };

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        } else if line.is_empty()
            && let Some(matched) = flush(&mut data, &mut last)
        {
            return outcome_from_message(&matched);
        }
    }
    if let Some(matched) = flush(&mut data, &mut last) {
        return outcome_from_message(&matched);
    }

    match last {
        Some(value) => outcome_from_message(&value),
        None => Err(McpError::Protocol("no SSE data event".into())),
    }
}

// ── WebSocket transport ──────────────────────────────────────────────────────

#[cfg(feature = "remote")]
type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[cfg(feature = "remote")]
struct WsTransport {
    writer: Mutex<futures::stream::SplitSink<WsStream, tokio_tungstenite::tungstenite::Message>>,
    pending: Pending,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
}

#[cfg(feature = "remote")]
impl std::fmt::Debug for WsTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsTransport").finish_non_exhaustive()
    }
}

#[cfg(feature = "remote")]
impl Drop for WsTransport {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

#[cfg(feature = "remote")]
impl WsTransport {
    async fn connect(url: &str, headers: &[(String, String)]) -> Result<Self, McpError> {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};

        let mut request = url
            .into_client_request()
            .map_err(|e| McpError::Transport(e.to_string()))?;
        for (k, v) in headers {
            let name = HeaderName::from_bytes(k.as_bytes())
                .map_err(|e| McpError::Transport(format!("bad header name `{k}`: {e}")))?;
            let value = HeaderValue::from_str(v)
                .map_err(|e| McpError::Transport(format!("bad header value for `{k}`: {e}")))?;
            request.headers_mut().insert(name, value);
        }

        let (stream, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| McpError::Transport(e.to_string()))?;
        let (sink, source) = stream.split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = tokio::spawn(ws_read_loop(source, pending.clone()));
        Ok(Self {
            writer: Mutex::new(sink),
            pending,
            next_id: AtomicU64::new(1),
            reader_task,
        })
    }
}

#[cfg(feature = "remote")]
#[async_trait]
impl Transport for WsTransport {
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, McpError> {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message;

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let text =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
                .to_string();
        if let Err(e) = self.writer.lock().await.send(Message::text(text)).await {
            self.pending.lock().await.remove(&id);
            return Err(McpError::Transport(e.to_string()));
        }
        rx.await
            .map_err(|_| McpError::Transport("connection closed".into()))?
    }
}

#[cfg(feature = "remote")]
async fn ws_read_loop(mut source: futures::stream::SplitStream<WsStream>, pending: Pending) {
    use futures::StreamExt;
    use tokio_tungstenite::tungstenite::Message;

    while let Some(Ok(message)) = source.next().await {
        if let Message::Text(text) = message
            && let Ok(value) = serde_json::from_str::<serde_json::Value>(text.as_str())
        {
            dispatch(value, &pending).await;
        }
    }
    fail_all(&pending).await;
}

/// Resolve a connector command to something actually launchable.
///
/// Most commands pass through untouched (PATH lookup, plus the `cmd /C`
/// PATHEXT fallback above). `claude` gets special care: many users run Claude
/// Code only through their editor's extension, so the CLI is not on PATH at
/// all — but the very same binary ships inside the extension. Resolution
/// order: PATH-installed name as-is if an env override is absent →
/// `CLAUDE_CODE_EXECPATH` (set inside Claude Code sessions) → the newest
/// VS Code extension's `native-binary/claude(.exe)`.
pub fn resolve_launcher(command: &str) -> String {
    resolve_command(command)
}

fn resolve_command(command: &str) -> String {
    if command != "claude" {
        return command.to_string();
    }
    // An explicit env override always wins (also set inside CC sessions).
    if let Ok(path) = std::env::var("CLAUDE_CODE_EXECPATH")
        && std::path::Path::new(&path).is_file()
    {
        return path;
    }
    // The editor extension's bundled binary, newest version first.
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    let extensions = std::path::Path::new(&home).join(".vscode").join("extensions");
    if let Ok(entries) = std::fs::read_dir(&extensions) {
        let mut candidates: Vec<std::path::PathBuf> = entries
            .flatten()
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("anthropic.claude-code-"))
            })
            .map(|e| e.path())
            .collect();
        candidates.sort();
        for dir in candidates.into_iter().rev() {
            for name in ["claude.exe", "claude"] {
                let bin = dir.join("resources").join("native-binary").join(name);
                if bin.is_file() {
                    return bin.display().to_string();
                }
            }
        }
    }
    command.to_string()
}

#[cfg(test)]
mod resolve_tests {
    use super::resolve_command;

    #[test]
    fn ordinary_commands_pass_through_untouched() {
        assert_eq!(resolve_command("npx"), "npx");
        assert_eq!(resolve_command("uvx"), "uvx");
        assert_eq!(resolve_command("gemini"), "gemini");
    }

    #[test]
    fn claude_resolves_to_a_real_binary_or_stays_bare() {
        // Environment-dependent by design: inside a Claude Code session or on
        // a machine with the VS Code extension this must yield an existing
        // file; elsewhere the bare name comes back for normal PATH lookup.
        let resolved = resolve_command("claude");
        assert!(
            resolved == "claude" || std::path::Path::new(&resolved).is_file(),
            "resolved to a non-existent path: {resolved}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    /// A tiny in-memory MCP server answering the handshake and tool calls.
    async fn mock_server(
        reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = writer;
        while let Ok(Some(line)) = lines.next_line().await {
            let req: serde_json::Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let result = reply_for(method);
            let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = writer.write_all(&bytes).await;
            let _ = writer.flush().await;
        }
    }

    /// The canned result a mock server returns for each MCP method.
    fn reply_for(method: &str) -> serde_json::Value {
        match method {
            "initialize" => serde_json::json!({
                "capabilities": { "tools": {}, "resources": {}, "prompts": {} }
            }),
            "tools/list" => serde_json::json!({
                "tools": [{ "name": "search", "description": "web search", "inputSchema": { "type": "object" } }]
            }),
            "tools/call" => serde_json::json!({ "content": [{ "type": "text", "text": "ok" }] }),
            "resources/list" => serde_json::json!({
                "resources": [{ "uri": "file:///readme.md", "name": "readme", "mimeType": "text/markdown" }]
            }),
            "resources/read" => serde_json::json!({
                "contents": [{ "uri": "file:///readme.md", "text": "# Hello" }]
            }),
            "prompts/list" => serde_json::json!({
                "prompts": [{ "name": "summarize", "description": "Summarize text",
                    "arguments": [{ "name": "text", "required": true }] }]
            }),
            "prompts/get" => serde_json::json!({
                "messages": [{ "role": "user", "content": { "type": "text", "text": "summarize: hi" } }]
            }),
            _ => serde_json::Value::Null,
        }
    }

    fn connect() -> McpConnection {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (c_read, c_write) = tokio::io::split(client_side);
        let (s_read, s_write) = tokio::io::split(server_side);
        tokio::spawn(mock_server(s_read, s_write));
        McpConnection::new(c_read, c_write)
    }

    #[tokio::test]
    async fn handshake_reports_capabilities() {
        let conn = connect();
        let caps = conn.handshake().await.unwrap();
        assert!(caps.tools);
        assert!(caps.resources);
        assert!(caps.prompts);
    }

    #[tokio::test]
    async fn lists_and_calls_tools() {
        let conn = connect();
        let tools = conn.list_tools().await.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "search");

        let result = conn
            .call_tool("search", serde_json::json!({ "q": "rust" }))
            .await
            .unwrap();
        assert_eq!(result["content"][0]["text"], "ok");
    }

    #[tokio::test]
    async fn lists_and_reads_resources() {
        let conn = connect();
        let resources = conn.list_resources().await.unwrap();
        assert_eq!(resources.len(), 1);
        assert_eq!(resources[0].uri, "file:///readme.md");
        assert_eq!(resources[0].mime_type.as_deref(), Some("text/markdown"));

        let read = conn.read_resource("file:///readme.md").await.unwrap();
        assert_eq!(read["contents"][0]["text"], "# Hello");
    }

    #[tokio::test]
    async fn lists_and_gets_prompts() {
        let conn = connect();
        let prompts = conn.list_prompts().await.unwrap();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0].name, "summarize");
        assert_eq!(prompts[0].arguments[0].name, "text");
        assert!(prompts[0].arguments[0].required);

        let got = conn
            .get_prompt("summarize", serde_json::json!({ "text": "hi" }))
            .await
            .unwrap();
        assert_eq!(got["messages"][0]["role"], "user");
    }

    #[test]
    fn outcome_reads_result_and_error() {
        let ok = serde_json::json!({ "id": 1, "result": { "x": 1 } });
        assert_eq!(outcome_from_message(&ok).unwrap()["x"], 1);

        let err = serde_json::json!({ "id": 1, "error": { "code": -1, "message": "boom" } });
        assert!(matches!(
            outcome_from_message(&err),
            Err(McpError::Rpc { code: -1, .. })
        ));
    }

    // ── Remote transports (loopback, offline) ────────────────────────────────

    #[cfg(feature = "remote")]
    mod remote {
        use super::super::*;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        /// Read one HTTP/1.1 request off `stream`, returning its JSON body.
        async fn read_http_body(stream: &mut tokio::net::TcpStream) -> serde_json::Value {
            let mut buf = Vec::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = stream.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(header_end) = text.find("\r\n\r\n") {
                    let headers = &text[..header_end];
                    let len = headers
                        .lines()
                        .find_map(|l| {
                            let (k, v) = l.split_once(':')?;
                            (k.eq_ignore_ascii_case("content-length")).then(|| v.trim())
                        })
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    let body_start = header_end + 4;
                    if buf.len() >= body_start + len {
                        return serde_json::from_slice(&buf[body_start..body_start + len])
                            .unwrap_or(serde_json::Value::Null);
                    }
                }
            }
            serde_json::Value::Null
        }

        async fn write_http(stream: &mut tokio::net::TcpStream, ctype: &str, body: &str) {
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nMcp-Session-Id: sess-1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.flush().await.unwrap();
        }

        /// A loopback Streamable-HTTP MCP server. Replies to `tools/list` as an
        /// SSE stream and to everything else as plain JSON, so both response
        /// shapes are exercised. Handles one request per accepted connection.
        async fn spawn_http_server() -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                loop {
                    let (mut stream, _) = match listener.accept().await {
                        Ok(pair) => pair,
                        Err(_) => break,
                    };
                    let body = read_http_body(&mut stream).await;
                    let id = body.get("id").cloned().unwrap_or(serde_json::Value::Null);
                    let method = body.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let result = super::reply_for(method);
                    let rpc = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                    if method == "tools/list" {
                        let sse = format!("event: message\ndata: {rpc}\n\n");
                        write_http(&mut stream, "text/event-stream", &sse).await;
                    } else {
                        write_http(&mut stream, "application/json", &rpc.to_string()).await;
                    }
                }
            });
            format!("http://{addr}/mcp")
        }

        /// Build an HTTP connection that bypasses any ambient proxy, so the
        /// loopback server is reached directly.
        fn http_conn(url: &str) -> McpConnection {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let transport = HttpTransport::with_client(client, url, &[]).unwrap();
            McpConnection {
                transport: Box::new(transport),
            }
        }

        #[tokio::test]
        async fn http_handshake_and_json_call() {
            let url = spawn_http_server().await;
            let conn = http_conn(&url);

            let caps = conn.handshake().await.unwrap();
            assert!(caps.tools);

            let out = conn
                .call_tool("search", serde_json::json!({ "q": "rust" }))
                .await
                .unwrap();
            assert_eq!(out["content"][0]["text"], "ok");
        }

        #[tokio::test]
        async fn http_tools_list_over_sse() {
            let url = spawn_http_server().await;
            let conn = http_conn(&url);
            let tools = conn.list_tools().await.unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "search");
        }

        #[test]
        fn sse_extracts_matching_event() {
            let body = "event: message\ndata: {\"id\":1,\"result\":{\"ok\":true}}\n\n";
            let out = parse_sse(body, 1).unwrap();
            assert_eq!(out["ok"], true);
        }

        #[tokio::test]
        async fn unsupported_scheme_is_rejected() {
            let err = McpConnection::connect_url("ftp://example.com", &[])
                .await
                .unwrap_err();
            assert!(matches!(err, McpError::Transport(_)));
        }

        /// A loopback WebSocket MCP server answering JSON-RPC over text frames.
        async fn spawn_ws_server() -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                use futures::{SinkExt, StreamExt};
                use tokio_tungstenite::tungstenite::Message;
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(text) = msg {
                        let req: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                        let id = req.get("id").cloned().unwrap();
                        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let result = super::reply_for(method);
                        let resp =
                            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                        ws.send(Message::text(resp.to_string())).await.unwrap();
                    }
                }
            });
            format!("ws://{addr}")
        }

        #[tokio::test]
        async fn websocket_handshake_lists_and_calls() {
            let url = spawn_ws_server().await;
            let conn = McpConnection::connect_url(&url, &[]).await.unwrap();

            let caps = conn.handshake().await.unwrap();
            assert!(caps.tools);

            let tools = conn.list_tools().await.unwrap();
            assert_eq!(tools[0].name, "search");

            let out = conn
                .call_tool("search", serde_json::json!({}))
                .await
                .unwrap();
            assert_eq!(out["content"][0]["text"], "ok");
        }
    }
}
