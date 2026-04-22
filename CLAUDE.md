# nefor — AI context

## What this is
Rust rewrite of nefor. Monorepo: pure algebra library + NCP-speaking engine + separate-process plugins (Rust or Lua). Terminal frontend and Claude-Code wrapper both ship as plugins.

## Layout
- `crates/nefor-combinators/` — library (pure Rust, minimal deps). Publishable standalone.
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types and parsers.
- `crates/nefor/` — engine binary. NCP broker + Lua host for `init.lua`. No UI, no harness.
- `plugins/nefor-tui/` — Rust NCP plugin: ratatui/crossterm terminal frontend.
- `plugins/nefor-chat/` — Rust NCP plugin: chat UI bridging `mock-plugin` ↔ `nefor-tui`.
- `plugins/mock-plugin/` — Rust NCP plugin wrapping the `claude` CLI; emits `cc.*` events.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `starter/init.lua` — legacy MVP config (single-process Lua). Superseded by plugin composition; rewrite pending.

## Conventions (enforced)
- Errors: `thiserror` for domain errors, `anyhow` only at the top boundary (`main.rs`).
- No `unwrap()` / `expect()` outside tests + exhausted-paths in `main.rs`.
- Newtype every domain ID (`PluginId`, `SessionId`, `TurnId`, `CapabilityId`).
- Enums (ADTs) for state; no boolean flags alongside sentinel variants.
- Immutability by default; I/O only at boundaries.
- No YAML/TOML/JSON config schema in core — config is `init.lua`.
- Plugins are separate OS processes communicating via NCP (see `protocol/v0.1/spec.md`). Any language. Lua stays embedded for `init.lua` composition.
- Comments only for non-obvious *why*; code is self-documenting for *what*.

## Commands
- `just run` — launch engine with default config.
- `just test` — workspace tests (all plugins + engine unit tests).
- `just lint` — clippy with `-D warnings`.
- `just fmt` — rustfmt.
- Manual smoke: `NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./tmp/smoke-config-m2` → real TUI + Claude streaming.

## Spec

## Milestone status
**M2 shipped** (Claude on screen). End-to-end: engine spawns `mock-plugin + nefor-chat + nefor-tui` as three processes, NCP brokers the events, prompts flow user → tui → chat → harness → Claude and responses stream back into a grid. `/resume` reloads prior session history from `~/.claude/projects/<escaped-cwd>/`. Clean exit via mouse wheel scroll, Ctrl+C, or terminal close.

Open work:
- Full-process NCP integration test suite (task-7, unblocked by mock-plugin).
- NCP throughput + backpressure benchmarks (task-8, deferred).
- Tool input/output rendering in nefor-chat (`cc.tool.start` only shows names today).
- Rewrite `starter/init.lua` to spawn the three-plugin graph — currently lives in `tmp/smoke-config-m2/`.
- Plugin-root resolver polish (XDG → dev fallback).

Deferred / not coming: DAG orchestrator, permission-gate UI, review-flow, persona system, hook runner, WASM runtime, bundled-config auto-install, plugin manager (Mason-style) — all post-MVP plugin-land.
