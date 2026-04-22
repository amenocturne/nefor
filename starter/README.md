# starter/

Reference config for the nefor engine. This is also where NCP v0.1 protocol
semantics live — Slice 2 I4 moved the handshake / broadcast / replay logic out
of the Rust engine and into Lua. The engine is now a pure string-layer event
bus; everything NCP-shaped happens here.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global
  `step` hook (delegates to `ncp.step`), and registers plugins via
  `nefor.plugins.spawn`. Edit this file to change which plugins run.
- `ncp.lua` — NCP v0.1 protocol module. Handles `ready` / `ready_ok`,
  broadcast-minus-sender, replay-on-attach, and `error` emission.
- `lib/json.lua` — bundled rxi/json.lua (MIT). Do not edit; re-vendor from
  upstream if a newer version is needed.
- `ncp_test.lua` — Lua unit tests for `ncp.lua`. Driven by
  `crates/nefor/tests/starter_ncp_test.rs`; not run directly.

## Run

From the monorepo root:

```
NEFOR_PLUGIN_DIR=$PWD/plugins cargo run --bin nefor -- --config ./starter
```

The default composition spawns `mock-plugin`, `nefor-chat`, `nefor-tui`, and
`nefor-combinators`. Build them first:

```
cargo build -p mock-plugin -p nefor-chat -p nefor-tui -p nefor-combinators-plugin
```

## Customize

- **Add/remove plugins**: edit the `nefor.plugins.spawn { ... }` block at the
  end of `init.lua`.
- **Resume a prior session**: uncomment `nefor.parent_session = "<uuid>"` at
  the top of `init.lua` and fill in a prior session id from the engine log.
  (Note: parent-session replay is deferred; `saved_log` is ignored by
  `ncp.step` today.)
- **Change protocol behavior**: `ncp.lua` is where handshake, broadcast,
  replay, and error rules live. Swap it for your own module if you need a
  non-standard dispatch policy.

## Known gaps (next slice)

- No graceful `shutdown` system-message emission. The engine still cascades
  process shutdown when any plugin exits; plugins observe EOF on their stdin
  and exit. Spec §5.3 `shutdown` message emission would require an
  `nefor.engine.on_shutdown(fn)` binding and is deferred.
- `saved_log` (parent-session hydration) is accepted by `ncp.step` and
  ignored. Session-resumption semantics are tracked as D-21a-deferred.
