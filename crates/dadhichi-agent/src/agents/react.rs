//! A tool-using agent that actually *acts* on a goal.
//!
//! [`ConversationalAgent`] only talks to the model and returns prose. The
//! `ReactAgent` closes the loop: it hands the model the tool catalogue, asks it
//! to reply with a single JSON action (either a tool call or a final answer),
//! executes the requested tool through the permission/approval gate, feeds the
//! result back, and repeats until the model declares it is done.
//!
//! The protocol is a plain-text ReAct convention rather than a provider-specific
//! function-calling API, so it works uniformly across every backend — hosted or
//! a local Ollama model — without depending on native tool-call support.

use crate::agent::{Agent, AgentContext, AgentError, AgentOutcome, AgentStatus};
use crate::memory::Tier;
use crate::plan::{Plan, Step};
use async_trait::async_trait;
use dadhichi_ai::{CompletionRequest, Message};
use dadhichi_mcp::ToolSpec;

/// Default cap on tool-call iterations before the run is forced to conclude.
const DEFAULT_MAX_STEPS: usize = 10;

/// An agent that pursues a goal by calling tools in a reason-act loop.
#[derive(Debug, Clone)]
pub struct ReactAgent {
    name: String,
    model: String,
    max_steps: usize,
}

impl Default for ReactAgent {
    fn default() -> Self {
        Self {
            name: "react-agent".into(),
            model: "mock".into(),
            max_steps: DEFAULT_MAX_STEPS,
        }
    }
}

