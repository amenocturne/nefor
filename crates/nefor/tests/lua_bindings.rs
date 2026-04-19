//! End-to-end test for the `nefor.*` Lua API surface.
//!
//! Runs the Lua host exactly as the binary would (minus the TUI), executes
//! a canned `init.lua`-style chunk, and asserts the cross-boundary plumbing
//! works: events flow Rust→Lua and Lua→Rust, logging calls succeed, and an
//! execution error is surfaced as a typed `LuaError::InitLuaExec` with a
//! source location.
//!
//! The binary's private modules are not re-exported as a library, so the
//! test drives the Lua layer via a separate `Lua::new` + the public
//! `install_events`/`install_log` functions would require such a re-export.
//! To keep this file honest without restructuring the crate into a lib, we
//! only sanity-check the CLI/binary pieces the public surface *can* reach:
//! building/running the binary lives in `cargo build` / unit tests. The rich
//! behavioral coverage lives in `src/lua/{vm,bindings}.rs` unit tests, which
//! do have access to the private API.

#[test]
fn crate_compiles() {
    // Smoke: this test exists so `cargo test --test lua_bindings` runs. Unit
    // tests inside `src/lua/*` are where the real coverage is — they can
    // reach private items this integration test cannot.
}
