//! The [`Reasoner<C>`](Reasoner) trait — the one composition primitive.
//!
//! Spec §Context Algebra / §One Primitive, Enforced: harness, tool, skill,
//! agent, hook, review-flow are all `Reasoner<C>` for some `C`. There is
//! no `Harness` / `Tool` / `Skill` trait in core.

use crate::context::Context;

/// A fallible async transformation `C -> C`.
///
/// Implementors provide `apply`; the returned future must be `Send` so that
/// combinators can hand reasoners to multi-threaded runtimes. Uses native
/// `async fn` in traits (stable since Rust 1.75); no `async-trait` crate.
pub trait Reasoner<C: Context>: Send + Sync {
    /// The typed error this reasoner can produce.
    type Err: std::error::Error + Send + Sync + 'static;

    /// Apply the reasoner, consuming `ctx` and yielding a new one.
    fn apply(&self, ctx: C) -> impl std::future::Future<Output = Result<C, Self::Err>> + Send;
}

#[cfg(test)]
mod tests {
    use super::Reasoner;
    use crate::context::Context;
    use std::convert::Infallible;

    #[derive(Clone, Debug, PartialEq)]
    struct Ctx(String);

    impl Context for Ctx {}

    struct Append(&'static str);

    impl Reasoner<Ctx> for Append {
        type Err = Infallible;

        async fn apply(&self, ctx: Ctx) -> Result<Ctx, Self::Err> {
            Ok(Ctx(ctx.0 + self.0))
        }
    }

    #[tokio::test]
    async fn append_reasoner_appends() {
        let t = Append("!");
        let out = t.apply(Ctx(String::from("hi"))).await.expect("infallible");
        assert_eq!(out, Ctx(String::from("hi!")));
    }
}
