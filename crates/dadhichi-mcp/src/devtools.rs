//! Native full-stack development tools.
//!
//! These are higher-level, *discoverable* capabilities layered over the same
//! sandboxed shell the [`TerminalTool`](crate::shell::TerminalTool) uses: a
//! project **scaffolder**, a **build** runner, a **test** runner, and a database
//! **query** tool. Encoding the right command per stack (React/Vite, Angular,
//! Express, FastAPI, Axum; npm/pnpm/cargo/python; sqlite/psql) as named tools
//! lets the model pick them reliably instead of hand-assembling shell lines —
//! while keeping the tool count small so tool-selection accuracy stays high.
//!
//! Every tool runs commands pinned to the sandbox `root` and requires
//! [`Permission::RunCommands`], so each call passes through the same approval
//! gate as any other shell command.

use crate::tool::{Permission, Tool, ToolError, ToolResult, ToolSpec};
use async_trait::async_trait;
use std::path::PathBuf;

/// Run a shell `command` pinned to `cwd`, returning the captured result as JSON.
/// Shared by the dev tools so they behave exactly like the terminal tool.
async fn run_in(cwd: &PathBuf, command: &str) -> ToolResult {
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = tokio::process::Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(cwd);
    let output = cmd
        .output()
        .await
        .map_err(|e| ToolError::Execution(format!("failed to spawn `{command}`: {e}")))?;
    Ok(serde_json::json!({
        "command": command,
        "status": output.status.code(),
        "success": output.status.success(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

/// Scaffolds a new frontend or backend project in the workspace.
#[derive(Debug)]
pub struct ScaffoldTool {
    root: PathBuf,
}

impl ScaffoldTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "project.scaffold";

    /// Scaffold projects under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The shell command that scaffolds `stack` into a directory `name`.
    /// Returns `None` for an unknown stack (the caller turns that into an error
    /// listing the supported stacks).
    fn command_for(stack: &str, name: &str) -> Option<String> {
        // Non-interactive flags are chosen so scaffolding never blocks on a prompt.
        let cmd = match stack {
            // Frontend
            "react" | "react-vite" | "vite-react" => format!(
                "npm create vite@latest {name} -- --template react-ts --no-interactive || \
                 npm create vite@latest {name} -- --template react"
            ),
            "react-js" => format!("npm create vite@latest {name} -- --template react"),
            "vue" => format!("npm create vite@latest {name} -- --template vue-ts"),
            "svelte" => format!("npm create vite@latest {name} -- --template svelte-ts"),
            "vanilla" | "html" | "html-js" => format!(
                "npm create vite@latest {name} -- --template vanilla-ts || \
                 npm create vite@latest {name} -- --template vanilla"
            ),
            "angular" => format!(
                "npx --yes @angular/cli@latest new {name} --defaults --skip-git --skip-install=false"
            ),
            "next" | "nextjs" => format!(
                "npx --yes create-next-app@latest {name} --ts --eslint --app --use-npm --yes"
            ),
            // Backend
            "express" | "node" | "node-express" => format!(
                "mkdir {name} && cd {name} && npm init -y && npm install express && \
                 npm install --save-dev nodemon"
            ),
            "fastapi" | "python-fastapi" => format!(
                "mkdir {name} && cd {name} && python -m venv .venv && \
                 (.venv/bin/pip install fastapi uvicorn || .venv/Scripts/pip install fastapi uvicorn)"
            ),
            "flask" => format!(
                "mkdir {name} && cd {name} && python -m venv .venv && \
                 (.venv/bin/pip install flask || .venv/Scripts/pip install flask)"
            ),
            "axum" | "rust-axum" => format!(
                "cargo new {name} && cd {name} && cargo add axum tokio --features tokio/full"
            ),
            "rust" => format!("cargo new {name}"),
            _ => return None,
        };
        Some(cmd)
    }

    /// The list of supported stack identifiers, for error messages and docs.
    fn supported() -> &'static str {
        "react, react-js, vue, svelte, vanilla/html, angular, next, express, fastapi, flask, \
         axum, rust"
    }
}

#[async_trait]
impl Tool for ScaffoldTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: format!(
                "Scaffold a new frontend or backend project in the workspace. Supported stacks: {}. \
                 Runs the appropriate create/init command and returns its output.",
                Self::supported()
            ),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "stack": { "type": "string", "description": "Which stack to scaffold, e.g. react, angular, express, fastapi, axum." },
                    "name": { "type": "string", "description": "Directory name for the new project." }
                },
                "required": ["stack", "name"]
            }),
            permissions: vec![Permission::RunCommands],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let stack = args
            .get("stack")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `stack`".into()))?;
        let name = args
            .get("name")
            .and_then(|n| n.as_str())
            .filter(|n| !n.is_empty() && !n.contains(['/', '\\', '.']))
            .ok_or_else(|| {
                ToolError::InvalidArguments(
                    "missing or invalid `name` (a bare directory name, no path separators)".into(),
                )
            })?;
        let command = Self::command_for(stack, name).ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "unsupported stack `{stack}`; supported: {}",
                Self::supported()
            ))
        })?;
        run_in(&self.root, &command).await
    }
}

