//! Declarative MCP connectors: launch configured servers and bridge their
//! tools into the [`ToolRegistry`](crate::ToolRegistry).
//!
//! An [`McpServersConfig`] — loaded from an `mcp.json` file, à la the skills
//! manifests — names a set of servers by `command`/`args`/`env`, plus the
//! [`Permission`]s their tools should require. [`connect_servers`] launches each
//! enabled server over stdio, discovers its tools, stamps them with the
//! configured permission envelope, and registers them. Because the registry is
//! interior-mutable, this works on the shared `Arc<ToolRegistry>` every agent
//! already holds — no rebuild.
//!
//! Secrets never live in the config: an `env` value may contain `${key}`
//! placeholders resolved through a caller-supplied closure (e.g. reading process
//! environment or a credential vault), so a token is injected at launch, not
//! committed.

use crate::bridge::McpToolBridge;
use crate::client::McpConnection;
use crate::registry::ToolRegistry;
use crate::tool::Permission;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

fn default_true() -> bool {
    true
}

/// One configured MCP server — either a local subprocess (`command`) or a hosted
/// endpoint (`url`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerConfig {
    /// The executable to launch for a stdio server, e.g. `npx` or `uvx`. Ignored
    /// when `url` is set.
    #[serde(default)]
    pub command: String,
    /// Arguments passed to the command.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the child process. Values may contain `${key}`
    /// placeholders resolved at launch (see [`connect_servers`]).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// A remote MCP endpoint. When set, the server is reached over the network —
    /// Streamable HTTP for `http(s)://`, WebSocket for `ws(s)://` — instead of
    /// launching `command`. Requires the crate's `remote` feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Headers sent to a remote `url` (on every HTTP request, or on the WebSocket
    /// handshake). Values may contain `${key}` placeholders — the place for an
    /// `Authorization: Bearer ${env:TOKEN}` on a hosted server.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// The permission envelope stamped onto every tool this server exposes, so
    /// external tools are gated exactly like built-ins. Empty means ungated.
    #[serde(default)]
    pub grants: Vec<Permission>,
    /// Whether to launch this server. Defaults to `true`.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl McpServerConfig {
    /// How this server is reached, for display (`stdio` or the remote scheme).
    pub fn transport(&self) -> &'static str {
        match self.url.as_deref() {
            Some(u) if u.starts_with("ws://") || u.starts_with("wss://") => "websocket",
            Some(_) => "http",
            None => "stdio",
        }
    }
}

/// A set of named MCP servers — the parsed form of an `mcp.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServersConfig {
    /// Servers keyed by a stable local name.
    #[serde(default, alias = "mcpServers")]
    pub servers: BTreeMap<String, McpServerConfig>,
}

impl McpServersConfig {
    /// Parse a config from JSON (`{ "servers": { ... } }`, or the
    /// `{ "mcpServers": { ... } }` shape used by other MCP hosts).
    pub fn from_json(source: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(source)
    }

    /// Merge `other` into `self`, with `other`'s servers overriding by name.
    pub fn merge(&mut self, other: McpServersConfig) {
        self.servers.extend(other.servers);
    }

    /// The enabled servers, in name order.
    pub fn enabled(&self) -> impl Iterator<Item = (&String, &McpServerConfig)> {
        self.servers.iter().filter(|(_, c)| c.enabled)
    }

    /// Load and merge every standard `mcp.json` for `project_root`, in
    /// increasing precedence: `~/.dadhichi/mcp.json`,
    /// `<project_root>/.dadhichi/mcp.json`, then `$DADHICHI_MCP_CONFIG`. Missing
    /// files are skipped; a malformed one is reported (not fatal) in the
    /// returned messages.
    pub fn discover_in(project_root: impl AsRef<std::path::Path>) -> (Self, Vec<String>) {
        let mut config = Self::default();
        let mut errors = Vec::new();
        for path in config_files_in(project_root.as_ref()) {
            match std::fs::read_to_string(&path) {
                Ok(text) => match Self::from_json(&text) {
                    Ok(loaded) => config.merge(loaded),
                    Err(err) => errors.push(format!("{}: {err}", path.display())),
                },
                Err(_) => continue, // missing file is fine
            }
        }
        (config, errors)
    }
}

