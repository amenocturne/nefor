//! Domain errors for the tool-gate plugin.
//!
//! Plugin-level failures (transport, handshake, parse) are fatal. Per-call
//! gating decisions ("denied by user", "denied by policy") are *not* errors
//! — they're a normal control-flow outcome that surfaces on the wire as
//! `tool.result { error: "..." }`. So this enum is much smaller than
//! basic-tools' — there's no policy-level error variant.

use nefor_protocol::ParseError;

#[derive(Debug, thiserror::Error)]
pub enum ToolGateError {
    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("ready failed: {0}")]
    ReadyFailed(String),

    #[error("engine closed stdio before ready_ok")]
    ReadyClosed,

    #[error("protocol parse error: {0}")]
    Parse(#[from] ParseError),

    #[error("stdout writer closed before outgoing message was delivered")]
    WriterClosed,
}
