//! `nefor.concurrency.*` bindings.
//!
//! # MVP scope
//!
//! This commit ships only `nefor.concurrency.sleep(ms)` — a cooperative
//! timer that yields the current Lua coroutine for `ms` milliseconds,
//! driven by `tokio::time::sleep`. It's the simplest proof that mlua's
//! `async` feature is wired correctly and that Lua code can `await`
//! asynchronous work without blocking the tokio runtime.
//!
//! `nefor.concurrency.spawn` / `.await` / `.join_all` / `.select` are
//! deliberately punted to the mock-plugin task (the first real caller that
//! needs them). The concurrency story there — where Lua must run in serial
//! with event handlers and stdio callbacks — is non-obvious enough that
//! we'd rather land it *alongside* the first consumer that drives the
//! requirements, not in isolation.
//!
//! # Spec mapping
//!
//! ```text
//! nefor.concurrency.sleep(ms)
//! ```
//!
//! Per spec §Core API Surface (Lua), §nefor binary ("Heavy work spawns onto
//! worker threads via `nefor.concurrency.spawn` which returns a Lua-side
//! handle; Lua awaits by yielding to the event loop, not blocking the main
//! thread."). `sleep` exercises exactly that yield-to-tokio pathway.

use std::time::Duration;

use mlua::{Lua, Table};

/// Install `nefor.concurrency.sleep` onto `nefor_tbl`.
///
/// The function must be called from a Lua coroutine (which it is when
/// invoked inside a handler reached via mlua's `call_async`). If called
/// from a non-coroutine context, mlua falls back to a no-op waker and the
/// future is polled-but-not-driven — effectively a busy wait until someone
/// else pumps the runtime. In practice all handler paths inside `nefor`
/// are already under tokio.
pub fn install_concurrency(lua: &Lua, nefor_tbl: &Table) -> mlua::Result<()> {
    let concurrency = lua.create_table()?;

    // nefor.concurrency.sleep(ms) — cooperative sleep.
    let sleep_fn = lua.create_async_function(|_, ms: u64| async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        Ok(())
    })?;
    concurrency.set("sleep", sleep_fn)?;

    nefor_tbl.set("concurrency", concurrency)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn setup() -> Lua {
        let lua = Lua::new();
        let nefor = lua.create_table().unwrap();
        install_concurrency(&lua, &nefor).unwrap();
        lua.globals().set("nefor", nefor).unwrap();
        lua
    }

    #[tokio::test]
    async fn sleep_yields_for_expected_duration() {
        let lua = setup();
        let start = Instant::now();
        lua.load("nefor.concurrency.sleep(50)")
            .exec_async()
            .await
            .expect("sleep ok");
        let elapsed = start.elapsed();
        // Generous lower bound — tokio timers are coarse. The point is that
        // sleep actually yields, not that timing is precise.
        assert!(
            elapsed >= Duration::from_millis(30),
            "sleep returned too fast: {elapsed:?}",
        );
    }

    #[tokio::test]
    async fn sleep_zero_returns_immediately() {
        let lua = setup();
        lua.load("nefor.concurrency.sleep(0)")
            .exec_async()
            .await
            .expect("zero sleep ok");
    }
}
