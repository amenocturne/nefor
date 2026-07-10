# Plugin authoring guide

This is a guide for writing nefor plugins against the behavior shipped by the current engine and starter. The current protocol behavior is summarized in [`docs/protocol.md`](protocol.md); it is a live implementation document, not a frozen versioned conformance spec.

## Naming your plugin

Plugin names SHOULD be lowercase alphanumeric with hyphens — e.g., `my-plugin`, `fast-bus`, `some-harness`. Dots are reserved for the `kind` prefix convention below, so avoid them in plugin names.

Names are assigned at spawn time by the engine from `init.lua`. There is no wire-level name negotiation; the engine stamps `from` on every message from that connection. A name conflict (two plugins registered under the same name) surfaces at `init.lua` load time, before the engine spawns any process.

Pick something identifiable and unlikely to collide with common words.

## Settings are freeform

nefor has no per-plugin settings schema at the engine level. A plugin exposes whatever CLI it wants, and `init.lua` composes a command array:

```lua
nefor.plugins.spawn {
  name    = "my-harness",
  command = { "./bin/my-harness", "--model", "claude-opus", "--timeout", "30s" },
}
```

The engine validates only the spawn shape: `name` is required, `command` is an array of non-empty strings when present, duplicate names are rejected, and removed fields such as `args`, `env`, and `cwd` are rejected. The plugin is responsible for parsing its own argv. Users customise behaviour by editing the `command` array or by wrapping the command.

This is deliberate: a schema at the engine layer would ossify plugin CLIs and force every plugin into the same config shape. Freeform `command` keeps plugins independent and lets each pick the conventions that fit its domain.

## Structural interfaces, not nominal coupling

Plugins SHOULD declare the minimum input shape they consume and emit, and consumers SHOULD specify which peer satisfies that shape at spawn time. This is structural typing over the bus: a consumer needs "something that emits transcript events" or "a provider-compatible peer," not a hard-coded dependency on one binary.

Concretely:

- Advertise the kinds you consume/emit in your README or in a `*.manifest` event (see below).
- When a downstream plugin needs a specific peer's output, pass the peer name as a CLI arg rather than hard-coding it. Example: `my-chat --renderer nefor-tui` — the user can substitute `--renderer my-gui` without editing the chat plugin.

The cost is a small amount of plumbing at the composition layer. The payoff is that plugins compose without knowing each other's names.

## Composition wrappers

`lua/core/ncp.lua` owns the shipped handshake and default bus routing. Config code may register a wrapper for a subprocess with `ncp.spawn`:

```lua
local ncp = require("core.ncp")

ncp.spawn {
  name    = "example-provider",
  command = { bin("openai-provider"), "--name", "example-provider" },
  from_plugin = function(envs)
    for _, env in ipairs(envs) do
      -- Translate or drop plugin-originated events, then publish if desired.
      nefor.engine.send(nefor.json.encode({
        type = "event",
        from = env.from,
        ts   = nefor.engine.now(),
        body = env.body,
      }))
    end
  end,
  to_plugin = function(envs)
    for _, env in ipairs(envs) do
      -- Translate or drop bus events, then deliver if desired.
      nefor.engine.deliver("example-provider", nefor.json.encode({
        type = env.type,
        from = env.from,
        ts   = env.ts,
        body = env.body,
      }))
    end
  end,
}
```

These callbacks are batched, side-effecting callbacks, not return-value transforms:

- **`from_plugin(envs)`** receives a list of event envelopes decoded from that peer in the current tick. System handshake messages are handled by `core.ncp` and do not reach this callback. The callback decides what to publish with `nefor.engine.send`; returning a value has no effect.
- **`to_plugin(envs)`** receives a list of bus envelopes destined for that wrapper in the current dispatch tick. The callback decides what to write to the subprocess with `nefor.engine.deliver`; returning a value has no effect.

If a wrapper omits `from_plugin`, `core.ncp` publishes plugin event envelopes verbatim with `nefor.engine.send`. If it omits `to_plugin`, `core.ncp` delivers bus events verbatim to the peer after default filtering.

### Default routing

Default delivery offers published Step events to ready wrappers, then:

- skips self-emissions;
- respects an explicit target passed to `nefor.engine.send(payload, target)`;
- applies the legacy peer-prefix convention when `body.kind` starts with `<ready-peer>.`; and
- otherwise broadcasts to other ready peers.

