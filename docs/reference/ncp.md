# Nefor Composition Protocol reference

NCP is a convention implemented by configs, plugins, and the shipped [`lua/core/ncp.lua`](../../lua/core/ncp.lua). The engine is a pure string bus and is unaware of NCP envelopes or semantics. It is version-specific behavior, not yet a frozen external standard. Runtime code and focused tests are authoritative; [`docs/protocol.md`](../protocol.md) remains the detailed live behavior document.

NCP connects subprocesses. Lua actors and MAG actors are different in-process abstractions and do not perform the NCP handshake.

## Transport

A plugin is an OS subprocess whose stdin and stdout carry one complete JSON value per line. Stdout is bus traffic; stderr is logging.

The engine starts `command[0]` directly and passes the remaining strings as argv. It does not invoke a shell or provide per-plugin cwd/environment maps. Children inherit the engine cwd and environment, including resolved `NEFOR_CONFIG_DIR`, `NEFOR_DATA_DIR`, and `NEFOR_PLUGIN_DIR`. Use an explicit wrapper for shell features, environment setup, another cwd, supervision, or daemon bridging.

EOF/process death is the shutdown boundary. There is no NCP shutdown message synthesized on ordinary departure.

## Authored and delivered forms

Canonical plugin-authored lines omit transport identity:

```json
{"type":"system","body":{"kind":"ready","protocol_version":"0.1"}}
{"type":"event","body":{"kind":"example.event","value":1}}
```

A delivered bus envelope includes identity and time:

```json
{
  "type": "event",
  "from": "example",
  "ts": "...",
  "body": { "kind": "example.event", "value": 1 }
}
```

Current Lua accepts a plugin-supplied `from` on event lines for compatibility. Plugins must not rely on spoofing or overriding connection identity. The Rust protocol codec is intentionally stricter and rejects authored `from`/`ts`.

Event bodies must be JSON objects. The Rust engine treats them as strings; Lua NCP owns current envelope validation and routing semantics.

## Handshake

A plugin sends system `ready` with `protocol_version: "0.1"`. Lua replies directly with `ready_ok`, engine identity, timestamp, and the current engine protocol version. Events before readiness, duplicate readiness, and version mismatch produce direct error replies. Extra ready fields are currently tolerated. No ready timeout is shipped.

System messages are framework traffic. Ordinary plugin speech uses `type: "event"`.

## Publication and delivery

`nefor.engine.send(payload, target?)` publishes an entry to the in-memory bus log and causes Lua dispatch. `nefor.engine.deliver(peer, payload)` writes directly to one peer without publishing history. `deliver_batch(peer, payloads)` does the same for a finite ordered replay/restoration batch.

Default routing considers ready wrappers and then:

1. skips the source peer;
2. respects an explicit `send` target;
3. applies the legacy `<ready-peer>.` kind-prefix convention;
4. otherwise broadcasts to other ready peers.

A wrapper can still transform, drop, or replace a delivery. Publication therefore does not imply every plugin receives the entry.

The ordinary live writer queue is bounded and drops the oldest queued live line on overflow. `deliver_batch` preserves a finite materialized batch's ordering relative to earlier/later writes and close, but is not for open-ended streams.

## Replay

When a plugin becomes ready, `core.ncp` offers prior published bus events to its wrapper. This is in-memory reconstruction, not durable session resume. During session replay windows, wrappers receive `env.replay = true` and should avoid repeating external side effects.

Long-term persistence and resume are owned by Lua session actors. Direct deliveries are not bus history and are not replayed.

## Config wrapper API

Config authors normally use:

```lua
local ncp = require("core.ncp")

ncp.spawn {
  name = "example",
  command = { "/absolute/path/example", "--flag" },
  from_plugin = function(envs)
    for _, env in ipairs(envs) do
      nefor.engine.send(nefor.json.encode(env))
    end
  end,
  to_plugin = function(envs)
    for _, env in ipairs(envs) do
      nefor.engine.deliver("example", nefor.json.encode(env))
    end
  end,
  to_plugin_readonly = false,
}
```

`from_plugin(envs)` receives a batch of decoded plugin events after handshake processing and explicitly publishes desired entries. `to_plugin(envs)` receives bus envelopes offered to the wrapper and explicitly delivers desired lines. Return values are ignored. Omitting a callback selects default publish/delivery behavior.

Custom mutating callbacks receive isolated decoded values by default. `to_plugin_readonly = true` is an explicit no-mutation promise that allows sharing. Treat broker invocation helpers, `dispatch`, reset/test fields, and underscored actor fields as internal APIs.

Higher-level naming and request/response conventions belong to the [plugin authoring guide](../plugin-authoring.md).

## Rust helpers

Rust plugins may use `nefor-plugin-sdk` for handshake/event I/O and `nefor-protocol` for canonical codecs. They are conveniences, not requirements; any language that reliably reads and writes JSON Lines can implement a plugin.

## Evidence

- live behavior: [`docs/protocol.md`](../protocol.md)
- Lua framework: [`lua/core/ncp.lua`](../../lua/core/ncp.lua)
- behavior tests: `tests/lua/core/ncp_test.lua`
- engine transport: `engine/src/ncp/`
- Rust codecs: `crates/nefor-protocol/` and `crates/nefor-plugin-sdk/`

For authoring and operational patterns, continue with [Plugin authoring](../plugin-authoring.md).
