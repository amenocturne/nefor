//! Bridge between inbound `tool.result` events and the in-flight turn
//! task that's awaiting them.
//!
//! Mirrors openai-provider's ToolBroker: the dispatch loop is the sole
//! reader of `tool.result`; the turn task that fired the
//! `<plugin>.tool.invoke` needs to block on the matching reply. The
//! broker is the seam — the turn task registers a oneshot keyed by the
//! tool-call id, the dispatcher fires it on receipt, the turn wakes.
//!
//! No timeout is enforced here. The turn task layers its own
//! cancellation (per-turn `CancellationToken`) + a wall-clock fallback
//! so a stuck tool can't wedge the chat slot indefinitely.

use std::collections::HashMap;

use tokio::sync::{oneshot, Mutex};

/// Result delivered from a `tool.result` event back to the awaiting
/// turn-runner. Exactly one of `output`/`error` is set per successful
/// reply (the chat-contract guarantees this; the broker doesn't check).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    pub id: String,
    pub output: Option<String>,
    pub error: Option<String>,
}

impl ToolResult {
    /// Render the result into a single string the model sees in the
    /// follow-up turn. Both branches are equally valid from the
    /// model's POV — "the tool said X" is the same shape whether or
    /// not it's flagged as an error.
    pub fn into_content(self) -> (String, bool) {
        if let Some(out) = self.output {
            (out, false)
        } else if let Some(err) = self.error {
            (err, true)
        } else {
            ("tool replied without output or error".to_string(), true)
        }
    }
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
        assert!(rx.await.is_err());
    }

    #[test]
    fn into_content_prefers_output_over_error() {
        let (text, is_err) = ToolResult {
            id: "x".into(),
            output: Some("ok".into()),
            error: None,
        }
        .into_content();
        assert_eq!(text, "ok");
        assert!(!is_err);
    }

    #[test]
    fn into_content_falls_back_to_error_string() {
        let (text, is_err) = ToolResult {
            id: "x".into(),
            output: None,
            error: Some("boom".into()),
        }
        .into_content();
        assert_eq!(text, "boom");
        assert!(is_err);
    }
}