`nefor.engine.deliver(peer, payload)` is direct delivery to one peer's stdin and does not append to the bus log. Use `send` for events that should become bus history; use `deliver` for targeted side effects.

### Per-peer isolation and replay

`core.ncp` deep-copies event bodies for each peer before calling `to_plugin`, so one wrapper's mutation does not leak into another wrapper's batch.

When a plugin completes the ready handshake, `core.ncp` replays prior bus-log events to that plugin's wrapper. Replay is an in-memory framework behavior; longer-term session resume is implemented by Lua session actors. During session replay windows, wrappers receive `env.replay = true` and can skip side-effecting work if replayed envelopes should not re-trigger external actions.

### Current provider composition

The shipped provider composition lives in [`lua/libs/compositors/provider.lua`](../lua/libs/compositors/provider.lua). It wraps OpenAI-compatible provider peers, delegates protocol translation to the provider library, publishes canonical `chat.*` events, delivers provider-prefixed commands, handles replay rebuilds, and connects provider stream/completion events to the starter's orchestration state.

Use that file as the current example for a production wrapper: it iterates batched `envs`, explicitly calls `nefor.engine.send` or provider delivery helpers, drops events by doing nothing, and treats replay as a wrapper concern.

## Kind namespacing

Event-message `kind` values SHOULD be prefixed with your plugin's name and a dot:

```
plugin-a.event_occurred
plugin-a.run_action
plugin-b.input_received
plugin-b.render_complete
plugin-c.state_changed
```

Why: a message's `kind` is global across the bus. If your plugin emits a `kind` without its name as a prefix, another plugin's message could collide with yours. The prefix convention makes kinds globally unique by piggybacking on already-unique plugin names.

System `kind` values handled by the NCP framework are unprefixed.

## Lifecycle conventions

The shipped framework handles a small system handshake: plugin sends `ready` with `protocol_version = "0.1"`; Lua replies directly with `ready_ok`; event traffic before readiness is rejected with a direct error. Everything else about the plugin lifecycle — "who's on the bus," "is my peer healthy," "what version are you" — is convention.

### Hello

After receiving `ready_ok`, a plugin MAY emit a `<name>.hello` event declaring its version and any other self-description:

```json
{
  "type": "event",
  "from": "example-plugin",
  "ts": "…",
  "body": { "kind": "example-plugin.hello", "version": "0.1.0" }
}
```

Peers that want a topology view subscribe to `*.hello` events across the bus. No engine mediation required.

### Goodbye

Before closing stdout, a plugin MAY emit a `<name>.goodbye` event with a reason:

```json
{
  "type": "event",
  "from": "example-plugin",
  "ts": "…",
  "body": { "kind": "example-plugin.goodbye", "reason": "finished" }
}
```

This is ordinary plugin-authored event traffic. The engine does not synthesize an NCP `shutdown` system message for plugins on normal peer departure.

Current broker behavior is cascade-close: when a subprocess exits while other peers are still alive, the broker requests engine shutdown, emits the engine-internal lifecycle shutdown for Lua subscribers, closes peer connections, and waits for the cooperative grace window. Plugins should treat stdin EOF / process termination as the shutdown signal. If a component is meant to survive peer exits, run it outside that engine session or behind a daemon/shim.

### Heartbeat

A plugin concerned about peer liveness can emit a periodic `<name>.heartbeat` event:

```json
{
  "type": "event",
  "from": "example-plugin",
  "ts": "…",
  "body": { "kind": "example-plugin.heartbeat", "seq": 42 }
}
```

Consumers tracking a peer's heartbeat can treat N missed beats as "probably gone." The frequency and tolerance are agreements between the interested parties.

## Manifest advertisement

A plugin MAY emit a single `<name>.manifest` event right after `ready_ok` (typically alongside `hello`) declaring what it consumes and emits:

```json
{
  "type": "event",
  "from": "example-plugin",
  "ts": "…",
  "body": {
    "kind": "example-plugin.manifest",
    "version": "0.2.0",
    "accepts": ["chat.input.submit"],
    "emits": ["chat.message.append", "chat.stream.delta"]
  }
}
```

This lets peers make compatibility decisions up front without hardcoding plugin names. It also gives observability plugins enough information to draw a graph of the live bus.

Manifests are purely informational: emitting one doesn't obligate the plugin to anything, and not emitting one doesn't prevent the plugin from working.

## Request/response pattern

NCP does not define a request/response primitive. When your plugin needs the pattern, implement it in `body`. The ecosystem convention looks like this:

