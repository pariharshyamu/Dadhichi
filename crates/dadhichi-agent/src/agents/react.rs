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

/// Default per-round budget of tool-call iterations. A round is one uninterrupted
/// stretch of reasoning+acting; when it runs out the agent is asked to re-plan
/// and (budget permitting) a fresh round begins, so a long task continues
/// coherently instead of dead-stopping after a fixed number of tool calls.
const DEFAULT_MAX_STEPS: usize = 25;

/// Default number of re-planning rounds before the run is forced to conclude.
/// With the default step budget this allows up to 100 tool calls across a task
/// while keeping a hard safety ceiling.
const DEFAULT_MAX_ROUNDS: usize = 4;

/// The action-protocol block shared by every ReAct run: how to reply on each
/// turn. Kept separate so the role/persona/full-stack guidance can be composed
/// around it without duplicating the protocol.
fn action_protocol() -> &'static str {
    "On every turn reply with EXACTLY ONE JSON object and NOTHING else — no prose, no \
     markdown fences. Use one of these two shapes:\n\
     1. To run a tool:  {\"thought\": \"why\", \"tool\": \"<tool name>\", \"args\": { ... }}\n\
     2. To finish:      {\"thought\": \"why\", \"final\": \"<your answer to the user>\"}\n\n\
     Rules:\n\
     - Prefer acting over explaining. If the goal is a task, PERFORM it with tools, then \
     confirm what you did in \"final\".\n\
     - After each tool call you receive its result as the next message; use it to decide the \
     next step. Read files before editing them; verify with a build/test tool after changing code.\n\
     - \"args\" must be valid JSON matching the tool's schema.\n\
     - Work in small, verifiable steps. Do not claim something is done until a tool result \
     confirms it.\n\
     - When the goal is fully accomplished (or truly cannot be), reply with \"final\"."
}

/// The comprehensive full-stack engineering system prompt. It steers the agent
/// to build complete frontend + backend + database applications end to end,
/// using the scaffold/build/test/db tools, and to delegate chunky independent
/// work to specialists via the `task` tool. This is the single source of truth
/// for the agent's full-stack behaviour; specialist roles layer a `playbook` on
/// top of it rather than replacing it.
pub fn full_stack_system_prompt() -> &'static str {
    "You are Dadhichi, an autonomous full-stack software engineer working inside a code IDE. \
     You accomplish the user's goal by CALLING TOOLS — you do not merely describe what to do, \
     you do it: you scaffold projects, write and edit files, run builds and tests, query and \
     migrate databases, and wire the frontend to the backend until the application actually runs.\n\n\
     How you work:\n\
     - PLAN briefly, then ACT. Break a feature into: understand the request → inspect the existing \
     code (grep/glob/read) → make the change → verify it builds and tests pass → report.\n\
     - FULL STACK. For a new app choose a coherent stack and scaffold it: a frontend (React+Vite, \
     Angular, or plain HTML/JS as asked), a backend (Node/Express, Python/FastAPI, or Rust/Axum), \
     and a database (Postgres for relational, SQLite for local/simple). Keep the frontend and \
     backend contract in sync (API routes ↔ client calls, shared types).\n\
     - DATABASE. Design a schema, write migrations, and run them; query the database to verify \
     data rather than guessing. Never hard-code secrets — read them from env/config.\n\
     - VERIFY. After writing code, run the project's build and test tools. Fix failures before \
     declaring success. If you add an endpoint, exercise it; if you add a component, build it.\n\
     - DELEGATE. For a large, self-contained piece of work (a whole subsystem, a migration, a \
     test suite) use the `task` tool to hand it to a specialist that works in an isolated \
     context and returns only its summary — this keeps your own context focused.\n\
     - REMEMBER. The conversation may span several turns. At the START of a task, use \
     `memory.recall` to retrieve what was decided or built earlier (project name, chosen stack, \
     file layout, open TODOs), and inspect the workspace — do not assume it is empty. As you \
     work, use `memory.write` to record durable facts and decisions so later turns can pick them \
     up.\n\
     - COMMIT sensibly when asked, with clear messages.\n\
     - Prefer the project's existing conventions and dependencies; match the surrounding code."
}

/// An agent that pursues a goal by calling tools in a reason-act loop.
#[derive(Debug, Clone)]
pub struct ReactAgent {
    name: String,
    model: String,
    max_steps: usize,
    max_rounds: usize,
    /// An optional role the agent adopts (e.g. "a meticulous code reviewer"),
    /// prepended to the steering prompt so a delegated specialist behaves in
    /// character while still following the same tool-call protocol.
    persona: Option<String>,
    /// An optional extra system-prompt block (role playbook, tool guidance)
    /// appended after the base protocol — how a specialist role injects its
    /// domain instructions without a separate agent implementation.
    playbook: Option<String>,
}

