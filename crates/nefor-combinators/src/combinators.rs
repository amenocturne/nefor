//! Combinators over [`Reasoner<C>`](crate::Reasoner).
//!
//! MVP exposes exactly one: [`chain`]. Further combinators (`parallel`,
//! `fanout`, `retry`, `timeout`, `race`, ...) land when a concrete
//! consumer needs them — the spec explicitly warns against speculative
//! surface area (§Architectural Rules #14, §Core enums closed sets).

use crate::context::Context;
use crate::reasoner::Reasoner;

/// Sequential composition: apply `f` then `g`.
///
/// Only requires `C: Context`; no refinement traits. Errors from either
/// half surface as [`ChainError`] variants so the caller can distinguish
/// which step failed without string-sniffing.
pub fn chain<C, F, G>(f: F, g: G) -> Chain<F, G>
where
    C: Context,
    F: Reasoner<C>,
    G: Reasoner<C>,
{
    Chain { f, g }
}

/// Reasoner produced by [`chain`]. Exposed so callers can name the type
/// when storing chained reasoners in structs or returning them from
/// functions.
pub struct Chain<F, G> {
    f: F,
    g: G,
}

impl<C, F, G> Reasoner<C> for Chain<F, G>
where
    C: Context,
    F: Reasoner<C>,
    G: Reasoner<C>,
{
    type Err = ChainError<F::Err, G::Err>;

    async fn apply(&self, ctx: C) -> Result<C, Self::Err> {
        let mid = self.f.apply(ctx).await.map_err(ChainError::First)?;
        self.g.apply(mid).await.map_err(ChainError::Second)
    }
}

/// Error type for [`Chain`]. Distinguishes which stage failed.
///
/// The variant set is closed (`First`, `Second`); per the spec, adding
/// `#[non_exhaustive]` would signal uncertainty about the shape — not
/// the case here.
#[derive(Debug, thiserror::Error)]
pub enum ChainError<E1, E2>
where
    E1: std::error::Error + Send + Sync + 'static,
    E2: std::error::Error + Send + Sync + 'static,
{
    /// The first reasoner in the chain failed.
    #[error("first reasoner failed: {0}")]
    First(#[source] E1),
    /// The second reasoner in the chain failed.
    #[error("second reasoner failed: {0}")]
    Second(#[source] E2),
}

#[cfg(test)]
mod tests {
    use super::{chain, Chain, ChainError};
    use crate::context::Context;
    use crate::reasoner::Reasoner;
    use std::convert::Infallible;

    #[derive(Clone, Debug, PartialEq)]
    struct Ctx(String);

    impl Context for Ctx {}

    fn ctx(s: &str) -> Ctx {
        Ctx(s.to_owned())
    }

    struct Append(&'static str);

    impl Reasoner<Ctx> for Append {
        type Err = Infallible;

        async fn apply(&self, c: Ctx) -> Result<Ctx, Self::Err> {
            Ok(Ctx(c.0 + self.0))
        }
    }

    #[derive(Debug, thiserror::Error)]
    #[error("boom: {0}")]
    struct Boom(&'static str);

    struct FailFirst;
    impl Reasoner<Ctx> for FailFirst {
        type Err = Boom;
        async fn apply(&self, _c: Ctx) -> Result<Ctx, Self::Err> {
            Err(Boom("first"))
        }
    }

    struct FailSecond;
    impl Reasoner<Ctx> for FailSecond {
        type Err = Boom;
        async fn apply(&self, _c: Ctx) -> Result<Ctx, Self::Err> {
            Err(Boom("second"))
        }
    }

    #[tokio::test]
    async fn chain_composes_two_reasoners() {
        let pipeline: Chain<Append, Append> = chain(Append("a"), Append("b"));
        let out = pipeline.apply(ctx("hello")).await.expect("both infallible");
        assert_eq!(out, ctx("helloab"));
    }

    #[tokio::test]
    async fn chain_propagates_first_error() {
        let pipeline = chain(FailFirst, Append("x"));
        let err = pipeline.apply(ctx("hello")).await.expect_err("first fails");
        match err {
            ChainError::First(Boom("first")) => {}
            other => panic!("expected ChainError::First, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_propagates_second_error() {
        let pipeline = chain(Append("x"), FailSecond);
        let err = pipeline
            .apply(ctx("hello"))
            .await
            .expect_err("second fails");
        match err {
            ChainError::Second(Boom("second")) => {}
            other => panic!("expected ChainError::Second, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn chain_is_associative_at_value_level() {
        // (a . b) . c
        let left = chain(chain(Append("a"), Append("b")), Append("c"));
        // a . (b . c)
        let right = chain(Append("a"), chain(Append("b"), Append("c")));

        let lout = left.apply(ctx("x")).await.expect("infallible");
        let rout = right.apply(ctx("x")).await.expect("infallible");

        assert_eq!(lout, ctx("xabc"));
        assert_eq!(rout, ctx("xabc"));
        assert_eq!(lout, rout);
    }
}
