//! An async Debug Adapter Protocol client.
//!
//! Structurally a sibling of the LSP client: a background reader decodes frames
//! and correlates each `response` to its `request` by `request_seq`, answers the
//! adapter's reverse requests, and republishes adapter `event`s (`stopped`,
//! `terminated`, …) onto the kernel bus as `dap.<event>`. It drives a real debug
//! adapter over stdio and a mock over an in-memory pipe with identical logic.

use crate::codec::{DapDecoder, encode};
use dadhichi_core::{Event, EventBus};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;

/// Errors from the DAP client.
#[derive(Debug, Error)]
pub enum DapError {
    /// The transport closed before a response arrived.
    #[error("dap transport closed")]
    Closed,
    /// An I/O error on the transport.
    #[error("dap io error: {0}")]
    Io(String),
    /// The adapter reported a failed request.
    #[error("dap adapter error: {0}")]
    Adapter(String),
}

type SharedWriter = Arc<Mutex<Box<dyn AsyncWrite + Unpin + Send>>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<serde_json::Value, DapError>>>>>;

/// A Debug Adapter Protocol client.
pub struct DapClient {
    writer: SharedWriter,
    pending: Pending,
    seq: AtomicU64,
    reader_task: JoinHandle<()>,
    _child: Option<tokio::process::Child>,
}

impl std::fmt::Debug for DapClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DapClient").finish_non_exhaustive()
    }
}

impl Drop for DapClient {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

impl DapClient {
    /// Build a client over an arbitrary transport, spawning the reader loop.
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
            seq: AtomicU64::new(1),
            reader_task,
            _child: None,
        }
    }

    /// Launch a debug adapter subprocess and speak DAP over stdio.
    pub async fn connect_stdio(
        command: &str,
        args: &[&str],
        bus: Option<EventBus>,
    ) -> Result<Self, DapError> {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| DapError::Io(e.to_string()))?;
        let stdout = child.stdout.take().ok_or(DapError::Closed)?;
        let stdin = child.stdin.take().ok_or(DapError::Closed)?;
        let mut client = Self::new(stdout, stdin, bus);
        client._child = Some(child);
        Ok(client)
    }

    /// Send a request and await its correlated response body.
    pub async fn request(
        &self,
        command: &str,
        arguments: serde_json::Value,
    ) -> Result<serde_json::Value, DapError> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(seq, tx);

        let message = serde_json::json!({
            "seq": seq, "type": "request", "command": command, "arguments": arguments
        });
        if let Err(e) = send(&self.writer, &message).await {
            self.pending.lock().await.remove(&seq);
            return Err(e);
        }
        rx.await.map_err(|_| DapError::Closed)?
    }

    /// Perform the `initialize` handshake, returning adapter capabilities.
    pub async fn initialize(&self, adapter_id: &str) -> Result<serde_json::Value, DapError> {
        self.request(
            "initialize",
            serde_json::json!({
                "clientID": "dadhichi",
                "adapterID": adapter_id,
                "linesStartAt1": true,
                "columnsStartAt1": true,
                "pathFormat": "path"
            }),
        )
        .await
    }

    /// Launch the debuggee with an adapter-specific `config`.
    pub async fn launch(&self, config: serde_json::Value) -> Result<serde_json::Value, DapError> {
        self.request("launch", config).await
    }

    /// Set breakpoints for `path` at the given 1-based `lines`.
    pub async fn set_breakpoints(
        &self,
        path: &str,
        lines: &[u32],
    ) -> Result<serde_json::Value, DapError> {
        let breakpoints: Vec<_> = lines
            .iter()
            .map(|l| serde_json::json!({ "line": l }))
            .collect();
        self.request(
            "setBreakpoints",
            serde_json::json!({ "source": { "path": path }, "breakpoints": breakpoints }),
        )
        .await
    }

    /// Signal that configuration (breakpoints, etc.) is complete.
    pub async fn configuration_done(&self) -> Result<serde_json::Value, DapError> {
        self.request("configurationDone", serde_json::json!({}))
            .await
    }

    /// List the debuggee's threads.
    pub async fn threads(&self) -> Result<serde_json::Value, DapError> {
        self.request("threads", serde_json::json!({})).await
    }

    /// Resume execution of `thread_id`.
    pub async fn continue_(&self, thread_id: i64) -> Result<serde_json::Value, DapError> {
        self.request("continue", serde_json::json!({ "threadId": thread_id }))
            .await
    }

    /// Disconnect from the adapter.
    pub async fn disconnect(&self) -> Result<serde_json::Value, DapError> {
        self.request("disconnect", serde_json::json!({})).await
    }
}

async fn send(writer: &SharedWriter, message: &serde_json::Value) -> Result<(), DapError> {
    let bytes = encode(message);
    let mut guard = writer.lock().await;
    guard
        .write_all(&bytes)
        .await
        .map_err(|e| DapError::Io(e.to_string()))?;
    guard
        .flush()
        .await
        .map_err(|e| DapError::Io(e.to_string()))?;
    Ok(())
}

