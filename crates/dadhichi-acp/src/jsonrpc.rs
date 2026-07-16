//! Minimal JSON-RPC 2.0 message types for the ACP stdio transport.
//!
//! Messages are newline-delimited JSON objects (one per line). Requests carry
//! an `id`; notifications (like `session/update`) do not.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// An incoming request or notification.
#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    /// Present on requests, absent on notifications.
    #[serde(default)]
    pub id: Option<Value>,
    /// The method name, e.g. `session/new`.
    pub method: String,
    /// The parameters (defaults to `null`).
    #[serde(default)]
    pub params: Value,
}

impl Request {
    /// Whether this is a notification (no `id`, so no response is expected).
    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A successful or failed response to a request.
#[derive(Debug, Clone, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A success response carrying `result`.
    pub fn ok(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: Some(result),
            error: None,
        }
    }

    /// An error response.
    pub fn err(id: Value, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// A JSON-RPC error object.
#[derive(Debug, Clone, Serialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

/// An outgoing notification (no id), e.g. a streamed `session/update`.
#[derive(Debug, Clone, Serialize)]
pub struct Notification {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: Value,
}

impl Notification {
    /// Build a notification for `method` with `params`.
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.into(),
            params,
        }
    }
}

/// Standard JSON-RPC error codes used by the server.
pub mod codes {
    /// The method is not recognised.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// The params were malformed for the method.
    pub const INVALID_PARAMS: i64 = -32602;
    /// A server-side error while handling the request.
    pub const INTERNAL_ERROR: i64 = -32603;
}