/// Builds a project by auto-detecting its toolchain (or an explicit one).
#[derive(Debug)]
pub struct BuildTool {
    root: PathBuf,
}

impl BuildTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "project.build";

    /// Build projects under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The build command for `tool` in directory `dir` (relative to root).
    fn command_for(tool: &str, dir: &str) -> Option<String> {
        let cd = if dir.is_empty() {
            String::new()
        } else {
            format!("cd {dir} && ")
        };
        let cmd = match tool {
            "npm" => format!("{cd}npm install && npm run build --if-present"),
            "pnpm" => format!("{cd}pnpm install && pnpm build"),
            "yarn" => format!("{cd}yarn install && yarn build"),
            "cargo" | "rust" => format!("{cd}cargo build"),
            "python" | "pip" => format!(
                "{cd}(.venv/bin/pip install -r requirements.txt || \
                 .venv/Scripts/pip install -r requirements.txt || pip install -r requirements.txt)"
            ),
            "make" => format!("{cd}make"),
            _ => return None,
        };
        Some(cmd)
    }
}

#[async_trait]
impl Tool for BuildTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Install dependencies and build a project. Give the toolchain (npm, \
                          pnpm, yarn, cargo, python, make) and an optional subdirectory. Use this \
                          to verify code compiles after changes."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "description": "npm, pnpm, yarn, cargo, python, or make." },
                    "dir": { "type": "string", "description": "Optional subdirectory to build in." }
                },
                "required": ["tool"]
            }),
            permissions: vec![Permission::RunCommands],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let tool = args
            .get("tool")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `tool`".into()))?;
        let dir = args.get("dir").and_then(|d| d.as_str()).unwrap_or("");
        let command = Self::command_for(tool, dir).ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "unsupported build tool `{tool}`; use npm, pnpm, yarn, cargo, python, or make"
            ))
        })?;
        run_in(&self.root, &command).await
    }
}

/// Runs a project's test suite via its toolchain.
#[derive(Debug)]
pub struct TestRunnerTool {
    root: PathBuf,
}

impl TestRunnerTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "project.test";

    /// Run tests under `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The test command for `tool` in directory `dir`.
    fn command_for(tool: &str, dir: &str) -> Option<String> {
        let cd = if dir.is_empty() {
            String::new()
        } else {
            format!("cd {dir} && ")
        };
        let cmd = match tool {
            "npm" => format!("{cd}npm test"),
            "pnpm" => format!("{cd}pnpm test"),
            "yarn" => format!("{cd}yarn test"),
            "cargo" | "rust" => format!("{cd}cargo test"),
            "pytest" | "python" => format!(
                "{cd}(.venv/bin/pytest || .venv/Scripts/pytest || pytest)"
            ),
            "jest" => format!("{cd}npx --yes jest"),
            "vitest" => format!("{cd}npx --yes vitest run"),
            _ => return None,
        };
        Some(cmd)
    }
}

#[async_trait]
impl Tool for TestRunnerTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Run a project's test suite. Give the runner (npm, pnpm, yarn, cargo, \
                          pytest, jest, vitest) and an optional subdirectory. Use this to verify \
                          behaviour after changes."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "tool": { "type": "string", "description": "npm, pnpm, yarn, cargo, pytest, jest, or vitest." },
                    "dir": { "type": "string", "description": "Optional subdirectory to test in." }
                },
                "required": ["tool"]
            }),
            permissions: vec![Permission::RunCommands],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let tool = args
            .get("tool")
            .and_then(|t| t.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `tool`".into()))?;
        let dir = args.get("dir").and_then(|d| d.as_str()).unwrap_or("");
        let command = Self::command_for(tool, dir).ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "unsupported test runner `{tool}`; use npm, pnpm, yarn, cargo, pytest, jest, or vitest"
            ))
        })?;
        run_in(&self.root, &command).await
    }
}

