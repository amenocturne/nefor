//! nefor-protocol — types and wire codec for the Nefor Composition Protocol.
//!
//! This crate models the envelope (§3) and the seven system message kinds
//! (§5) of [NCP v0.1][spec], plus encode/decode helpers for the JSON Lines
//! wire format. It is consumed by the engine broker and by plugin-side
//! Rust implementations.
//!
//! # Shape
//!
//! - [`Envelope`] — fully-stamped `{type, from, ts, body}`, as seen by
//!   plugin receivers and produced by the engine.
//! - [`PluginOutgoing`] — the reduced `{type, body}` form a plugin emits;
//!   the engine stamps `from` / `ts` before broadcast.
//! - [`SystemBody`] — the closed set of §5 bodies, tagged by `kind`.
//! - [`ParseError`] — the decoder's failure modes. The engine maps these
//!   to §8 [`ErrorCode`] values when reporting back to the offending
//!   sender; the mapping is intentionally outside this crate's surface.
//!
//! # Encoding
//!
//! [`Envelope::to_line`] / [`PluginOutgoing::to_line`] emit a single
//! compact JSON object per §10's canonical encoding guidance — no
//! insignificant whitespace, stable key ordering (`type, from, ts, body`
//! for envelopes). The caller owns newline framing.
//!
//! [spec]: https://github.com/amenocturne/nefor/blob/main/protocol/v0.1/spec.md

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod envelope;
mod newtypes;
mod parse;
mod system;

pub use envelope::{Body, Envelope, MessageKind, PluginOutgoing};
pub use newtypes::{PluginName, PluginNameError, Timestamp, TimestampParseError};
pub use parse::{InvalidAttachReason, InvalidBodyReason, ParseError, SystemBodyKind};
pub use system::{ErrorCode, Offending, PluginLeftReason, SystemBody};
