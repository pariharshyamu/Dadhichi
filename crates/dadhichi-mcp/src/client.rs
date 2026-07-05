//! A live MCP client over a line-delimited JSON-RPC transport.
//!
//! The MCP stdio transport frames each JSON-RPC message as one line of UTF-8.
//! [`McpConnection`] is generic over any [`AsyncRead`]/[`AsyncWrite`] pair — it
//! drives a real server subprocess via [`connect_stdio`](McpConnection::connect_stdio)
//! in production and an in-memory pipe in tests, with identical logic. A
//! background reader correlates responses to requests by id.

use crate::protocol::{McpClient, McpError, RpcRequest, RpcResponse, ServerCapabilities};
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

/// A connection to an MCP server.
pub struct McpConnection {
    writer: SharedWriter,
    pending: Pending,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
    _child: Option<tokio::process::Child>,
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection").finish_non_exhaustive()
    }
}

impl Drop for McpConnection {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

impl McpConnection {
    /// Build a connection over an arbitrary transport, spawning the reader loop.
    pub fn new<R, W>(reader: R, writer: W) -> Self
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
            _child: None,
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
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .envs(env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| McpError::Transport(e.to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("no stdout".into()))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("no stdin".into()))?;
        let mut conn = Self::new(stdout, stdin);
        conn._child = Some(child);
        Ok(conn)
    }

    /// Send a request and await its correlated response result.
    pub async fn request(
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
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(McpError::Transport("connection closed".into())));
    }
}

async fn dispatch(message: serde_json::Value, pending: &Pending) {
    let Some(id) = message.get("id").and_then(|v| v.as_u64()) else {
        return; // a notification — nothing awaits it
    };
    if let Some(tx) = pending.lock().await.remove(&id) {
        let outcome = match message.get("error") {
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
        };
        let _ = tx.send(outcome);
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
            let result = match method {
                "initialize" => serde_json::json!({ "capabilities": { "tools": {} } }),
                "tools/list" => serde_json::json!({
                    "tools": [{ "name": "search", "description": "web search", "inputSchema": { "type": "object" } }]
                }),
                "tools/call" => {
                    serde_json::json!({ "content": [{ "type": "text", "text": "ok" }] })
                }
                _ => serde_json::Value::Null,
            };
            let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            let _ = writer.write_all(&bytes).await;
            let _ = writer.flush().await;
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
        assert!(!caps.resources);
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
}
