//! A built-in catalogue of well-known MCP servers.
//!
//! Hand-writing an `mcp.json` means knowing each server's launch command, its
//! npm/PyPI package, which secret it needs, and which permissions to grant. The
//! catalogue turns that into a pick-list: a [`Connector`] carries everything
//! needed to materialise an [`McpServerConfig`], so a frontend can offer
//! "connect GitHub" without the user typing any of it.
//!
//! Secrets are never baked in. A connector that needs one declares it as an
//! [`SecretRequirement`] whose config value is a `${env:NAME}` placeholder — the
//! same placeholder the launcher resolves from the environment or the credential
//! vault at connect time.

use crate::connector::McpServerConfig;
use crate::tool::Permission;
use std::collections::BTreeMap;

/// A secret a connector needs before it can run, e.g. an API token. The `env`
/// name is where the value is read from (process env or vault); `placeholder`
/// is what gets written into the server's config so it resolves at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRequirement {
    /// The environment variable the server process receives.
    pub var: &'static str,
    /// The `${...}` reference written into the config for that variable.
    pub placeholder: &'static str,
    /// A one-line human description of what the secret is.
    pub description: &'static str,
}

/// A ready-to-use MCP server preset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connector {
    /// Stable catalogue id, also the default local server name (e.g. `github`).
    pub id: &'static str,
    /// One-line description of what the server does.
    pub description: &'static str,
    /// The executable to launch (`npx`, `uvx`, …).
    pub command: &'static str,
    /// Base arguments. A `{root}` token is replaced with the workspace path.
    pub args: &'static [&'static str],
    /// The permission envelope stamped onto the server's tools.
    pub grants: &'static [Permission],
    /// Secrets the server needs, if any.
    pub secrets: &'static [SecretRequirement],
    /// Where to read more (the package or docs URL).
    pub homepage: &'static str,
}

impl Connector {
    /// Whether this connector needs one or more secrets set before it will run.
    pub fn needs_secrets(&self) -> bool {
        !self.secrets.is_empty()
    }

    /// Materialise a launchable [`McpServerConfig`], substituting `{root}` in the
    /// arguments with `workspace_root` (so a filesystem server is scoped to the
    /// open project) and wiring each declared secret to its `${...}` placeholder.
    pub fn to_config(&self, workspace_root: &str) -> McpServerConfig {
        let args = self
            .args
            .iter()
            .map(|a| a.replace("{root}", workspace_root))
            .collect();
        let mut env = BTreeMap::new();
        for secret in self.secrets {
            env.insert(secret.var.to_string(), secret.placeholder.to_string());
        }
        McpServerConfig {
            command: self.command.to_string(),
            args,
            env,
            url: None,
            headers: BTreeMap::new(),
            grants: self.grants.to_vec(),
            enabled: true,
        }
    }
}