/// The candidate `mcp.json` paths for `project_root`, in increasing precedence.
fn config_files_in(project_root: &std::path::Path) -> Vec<std::path::PathBuf> {
    use std::path::PathBuf;
    let nonempty = |v: std::ffi::OsString| (!v.is_empty()).then_some(v);
    let mut paths = Vec::new();
    if let Some(home) = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .and_then(nonempty)
    {
        paths.push(PathBuf::from(home).join(".dadhichi").join("mcp.json"));
    }
    paths.push(project_root.join(".dadhichi").join("mcp.json"));
    if let Some(explicit) = std::env::var_os("DADHICHI_MCP_CONFIG").and_then(nonempty) {
        paths.push(PathBuf::from(explicit));
    }
    paths
}

/// A server whose tools were bridged in successfully.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConnectedServer {
    /// The configured server name.
    pub name: String,
    /// The names of the tools it contributed.
    pub tools: Vec<String>,
}

/// A server that failed to connect or discover.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ServerError {
    /// The configured server name.
    pub server: String,
    /// A human-readable reason.
    pub message: String,
}

/// The outcome of a [`connect_servers`] pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ConnectReport {
    /// Servers connected, with the tools each contributed.
    pub connected: Vec<ConnectedServer>,
    /// Servers that failed (with the reason), non-fatally.
    pub errors: Vec<ServerError>,
}

impl ConnectReport {
    /// Total tools bridged across all connected servers.
    pub fn tool_count(&self) -> usize {
        self.connected.iter().map(|s| s.tools.len()).sum()
    }
}

/// Live MCP connections. Keep this alive for as long as the bridged tools should
/// remain usable — dropping it closes the server subprocesses.
#[derive(Debug, Default)]
pub struct McpConnections {
    connections: BTreeMap<String, Arc<McpConnection>>,
}

impl McpConnections {
    /// The names of the currently-connected servers.
    pub fn names(&self) -> Vec<String> {
        self.connections.keys().cloned().collect()
    }

    /// Whether a server with `name` is connected.
    pub fn contains(&self, name: &str) -> bool {
        self.connections.contains_key(name)
    }
}

/// Substitute `${key}` placeholders in `env` values via `resolve`, returning the
/// concrete environment. Fails (naming the key) if a placeholder cannot be
/// resolved, so a server is never launched with a missing secret.
pub fn resolve_env<F>(
    env: &BTreeMap<String, String>,
    resolve: &F,
) -> Result<Vec<(String, String)>, String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = Vec::with_capacity(env.len());
    for (key, template) in env {
        out.push((key.clone(), substitute(template, resolve)?));
    }
    Ok(out)
}

/// Replace every `${...}` in `template`, or return the name of the first
/// placeholder that could not be resolved.
fn substitute<F>(template: &str, resolve: &F) -> Result<String, String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let end = after
            .find('}')
            .ok_or_else(|| format!("unterminated placeholder in `{template}`"))?;
        let key = &after[..end];
        let value = resolve(key).ok_or_else(|| format!("unresolved secret `{key}`"))?;
        out.push_str(&value);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Launch every enabled server in `config`, bridge its tools into `registry`
/// (stamped with the server's permission envelope), and return the live
/// connections plus a report. Failures are collected, not fatal.
pub async fn connect_servers<F>(
    config: &McpServersConfig,
    registry: &ToolRegistry,
    resolve: F,
) -> (McpConnections, ConnectReport)
where
    F: Fn(&str) -> Option<String>,
{
    let mut connections = McpConnections::default();
    let mut report = ConnectReport::default();

    for (name, server) in config.enabled() {
        match connect_one(name, server, registry, &resolve).await {
            Ok((conn, tools)) => {
                connections.connections.insert(name.clone(), conn);
                report.connected.push(ConnectedServer {
                    name: name.clone(),
                    tools,
                });
            }
            Err(message) => report.errors.push(ServerError {
                server: name.clone(),
                message,
            }),
        }
    }

    (connections, report)
}

