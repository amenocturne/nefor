# Plugin authoring guide

This is a guide for writing nefor plugins. It documents ecosystem conventions that are not part of NCP (the protocol spec lives at [`protocol/v0.1/spec.md`](../protocol/v0.1/spec.md)). Following them is optional but helps your plugin interoperate with the rest of the ecosystem.

The spec tells you what you MUST do. This document tells you what you're well advised to do.

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

The engine does not validate plugin flags. The plugin is responsible for parsing its own argv with whatever library it likes. Users customise behaviour by editing the `command` array.

This is deliberate: a schema at the engine layer would ossify plugin CLIs and force every plugin into the same config shape. Freeform `command` keeps plugins independent and lets each pick the conventions that fit its domain.

## Structural interfaces, not nominal coupling

Plugins SHOULD declare the minimum input shape they consume and emit, and consumers SHOULD specify which peer satisfies that shape at spawn time. This is structural typing over the bus: a consumer needs "something that emits `*.grid.line` events," not "specifically the `nefor-tui` plugin."

Concretely:

- Advertise the kinds you consume/emit in your README or in a `*.manifest` event (see below).
- When a downstream plugin needs a specific peer's output, pass the peer name as a CLI arg rather than hard-coding it. Example: `my-chat --renderer nefor-tui` — the user can substitute `--renderer my-gui` without editing the chat plugin.

The cost is a small amount of plumbing at the composition layer. The payoff is that plugins compose without knowing each other's names.

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

System `kind` values defined by NCP are unprefixed — they are owned by the protocol spec, not by any plugin.

## Lifecycle conventions

NCP only defines `ready`, `ready_ok`, `shutdown`, and `error`. Everything else about the plugin lifecycle — "who's on the bus," "is my peer healthy," "what version are you" — is convention.

### Hello

After receiving `ready_ok`, a plugin MAY emit a `<name>.hello` event declaring its version and any other self-description:

```json
{ "type": "event", "from": "nefor-tui", "ts": "…",
  "body": { "kind": "nefor-tui.hello", "version": "0.1.0" } }
```

Peers that want a topology view subscribe to `*.hello` events across the bus. No engine mediation required.

### Goodbye

Before closing stdout, a plugin MAY emit a `<name>.goodbye` event with a reason:

```json
{ "type": "event", "from": "nefor-tui", "ts": "…",
  "body": { "kind": "nefor-tui.goodbye", "reason": "stream closed" } }
```

The engine doesn't relay `goodbye` events, but it does apply one policy when a connection drops: if the departed plugin was fully `ready` and other plugins are still alive, the broker broadcasts `shutdown` to them so the session winds down as a cooperating group. The rationale is that the reference compositions treat the plugin set as one unit — losing the terminal frontend, for example, shouldn't leave the Claude-harness hanging. Plugins that want to survive their peers (daemons, always-on helpers) should be spawned as a separate engine instance, not as part of the same graph.

### Heartbeat

A plugin concerned about peer liveness (e.g. "my renderer is stuck") can emit a periodic `<name>.heartbeat` event:

```json
{ "type": "event", "from": "mock-plugin", "ts": "…",
  "body": { "kind": "mock-plugin.heartbeat", "seq": 42 } }
```

Consumers tracking a peer's heartbeat can treat N missed beats as "probably gone." The frequency and tolerance are agreements between the interested parties.

## Manifest advertisement

A plugin MAY emit a single `<name>.manifest` event right after `ready_ok` (typically alongside `hello`) declaring what it consumes and emits:

```json
{ "type": "event", "from": "nefor-chat", "ts": "…",
  "body": {
    "kind": "nefor-chat.manifest",
    "version": "0.2.0",
    "accepts": ["mock-plugin.message_delta", "mock-plugin.tool_start"],
    "emits":   ["nefor-tui.grid.line", "nefor-tui.grid.flush"]
  } }
```

This lets peers make compatibility decisions up front ("I need a renderer that accepts `*.grid.line`; `nefor-tui` satisfies that") without hardcoding plugin names. It also gives observability plugins enough information to draw a graph of the live bus.

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

The engine's runner spawns `command[0]` directly with `std::process::Command`. No shell. No env map. No supervisor. No reconnect. If your plugin needs more lifecycle plumbing than "exec this binary with these args," you wrap it.

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

The engine inherits its environment to the child and nothing more. To inject env vars, invoke a wrapper script:

```lua
-- launcher.sh:
--   #!/bin/sh
--   export ANTHROPIC_API_KEY="$(pass anthropic/api-key)"
--   exec "$@"

nefor.plugins.spawn {
  name    = "mock-plugin",
  command = { "/home/user/bin/launcher.sh", "claude", "-p", "--output-format", "stream-json" },
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

A future `xp-runner` community plugin could dispatch per platform (reading `os.type()` in `init.lua` and picking a shell) if that pattern repeats enough to be worth abstracting. For now, do it yourself when you need it.

## More to come

This guide will grow as ecosystem conventions stabilise. If you find yourself implementing a pattern that feels universal — a way to report progress, a way to discover peer capabilities, a way to offer services — propose it here.