impl Default for ReactAgent {
    fn default() -> Self {
        Self {
            name: "react-agent".into(),
            model: "mock".into(),
            max_steps: DEFAULT_MAX_STEPS,
            max_rounds: DEFAULT_MAX_ROUNDS,
            persona: None,
            playbook: None,
        }
    }
}

impl ReactAgent {
    /// Create an agent that routes its calls to `model`.
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            ..Self::default()
        }
    }

    /// Override the per-round tool-call budget.
    pub fn with_max_steps(mut self, max_steps: usize) -> Self {
        self.max_steps = max_steps.max(1);
        self
    }

    /// Override the number of re-planning rounds before the run is forced to end.
    pub fn with_max_rounds(mut self, max_rounds: usize) -> Self {
        self.max_rounds = max_rounds.max(1);
        self
    }

    /// Give the agent a name and a role it adopts — used when a specialist is
    /// delegated so its behaviour and reported name match the role.
    pub fn as_role(mut self, name: impl Into<String>, persona: impl Into<String>) -> Self {
        self.name = name.into();
        self.persona = Some(persona.into());
        self
    }

    /// Attach a role playbook: an extra system-prompt block appended after the
    /// base action protocol, carrying the role's domain instructions and its
    /// preferred tools. This is how a specialist role (frontend, backend, db, …)
    /// customises the one shared ReAct engine instead of a bespoke agent type.
    pub fn with_playbook(mut self, playbook: impl Into<String>) -> Self {
        self.playbook = Some(playbook.into());
        self
    }

    /// The agent's total tool-call budget across all rounds (for reporting).
    pub fn total_budget(&self) -> usize {
        self.max_steps * self.max_rounds
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

    /// The steering prompt that teaches the model the action protocol. When the
    /// agent has a `persona`, a role line is prepended so the specialist behaves
    /// in character.
    fn system_prompt(&self, tools: &str) -> String {
        let role = match &self.persona {
            Some(p) => format!("You are acting as {p}.\n\n"),
            None => String::new(),
        };
        let playbook = match &self.playbook {
            Some(p) => format!("\n\n{p}"),
            None => String::new(),
        };
        format!(
            "{role}{}\n\n\
             Tools available to you:\n{tools}\n\n\
             {}{playbook}",
            full_stack_system_prompt(),
            action_protocol(),
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
        let system = self.system_prompt(&Self::describe_tools(&tool_specs));

        let mut messages = vec![Message::system(system), Message::user(goal)];

        ctx.emit("agent.status", serde_json::json!({ "status": "running" }));

        let mut final_answer: Option<String> = None;
        let mut steps_taken = 0usize;
        // Track the last few tool signatures to detect a stall (the model looping
        // on the same call), so a round can be cut short and re-planned rather than
        // burning the whole budget repeating itself.
        let mut recent_calls: Vec<String> = Vec::new();

        // The outer loop runs re-planning rounds. Each round has a fresh step
        // budget; between rounds the agent is asked to reflect on progress and
        // decide what remains, so a long task continues coherently instead of
        // hard-stopping the moment a fixed tool-call count is reached.
        'rounds: for round in 0..self.max_rounds {
            for step_in_round in 0..self.max_steps {
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
                    break 'rounds;
                };

                if let Some(final_text) = action.get("final").and_then(|f| f.as_str()) {
                    final_answer = Some(final_text.to_string());
                    break 'rounds;
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

                // Stall guard: if the same tool+args has been issued repeatedly with
                // no progress, break the round early to force a re-plan.
                let signature = format!("{tool_name}:{args}");
                recent_calls.push(signature.clone());
                if recent_calls.len() > 4 {
                    recent_calls.remove(0);
                }
                let stalling = recent_calls.len() == 4
                    && recent_calls.iter().all(|s| *s == signature);

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

                if stalling {
                    messages.push(Message::user(
                        "You have repeated the same tool call several times with no progress. \
                         Step back: try a different approach, or reply with \"final\" if the goal \
                         is done or truly blocked.",
                    ));
                    break; // end this round early → re-plan below
                }

                // Last step of the round (and more rounds remain): let the model
                // wrap up this step before the re-plan prompt below.
                let _ = step_in_round;
            }

            // The round's step budget is spent. If more rounds remain, ask the
            // agent to reflect and continue instead of dead-stopping — this is the
            // continuation that makes long tasks coherent.
            if round + 1 < self.max_rounds && final_answer.is_none() {
                ctx.emit(
                    "agent.status",
                    serde_json::json!({ "status": "replanning", "round": round + 1 }),
                );
                messages.push(Message::user(format!(
                    "You have used {steps_taken} tool calls so far (round {}/{}). Briefly reflect: \
                     what is done, what remains, and what is the next concrete step? Then CONTINUE \
                     working toward the goal with tool calls. Only reply with \"final\" if the goal \
                     is genuinely complete.",
                    round + 1,
                    self.max_rounds,
                )));
            }
        }

        plan.complete(step_act);
        ctx.emit_plan(&plan);

        let summary = final_answer.unwrap_or_else(|| {
            format!(
                "Reached the tool-call budget ({} calls across {} rounds) with substantial work \
                 done but no explicit final answer. See the transcript for what was accomplished.",
                steps_taken, self.max_rounds
            )
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

    #[tokio::test]
    async fn continues_across_rounds_instead_of_dead_stopping() {
        // With a 2-step round budget, three tool calls would exceed one round.
        // The agent should re-plan and CONTINUE into a second round, then finish —
        // the coherence fix, versus the old behaviour of stopping at the cap.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingTool {
            calls: calls.clone(),
        }));
        let provider = Arc::new(ScriptedProvider::new(vec![
            // Round 1 (2 steps).
            r#"{"tool":"terminal.run","args":{"command":"step 1"}}"#,
            r#"{"tool":"terminal.run","args":{"command":"step 2"}}"#,
            // Round 2: one more call, then finish.
            r#"{"tool":"terminal.run","args":{"command":"step 3"}}"#,
            r#"{"final":"finished after three steps across two rounds"}"#,
        ]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut statuses = bus.subscribe_topic("agent.status");

        let outcome = ReactAgent::new("mock")
            .with_max_steps(2)
            .with_max_rounds(3)
            .run("do a multi-step task", &mut ctx)
            .await
            .unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.summary.contains("three steps"));
        // All three tool calls ran — the run did NOT dead-stop at the 2-step cap.
        assert_eq!(calls.lock().unwrap().len(), 3);
        // A replanning status was emitted between rounds.
        let mut saw_replan = false;
        while let Ok(Some(ev)) = statuses.try_recv() {
            if ev.payload.get("status").and_then(|s| s.as_str()) == Some("replanning") {
                saw_replan = true;
            }
        }
        assert!(saw_replan, "a replanning round boundary was emitted");
    }

    #[tokio::test]
    async fn budget_exhaustion_reports_progress_not_a_bare_stop() {
        // A model that never finishes: the run exhausts its whole budget and must
        // return a progress summary rather than erroring or hanging.
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingTool::default()));
        // Always emits a (varying) tool call, never a final.
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"tool":"terminal.run","args":{"command":"a"}}"#,
            r#"{"tool":"terminal.run","args":{"command":"b"}}"#,
            r#"{"tool":"terminal.run","args":{"command":"c"}}"#,
            r#"{"tool":"terminal.run","args":{"command":"d"}}"#,
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);
        let outcome = ReactAgent::new("mock")
            .with_max_steps(2)
            .with_max_rounds(2)
            .run("never-ending task", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(
            outcome.summary.contains("budget"),
            "reports hitting the budget: {}",
            outcome.summary
        );
    }

    #[tokio::test]
    async fn stall_guard_breaks_a_round_on_repeated_identical_calls() {
        // The model repeats the exact same call; the stall guard should cut the
        // round short (nudging a re-plan) rather than burning the whole budget.
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingTool {
            calls: calls.clone(),
        }));
        let same = r#"{"tool":"terminal.run","args":{"command":"same"}}"#;
        let provider = Arc::new(ScriptedProvider::new(vec![
            same, same, same, same, same, same,
            r#"{"final":"gave up repeating and finished"}"#,
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);
        let outcome = ReactAgent::new("mock")
            .with_max_steps(20)
            .with_max_rounds(2)
            .run("stall", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        // The stall guard triggers at 4 identical calls, ending the round early,
        // so far fewer than the 20-step budget were spent before finishing.
        assert!(
            calls.lock().unwrap().len() <= 8,
            "stall guard curtailed the repetition: {} calls",
            calls.lock().unwrap().len()
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
