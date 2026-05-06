//! Domain errors for the reasoner-graph plugin.
//!
//! `thiserror` for typed variants. Wire-level error codes live as a closed
//! enum mirroring nefor-combinators' `ErrorCode` style: every variant maps
//! to exactly one kebab-case wire string via [`ErrorCode::as_wire`]. Adding
//! a new code is a protocol change, not a free-form string.

use nefor_protocol::ParseError;

/// Wire-level error codes the scheduler reports inside synthetic node
/// results in `graph.run_complete` (under `_error`, `_typecheck`,
/// `_missing_combinators`, etc.) and as node-level errors when the
/// scheduler manufactures a failure (e.g. reasoner not connected at
/// dispatch, ack timeout).
///
/// Closed set by design: extending it is a protocol change. Currently the
/// state machine emits free-form `error` strings; this enum is the
/// machine-readable side that callers can match against once the wire
/// shape grows a `code` field. Kept now so the discriminant set stays
/// authoritative in one place.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// Submitted graph is structurally invalid (duplicate ids, dangling
    /// edges, missing fields). Reported under synthetic `_error` node.
    MalformedGraph,
    /// A node became runnable but its named reasoner is not connected.
    ReasonerNotConnected,
    /// The reasoner did not reply with `tool.result` within
    /// `ack_deadline_ms` of dispatch. (Tool contract has no acks; this
    /// is the deadline-on-result watchdog per wire-spec D1.)
    AckTimeout,
    /// Submit-time type check failed (slot mismatches, duplicate output
    /// types in fanout multiset, etc.). Reported under synthetic
    /// `_typecheck` node. Reserved — full implementation lands with T6.
    TypecheckFailed,
    /// One or more combinators referenced by the graph weren't found in
    /// nefor-combinators' registry. Reported under synthetic
    /// `_missing_combinators` node. Reserved — full implementation lands
    /// with T6.
    MissingCombinators,
}

impl ErrorCode {
    /// Stable wire string for this code.
    #[allow(dead_code)]
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::MalformedGraph => "malformed_graph",
            Self::ReasonerNotConnected => "reasoner_not_connected",
            Self::AckTimeout => "ack_timeout",
            Self::TypecheckFailed => "typecheck_failed",
            Self::MissingCombinators => "missing_combinators",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// All failure modes inside the reasoner-graph plugin.
#[derive(Debug, thiserror::Error)]
pub enum ReasonerGraphError {
    /// I/O error on stdio or inside a transport task.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// Engine rejected our ready handshake, or closed before replying.
    #[error("ready failed: {0}")]
    ReadyFailed(String),

    /// Stdin closed before we saw `ready_ok`.
    #[error("engine closed stdio before ready_ok")]
    ReadyClosed,

    /// Wire-format decode failure we could not recover from.
    #[error("protocol parse error: {0}")]
    Parse(#[from] ParseError),

    /// The writer task exited before the outgoing channel drained.
    #[error("stdout writer closed before outgoing message was delivered")]
    WriterClosed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_stable_wire_name() {
        assert_eq!(ErrorCode::MalformedGraph.as_wire(), "malformed_graph");
        assert_eq!(
            ErrorCode::ReasonerNotConnected.as_wire(),
            "reasoner_not_connected"
        );
        assert_eq!(ErrorCode::AckTimeout.as_wire(), "ack_timeout");
        assert_eq!(ErrorCode::TypecheckFailed.as_wire(), "typecheck_failed");
        assert_eq!(
            ErrorCode::MissingCombinators.as_wire(),
            "missing_combinators"
        );
    }
}
