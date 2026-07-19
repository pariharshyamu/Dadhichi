//! # dadhichi-gui
//!
//! Dadhichi's web GUI: a local HTTP server that serves a full IDE to the
//! browser — Monaco-powered editor (the same editor VS Code ships), a live
//! file explorer, and the agent console streaming over WebSocket — all backed
//! by the very same [`AppController`] the TUI uses, so agents, tools, approval
//! gates, session memory, and language-server diagnostics behave identically
//! across frontends.
//!
//! Everything is served from the binary (Monaco is embedded at compile time),
//! so it works fully offline and binds to localhost only.
//!
//! ```text
//! dadhichi-gui [--root <dir>] [--port <port>]
//! ```

use axum::{
    Router,
    extract::{Path as UrlPath, Query, State, WebSocketUpgrade, ws},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use dadhichi_app::{AppController, Decision};
use include_dir::{Dir, include_dir};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Monaco's `min/vs` distribution, embedded so the GUI works offline.
static MONACO: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/assets/vs");

const INDEX_HTML: &str = include_str!("../assets/index.html");
const APP_JS: &str = include_str!("../assets/app.js");
const STYLE_CSS: &str = include_str!("../assets/style.css");

/// Shared server state: the active controller, swappable when the user opens
/// a different workspace folder from the GUI.
#[derive(Clone)]
struct AppState {
    ctrl: Arc<tokio::sync::RwLock<Arc<AppController>>>,
}

impl AppState {
    /// The current controller (a cheap Arc clone; never hold the lock).
    async fn ctrl(&self) -> Arc<AppController> {
        self.ctrl.read().await.clone()
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let mut root = std::env::current_dir().expect("cwd");
    let mut port: u16 = 7433;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--root" => {
                if let Some(dir) = args.next() {
                    root = PathBuf::from(dir);
                }
            }
            "--port" => {
                if let Some(p) = args.next().and_then(|p| p.parse().ok()) {
                    port = p;
                }
            }
            "-h" | "--help" => {
                println!("dadhichi-gui — Dadhichi's local web IDE\n");
                println!("USAGE: dadhichi-gui [--root <dir>] [--port <port>]");
                println!("Serves the IDE at http://127.0.0.1:<port> (default {port}).");
                return;
            }
            _ => {}
        }
    }
    if let Ok(p) = std::env::var("DADHICHI_GUI_PORT")
        && let Ok(p) = p.parse()
    {
        port = p;
    }

    let ctrl = Arc::new(AppController::new(&root).await);
    let state = AppState {
        ctrl: Arc::new(tokio::sync::RwLock::new(ctrl)),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/style.css", get(style_css))
        .route("/vs/{*path}", get(monaco_asset))
        .route("/api/workspace", get(api_workspace).post(api_open_workspace))
        .route("/api/fs", get(api_fs))
        .route("/api/mkdir", axum::routing::post(api_mkdir))
        .route("/api/tree", get(api_tree))
        .route("/api/file", get(api_file_read).put(api_file_write))
        .route("/api/models", get(api_models))
        .route("/api/model", axum::routing::post(api_set_model))
        .route("/api/dispatch", axum::routing::post(api_dispatch))
        .route("/api/events", get(ws_events))
        .with_state(state);

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .unwrap_or_else(|e| panic!("cannot bind {addr}: {e}"));
    let url = format!("http://{addr}");
    println!("dadhichi-gui ▸ serving {} at {url}", root.display());
    open_browser(&url);
    axum::serve(listener, app).await.expect("server");
}

/// Best-effort: pop the default browser at `url`.
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    ([(header::CONTENT_TYPE, "application/javascript")], APP_JS).into_response()
}

async fn style_css() -> Response {
    ([(header::CONTENT_TYPE, "text/css")], STYLE_CSS).into_response()
}

