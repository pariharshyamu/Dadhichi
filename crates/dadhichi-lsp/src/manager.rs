//! One manager over every language server the workspace needs.
//!
//! The editor asks a single question — *"completions for this file at this
//! position, given this (possibly unsaved) text"* — and [`LspManager`] does
//! whatever that takes: resolve which server speaks for the file, spawn and
//! initialize it on first use (one instance per server command, so TypeScript
//! and JavaScript share a process), keep the server's view of the document in
//! sync via `didOpen`/`didChange`, and forward the completion request.
//!
//! Missing servers degrade gracefully: a spawn failure is cached and reported
//! as [`LspError::Unavailable`] instead of retrying the spawn on every
//! keystroke. Diagnostics from every managed server flow onto the kernel event
//! bus as `lsp.diagnostics` (the same seam the Problems panel already reads).

use crate::client::{LspClient, LspError};
use crate::protocol::{CompletionItem, Position};
use crate::registry::{ServerSpec, server_for_path_in_root};
use dadhichi_core::EventBus;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Resolves a file to its language server. Injectable so tests (and later,
/// user config) can supply their own table.
type Resolver = Box<dyn Fn(&Path) -> Option<ServerSpec> + Send + Sync>;

/// The mutable interior: live clients, cached failures, and document versions.
#[derive(Default)]
struct ManagerState {
    /// Running clients, keyed by server command (shared across languages that
    /// use the same server).
    clients: HashMap<String, Arc<LspClient>>,
    /// Servers that failed to start, with the reason — checked before every
    /// spawn so a missing binary errors once, fast, instead of on every call.
    failed: HashMap<String, String>,
    /// Version counter per open document URI, for `didChange` sync.
    docs: HashMap<String, i64>,
}

/// Spawns, initializes and multiplexes language servers for a workspace.
pub struct LspManager {
    root: PathBuf,
    bus: Option<EventBus>,
    state: Mutex<ManagerState>,
    resolver: Resolver,
}

impl std::fmt::Debug for LspManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LspManager")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl LspManager {
    /// A manager for the workspace at `root`, using the built-in
    /// [server registry](crate::registry) with project-aware overrides (e.g.
    /// Angular workspaces). Diagnostics are published on `bus`.
    pub fn new(root: impl Into<PathBuf>, bus: Option<EventBus>) -> Self {
        let root = root.into();
        let resolver_root = root.clone();
        Self::with_resolver(
            root,
            bus,
            Box::new(move |path| server_for_path_in_root(&resolver_root, path)),
        )
    }

    /// A manager with a custom file→server resolver (tests, user overrides).
    pub fn with_resolver(root: impl Into<PathBuf>, bus: Option<EventBus>, resolver: Resolver) -> Self {
        Self {
            root: root.into(),
            bus,
            state: Mutex::new(ManagerState::default()),
            resolver,
        }
    }

    /// Completion suggestions for `path` at `position`, given the buffer's
    /// current `text` (which may be dirtier than the file on disk — the server
    /// is synced with exactly this text before the request).
    pub async fn completions(
        &self,
        path: &Path,
        text: &str,
        position: Position,
    ) -> Result<Vec<CompletionItem>, LspError> {
        let (client, uri) = self.ensure_synced(path, text).await?;
        client.completion(&uri, position).await
    }

    /// Sync `path`'s current `text` with its language server without asking for
    /// anything back — spawning and initializing the server on first sight.
    /// This is what makes diagnostics flow on file *open* and *save*: servers
    /// only publish problems for documents they've been told about, so without
    /// this the Problems panel stayed empty until the first completion request.
    pub async fn sync(&self, path: &Path, text: &str) -> Result<(), LspError> {
        self.ensure_synced(path, text).await.map(|_| ())
    }

    /// The running (or newly spawned + initialized) client for `path`'s
    /// language, with the document's text synced via `didOpen`/`didChange`.
    async fn ensure_synced(
        &self,
        path: &Path,
        text: &str,
    ) -> Result<(Arc<LspClient>, String), LspError> {
        let spec = (self.resolver)(path)
            .ok_or_else(|| LspError::Unsupported(path.display().to_string()))?;

        let mut state = self.state.lock().await;

        // A server that already failed to start stays failed for the session.
        if let Some(reason) = state.failed.get(&spec.command) {
            return Err(LspError::Unavailable {
                command: spec.command.clone(),
                reason: reason.clone(),
            });
        }

        // Get the running client for this server, or spawn + initialize it.
        let client = match state.clients.get(&spec.command) {
            Some(client) => client.clone(),
            None => {
                let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
                let spawned =
                    LspClient::connect_stdio(&spec.command, &args, self.bus.clone()).await;
                let client = match spawned {
                    Ok(client) => Arc::new(client),
                    Err(err) => {
                        let reason = err.to_string();
                        state.failed.insert(spec.command.clone(), reason.clone());
                        return Err(LspError::Unavailable {
                            command: spec.command,
                            reason,
                        });
                    }
                };
                let root_uri = to_uri(&self.root);
                if let Err(err) = client.initialize(&root_uri).await {
                    let reason = format!("initialize failed: {err}");
                    state.failed.insert(spec.command.clone(), reason.clone());
                    return Err(LspError::Unavailable {
                        command: spec.command,
                        reason,
                    });
                }
                state
                    .clients
                    .insert(spec.command.clone(), client.clone());
                client
            }
        };

        // Sync the buffer: first sight of a document opens it, after that each
        // call replaces the server's copy with the current text (full sync).
        let uri = to_uri(&self.absolute(path));
        match state.docs.get_mut(&uri) {
            None => {
                client.did_open(&uri, &spec.language_id, text).await?;
                state.docs.insert(uri.clone(), 1);
            }
            Some(version) => {
                *version += 1;
                client.did_change(&uri, *version, text).await?;
            }
        }

        Ok((client, uri))
    }

    /// The workspace root this manager serves.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn absolute(&self, path: &Path) -> PathBuf {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        }
    }
}

