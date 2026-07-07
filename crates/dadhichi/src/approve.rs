//! A stdin-backed [`Approver`] for the CLI.
//!
//! In the TUI a gated tool call raises a `y/n` prompt on the bus; in the
//! headless CLI there is no event loop to answer it, so this approver reads the
//! decision straight from the terminal. It keeps the same human-in-the-loop
//! guarantee: a shell command or file write pauses for confirmation before it
//! runs, defaulting to *deny* on anything but an explicit yes.

use async_trait::async_trait;
use dadhichi_mcp::{ApprovalRequest, Approver, Decision};
use std::io::{self, Write};

/// Prompts the operator on the controlling terminal before a consequential tool
/// call proceeds.
pub struct CliApprover;

#[async_trait]
impl Approver for CliApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let summary = request.summary();
        // stdin reads block, so keep them off the async runtime's worker.
        tokio::task::spawn_blocking(move || {
            print!("\n  ⚠ approve  {summary}  ? [y/N] ");
            let _ = io::stdout().flush();
            let mut line = String::new();
            if io::stdin().read_line(&mut line).is_err() {
                return Decision::Deny;
            }
            match line.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" => Decision::Approve,
                _ => Decision::Deny,
            }
        })
        .await
        .unwrap_or(Decision::Deny)
    }
}
