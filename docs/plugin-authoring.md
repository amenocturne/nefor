# Plugin authoring guide

A Nefor plugin is an independent subprocess connected to the engine through NCP JSON Lines. This guide separates that process contract from the Lua wrapper API used by configuration authors. The concise wire reference is [`reference/ncp.md`](reference/ncp.md); [`protocol.md`](protocol.md) records detailed current implementation behavior. Neither is a frozen cross-version conformance specification.

## Independent process contract

### Command and environment

The composition assigns the plugin's connection name and command:

```lua
local ncp = require("core.ncp")

ncp.spawn {
  name = "example-provider",
  command = { "/absolute/path/example-provider", "--model", "small" },
}
```

Names should be lowercase alphanumeric with hyphens. Avoid dots because event kinds use `<plugin-name>.` prefixes.

The engine starts `command[0]` directly with the remaining entries as argv. It does not invoke a shell or manage per-plugin cwd/env maps. Children inherit the engine process cwd and environment. Resolved `NEFOR_CONFIG_DIR` and `NEFOR_DATA_DIR` are exported before spawn. Executable lookup belongs to the Lua composition or its distribution helper.

A plugin owns its CLI and settings schema. The engine validates the composition's `name` and non-empty command strings; it does not interpret plugin flags. Use an explicit shell/wrapper when custom environment, cwd, shell expansion, supervision, or a daemon bridge is required.

### JSON Lines and handshake

Stdout contains one complete JSON value per line. Stderr is logging. The canonical startup exchange is:

```json
{ "type": "system", "body": { "kind": "ready", "protocol_version": "0.1" } }
```

After direct `ready_ok`, publish events:

```json
{"type":"event","body":{"kind":"example-provider.hello","version":"1.0.0"}}
{"type":"event","body":{"kind":"example-provider.result","request_id":"42","value":"done"}}
```

Plugin-authored lines should omit `from` and `ts`; delivered bus envelopes add them. Current Lua accepts authored `from` for compatibility, but it is not an authority model and canonical Rust codecs reject that form. Never depend on spoofing connection identity.

Events before readiness, duplicate readiness, malformed envelopes, and protocol-version mismatch receive direct errors. There is no shipped ready timeout.

### Kinds and higher-level contracts

Prefix event kinds with the plugin name to avoid global collisions. NCP does not define provider, tool, request/response, or state schemas. Declare the minimum higher-level shapes the plugin consumes and emits, and keep peer selection in composition/CLI arguments rather than hard-coding another binary.

Optional ecosystem conventions include:

- `<name>.hello`, `.goodbye`, `.heartbeat`, and `.manifest` events;
- unique `request_id` plus exact response `in_reply_to`;
- advisory `body.to`.

These are ordinary event data. NCP does not enforce them, and `body.to` is not a routing primitive. Actual delivery may already be narrowed by explicit target, self-skip, ready-peer prefix, or wrappers.

### Lifecycle

Rust reports every spawned-process termination to Lua as one
`engine.plugin_process_terminated` fact with the composition-assigned plugin
identity and a closed `outcome`: `clean_exit`, `exit_code`, `signal`, `crash`,
`spawn_failure`, `transport_failure`, or `unknown`. Reporting is observational:
it does not close peers or select which processes are essential.

Composition requests engine shutdown explicitly with:

```lua
nefor.engine.shutdown {
  code = 0,
  reason = "operator requested shutdown",
  grace_ms = 2000,
}
```

The first request owns all three fields; later requests are ignored. The engine
notifies Lua before honoring a shutdown requested in reaction to a process
fact. During the single grace window, lifecycle subscribers and queued plugin
writes run before stdin EOF; remaining connections are force-closed at the
deadline. The shipped starter keeps its existing product behavior by requesting
shutdown with a 2000 ms grace whenever any spawned plugin terminates.

A plugin may publish a namespaced goodbye event before exiting, but the engine
does not synthesize an NCP shutdown message.

For restart/backoff, use an external supervisor or explicit wrapper. For a long-running daemon, spawn a small stdio bridge per engine session. Containers work when attached to stdin/stdout, for example:

```lua
ncp.spawn {
  name = "sandboxed-tool",
  command = { "docker", "run", "--rm", "-i", "image:tag" },
}
```

## Config wrapper API

`core.ncp.spawn` is a Lua config-author API, not part of what an independent plugin must implement:

```lua
ncp.spawn {
  name = "example",
  command = { bin("example") },

  from_plugin = function(envs)
    for _, env in ipairs(envs) do
      -- Translate or drop plugin-originated events.
      nefor.engine.send(nefor.json.encode({
        type = "event",
        from = env.from,
        ts = env.ts,
        body = env.body,
      }))
    end
  end,

  to_plugin = function(envs)
    for _, env in ipairs(envs) do
      -- Translate or drop events offered from bus history.
      nefor.engine.deliver("example", nefor.json.encode(env))
    end
  end,

  to_plugin_readonly = false,
}
```

Callbacks receive batches and perform side effects explicitly; their return values are ignored. Handshake messages do not reach `from_plugin`. Without callbacks, NCP applies its default publish/deliver paths.

Mutating custom callbacks receive isolated decoded values. `to_plugin_readonly = true` promises no mutation and permits shared values. During session replay, wrappers can inspect `env.replay` and suppress external side effects.

`nefor.engine.send` publishes bus history. `deliver` writes directly without publishing. `deliver_batch` is the lossless, ordered path for a finite materialized replay/restoration batch, not an unbounded stream.

Default routing skips self, honors an explicit `send` target, applies the legacy ready-peer kind-prefix rule, then broadcasts. Wrappers can transform or drop what they are offered.

Use [`lua/libs/compositors/provider.lua`](../lua/libs/compositors/provider.lua) as the production example for provider translation and [agent example customization](../examples/nefor-agent/docs/customization.md) for supported compositor seams. Broker invocation helpers, `dispatch`, reset/test hooks, underscored actor fields, and MAG kernel factories are internal APIs.

## Rust helpers

Rust authors can use:

- `nefor-plugin-sdk` for handshake and event I/O;
- `nefor-protocol` for canonical envelope/body codecs.

They are optional. Any language capable of reliable stdin/stdout JSON Lines can participate. The Rust authored-line parser is stricter than Lua compatibility behavior, which helps new plugins avoid accidentally supplying transport-owned fields.

## Operational wrappers

Shell features must be explicit:

```lua
ncp.spawn {
  name = "service",
  command = { "/bin/sh", "-c", "exec service --port \"$PORT\"" },
}
```

For secrets or environment setup, prefer a narrowly scoped launcher script that exports the values and `exec`s the real plugin. On Windows, invoke `cmd.exe /c` or PowerShell explicitly. Nefor itself makes no implicit shell choice.

For tool-specific integration, continue with [the example provider and tool guide](../examples/nefor-agent/docs/providers-and-tools.md).