/// Connect a single server and register its tools; returns the connection and
/// the tool names it contributed.
async fn connect_one<F>(
    name: &str,
    server: &McpServerConfig,
    registry: &ToolRegistry,
    resolve: &F,
) -> Result<(Arc<McpConnection>, Vec<String>), String>
where
    F: Fn(&str) -> Option<String>,
{
    let conn = match &server.url {
        Some(url) => connect_remote(url, server, resolve).await?,
        None => {
            let env = resolve_env(&server.env, resolve)?;
            McpConnection::connect_stdio_env(&server.command, &server.args, &env)
                .await
                .map_err(|e| e.to_string())?
        }
    };
    conn.handshake().await.map_err(|e| e.to_string())?;
    let conn = Arc::new(conn);

    let tool_names = bridge_tools(name, server, registry, &conn)
        .await
        .map_err(|e| e.to_string())?;
    Ok((conn, tool_names))
}

/// Open a connection to a remote (`http(s)`/`ws(s)`) server, resolving any
/// `${...}` placeholders in its headers first.
#[cfg(feature = "remote")]
async fn connect_remote<F>(
    url: &str,
    server: &McpServerConfig,
    resolve: &F,
) -> Result<McpConnection, String>
where
    F: Fn(&str) -> Option<String>,
{
    let headers = resolve_env(&server.headers, resolve)?;
    McpConnection::connect_url(url, &headers)
        .await
        .map_err(|e| e.to_string())
}

/// Without the `remote` feature, a `url` server cannot be reached — report it as
/// a non-fatal error rather than silently skipping it.
#[cfg(not(feature = "remote"))]
async fn connect_remote<F>(
    _url: &str,
    _server: &McpServerConfig,
    _resolve: &F,
) -> Result<McpConnection, String>
where
    F: Fn(&str) -> Option<String>,
{
    Err("remote MCP transport not compiled in (enable the `remote` feature)".to_string())
}

