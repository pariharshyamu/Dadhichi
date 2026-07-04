//! Minimal JSON-RPC 2.0 envelope and MCP client/server traits.
//!
//! Dadhichi is *MCP-native*: it is both a client (consuming external MCP
//! servers such as GitHub or Docker) and a server (exposing its own workspace
//! tools to other agents). These types model the wire envelope; a concrete
//! transport (stdio, WebSocket, HTTP) plugs in behind [`McpClient`].

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A JSON-RPC 2.0 request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Correlates a response to its request.
    pub id: u64,
    /// The method name, e.g. `"tools/call"`.
    pub method: String,
    /// Method parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

impl RpcRequest {
    /// Build a request with the given id, method, and params.
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Self {
            jsonrpc: "2.0".into(),
            id,
            method: method.into(),
            params: Some(params),
        }
    }
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcErrorObject {
    /// Numeric error code.
    pub code: i64,
    /// Human-readable message.
    pub message: String,
}

/// A JSON-RPC 2.0 response (result *or* error).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Correlates back to the request id.
    pub id: u64,
    /// The successful result, if the call succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// The error, if the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcErrorObject>,
}

/// Errors arising from MCP transport or protocol handling.
#[derive(Debug, Error)]
pub enum McpError {
    /// The transport failed.
    #[error("transport error: {0}")]
    Transport(String),
    /// The peer returned a JSON-RPC error.
    #[error("rpc error {code}: {message}")]
    Rpc {
        /// JSON-RPC error code.
        code: i64,
        /// JSON-RPC error message.
        message: String,
    },
    /// The response could not be decoded.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// What a peer advertised during capability negotiation (`initialize`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    /// The server exposes callable tools.
    pub tools: bool,
    /// The server exposes readable resources.
    pub resources: bool,
    /// The server exposes prompt templates.
    pub prompts: bool,
}

/// A client that speaks MCP to a remote server over some transport.
#[async_trait]
pub trait McpClient: Send + Sync {
    /// Perform the `initialize` handshake and return negotiated capabilities.
    async fn initialize(&self) -> Result<ServerCapabilities, McpError>;

    /// Send a request and await its response.
    async fn call(&self, request: RpcRequest) -> Result<RpcResponse, McpError>;
}
