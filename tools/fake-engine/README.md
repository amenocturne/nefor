# fake-engine

Developer harness that impersonates a nefor engine over NCP stdio. Lets
plugins (notably `nefor-tui`) be developed and tested without running the
real engine.

## What it does

1. Spawns a plugin binary with stdin and stdout piped.
2. Reads the plugin's first line and validates it as a [§5.1 `attach`][spec]
   system message. Malformed attach → print parse error and exit 1.
3. Sends an `attach_ok` (with `engine_version: "fake-0.1.0"`) back on the
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

[spec]: ../../protocol/v0.1/spec.md

## Usage

```
fake-engine path/to/plugin-binary [--script path/to/script.jsonl]
```

From the workspace root:

```
# Passive mode: attach and listen.
cargo run -p fake-engine -- target/debug/nefor-tui

# Script mode: drive the plugin through a render.
cargo run -p fake-engine -- target/debug/nefor-tui --script tools/fake-engine/scripts/hello-world.jsonl
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

Example (from `scripts/hello-world.jsonl`):

```jsonl
# Draw "Hello, NCP!" to grid 1 then shut down.
{"type":"event","body":{"kind":"nefor-tui.default_colors","fg":16777215,"bg":0,"sp":16777215}}
{"type":"event","body":{"kind":"nefor-tui.grid.resize","grid":1,"width":80,"height":24}}
{"type":"event","body":{"kind":"nefor-tui.grid.clear","grid":1}}
{"type":"event","body":{"kind":"nefor-tui.grid.line","grid":1,"row":0,"col_start":0,"cells":[["Hello, NCP!",1,null]]}}
{"type":"event","body":{"kind":"nefor-tui.grid.flush"}}
# sleep 2s
{"type":"system","body":{"kind":"shutdown","grace_ms":1000}}
```

## Included scripts

| Script                   | Purpose                                                  |
| ------------------------ | -------------------------------------------------------- |
| `hello-world.jsonl`      | Basic render path: colors, highlight, grid, line, flush. |
| `echo-keys.jsonl`        | Minimal grid + hold-open; verify plugin input routing.   |

Passive mode (no `--script`) is equivalent to an empty script that never
terminates — the default choice when you just want to log plugin output.

## What it does NOT do

The fake engine is deliberately tiny. It does not:

- Broadcast events between multiple plugins (it drives one plugin at a
  time).
- Emit `plugin_joined` / `plugin_left` roster messages.
- Enforce backpressure or queue overflow.
- Validate plugin-emitted messages against any schema beyond the NCP
  envelope — unparseable lines are printed with an `<unparseable>` prefix
  rather than dropped silently.

When you need full engine semantics, run the real engine.
