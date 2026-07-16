//! # dadhichi-acp
//!
//! An [Agent Client Protocol](https://agentclientprotocol.com)-style stdio
//! server that lets an editor drive Dadhichi as a headless agent: create and
//! load sessions, send prompts, and receive streamed updates — all as
//! newline-delimited JSON-RPC over stdin/stdout.
//!
//! The protocol and session plumbing live here; the actual agent run is
//! supplied by a [`PromptHandler`], so this crate stays light (it does not
//! depend on the model or tool stack) and is testable with a mock handler. The
//! binary provides the real handler that drives the ReAct agent.
//!
//! ## Methods
//!
//! | Method | Params | Result |
//! |---|---|---|
//! | `initialize` | — | agent + protocol capabilities |
//! | `session/new` | `{ cwd }` | `{ sessionId }` |
//! | `session/load` | `{ sessionId }` | `{}` (errors if unknown) |
//! | `session/list` | `{ cwd }` | `{ sessions: [...] }` |
//! | `session/prompt` | `{ sessionId, prompt }` | streams `session/update`, then `{ result, status }` |

pub mod jsonrpc;

use async_trait::async_trait;
use dadhichi_session::{Session, SessionStore};
use jsonrpc::{Notification, Request, Response, codes};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// The result of running one prompt turn.
#[derive(Debug, Clone)]
pub struct PromptResult {
    /// The agent's final message.
    pub text: String,
    /// A short status string (e.g. `Completed`).
    pub status: String,
}

/// Collects `session/update` payloads emitted while a prompt runs; the server
/// writes them to the client before the final response.
#[derive(Debug, Default)]
pub struct UpdateSink {
    items: Vec<Value>,
}

impl UpdateSink {
    /// Emit a raw update payload for `session_id`.
    pub fn update(&mut self, session_id: &str, payload: Value) {
        self.items
            .push(json!({ "sessionId": session_id, "update": payload }));
    }

    /// Emit an agent-message text chunk.
    pub fn text(&mut self, session_id: &str, text: &str) {
        self.update(session_id, json!({ "type": "agent_message", "text": text }));
    }

    /// Emit a tool-call notice.
    pub fn tool_call(&mut self, session_id: &str, tool: &str) {
        self.update(session_id, json!({ "type": "tool_call", "tool": tool }));
    }
}

/// Runs a prompt for a session, streaming updates through the sink and
/// returning the final result. Implemented by the binary to drive the agent.
#[async_trait]
pub trait PromptHandler: Send + Sync {
    /// Handle one prompt turn.
    async fn prompt(
        &self,
        session: &Session,
        prompt: &str,
        sink: &mut UpdateSink,
    ) -> Result<PromptResult, String>;
}

/// The ACP server: a session store plus a prompt handler.
pub struct AcpServer {
    store: SessionStore,
    handler: Arc<dyn PromptHandler>,
}

impl std::fmt::Debug for AcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpServer").field("store", &self.store).finish()
    }
}

impl AcpServer {
    /// Build a server over `store`, driving prompts through `handler`.
    pub fn new(store: SessionStore, handler: Arc<dyn PromptHandler>) -> Self {
        Self { store, handler }
    }

    /// Serve requests from `reader`, writing responses/notifications to
    /// `writer`, until the input ends. Malformed lines are skipped.
    pub async fn serve<R, W>(&self, reader: R, mut writer: W) -> std::io::Result<()>
    where
        R: AsyncBufRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut lines = reader.lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(req) = serde_json::from_str::<Request>(&line) else {
                continue; // ignore unparseable input
            };
            let is_notification = req.is_notification();
            let (updates, response) = self.dispatch(req).await;
            for update in updates {
                write_json(&mut writer, &update).await?;
            }
            if !is_notification {
                write_json(&mut writer, &response).await?;
            }
        }
        Ok(())
    }

    async fn dispatch(&self, req: Request) -> (Vec<Notification>, Response) {
        let id = req.id.clone().unwrap_or(Value::Null);
        match req.method.as_str() {
            "initialize" => (vec![], Response::ok(id, initialize_result())),

            "session/new" => match self.store.create(&param_path(&req.params, "cwd")) {
                Ok(s) => (vec![], Response::ok(id, json!({ "sessionId": s.id() }))),
                Err(e) => (
                    vec![],
                    Response::err(id, codes::INTERNAL_ERROR, e.to_string()),
                ),
            },

            "session/load" => match param_str(&req.params, "sessionId") {
                Some(sid) => match self.store.resume(&sid) {
                    Ok(_) => (vec![], Response::ok(id, json!({}))),
                    Err(e) => (
                        vec![],
                        Response::err(id, codes::INTERNAL_ERROR, e.to_string()),
                    ),
                },
                None => (
                    vec![],
                    Response::err(id, codes::INVALID_PARAMS, "missing sessionId"),
                ),
            },

            "session/list" => {
                let sessions: Vec<Value> = self
                    .store
                    .list(&param_path(&req.params, "cwd"))
                    .iter()
                    .map(|s| {
                        json!({
                            "sessionId": s.id,
                            "title": s.title,
                            "updatedAt": s.updated_at,
                            "numEvents": s.num_events,
                        })
                    })
                    .collect();
                (vec![], Response::ok(id, json!({ "sessions": sessions })))
            }

            "session/prompt" => self.handle_prompt(id, &req.params).await,

            other => (
                vec![],
                Response::err(
                    id,
                    codes::METHOD_NOT_FOUND,
                    format!("unknown method: {other}"),
                ),
            ),
        }
    }

    async fn handle_prompt(&self, id: Value, params: &Value) -> (Vec<Notification>, Response) {
        let (Some(sid), Some(prompt)) = (
            param_str(params, "sessionId"),
            param_str(params, "prompt").or_else(|| param_str(params, "text")),
        ) else {
            return (
                vec![],
                Response::err(id, codes::INVALID_PARAMS, "missing sessionId or prompt"),
            );
        };

        let session = match self.store.resume(&sid) {
            Ok(s) => s,
            Err(e) => {
                return (
                    vec![],
                    Response::err(id, codes::INTERNAL_ERROR, e.to_string()),
                );
            }
        };
        let _ = session.append(&json!({ "role": "user", "text": prompt }));

        let mut sink = UpdateSink::default();
        match self.handler.prompt(&session, &prompt, &mut sink).await {
            Ok(result) => {
                let _ = session.append(&json!({
                    "role": "assistant",
                    "text": result.text,
                    "status": result.status,
                }));
                let updates = sink
                    .items
                    .into_iter()
                    .map(|p| Notification::new("session/update", p))
                    .collect();
                (
                    updates,
                    Response::ok(
                        id,
                        json!({ "result": result.text, "status": result.status }),
                    ),
                )
            }
            Err(e) => (vec![], Response::err(id, codes::INTERNAL_ERROR, e)),
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": "0.1",
        "agent": { "name": "dadhichi", "version": env!("CARGO_PKG_VERSION") },
        "capabilities": {
            "sessions": { "new": true, "load": true, "list": true },
            "prompt": { "streaming": true }
        }
    })
}

