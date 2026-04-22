//! Domain errors for the combinators plugin.
//!
//! `thiserror` for typed variants; no stringly-typed branching. The wire
//! `combinators.error` payload carries an [`ErrorCode`] discriminant so
//! callers can `match` on it exhaustively (spec D-16).

use nefor_protocol::ParseError;

/// Wire-level error codes surfaced on `combinators.error` events.
///
/// Closed set by design: adding a new failure mode is a protocol change,
/// not a free-form string. Every variant maps to exactly one `code` string
/// via [`ErrorCode::as_wire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// `combinators.register` referenced a `trait` name this plugin doesn't
    /// know.
    UnknownTrait,
    /// A trait implementation named a `type` that wasn't in the sender's
    /// `types` list.
    TypeNotDeclared,
    /// A registration entry was missing a required field or had the wrong
    /// shape.
    MalformedEntry,
    /// `combinators.run` referenced an op/type with no registered handler.
    NoHandlerRegistered,
    /// The handler returned an error (e.g. `*.error` reply).
    HandlerError,
    /// The handler didn't reply within the timeout window.
    HandlerTimeout,
    /// Wrong number of `inputs` for the requested op.
    BadArity,
    /// `combinators.run` named an op outside the closed v1 set.
    UnknownOp,
}

impl ErrorCode {
    /// Stable wire string for this code.
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::UnknownTrait => "unknown_trait",
            Self::TypeNotDeclared => "type_not_declared",
            Self::MalformedEntry => "malformed_entry",
            Self::NoHandlerRegistered => "no_handler_registered",
            Self::HandlerError => "handler_error",
            Self::HandlerTimeout => "handler_timeout",
            Self::BadArity => "bad_arity",
            Self::UnknownOp => "unknown_op",
        }
    }
}

impl std::fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_wire())
    }
}

/// All failure modes inside the combinators plugin.
#[derive(Debug, thiserror::Error)]
pub enum CombinatorsError {
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

    /// A `combinators.register` payload was structurally invalid in a way
    /// we couldn't repair. The enclosed code identifies the specific fault.
    #[error("register rejected: {code}: {message}")]
    RegisterRejected {
        /// Wire error code.
        code: ErrorCode,
        /// Human diagnostic.
        message: String,
    },

    /// A `combinators.run` payload was structurally invalid.
    #[error("run rejected: {code}: {message}")]
    RunRejected {
        /// Wire error code.
        code: ErrorCode,
        /// Human diagnostic.
        message: String,
    },

    /// Handler reported an error by wire (`*.error` reply).
    #[error("handler reported error: {0}")]
    Handler(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_code_has_stable_wire_name() {
        assert_eq!(ErrorCode::UnknownTrait.as_wire(), "unknown_trait");
        assert_eq!(ErrorCode::TypeNotDeclared.as_wire(), "type_not_declared");
        assert_eq!(ErrorCode::MalformedEntry.as_wire(), "malformed_entry");
        assert_eq!(
            ErrorCode::NoHandlerRegistered.as_wire(),
            "no_handler_registered"
        );
        assert_eq!(ErrorCode::HandlerError.as_wire(), "handler_error");
        assert_eq!(ErrorCode::HandlerTimeout.as_wire(), "handler_timeout");
        assert_eq!(ErrorCode::BadArity.as_wire(), "bad_arity");
        assert_eq!(ErrorCode::UnknownOp.as_wire(), "unknown_op");
    }

    #[test]
    fn rejection_variants_expose_their_wire_code() {
        let e = CombinatorsError::RegisterRejected {
            code: ErrorCode::MalformedEntry,
            message: "missing handler".into(),
        };
        match e {
            CombinatorsError::RegisterRejected { code, .. } => {
                assert_eq!(code, ErrorCode::MalformedEntry);
            }
            _ => panic!("unexpected variant"),
        }
    }
}
