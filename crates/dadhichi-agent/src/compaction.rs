//! Context compaction: keeping a long run inside the model's context window.
//!
//! As an agent talks to the model across many turns, its [`Conversation`](
//! crate::memory::Tier::Conversation) memory grows without bound. Left alone it
//! eventually overflows the model's context window and the run fails. A
//! [`CompactionPolicy`] watches the estimated token footprint and, once it
//! crosses a fraction of the window (0.85 by default, matching the "deep agent"
//! practice), asks the model to summarise the conversation so far and collapses
//! those turns into a single durable summary — freeing space while preserving
//! the gist.
//!
//! The trigger and window are configurable from the environment so a deployment
//! can tune them to whatever model it routes to, without touching call sites.

/// When, and against what budget, to compact a run's conversation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionPolicy {
    /// The model's usable context window, in tokens.
    pub context_window: usize,
    /// Compact once the estimate reaches this fraction of the window (`0..=1`).
    pub trigger_fraction: f32,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        // A generous default window (comfortably covers current frontier models)
        // and the 0.85 trigger the deep-agent pattern recommends.
        Self {
            context_window: 200_000,
            trigger_fraction: 0.85,
        }
    }
}

impl CompactionPolicy {
    /// Build a policy explicitly.
    pub fn new(context_window: usize, trigger_fraction: f32) -> Self {
        Self {
            context_window,
            trigger_fraction: trigger_fraction.clamp(0.0, 1.0),
        }
    }

    /// Read the policy from the environment, falling back to the defaults:
    /// `DADHICHI_CONTEXT_WINDOW` (tokens) and `DADHICHI_COMPACT_FRACTION`
    /// (`0.0..=1.0`). Malformed values are ignored.
    pub fn from_env() -> Self {
        let mut policy = Self::default();
        if let Some(window) = std::env::var("DADHICHI_CONTEXT_WINDOW")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|w| *w > 0)
        {
            policy.context_window = window;
        }
        if let Some(fraction) = std::env::var("DADHICHI_COMPACT_FRACTION")
            .ok()
            .and_then(|v| v.parse::<f32>().ok())
            .filter(|f| *f > 0.0 && *f <= 1.0)
        {
            policy.trigger_fraction = fraction;
        }
        policy
    }

    /// The token count at or above which a compaction fires.
    pub fn threshold_tokens(&self) -> usize {
        (self.context_window as f32 * self.trigger_fraction) as usize
    }

    /// Whether an estimated footprint of `tokens` should trigger compaction.
    pub fn should_compact(&self, tokens: usize) -> bool {
        tokens >= self.threshold_tokens()
    }
}

/// A rough token estimate for `text`, using the common ~4-characters-per-token
/// heuristic. Deliberately provider-agnostic — it only needs to be good enough
/// to decide *when* to compact, never to bill.
pub fn estimate_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// What a compaction did, for logging and the console.
#[derive(Debug, Clone)]
pub struct CompactionReport {
    /// Estimated tokens before compaction.
    pub before_tokens: usize,
    /// Estimated tokens after compaction.
    pub after_tokens: usize,
    /// The summary that replaced the conversation.
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn threshold_is_the_fraction_of_the_window() {
        let policy = CompactionPolicy::new(1000, 0.85);
        assert_eq!(policy.threshold_tokens(), 850);
        assert!(!policy.should_compact(849));
        assert!(policy.should_compact(850));
        assert!(policy.should_compact(1200));
    }

    #[test]
    fn fraction_is_clamped() {
        assert_eq!(CompactionPolicy::new(100, 5.0).trigger_fraction, 1.0);
        assert_eq!(CompactionPolicy::new(100, -1.0).trigger_fraction, 0.0);
    }

    #[test]
    fn token_estimate_scales_with_length() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
