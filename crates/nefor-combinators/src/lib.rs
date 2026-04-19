//! nefor-combinators — pure Rust substrate for `Transform<C>` composition.
//!
//! MVP scope: [`Context`], [`Transform<C>`](Transform), and the [`chain`]
//! combinator. Additional combinators (parallel, retry, timeout, ...) and
//! refinement traits (`Mergeable`, `Journaled`, `Sequenced`) land when a
//! concrete consumer requires them.
//!
//! See the Nefor Rust v1 spec — §Context Algebra — for the governing design.

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod combinators;
mod context;
mod transform;

pub use combinators::{chain, Chain, ChainError};
pub use context::Context;
pub use transform::Transform;