/// A `file://` URI for a filesystem path.
///
/// Windows paths need care: separators become `/`, a drive-letter path gets the
/// empty-authority third slash (`file:///C:/...`), and spaces are
/// percent-encoded — servers reject `file://C:\Users\...` outright, which on
/// Windows silently broke document sync and every diagnostic keyed by URI.
fn to_uri(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/").replace(' ', "%20");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal LSP server in python3 that speaks real Content-Length framing
    /// over stdio: answers `initialize`, tracks the document version through
    /// `didOpen`/`didChange`, and answers `completion` with an item whose label
    /// encodes the synced version — so the test can prove the open→change
    /// sequencing actually reached a real subprocess.
    const FAKE_SERVER: &str = r#"
import json, sys

version = 0

def send(msg):
    body = json.dumps(msg).encode()
    sys.stdout.buffer.write(b"Content-Length: %d\r\n\r\n" % len(body))
    sys.stdout.buffer.write(body)
    sys.stdout.buffer.flush()

while True:
    line = sys.stdin.buffer.readline()
    if not line:
        break
    length = 0
    while line.strip():
        if line.lower().startswith(b"content-length:"):
            length = int(line.split(b":")[1])
        line = sys.stdin.buffer.readline()
    msg = json.loads(sys.stdin.buffer.read(length))
    method, mid = msg.get("method", ""), msg.get("id")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"completionProvider": {}}}})
    elif method == "textDocument/didOpen":
        version = msg["params"]["textDocument"]["version"]
    elif method == "textDocument/didChange":
        version = msg["params"]["textDocument"]["version"]
    elif method == "textDocument/completion":
        send({"jsonrpc": "2.0", "id": mid, "result": {"isIncomplete": False, "items": [
            {"label": "sync_v%d" % version, "kind": 3},
            {"label": "second", "kind": 6, "insertText": "second()"},
        ]}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
"#;

    fn fake_server_resolver(script: PathBuf) -> Resolver {
        Box::new(move |path: &Path| {
            (path.extension()?.to_str()? == "py").then(|| ServerSpec {
                language_id: "python".into(),
                command: "python3".into(),
                args: vec!["-u".into(), script.display().to_string()],
            })
        })
    }

    #[tokio::test]
    async fn completes_through_a_real_subprocess_and_syncs_versions() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_lsp.py");
        std::fs::write(&script, FAKE_SERVER).unwrap();

        let manager =
            LspManager::with_resolver(dir.path(), None, fake_server_resolver(script));

        // First request: spawns the server, initializes, opens the doc (v1).
        let items = manager
            .completions(Path::new("app.py"), "imp", Position::new(0, 3))
            .await
            .unwrap();
        assert_eq!(items[0].label, "sync_v1", "first call opened the doc");
        assert_eq!(items[1].insert, "second()");

        // Second request with new text: same server, didChange bumps to v2.
        let items = manager
            .completions(Path::new("app.py"), "impo", Position::new(0, 4))
            .await
            .unwrap();
        assert_eq!(items[0].label, "sync_v2", "second call synced the change");
    }

    #[test]
    fn uris_are_valid_on_both_unix_and_windows_shapes() {
        // Unix absolute path: authority-less file URI.
        assert_eq!(to_uri(Path::new("/home/x/main.rs")), "file:///home/x/main.rs");
        // Windows drive-letter path: forward slashes and the third slash.
        let uri = to_uri(Path::new(r"C:\Users\x\src\main.rs"));
        assert_eq!(uri, "file:///C:/Users/x/src/main.rs");
        // Spaces are percent-encoded so the URI parses.
        let uri = to_uri(Path::new(r"C:\My Projects\app.ts"));
        assert_eq!(uri, "file:///C:/My%20Projects/app.ts");
    }

    /// End-to-end against a real rust-analyzer, which rejects malformed URIs —
    /// exactly what broke on Windows. Ignored by default (needs rust-analyzer
    /// on PATH and a few seconds); run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires rust-analyzer on PATH"]
    async fn real_rust_analyzer_accepts_windows_uris() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("Cargo.toml"),
            "[package]\nname = \"smoke\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        let text = "fn main() { let value = 1; val }\n";
        std::fs::write(dir.path().join("src/main.rs"), text).unwrap();

        let manager = LspManager::new(dir.path(), None);
        // The old file://C:\... URIs made initialize/didOpen fail outright, so
        // an Ok here proves the server accepted the workspace and document.
        let result = manager
            .completions(Path::new("src/main.rs"), text, Position::new(0, 30))
            .await;
        assert!(
            result.is_ok(),
            "rust-analyzer rejected the session: {result:?}"
        );
    }

    #[tokio::test]
    async fn unsupported_files_and_missing_servers_degrade_gracefully() {
        let dir = tempfile::tempdir().unwrap();
        let manager = LspManager::new(dir.path(), None);

        // No server in the registry for .txt.
        let err = manager
            .completions(Path::new("notes.txt"), "", Position::new(0, 0))
            .await
            .unwrap_err();
        assert!(matches!(err, LspError::Unsupported(_)));

        // A registered server whose binary doesn't exist: Unavailable, and the
        // failure is cached so the second call fails the same way (fast).
        let manager = LspManager::with_resolver(
            dir.path(),
            None,
            Box::new(|_| {
                Some(ServerSpec {
                    language_id: "x".into(),
                    command: "definitely-not-installed-lsp".into(),
                    args: vec![],
                })
            }),
        );
        for _ in 0..2 {
            let err = manager
                .completions(Path::new("a.x"), "", Position::new(0, 0))
                .await
                .unwrap_err();
            match err {
                LspError::Unavailable { command, .. } => {
                    assert_eq!(command, "definitely-not-installed-lsp");
                }
                other => panic!("expected Unavailable, got {other:?}"),
            }
        }
    }
}
