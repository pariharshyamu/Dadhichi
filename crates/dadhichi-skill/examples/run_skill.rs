//! Runnable demonstration of the skills layer — no kernel, no indexing.
//!
//!   cargo run -p dadhichi-skill --example run_skill
//!
//! It equips three skills against an offline mock model and a single `echo`
//! tool, printing the outcome and the `skill.*` events each run emits, and
//! shows the permission gate refusing a write-scoped skill under a read-only
//! grant.

use std::sync::Arc;

use dadhichi_agent::{Agent, AgentContext};
use dadhichi_ai::{MockProvider, ModelRouter};
use dadhichi_core::{EventBus, Subscription};
use dadhichi_mcp::{EchoTool, GrantSet, Permission, ToolRegistry};
use dadhichi_skill::{Skill, SkillAgent, SkillRegistry, SkillStep};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Shared services: an offline model, one tool, an event bus.
    let router = {
        let mut r = ModelRouter::new();
        r.register(Arc::new(MockProvider::default()));
        Arc::new(r)
    };
    let tools = {
        let mut t = ToolRegistry::new();
        t.register(Arc::new(EchoTool));
        Arc::new(t)
    };
    let bus = EventBus::new();

    let mut registry = SkillRegistry::with_builtins();
    println!(
        "{} built-in skills: {}",
        registry.len(),
        registry.names().join(", ")
    );

    // Load a user-authored skill from a JSON manifest on disk. In a real
    // install these live in ~/.dadhichi/skills or ./.dadhichi/skills; here we
    // write one to a temp directory to keep the example self-contained.
    let skill_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        skill_dir.path().join("greeter.json"),
        r#"{
            "name": "greeter",
            "description": "A friendly greeting skill",
            "instructions": "Greet the user warmly.",
            "steps": [{ "description": "compose a greeting" }]
        }"#,
    )
    .unwrap();
    let report = registry.load_dir(skill_dir.path());
    println!(
        "loaded {} skill(s) from disk: {}\n",
        report.loaded.len(),
        report.loaded.join(", ")
    );

    // 1. A pure-prompt skill (no permissions, no tools).
    run(
        &router,
        &tools,
        &bus,
        GrantSet::none(),
        SkillAgent::new(registry.get("explain").unwrap()),
        "What is a microkernel?",
    )
    .await;

    // 2. A skill scoped to the `echo` tool that actually invokes it.
    let echo_skill = Skill::new("echo-demo", "Demonstrate a scoped tool call")
        .with_instructions("Summarise the tool output.")
        .allow_tools(["echo"])
        .step(SkillStep::tool(
            "echo the greeting",
            "echo",
            serde_json::json!({ "value": "hello from a skill" }),
        ));
    run(
        &router,
        &tools,
        &bus,
        GrantSet::none(),
        SkillAgent::new(echo_skill),
        "Greet the user",
    )
    .await;

    // 3. The capability gate: a write-scoped skill under a read-only grant.
    run(
        &router,
        &tools,
        &bus,
        GrantSet::from_iter([Permission::ReadWorkspace]),
        SkillAgent::new(registry.get("implement").unwrap()),
        "Edit a file",
    )
    .await;

    // 4. The disk-loaded skill runs exactly like a built-in.
    run(
        &router,
        &tools,
        &bus,
        GrantSet::none(),
        SkillAgent::new(registry.get("greeter").unwrap()),
        "Say hello",
    )
    .await;
}

async fn run(
    router: &Arc<ModelRouter>,
    tools: &Arc<ToolRegistry>,
    bus: &EventBus,
    grants: GrantSet,
    agent: SkillAgent,
    goal: &str,
) {
    let mut sub = bus.subscribe();
    let mut ctx = AgentContext::new(router.clone(), tools.clone(), grants, bus.clone());

    println!("▸ {} — goal: {goal}", agent.name());
    match agent.run(goal, &mut ctx).await {
        Ok(outcome) => println!(
            "  outcome: {:?} (confidence {:.0}%)",
            outcome.status,
            outcome.confidence * 100.0
        ),
        Err(err) => println!("  refused: {err}"),
    }
    for topic in drain(&mut sub) {
        println!("    · {topic}");
    }
    println!();
}

fn drain(sub: &mut Subscription) -> Vec<String> {
    let mut topics = Vec::new();
    while let Ok(Some(event)) = sub.try_recv() {
        topics.push(event.topic.as_str().to_string());
    }
    topics
}
