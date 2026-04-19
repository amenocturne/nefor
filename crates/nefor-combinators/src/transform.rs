//! The [`Transform<C>`](Transform) trait — the one composition primitive.
//!
//! Spec §Context Algebra / §One Primitive, Enforced: harness, tool, skill,
//! agent, hook, review-flow are all `Transform<C>` for some `C`. There is
//! no `Harness` / `Tool` / `Skill` trait in core.

use crate::context::Context;

/// A fallible async transformation `C -> C`.
///
/// Implementors provide `apply`; the returned future must be `Send` so that
/// combinators can hand transforms to multi-threaded runtimes. Uses native
/// `async fn` in traits (stable since Rust 1.75); no `async-trait` crate.
pub trait Transform<C: Context>: Send + Sync {
    /// The typed error this transform can produce.
    type Err: std::error::Error + Send + Sync + 'static;

    /// Apply the transformation, consuming `ctx` and yielding a new one.
    fn apply(&self, ctx: C) -> impl std::future::Future<Output = Result<C, Self::Err>> + Send;
}

#[cfg(test)]
mod tests {
    use super::Transform;
    use crate::context::Context;
    use std::convert::Infallible;

    #[derive(Clone, Debug, PartialEq)]
    struct Ctx(String);

    impl Context for Ctx {}

    struct Append(&'static str);

    impl Transform<Ctx> for Append {
        type Err = Infallible;

        async fn apply(&self, ctx: Ctx) -> Result<Ctx, Self::Err> {
            Ok(Ctx(ctx.0 + self.0))
        }
    }

    #[tokio::test]
    async fn append_transform_appends() {
        let t = Append("!");
        let out = t.apply(Ctx(String::from("hi"))).await.expect("infallible");
        assert_eq!(out, Ctx(String::from("hi!")));
    }
}
