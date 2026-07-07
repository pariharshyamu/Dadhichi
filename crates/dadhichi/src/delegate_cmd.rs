//! The `dadhichi delegate` command: run a specialist on an isolated sub-task,
//! then land its work on the branch after verification or approval.
//!
//! This is the end-to-end "deep agents" flow. The delegate is a tool-using
//! agent whose file writes are staged in a copy-on-write overlay, so nothing
//! touches the working tree until the orchestrator decides. Work whose verified
//! confidence clears the threshold lands automatically; otherwise the change set
//! is shown and a `y/N` prompt gates it. Landing flushes the overlay to the
//! workspace and commits it to the current branch.

use std::io::{self, Write};
use std::sync::Arc;

use dadhichi_agent::{Delegator, ModelCritic, ReactAgent, SubAgentSpec};
use dadhichi_ai::ProviderPlan;
use dadhichi_core::EventBus;
use dadhichi_git::GitRepo;
use dadhichi_mcp::{StateStore, WorkspaceStore};
use dadhichi_skill::SkillRegistry;

use crate::console;

/// Run the delegation flow for `subagent` on `task`, blocking on the async
/// runtime the caller already set up.
pub async fn run(subagent: String, task: String) {
    let plan = ProviderPlan::from_env();
    println!("dadhichi ▸ model provider: {}", plan.summary());
    let model_id = plan.default_model();
    let router = Arc::new(plan.build_router());

    // Resolve the named specialist to its role + tool/permission envelope. Each
    // specialist gets exactly the powers its job needs (a reviewer is read-only,
    // a coder may write and run commands).
    let Some(spec) = SubAgentSpec::for_role(&subagent) else {
        eprintln!(
            "dadhichi ▸ unknown specialist '{subagent}'. Available: {}",
            SubAgentSpec::roster().join(", ")
        );
        return;
    };

    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let base: Arc<dyn StateStore> = Arc::new(WorkspaceStore::new(&cwd));

    // Equip the specialist's skills from the library (prebuilt plus any on disk),
    // folding each skill's instructions into the delegate's persona so it gains
    // the reusable capability, not just the role.
    let persona = equip_skills(&spec.persona, &spec.skills);

    // Stream the delegation's bus events (delegated → reviewed, plus the
    // delegate's own agent.* activity) to stdout.
    let bus = EventBus::new();
    let console = console::spawn(&bus);

    // The delegate is a tool-using agent in the specialist's role. Its file
    // writes land in the overlay, so it stages work rather than mutating the tree.
    let agent = ReactAgent::new(&model_id).as_role(&spec.name, persona);

    // Verify the delegate's work with an orchestrator-side critic model rather
    // than trusting its own self-assessment.
    let critic = Arc::new(ModelCritic::new(router.clone(), &model_id));
    let delegator = Delegator::new(router, bus.clone()).with_critic(critic);

    println!("dadhichi ▸ delegating to '{}': {task}\n", spec.name);
    let review = match delegator
        .delegate(&agent, &spec, &task, base.clone(), &cwd)
        .await
    {
        Ok(review) => review,
        Err(err) => {
            eprintln!("dadhichi ▸ delegation failed: {err}");
            drop(bus);
            let _ = console.await;
            return;
        }
    };

    println!("\ndadhichi ▸ {}", review.verdict_note());
    println!("dadhichi ▸ answer: {}", review.outcome.summary);
    if review.changes.is_empty() {
        println!("dadhichi ▸ the delegate staged no file changes — nothing to land.");
        drop(bus);
        let _ = console.await;
        return;
    }
    println!("dadhichi ▸ staged changes:");
    for change in &review.changes {
        let mark = if change.deleted { "D" } else { "M" };
        println!("dadhichi ▸   {mark} {}", change.path);
    }

    // Verify-then-approve: land automatically when confident, else ask.
    let land = if review.auto_approved() {
        println!("dadhichi ▸ verified (confidence ≥ threshold) — landing automatically.");
        true
    } else {
        print!(
            "dadhichi ▸ confidence below threshold — land these {} change(s) on '{}'? [y/N] ",
            review.changes.len(),
            GitRepo::open(&cwd)
                .ok()
                .and_then(|r| r.current_branch())
                .unwrap_or_else(|| "the branch".into()),
        );
        let _ = io::stdout().flush();
        let mut line = String::new();
        io::stdin().read_line(&mut line).ok();
        matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    };

    if !land {
        println!("dadhichi ▸ discarded — nothing was written or committed.");
        drop(bus);
        let _ = console.await;
        return;
    }

    // Land: flush the overlay onto the working tree, then commit to the branch.
    match review.land() {
        Ok(n) => println!("dadhichi ▸ flushed {n} file(s) to the workspace."),
        Err(err) => {
            eprintln!("dadhichi ▸ failed to land changes: {err}");
            drop(bus);
            let _ = console.await;
            return;
        }
    }

    match commit(&cwd, &spec.name, &task) {
        Ok(id) => println!(
            "dadhichi ▸ committed {} to the branch.",
            &id[..id.len().min(10)]
        ),
        Err(err) => eprintln!("dadhichi ▸ changes landed but commit failed: {err}"),
    }

    drop(bus);
    let _ = console.await;
}

/// Resolve `skills` from the skill library (built-ins plus any user/project
/// manifests on disk) and append their instructions to `persona`, so the
/// delegate adopts both the role and the reusable capability. Unknown skill
/// names are noted and skipped.
fn equip_skills(persona: &str, skills: &[String]) -> String {
    if skills.is_empty() {
        return persona.to_string();
    }
    let (registry, _load) = SkillRegistry::discover();
    let mut out = persona.to_string();
    let mut equipped = Vec::new();
    for name in skills {
        match registry.get(name) {
            Some(skill) => {
                out.push_str(&format!(
                    "\n\nSkill — {}: {}",
                    skill.name, skill.instructions
                ));
                equipped.push(name.clone());
            }
            None => eprintln!("dadhichi ▸ skill '{name}' not found; skipping"),
        }
    }
    if !equipped.is_empty() {
        println!("dadhichi ▸ equipped skills: {}", equipped.join(", "));
    }
    out
}

/// Stage and commit the landed changes, attributing them to the sub-agent.
fn commit(cwd: &std::path::Path, subagent: &str, task: &str) -> Result<String, String> {
    let repo = GitRepo::open(cwd).map_err(|e| e.to_string())?;
    repo.stage_all().map_err(|e| e.to_string())?;
    let (name, email) = author();
    let message = format!("{subagent}: {task}");
    repo.commit(&message, &name, &email)
        .map_err(|e| e.to_string())
}

/// The commit author, from the standard git env vars, falling back to a
/// sub-agent identity.
fn author() -> (String, String) {
    let name = std::env::var("GIT_AUTHOR_NAME").unwrap_or_else(|_| "dadhichi-agent".to_string());
    let email =
        std::env::var("GIT_AUTHOR_EMAIL").unwrap_or_else(|_| "agent@dadhichi.local".to_string());
    (name, email)
}
