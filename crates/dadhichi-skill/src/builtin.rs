//! A small library of ready-to-use skills.
//!
//! These demonstrate the full range of the model: a pure-prompt skill with no
//! capabilities, read-only skills, and skills that require write or command
//! permissions and scope themselves to the matching tools. They reference the
//! conventional tool names (`fs.read`, `fs.write`, `git.diff`, `shell.run`);
//! the actual tools are supplied by whatever [`ToolRegistry`](dadhichi_mcp::ToolRegistry)
//! the run is given, and a skill can only reach the ones both it and the run's
//! grants allow.

use crate::skill::{Skill, SkillStep};
use dadhichi_mcp::Permission;

/// Explain code or concepts. Pure reasoning — no permissions, no tools.
pub fn explain() -> Skill {
    Skill::new("explain", "Explain code or a concept in plain language")
        .with_instructions(
            "You are a patient teacher inside the Dadhichi IDE. Explain the subject clearly and \
             concisely, using concrete examples from the code when relevant.",
        )
        .think("identify what needs explaining")
        .think("explain it plainly with an example")
}

/// Review a change. Read-only: may read files and inspect the git diff.
pub fn code_review() -> Skill {
    Skill::new("code-review", "Review a change for correctness and clarity")
        .with_instructions(
            "You are a meticulous code reviewer. Assess correctness, edge cases, readability, and \
             tests. Be specific and cite lines. Do not modify files.",
        )
        .require(Permission::ReadWorkspace)
        .allow_tools(["fs.read", "git.diff"])
        .step(SkillStep::think("read the change and its context"))
        .think("evaluate correctness and edge cases")
        .think("summarise findings with concrete suggestions")
}

/// Implement a change. Requires read+write and scopes to the file tools.
pub fn implement() -> Skill {
    Skill::new("implement", "Implement a change with idiomatic code")
        .with_instructions(
            "You are an expert software engineer. Implement the requested change with correct, \
             idiomatic code that matches the surrounding style, then self-review it.",
        )
        .require(Permission::ReadWorkspace)
        .require(Permission::WriteWorkspace)
        .allow_tools(["fs.read", "fs.write"])
        .think("analyse the request and locate the code")
        .think("write the change")
        .think("self-review for correctness and style")
}

/// Author and run tests. Requires read, write, and command execution.
pub fn author_tests() -> Skill {
    Skill::new("author-tests", "Write tests and run the suite")
        .with_instructions(
            "You are a testing specialist. Write focused, meaningful tests for the change and run \
             the suite to confirm they pass.",
        )
        .require(Permission::ReadWorkspace)
        .require(Permission::WriteWorkspace)
        .require(Permission::RunCommands)
        .allow_tools(["fs.read", "fs.write", "shell.run"])
        .think("identify what to test")
        .think("write the tests")
        .think("run the suite and report results")
}

/// Audit for security issues. Read-only inspection.
pub fn security_audit() -> Skill {
    Skill::new("security-audit", "Audit code for security issues")
        .with_instructions(
            "You are a security auditor. Look for injection, secret leakage, unsafe deserialisation, \
             and missing authorisation. Report findings with severity; do not modify files.",
        )
        .require(Permission::ReadWorkspace)
        .allow_tools(["fs.read"])
        .think("scan for risky patterns")
        .think("assess severity and report")
}

/// Every built-in skill, in a stable order.
pub fn all() -> Vec<Skill> {
    vec![
        explain(),
        code_review(),
        implement(),
        author_tests(),
        security_audit(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_are_well_formed() {
        for skill in all() {
            assert!(!skill.name.is_empty());
            assert!(!skill.description.is_empty());
            assert!(!skill.instructions.is_empty());
            assert!(!skill.steps.is_empty());
        }
    }

    #[test]
    fn permission_scopes_match_intent() {
        assert!(explain().required_permissions.is_empty());
        assert!(
            code_review()
                .required_permissions
                .contains(&Permission::ReadWorkspace)
        );
        assert!(
            implement()
                .required_permissions
                .contains(&Permission::WriteWorkspace)
        );
        assert!(
            author_tests()
                .required_permissions
                .contains(&Permission::RunCommands)
        );
    }

    #[test]
    fn read_only_skills_cannot_reach_write_tools() {
        assert!(!code_review().tools.allows("fs.write"));
        assert!(code_review().tools.allows("fs.read"));
    }
}
