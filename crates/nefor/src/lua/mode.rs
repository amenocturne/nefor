//! `EngineMode` — closed enum describing how the engine binary was
//! invoked.
//!
//! Lives in the lua module rather than alongside the clap-derived `Cli`
//! because the bindings (`nefor.io`, dispatch helpers) are part of the
//! library surface and need this type without dragging clap in.
//! `cli.rs` re-exports `EngineMode` and provides the clap-driven
//! constructor.
//!
//! D-16: every state-bearing distinction is a closed enum, never a
//! boolean flag. Adding a future mode (e.g. `EngineMode::Eval` for a
//! one-shot Lua snippet) fails the compiler everywhere a decision needs
//! to be made.

/// Run-time mode the engine adopts after parsing CLI arguments.
#[derive(Debug, Clone)]
pub enum EngineMode {
    /// Default mode on bare `nefor` invocation: spawn plugins, run the
    /// broker until shutdown. The engine acts as the bus host; whether a
    /// UI plugin attaches is up to `init.lua`.
    Serve,
    /// `nefor plugin` (no name) — list registered plugins with a `cli`
    /// entry, exit 0. Engine boots, init.lua runs, spawn registry is
    /// inspected; no broker, no subprocess spawns.
    PluginList,
    /// `nefor plugin <name> [args...]` — boot the engine, spawn process
    /// plugins, then invoke the named plugin's `cli` function with `args`.
    /// Stdin of the engine binary is piped into `nefor.io.read_line()`.
    PluginDispatch {
        /// Plugin name to dispatch to. Resolved against the spawn registry.
        name: String,
        /// Positional args forwarded to the plugin's `cli` function as a
        /// 1-indexed Lua table.
        args: Vec<String>,
    },
}