/// The built-in connectors, in catalogue order. These are the reference MCP
/// servers published by the Model Context Protocol project, launched via their
/// standard `npx`/`uvx` invocations.
pub fn builtin_connectors() -> &'static [Connector] {
    const NPX_MCP: &str = "npx";
    const UVX: &str = "uvx";
    &[
        Connector {
            id: "filesystem",
            description: "Read and write files under the workspace directory",
            command: NPX_MCP,
            args: &["-y", "@modelcontextprotocol/server-filesystem", "{root}"],
            grants: &[Permission::ReadWorkspace, Permission::WriteWorkspace],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/filesystem",
        },
        Connector {
            id: "github",
            description: "Query and act on GitHub repositories, issues, and PRs",
            command: NPX_MCP,
            args: &["-y", "@modelcontextprotocol/server-github"],
            grants: &[Permission::Network],
            secrets: &[SecretRequirement {
                var: "GITHUB_PERSONAL_ACCESS_TOKEN",
                placeholder: "${env:GITHUB_PERSONAL_ACCESS_TOKEN}",
                description: "A GitHub personal access token",
            }],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/github",
        },
        Connector {
            id: "memory",
            description: "A persistent knowledge-graph memory across sessions",
            command: NPX_MCP,
            args: &["-y", "@modelcontextprotocol/server-memory"],
            grants: &[],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/memory",
        },
        Connector {
            id: "sequential-thinking",
            description: "Structured step-by-step reasoning as a tool",
            command: NPX_MCP,
            args: &["-y", "@modelcontextprotocol/server-sequential-thinking"],
            grants: &[],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/sequentialthinking",
        },
        Connector {
            id: "everything",
            description: "The MCP reference server (tools, prompts, resources) for testing",
            command: NPX_MCP,
            args: &["-y", "@modelcontextprotocol/server-everything"],
            grants: &[],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/everything",
        },
        Connector {
            id: "fetch",
            description: "Fetch a URL and return its content as Markdown",
            command: UVX,
            args: &["mcp-server-fetch"],
            grants: &[Permission::Network],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/fetch",
        },
        Connector {
            id: "playwright",
            description: "Drive a real browser: navigate, click, fill, snapshot, screenshot (for E2E tests and verifying web apps)",
            command: NPX_MCP,
            args: &["-y", "@playwright/mcp@latest"],
            grants: &[Permission::Network],
            secrets: &[],
            homepage: "https://github.com/microsoft/playwright-mcp",
        },
        Connector {
            id: "git",
            description: "Read, search, and manipulate a local Git repository",
            command: UVX,
            args: &["mcp-server-git", "--repository", "{root}"],
            grants: &[Permission::ReadWorkspace],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/git",
        },
        Connector {
            id: "sqlite",
            description: "Inspect and query a local SQLite database (schema, tables, SQL)",
            command: UVX,
            args: &["mcp-server-sqlite", "--db-path", "{root}/dadhichi.db"],
            grants: &[Permission::ReadWorkspace, Permission::WriteWorkspace],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/sqlite",
        },
        Connector {
            id: "time",
            description: "The current time and timezone conversions",
            command: UVX,
            args: &["mcp-server-time"],
            grants: &[],
            secrets: &[],
            homepage: "https://github.com/modelcontextprotocol/servers/tree/main/src/time",
        },
    ]
}

/// Look up a connector by its catalogue id.
pub fn connector(id: &str) -> Option<&'static Connector> {
    builtin_connectors().iter().find(|c| c.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_connector_has_a_unique_id_and_launch_command() {
        let connectors = builtin_connectors();
        let mut ids: Vec<_> = connectors.iter().map(|c| c.id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), connectors.len(), "ids must be unique");
        for c in connectors {
            assert!(!c.command.is_empty(), "{} has no command", c.id);
            assert!(!c.description.is_empty(), "{} has no description", c.id);
        }
    }

    #[test]
    fn lookup_finds_known_and_rejects_unknown() {
        assert!(connector("github").is_some());
        assert!(connector("does-not-exist").is_none());
    }

    #[test]
    fn filesystem_config_scopes_to_the_workspace_root() {
        let cfg = connector("filesystem").unwrap().to_config("/home/me/proj");
        assert_eq!(cfg.command, "npx");
        assert_eq!(cfg.args.last().unwrap(), "/home/me/proj");
        assert!(cfg.enabled);
        assert!(cfg.env.is_empty());
    }

    #[test]
    fn github_config_wires_the_token_placeholder() {
        let c = connector("github").unwrap();
        assert!(c.needs_secrets());
        let cfg = c.to_config("/anything");
        assert_eq!(
            cfg.env
                .get("GITHUB_PERSONAL_ACCESS_TOKEN")
                .map(String::as_str),
            Some("${env:GITHUB_PERSONAL_ACCESS_TOKEN}")
        );
        assert_eq!(cfg.grants, vec![Permission::Network]);
    }

    #[test]
    fn keyless_tier1_connectors_are_present_and_need_no_secrets() {
        for id in ["playwright", "sqlite", "time"] {
            let c = connector(id).unwrap_or_else(|| panic!("{id} in catalogue"));
            assert!(!c.needs_secrets(), "{id} is keyless");
            assert!(c.to_config("/proj").env.is_empty(), "{id} sets no env");
        }
        // Playwright drives a browser, so it needs the network.
        assert_eq!(
            connector("playwright").unwrap().grants,
            &[Permission::Network]
        );
        // The SQLite db path is scoped under the workspace root.
        let sqlite = connector("sqlite").unwrap().to_config("/home/me/proj");
        assert!(
            sqlite.args.iter().any(|a| a == "/home/me/proj/dadhichi.db"),
            "sqlite db path scoped to root: {:?}",
            sqlite.args
        );
        // Time needs nothing at all.
        assert!(connector("time").unwrap().grants.is_empty());
    }
}
