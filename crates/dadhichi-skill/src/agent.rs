//! [`SkillAgent`]: runs a [`Skill`] as a first-class [`Agent`].
//!
//! Equipping a skill means: enforce its required permissions, inject its
//! instructions as the system prompt, execute its plan template (invoking tool
//! steps through a [`ScopedTools`] gate), consult the model, then verify and
//! report — emitting `skill.*` events throughout so the Agent Console reflects
//! progress exactly as it does for the built-in agents.

use crate::scope::ScopedTools;
use crate::skill::Skill;
use async_trait::async_trait;
use dadhichi_agent::{
    Agent, AgentContext, AgentError, AgentOutcome, AgentStatus, HeuristicVerifier, Plan, Step,
    Tier, Verifier,
};
use dadhichi_ai::{CompletionRequest, Message};
use serde_json::json;
use std::sync::Arc;

/// An [`Agent`] that pursues a goal through the lens of one [`Skill`].
#[derive(Debug, Clone)]
pub struct SkillAgent {
    skill: Arc<Skill>,
    name: String,
    default_model: Option<String>,
}

impl SkillAgent {
    /// Equip `skill`. The agent's name is `skill:<skill-name>`.
    pub fn new(skill: impl Into<Arc<Skill>>) -> Self {
        let skill = skill.into();
        let name = format!("skill:{}", skill.name);
        Self {
            skill,
            name,
            default_model: None,
        }
    }

    /// Set the model used when the skill does not name one of its own.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.default_model = Some(model.into());
        self
    }

    /// The equipped skill.
    pub fn skill(&self) -> &Skill {
        &self.skill
    }

    /// The model id this run should drive: the skill's preference, else the
    /// agent default, else the offline mock.
    fn model(&self) -> String {
        self.skill
            .model
            .clone()
            .or_else(|| self.default_model.clone())
            .unwrap_or_else(|| "mock".to_string())
    }
}

#[async_trait]
impl Agent for SkillAgent {
    fn name(&self) -> &str {
        &self.name
    }