impl ReactAgent {
    /// Create an agent that routes its calls to `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            name: "react-agent".into(),
            model: model.into(),
            max_steps: DEFAULT_MAX_STEPS,
        }
    }

    /// Override the maximum number of tool-call iterations.
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    /// Render the tool catalogue into a compact prompt block the model can read.
    fn describe_tools(specs: &[ToolSpec]) -> String {
        if specs.is_empty() {
            return "(no tools are available)".to_string();
        }
        specs
            .iter()
            .map(|s| {
                let perms: Vec<String> = s.permissions.iter().map(|p| p.to_string()).collect();
                let perms = if perms.is_empty() {
                    String::new()
                } else {
                    format!("  [requires: {}]", perms.join(", "))
                };
                format!(
                    "- {}: {}{}\n  args schema: {}",
                    s.name, s.description, perms, s.input_schema
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The steering prompt that teaches the model the action protocol.
    fn system_prompt(tools: &str) -> String {
        format!(
            "You are Dadhichi, an autonomous agent working inside a code IDE. You accomplish the \
             user's goal by CALLING TOOLS — you do not merely describe what to do, you do it.\n\n\
             Tools available to you:\n{tools}\n\n\
             On every turn reply with EXACTLY ONE JSON object and NOTHING else — no prose, no \
             markdown fences. Use one of these two shapes:\n\
             1. To run a tool:  {{\"thought\": \"why\", \"tool\": \"<tool name>\", \"args\": {{ ... }}}}\n\
             2. To finish:      {{\"thought\": \"why\", \"final\": \"<your answer to the user>\"}}\n\n\
             Rules:\n\
             - Prefer acting over explaining. If the goal is a task (e.g. \"add git to this \
             folder\"), perform it with tools, then confirm what you did in \"final\".\n\
             - After each tool call you receive its result as the next message; use it to decide \
             the next step.\n\
             - \"args\" must be valid JSON matching the tool's schema.\n\
             - When the goal is fully accomplished (or truly cannot be), reply with \"final\"."
        )
    }

    /// Pull the first balanced top-level JSON object out of a model reply,
    /// tolerating stray prose or ```json fences around it. Returns `None` when
    /// no object is present.
    fn extract_json(text: &str) -> Option<serde_json::Value> {
        let bytes = text.as_bytes();
        let start = text.find('{')?;
        let mut depth = 0usize;
        let mut in_str = false;
        let mut escaped = false;
        for (i, &b) in bytes.iter().enumerate().skip(start) {
            match b {
                b'"' if !escaped => in_str = !in_str,
                b'\\' if in_str => {
                    escaped = !escaped;
                    continue;
                }
                b'{' if !in_str => depth += 1,
                b'}' if !in_str => {
                    depth -= 1;
                    if depth == 0 {
                        let slice = &text[start..=i];
                        return serde_json::from_str(slice).ok();
                    }
                }
                _ => {}
            }
            escaped = false;
        }
        None
    }
}

#[async_trait]
impl Agent for ReactAgent {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        "Accomplishes tasks by calling tools (run commands, read/write files) in a reason-act loop."
    }

    async fn run(&self, goal: &str, ctx: &mut AgentContext) -> Result<AgentOutcome, AgentError> {
        ctx.emit("agent.status", serde_json::json!({ "status": "planning" }));
        let mut plan = Plan::new(goal)
            .step(Step::think("understand the goal"))
            .step(Step::using("use tools to accomplish it", "tools"))
            .step(Step::think("report the result"));
        ctx.emit_plan(&plan);

        let step_understand = plan.steps[0].id;
        let step_act = plan.steps[1].id;
        let step_report = plan.steps[2].id;

        ctx.memory.remember(Tier::Working, format!("goal: {goal}"));
        plan.complete(step_understand);
        ctx.emit_plan(&plan);

        let tool_specs = ctx.tools.list();
        let system = Self::system_prompt(&Self::describe_tools(&tool_specs));

        let mut messages = vec![Message::system(system), Message::user(goal)];

        ctx.emit("agent.status", serde_json::json!({ "status": "running" }));

        let mut final_answer: Option<String> = None;
        let mut steps_taken = 0usize;

        for _ in 0..self.max_steps {
            let request = CompletionRequest {
                model: self.model.clone(),
                messages: messages.clone(),
                params: Default::default(),
            };
            let completion = ctx
                .models
                .complete(request)
                .await
                .map_err(|e| AgentError::Model(e.to_string()))?;
            ctx.emit(
                "agent.tokens",
                serde_json::json!({ "total": completion.usage.total() }),
            );

            let reply = completion.content.trim().to_string();
            let action = Self::extract_json(&reply);

            // No parseable action ⇒ treat the whole reply as the final answer,
            // so a model that just answers in prose still terminates cleanly.
            let Some(action) = action else {
                final_answer = Some(reply);
                break;
            };

            if let Some(final_text) = action.get("final").and_then(|f| f.as_str()) {
                final_answer = Some(final_text.to_string());
                break;
            }

            let Some(tool_name) = action.get("tool").and_then(|t| t.as_str()) else {
                // A JSON object with neither `tool` nor `final`: nudge and retry.
                messages.push(Message::assistant(reply));
                messages.push(Message::user(
                    "Your reply had neither a \"tool\" nor a \"final\" field. Reply with one \
                     valid action JSON object.",
                ));
                continue;
            };
            let args = action.get("args").cloned().unwrap_or(serde_json::json!({}));

            steps_taken += 1;
            ctx.emit(
                "agent.tool",
                serde_json::json!({ "tool": tool_name, "args": args, "step": steps_taken }),
            );
            messages.push(Message::assistant(reply));

            match ctx.tools.invoke(tool_name, args, &ctx.grants).await {
                Ok(result) => {
                    ctx.emit(
                        "agent.tool.result",
                        serde_json::json!({ "tool": tool_name, "result": result }),
                    );
                    messages.push(Message::user(format!(
                        "TOOL RESULT [{tool_name}]: {result}"
                    )));
                }
                Err(err) => {
                    let msg = err.to_string();
                    ctx.emit(
                        "agent.tool.error",
                        serde_json::json!({ "tool": tool_name, "error": msg }),
                    );
                    messages.push(Message::user(format!(
                        "TOOL ERROR [{tool_name}]: {msg}. Try a different approach or finish."
                    )));
                }
            }

            // Keep the loop inside the context window as the transcript grows.
            ctx.memory.remember(
                Tier::Conversation,
                messages
                    .last()
                    .map(|m| m.content.clone())
                    .unwrap_or_default(),
            );
            ctx.maybe_compact(&self.model).await;
        }

        plan.complete(step_act);
        ctx.emit_plan(&plan);

        let summary = final_answer.unwrap_or_else(|| {
            format!("Stopped after {steps_taken} tool call(s) without a final answer.")
        });
        ctx.memory.remember(Tier::Conversation, summary.clone());
        ctx.emit(
            "agent.message",
            serde_json::json!({ "role": "assistant", "content": summary }),
        );

        plan.complete(step_report);
        ctx.emit_plan(&plan);

        let outcome = AgentOutcome {
            status: AgentStatus::Completed,
            summary,
            confidence: 0.9,
            plan,
        };
        ctx.emit(
            "agent.status",
            serde_json::json!({ "status": "completed", "confidence": outcome.confidence }),
        );
        Ok(outcome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentContext;
    use async_trait::async_trait;
    use dadhichi_ai::{
        Completion, CompletionRequest, LanguageModel, ModelCapabilities, ModelRouter,
        ProviderResult, Usage,
    };
    use dadhichi_core::EventBus;
    use dadhichi_mcp::{GrantSet, Permission, Tool, ToolRegistry, ToolResult, ToolSpec};
    use std::sync::{Arc, Mutex};

    /// A provider that returns a scripted sequence of replies, one per call.
    #[derive(Debug)]
    struct ScriptedProvider {
        replies: Mutex<std::collections::VecDeque<String>>,
    }

    impl ScriptedProvider {
        fn new(replies: Vec<&str>) -> Self {
            Self {
                replies: Mutex::new(replies.into_iter().map(|s| s.to_string()).collect()),
            }
        }
    }

    #[async_trait]
    impl LanguageModel for ScriptedProvider {
        fn id(&self) -> &str {
            "mock"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                tools: true,
                local: true,
                ..Default::default()
            }
        }
        async fn complete(&self, _request: CompletionRequest) -> ProviderResult<Completion> {
            // Scope the guard so it is dropped before the function's await point,
            // keeping clippy's `await_holding_lock` quiet.
            let next = {
                let mut replies = self.replies.lock().unwrap();
                replies.pop_front()
            };
            let content = next.unwrap_or_else(|| "{\"final\": \"done\"}".to_string());
            Ok(Completion {
                content,
                model: "mock".into(),
                usage: Usage::default(),
            })
        }
    }

    /// A tool that records the args it was called with.
    #[derive(Debug, Default)]
    struct RecordingTool {
        calls: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    #[async_trait]
    impl Tool for RecordingTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "terminal.run".into(),
                description: "Run a shell command".into(),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "command": { "type": "string" } },
                    "required": ["command"]
                }),
                permissions: vec![Permission::RunCommands],
            }
        }
        async fn invoke(&self, args: serde_json::Value) -> ToolResult {
            self.calls.lock().unwrap().push(args.clone());
            Ok(serde_json::json!({ "stdout": "Initialized empty Git repository", "code": 0 }))
        }
    }

    fn ctx_with(
        provider: Arc<dyn LanguageModel>,
        tools: Arc<ToolRegistry>,
    ) -> (AgentContext, EventBus) {
        let mut router = ModelRouter::new();
        router.register(provider);
        let bus = EventBus::new();
        let ctx = AgentContext::new(
            Arc::new(router),
            tools,
            GrantSet::from_iter([Permission::RunCommands]),
            bus.clone(),
        );
        (ctx, bus)
    }

    #[tokio::test]
    async fn calls_a_tool_then_finishes() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingTool {
            calls: calls.clone(),
        }));
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"thought":"init a repo","tool":"terminal.run","args":{"command":"git init"}}"#,
            r#"{"thought":"done","final":"I initialised a git repository in this folder."}"#,
        ]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut msgs = bus.subscribe_topic("agent.message");

        let outcome = ReactAgent::new("mock")
            .run("add git to this folder", &mut ctx)
            .await
            .unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.summary.contains("initialised a git repository"));
        // The tool actually ran, with the model's args. Scope the guard so it is
        // dropped before the await below.
        {
            let recorded = calls.lock().unwrap();
            assert_eq!(recorded.len(), 1);
            assert_eq!(recorded[0]["command"], "git init");
        }
        // The final answer reached the bus for the console to render.
        let event = msgs.recv().await.unwrap();
        assert!(
            event.payload["content"]
                .as_str()
                .unwrap()
                .contains("git repository")
        );
    }

    #[tokio::test]
    async fn plain_prose_reply_terminates_as_final() {
        let tools = Arc::new(ToolRegistry::new());
        let provider = Arc::new(ScriptedProvider::new(vec![
            "A type-safe value is one the compiler guarantees matches its type.",
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);
        let outcome = ReactAgent::new("mock")
            .run("what is a type-safe value", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.summary.contains("type-safe value"));
    }

    #[tokio::test]
    async fn tool_error_is_fed_back_and_run_recovers() {
        let tools = Arc::new(ToolRegistry::new());
        // No tool registered ⇒ the first call errors; the model then finishes.
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"tool":"terminal.run","args":{"command":"git init"}}"#,
            r#"{"final":"Could not run the command, but here is guidance instead."}"#,
        ]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut errs = bus.subscribe_topic("agent.tool.error");
        let outcome = ReactAgent::new("mock")
            .run("do a thing", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(
            matches!(errs.try_recv(), Ok(Some(_))),
            "a tool error was surfaced"
        );
    }

    #[test]
    fn extract_json_pulls_object_from_fenced_prose() {
        let v = ReactAgent::extract_json("Sure!\n```json\n{\"final\": \"hi\"}\n```").unwrap();
        assert_eq!(v["final"], "hi");
        // Braces inside strings don't confuse the balance scan.
        let v2 = ReactAgent::extract_json(r#"{"final": "use {curly} braces"}"#).unwrap();
        assert_eq!(v2["final"], "use {curly} braces");
        assert!(ReactAgent::extract_json("no json here").is_none());
    }
}
