# fake-engine

Developer harness that impersonates a nefor engine over NCP stdio. Lets
plugins (notably `nefor-tui`) be developed and tested without running the
real engine.

## What it does

1. Spawns a plugin binary with stdin and stdout piped.
2. Reads the plugin's first line and validates it as a [§5.1 `ready`][spec]
   system message. Malformed ready → print parse error and exit 1.
3. Sends a `ready_ok` (with `engine_version: "fake-0.1.0"`) back on the
   plugin's stdin.
4. Then either:
   - **Passive mode** (no `--script`): stays connected and logs every
     message the plugin emits to the harness's stderr. Good for developing
     the plugin's input path.
   - **Script mode** (`--script path/to/script.jsonl`): plays back a
     sequence of engine-authored messages to drive the plugin, while still
     logging everything the plugin emits.
5. On plugin EOF / exit: prints a summary line and exits with the plugin's
   status code.
6. On ctrl-c: sends a `shutdown` system message with `grace_ms: 2000`,
   waits 2 s, then force-kills the plugin if it hasn't exited.

The harness's stderr is the main debugging affordance. Every received
plugin message is one line:

```
<ts> <from> <type>: <body-summary>
```

The fake-engine does not assign a plugin name from spawn-config (that's
a real-engine concern); it derives a display label from the binary's
file stem purely for its own logs.

[spec]: ../../protocol/v0.1/spec.md

## Usage

```
fake-engine path/to/plugin-binary [--script path/to/script.jsonl]
```

From the workspace root:

```
# Passive mode: ready and listen.
cargo run -p fake-engine -- target/debug/nefor-tui

# Script mode: play a JSONL event script into a plugin.
cargo run -p fake-engine -- target/debug/mock-plugin --script path/to/script.jsonl
```

## Script file format

`.jsonl` — one JSON value per line, `\n`-separated. Each line is one of:

- A **complete envelope** (all four fields: `type`, `from`, `ts`, `body`).
  Sent verbatim to the plugin. Use when you need precise control over
  `from` or `ts`, e.g. to simulate messages from other plugins on the bus.
- A **plugin-outgoing shape** (`{"type": ..., "body": ...}`). The harness
  stamps `from: "engine"` and a fresh `ts` before sending. This is the
  usual form for hand-written scripts.
- A **comment**: any line whose first non-whitespace character is `#`.
- A **sleep pragma**: `# sleep 500ms`, `# sleep 2s`. Pauses playback.

Example:

```jsonl
# Send a chat-style event, wait, then shut down.
{"type":"event","body":{"kind":"chat.input.submit","text":"hello"}}
# sleep 500ms
{"type":"system","body":{"kind":"shutdown","grace_ms":1000}}
```

For `nefor-tui`, current scripts should target the declarative Lua app loaded
with `nefor-tui --script <lua>` and send the app-specific events its
`update(msg, state)` handles. The old `nefor-tui.grid.*` protocol is not a
current TUI input API.

## Included scripts

No current script is part of the public contract. Treat any checked-in scripts
that use `nefor-tui.grid.*` as legacy examples for the pre-declarative TUI and
not as functional protocol documentation.

Passive mode (no `--script`) is equivalent to an empty script that never
terminates — the default choice when you just want to log plugin output.

## What it does NOT do

The fake engine is deliberately tiny. It does not:

- Broadcast events between multiple plugins (it drives one plugin at a
  time).
- Enforce backpressure or queue overflow.
- Validate plugin-emitted messages against any schema beyond the NCP
  envelope — unparseable lines are printed with an `<unparseable>` prefix
  rather than dropped silently.

When you need full engine semantics, run the real engine.
