//! An async LSP client with request/response correlation.
//!
//! The client is generic over any [`AsyncRead`]/[`AsyncWrite`] pair, so it drives
//! a real language server over stdio ([`LspClient::connect_stdio`]) in production
//! and an in-memory pipe in tests — the logic under test is identical. A
//! background reader task decodes incoming frames, matches responses to their
//! pending requests by id, answers server-initiated requests, and forwards
//! `publishDiagnostics` notifications onto the kernel event bus as
//! `lsp.diagnostics` events.

use crate::codec::{LspDecoder, encode};
use crate::protocol::{
    CompletionItem, Location, Position, did_change_params, did_open_params, initialize_params,
    parse_completion, parse_diagnostics, parse_hover, parse_locations, references_params,
    text_document_position,
};
use dadhichi_core::{Event, EventBus};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

/// Errors from the LSP client.
#[derive(Debug, Error)]
pub enum LspError {
    /// The transport closed before a response arrived.
    #[error("lsp transport closed")]
    Closed,
    /// An I/O error on the transport.
    #[error("lsp io error: {0}")]
    Io(String),
    /// The server returned a JSON-RPC error.
    #[error("lsp server error {code}: {message}")]
    Server {
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
    /// No language server is registered for this file type.
    #[error("no language server for {0}")]
    Unsupported(String),
    /// The registered server couldn't be started (usually: not installed).
    #[error("language server '{command}' unavailable: {reason}")]
    Unavailable {
        /// The server executable that failed to launch.
        command: String,
        /// Why (spawn error, failed handshake).
        reason: String,
    },
}

/// A boxed, shareable async writer for the transport's outbound half.
type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;
/// Outstanding requests awaiting a response, keyed by request id.
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, LspError>>>>>;

/// A Language Server Protocol client.
pub struct LspClient {
    writer: SharedWriter,
    pending: Pending,
    next_id: AtomicU64,
    reader_task: JoinHandle<()>,
    _child: Option<tokio::process::Child>,
}

impl std::fmt::Debug for LspClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspClient")
            .field("next_id", &self.next_id.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Stop the background reader when the client goes away.
        self.reader_task.abort();
    }
}

impl LspClient {
    /// Build a client over an arbitrary transport, spawning the reader loop.
    ///
    /// When `bus` is provided, diagnostics pushed by the server are published as
    /// `lsp.diagnostics` events.
    pub fn new<R, W>(reader: R, writer: W, bus: Option<EventBus>) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let reader_task = tokio::spawn(read_loop(reader, pending.clone(), writer.clone(), bus));
        Self {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            reader_task,
            _child: None,
        }
    }

    /// Launch a language server subprocess and speak LSP to it over stdio.
    pub async fn connect_stdio(
        command: &str,
        args: &[&str],
        bus: Option<EventBus>,
    ) -> Result<Self, LspError> {
        use std::process::Stdio;
        let spawn_direct = || {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            cmd.spawn()
        };
        let mut child = match spawn_direct() {
            Ok(child) => child,
            // npm-installed servers (typescript-language-server, pyright,
            // ngserver, ...) are `.cmd` shims on Windows, which CreateProcess
            // can't launch by bare name — let cmd.exe resolve PATHEXT.
            Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::NotFound => {
                let mut cmd = tokio::process::Command::new("cmd");
                cmd.arg("/C")
                    .arg(command)
                    .args(args)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null());
                cmd.spawn().map_err(|e| LspError::Io(e.to_string()))?
            }
            Err(err) => return Err(LspError::Io(err.to_string())),
        };

        let stdout = child.stdout.take().ok_or(LspError::Closed)?;
        let stdin = child.stdin.take().ok_or(LspError::Closed)?;

        let mut client = Self::new(stdout, stdin, bus);
        client._child = Some(child);
        Ok(client)
    }

    /// Send a request and await the correlated response.
    async fn request(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, LspError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        let message =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = send(&self.writer, &message).await {
            self.pending.lock().await.remove(&id);
            return Err(e);
        }
        rx.await.map_err(|_| LspError::Closed)?
    }

    /// Send a notification (no response expected).
    async fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), LspError> {
        let message = serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params });
        send(&self.writer, &message).await
    }

    /// Perform the `initialize` handshake and confirm with `initialized`.
    /// Returns the server's advertised capabilities.
    pub async fn initialize(&self, root_uri: &str) -> Result<serde_json::Value, LspError> {
        let capabilities = self
            .request("initialize", initialize_params(root_uri))
            .await?;
        self.notify("initialized", serde_json::json!({})).await?;
        Ok(capabilities)
    }

    /// Notify the server a document was opened.
    pub async fn did_open(&self, uri: &str, language_id: &str, text: &str) -> Result<(), LspError> {
        self.notify(
            "textDocument/didOpen",
            did_open_params(uri, language_id, text),
        )
        .await
    }

    /// Notify the server a document changed, replacing its whole text (full
    /// sync). `version` must increase monotonically per document.
    pub async fn did_change(&self, uri: &str, version: i64, text: &str) -> Result<(), LspError> {
        self.notify(
            "textDocument/didChange",
            did_change_params(uri, version, text),
        )
        .await
    }

    /// Request completion suggestions at `position`.
    pub async fn completion(
        &self,
        uri: &str,
        position: Position,
    ) -> Result<Vec<CompletionItem>, LspError> {
        let result = self
            .request(
                "textDocument/completion",
                text_document_position(uri, position),
            )
            .await?;
        Ok(parse_completion(&result))
    }

    /// Request hover text at `position`, returning the plain-text contents.
    pub async fn hover(&self, uri: &str, position: Position) -> Result<Option<String>, LspError> {
        let result = self
            .request("textDocument/hover", text_document_position(uri, position))
            .await?;
        Ok(parse_hover(&result))
    }

    /// Resolve the definition(s) of the symbol at `position`.
    pub async fn goto_definition(
        &self,
        uri: &str,
        position: Position,
    ) -> Result<Vec<Location>, LspError> {
        let result = self
            .request(
                "textDocument/definition",
                text_document_position(uri, position),
            )
            .await?;
        Ok(parse_locations(&result))
    }

    /// Find references to the symbol at `position`.
    pub async fn references(
        &self,
        uri: &str,
        position: Position,
    ) -> Result<Vec<Location>, LspError> {
        let result = self
            .request("textDocument/references", references_params(uri, position))
            .await?;
        Ok(parse_locations(&result))
    }

    /// Ask the server to shut down and exit.
    pub async fn shutdown(&self) -> Result<(), LspError> {
        self.request("shutdown", serde_json::Value::Null).await?;
        self.notify("exit", serde_json::Value::Null).await
    }
}

