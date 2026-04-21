//! nefor-combinators — pure Rust substrate for `Reasoner<C>` composition.
//!
//! MVP scope: [`Context`], [`Reasoner<C>`](Reasoner), and the [`chain`]
//! combinator. Additional combinators (parallel, retry, timeout, ...) and
//! refinement traits (`Mergeable`, `Journaled`, `Sequenced`) land when a
//! concrete consumer requires them.
//!
//! See the Nefor Rust v1 spec — §Context Algebra — for the governing design.

#![deny(unsafe_code)]
#![warn(missing_docs)]

mod combinators;
mod context;
mod reasoner;

pub use combinators::{chain, Chain, ChainError};
pub use context::Context;
pub use reasoner::Reasoner;
