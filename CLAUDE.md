# nefor — AI context

## What this is
Rust rewrite of nefor. Monorepo: pure algebra library + NCP-speaking engine + separate-process plugins (Rust or Lua). Terminal frontend and Claude-Code wrapper both ship as plugins.

## Layout
- `crates/nefor-combinators/` — in-process algebra library (pure Rust, minimal deps). Trait shapes for Rust-native plugins. The *canonical* combinator library at runtime is the plugin, not the crate.
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types and parsers.
- `crates/nefor/` — engine binary (and a thin `lib.rs` exposing `ncp::*` for integration tests). NCP broker + Lua host + bus-wide event log with replay-on-attach.
- `plugins/nefor-tui/` — Rust NCP plugin: ratatui/crossterm terminal frontend.
- `plugins/nefor-chat/` — Rust NCP plugin: chat UI bridging `mock-plugin` ↔ `nefor-tui`.
- `plugins/mock-plugin/` — Rust NCP plugin wrapping the `claude` CLI; emits `cc.*` events; declares `Context`/`Message` types + `Merge<Message>` handler.
- `plugins/nefor-combinators/` — Rust NCP plugin: type-aware combinator registry + executor (`Merge`, `Into`). Binary is `nefor-combinators`; package is `nefor-combinators-plugin` (library crate already owns the `nefor-combinators` name).
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


Open work (priority order):
- DAG scheduler plugin (`dag-scheduler`) — port of the old scheduler with types + `Option<T>` wrapping.
- mock-plugin-as-Reasoner (id-correlated `Context → Message` invocation path alongside the existing broadcast `cc.prompt` flow).
- `replay` plugin — bus recorder + filtered replayer.
- Leaf Reasoners: `at-file`, `review-terminal`, `review-file-annotation`.
- Rewrite `starter/init.lua` — replace `tmp/smoke-config-m2/` as canonical reference.
- Tool input/output rendering in nefor-chat (`cc.tool.start` only shows names today).
- Extend full-process integration test suite beyond `combinators_slice1.rs`.
- Plugin-root resolver polish (XDG → dev fallback).

Deferred / not coming: permission-gate UI, persona system, hook runner, WASM runtime, bundled-config auto-install, plugin manager (Mason-style) — all post-MVP plugin-land.
