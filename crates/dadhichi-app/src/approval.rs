//! Bridging tool-approval interrupts to the event bus and back.
//!
//! When the [`ToolRegistry`](dadhichi_mcp::ToolRegistry) interrupts a call, it
//! asks a [`BusApprover`], which publishes an `agent.approval` event and then
//! *suspends the tool call* on a one-shot channel. A frontend renders the
//! prompt, the user answers `y`/`n`, and the controller's
//! [`resolve_approval`](crate::AppController::resolve_approval) sends the verdict
//! back through that channel — unblocking the tool. Because the wait is `async`,
//! the render loop keeps pumping while the call is parked.

use dadhichi_core::{Event, EventBus};
use dadhichi_mcp::{ApprovalRequest, Approver, Decision};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use uuid::Uuid;

/// The set of tool calls currently parked awaiting a verdict, keyed by request
/// id. Shared between the [`BusApprover`] (which inserts a sender per call) and
/// the controller (which removes and fires it when the user answers).
pub type PendingApprovals = Arc<Mutex<HashMap<Uuid, oneshot::Sender<Decision>>>>;

/// An [`Approver`] that turns an interrupt into an `agent.approval` event and
/// waits for the answer on a one-shot channel.
pub struct BusApprover {
    bus: EventBus,
    pending: PendingApprovals,
}

impl std::fmt::Debug for BusApprover {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BusApprover").finish_non_exhaustive()
    }
}

impl BusApprover {
    /// Build an approver that publishes on `bus` and parks calls in `pending`.
    pub fn new(bus: EventBus, pending: PendingApprovals) -> Self {
        Self { bus, pending }
    }
}

#[async_trait::async_trait]
impl Approver for BusApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Decision {
        let id = Uuid::new_v4();
        let (tx, rx) = oneshot::channel();
        // Register the waiter *before* announcing, so a very fast answer can't
        // race ahead of the sender being in the map.
        if let Ok(mut guard) = self.pending.lock() {
            guard.insert(id, tx);
        } else {
            return Decision::Deny;
        }

        self.bus.publish(Event::new(
            "agent.approval",
            serde_json::json!({
                "id": id.to_string(),
                "tool": request.tool,
                "permission": request.permission.to_string(),
                "summary": request.summary(),
                // The full arguments, so a frontend can render a real preview
                // (e.g. a diff for fs.write) instead of a truncated one-liner.
                "args": request.args,
            }),
        ));

        // Park until the frontend resolves it. A dropped sender (e.g. the app is
        // shutting down) is treated as a denial — fail safe.
        rx.await.unwrap_or(Decision::Deny)
    }
}
