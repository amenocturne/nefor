//! Bridge between the inbound event channel (where `tool.result`
//! arrives) and the in-flight turn that's awaiting it.
//!
//! The provider's dispatch loop owns the only `tool.result` listener.
//! The turn-runner task that issued the `<plugin>.tool.invoke` is a
//! separate task and needs to block on the matching reply. The broker
//! is the seam: the turn-runner registers a oneshot keyed by the
//! correlation id, the dispatcher fires it when a `tool.result`
//! arrives, and the turn-runner wakes up.
//!
//! No timeout is enforced here — the turn-runner applies its own
//! cancellation (the per-turn `CancellationToken`) so a hung tool
//! doesn't hang the plugin forever.

use std::collections::HashMap;

use tokio::sync::{oneshot, Mutex};

/// Result delivered from a `tool.result` event back to the awaiting
/// turn-runner. Exactly one of `output`/`error` is set per successful
/// reply (the protocol guarantees this; the broker doesn't check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub id: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Default)]
pub struct ToolBroker {
    /// Pending invocations, keyed by tool-call id.
    pending: Mutex<HashMap<String, oneshot::Sender<ToolResult>>>,
}

impl ToolBroker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a pending invocation. Returns the receiver the caller
    /// awaits. Replaces any previous oneshot for the same id (which
    /// would only happen on a programming error — caller is using
    /// fresh ids per call).
    pub async fn register(&self, id: String) -> oneshot::Receiver<ToolResult> {
        let (tx, rx) = oneshot::channel();
        let mut g = self.pending.lock().await;
        g.insert(id, tx);
        rx
    }

    /// Deliver a result to the caller awaiting `id`. Returns `true` if
    /// a caller was waiting; `false` if the id is unknown (stray
    /// replies, replies for already-cancelled turns).
    pub async fn deliver(&self, result: ToolResult) -> bool {
        let mut g = self.pending.lock().await;
        match g.remove(&result.id) {
            Some(tx) => tx.send(result).is_ok(),
            None => false,
        }
    }

    /// Drop a pending registration without delivering. Used when a turn
    /// is interrupted before the tool replies.
    pub async fn cancel(&self, id: &str) {
        let mut g = self.pending.lock().await;
        g.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deliver_wakes_pending_register() {
        let broker = ToolBroker::new();
        let rx = broker.register("call_1".into()).await;
        let delivered = broker
            .deliver(ToolResult {
                id: "call_1".into(),
                output: Some("hi".into()),
                error: None,
            })
            .await;
        assert!(delivered);
        let r = rx.await.expect("oneshot");
        assert_eq!(r.output.as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn deliver_without_register_returns_false() {
        let broker = ToolBroker::new();
        let delivered = broker
            .deliver(ToolResult {
                id: "stray".into(),
                output: Some("x".into()),
                error: None,
            })
            .await;
        assert!(!delivered);
    }

    #[tokio::test]
    async fn cancel_removes_pending_entry() {
        let broker = ToolBroker::new();
        let rx = broker.register("call_1".into()).await;
        broker.cancel("call_1").await;
        let delivered = broker
            .deliver(ToolResult {
                id: "call_1".into(),
                output: Some("late".into()),
                error: None,
            })
            .await;
        assert!(!delivered);
        // The receiver still exists but its sender dropped — should
        // resolve to RecvError.
        assert!(rx.await.is_err());
    }

    #[tokio::test]
    async fn deliver_only_wakes_matching_id() {
        let broker = ToolBroker::new();
        let rx_a = broker.register("a".into()).await;
        let rx_b = broker.register("b".into()).await;
        broker
            .deliver(ToolResult {
                id: "b".into(),
                output: Some("B".into()),
                error: None,
            })
            .await;
        let b = rx_b.await.expect("b");
        assert_eq!(b.output.as_deref(), Some("B"));
        // a is still pending — fire it now.
        broker
            .deliver(ToolResult {
                id: "a".into(),
                output: Some("A".into()),
                error: None,
            })
            .await;
        let a = rx_a.await.expect("a");
        assert_eq!(a.output.as_deref(), Some("A"));
    }
}
