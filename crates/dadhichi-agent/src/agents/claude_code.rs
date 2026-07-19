//! Claude Code as an agent backend.
//!
//! When the user flips the model picker to `claude-code`, goals are executed
//! by Anthropic's agentic CLI in headless mode (`claude -p <goal>`) instead of
//! the built-in ReAct loop — Claude Code plans, calls its own tools, and edits
//! the workspace directly. Its `stream-json` event stream is translated onto
//! the kernel bus as the same `agent.*` topics every frontend already renders,
//! so the console shows its tool calls and messages live.
//!
//! The `claude` binary is resolved like the MCP connector resolves it
//! ([`resolve_launcher`]): PATH, `CLAUDE_CODE_EXECPATH`, or the editor
//! extension's bundled binary.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
use crate::memory::Tier;
use crate::plan::{Plan, Step};
use async_trait::async_trait;
use dadhichi_mcp::resolve_launcher;
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, BufReader};

/// Runs goals through the Claude Code CLI in the workspace.
#[derive(Debug)]
pub struct ClaudeCodeAgent {
    root: PathBuf,
}

impl ClaudeCodeAgent {
    /// The orchestrator name (and the model-picker id) for this backend.
    pub const NAME: &'static str = "claude-code";

    /// An agent that runs Claude Code inside `root`.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

/// One event distilled from a `stream-json` line, ready for the bus.
#[derive(Debug, PartialEq)]
enum StreamEvent {
    /// Claude called a tool: `(tool name, input)`.
    Tool(String, serde_json::Value),
    /// Claude said something.
    Text(String),
    /// The run finished: `(result text, is_error)`.
    Result(String, bool),
    /// Anything else (init, tool results, …) — ignored.
    Other,
}

/// Distill one JSONL line of `claude --output-format stream-json` output.
fn parse_stream_line(line: &str) -> StreamEvent {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return StreamEvent::Other;
    };
    match value.get("type").and_then(|t| t.as_str()) {
        Some("assistant") => {
            let blocks = value
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            // A message can hold several blocks; surface the first meaningful
            // one (tool_use beats text — the text is usually its preamble).
            for block in &blocks {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    let name = block
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("tool")
                        .to_string();
                    let input = block.get("input").cloned().unwrap_or_default();
                    return StreamEvent::Tool(name, input);
                }
            }
            for block in &blocks {
                if let Some(text) = block.get("text").and_then(|t| t.as_str())
                    && !text.trim().is_empty()
                {
                    return StreamEvent::Text(text.to_string());
                }
            }
            StreamEvent::Other
        }
        Some("result") => {
            let text = value
                .get("result")
                .and_then(|r| r.as_str())
                .unwrap_or("(no result)")
                .to_string();
            let is_error = value
                .get("is_error")
                .and_then(|e| e.as_bool())
                .unwrap_or(false);
            StreamEvent::Result(text, is_error)
        }
        _ => StreamEvent::Other,
    }
}

#[async_trait]
impl Agent for ClaudeCodeAgent {
    fn name(&self) -> &str {
        Self::NAME
    }

    fn description(&self) -> &str {
        "Delegates the goal to the Claude Code CLI (headless), streaming its tool calls and \
         messages into the console."
    }

