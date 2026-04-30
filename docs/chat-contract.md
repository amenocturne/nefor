# chat-contract v0.1

The event vocabulary `nefor-chat` consumes and emits. It is producer-agnostic: any LLM harness — `mock-plugin`, a future `ollama-harness`, a `example-harness` — can drive `nefor-chat` by hitting these event kinds. Harnesses that already speak a different namespace adapt via per-plugin transforms in `init.lua` (see [Adapting a non-conforming harness](#adapting-a-non-conforming-harness)).

This document is plugin-layer convention, not part of NCP. The protocol spec lives at [`protocol/v0.1/spec.md`](../protocol/v0.1/spec.md); ecosystem conventions for emitting and consuming events generally live in [`plugin-authoring.md`](./plugin-authoring.md).

## Required events

`nefor-chat` shows a fallback diagnostic in the transcript if no producer ever emits any of these during a session.

### `chat.message.append`

```json
{ "kind": "chat.message.append", "role": "user"|"assistant"|"system", "text": "…" }
```

Append a complete, non-streaming entry to the transcript. Used for user echoes the harness owns, system notices, and any single-shot assistant messages that aren't streamed through deltas. Empty `text` is dropped: a blank entry has nothing to render and only confuses per-role cadence (e.g. an empty assistant entry would still get the model+duration footer stamped).

### `chat.stream.delta`

```json
{ "kind": "chat.stream.delta", "text": "…" }
```

Append `text` to the in-flight assistant entry. Multiple deltas concatenate; `nefor-chat` creates the assistant entry on first delta if none is open.

### `chat.stream.end`

```json
{
  "kind": "chat.stream.end",
  "text": "…",
  "model": "claude-…",       // optional
  "duration_ms": 1500          // optional, u64
}
```

Finalize the in-flight assistant entry. If `text` is present it replaces the accumulated delta text (the harness's authoritative final), otherwise the accumulated text stands. Flips the chat plugin out of its "thinking…" pending state.

`model` and `duration_ms` are pinned to this specific turn (unlike the cumulative `chat.session.stats` view) and drive the per-turn footer beneath the assistant body. Either or both render: `▣ <model> · <duration>` when both are present, `▣ <model>` or `▣ <duration>` when only one is. With neither, no footer renders. The model-only case is the typical resume-replay shape — claude's session log records `message.model` per assistant frame but doesn't record per-turn wall-clock duration.

## User → harness

### `chat.input.submit`

```json
{ "kind": "chat.input.submit", "text": "…" }
```

Emitted by `nefor-chat` when the user hits Enter on a non-empty input buffer. The harness consumes this and starts a new turn; the conventional response is one or more `chat.stream.delta` events followed by a `chat.stream.end`.

### `chat.interrupt`

```json
{ "kind": "chat.interrupt" }
```

Emitted by `nefor-chat` when the user hits Esc *while a turn is in flight* (i.e. between the previous `chat.input.submit` and its terminating `chat.stream.end`). The harness is expected to abort the running turn and send a terminating `chat.stream.end` (with `text: ""`, leaving any partial deltas in place) so the chat plugin's normal finalize codepath winds the turn down.

The visual marker convention is a system-role `chat.message.append` with `text: "[interrupted]"` appended after the partial assistant entry — picked over a per-entry "interrupted" flag because it requires no schema change and reuses existing rendering. Producers that don't speak `chat.*` can map their native abort-confirmation to this same shape (see [the mock-plugin adapter](../starter/mock_plugin_adapter.lua)).

If no turn is in flight, `nefor-chat` does not emit; harnesses do not need to special-case "interrupt with nothing running".

## Optional events

Producers populate whichever subset they have. Missing fields render as `—` in the statusline or skip rendering entirely; missing kinds simply do not surface.

### `chat.stream.reasoning_delta`

```json
{ "kind": "chat.stream.reasoning_delta", "text": "…" }
```

Append `text` to the in-flight assistant entry's *reasoning* channel — separate from the content stream consumed by `chat.stream.delta`. Producers that surface a model's thinking trace (Ollama's `delta.reasoning` for Qwen 3 / Gemma 3) emit one of these per chunk. `nefor-chat` renders the accumulating trace as a dim live preview while the entry has no content yet, and collapses it to a single-row marker once content begins (or `reasoning_end` arrives reasoning-only). Reasoning is NOT included in the stored assistant text — it doesn't feed back into the next request's history.

### `chat.stream.reasoning_end`

```json
{
  "kind": "chat.stream.reasoning_end",
  "text": "…",          // full accumulated reasoning trace
  "duration_ms": 1840    // optional, u64
}
```

Close the in-flight assistant entry's reasoning channel. Fires exactly once per turn at the boundary where the model transitions out of thinking — either the first content delta arrives, `finish_reason` lands without content (reasoning-only turn), or the body terminates. `text` is the authoritative full trace; `duration_ms` is the wall-clock from first reasoning chunk to this event. `nefor-chat` flips the live preview into the collapsed `▶ reasoning (Ns)` row and preserves the full trace for the Ctrl+O expanded view (same toggle that expands tool I/O details).

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
{ "kind": "chat.tool.start", "id": "toolu_…", "name": "Read", "input": { … } }
```

The harness invoked a tool. `id` is producer-assigned (Claude's `tool_use_id`) and pairs the start with its later `chat.tool.end`. `nefor-chat` renders the call collapsed to a one-liner (`▸ Name(<truncated input>)`) by default; pressing Ctrl+O toggles every tool row to its expanded form (full input + output).

### `chat.tool.end`

```json
{ "kind": "chat.tool.end", "id": "toolu_…", "output": <any>, "error": false }
```

Tool returned. `id` matches the prior `chat.tool.start`; `nefor-chat` walks the transcript backward to find the row to attach the result to. `output` may be a string or any JSON value (objects/arrays are pretty-printed in the expanded view). `error` defaults to `false` and tints the row red when `true`.

### `chat.history.replay`

```json
{
  "kind": "chat.history.replay",
  "session_id": "…",
  "entries": [
    { "role": "user"|"assistant", "text": "…" },
    {
      "role": "tool",
      "id": "toolu_…",
      "name": "Read",
      "input": { … },
      "output": "…" | null,
      "error": false
    },
    …
  ]
}
```

The harness is replaying a prior session's transcript (typically in response to `chat.resume`). `nefor-chat` clears its current transcript and populates it from `entries` in the order given, then appends a `resumed · <count> messages · session <id>` system line.

`role: "tool"` entries mirror the live `chat.tool.start` / `chat.tool.end` field shape (same `id` / `name` / `input` / `output` / `error`) so the replay handler can route them through the same code path. `output: null` means the source session was truncated mid-turn before the tool returned; the tool row renders collapsed with no result. An assistant turn that interleaves text and tool calls lowers to multiple consecutive entries (text → tool → text → …) preserving live visual order.

Assistant entries may carry an optional `model` field (string). When present, `nefor-chat` stamps it on the replayed entry so the per-turn footer renders model-only (no `duration_ms`, since session logs don't record wall-clock duration per turn). Producers that don't surface model info simply omit the field.

After emitting `chat.history.replay`, a producer that tracks cumulative usage SHOULD emit one `chat.session.stats` immediately so the statusline reflects accumulated tokens / model / turns from the prior session before the first live turn lands. `cumulative_cost_usd` may be reported as `0.0` when the source log doesn't record cost (which is the case for claude's session.jsonl); resumed-session cost is approximate.

### `chat.resume` (nefor-chat → harness)

```json
{ "kind": "chat.resume", "session_id": "…" }   // session_id optional
```

Emitted when the user runs `/resume` (most-recent session) or `/resume <id>` (specific session). The harness's expected response is a `chat.history.replay`. Harnesses that don't support resumption ignore the event.

## Auth state

Provider plugins (mock-plugin, openai-provider, …) own their own auth state. Auth-only plugins (a hypothetical `anthropic-auth`, `copilot-auth`, etc.) acquire credentials and push them to the targeted provider; provider plugins report their current auth posture so chat can render `connected`, `login_required`, or `error` per provider and prompt the user to `/login` / `/logout` as needed.

Every event in this section carries a `provider` field — the spawn name of the targeted provider (e.g. `"ollama"`, `"groq"`). Adapters filter on this field so events are delivered only to the matching provider plugin.

### `chat.auth.status` (chat-bound)

```json
{
  "kind": "chat.auth.status",
  "provider": "ollama",
  "state": "connected" | "disconnected" | "login_required" | "error",
  "message": "…"     // optional, present when state == "error"
}
```

Emitted by a provider's adapter whenever the provider's auth state changes (startup, post-`auth.set`, post-`login_requested`, post-`logout_requested`, after a 401 mid-request). nefor-chat keeps a per-provider map of the latest status and renders the active provider's state in the statusline / `/auth` view.

### `chat.login_requested` (provider-bound)

```json
{ "kind": "chat.login_requested", "provider": "ollama" }
```

Emitted by nefor-chat when the user runs `/login`. The active provider is named in `provider`; the adapter forwards to that one provider only. Provider behaviour is provider-specific:

- A harness with a built-in OAuth flow (e.g. mock-plugin) starts the flow and reports progress back via `chat.auth.status`.
- A thin HTTP client like `openai-provider` has no flow; it replies with `chat.auth.status { state: "error", message: "…" }` instructing the user to wire up an external auth plugin.

### `chat.logout_requested` (provider-bound)

```json
{ "kind": "chat.logout_requested", "provider": "ollama" }
```

Emitted by nefor-chat when the user runs `/logout`. The provider clears any in-memory token (when it can) and reports via `chat.auth.status`. Providers backed by env-supplied credentials cannot revoke at runtime — they reply with an error status explaining how to clear (typically: restart without the env var).

### `chat.auth.set` (provider-bound)

```json
{ "kind": "chat.auth.set", "provider": "ollama", "token": "…" }
```

Emitted by an external auth plugin once it has acquired a fresh token. The adapter forwards only to the matching provider. The provider adopts the token, transitions to `connected`, and emits `chat.auth.status { state: "connected" }`.

This is the seam that lets auth plugins and provider plugins evolve independently: provider plugins never speak any auth protocol (OAuth, device-code, JWT…); they just accept a bearer token. Auth plugins do all the protocol-specific work and inject the result here.

## Adapting a non-conforming harness

A harness that emits its own native namespace (e.g. `mock-plugin` with `cc.stream.delta`, `cc.tool.start`) plugs into `nefor-chat` via per-plugin transforms registered on `ncp.spawn`. The transforms run in the engine's Lua step hook: `from_plugin` rewrites events at ingress before the broker broadcasts them, and `to_plugin` rewrites events at egress before they are delivered to that peer.

The reference example is [`starter/mock_plugin_adapter.lua`](../starter/mock_plugin_adapter.lua), wired in [`starter/init.lua`](../starter/init.lua). Its `from_plugin` rewrites `cc.stream.delta` → `chat.stream.delta`, `cc.stream.end` → `chat.stream.end` (keeping `model` and `duration_ms` for the per-turn footer; stripping `cost_usd` and `num_turns` since those are cumulative globals on `cc.session.stats`), `cc.tool.start` → `chat.tool.start`, `cc.session.stats` → `chat.session.stats`, `cc.history.replay` → `chat.history.replay`, and folds `cc.turn.error` into a system `chat.message.append`. Its `to_plugin` rewrites `chat.input.submit` → `cc.prompt` and `chat.resume` → `cc.resume`.

A new harness gets its own adapter alongside this one; `nefor-chat` does not change. See [`plugin-authoring.md`](./plugin-authoring.md#per-plugin-transforms) for the transform contract — return semantics, per-peer isolation, error handling.

## Tool calling (v1)

Tool-providing plugins (the first is [`basic-tools`](../plugins/basic-tools/README.md)) advertise a catalog of tools to the bus; provider plugins (e.g. `openai-provider`) collect those catalogs, surface them to the LLM via the API's tool-calling format, and route the model's tool calls back to the owning plugin. v1 ships only the wire contract and `basic-tools`; the provider integration lands in phase 2.

The contract is producer-agnostic the same way `chat.*` is: any future tool-providing plugin (`web-tools`, `database-tools`, …) speaks these events directly, and any future provider that supports tool-calling consumes them.

### `tool.register` (tool plugin → bus)

```json
{ "kind": "tool.register",
  "tools": [
    { "name": "read_file",
      "description": "Read the contents of a file. Returns the file's text content or an error.",
      "parameters": {
        "type": "object",
        "properties": {
          "path": { "type": "string", "description": "Absolute or relative path to the file." }
        },
        "required": ["path"]
      } } ] }
```

Broadcast by tool-providing plugins immediately after their `ready_ok` handshake completes, before any invokes can land. `parameters` is JSON Schema in OpenAI tool-call format directly — that keeps the provider's mapping into the API's `tools` array trivial (one passthrough, no shape-shifting).

`tool.register` is broadcast (no plugin-name prefix on the kind) so every consumer — the provider's catalog builder, debug listeners, future logging plugins — sees it. Multiple tool-providing plugins may each emit their own `tool.register`; consumers union the catalogs. Tool names are global across the bus: if two plugins register the same `name`, behaviour is "last register wins" at the consumer's catalog layer (consumers MAY warn). This v1 mirrors how OpenAI's API itself flattens names — fixing it requires either disambiguation in the provider's `tool_call_id` mapping or a `qualified_name` field, both deferred until a clash actually shows up.

### `<plugin>.tool.invoke` (caller → tool plugin)

```json
{ "kind": "basic-tools.tool.invoke",
  "id": "<correlation-id>",
  "name": "read_file",
  "args": { "path": "/etc/hosts" } }
```

Invocation is **targeted via kind-prefix routing**: the caller emits a kind shaped `<tool-plugin-name>.tool.invoke` and the engine's `handle_event` in [`starter/ncp.lua`](../starter/ncp.lua) routes events whose kind starts with `<peer>.` only to that peer (when the peer is connected and isn't the sender). With prefix routing, only the named tool plugin sees the invoke — no broadcast traffic, no every-plugin-filters-out branching.

The alternative considered was a generic `tool.invoke` broadcast where every tool plugin filters by `name`. Rejected: it spends a parse + send per peer per call, and it forces every tool plugin to know the global tool name set to filter cleanly. Prefix routing reuses the engine's existing routing primitive and keeps tool plugins ignorant of one another. The cost is that callers must look up "which plugin owns tool X" before sending — but they already need that mapping (they collected it from `tool.register`'s `from`), so the lookup is free.

`id` is a free-form string the caller picks; the result echoes it back. Provider plugins use the OpenAI `tool_call_id`; direct callers can use any uuid. `args` is a JSON object matching the tool's `parameters` schema; tools default to `{}` when absent so zero-argument tools don't require an empty object on the wire.

### `tool.result` (tool plugin → bus)

```json
{ "kind": "tool.result", "id": "<correlation-id>", "output": "<string>" }
```

On error:

```json
{ "kind": "tool.result", "id": "<correlation-id>", "error": "<string>" }
```

Exactly one of `output`/`error` is set per reply. Broadcast (no plugin prefix) — the original caller correlates by `id`, and intermediate listeners (debug, logging, future audit plugins) get to see results too. `output` is always a string in v1: stringly typed so providers can drop it directly into the API's `tool` message slot without re-serializing structured data. Tools that produce structured output JSON-encode it themselves and let the LLM parse it.

A tool that fails replies with `error` and a human-readable message — these messages go straight to the LLM via the provider, so they're shaped for that audience (no internal trace, no Rust types, no codes). Plugin-level failures (transport, parse) are not surfaced via `tool.result`; they short-circuit the tool plugin and the caller times out. Caller-side timeouts are the caller's responsibility; basic-tools doesn't enforce one.

If a tool plugin receives a `tool.invoke` for a name it doesn't own, the engine's prefix routing means this can only happen via direct misdelivery — the plugin SHOULD ignore the event silently rather than reply with an error (the caller addressed the wrong plugin; replying noise muddies the bus). Missing `id` is also dropped silently (no caller to address). Missing `name` with a valid `id` produces a `tool.result { id, error: "<diagnostic>" }` so the caller can correlate.