fn param_str(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn param_path(params: &Value, key: &str) -> PathBuf {
    params
        .get(key)
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()))
}

async fn write_json<W, T>(writer: &mut W, value: &T) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: serde::Serialize,
{
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    writer.write_all(&line).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EchoHandler;

    #[async_trait]
    impl PromptHandler for EchoHandler {
        async fn prompt(
            &self,
            session: &Session,
            prompt: &str,
            sink: &mut UpdateSink,
        ) -> Result<PromptResult, String> {
            // Stream one update, then return a result.
            sink.text(session.id(), &format!("working on: {prompt}"));
            Ok(PromptResult {
                text: format!("echo: {prompt}"),
                status: "Completed".into(),
            })
        }
    }

    fn server() -> (tempfile::TempDir, AcpServer) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(dir.path().join("sessions"));
        (dir, AcpServer::new(store, Arc::new(EchoHandler)))
    }

    /// Drive the server with a script of request lines; return the response
    /// lines it wrote, parsed as JSON.
    async fn run(server: &AcpServer, requests: &[Value]) -> Vec<Value> {
        let mut input = String::new();
        for r in requests {
            input.push_str(&serde_json::to_string(r).unwrap());
            input.push('\n');
        }
        let bytes = input.into_bytes();
        let reader = tokio::io::BufReader::new(bytes.as_slice());
        let mut output: Vec<u8> = Vec::new();
        server.serve(reader, &mut output).await.unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn initialize_reports_capabilities() {
        let (_d, srv) = server();
        let out = run(&srv, &[json!({"id":1,"method":"initialize"})]).await;
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["result"]["agent"]["name"], "dadhichi");
        assert_eq!(out[0]["result"]["capabilities"]["prompt"]["streaming"], true);
    }

    #[tokio::test]
    async fn new_then_prompt_streams_update_then_result() {
        let (dir, srv) = server();
        let cwd = dir.path().to_string_lossy().to_string();

        // Create a session, capture its id.
        let out = run(&srv, &[json!({"id":1,"method":"session/new","params":{"cwd":cwd}})]).await;
        let sid = out[0]["result"]["sessionId"].as_str().unwrap().to_string();

        // Prompt it: expect a session/update notification, then the response.
        let out = run(
            &srv,
            &[json!({"id":2,"method":"session/prompt","params":{"sessionId":sid,"prompt":"hi"}})],
        )
        .await;
        assert_eq!(out.len(), 2);
        // First line: the streamed update (a notification, no id).
        assert_eq!(out[0]["method"], "session/update");
        assert_eq!(out[0]["params"]["update"]["text"], "working on: hi");
        assert!(out[0].get("id").is_none());
        // Second line: the response.
        assert_eq!(out[1]["id"], 2);
        assert_eq!(out[1]["result"]["result"], "echo: hi");
        assert_eq!(out[1]["result"]["status"], "Completed");
    }

    #[tokio::test]
    async fn load_unknown_session_errors() {
        let (_d, srv) = server();
        let out = run(
            &srv,
            &[json!({"id":1,"method":"session/load","params":{"sessionId":"nope"}})],
        )
        .await;
        assert_eq!(out[0]["error"]["code"], codes::INTERNAL_ERROR);
    }

    #[tokio::test]
    async fn unknown_method_is_method_not_found() {
        let (_d, srv) = server();
        let out = run(&srv, &[json!({"id":7,"method":"does/notExist"})]).await;
        assert_eq!(out[0]["error"]["code"], codes::METHOD_NOT_FOUND);
        assert_eq!(out[0]["id"], 7);
    }

    #[tokio::test]
    async fn notification_gets_no_response() {
        let (_d, srv) = server();
        // A request without an id is a notification; unknown method ⇒ no reply.
        let out = run(&srv, &[json!({"method":"some/notify","params":{}})]).await;
        assert!(out.is_empty());
    }
}