    async fn run(&self, goal: &str, ctx: &mut AgentContext) -> Result<AgentOutcome, AgentError> {
        let skill = self.skill.clone();

        // 1. Enforce the skill's capability contract before doing any work.
        if let Some(missing) = ctx.grants.first_missing(&skill.required_permissions) {
            ctx.emit(
                "skill.denied",
                json!({ "skill": skill.name, "missing_permission": missing.to_string() }),
            );
            return Err(AgentError::Tool(format!(
                "skill '{}' requires permission '{missing}' which was not granted",
                skill.name
            )));
        }

        // 2. Equip: announce the skill, its permissions, and its tool scope.
        ctx.emit(
            "skill.equipped",
            json!({
                "skill": skill.name,
                "permissions": skill
                    .required_permissions
                    .iter()
                    .map(|p| p.to_string())
                    .collect::<Vec<_>>(),
                "tools": skill.tools.names(),
            }),
        );

        // 3. Build the plan from the skill's template.
        let mut plan = Plan::new(goal);
        for step in &skill.steps {
            plan = plan.step(match &step.tool {
                Some(tool) => Step::using(step.description.clone(), tool.clone()),
                None => Step::think(step.description.clone()),
            });
        }
        if plan.steps.is_empty() {
            plan = plan.step(Step::think("apply the skill"));
        }
        ctx.emit(
            "skill.plan",
            json!({ "skill": skill.name, "steps": plan.steps.len() }),
        );

        // 4. Execute the tool steps through the scoped gate. Results are
        //    collected first (while the scope borrows the context immutably),
        //    then recorded — keeping the borrow checker happy and the scope
        //    enforcement in one place.
        let tool_calls: Vec<(String, serde_json::Value)> = skill
            .steps
            .iter()
            .filter_map(|s| s.tool.clone().map(|t| (t, s.args.clone())))
            .collect();

        let results = {
            let scoped = ScopedTools::new(ctx.tools.as_ref(), &skill.tools, &ctx.grants);
            let mut out = Vec::with_capacity(tool_calls.len());
            for (tool, args) in &tool_calls {
                out.push((tool.clone(), scoped.invoke(tool, args.clone()).await));
            }
            out
        };

        let mut tool_summaries = Vec::new();
        for (tool, result) in results {
            match result {
                Ok(value) => {
                    ctx.emit(
                        "skill.tool.invoked",
                        json!({ "skill": skill.name, "tool": tool }),
                    );
                    let line = format!("{tool} -> {value}");
                    ctx.memory.remember(Tier::Working, line.clone());
                    tool_summaries.push(line);
                }
                Err(err) => {
                    ctx.emit(
                        "skill.tool.denied",
                        json!({ "skill": skill.name, "tool": tool, "reason": err.to_string() }),
                    );
                    return Err(AgentError::Tool(err.to_string()));
                }
            }
        }

        // 5. Consult the model under the skill's instructions, folding in any
        //    tool results as grounding.
        ctx.emit(
            "skill.status",
            json!({ "skill": skill.name, "status": "running" }),
        );
        let system = if skill.instructions.trim().is_empty() {
            format!(
                "You are equipped with the '{}' skill. {}",
                skill.name, skill.description
            )
        } else {
            skill.instructions.clone()
        };
        let user = if tool_summaries.is_empty() {
            goal.to_string()
        } else {
            format!("{goal}\n\nTool results:\n{}", tool_summaries.join("\n"))
        };
        let request = CompletionRequest::new(self.model())
            .message(Message::system(system))
            .message(Message::user(user));
        let completion = ctx
            .models
            .complete(request)
            .await
            .map_err(|e| AgentError::Model(e.to_string()))?;
        ctx.emit(
            "skill.tokens",
            json!({ "skill": skill.name, "total": completion.usage.total() }),
        );
        ctx.memory
            .remember(Tier::Conversation, completion.content.clone());

        // 6. Mark the plan complete and reflect.
        let ids: Vec<_> = plan.steps.iter().map(|s| s.id).collect();
        for id in ids {
            plan.complete(id);
        }
        let verdict = HeuristicVerifier.verify(goal, &completion.content, &plan);
        ctx.emit(
            "skill.completed",
            json!({ "skill": skill.name, "confidence": verdict.confidence }),
        );

        Ok(AgentOutcome {
            status: AgentStatus::Completed,
            summary: completion.content,
            confidence: verdict.confidence,
            plan,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::{Skill, SkillStep, SkillTools};
    use dadhichi_ai::{MockProvider, ModelRouter};
    use dadhichi_core::{EventBus, Subscription};
    use dadhichi_mcp::{EchoTool, GrantSet, Permission, ToolRegistry};

    fn context(grants: GrantSet, tools: ToolRegistry) -> (AgentContext, EventBus) {
        let mut router = ModelRouter::new();
        router.register(Arc::new(MockProvider::default()));
        let bus = EventBus::new();
        let ctx = AgentContext::new(Arc::new(router), Arc::new(tools), grants, bus.clone());
        (ctx, bus)
    }

    fn drain_topics(sub: &mut Subscription) -> Vec<String> {
        let mut topics = Vec::new();
        while let Ok(Some(event)) = sub.try_recv() {
            topics.push(event.topic.as_str().to_string());
        }
        topics
    }

    #[tokio::test]
    async fn pure_skill_runs_and_completes() {
        let skill = Skill::new("explain", "Explain code").with_instructions("Explain clearly.");
        let (mut ctx, bus) = context(GrantSet::none(), ToolRegistry::new());
        let mut sub = bus.subscribe();

        let agent = SkillAgent::new(skill);
        let outcome = agent.run("What is a microkernel?", &mut ctx).await.unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        assert!(outcome.confidence > 0.0);
        assert!(outcome.summary.contains("microkernel"));

        let topics = drain_topics(&mut sub);
        assert!(topics.contains(&"skill.equipped".to_string()));
        assert!(topics.contains(&"skill.completed".to_string()));
    }

    #[tokio::test]
    async fn missing_permission_is_refused() {
        let skill = Skill::new("edit", "Edit files").require(Permission::WriteWorkspace);
        let (mut ctx, bus) = context(GrantSet::none(), ToolRegistry::new());
        let mut sub = bus.subscribe();

        let agent = SkillAgent::new(skill);
        let err = agent.run("do it", &mut ctx).await.unwrap_err();

        assert!(matches!(err, AgentError::Tool(_)));
        assert!(drain_topics(&mut sub).contains(&"skill.denied".to_string()));
    }

    #[tokio::test]
    async fn scoped_tool_step_is_invoked() {
        let tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        let skill = Skill::new("echoer", "Echo something")
            .allow_tools(["echo"])
            .step(SkillStep::tool("echo it", "echo", json!({ "value": "hi" })));

        let (mut ctx, bus) = context(GrantSet::none(), tools);
        let mut sub = bus.subscribe();

        let agent = SkillAgent::new(skill);
        let outcome = agent.run("echo hi", &mut ctx).await.unwrap();

        assert_eq!(outcome.status, AgentStatus::Completed);
        let topics = drain_topics(&mut sub);
        assert!(topics.contains(&"skill.tool.invoked".to_string()));
    }

    #[tokio::test]
    async fn tool_step_outside_scope_is_denied() {
        let tools = ToolRegistry::new();
        tools.register(Arc::new(EchoTool));
        // The step calls `echo`, but the scope forbids it.
        let skill = Skill::new("blocked", "Try a forbidden tool").step(SkillStep::tool(
            "sneak",
            "echo",
            json!({}),
        ));
        assert_eq!(skill.tools, SkillTools::None);

        let (mut ctx, bus) = context(GrantSet::none(), tools);
        let mut sub = bus.subscribe();

        let agent = SkillAgent::new(skill);
        let err = agent.run("go", &mut ctx).await.unwrap_err();

        assert!(matches!(err, AgentError::Tool(_)));
        assert!(drain_topics(&mut sub).contains(&"skill.tool.denied".to_string()));
    }

    #[tokio::test]
    async fn skill_preferred_model_overrides_default() {
        let skill = Skill::new("x", "y").with_model("mock");
        let agent = SkillAgent::new(skill).with_model("some-other");
        assert_eq!(agent.model(), "mock");
    }
}
