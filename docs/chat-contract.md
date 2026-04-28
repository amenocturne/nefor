# chat-contract v0.1

The event vocabulary `nefor-chat` consumes and emits. It is producer-agnostic: any LLM harness — `mock-plugin`, a future `ollama-harness`, a `example-harness` — can drive `nefor-chat` by hitting these event kinds. Harnesses that already speak a different namespace adapt via per-plugin transforms in `init.lua` (see [Adapting a non-conforming harness](#adapting-a-non-conforming-harness)).

This document is plugin-layer convention, not part of NCP. The protocol spec lives at [`protocol/v0.1/spec.md`](../protocol/v0.1/spec.md); ecosystem conventions for emitting and consuming events generally live in [`plugin-authoring.md`](./plugin-authoring.md).

## Required events

`nefor-chat` shows a fallback diagnostic in the transcript if no producer ever emits any of these during a session.

### `chat.message.append`

```json
{ "kind": "chat.message.append", "role": "user"|"assistant"|"system", "text": "…" }
```

Append a complete, non-streaming entry to the transcript. Used for user echoes the harness owns, system notices, and any single-shot assistant messages that aren't streamed through deltas.

### `chat.stream.delta`

```json
{ "kind": "chat.stream.delta", "text": "…" }
```

Append `text` to the in-flight assistant entry. Multiple deltas concatenate; `nefor-chat` creates the assistant entry on first delta if none is open.

### `chat.stream.end`

```json
{ "kind": "chat.stream.end", "text": "…" }
```

Finalize the in-flight assistant entry. If `text` is present it replaces the accumulated delta text (the harness's authoritative final), otherwise the accumulated text stands. Flips the chat plugin out of its "thinking…" pending state.

## User → harness

### `chat.input.submit`

```json
{ "kind": "chat.input.submit", "text": "…" }
```

Emitted by `nefor-chat` when the user hits Enter on a non-empty input buffer. The harness consumes this and starts a new turn; the conventional response is one or more `chat.stream.delta` events followed by a `chat.stream.end`.

## Optional events

Producers populate whichever subset they have. Missing fields render as `—` in the statusline or skip rendering entirely; missing kinds simply do not surface.

### `chat.session.stats`

```json
{
  "kind": "chat.session.stats",
  "model": "claude-…",                     // optional
  "turns": 7,                              // optional, u64
  "cumulative_cost_usd": 0.42,             // optional, f64
  "cumulative_input_tokens": 12345,        // optional, u64
  "cumulative_output_tokens": 6789,        // optional, u64
  "cumulative_cache_read": 0,              // optional, u64
  "cumulative_cache_creation": 0,          // optional, u64
  "last_turn_duration_ms": 1834            // optional, u64
}
```

Telemetry for the statusline. Each field present overwrites the prior value; absent fields preserve. The first arrival flips the `stats_seen` flag — that's the signal that a stats provider is wired, even if every field is absent.

### `chat.tool.start`

```json
{ "kind": "chat.tool.start", "name": "Read", "input": { … } }
```

The harness invoked a tool. `nefor-chat` renders this as a `[tool: <name>]` system entry; `input` is opaque and forwarded for future tool-pane consumers.

### `chat.tool.end`

```json
{ "kind": "chat.tool.end", "name": "Read", "output": <any> }
```

Tool returned. `output` is optional and free-form. Reserved in v1 — `nefor-chat` consumes the event but does not surface results in the transcript.

### `chat.history.replay`

```json
{
  "kind": "chat.history.replay",
  "session_id": "…",
  "entries": [ { "role": "user"|"assistant", "text": "…" }, … ]
}
```

The harness is replaying a prior session's transcript (typically in response to `chat.resume`). `nefor-chat` clears its current transcript and populates it from `entries` in the order given, then appends a `resumed · <count> messages · session <id>` system line.

### `chat.resume` (nefor-chat → harness)

```json
{ "kind": "chat.resume", "session_id": "…" }   // session_id optional
```

Emitted when the user runs `/resume` (most-recent session) or `/resume <id>` (specific session). The harness's expected response is a `chat.history.replay`. Harnesses that don't support resumption ignore the event.

## Adapting a non-conforming harness

A harness that emits its own native namespace (e.g. `mock-plugin` with `cc.stream.delta`, `cc.tool.start`) plugs into `nefor-chat` via per-plugin transforms registered on `ncp.spawn`. The transforms run in the engine's Lua step hook: `from_plugin` rewrites events at ingress before the broker broadcasts them, and `to_plugin` rewrites events at egress before they are delivered to that peer.

The reference example is [`starter/mock_plugin_adapter.lua`](../starter/mock_plugin_adapter.lua), wired in [`starter/init.lua`](../starter/init.lua). Its `from_plugin` rewrites `cc.stream.delta` → `chat.stream.delta`, `cc.stream.end` → `chat.stream.end` (stripping per-turn meta now carried on `cc.session.stats`), `cc.tool.start` → `chat.tool.start`, `cc.session.stats` → `chat.session.stats`, `cc.history.replay` → `chat.history.replay`, and folds `cc.turn.error` into a system `chat.message.append`. Its `to_plugin` rewrites `chat.input.submit` → `cc.prompt` and `chat.resume` → `cc.resume`.

A new harness gets its own adapter alongside this one; `nefor-chat` does not change. See [`plugin-authoring.md`](./plugin-authoring.md#per-plugin-transforms) for the transform contract — return semantics, per-peer isolation, error handling.
