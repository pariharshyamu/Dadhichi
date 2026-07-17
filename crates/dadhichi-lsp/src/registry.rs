//! Which language server speaks for which file.
//!
//! A static table maps file extensions to the *de facto standard* language
//! server for each prominent frontend and backend language. Servers are
//! external programs found on `PATH`; a missing binary degrades gracefully
//! (the manager reports it once instead of erroring every keystroke).
//!
//! TypeScript/JavaScript (and their React dialects) deliberately share one
//! server, as do C/C++ — the manager keys running instances by command, so
//! opening `app.tsx` and `util.js` spawns a single `typescript-language-server`.

use std::path::Path;

/// How to launch and identify the language server for one language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// The LSP `languageId` for documents of this language, e.g. `"typescript"`.
    pub language_id: String,
    /// The server executable, resolved from `PATH`.
    pub command: String,
    /// Arguments (most stdio servers need `--stdio` or nothing).
    pub args: Vec<String>,
}

impl ServerSpec {
    fn new(language_id: &str, command: &str, args: &[&str]) -> Self {
        Self {
            language_id: language_id.to_string(),
            command: command.to_string(),
            args: args.iter().map(|a| a.to_string()).collect(),
        }
    }
}

/// The language server for `path` within the workspace at `root` — like
/// [`server_for_path`], plus project-aware overrides: inside an Angular
/// workspace (a directory with `angular.json` between the file and the root),
/// TypeScript and template files are served by the Angular language server
/// (`ngserver`, from `@angular/language-server`), which layers template
/// intelligence over the plain TypeScript server.
pub fn server_for_path_in_root(root: &Path, path: &Path) -> Option<ServerSpec> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(ext, "ts" | "html")
        && let Some(ng_root) = angular_root(root, path)
    {
        let node_modules = ng_root.join("node_modules").display().to_string();
        let language_id = if ext == "ts" { "typescript" } else { "html" };
        return Some(ServerSpec::new(
            language_id,
            "ngserver",
            &[
                "--stdio",
                "--tsProbeLocations",
                &node_modules,
                "--ngProbeLocations",
                &node_modules,
            ],
        ));
    }
    server_for_path(path)
}

/// The nearest ancestor of `path` (up to and including `root`) holding an
/// `angular.json`, i.e. the Angular workspace the file belongs to.
fn angular_root(root: &Path, path: &Path) -> Option<std::path::PathBuf> {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let mut dir = abs.parent();
    while let Some(d) = dir {
        if d.join("angular.json").is_file() {
            return Some(d.to_path_buf());
        }
        if d == root {
            break;
        }
        dir = d.parent();
    }
    None
}

/// The language server for `path`, by extension — `None` for files no entry in
/// the table covers.
pub fn server_for_path(path: &Path) -> Option<ServerSpec> {
    let ext = path.extension()?.to_str()?;
    let spec = match ext {
        // ---- Backend ----
        "rs" => ServerSpec::new("rust", "rust-analyzer", &[]),
        "py" | "pyi" => ServerSpec::new("python", "pyright-langserver", &["--stdio"]),
        "go" => ServerSpec::new("go", "gopls", &[]),
        "java" => ServerSpec::new("java", "jdtls", &[]),
        "c" | "h" => ServerSpec::new("c", "clangd", &[]),
        "cpp" | "cc" | "cxx" | "hpp" | "hh" => ServerSpec::new("cpp", "clangd", &[]),
        "cs" => ServerSpec::new("csharp", "omnisharp", &["-lsp"]),
        "rb" => ServerSpec::new("ruby", "solargraph", &["stdio"]),
        "php" => ServerSpec::new("php", "intelephense", &["--stdio"]),
        "kt" | "kts" => ServerSpec::new("kotlin", "kotlin-language-server", &[]),
        // ---- Frontend ----
        "ts" | "mts" | "cts" => {
            ServerSpec::new("typescript", "typescript-language-server", &["--stdio"])
        }
        "tsx" => ServerSpec::new("typescriptreact", "typescript-language-server", &["--stdio"]),
        "js" | "mjs" | "cjs" => {
            ServerSpec::new("javascript", "typescript-language-server", &["--stdio"])
        }
        "jsx" => ServerSpec::new("javascriptreact", "typescript-language-server", &["--stdio"]),
        "html" => ServerSpec::new("html", "vscode-html-language-server", &["--stdio"]),
        "css" | "scss" | "less" => ServerSpec::new("css", "vscode-css-language-server", &["--stdio"]),
        "json" | "jsonc" => ServerSpec::new("json", "vscode-json-language-server", &["--stdio"]),
        "vue" => ServerSpec::new("vue", "vue-language-server", &["--stdio"]),
        "svelte" => ServerSpec::new("svelte", "svelteserver", &["--stdio"]),
        _ => return None,
    };
    Some(spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_prominent_languages_to_their_servers() {
        let spec = server_for_path(Path::new("src/main.rs")).unwrap();
        assert_eq!(spec.language_id, "rust");
        assert_eq!(spec.command, "rust-analyzer");

        let spec = server_for_path(Path::new("web/app.tsx")).unwrap();
        assert_eq!(spec.language_id, "typescriptreact");
        assert_eq!(spec.command, "typescript-language-server");

        // JS and TS share the same server process.
        let js = server_for_path(Path::new("a.js")).unwrap();
        let ts = server_for_path(Path::new("b.ts")).unwrap();
        assert_eq!(js.command, ts.command);

        // C and C++ share clangd.
        assert_eq!(server_for_path(Path::new("x.c")).unwrap().command, "clangd");
        assert_eq!(
            server_for_path(Path::new("x.cpp")).unwrap().command,
            "clangd"
        );

        assert_eq!(
            server_for_path(Path::new("api.py")).unwrap().command,
            "pyright-langserver"
        );
        assert_eq!(server_for_path(Path::new("m.go")).unwrap().command, "gopls");

        // Unknown extensions and extension-less files have no server.
        assert!(server_for_path(Path::new("notes.txt")).is_none());
        assert!(server_for_path(Path::new("Makefile")).is_none());
    }

    #[test]
    fn angular_workspaces_get_the_angular_language_server() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("angular.json"), "{}").unwrap();
        std::fs::create_dir_all(root.join("src/app")).unwrap();

        // Components and templates inside the workspace resolve to ngserver,
        // pointed at the workspace's node_modules for its probe locations.
        let spec = server_for_path_in_root(root, Path::new("src/app/app.component.ts")).unwrap();
        assert_eq!(spec.command, "ngserver");
        assert_eq!(spec.language_id, "typescript");
        assert!(spec.args.iter().any(|a| a.contains("node_modules")));

        let spec = server_for_path_in_root(root, Path::new("src/app/app.component.html")).unwrap();
        assert_eq!(spec.command, "ngserver");
        assert_eq!(spec.language_id, "html");

        // Other languages in the same workspace keep their own servers.
        let spec = server_for_path_in_root(root, Path::new("tools/gen.go")).unwrap();
        assert_eq!(spec.command, "gopls");

        // Outside an Angular workspace, .ts falls back to the TS server.
        let plain = tempfile::tempdir().unwrap();
        let spec = server_for_path_in_root(plain.path(), Path::new("src/app.ts")).unwrap();
        assert_eq!(spec.command, "typescript-language-server");
    }
}
