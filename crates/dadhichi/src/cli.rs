//! Command-line argument parsing for the `dadhichi` binary.
//!
//! Kept deliberately dependency-free (no `clap`) so the entry point stays lean
//! and the parsing is trivially testable offline. Packaging conventions across
//! Debian, RPM, Homebrew, and Windows installers all expect a well-behaved
//! `--version` and `--help`, so those are first-class here.

/// The name advertised in `--version`/`--help`. Matches the installed binary.
pub const BIN_NAME: &str = "dadhichi";

/// The crate version, sourced from `Cargo.toml` at compile time so packaging
/// metadata and the running binary can never drift apart.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A parsed invocation of the CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Print the version string and exit.
    Version,
    /// Print usage help and exit.
    Help,
    /// Manage the encrypted credential vault, then exit.
    Vault(VaultCommand),
    /// Manage the skill library (import a manifest, list installed skills).
    Skill(SkillCommand),
    /// Delegate an isolated sub-task to a specialist that stages its work and
    /// commits it to the branch after verification/approval.
    Delegate {
        /// The specialist to run (e.g. `code-agent`).
        subagent: String,
        /// The self-contained task description.
        task: String,
    },
    /// Boot the kernel and run the agent against `goal`.
    Run { goal: Option<String> },
    /// Start an interactive multi-turn chat session (a persistent kernel and
    /// agent context that remembers the whole conversation until you exit).
    Chat,
}

/// A `dadhichi skill …` subcommand for managing the on-disk skill library that
/// the agent console and TUI load from `~/.dadhichi/skills`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillCommand {
    /// Validate and install the manifest at `path` into the user skills dir.
    Import { path: String },
    /// List the available skills (built-ins plus everything on disk).
    List,
    /// Print skill usage.
    Help,
}

/// Parse the tokens following `skill` into a [`SkillCommand`].
fn parse_skill(args: &[String]) -> SkillCommand {
    match args.first().map(String::as_str) {
        Some("import" | "add" | "install") => match args.get(1) {
            Some(path) if !path.is_empty() => SkillCommand::Import { path: path.clone() },
            _ => SkillCommand::Help,
        },
        Some("list" | "ls") => SkillCommand::List,
        _ => SkillCommand::Help,
    }
}

/// A `dadhichi vault …` subcommand. Populates the encrypted store the MCP
/// connectors read `${vault:NAME}` secrets from, so tokens never sit in
/// plaintext environment variables or config files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultCommand {
    /// Store a secret under `name` (its value is read from stdin).
    Set { name: String },
    /// List the stored secret names (values stay encrypted).
    List,
    /// Remove the secret under `name`.
    Remove { name: String },
    /// Print vault usage.
    Help,
}

/// Parse the tokens following `vault` into a [`VaultCommand`].
fn parse_vault(args: &[String]) -> VaultCommand {
    let named = |args: &[String]| match args.first() {
        Some(name) if !name.is_empty() => Some(name.clone()),
        _ => None,
    };
    match args.first().map(String::as_str) {
        Some("set") => match named(&args[1..]) {
            Some(name) => VaultCommand::Set { name },
            None => VaultCommand::Help,
        },
        Some("remove" | "rm") => match named(&args[1..]) {
            Some(name) => VaultCommand::Remove { name },
            None => VaultCommand::Help,
        },
        Some("list" | "ls") => VaultCommand::List,
        _ => VaultCommand::Help,
    }
}

/// Parse the tokens following `delegate` into a [`Command::Delegate`]. Takes the
/// specialist name, then the rest of the line as the task. Falls back to `Help`
/// when either is missing.
fn parse_delegate(args: &[String]) -> Command {
    match (args.first(), args.get(1)) {
        (Some(subagent), Some(_)) if !subagent.is_empty() => Command::Delegate {
            subagent: subagent.clone(),
            task: args[1..].join(" "),
        },
        _ => Command::Help,
    }
}

/// The one-line version string, e.g. `dadhichi 0.1.0`.
pub fn version_line() -> String {
    format!("{BIN_NAME} {VERSION}")
}

