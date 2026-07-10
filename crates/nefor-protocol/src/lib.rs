//! nefor-protocol — types and wire codec for the Nefor Composition Protocol.
//!
//! This crate models NCP envelope and system-message shapes, plus
//! encode/decode helpers for the JSON Lines wire format. It is consumed
//! by plugin-side Rust implementations and tests; the shipped engine's
//! current behavior is summarized in [docs/protocol.md][spec].
//!
//! # Shape
//!
//! - [`Envelope`] — fully-stamped `{type, from, ts, body}`, as seen by
//!   plugin receivers and produced by the engine.
//! - [`PluginOutgoing`] — the reduced `{type, body}` form a plugin emits;
//!   the engine stamps `from` / `ts` before broadcast.
//! - [`SystemBody`] — supported system bodies, tagged by `kind`.
//! - [`ParseError`] — the decoder's failure modes. Error reporting policy
//!   is intentionally outside this crate's surface.
//!
//! # Encoding
//!
//! [`Envelope::to_line`] / [`PluginOutgoing::to_line`] emit a single
//! compact JSON object per §10's canonical encoding guidance — no
//! insignificant whitespace, stable key ordering (`type, from, ts, body`
//! for envelopes). The caller owns newline framing.
//!
//! [spec]: https://github.com/amenocturne/nefor/blob/main/docs/protocol.md

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod envelope;
mod newtypes;
mod parse;
mod system;

pub use envelope::{Body, Envelope, MessageKind, PluginOutgoing};
pub use newtypes::{PluginName, PluginNameError, Timestamp, TimestampParseError};
pub use parse::{InvalidBodyReason, InvalidReadyReason, ParseError, SystemBodyKind};
pub use system::{ErrorCode, Offending, SystemBody};