/// Runs a SQL query or migration against a SQLite or Postgres database.
#[derive(Debug)]
pub struct DbQueryTool {
    root: PathBuf,
}

impl DbQueryTool {
    /// The registry name for this tool.
    pub const NAME: &'static str = "db.query";

    /// Run queries from `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Build the shell command that runs `sql` against `engine`/`target`.
    fn command_for(engine: &str, target: &str, sql: &str) -> Option<String> {
        // The SQL is passed via a heredoc-free single argument; escape double
        // quotes so it survives the shell. Callers keep statements single-line.
        let escaped = sql.replace('"', "\\\"");
        let cmd = match engine {
            // SQLite: `target` is a database file path in the workspace.
            "sqlite" | "sqlite3" => format!("sqlite3 \"{target}\" \"{escaped}\""),
            // Postgres: `target` is a connection string (or a psql-compatible URL).
            "postgres" | "psql" | "postgresql" => {
                format!("psql \"{target}\" -c \"{escaped}\"")
            }
            _ => return None,
        };
        Some(cmd)
    }
}

#[async_trait]
impl Tool for DbQueryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: Self::NAME.into(),
            description: "Run a SQL statement (query or migration) against a SQLite file or a \
                          Postgres connection. Use it to create tables, run migrations, and \
                          verify data. Requires the sqlite3/psql client on PATH."
                .into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "engine": { "type": "string", "description": "sqlite or postgres." },
                    "target": { "type": "string", "description": "For sqlite, the .db file path; for postgres, the connection string/URL." },
                    "sql": { "type": "string", "description": "The SQL statement to execute." }
                },
                "required": ["engine", "target", "sql"]
            }),
            permissions: vec![Permission::RunCommands],
        }
    }

    async fn invoke(&self, args: serde_json::Value) -> ToolResult {
        let engine = args
            .get("engine")
            .and_then(|e| e.as_str())
            .ok_or_else(|| ToolError::InvalidArguments("missing `engine`".into()))?;
        let target = args
            .get("target")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `target`".into()))?;
        let sql = args
            .get("sql")
            .and_then(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| ToolError::InvalidArguments("missing `sql`".into()))?;
        let command = Self::command_for(engine, target, sql).ok_or_else(|| {
            ToolError::InvalidArguments(format!(
                "unsupported db engine `{engine}`; use sqlite or postgres"
            ))
        })?;
        run_in(&self.root, &command).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scaffold_maps_known_stacks_and_rejects_unknown() {
        assert!(ScaffoldTool::command_for("react", "app")
            .unwrap()
            .contains("vite"));
        assert!(ScaffoldTool::command_for("angular", "app")
            .unwrap()
            .contains("@angular/cli"));
        assert!(ScaffoldTool::command_for("fastapi", "api")
            .unwrap()
            .contains("fastapi"));
        assert!(ScaffoldTool::command_for("axum", "api")
            .unwrap()
            .contains("axum"));
        assert!(ScaffoldTool::command_for("nope", "app").is_none());
    }

    #[test]
    fn build_and_test_map_toolchains() {
        assert!(BuildTool::command_for("cargo", "").unwrap().contains("cargo build"));
        assert!(BuildTool::command_for("npm", "web").unwrap().contains("cd web"));
        assert!(BuildTool::command_for("nope", "").is_none());
        assert!(TestRunnerTool::command_for("pytest", "").unwrap().contains("pytest"));
        assert!(TestRunnerTool::command_for("cargo", "").unwrap().contains("cargo test"));
        assert!(TestRunnerTool::command_for("nope", "").is_none());
    }

    #[test]
    fn db_query_builds_engine_specific_commands_and_escapes() {
        let s = DbQueryTool::command_for("sqlite", "app.db", "SELECT 1").unwrap();
        assert!(s.starts_with("sqlite3"));
        let p = DbQueryTool::command_for("postgres", "postgres://localhost/db", "SELECT 1").unwrap();
        assert!(p.starts_with("psql"));
        // Embedded double quotes are escaped so the shell command stays intact.
        let e = DbQueryTool::command_for("sqlite", "app.db", "INSERT INTO t VALUES(\"x\")").unwrap();
        assert!(e.contains("\\\""));
        assert!(DbQueryTool::command_for("mongo", "x", "y").is_none());
    }

    #[tokio::test]
    async fn scaffold_rejects_bad_name() {
        let tool = ScaffoldTool::new(".");
        let err = tool
            .invoke(serde_json::json!({ "stack": "react", "name": "../escape" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }
}