/// The full `--help` text.
pub fn help_text() -> String {
    format!(
        "{name} {version} — a modern, AI-first, agent-native IDE written in Rust.

USAGE:
    {name} [OPTIONS] GOAL...
    {name} chat
    {name} vault <set NAME | list | remove NAME>
    {name} skill <import PATH | list>
    {name} delegate <SUBAGENT> <TASK...>

ARGS:
    <GOAL...>  Natural-language goal for the agent to plan and execute. Multiple
               words are joined, so quoting is optional:
                   {name} write a tic tac toe game in html
               Context is remembered across runs in the same folder (stored in
               .dadhichi/session.json), so a follow-up like
                   {name} now add a scoreboard
               continues from the previous run. When omitted, a demo goal runs.

OPTIONS:
    -h, --help       Print this help text and exit.
    -V, --version    Print version information and exit.

SUBCOMMANDS:
    chat                 Start an interactive multi-turn session: one persistent
                         agent that remembers the whole conversation until you
                         type `exit` (or press Ctrl-D).
    vault set NAME       Store a secret under NAME (value read from stdin).
    vault list           List stored secret names (values stay encrypted).
    vault remove NAME    Delete the secret under NAME.
                         MCP servers in mcp.json reference these as
                         `${{vault:NAME}}`, keeping tokens out of plaintext env.
    skill import PATH    Validate a skill JSON manifest and install it into
                         ~/.dadhichi/skills so the agent console and TUI load it.
    skill list           List the available skills (built-ins plus on-disk).
    delegate SUB TASK    Run specialist SUB on TASK in an isolated overlay; it
                         stages file changes, which land on the current branch
                         (git commit) only after verification or your approval.

ENVIRONMENT:
    RUST_LOG              Tracing filter (e.g. `info`, `dadhichi=debug`). Defaults to `warn`.

    Credential vault:
    DADHICHI_VAULT_PASSPHRASE  Master passphrase that unlocks the vault.
    DADHICHI_VAULT             Vault file path (default: ~/.dadhichi/vault.json).

    Model providers (set any to use a real model; none = offline mock):
    ANTHROPIC_API_KEY    Use the Anthropic Messages API.
    OPENAI_API_KEY       Use OpenAI (OPENAI_BASE_URL overrides the endpoint for
                         Azure / vLLM / LM Studio / proxies).
    OPENROUTER_API_KEY   Use the OpenRouter aggregator.
    OLLAMA_HOST          Use a local Ollama server at this host (no key needed).
    OLLAMA_MODEL         The Ollama model name to run (e.g. llama3.2).
    DADHICHI_PROVIDER    Pick the default provider when several are configured
                         (anthropic | openai | openrouter | ollama | mock).
    DADHICHI_MODEL       The concrete model name to send to the default provider
                         (e.g. gpt-4o, claude-3-5-sonnet-latest, llama3.2).

The interactive terminal shell installs alongside this CLI as `dadhichi-tui`
(Ctrl-P for the command palette; `>` runs skills, `@` manages MCP servers).

Everything runs offline by default via the built-in mock model provider, so no
API keys are required to try it. See https://github.com/pariharshyamu/Dadhichi.",
        name = BIN_NAME,
        version = VERSION,
    )
}

