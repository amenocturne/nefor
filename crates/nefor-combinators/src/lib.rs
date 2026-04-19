//! nefor-combinators — pure Rust substrate for Transform<C> composition.
//!
//! MVP scope: Context, Transform<C>, and the combinators nefor binary + mock-plugin
//! actually need. Additional combinators (parallel, retry, timeout, etc.) and
//! refinement traits (Mergeable, Journaled, Sequenced) land when a concrete
//! consumer requires them.

#![deny(unsafe_code)]
#![warn(missing_docs)]