/// Serve an embedded Monaco file, typed by extension.
async fn monaco_asset(UrlPath(path): UrlPath<String>) -> Response {
    match MONACO.get_file(&path) {
        Some(file) => {
            let mime = match path.rsplit('.').next() {
                Some("js") => "application/javascript",
                Some("css") => "text/css",
                Some("json") => "application/json",
                Some("ttf") => "font/ttf",
                Some("svg") => "image/svg+xml",
                _ => "application/octet-stream",
            };
            (
                [
                    (header::CONTENT_TYPE, mime),
                    // Monaco's files are versioned with the binary: cache hard.
                    (header::CACHE_CONTROL, "public, max-age=86400"),
                ],
                file.contents(),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn api_workspace(State(state): State<AppState>) -> Response {
    let ctrl = state.ctrl().await;
    let root = ctrl.workspace_root();
    axum::Json(serde_json::json!({
        "root": root.display().to_string(),
        "name": root.file_name().and_then(|n| n.to_str()).unwrap_or("workspace"),
        "model": ctrl.model_label(),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct OpenWorkspaceBody {
    root: String,
}

/// Open a different folder as the workspace: boots a fresh controller on it
/// (agents, tools, session memory, MCP config — all re-rooted) and swaps it in.
/// The frontend reloads afterwards so every panel rebinds to the new root.
async fn api_open_workspace(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<OpenWorkspaceBody>,
) -> Response {
    let root = PathBuf::from(body.root.trim());
    if !root.is_dir() {
        return (
            StatusCode::BAD_REQUEST,
            format!("not a directory: {}", root.display()),
        )
            .into_response();
    }
    let new_ctrl = Arc::new(AppController::new(&root).await);
    *state.ctrl.write().await = new_ctrl;
    axum::Json(serde_json::json!({
        "ok": true,
        "root": root.display().to_string(),
        "name": root.file_name().and_then(|n| n.to_str()).unwrap_or("workspace"),
    }))
    .into_response()
}

#[derive(Deserialize)]
struct FsQuery {
    #[serde(default)]
    path: String,
}

/// Browse the machine's directories for the Open Folder picker. An empty
/// `path` lists the roots (drives on Windows, `/` elsewhere); otherwise the
/// subdirectories of `path`, with its parent for "up" navigation.
async fn api_fs(Query(q): Query<FsQuery>) -> Response {
    let path = q.path.trim();
    if path.is_empty() {
        return axum::Json(serde_json::json!({
            "path": "",
            "parent": null,
            "dirs": fs_roots(),
        }))
        .into_response();
    }
    let dir = PathBuf::from(path);
    if !dir.is_dir() {
        return (StatusCode::BAD_REQUEST, "not a directory").into_response();
    }
    let mut dirs: Vec<serde_json::Value> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .filter_map(|e| {
                    let name = e.file_name().to_str()?.to_string();
                    if name.starts_with('.') || name == "node_modules" || name == "target" {
                        return None;
                    }
                    Some(serde_json::json!({
                        "name": name,
                        "path": e.path().display().to_string(),
                    }))
                })
                .collect()
        })
        .unwrap_or_default();
    dirs.sort_by(|a, b| {
        a["name"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .cmp(&b["name"].as_str().unwrap_or("").to_lowercase())
    });
    let parent = dir
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.display().to_string());
    axum::Json(serde_json::json!({
        "path": dir.display().to_string(),
        "parent": parent,
        "dirs": dirs,
    }))
    .into_response()
}

/// The filesystem roots the picker starts from.
fn fs_roots() -> Vec<serde_json::Value> {
    if cfg!(windows) {
        (b'A'..=b'Z')
            .filter_map(|letter| {
                let drive = format!("{}:\\", letter as char);
                std::path::Path::new(&drive)
                    .exists()
                    .then(|| serde_json::json!({ "name": drive.clone(), "path": drive }))
            })
            .collect()
    } else {
        vec![serde_json::json!({ "name": "/", "path": "/" })]
    }
}

#[derive(Deserialize)]
struct MkdirBody {
    path: String,
}

/// Create a folder (workspace-relative) in the open workspace.
async fn api_mkdir(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<MkdirBody>,
) -> Response {
    let ctrl = state.ctrl().await;
    let Some(path) = safe_join(ctrl.workspace_root(), &body.path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match std::fs::create_dir_all(&path) {
        Ok(()) => axum::Json(serde_json::json!({ "ok": true })).into_response(),
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

/// One level of the file tree at `?path=` (workspace-relative; empty = root).
/// Directories first, then files, both alphabetical; dotfiles and heavyweight
/// build dirs are skipped.
async fn api_tree(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let ctrl = state.ctrl().await;
    let rel = q.get("path").map(String::as_str).unwrap_or("");
    let root = ctrl.workspace_root();
    let Some(dir) = safe_join(root, rel) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    axum::Json(list_dir(root, &dir)).into_response()
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn api_file_read(
    State(state): State<AppState>,
    Query(q): Query<FileQuery>,
) -> Response {
    let ctrl = state.ctrl().await;
    let Some(path) = safe_join(ctrl.workspace_root(), &q.path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            // Opening a file is the moment its language server should hear
            // about it — diagnostics then stream to the Problems panel.
            ctrl.sync_document_at(path, text.clone());
            axum::Json(serde_json::json!({ "text": text })).into_response()
        }
        Err(err) => (StatusCode::NOT_FOUND, err.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct SaveBody {
    path: String,
    text: String,
}

async fn api_file_write(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<SaveBody>,
) -> Response {
    let ctrl = state.ctrl().await;
    let Some(path) = safe_join(ctrl.workspace_root(), &body.path) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // Creating a file in a folder that doesn't exist yet should just work
    // (the explorer's "new file" can name a nested path).
    if let Some(parent) = path.parent()
        && let Err(err) = std::fs::create_dir_all(parent)
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response();
    }
    match std::fs::write(&path, &body.text) {
        Ok(()) => {
            ctrl.sync_document_at(path, body.text);
            axum::Json(serde_json::json!({ "ok": true })).into_response()
        }
        Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
    }
}

/// The models available for switching: the active one plus everything the
/// local/remote Ollama daemon advertises via `/api/tags`. An unreachable
/// daemon degrades to just the current model.
async fn api_models(State(state): State<AppState>) -> Response {
    let current = state.ctrl().await.model_label();
    let host = std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(normalize_ollama_host)
        .unwrap_or_else(|| "http://localhost:11434".to_string());

    let mut models: Vec<String> = Vec::new();
    let tags = reqwest::Client::new()
        .get(format!("{host}/api/tags"))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await;
    if let Ok(resp) = tags
        && let Ok(body) = resp.json::<serde_json::Value>().await
        && let Some(list) = body.get("models").and_then(|m| m.as_array())
    {
        models = list
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()))
            .map(String::from)
            .collect();
    }
    // Claude Code is a switchable backend, not an Ollama model — offer it
    // whenever its CLI is resolvable (PATH, env, or the editor extension).
    let claude = "claude-code".to_string();
    if !models.contains(&claude)
        && (which_claude_exists() || current == claude)
    {
        models.push(claude);
    }
    if !models.contains(&current) {
        models.insert(0, current.clone());
    }
    axum::Json(serde_json::json!({ "current": current, "models": models })).into_response()
}

/// Whether a launchable Claude Code CLI can be found on this machine.
fn which_claude_exists() -> bool {
    let resolved = dadhichi_mcp::resolve_launcher("claude");
    if resolved != "claude" {
        return true;
    }
    // Bare name: check PATH (with Windows launcher extensions).
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path).any(|dir| {
        ["claude", "claude.exe", "claude.cmd", "claude.ps1"]
            .iter()
            .any(|n| dir.join(n).is_file())
    })
}

/// `OLLAMA_HOST` accepts bare hostnames and host:port; requests need a scheme.
fn normalize_ollama_host(raw: String) -> String {
    let raw = raw.trim().trim_end_matches('/').to_string();
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw
    } else if raw.contains(':') {
        format!("http://{raw}")
    } else {
        format!("http://{raw}:11434")
    }
}

#[derive(Deserialize)]
struct ModelBody {
    model: String,
}

async fn api_set_model(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<ModelBody>,
) -> Response {
    let ctrl = state.ctrl().await;
    ctrl.set_model(&body.model);
    axum::Json(serde_json::json!({ "ok": true, "model": ctrl.model_label() })).into_response()
}

#[derive(Deserialize)]
struct DispatchBody {
    name: String,
    #[serde(default)]
    args: serde_json::Value,
}

/// A generic bridge to kernel commands (`mcp.list`, `mcp.add`, …) — the same
/// surface the TUI palette drives, exposed to the localhost browser.
async fn api_dispatch(
    State(state): State<AppState>,
    axum::Json(body): axum::Json<DispatchBody>,
) -> Response {
    let args = if body.args.is_null() {
        serde_json::json!({})
    } else {
        body.args
    };
    match state.ctrl().await.dispatch(&body.name, args).await {
        Ok(value) => axum::Json(value).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, err.to_string()).into_response(),
    }
}

/// The WebSocket: every kernel event streams out as JSON; goal submissions,
/// approval verdicts, and completion requests come back in.
async fn ws_events(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| handle_ws(socket, state))
}

async fn handle_ws(socket: ws::WebSocket, state: AppState) {
    let (mut tx, mut rx) = {
        use futures_split::split;
        split(socket)
    };
    // The socket binds to the controller live at connect time; after an Open
    // Folder swap the frontend reloads and reconnects to the new one.
    let ctrl = state.ctrl().await;
    let mut events = ctrl.subscribe_events();

    // Outbound: bus → browser.
    let forward = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let msg = serde_json::json!({
                        "topic": event.topic.as_str(),
                        "payload": event.payload,
                        "ts": event.timestamp_ms as u64,
                    });
                    if tx.send(ws::Message::Text(msg.to_string().into())).await.is_err() {
                        break;
                    }
                }
                Err(dadhichi_core::RecvError::Lagged(_)) => continue,
                Err(dadhichi_core::RecvError::Closed) => break,
            }
        }
    });

    // Inbound: browser → controller.
    while let Some(Ok(msg)) = rx.recv().await {
        let ws::Message::Text(text) = msg else { continue };
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match kind {
            "goal" => {
                if let Some(goal) = value.get("goal").and_then(|g| g.as_str()) {
                    ctrl.start_agent_goal(goal);
                }
            }
            "approval" => {
                let id = value.get("id").and_then(|i| i.as_str()).unwrap_or("");
                let approve = value
                    .get("approve")
                    .and_then(|a| a.as_bool())
                    .unwrap_or(false);
                let decision = if approve {
                    Decision::Approve
                } else {
                    Decision::Deny
                };
                ctrl.resolve_approval(id, decision);
            }
            "completion" => {
                let path = value.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let line = value.get("line").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
                let col = value.get("col").and_then(|c| c.as_u64()).unwrap_or(0) as u32;
                if let Some(abs) = safe_join(ctrl.workspace_root(), path) {
                    ctrl.request_completions_at(abs, text.to_string(), line, col);
                }
            }
            "sync" => {
                let path = value.get("path").and_then(|p| p.as_str()).unwrap_or("");
                let text = value.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if let Some(abs) = safe_join(ctrl.workspace_root(), path) {
                    ctrl.sync_document_at(abs, text.to_string());
                }
            }
            _ => {}
        }
    }
    forward.abort();
}