    async fn run(&self, goal: &str, ctx: &mut AgentContext) -> Result<AgentOutcome, AgentError> {
        ctx.emit("agent.status", serde_json::json!({ "status": "planning" }));
        let mut plan = Plan::new(goal)
            .step(Step::using("delegate to Claude Code", "claude"))
            .step(Step::think("report the result"));
        ctx.emit_plan(&plan);
        let step_run = plan.steps[0].id;
        let step_report = plan.steps[1].id;
        ctx.memory.remember(Tier::Working, format!("goal: {goal}"));

        let command = resolve_launcher("claude");
        ctx.emit("agent.status", serde_json::json!({ "status": "running" }));
        ctx.emit(
            "agent.delegated",
            serde_json::json!({ "agent": "claude-code", "task": goal }),
        );

        use std::process::Stdio;
        let spawn = |program: &str, via_cmd: bool| {
            let mut cmd = if via_cmd {
                let mut c = tokio::process::Command::new("cmd");
                c.arg("/C").arg(program);
                c
            } else {
                tokio::process::Command::new(program)
            };
            cmd.args([
                "-p",
                goal,
                "--output-format",
                "stream-json",
                "--verbose",
                // Headless runs can't answer prompts; allow edits inside the
                // workspace while still refusing anything more dangerous.
                "--permission-mode",
                "acceptEdits",
            ])
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // An abandoned run must not leave a headless claude behind.
            .kill_on_drop(true);
            cmd.spawn()
        };
        let mut child = match spawn(&command, false) {
            Ok(child) => child,
            Err(err) if cfg!(windows) && err.kind() == std::io::ErrorKind::NotFound => {
                spawn(&command, true).map_err(|e| {
                    AgentError::Model(format!("cannot launch claude: {e}"))
                })?
            }
            Err(err) => {
                return Err(AgentError::Model(format!(
                    "cannot launch claude ({command}): {err}. Install Claude Code or open it \
                     through your editor once so its binary can be found."
                )));
            }
        };

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AgentError::Model("claude: no stdout".into()))?;
        let mut lines = BufReader::new(stdout).lines();

        let mut result: Option<(String, bool)> = None;
        let mut last_text = String::new();
        while let Ok(Some(line)) = lines.next_line().await {
            match parse_stream_line(&line) {
                StreamEvent::Tool(name, input) => {
                    ctx.emit(
                        "agent.tool",
                        serde_json::json!({ "tool": name, "args": input }),
                    );
                }
                StreamEvent::Text(text) => {
                    ctx.emit(
                        "agent.message",
                        serde_json::json!({ "role": "assistant", "content": text }),
                    );
                    last_text = text;
                }
                StreamEvent::Result(text, is_error) => result = Some((text, is_error)),
                StreamEvent::Other => {}
            }
        }
        let status_code = child.wait().await.ok();

        plan.complete(step_run);
        ctx.emit_plan(&plan);

        let (summary, failed) = match result {
            Some((text, is_error)) => (text, is_error),
            None => {
                // The stream ended without a result record — surface stderr.
                let succeeded = status_code.map(|s| s.success()).unwrap_or(false);
                if succeeded && !last_text.is_empty() {
                    (last_text, false)
                } else {
                    (
                        format!(
                            "claude exited without a result (status: {status_code:?}). Is the \
                             CLI logged in? Try running `claude` once interactively."
                        ),
                        true,
                    )
                }
            }
        };
        ctx.memory.remember(Tier::Conversation, summary.clone());
        ctx.emit(
            "agent.message",
            serde_json::json!({ "role": "assistant", "content": summary }),
        );
        plan.complete(step_report);
        ctx.emit_plan(&plan);

        Ok(AgentOutcome {
            status: if failed {
                AgentStatus::Failed
            } else {
                AgentStatus::Completed
            },
            summary,
            confidence: if failed { 0.1 } else { 0.85 },
            plan,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distills_stream_json_lines() {
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls"}}]}}"#;
        assert_eq!(
            parse_stream_line(tool),
            StreamEvent::Tool("Bash".into(), serde_json::json!({ "command": "ls" }))
        );

        let text = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Fixing the bug now."}]}}"#;
        assert_eq!(
            parse_stream_line(text),
            StreamEvent::Text("Fixing the bug now.".into())
        );

        // tool_use wins over accompanying preamble text in the same message.
        let both = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Let me look."},{"type":"tool_use","id":"t2","name":"Read","input":{"file_path":"a.rs"}}]}}"#;
        assert!(matches!(parse_stream_line(both), StreamEvent::Tool(name, _) if name == "Read"));

        let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"Fixed both bugs."}"#;
        assert_eq!(
            parse_stream_line(result),
            StreamEvent::Result("Fixed both bugs.".into(), false)
        );

        assert_eq!(
            parse_stream_line(r#"{"type":"system","subtype":"init"}"#),
            StreamEvent::Other
        );
        assert_eq!(parse_stream_line("not json"), StreamEvent::Other);
    }
}
