# nefor — AI context

## What this is
Rust rewrite of nefor. Monorepo: pure algebra library + NCP-speaking engine + separate-process plugins (Rust or Lua). Terminal frontend and Claude-Code wrapper both ship as plugins.

## Layout
- `crates/nefor-combinators/` — in-process algebra library (pure Rust, minimal deps). Trait shapes for Rust-native plugins. The *canonical* combinator library at runtime is the plugin, not the crate.
- `crates/nefor-protocol/` — NCP v0.1 envelope + system-body types and parsers. Used by plugins; engine stopped importing it in Slice 2.
- `crates/nefor/` — engine binary. Pure string-layer event bus: reads plugin stdin, stamps `{origin, ts}`, appends to session log, invokes a required Lua `step` hook, routes step's `nefor.engine.send` calls. All NCP semantics live in Lua now.
- `plugins/nefor-tui/` — Rust NCP plugin: ratatui/crossterm terminal frontend.
- `plugins/nefor-chat/` — Rust NCP plugin: chat UI bridging `mock-plugin` ↔ `nefor-tui`.
- `plugins/mock-plugin/` — Rust NCP plugin wrapping the `claude` CLI; emits `cc.*` events; declares `Context`/`Message` types + `Merge<Message>` handler.
- `plugins/nefor-combinators/` — Rust NCP plugin: type-aware combinator registry + executor (`Merge`, `Into`). Binary `nefor-combinators`; package `nefor-combinators-plugin`.
- `plugins/mock-plugin/` — scriptable NCP actor for integration tests.
- `tools/fake-engine/` — harness that impersonates the engine for plugin-side tests.
- `starter/init.lua` — glue: `package.path`, plugin spawn, `step` delegator.
- `starter/ncp.lua` — NCP v0.1 protocol implementation in Lua (handshake, broadcast, replay, errors). JSON encode/decode go through `nefor.json` (serde_json bridged via mlua).

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

**Slice 1 shipped** (combinator foundations). `nefor-combinators` plugin acts as type-aware registry + dispatch executor; mock-plugin declares `Context`/`Message` + `Merge<Message>` handler. Validated by `crates/nefor/tests/combinators_slice1.rs`.

**Slice 2 shipped** (engine = pure event bus + sessions). Engine no longer implements NCP; it reads stdin lines, stamps `{origin, ts}`, persists to `$XDG_DATA_HOME/nefor/sessions/<id>.jsonl`, and invokes a Lua `step(saved_log, current_log)` function. NCP v0.1 is implemented in `starter/ncp.lua`. `init.lua` can set `nefor.parent_session` to load a prior session into `saved_log`; user-authored step functions can impersonate plugins from the recording. Validated by `starter_smoke.rs`, reworked `combinators_slice1.rs`, and `session_impersonation.rs` (two-phase: record → impersonate). ~800 lines deleted from `broker.rs`; plugins untouched.


Open work (priority order):
- DAG scheduler plugin (`dag-scheduler`) — port with types + `Option<T>` wrapping.
- mock-plugin-as-Reasoner (id-correlated `Context → Message` invocation alongside broadcast `cc.prompt`).
- Session resumption semantics — declarative per-plugin replay protocol (read-only vs unsafe handlers).
- Leaf Reasoners: `at-file`, `review-terminal`, `review-file-annotation`.
- Graceful shutdown emission from starter (needs `nefor.engine.on_shutdown` hook).
- Tool input/output rendering in nefor-chat (`cc.tool.start` only shows names today).
- Plugin-root resolver polish (XDG → dev fallback).
- `ts` override on `nefor.engine.send` for causal fidelity during replay.

Deferred / not coming: permission-gate UI, persona system, hook runner, WASM runtime, bundled-config auto-install, plugin manager (Mason-style) — all post-MVP plugin-land.