/// Split helper so the ws socket halves can live on separate tasks without
/// pulling the whole `futures` crate into the dependency graph.
mod futures_split {
    use axum::extract::ws::{Message, WebSocket};
    use tokio::sync::mpsc;

    pub struct Tx(mpsc::Sender<Message>);
    pub struct Rx {
        inbound: mpsc::Receiver<Result<Message, axum::Error>>,
    }

    impl Tx {
        pub async fn send(&mut self, msg: Message) -> Result<(), ()> {
            self.0.send(msg).await.map_err(|_| ())
        }
    }

    impl Rx {
        pub async fn recv(&mut self) -> Option<Result<Message, axum::Error>> {
            self.inbound.recv().await
        }
    }

    /// Drive the socket on a private task; expose channel-backed halves.
    pub fn split(mut socket: WebSocket) -> (Tx, Rx) {
        let (out_tx, mut out_rx) = mpsc::channel::<Message>(64);
        let (in_tx, in_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    outgoing = out_rx.recv() => match outgoing {
                        Some(msg) => {
                            if socket.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                    incoming = socket.recv() => match incoming {
                        Some(msg) => {
                            if in_tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    },
                }
            }
        });
        (Tx(out_tx), Rx { inbound: in_rx })
    }
}

/// Resolve a workspace-relative request path, refusing anything that would
/// escape the root (`..`, absolute paths, drive letters).
fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let rel = rel.trim_start_matches(['/', '\\']);
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return None;
    }
    let mut out = root.to_path_buf();
    for part in candidate.components() {
        match part {
            std::path::Component::Normal(seg) => {
                // A rooted or drive-lettered segment can't sneak through.
                let seg_str = seg.to_str()?;
                if seg_str.contains(':') {
                    return None;
                }
                out.push(seg);
            }
            std::path::Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Directories a file tree should not descend into.
const TREE_SKIP: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".dadhichi",
    "dist",
    "build",
    "__pycache__",
];

/// One directory level as JSON rows, directories first.
fn list_dir(root: &Path, dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(bool, String, String)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_str()?.to_string();
            if name.starts_with('.') || TREE_SKIP.contains(&name.as_str()) {
                return None;
            }
            let is_dir = entry.file_type().ok()?.is_dir();
            let rel = entry
                .path()
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .replace('\\', "/");
            Some((is_dir, name, rel))
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    rows.into_iter()
        .map(|(is_dir, name, rel)| {
            serde_json::json!({ "name": name, "path": rel, "is_dir": is_dir })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_confines_paths_to_the_root() {
        let root = Path::new("/work");
        assert_eq!(safe_join(root, "src/main.rs").unwrap(), root.join("src").join("main.rs"));
        assert_eq!(safe_join(root, "").unwrap(), root);
        // Escapes are refused outright, not resolved.
        assert!(safe_join(root, "../secrets").is_none());
        assert!(safe_join(root, "src/../../etc/passwd").is_none());
        assert!(safe_join(root, "C:/Windows/system32").is_none());
        assert!(safe_join(root, "C:\\Windows").is_none());
        // A rooted path is reinterpreted as workspace-relative — confined, not honored.
        assert_eq!(
            safe_join(root, "/etc/passwd").unwrap(),
            root.join("etc").join("passwd")
        );
    }

    #[test]
    fn list_dir_returns_sorted_levels_and_skips_noise() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("node_modules")).unwrap();
        std::fs::write(root.join("zeta.rs"), "").unwrap();
        std::fs::write(root.join("Alpha.rs"), "").unwrap();
        std::fs::write(root.join(".hidden"), "").unwrap();

        let rows = list_dir(root, root);
        let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
        // Directory first, then files case-insensitively; noise gone.
        assert_eq!(names, vec!["src", "Alpha.rs", "zeta.rs"]);
        assert!(rows[0]["is_dir"].as_bool().unwrap());
        // Paths are workspace-relative with forward slashes.
        assert_eq!(rows[0]["path"], "src");
    }

    #[test]
    fn ollama_hosts_are_normalized_to_urls() {
        assert_eq!(normalize_ollama_host("http://x:1234".into()), "http://x:1234");
        assert_eq!(normalize_ollama_host("https://o.example/".into()), "https://o.example");
        assert_eq!(normalize_ollama_host("localhost:11434".into()), "http://localhost:11434");
        assert_eq!(normalize_ollama_host("ollama".into()), "http://ollama:11434");
    }

    #[test]
    fn monaco_assets_are_embedded() {
        assert!(
            MONACO.get_file("loader.js").is_some(),
            "monaco loader must ship inside the binary"
        );
        assert!(MONACO.get_file("editor/editor.main.js").is_some());
    }
}