async fn read_loop<R: AsyncRead + Unpin>(
    mut reader: R,
    pending: Pending,
    writer: SharedWriter,
    bus: Option<EventBus>,
) {
    let mut decoder = DapDecoder::new();
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
    let mut map = pending.lock().await;
    for (_, tx) in map.drain() {
        let _ = tx.send(Err(DapError::Closed));
    }
}

async fn dispatch(
    message: serde_json::Value,
    pending: &Pending,
    writer: &SharedWriter,
    bus: &Option<EventBus>,
) {
    match message.get("type").and_then(|t| t.as_str()) {
        Some("response") => {
            let Some(request_seq) = message.get("request_seq").and_then(|s| s.as_u64()) else {
                return;
            };
            if let Some(tx) = pending.lock().await.remove(&request_seq) {
                let outcome = if message
                    .get("success")
                    .and_then(|s| s.as_bool())
                    .unwrap_or(false)
                {
                    Ok(message
                        .get("body")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null))
                } else {
                    Err(DapError::Adapter(
                        message
                            .get("message")
                            .and_then(|m| m.as_str())
                            .unwrap_or("failed")
                            .to_string(),
                    ))
                };
                let _ = tx.send(outcome);
            }
        }
        Some("event") => {
            if let Some(event) = message.get("event").and_then(|e| e.as_str())
                && let Some(bus) = bus
            {
                let body = message
                    .get("body")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null);
                bus.publish(Event::new(format!("dap.{event}"), body));
            }
        }
        Some("request") => {
            // A reverse request (e.g. runInTerminal). Acknowledge so the adapter
            // proceeds; a full client would honour it.
            if let Some(seq) = message.get("seq").and_then(|s| s.as_u64()) {
                let command = message
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                let reply = serde_json::json!({
                    "seq": 0, "type": "response", "request_seq": seq,
                    "success": true, "command": command
                });
                let _ = send(writer, &reply).await;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{ReadHalf, WriteHalf};

    async fn mock_adapter(
        reader: ReadHalf<tokio::io::DuplexStream>,
        writer: WriteHalf<tokio::io::DuplexStream>,
    ) {
        let writer = Arc::new(Mutex::new(
            Box::new(writer) as Box<dyn AsyncWrite + Unpin + Send>
        ));
        let mut decoder = DapDecoder::new();
        let mut reader = reader;
        let mut buf = vec![0u8; 4096];
        loop {
            let n = match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for msg in decoder.feed(&buf[..n]) {
                let seq = msg.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
                let command = msg.get("command").and_then(|c| c.as_str()).unwrap_or("");
                let body = match command {
                    "initialize" => serde_json::json!({ "supportsConfigurationDoneRequest": true }),
                    "setBreakpoints" => {
                        serde_json::json!({ "breakpoints": [{ "verified": true, "line": 10 }] })
                    }
                    "threads" => serde_json::json!({ "threads": [{ "id": 1, "name": "main" }] }),
                    _ => serde_json::Value::Null,
                };
                let resp = serde_json::json!({
                    "seq": 0, "type": "response", "request_seq": seq,
                    "success": true, "command": command, "body": body
                });
                send(&writer, &resp).await.unwrap();

                // After initialize, push a `stopped` event.
                if command == "initialize" {
                    let event = serde_json::json!({
                        "seq": 0, "type": "event", "event": "stopped",
                        "body": { "reason": "breakpoint", "threadId": 1 }
                    });
                    send(&writer, &event).await.unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn drives_a_mock_adapter() {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (c_read, c_write) = tokio::io::split(client_side);
        let (s_read, s_write) = tokio::io::split(server_side);
        tokio::spawn(mock_adapter(s_read, s_write));

        let bus = EventBus::new();
        let mut stopped = bus.subscribe_topic("dap.stopped");
        let client = DapClient::new(c_read, c_write, Some(bus));

        let caps = client.initialize("mock").await.unwrap();
        assert_eq!(caps["supportsConfigurationDoneRequest"], true);

        let bps = client.set_breakpoints("src/main.rs", &[10]).await.unwrap();
        assert_eq!(bps["breakpoints"][0]["verified"], true);

        let threads = client.threads().await.unwrap();
        assert_eq!(threads["threads"][0]["name"], "main");

        // The adapter's `stopped` event reached the bus.
        let event = stopped.recv().await.unwrap();
        assert_eq!(event.payload["reason"], "breakpoint");
    }

    #[tokio::test]
    async fn pending_requests_fail_on_close() {
        let (client_side, server_side) = tokio::io::duplex(1024);
        let (c_read, c_write) = tokio::io::split(client_side);
        drop(server_side);
        let client = DapClient::new(c_read, c_write, None);
        assert!(matches!(
            client.threads().await.unwrap_err(),
            DapError::Closed | DapError::Io(_)
        ));
    }
}
