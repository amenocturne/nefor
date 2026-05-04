# starter/

Reference config for the nefor engine. This is also where NCP v0.1 protocol
semantics live — Slice 2 I4 moved the handshake / broadcast / replay logic out
of the Rust engine and into Lua. The engine is now a pure string-layer event
bus; everything NCP-shaped happens here.

## Layout

- `init.lua` — top-level composition. Sets `package.path`, defines the global
  `dispatch` hook (delegates to `ncp.dispatch`), and registers plugins via
  `nefor.plugins.spawn`. Edit this file to change which plugins run.
- `ncp.lua` — NCP v0.1 protocol module. Handles `ready` / `ready_ok`,
  broadcast-minus-sender, replay-on-attach, and `error` emission. JSON
  encode/decode go through the engine-provided `nefor.json` (serde_json
  bridged via mlua) — no pure-Lua JSON dependency.
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
- **Resume a prior session**: emit `sessions.resume_request { session_id =
  "<uuid>" }` on the bus (the chat slash-command surface does this for you).
  `starter/sessions.lua` handles the rest in-process. The legacy
  `nefor.parent_session` engine handoff has been removed.
- **Change protocol behavior**: `ncp.lua` is where handshake, broadcast,
  replay, and error rules live. Swap it for your own module if you need a
  non-standard dispatch policy.

## Known gaps (next slice)

- No graceful `shutdown` system-message emission. The engine still cascades
  process shutdown when any plugin exits; plugins observe EOF on their stdin
  and exit. Spec §5.3 `shutdown` message emission would require an
  `nefor.engine.on_shutdown(fn)` binding and is deferred.
- Per-plugin replay protocol — declarative replay annotation per plugin.
  Currently every late-attaching plugin sees `replay-on-attach` of all prior
  events; some plugins want a coalesced "we're back" signal instead. Tracked
  as D-21a-deferred.