/// Discover `conn`'s tools, stamp them with `server`'s permission envelope and a
/// `<name>.` prefix, and register each in `registry`. Returns the tool names.
///
/// Split out from the process launch so it can be exercised over an in-memory
/// transport in tests.
async fn bridge_tools(
    name: &str,
    server: &McpServerConfig,
    registry: &ToolRegistry,
    conn: &Arc<McpConnection>,
) -> Result<Vec<String>, crate::protocol::McpError> {
    let specs = conn.list_tools().await?;
    let mut tool_names = Vec::with_capacity(specs.len());
    for mut spec in specs {
        // Stamp the configured permission envelope so external tools are gated
        // like built-ins. A namespaced name avoids clashes across servers.
        if !server.grants.is_empty() {
            spec.permissions = server.grants.clone();
        }
        if !spec.name.contains('.') {
            spec.name = format!("{name}.{}", spec.name);
        }
        tool_names.push(spec.name.clone());
        registry.register(Arc::new(McpToolBridge::new(conn.clone(), spec)));
    }
    tool_names.sort();
    Ok(tool_names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn parses_remote_server_and_reports_transport() {
        let cfg = McpServersConfig::from_json(
            r#"{"servers":{
                "linear":{"url":"https://mcp.linear.app/sse","headers":{"Authorization":"Bearer ${env:LINEAR}"},"grants":["network"]},
                "chat":{"url":"wss://example.com/mcp"},
                "local":{"command":"npx"}
            }}"#,
        )
        .unwrap();
        assert_eq!(
            cfg.servers["linear"].url.as_deref(),
            Some("https://mcp.linear.app/sse")
        );
        assert_eq!(
            cfg.servers["linear"].headers["Authorization"],
            "Bearer ${env:LINEAR}"
        );
        assert_eq!(cfg.servers["linear"].transport(), "http");
        assert_eq!(cfg.servers["chat"].transport(), "websocket");
        assert_eq!(cfg.servers["local"].transport(), "stdio");
    }

    #[test]
    fn parses_both_config_shapes() {
        let a = McpServersConfig::from_json(
            r#"{"servers":{"gh":{"command":"npx","args":["-y","srv"]}}}"#,
        )
        .unwrap();
        let b = McpServersConfig::from_json(
            r#"{"mcpServers":{"gh":{"command":"npx","args":["-y","srv"]}}}"#,
        )
        .unwrap();
        assert_eq!(a, b);
        assert_eq!(a.servers["gh"].command, "npx");
        assert!(a.servers["gh"].enabled); // default true
    }

    #[test]
    fn grants_and_enabled_defaults() {
        let cfg = McpServersConfig::from_json(
            r#"{"servers":{
                "gh":{"command":"x","grants":["network","read_workspace"]},
                "off":{"command":"y","enabled":false}
            }}"#,
        )
        .unwrap();
        assert_eq!(
            cfg.servers["gh"].grants,
            vec![Permission::Network, Permission::ReadWorkspace]
        );
        let enabled: Vec<_> = cfg.enabled().map(|(n, _)| n.clone()).collect();
        assert_eq!(enabled, vec!["gh".to_string()]); // "off" filtered out
    }

    #[test]
    fn merge_overrides_by_name() {
        let mut base =
            McpServersConfig::from_json(r#"{"servers":{"a":{"command":"one"}}}"#).unwrap();
        base.merge(
            McpServersConfig::from_json(
                r#"{"servers":{"a":{"command":"two"},"b":{"command":"three"}}}"#,
            )
            .unwrap(),
        );
        assert_eq!(base.servers["a"].command, "two");
        assert!(base.servers.contains_key("b"));
    }

    #[test]
    fn resolves_env_placeholders() {
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), "${env:GH}".to_string());
        env.insert("PLAIN".to_string(), "literal".to_string());
        env.insert("MIXED".to_string(), "Bearer ${env:GH}!".to_string());

        let resolve = |k: &str| match k {
            "env:GH" => Some("secret123".to_string()),
            _ => None,
        };
        let resolved = resolve_env(&env, &resolve).unwrap();
        let map: BTreeMap<_, _> = resolved.into_iter().collect();
        assert_eq!(map["TOKEN"], "secret123");
        assert_eq!(map["PLAIN"], "literal");
        assert_eq!(map["MIXED"], "Bearer secret123!");
    }

    #[test]
    fn unresolved_secret_is_an_error_naming_the_key() {
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), "${vault:missing}".to_string());
        let err = resolve_env(&env, &|_| None).unwrap_err();
        assert!(err.contains("vault:missing"));
    }

    #[tokio::test]
    async fn connecting_a_bad_command_is_a_non_fatal_error() {
        let cfg = McpServersConfig::from_json(
            r#"{"servers":{"nope":{"command":"definitely-not-a-real-binary-xyz"}}}"#,
        )
        .unwrap();
        let registry = ToolRegistry::new();
        let (conns, report) = connect_servers(&cfg, &registry, |_| None).await;

        assert!(conns.names().is_empty());
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].server, "nope");
        assert!(report.connected.is_empty());
    }

    // ── In-memory bridging (no subprocess) ───────────────────────────────────

    use crate::client::McpConnection;
    use crate::registry::GrantSet;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    async fn mock_server(
        reader: tokio::io::ReadHalf<tokio::io::DuplexStream>,
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    ) {
        let mut lines = BufReader::new(reader).lines();
        let mut writer = writer;
        while let Ok(Some(line)) = lines.next_line().await {
            let req: serde_json::Value = serde_json::from_str(&line).unwrap();
            let id = req.get("id").cloned().unwrap();
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let result = match method {
                // A tool with a bare (un-namespaced) name and no permissions.
                "tools/list" => serde_json::json!({
                    "tools": [{ "name": "create_issue", "description": "open an issue", "inputSchema": {} }]
                }),
                "tools/call" => serde_json::json!({ "issue": 7 }),
                _ => serde_json::Value::Null,
            };
            let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
            let mut bytes = serde_json::to_vec(&resp).unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
            writer.flush().await.unwrap();
        }
    }

    #[tokio::test]
    async fn bridge_namespaces_and_stamps_permissions() {
        let (client_side, server_side) = tokio::io::duplex(8192);
        let (c_read, c_write) = tokio::io::split(client_side);
        let (s_read, s_write) = tokio::io::split(server_side);
        tokio::spawn(mock_server(s_read, s_write));
        let conn = Arc::new(McpConnection::new(c_read, c_write));

        let server = McpServerConfig {
            command: "unused".into(),
            args: vec![],
            env: BTreeMap::new(),
            url: None,
            headers: BTreeMap::new(),
            grants: vec![Permission::Network],
            enabled: true,
        };
        let registry = ToolRegistry::new();
        let names = bridge_tools("github", &server, &registry, &conn)
            .await
            .unwrap();

        // Bare `create_issue` was namespaced to `github.create_issue`.
        assert_eq!(names, vec!["github.create_issue".to_string()]);
        assert!(registry.contains("github.create_issue"));

        // The configured permission envelope now gates the external tool.
        let denied = registry
            .invoke(
                "github.create_issue",
                serde_json::json!({}),
                &GrantSet::none(),
            )
            .await;
        assert!(denied.is_err());

        let grants = GrantSet::from_iter([Permission::Network]);
        let out = registry
            .invoke(
                "github.create_issue",
                serde_json::json!({ "title": "bug" }),
                &grants,
            )
            .await
            .unwrap();
        assert_eq!(out["issue"], 7);
    }

    // ── End-to-end over a real remote transport (loopback WebSocket) ──────────

    /// Drives the full declarative path — `mcp.json` → `connect_servers` → live
    /// WebSocket → namespaced, permission-stamped tools — against a loopback
    /// server, so no network egress is needed.
    #[cfg(feature = "remote")]
    #[tokio::test]
    async fn connects_a_remote_websocket_server_and_bridges_its_tools() {
        use futures::{SinkExt, StreamExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::tungstenite::Message;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let req: serde_json::Value = serde_json::from_str(text.as_str()).unwrap();
                let id = req.get("id").cloned().unwrap();
                let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let result = match method {
                    "initialize" => serde_json::json!({ "capabilities": { "tools": {} } }),
                    "tools/list" => serde_json::json!({
                        "tools": [{ "name": "send_message", "description": "post", "inputSchema": {} }]
                    }),
                    "tools/call" => serde_json::json!({ "ok": true }),
                    _ => serde_json::Value::Null,
                };
                let resp = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
                ws.send(Message::text(resp.to_string())).await.unwrap();
            }
        });

        let cfg = McpServersConfig::from_json(&format!(
            r#"{{"servers":{{"slack":{{"url":"ws://{addr}","grants":["network"]}}}}}}"#
        ))
        .unwrap();
        let registry = ToolRegistry::new();
        let (conns, report) = connect_servers(&cfg, &registry, |_| None).await;

        assert!(conns.contains("slack"));
        assert_eq!(report.errors.len(), 0, "{:?}", report.errors);
        assert_eq!(report.tool_count(), 1);
        assert!(registry.contains("slack.send_message"));

        // Gated by the configured Network envelope, invocable with the grant.
        let grants = GrantSet::from_iter([Permission::Network]);
        let out = registry
            .invoke("slack.send_message", serde_json::json!({}), &grants)
            .await
            .unwrap();
        assert_eq!(out["ok"], true);
    }
}