/// Parse CLI arguments (excluding the program name).
///
/// The first recognised flag wins: `--help`/`-h` and `--version`/`-V` short
/// circuit before any goal is considered. A lone `--` terminates option
/// parsing so a goal may begin with a dash.
pub fn parse<I, S>(args: I) -> Command
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let args: Vec<String> = args.into_iter().map(Into::into).collect();

    // The `vault` and `skill` subcommands claim the whole invocation.
    if args.first().map(String::as_str) == Some("vault") {
        return Command::Vault(parse_vault(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("skill") {
        return Command::Skill(parse_skill(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("delegate") {
        return parse_delegate(&args[1..]);
    }
    // `dadhichi chat` (or `-i`/`--interactive`) starts the multi-turn REPL.
    if matches!(
        args.first().map(String::as_str),
        Some("chat" | "-i" | "--interactive" | "repl")
    ) {
        return Command::Chat;
    }

    let mut goal_words: Vec<String> = Vec::new();
    let mut options_done = false;

    for arg in args {
        if !options_done {
            match arg.as_str() {
                "-h" | "--help" => return Command::Help,
                "-V" | "--version" => return Command::Version,
                "--" => {
                    options_done = true;
                    continue;
                }
                _ => {}
            }
        }
        // Collect EVERY non-flag token into the goal and join them with spaces,
        // so an unquoted multi-word goal (`dadhichi write a tic tac toe game`)
        // behaves identically to a quoted one (`dadhichi "write a tic tac toe
        // game"`). Previously only the first token was kept, so the agent
        // received just "write" and had to ask for clarification.
        goal_words.push(arg);
    }

    let goal = if goal_words.is_empty() {
        None
    } else {
        Some(goal_words.join(" "))
    };

    Command::Run { goal }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_matches_cargo() {
        assert_eq!(
            version_line(),
            format!("dadhichi {}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn help_flags_short_circuit() {
        assert_eq!(parse(["--help"]), Command::Help);
        assert_eq!(parse(["-h"]), Command::Help);
        assert_eq!(parse(["-V"]), Command::Version);
        assert_eq!(parse(["--version"]), Command::Version);
    }

    #[test]
    fn help_wins_even_with_a_goal() {
        assert_eq!(parse(["do a thing", "--help"]), Command::Help);
    }

    #[test]
    fn bare_invocation_runs_the_demo() {
        assert_eq!(parse(Vec::<String>::new()), Command::Run { goal: None });
    }

    #[test]
    fn a_quoted_goal_is_captured() {
        assert_eq!(
            parse(["refactor the parser"]),
            Command::Run {
                goal: Some("refactor the parser".to_string())
            }
        );
    }

    #[test]
    fn an_unquoted_multi_word_goal_is_joined() {
        // The shell splits an unquoted goal into many argv tokens. They must be
        // rejoined so the agent receives the whole request, not just the first
        // word (the bug where `dadhichi write a tic tac toe game` became "write").
        assert_eq!(
            parse(["write", "a", "tic", "tac", "toe", "game", "in", "html"]),
            Command::Run {
                goal: Some("write a tic tac toe game in html".to_string())
            }
        );
    }

    #[test]
    fn chat_subcommand_and_aliases_parse() {
        assert_eq!(parse(["chat"]), Command::Chat);
        assert_eq!(parse(["-i"]), Command::Chat);
        assert_eq!(parse(["--interactive"]), Command::Chat);
        assert_eq!(parse(["repl"]), Command::Chat);
    }

    #[test]
    fn double_dash_lets_a_goal_start_with_a_dash() {
        assert_eq!(
            parse(["--", "--not-a-flag"]),
            Command::Run {
                goal: Some("--not-a-flag".to_string())
            }
        );
    }

    #[test]
    fn help_text_mentions_usage_and_version() {
        let text = help_text();
        assert!(text.contains("USAGE:"));
        assert!(text.contains(VERSION));
        assert!(text.contains("vault set NAME"));
    }

    #[test]
    fn vault_set_captures_the_name() {
        assert_eq!(
            parse(["vault", "set", "github"]),
            Command::Vault(VaultCommand::Set {
                name: "github".to_string()
            })
        );
    }

    #[test]
    fn vault_list_and_remove_parse() {
        assert_eq!(parse(["vault", "list"]), Command::Vault(VaultCommand::List));
        assert_eq!(parse(["vault", "ls"]), Command::Vault(VaultCommand::List));
        assert_eq!(
            parse(["vault", "rm", "openai"]),
            Command::Vault(VaultCommand::Remove {
                name: "openai".to_string()
            })
        );
    }

    #[test]
    fn delegate_captures_subagent_and_task() {
        assert_eq!(
            parse(["delegate", "code-agent", "add", "a", "ring", "buffer"]),
            Command::Delegate {
                subagent: "code-agent".to_string(),
                task: "add a ring buffer".to_string(),
            }
        );
        // Missing task falls back to help.
        assert_eq!(parse(["delegate", "code-agent"]), Command::Help);
        assert_eq!(parse(["delegate"]), Command::Help);
    }

    #[test]
    fn skill_import_captures_the_path() {
        assert_eq!(
            parse(["skill", "import", "./my-skill.json"]),
            Command::Skill(SkillCommand::Import {
                path: "./my-skill.json".to_string()
            })
        );
        // `add` and `install` are accepted aliases.
        assert_eq!(
            parse(["skill", "add", "x.json"]),
            Command::Skill(SkillCommand::Import {
                path: "x.json".to_string()
            })
        );
    }

    #[test]
    fn skill_list_and_bare_skill_parse() {
        assert_eq!(parse(["skill", "list"]), Command::Skill(SkillCommand::List));
        assert_eq!(parse(["skill"]), Command::Skill(SkillCommand::Help));
        assert_eq!(
            parse(["skill", "import"]),
            Command::Skill(SkillCommand::Help)
        );
    }

    #[test]
    fn vault_without_a_subcommand_or_name_is_help() {
        assert_eq!(parse(["vault"]), Command::Vault(VaultCommand::Help));
        assert_eq!(parse(["vault", "set"]), Command::Vault(VaultCommand::Help));
        assert_eq!(
            parse(["vault", "bogus"]),
            Command::Vault(VaultCommand::Help)
        );
    }

    #[test]
    fn a_goal_named_vault_still_works_after_double_dash() {
        // `--` forces goal parsing, so a literal goal can start with "vault".
        assert_eq!(
            parse(["--", "vault the crypt"]),
            Command::Run {
                goal: Some("vault the crypt".to_string())
            }
        );
    }
}
