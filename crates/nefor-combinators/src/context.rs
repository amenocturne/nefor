//! The [`Context`] marker trait.
//!
//! Spec §Context Algebra: core ships zero opinions about what a Context
//! *contains*. It only requires that a Context can be cloned (for fan-out)
//! and passed between threads (for async runtimes). Everything else —
//! merge semantics, journals, ordering — lives in optional refinement
//! traits that land when a combinator genuinely needs them.

/// Marker trait for types usable as a Nefor context.
///
/// Any `Transform<C>` composes over a `C: Context`. Downstream users
/// implement this for their concrete context type; no blanket impl is
/// provided, matching the spec's explicit `impl Context for String {}`
/// example.
pub trait Context: Clone + Send + Sync + 'static {}

#[cfg(test)]
mod tests {
    use super::Context;

    // Newtype rather than `impl Context for String` so each test module in
    // the crate can define its own test types without coherence clashes.
    #[derive(Clone)]
    struct TestCtx;

    impl Context for TestCtx {}

    fn assert_context<C: Context>(_: &C) {}

    #[test]
    fn newtype_is_context() {
        let s = TestCtx;
        assert_context(&s);
    }
}
