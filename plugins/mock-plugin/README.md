# mock-plugin

A test/dev NCP peer plugin. Speaks [NCP v0.1](../../protocol/v0.1/spec.md) over stdio and drives its behaviour from a user-supplied Lua script. Use it to exercise the engine or other plugins without having to ship real functionality: the script decides what events to emit, what to do with incoming events, and how to react to the lifecycle hooks.

## Usage

```
mock-plugin --script <path-to-lua-file>
```

Example:

```
mock-plugin --script plugins/mock-plugin/scenarios/minimal.lua
```

The binary reads its stdin / writes its stdout as the NCP channel, and logs to stderr. The script is loaded once, before the handshake, so syntax errors and top-level script faults surface immediately without ever opening the wire.

The plugin name is hardcoded as `mock-plugin` (exposed to Lua as `nefor.name`). If you need multiple `mock-plugin` instances on the same bus under different identities, extend the binary with a `--name` flag.

## Lua API

All entry points live under a global `nefor` table.

### Identity and state

| Entry         | Type   | Description                                                                                                          |
| ------------- | ------ | -------------------------------------------------------------------------------------------------------------------- |
| `nefor.name`  | string | The plugin's wire identity. Read-only.                                                                               |
| `nefor.state` | string | Current lifecycle state: `"awaiting_ready_ok"`, `"ready"`, or `"shutting_down"`. Reads always return the live value. |

### Handlers

All handlers are optional — a script with no handlers loads fine and does nothing.

| Entry                   | Description                                                                                                                                                                                                               |
| ----------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `nefor.on(kind, fn)`    | Register a handler for a specific event `kind`. The callback is called with `(body_table, envelope_table)` where `envelope_table` has `type`, `from`, and `ts`. `kind` is matched literally (e.g. `"mock-plugin.delta"`). |
| `nefor.on_any(fn)`      | Register a catch-all handler, called for every event after any specific `on` handler fires.                                                                                                                               |
| `nefor.on_ready_ok(fn)` | Called once, immediately after the engine's `ready_ok` arrives. Takes no args.                                                                                                                                            |
| `nefor.on_shutdown(fn)` | Called once when the engine asks us to shut down (or stdin closes, or a signal arrives). Takes no args. The plugin exits after the handler returns.                                                                       |

### Emitting events

| Entry                              | Description                                                                                                                                                                                                                                                                                       |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `nefor.emit(sub_kind, body?)`      | Emit an event with `kind = "<nefor.name>.<sub_kind>"`. `body` is an optional table; if omitted, an empty body is used. **Error** if `body.kind` is already set (scripts must not bypass the host's prefix; use `emit_raw` if you really mean to). **Error** if emitted before `ready_ok` arrives. |
| `nefor.emit_raw(full_kind, body?)` | Emit with `body.kind = full_kind` verbatim — no prefixing. Useful for impersonating other plugins' event shapes in tests. Caveat: nothing stops you from sending malformed or namespace-colliding kinds; that's on you.                                                                           |

### Utilities

| Entry             | Description                                                                                                                                                                                                                      |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `nefor.sleep(ms)` | Async sleep. Must be called from inside an async handler context — i.e. from an `on`, `on_any`, `on_ready_ok`, or `on_shutdown` callback. Calling it at script top level panics because the script isn't inside a coroutine yet. |
| `nefor.log(msg)`  | Write `msg` to stderr via `tracing::info!`. Never use Lua's `print` — it goes to stdout, which would corrupt the NCP stream.                                                                                                     |

## Handshake and lifecycle

1. **Script load.** The binary reads `--script` and executes it once with `nefor.state == "awaiting_ready_ok"`. Scripts use this phase to register handlers; emitting is an error here.
2. **Ready.** The binary sends `{type: "system", body: {kind: "ready", protocol_version: "0.1"}}` and waits for `ready_ok`.
3. **Ready_ok.** `nefor.state` flips to `"ready"`. The `on_ready_ok` handler fires.
4. **Runtime.** For each incoming event envelope, specific `on(kind)` handlers fire, then `on_any`. Scripts can emit events freely.
5. **Shutdown.** When the engine sends `shutdown`, stdin closes, or a signal arrives, `nefor.state` flips to `"shutting_down"`, `on_shutdown` fires, and the process exits 0.

## Bundled scenarios

Live under `scenarios/`:

- **`minimal.lua`** — The smallest useful scenario. On `ready_ok`, emits a single `mock-plugin.hello` event. On `shutdown`, logs and exits. Good smoke test that the binary and your engine handshake cleanly.
- **`echo.lua`** — Subscribes to everything via `on_any`. For each event, emits a `mock-plugin.echo` event naming the original's `kind` and `from`. Skips its own echoes as a self-loop guard. Good for round-trip-delivery tests.
- **`cc-like.lua`** — Simulates a `mock-plugin`-style streaming response. On `ready_ok`, emits a sequence of `mock-plugin.delta` events (separated by `nefor.sleep`), then a terminal `mock-plugin.result`. Good for testing renderers or consumers of streamed output without needing to wire up the real LLM.

## Stdio discipline

- **stdout** is the NCP channel — JSON Lines, UTF-8, `\n`-terminated. Nothing else writes to it.
- **stderr** is the diagnostic channel — `tracing`'s default env filter is `info`. Set `RUST_LOG=debug` for more.
- **stdin** is the inbound NCP channel. EOF on stdin is treated as shutdown.

## Quality

- `cargo build -p mock-plugin` clean.
- `cargo test -p mock-plugin` green (18 unit tests, 3 integration tests spawning the binary).
- `cargo clippy -p mock-plugin -- -D warnings` clean.
- No `unwrap()` / `expect()` outside tests.
