//! Reflection and verification: scoring how much to trust an agent's outcome.
//!
//! After acting, an agent *reflects* — it checks its own work and assigns a
//! confidence in `0.0..=1.0`. The [`Verifier`] trait abstracts that check so
//! different agents can verify differently (running tests, re-reading a diff,
//! asking a critic model). [`HeuristicVerifier`] is a fast, offline default that
//! scores structural signals: did the plan finish, is the answer substantive,
//! did any step fail.

use crate::plan::Plan;

/// The result of verifying an outcome.
#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    /// Confidence in the outcome, `0.0..=1.0`.
    pub confidence: f32,
    /// Human-readable notes explaining the score.
    pub notes: Vec<String>,
}

impl Verdict {
    /// Whether the outcome clears a confidence `threshold`.
    pub fn is_confident(&self, threshold: f32) -> bool {
        self.confidence >= threshold
    }
}

/// Something that can judge an agent's work.
pub trait Verifier: Send + Sync {
    /// Score `answer` produced for `goal` under `plan`.
    fn verify(&self, goal: &str, answer: &str, plan: &Plan) -> Verdict;
}

/// A structural verifier that needs no model call.
///
/// It rewards a fully completed plan and a substantive answer, and penalises
/// empty or apologetic answers. This makes confidence reflect real signals
/// rather than a hard-coded constant.
#[derive(Debug, Default, Clone)]
pub struct HeuristicVerifier;

impl Verifier for HeuristicVerifier {
    fn verify(&self, _goal: &str, answer: &str, plan: &Plan) -> Verdict {
        let mut score = 0.5;
        let mut notes = Vec::new();

        let progress = plan.progress();
        score += 0.3 * progress;
        notes.push(format!("plan {:.0}% complete", progress * 100.0));

        let trimmed = answer.trim();
        if trimmed.is_empty() {
            score -= 0.4;
            notes.push("answer is empty".into());
        } else if trimmed.len() >= 16 {
            score += 0.2;
            notes.push("answer is substantive".into());
        }

        let lower = trimmed.to_lowercase();
        if lower.contains("i cannot") || lower.contains("i'm not sure") || lower.contains("error") {
            score -= 0.2;
            notes.push("answer hedges or reports failure".into());
        }

        Verdict {
            confidence: score.clamp(0.0, 1.0),
            notes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Plan, Step};

    fn completed_plan() -> Plan {
        let mut plan = Plan::new("goal")
            .step(Step::think("a"))
            .step(Step::think("b"));
        let ids: Vec<_> = plan.steps.iter().map(|s| s.id).collect();
        for id in ids {
            plan.complete(id);
        }
        plan
    }

    #[test]
    fn complete_plan_and_good_answer_scores_high() {
        let v = HeuristicVerifier;
        let verdict = v.verify(
            "goal",
            "Here is a thorough, detailed answer.",
            &completed_plan(),
        );
        assert!(verdict.is_confident(0.8), "got {}", verdict.confidence);
    }

    #[test]
    fn empty_answer_scores_low() {
        let v = HeuristicVerifier;
        let verdict = v.verify("goal", "", &completed_plan());
        assert!(!verdict.is_confident(0.5), "got {}", verdict.confidence);
    }

    #[test]
    fn incomplete_plan_lowers_confidence() {
        let v = HeuristicVerifier;
        let mut plan = Plan::new("g").step(Step::think("a")).step(Step::think("b"));
        plan.complete(plan.steps[0].id); // only half done
        let partial = v.verify("g", "a reasonable answer here", &plan).confidence;
        let full = v
            .verify("g", "a reasonable answer here", &completed_plan())
            .confidence;
        assert!(full > partial);
    }
}
