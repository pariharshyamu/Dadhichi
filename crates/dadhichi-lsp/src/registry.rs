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
}
