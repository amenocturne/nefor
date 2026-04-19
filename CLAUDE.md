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
- Plugins are Lua-only (WASM post-MVP). No Rust plugin API.
- Comments only for non-obvious *why*; code is self-documenting for *what*.

## Commands
- `just run` — launch nefor TUI.
- `just test` — workspace tests.
- `just lint` — clippy with `-D warnings`.
- `just fmt` — rustfmt.

## Spec

## MVP stop-line
mock-plugin + starter `init.lua` running a minimal chat TUI. DAG, permission-gate, review-flow, roles, widgets are all post-MVP.