/// Frame and write a message to the transport.
async fn send(writer: &SharedWriter, message: &serde_json::Value) -> Result<(), LspError> {
    let bytes = encode(message);
    let mut guard = writer.lock().await;
    guard
        .write_all(&bytes)
        .await
        .map_err(|e| LspError::Io(e.to_string()))?;
    guard
        .flush()
        .await
        .map_err(|e| LspError::Io(e.to_string()))?;
    Ok(())
}

/// The background loop: decode frames and dispatch them until the transport
/// closes, then fail every outstanding request.
async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    pending: Pending,
    writer: SharedWriter,
    bus: Option<EventBus>,
) {
    let mut decoder = LspDecoder::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                for message in decoder.feed(&buf[..n]) {
                    dispatch(message, &pending, &writer, &bus).await;
                }
            }
        }
    }
    // Transport closed: unblock anyone still waiting.
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(LspError::Closed));
    }
}

/// Route one decoded message: response, server request, or notification.
async fn dispatch(
    message: serde_json::Value,
    pending: &Pending,
    writer: &SharedWriter,
    bus: &Option<EventBus>,
) {
    let id = message.get("id").and_then(|v| v.as_u64());
    let method = message.get("method").and_then(|m| m.as_str());

    // Response to one of our requests: has an id and result/error, no method.
    if let Some(id) = id
        && method.is_none()
    {
        if let Some(tx) = pending.lock().await.remove(&id) {
            let outcome = match message.get("error") {
                Some(err) => Err(LspError::Server {
                    code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                    message: err
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                }),
                None => Ok(message
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)),
            };
            let _ = tx.send(outcome);
        }
        return;
    }

    // Server-initiated request: has both method and id. Reply with a null result
    // so servers that gate on capabilities (e.g. workDoneProgress/create) proceed.
    if let (Some(_method), Some(id)) = (method, id) {
        let reply =
            serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": serde_json::Value::Null });
        let _ = send(writer, &reply).await;
        return;
    }

    // Notification: method, no id.
    if let Some(method) = method
        && method == "textDocument/publishDiagnostics"
        && let Some(params) = message.get("params")
        && let Some(diagnostics) = parse_diagnostics(params)
        && let Some(bus) = bus
    {
        let payload = serde_json::to_value(&diagnostics).unwrap_or_default();
        bus.publish(Event::new("lsp.diagnostics", payload));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{ReadHalf, WriteHalf};

    /// A tiny in-memory LSP server that answers the requests the client makes
    /// and pushes a diagnostic after initialization.
    async fn mock_server(
        reader: ReadHalf<tokio::io::DuplexStream>,
        writer: WriteHalf<tokio::io::DuplexStream>,
    ) {
        let writer = Arc::new(Mutex::new(
            Box::new(writer) as Box<dyn AsyncWrite + Unpin + Send>
        ));
        let mut decoder = LspDecoder::new();
        let mut reader = reader;
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for msg in decoder.feed(&buf[..n]) {
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let id = msg.get("id").and_then(|v| v.as_u64());
                match (method, id) {
                    ("initialize", Some(id)) => {
                        respond(
                            &writer,
                            id,
                            serde_json::json!({ "capabilities": { "hoverProvider": true } }),
                        )
                        .await;
                        // Push a diagnostic notification.
                        let note = serde_json::json!({
                            "jsonrpc": "2.0",
                            "method": "textDocument/publishDiagnostics",
                            "params": {
                                "uri": "file:///a.rs",
                                "diagnostics": [{
                                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 3 } },
                                    "severity": 2,
                                    "message": "unused import"
                                }]
                            }
                        });
                        let _ = send(&writer, &note).await;
                    }
                    ("textDocument/hover", Some(id)) => {
                        respond(&writer, id, serde_json::json!({ "contents": { "kind": "markdown", "value": "fn main()" } })).await;
                    }
                    ("textDocument/completion", Some(id)) => {
                        respond(
                            &writer,
                            id,
                            serde_json::json!({
                                "isIncomplete": false,
                                "items": [
                                    { "label": "main", "kind": 3, "detail": "fn main()" },
                                    { "label": "map", "kind": 2, "insertText": "map()" }
                                ]
                            }),
                        )
                        .await;
                    }
                    ("textDocument/definition", Some(id)) => {
                        respond(&writer, id, serde_json::json!([{
                            "uri": "file:///a.rs",
                            "range": { "start": { "line": 10, "character": 0 }, "end": { "line": 10, "character": 4 } }
                        }])).await;
                    }
                    ("shutdown", Some(id)) => respond(&writer, id, serde_json::Value::Null).await,
                    (_, Some(id)) => respond(&writer, id, serde_json::Value::Null).await,
                    (_, None) => {} // notifications: ignore
                }
            }
        }
    }

    async fn respond(writer: &SharedWriter, id: u64, result: serde_json::Value) {
        let msg = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let _ = send(writer, &msg).await;
    }

    #[tokio::test]
    async fn drives_a_mock_server_end_to_end() {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (c_read, c_write) = tokio::io::split(client_side);
        let (s_read, s_write) = tokio::io::split(server_side);
        tokio::spawn(mock_server(s_read, s_write));

        let bus = EventBus::new();
        let mut diag = bus.subscribe_topic("lsp.diagnostics");
        let client = LspClient::new(c_read, c_write, Some(bus));

        let caps = client.initialize("file:///proj").await.unwrap();
        assert_eq!(caps["capabilities"]["hoverProvider"], true);

        client
            .did_open("file:///a.rs", "rust", "fn main() {}")
            .await
            .unwrap();

        let hover = client
            .hover("file:///a.rs", Position::new(0, 3))
            .await
            .unwrap();
        assert_eq!(hover.as_deref(), Some("fn main()"));

        let defs = client
            .goto_definition("file:///a.rs", Position::new(0, 3))
            .await
            .unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].range.start.line, 10);

        // Sync an edit (a notification — fire and forget), then complete.
        client
            .did_change("file:///a.rs", 2, "fn main() { m }")
            .await
            .unwrap();
        let items = client
            .completion("file:///a.rs", Position::new(0, 13))
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].label, "main");
        assert_eq!(items[0].kind, "fn");
        assert_eq!(items[1].insert, "map()");

        // The diagnostic pushed during initialize reaches the event bus.
        let event = diag.recv().await.unwrap();
        assert_eq!(event.payload["uri"], "file:///a.rs");
        assert_eq!(event.payload["diagnostics"][0]["severity"], "warning");
    }

    #[tokio::test]
    async fn pending_requests_fail_when_transport_closes() {
        let (client_side, server_side) = tokio::io::duplex(1024);
        let (c_read, c_write) = tokio::io::split(client_side);
        // Tear the whole transport down. A request must then fail promptly rather
        // than hang — whether the write breaks (`Io`) or the reader hits EOF and
        // drains pending requests (`Closed`), both are correct terminal states.
        drop(server_side);

        let client = LspClient::new(c_read, c_write, None);
        let err = client
            .hover("file:///a.rs", Position::new(0, 0))
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::Closed | LspError::Io(_)));
    }
}
