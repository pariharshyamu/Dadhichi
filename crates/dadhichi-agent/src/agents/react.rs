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
     - To FINISH, use shape 2 exactly: put the answer in a top-level \"final\" field. Do NOT \
     write {\"tool\": \"final\", ...} — there is no tool named final; that is an error.\n\
     - Prefer acting over explaining. If the goal is a task, PERFORM it with tools, then \
     confirm what you did in \"final\".\n\
     - After each tool call you receive its result as the next message; use it to decide the \
     next step. To find code, fs.grep/fs.glob first, then fs.read the relevant slice — do not \
     read whole files to look around. Verify with code.check or a build/test tool after edits.\n\
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
     - SEARCH, DON'T SCAN. Never read a whole file to find something — it wastes context and \
     money. To locate code use `fs.grep` (a symbol, string, or error text) or `fs.glob` (find \
     files by name); the hits tell you the exact file and line. THEN `fs.read` that file with \
     `offset`/`limit` around the hit — a focused window, not the whole file. Read a file whole \
     only when it is genuinely small or you must understand all of it. Never re-read a file you \
     already have in context; never read your own prior tool output back.\n\
     - TOKEN DISCIPLINE: use the cheapest tool that answers the question. Change existing files \
     with `fs.edit` (send only the snippet that changes); reserve `fs.write` for new files or \
     full rewrites. After an edit, `code.check` the file instead of re-reading it whole.\n\
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
    /// An optional project-rules block (discovered `AGENTS.md` / `.dadhichi/
    /// rules`) appended to the prompt so the agent follows the repository's
    /// conventions. See [`crate::rules`].
    project_rules: Option<String>,
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
            project_rules: None,
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

    /// Attach a project-rules block (from [`crate::rules::ProjectRules`]) so the
    /// agent follows the repository's conventions. A no-op when `rules` is empty.
    pub fn with_project_rules(mut self, rules: impl Into<String>) -> Self {
        let rules = rules.into();
        if !rules.trim().is_empty() {
            self.project_rules = Some(rules);
        }
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
        let rules = match &self.project_rules {
            Some(r) => format!("\n\n{r}"),
            None => String::new(),
        };
        format!(
            "{role}{}\n\n\
             Tools available to you:\n{tools}\n\n\
             {}{playbook}{rules}",
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
        // Native tool-calling: advertise the tools in the provider's own
        // function-calling channel. The model then returns structured calls
        // instead of hand-written JSON, which removes the whole class of
        // parse/shape bugs and cuts prompt overhead. When the provider or model
        // doesn't support it, the model just answers in text and the loop's
        // JSON-scraping fallback takes over — so this is strictly additive.
        let tool_defs: Vec<dadhichi_ai::ToolDef> = tool_specs
            .iter()
            .map(|s| dadhichi_ai::ToolDef {
                name: s.name.clone(),
                description: s.description.clone(),
                parameters: s.input_schema.clone(),
            })
            .collect();
        let mut system = self.system_prompt(&Self::describe_tools(&tool_specs));

        // Self-improvement: lessons distilled from previous runs in this
        // workspace (stored as `lesson:` long-term memories) are injected up
        // front, so past mistakes inform this run before it repeats them.
        let lessons: Vec<String> = ctx
            .memory
            .recall_tier(Tier::LongTerm)
            .iter()
            .filter(|item| item.content.starts_with("lesson:"))
            .rev()
            .take(5)
            .map(|item| item.content.trim_start_matches("lesson:").trim().to_string())
            .collect();
        if !lessons.is_empty() {
            system.push_str("\n\nLESSONS from previous runs in this workspace:\n");
            for lesson in lessons.iter().rev() {
                system.push_str(&format!("- {lesson}\n"));
            }
        }

        let mut messages = vec![Message::system(system), Message::user(goal)];

        ctx.emit("agent.status", serde_json::json!({ "status": "running" }));

        // Token and wall-clock accounting for the whole run. Two distinct
        // numbers matter, and conflating them is misleading:
        //  * BILLED tokens — the sum across every model call. Because the whole
        //    conversation is resent each call, prompt tokens are re-counted
        //    every step; this sum is what the provider actually bills.
        //  * CONTEXT tokens — the size of the *latest* prompt, i.e. how full the
        //    context window is right now. This does not grow with call count.
        let run_start = std::time::Instant::now();
        let mut billed_prompt: u64 = 0;
        let mut billed_completion: u64 = 0;
        let mut context_tokens: u64 = 0;
        let mut model_calls: u64 = 0;
        // Total time spent waiting on the model — the "thinking" time, as
        // distinct from tool-execution time.
        let mut thinking_ms: u128 = 0;

        let mut final_answer: Option<String> = None;
        let mut steps_taken = 0usize;
        // Track the last few tool signatures to detect a stall (the model looping
        // on the same call), so a round can be cut short and re-planned rather than
        // burning the whole budget repeating itself.
        let mut recent_calls: Vec<String> = Vec::new();

        // Completion guard: a goal that asks for changes ("fix", "implement",
        // "add"…) must not end after analysis alone — models love to describe
        // the fix and stop. Track whether any state-changing tool succeeded and,
        // if not, bounce the first attempts to finish back into the loop. Only
        // tools this run is actually *granted* to use count: a read-only
        // delegate can't be badgered into writing.
        let mutating_tools: std::collections::HashSet<&str> = tool_specs
            .iter()
            .filter(|spec| {
                spec.permissions.iter().any(|p| {
                    matches!(
                        p,
                        dadhichi_mcp::Permission::WriteWorkspace
                            | dadhichi_mcp::Permission::RunCommands
                    )
                }) && ctx.grants.allows(&spec.permissions)
            })
            .map(|spec| spec.name.as_str())
            .collect();
        let goal_wants_mutation = goal_implies_mutation(goal) && !mutating_tools.is_empty();
        let search_available = tool_specs
            .iter()
            .any(|s| s.name == "fs.grep" || s.name == "fs.glob");
        let mut whole_file_nudged = false;
        let mut mutated = false;
        let mut completion_nudges = 0usize;
        const MAX_COMPLETION_NUDGES: usize = 2;
        const ACT_NUDGE: &str = "You have analysed the problem but not changed anything yet — the \
             goal asks you to make changes, and no file write or command has run. Apply the fix \
             now with tool calls (e.g. fs.write the corrected content), verify the result, and \
             only then reply with \"final\".";

        // The outer loop runs re-planning rounds. Each round has a fresh step
        // budget; between rounds the agent is asked to reflect on progress and
        // decide what remains, so a long task continues coherently instead of
        // hard-stopping the moment a fixed tool-call count is reached.
        let mut cancelled = false;
        let mut last_check_clean: Option<bool> = None;
        let mut had_tool_errors = false;

        'rounds: for round in 0..self.max_rounds {
            for step_in_round in 0..self.max_steps {
                // The user pressed Stop: wind down immediately but cleanly.
                if ctx.control.is_cancelled() {
                    cancelled = true;
                    break 'rounds;
                }
                // Mid-run steering: messages the user typed while the agent
                // worked join the conversation as fresh user turns.
                for steer in ctx.control.drain_messages() {
                    ctx.emit("agent.steered", serde_json::json!({ "text": steer }));
                    messages.push(Message::user(format!(
                        "USER (mid-run steering): {steer}"
                    )));
                }

                let request = CompletionRequest {
                    model: self.model.clone(),
                    messages: messages.clone(),
                    params: Default::default(),
                    tools: tool_defs.clone(),
                };
                // Time the model call — this is the run's "thinking" latency,
                // reported live so the console can show an elapsed timer.
                ctx.emit("agent.thinking", serde_json::json!({ "state": "start" }));
                let call_start = std::time::Instant::now();
                // Race the (possibly multi-second) model call against a stop:
                // pressing Stop abandons the in-flight call immediately rather
                // than waiting for it to return.
                let completion = tokio::select! {
                    biased;
                    _ = ctx.control.cancelled() => {
                        ctx.emit("agent.thinking", serde_json::json!({ "state": "end", "ms": call_start.elapsed().as_millis() }));
                        cancelled = true;
                        break 'rounds;
                    }
                    result = ctx.models.complete(request) => {
                        result.map_err(|e| AgentError::Model(e.to_string()))?
                    }
                };
                let call_ms = call_start.elapsed().as_millis();
                thinking_ms += call_ms;

                // Billed = cumulative across calls; context = this call's prompt
                // (the live window size), not a running sum.
                billed_prompt += completion.usage.prompt_tokens as u64;
                billed_completion += completion.usage.completion_tokens as u64;
                context_tokens = completion.usage.prompt_tokens as u64
                    + completion.usage.completion_tokens as u64;
                model_calls += 1;
                ctx.emit(
                    "agent.tokens",
                    serde_json::json!({
                        // Billed totals (what you pay).
                        "billed_prompt": billed_prompt,
                        "billed_completion": billed_completion,
                        "billed_total": billed_prompt + billed_completion,
                        // Live context-window size (does not grow with call count).
                        "context": context_tokens,
                        "calls": model_calls,
                        "last_call": completion.usage.total(),
                        "thinking_ms": thinking_ms,
                        "elapsed_ms": run_start.elapsed().as_millis(),
                    }),
                );
                ctx.emit(
                    "agent.thinking",
                    serde_json::json!({ "state": "end", "ms": call_ms }),
                );

                // ---- Native tool-calling path -------------------------------
                // The model used the provider's structured tool channel. Run
                // each call, thread proper assistant/tool messages, and loop.
                if !completion.tool_calls.is_empty() {
                    messages.push(Message::assistant_calls(
                        completion.content.clone(),
                        completion.tool_calls.clone(),
                    ));
                    let mut any_error = false;
                    for call in &completion.tool_calls {
                        // A finish disguised as a tool call still finishes.
                        if is_finish_alias(&call.name) {
                            let answer = call
                                .arguments
                                .get("content")
                                .or_else(|| call.arguments.get("answer"))
                                .or_else(|| call.arguments.get("text"))
                                .and_then(|v| v.as_str())
                                .map(String::from)
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| completion.content.clone());
                            if goal_wants_mutation
                                && !mutated
                                && completion_nudges < MAX_COMPLETION_NUDGES
                            {
                                completion_nudges += 1;
                                messages.push(Message::user(ACT_NUDGE));
                            } else {
                                final_answer = Some(answer);
                                break 'rounds;
                            }
                            continue;
                        }
                        steps_taken += 1;
                        ctx.emit(
                            "agent.tool",
                            serde_json::json!({ "tool": call.name, "args": call.arguments, "step": steps_taken }),
                        );
                        let edited_path = matches!(call.name.as_str(), "fs.write" | "fs.edit")
                            .then(|| call.arguments.get("path").and_then(|p| p.as_str()).map(String::from))
                            .flatten();
                        match ctx.tools.invoke(&call.name, call.arguments.clone(), &ctx.grants).await {
                            Ok(result) => {
                                if mutating_tools.contains(call.name.as_str()) {
                                    mutated = true;
                                }
                                ctx.emit(
                                    "agent.tool.result",
                                    serde_json::json!({ "tool": call.name, "result": result }),
                                );
                                messages.push(Message::tool_result(
                                    &call.id,
                                    clip_result(&result.to_string()),
                                ));
                                if let Some(path) = edited_path {
                                    last_check_clean = self.auto_check(ctx, &path, &mut messages).await;
                                }
                            }
                            Err(err) => {
                                any_error = true;
                                had_tool_errors = true;
                                let msg = err.to_string();
                                ctx.emit(
                                    "agent.tool.error",
                                    serde_json::json!({ "tool": call.name, "error": msg }),
                                );
                                messages.push(Message::tool_result(
                                    &call.id,
                                    format!("ERROR: {msg}. Try a different approach or finish."),
                                ));
                            }
                        }
                    }
                    ctx.maybe_compact(&self.model).await;
                    let _ = any_error;
                    continue;
                }
                // ---- Text / JSON-scraping fallback path ---------------------

                let reply = completion.content.trim().to_string();
                let action = Self::extract_json(&reply);

                // No parseable action ⇒ the model answered in prose. For a
                // read-only goal that's a clean finish; for a mutation goal
                // with nothing changed yet it's the classic analyse-and-stop
                // failure, so push it back into the loop instead.
                let Some(action) = action else {
                    if goal_wants_mutation && !mutated && completion_nudges < MAX_COMPLETION_NUDGES
                    {
                        completion_nudges += 1;
                        messages.push(Message::assistant(reply));
                        messages.push(Message::user(ACT_NUDGE));
                        continue;
                    }
                    final_answer = Some(reply);
                    break 'rounds;
                };

                if let Some(final_text) = action.get("final").and_then(|f| f.as_str()) {
                    if goal_wants_mutation && !mutated && completion_nudges < MAX_COMPLETION_NUDGES
                    {
                        completion_nudges += 1;
                        messages.push(Message::assistant(reply));
                        messages.push(Message::user(ACT_NUDGE));
                        continue;
                    }
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

                // Models frequently write the finish as a *tool call*
                // (`{"tool":"final","args":{"content":…}}`) instead of the
                // `{"final":…}` shape. Treat those aliases as a real finish
                // rather than trying to invoke a non-existent tool.
                if is_finish_alias(tool_name) {
                    let answer = args
                        .get("content")
                        .or_else(|| args.get("answer"))
                        .or_else(|| args.get("text"))
                        .or_else(|| args.get("final"))
                        .and_then(|v| v.as_str())
                        .map(String::from)
                        .unwrap_or_else(|| reply.clone());
                    if goal_wants_mutation && !mutated && completion_nudges < MAX_COMPLETION_NUDGES {
                        completion_nudges += 1;
                        messages.push(Message::assistant(reply));
                        messages.push(Message::user(ACT_NUDGE));
                        continue;
                    }
                    final_answer = Some(answer);
                    break 'rounds;
                }

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

                let edited_path = matches!(tool_name, "fs.write" | "fs.edit")
                    .then(|| args.get("path").and_then(|p| p.as_str()).map(String::from))
                    .flatten();
                // A whole-file read (no offset/limit) of a big file, while
                // search tools were available — the anti-pattern to nudge once.
                let unfocused_read = tool_name == "fs.read"
                    && args.get("offset").is_none()
                    && args.get("limit").is_none();
                match ctx.tools.invoke(tool_name, args, &ctx.grants).await {
                    Ok(result) => {
                        if mutating_tools.contains(tool_name) {
                            mutated = true;
                        }
                        ctx.emit(
                            "agent.tool.result",
                            serde_json::json!({ "tool": tool_name, "result": result }),
                        );
                        // Large results are clipped before joining the context;
                        // the model is told how to fetch a targeted slice.
                        messages.push(Message::user(clip_result(&format!(
                            "TOOL RESULT [{tool_name}]: {result}"
                        ))));

                        // One-time coaching: if the model read a big file whole
                        // when it could have searched, teach the cheaper path.
                        let big = result.get("lines").and_then(|l| l.as_u64()).unwrap_or(0) > 200;
                        if unfocused_read && big && search_available && !whole_file_nudged {
                            whole_file_nudged = true;
                            messages.push(Message::user(
                                "Note: that was a large whole-file read. Next time, fs.grep for \
                                 the symbol or text you need and fs.read only that slice \
                                 (offset/limit) — it is far cheaper. Continue.",
                            ));
                        }

                        // Verification loop: an edit is immediately checked
                        // (when a `code.check` tool is registered) and any
                        // problems go straight back to the model — an agent
                        // must not declare victory on a file it just broke.
                        if let Some(path) = edited_path {
                            last_check_clean =
                                self.auto_check(ctx, &path, &mut messages).await;
                        }
                    }
                    Err(err) => {
                        had_tool_errors = true;
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

        let summary = if cancelled {
            ctx.emit("agent.status", serde_json::json!({ "status": "cancelled" }));
            format!("Stopped by the user after {steps_taken} tool call(s).")
        } else {
            final_answer.unwrap_or_else(|| {
                format!(
                    "Reached the tool-call budget ({} calls across {} rounds) with substantial \
                     work done but no explicit final answer. See the transcript for what was \
                     accomplished.",
                    steps_taken, self.max_rounds
                )
            })
        };
        ctx.memory.remember(Tier::Conversation, summary.clone());
        ctx.emit(
            "agent.message",
            serde_json::json!({ "role": "assistant", "content": summary }),
        );

        plan.complete(step_report);
        ctx.emit_plan(&plan);

        // Self-improvement: distill one durable lesson from an eventful run
        // (tool errors, verification failures) into long-term memory. The
        // session store persists it, so the *next* run starts smarter. Its
        // model call is folded into the run's token tally.
        if !cancelled && (had_tool_errors || last_check_clean == Some(false) || steps_taken >= 4) {
            let billed = billed_prompt + billed_completion;
            let (p, c) = self
                .reflect(ctx, &mut messages, billed, model_calls)
                .await;
            billed_prompt += p;
            billed_completion += c;
            if p + c > 0 {
                model_calls += 1;
            }
        }

        // Confidence is earned, not asserted: edits that were verified clean
        // score high; unverified edits medium; a broken check low.
        let confidence = match (mutated, last_check_clean) {
            (true, Some(true)) => 0.9,
            (true, None) => 0.7,
            (true, Some(false)) => 0.4,
            (false, _) => 0.75,
        };

        // Final run accounting. `billed_*` is the true cost (sum over calls);
        // `context_tokens` is the final window size — reported separately so the
        // console doesn't present a re-counted prompt sum as one lump.
        let elapsed_ms = run_start.elapsed().as_millis();
        ctx.emit(
            "agent.usage",
            serde_json::json!({
                "billed_prompt": billed_prompt,
                "billed_completion": billed_completion,
                "billed_total": billed_prompt + billed_completion,
                "context_tokens": context_tokens,
                "model_calls": model_calls,
                "tool_calls": steps_taken,
                "thinking_ms": thinking_ms,
                "elapsed_ms": elapsed_ms,
            }),
        );

        let outcome = AgentOutcome {
            status: if cancelled {
                AgentStatus::Cancelled
            } else {
                AgentStatus::Completed
            },
            summary,
            confidence,
            plan,
        };
        ctx.emit(
            "agent.status",
            serde_json::json!({ "status": "completed", "confidence": outcome.confidence }),
        );
        Ok(outcome)
    }
}

impl ReactAgent {
    /// Run the registered `code.check` tool (if any) against a just-edited
    /// file and feed problems back into the conversation. Returns whether the
    /// check came back clean (`None` when no checker is available or the file
    /// type isn't supported).
    async fn auto_check(
        &self,
        ctx: &mut AgentContext,
        path: &str,
        messages: &mut Vec<Message>,
    ) -> Option<bool> {
        if !ctx.tools.list().iter().any(|s| s.name == "code.check") {
            return None;
        }
        let result = ctx
            .tools
            .invoke(
                "code.check",
                serde_json::json!({ "path": path }),
                &ctx.grants,
            )
            .await
            .ok()?;
        if !result.get("supported").and_then(|s| s.as_bool()).unwrap_or(true) {
            return None;
        }
        ctx.emit(
            "agent.tool.result",
            serde_json::json!({ "tool": "code.check", "result": result }),
        );
        let count = result.get("count").and_then(|c| c.as_u64()).unwrap_or(0);
        if count > 0 {
            messages.push(Message::user(clip_result(&format!(
                "VERIFICATION [{path}]: your edit left {count} problem(s): {}. Fix them with \
                 fs.edit before finishing.",
                result.get("problems").cloned().unwrap_or_default()
            ))));
            Some(false)
        } else {
            Some(true)
        }
    }

    /// Ask the model for one durable, workspace-specific lesson from this run
    /// and store it as long-term memory (`lesson: …`). One cheap extra call;
    /// the session store persists it, so future runs are seeded with it.
    /// Returns the `(prompt, completion)` tokens it used so the caller can fold
    /// them into the run tally.
    async fn reflect(
        &self,
        ctx: &mut AgentContext,
        messages: &mut Vec<Message>,
        total_tokens: u64,
        model_calls: u64,
    ) -> (u64, u64) {
        // A run that burned a lot of tokens should reflect on *efficiency*, so
        // the lesson kept is "how to do this cheaper" rather than a restatement
        // of what was done. Give the model the cost so it can judge.
        let cost_hint = if total_tokens > 40_000 {
            format!(
                " This run used {total_tokens} tokens across {model_calls} model calls, which is \
                 expensive — favour a lesson about how to reach the same answer with fewer / \
                 cheaper tool calls (search before reading, read narrow slices)."
            )
        } else {
            String::new()
        };
        messages.push(Message::user(format!(
            "Final step: in ONE line of at most 120 characters, state the single most useful \
             lesson from this run for future work in this workspace (a pitfall, a convention, a \
             faster/cheaper route).{cost_hint} Reply with exactly `lesson: <text>` and nothing \
             else. If there is no lesson worth keeping, reply `lesson: none`."
        )));
        // Reflection is plain text — no tools needed.
        let request = CompletionRequest {
            model: self.model.clone(),
            messages: messages.clone(),
            params: Default::default(),
            tools: Vec::new(),
        };
        let Ok(completion) = ctx.models.complete(request).await else {
            return (0, 0);
        };
        let line = completion.content.trim();
        if let Some(text) = line.strip_prefix("lesson:") {
            let text = text.trim();
            if !text.is_empty() && text != "none" && text.len() <= 200 {
                ctx.memory.remember(Tier::LongTerm, format!("lesson: {text}"));
                ctx.emit("agent.lesson", serde_json::json!({ "lesson": text }));
            }
        }
        (
            completion.usage.prompt_tokens as u64,
            completion.usage.completion_tokens as u64,
        )
    }
}

/// Clip an oversized tool result before it joins the conversation, telling the
/// model how to fetch precisely what it needs instead. Keeps one careless read
/// from flooding the context window.
fn clip_result(text: &str) -> String {
    const MAX_CHARS: usize = 6000;
    const HEAD: usize = 4500;
    const TAIL: usize = 800;
    if text.len() <= MAX_CHARS {
        return text.to_string();
    }
    let head_end = (0..=HEAD).rev().find(|i| text.is_char_boundary(*i)).unwrap_or(0);
    let tail_start = (text.len() - TAIL..text.len())
        .find(|i| text.is_char_boundary(*i))
        .unwrap_or(text.len());
    format!(
        "{}\n…[{} chars clipped — use fs.read with offset/limit or fs.grep to fetch exactly \
         what you need]…\n{}",
        &text[..head_end],
        text.len() - head_end - (text.len() - tail_start),
        &text[tail_start..]
    )
}

/// Whether a tool name is really a disguised "finish" — models emit these both
/// as JSON `{"tool":"final"}` and as native calls to a nonexistent `final`.
fn is_finish_alias(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "final" | "finish" | "done" | "answer" | "final_answer" | "complete"
    )
}

/// Whether a goal's wording asks for changes to be made (as opposed to a
/// question or explanation), so the loop can insist on action before finishing.
fn goal_implies_mutation(goal: &str) -> bool {
    const VERBS: &[&str] = &[
        "fix", "bug", "implement", "add ", "create", "write", "build", "make ", "update",
        "refactor", "improve", "remove", "delete", "rename", "install", "scaffold", "generate",
        "convert", "migrate", "set up", "setup",
    ];
    let goal = goal.to_lowercase();
    VERBS.iter().any(|v| goal.contains(v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentContext;

    #[test]
    fn project_rules_appear_in_the_system_prompt() {
        let agent = ReactAgent::new("mock")
            .with_project_rules("<project_rules>\nUse tabs, not spaces.\n</project_rules>");
        let prompt = agent.system_prompt("(no tools)");
        assert!(prompt.contains("Use tabs, not spaces."));
        assert!(prompt.contains("<project_rules>"));
    }

    #[test]
    fn empty_project_rules_are_ignored() {
        let agent = ReactAgent::new("mock").with_project_rules("   ");
        assert!(agent.project_rules.is_none());
    }

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
                // Non-zero so token-accounting assertions have something to add.
                usage: Usage {
                    prompt_tokens: 100,
                    completion_tokens: 10,
                },
                tool_calls: Vec::new(),
            })
        }
    }

    /// A provider that returns one native tool call, then a native finish —
    /// exercising the structured tool-calling path rather than JSON scraping.
    #[derive(Debug)]
    struct NativeToolProvider {
        step: Mutex<u32>,
    }

    #[async_trait]
    impl LanguageModel for NativeToolProvider {
        fn id(&self) -> &str {
            "mock"
        }
        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities { tools: true, local: true, ..Default::default() }
        }
        async fn complete(&self, request: CompletionRequest) -> ProviderResult<Completion> {
            // The loop must have advertised the registered tools natively.
            assert!(!request.tools.is_empty(), "tools were not advertised to the provider");
            let n = {
                let mut s = self.step.lock().unwrap();
                let v = *s;
                *s += 1;
                v
            };
            let tool_calls = if n == 0 {
                vec![dadhichi_ai::ToolCall {
                    id: "call_1".into(),
                    name: "terminal.run".into(),
                    arguments: serde_json::json!({ "command": "ls" }),
                }]
            } else {
                Vec::new()
            };
            Ok(Completion {
                content: if n == 0 { String::new() } else { "all done".into() },
                model: "mock".into(),
                usage: Usage { prompt_tokens: 50, completion_tokens: 5 },
                tool_calls,
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
    async fn native_tool_calls_are_executed_and_threaded() {
        // The model uses the provider's structured tool channel (no JSON in
        // text). The loop must run the tool and finish on the follow-up.
        let recorder = RecordingTool::default();
        let calls = recorder.calls.clone();
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(recorder));
        let (mut ctx, bus) =
            ctx_with(Arc::new(NativeToolProvider { step: Mutex::new(0) }), tools);
        let mut tool_events = bus.subscribe_topic("agent.tool");

        let outcome = ReactAgent::new("mock")
            .run("run ls for me", &mut ctx)
            .await
            .unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.summary.contains("all done"));
        // The native call actually reached the tool with its arguments.
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0]["command"], "ls");
        // And it surfaced on the bus as a normal tool event.
        let ev = tool_events.recv().await.unwrap();
        assert_eq!(ev.payload["tool"], "terminal.run");
    }

    #[tokio::test]
    async fn a_final_shaped_as_a_tool_call_is_treated_as_the_answer() {
        // The model writes the finish as a tool call — a very common mistake.
        // It must terminate cleanly with the answer, not error on an unknown
        // "final" tool.
        let tools = Arc::new(ToolRegistry::new());
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"tool":"final","args":{"content":"The repo is an AI-first IDE in Rust."}}"#,
        ]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut errs = bus.subscribe_topic("agent.tool.error");

        let outcome = ReactAgent::new("mock")
            .run("what is this repo", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.summary.contains("AI-first IDE in Rust"));
        // No bogus "unknown tool: final" error was emitted.
        assert!(matches!(errs.try_recv(), Ok(None)));
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
    async fn a_fix_goal_cannot_end_on_analysis_alone() {
        // The model tries to finish twice with pure analysis (the exact
        // failure seen in the wild: describe the bugs, change nothing, stop).
        // The guard bounces it back until it actually writes, then lets the
        // real final through.
        let recorder = RecordingTool::default();
        let calls = recorder.calls.clone();
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(recorder));
        let provider = Arc::new(ScriptedProvider::new(vec![
            "Here are the bugs I found: the margin property is misspelled.",
            r#"{"final":"The bugs are the misspelled CSS properties."}"#,
            r#"{"tool":"terminal.run","args":{"command":"apply-fix"}}"#,
            r#"{"final":"Fixed the misspelled properties and verified."}"#,
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);

        let outcome = ReactAgent::new("mock")
            .run("fix bugs in this file", &mut ctx)
            .await
            .unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(
            outcome.summary.contains("Fixed"),
            "run must end on the post-write final, got: {}",
            outcome.summary
        );
        assert_eq!(
            calls.lock().unwrap().len(),
            1,
            "the nudges must drive the model into actually acting"
        );
    }

    #[tokio::test]
    async fn a_stubborn_model_still_terminates_after_the_nudge_budget() {
        // A model that never acts must not loop forever: two nudges, then its
        // analysis is accepted as the final answer. (A mutating tool must be
        // registered — the guard only arms when acting is actually possible.)
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(RecordingTool::default()));
        let provider = Arc::new(ScriptedProvider::new(vec![
            "Analysis only, attempt 1.",
            "Analysis only, attempt 2.",
            "Analysis only, attempt 3.",
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);
        let outcome = ReactAgent::new("mock")
            .run("fix the parser bug", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.summary.contains("attempt 3"));
    }

    /// A fake tool with a configurable name/permissions and canned reply.
    #[derive(Debug)]
    struct FakeTool {
        name: &'static str,
        permissions: Vec<Permission>,
        reply: serde_json::Value,
    }

    #[async_trait]
    impl Tool for FakeTool {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.name.into(),
                description: "fake".into(),
                input_schema: serde_json::json!({ "type": "object" }),
                permissions: self.permissions.clone(),
            }
        }
        async fn invoke(&self, _args: serde_json::Value) -> ToolResult {
            Ok(self.reply.clone())
        }
    }

    #[tokio::test]
    async fn a_stopped_run_reports_cancelled() {
        let tools = Arc::new(ToolRegistry::new());
        let provider = Arc::new(ScriptedProvider::new(vec!["should never be consumed"]));
        let (mut ctx, _bus) = ctx_with(provider, tools);
        ctx.control.stop();

        let outcome = ReactAgent::new("mock")
            .run("what is up", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Cancelled);
        assert!(outcome.summary.contains("Stopped by the user"));
    }

    #[tokio::test]
    async fn steering_messages_reach_the_conversation_and_the_bus() {
        let tools = Arc::new(ToolRegistry::new());
        let provider = Arc::new(ScriptedProvider::new(vec![r#"{"final":"noted"}"#]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut steered = bus.subscribe_topic("agent.steered");
        ctx.control.say("only touch the parser module");

        let outcome = ReactAgent::new("mock")
            .run("what is up", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        let event = steered.recv().await.unwrap();
        assert_eq!(event.payload["text"], "only touch the parser module");
        assert!(ctx.control.drain_messages().is_empty(), "inbox was drained");
    }

    #[tokio::test]
    async fn an_eventful_run_distills_a_lesson_into_memory() {
        // A tool error makes the run "eventful"; the reflection turn's
        // `lesson:` line must land in long-term memory for future seeding.
        let tools = Arc::new(ToolRegistry::new());
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"tool":"terminal.run","args":{"command":"x"}}"#,
            r#"{"final":"could not run it"}"#,
            "lesson: the terminal tool is unavailable here; use fs tools instead",
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);

        ReactAgent::new("mock").run("do a thing", &mut ctx).await.unwrap();

        let lessons = ctx.memory.recall("lesson:");
        assert_eq!(lessons.len(), 1);
        assert!(lessons[0].content.contains("terminal tool is unavailable"));
    }

    #[tokio::test]
    async fn edits_are_auto_checked_and_broken_ones_dent_confidence() {
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(FakeTool {
            name: "fs.write",
            permissions: vec![Permission::RunCommands],
            reply: serde_json::json!({ "path": "a.html", "bytes": 10 }),
        }));
        tools.register(Arc::new(FakeTool {
            name: "code.check",
            permissions: vec![],
            reply: serde_json::json!({ "count": 2, "problems": ["unclosed <div>"] }),
        }));
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"tool":"fs.write","args":{"path":"a.html","content":"<div>"}}"#,
            r#"{"final":"wrote the file"}"#,
        ]));
        let (mut ctx, _bus) = ctx_with(provider, tools);

        let outcome = ReactAgent::new("mock")
            .run("fix the page", &mut ctx)
            .await
            .unwrap();
        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(
            outcome.confidence < 0.5,
            "a failing post-edit check must dent confidence, got {}",
            outcome.confidence
        );
    }

    #[tokio::test]
    async fn billed_tokens_accumulate_while_context_tracks_the_last_call() {
        // Two model calls at 100 prompt / 10 completion each. Billed must be
        // the SUM (200/20); context must be the LAST call (110), not the sum —
        // that distinction is the whole point of the fix.
        let recorder = RecordingTool::default();
        let tools = Arc::new(ToolRegistry::new());
        tools.register(Arc::new(recorder));
        let provider = Arc::new(ScriptedProvider::new(vec![
            r#"{"tool":"terminal.run","args":{"command":"ls"}}"#,
            r#"{"final":"done"}"#,
        ]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut usage = bus.subscribe_topic("agent.usage");

        ReactAgent::new("mock").run("list things", &mut ctx).await.unwrap();

        let event = usage.recv().await.unwrap();
        assert_eq!(event.payload["model_calls"], 2);
        assert_eq!(event.payload["billed_prompt"], 200);
        assert_eq!(event.payload["billed_completion"], 20);
        assert_eq!(event.payload["billed_total"], 220);
        assert_eq!(event.payload["context_tokens"], 110, "context is one call, not the sum");
        assert!(event.payload["elapsed_ms"].as_u64().is_some());
        assert!(event.payload["thinking_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn stop_interrupts_an_in_flight_model_call_immediately() {
        // A provider that blocks forever; the run must still return promptly
        // once stop() fires, proving the call is abandoned mid-flight rather
        // than awaited to completion.
        #[derive(Debug)]
        struct HangingProvider;
        #[async_trait]
        impl LanguageModel for HangingProvider {
            fn id(&self) -> &str { "mock" }
            fn capabilities(&self) -> ModelCapabilities {
                ModelCapabilities { tools: true, local: true, ..Default::default() }
            }
            async fn complete(&self, _r: CompletionRequest) -> ProviderResult<Completion> {
                // Never resolves on its own.
                std::future::pending::<()>().await;
                unreachable!()
            }
        }
        let tools = Arc::new(ToolRegistry::new());
        let (mut ctx, _bus) = ctx_with(Arc::new(HangingProvider), tools);
        let control = ctx.control.clone();

        // Fire stop shortly after the run begins its (hanging) model call.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            control.stop();
        });

        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            ReactAgent::new("mock").run("do something long", &mut ctx),
        )
        .await
        .expect("run must return promptly after stop, not hang")
        .unwrap();
        assert_eq!(outcome.status, AgentStatus::Cancelled);
    }

    #[tokio::test]
    async fn a_thinking_event_brackets_each_model_call() {
        let tools = Arc::new(ToolRegistry::new());
        let provider = Arc::new(ScriptedProvider::new(vec![r#"{"final":"hi"}"#]));
        let (mut ctx, bus) = ctx_with(provider, tools);
        let mut thinking = bus.subscribe_topic("agent.thinking");

        ReactAgent::new("mock").run("say hi", &mut ctx).await.unwrap();

        let start = thinking.recv().await.unwrap();
        assert_eq!(start.payload["state"], "start");
        let end = thinking.recv().await.unwrap();
        assert_eq!(end.payload["state"], "end");
        assert!(end.payload["ms"].as_u64().is_some());
    }

    #[test]
    fn clip_result_bounds_huge_results_with_guidance() {
        let huge = "x".repeat(50_000);
        let clipped = clip_result(&huge);
        assert!(clipped.len() < 7_000);
        assert!(clipped.contains("chars clipped"));
        assert!(clipped.contains("fs.read"));
        // Small results pass through untouched.
        assert_eq!(clip_result("small"), "small");
    }

    #[test]
    fn mutation_goals_are_recognised() {
        assert!(goal_implies_mutation("fix bugs in this file"));
        assert!(goal_implies_mutation("Implement a reverse function"));
        assert!(goal_implies_mutation("add a scoreboard to it"));
        assert!(!goal_implies_mutation("what is a type-safe value"));
        assert!(!goal_implies_mutation("explain how the parser works"));
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