```json
// Request (plugin-a asks plugin-b to run something):
{ "type": "event", "from": "plugin-a", "ts": "…",
  "body": { "kind": "plugin-b.run_action",
            "request_id": "plugin-a:42",
            "args": { … } }}

// Response (plugin-b answers):
{ "type": "event", "from": "plugin-b", "ts": "…",
  "body": { "kind": "plugin-b.run_action_ok",
            "in_reply_to": "plugin-a:42",
            "result": { … } }}
```

Conventions:

- `request_id` SHOULD be unique within the sender. A common scheme: `<plugin-name>:<counter>`.
- `in_reply_to` SHOULD echo the request_id exactly.

The requesting plugin filters incoming events by `from`, `kind`, and `in_reply_to` to match responses to requests.

## Addressed messages

To hint that a message is directed at a specific recipient, include an advisory `to` field in body:

```json
{ "body": { "kind": "…", "to": "plugin-b", … } }
```

This is a hint, not a filter. Every plugin still receives the message (the bus is broadcast); other plugins can fast-skip if `body.to` is present and not equal to their own name. Useful for request/response flows where the receiver-role is unambiguous.

## Supervision and daemon patterns

The engine's runner starts `command[0]` directly with `std::process::Command` and passes `command[1..]` as args. It does not invoke a shell, set a per-plugin cwd, or manage per-plugin env maps. Child processes inherit the engine process cwd and environment. The engine resolves and exports `NEFOR_CONFIG_DIR`, `NEFOR_DATA_DIR`, and `NEFOR_PLUGIN_DIR` before spawning so Lua configs and child processes see the same roots.

If your plugin needs shell features, custom environment setup, a different working directory, supervision, or daemon reconnect behavior, wrap it explicitly.

### Shell features via explicit invocation

If you need expansions, pipes, or shell built-ins, invoke the shell yourself:

```lua
nefor.plugins.spawn {
  name    = "my-plugin",
  command = { "/bin/sh", "-c", "exec my-daemon --port $PORT" },
}
```

Users on Windows can invoke `cmd.exe /c` or `powershell.exe -Command` with the same pattern. See the "Cross-platform" section below.

### Environment variables

To inject env vars, invoke a wrapper script:

```lua
-- launcher.sh:
--   #!/bin/sh
--   export ANTHROPIC_API_KEY="$(pass anthropic/api-key)"
--   exec "$@"

nefor.plugins.spawn {
  name    = "service-plugin",
  command = { "./launcher.sh", "service-plugin-bin", "--mode", "ncp" },
}
```

### Supervision

If your plugin needs automatic restart, backoff, or liveness checks beyond what nefor provides, delegate to a supervisor plugin in the ecosystem or use an OS-level supervisor (systemd user unit, launchd agent, runit). Example wrapper that restarts up to N times on exit:

```sh
#!/bin/sh
# simple-supervise.sh <max_restarts> <cmd...>
max=$1; shift
count=0
until [ "$count" -ge "$max" ]; do
  "$@" && break
  count=$((count + 1))
  sleep 1
done
```

Spawned as:

```lua
nefor.plugins.spawn {
  name    = "flaky-harness",
  command = { "/usr/local/bin/simple-supervise.sh", "3",
              "./bin/flaky-harness", "--mode", "batch" },
}
```

### Daemon reconnect

If your plugin talks to a long-running daemon over a UNIX socket or TCP port, write a tiny shim that bridges stdio to the daemon:

```sh
#!/bin/sh
# Bridge stdio to a UNIX socket the daemon owns.
exec socat - UNIX-CONNECT:/run/my-daemon.sock
```

Now every time the engine spawns the plugin, you get a fresh stdio pair into the same daemon.

### Docker / containerisation

If your plugin lives in a container:

```lua
nefor.plugins.spawn {
  name    = "sandboxed-tool",
  command = { "docker", "run", "--rm", "-i",
              "my-org/sandboxed-tool:latest", "--mode", "ncp" },
}
```

The container is exec'd like any other binary. Docker's `-i` plus `--rm` gives you a one-shot stdio-attached child that cleans up when the engine closes the connection.

## Cross-platform

The engine never invokes a shell, so it has no Unix-specific assumptions. Plugins that need shell features invoke one explicitly:

- Linux / macOS: `command = { "/bin/sh", "-c", "…" }`
- Windows: `command = { "cmd.exe", "/c", "…" }` or `command = { "powershell.exe", "-Command", "…" }`
