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
    /// Boot the kernel and run the agent against `goal`.
    Run { goal: Option<String> },
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
    {name} [OPTIONS] [GOAL]

ARGS:
    <GOAL>    Natural-language goal for the built-in agent to plan and execute.
              When omitted, a demonstration goal is used.

OPTIONS:
    -h, --help       Print this help text and exit.
    -V, --version    Print version information and exit.

ENVIRONMENT:
    RUST_LOG              Tracing filter (e.g. `info`, `dadhichi=debug`). Defaults to `warn`.

    Model providers (set any to use a real model; none = offline mock):
    ANTHROPIC_API_KEY    Use the Anthropic Messages API.
    OPENAI_API_KEY       Use OpenAI (OPENAI_BASE_URL overrides the endpoint for
                         Azure / vLLM / LM Studio / proxies).
    OPENROUTER_API_KEY   Use the OpenRouter aggregator.
    OLLAMA_HOST          Use a local Ollama server at this host (no key needed).
    DADHICHI_PROVIDER    Pick the default provider when several are configured
                         (anthropic | openai | openrouter | ollama | mock).

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
    let mut goal: Option<String> = None;
    let mut options_done = false;

    for raw in args {
        let arg: String = raw.into();
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
        // First non-flag token is the goal; later tokens are ignored so a
        // quoted multi-word goal and an unquoted one behave the same.
        if goal.is_none() {
            goal = Some(arg);
        }
    }

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
    fn a_goal_is_captured() {
        assert_eq!(
            parse(["refactor the parser"]),
            Command::Run {
                goal: Some("refactor the parser".to_string())
            }
        );
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
    }
}
