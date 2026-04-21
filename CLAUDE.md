# nefor — AI context

## What this is
Rust rewrite of nefor. Monorepo: pure algebra library + TUI binary + Lua plugins.

## Layout
- `crates/nefor-combinators/` — library (pure Rust, minimal deps). Publishable standalone.
- `crates/nefor/` — binary. Imports the library; wraps it in TUI + mlua + tokio.
- `plugins/<name>/` — Lua plugin directories. First one: `mock-plugin` (spawns `claude`).
- `starter/` — reference `init.lua`; not auto-installed.

## Conventions (enforced)
- Errors: `thiserror` for domain errors, `anyhow` only at the top boundary (`main.rs`).
- No `unwrap()` / `expect()` outside tests + exhausted-paths in `main.rs`.
- Newtype every domain ID (`PluginId`, `SessionId`, `TurnId`, `CapabilityId`).
- Enums (ADTs) for state; no boolean flags alongside sentinel variants.
- Immutability by default; I/O only at boundaries.
- No YAML/TOML/JSON config schema in core — config is `init.lua`.
- Plugins are separate OS processes communicating via NCP (see `protocol/v0.1/spec.md`). Any language. Lua stays embedded for `init.lua` and lightweight in-engine composition.
- Comments only for non-obvious *why*; code is self-documenting for *what*.

## Commands
- `just run` — launch nefor TUI.
- `just test` — workspace tests.
- `just lint` — clippy with `-D warnings`.
- `just fmt` — rustfmt.

## Spec

## MVP status
MVP complete (see git log for the landing commit).

Shipped:
- `nefor-combinators`: `Context`, `Reasoner<C>`, `chain`.
- `nefor` binary: clap CLI, XDG config, tokio runtime, ratatui TUI with region layout, event bus, mlua 5.4 embedding, subprocess binding.
- `mock-plugin` plugin: spawns `claude -p --output-format stream-json`; streams deltas, tool starts, final result; session resume via `--resume`.
- `starter/init.lua`: chat TUI driving mock-plugin.

Not shipped (post-MVP): DAG orchestrator, permission-gate, review-flow, role prompts, behavioral reminders, hook runner, MLG persona, Nestor harness, WASM runtime, Tauri GUI, bundled-config auto-install, plugin manager.
