# openai-provider

NCP plugin: generic OpenAI-compatible LLM provider. One binary that
talks to any `/v1/chat/completions` endpoint (Ollama, Groq, OpenRouter,
OpenAI, vLLM, ...). Spawn multiple instances under different plugin names
to run several providers in parallel.

## One binary, N instances

NCP can spawn the same executable any number of times under different plugin names. `openai-provider` is built around that: each spawn takes a `--name` CLI flag and uses that string as the **event-kind prefix** for everything it emits and consumes. So:

- `--name ollama` -> emits `ollama.hello`, `ollama.stream.delta`, `ollama.stream.end`, `ollama.session.stats`, `ollama.turn.error`, `ollama.goodbye`; consumes chat-scoped `ollama.chat.*` events plus legacy `ollama.prompt`, `ollama.interrupt`, `ollama.reset`.
- `--name groq` -> same shape, but `groq.*`.
- `--name openrouter` -> `openrouter.*`. Etc.

Two providers run as two separate `openai-provider` processes. Each process owns its own chat map keyed by chat id, so multiple chats can stream concurrently within a process and prefixes keep separate provider instances from colliding on the bus.

Configuration is via CLI flags (not env vars) because the engine's `nefor.plugins.spawn` API does not propagate per-instance env to children -- args ride the command line straight through. `--api-key` is the one exception: it falls back to the `OPENAI_PROVIDER_API_KEY` env var so secrets can stay out of `init.lua`.

The Lua adapter (`lua/openai-provider/init.lua`) is a factory: `make("ollama")` returns the `from_plugin` / `to_plugin` pair for the `ollama.*` namespace, `make("groq")` for `groq.*`, and so on. Same module, parameterised at instantiation.

## What it does

The current API is chat-scoped:

1. `<prefix>.chat.create` creates an in-memory chat with a model.
2. `<prefix>.chat.append` appends user/assistant/tool context to that chat.
3. `<prefix>.chat.complete` POSTs `{base_url}/v1/chat/completions` with streaming enabled. Direct `<prefix>.completion.request` calls may include `request_additions`, an object merged into the top-level upstream request; collisions with canonical request fields are rejected. This richer passthrough belongs to direct completions only; chat-scoped aggregate usage remains the separate legacy summary described below.
4. Successful completion responses must be SSE (`Content-Type: text/event-stream`); non-stream JSON fails explicitly rather than being mistaken for an empty stream. Connection failures, transient HTTP 429 responses, and every HTTP 5xx response share a bounded retry budget. A 429 body that reports exhausted quota, credits, balance, or a billing hard limit is terminal and is never replayed. An SSE transport failure after HTTP success may use the same budget only before text, reasoning, or tool-call state exists; after that boundary it fails without replay. SSE frames become `<prefix>.stream.delta` and `<prefix>.stream.end`; token accounting becomes `<prefix>.session.stats`. Direct completion usage preserves optional standard token fields, nests every unknown upstream usage member under `usage.extensions`, rejects malformed or duplicate usage payloads, and carries the upstream completion ID when one was supplied.
5. `<prefix>.chat.delete` drops the in-memory chat.

The provider consumes `tool.register`, sends model `tool_calls` to the registered tool's `<plugin>.tool.invoke` endpoint, waits for matching `tool.result`, emits `chat.tool.start` / `chat.tool.end`, and loops until the model returns final text. This works with gated tools because `tool-gate` advertises itself as the tool entry point.

`<prefix>.interrupt` cancels an in-flight HTTP request via a `CancellationToken`. The legacy default-chat path maps `chat.interrupt` -> `<prefix>.interrupt`, so an ESC keypress in the chat surface aborts the active turn. `<prefix>.reset` clears legacy default-chat history.

## Native reasoning continuation

Completed reasoning is carried in `provider_context` separately from display text.
Structured `reasoning_details` take precedence over the plaintext `reasoning` and
`reasoning_content` aliases. During streaming, adjacent text or summary fragments
with compatible metadata are concatenated without separators into one logical
block. Type changes, explicit identity/index changes, and conflicting metadata
preserve separate blocks; later fragments can supply missing metadata such as a
signature. Encrypted and unknown detail types stay opaque, ordered, and intact.
Completed arrays restored from history are not reinterpreted as streaming deltas.

## What it doesn't do

- **Vision / images** -- this provider does not construct multimodal OpenAI message content. Image media returned by tools is converted to an explicit "model does not support image input" error in the text-only flow.
- **Persistence** -- chat history lives in process memory. Restarting the plugin starts fresh.
- **Resume** -- there is no `<prefix>.resume` analogue. Restart = blank slate.

Legacy `<prefix>.prompt`, `<prefix>.interrupt`, and `<prefix>.reset` remain as default-chat compatibility events; new integrations should prefer `<prefix>.chat.*`.

## Configuration

Each native instance requires `--retry-policy <JSON>`; the other flags select its endpoint, model and identity.

| Flag               | Default                  | Notes                                                                                                                                                                                                                                                                                                                                                                                                         |
| ------------------ | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--retry-policy <JSON>` | required | Explicit eight-field transport budget described below; no native defaults. |
| `--auth-header <NAME>` | `Authorization` | Custom authentication header; nonstandard headers send the key without a Bearer prefix. |
| `--name <NAME>`    | `openai`                 | Per-instance identity. Used as the event-kind prefix (`<name>.hello`, `<name>.stream.delta`, ...).                                                                                                                                                                                                                                                                                                            |
| `--base-url <URL>` | `http://localhost:11434` | OpenAI-compatible base. Trailing slash trimmed automatically; the plugin appends `/v1/chat/completions`.                                                                                                                                                                                                                                                                                                      |
| `--model <MODEL>`  | --                       | Model id passed verbatim in the request body. Optional -- omitting it means `chat.create` without a model fails with `NoModelConfigured` until one is set via `/model`.                                                                                                                                                                                                                                       |
| `--api-key <KEY>`  | --                       | Initial bearer token. Falls back to the `OPENAI_PROVIDER_API_KEY` env var (so secrets can stay out of `init.lua`). When set, plugin starts in `auth_state = connected`; when unset, `login_required` (chat will prompt the user to `/login`). For local providers like Ollama that don't actually need credentials, leave it unset and ignore the `login_required` status -- request still goes through fine. |

Why CLI flags and not env vars: the engine's `nefor.plugins.spawn` API does not propagate per-instance env to children. CLI args ride the command line straight through. `--api-key` keeps an env-var fallback because real users want to set secrets through the shell, not by editing `init.lua`.

## Transport retry policy

Lua configuration owns every value. The starter chooses the following policy
once and attaches it to its OpenAI-compatible provider entries; these are example
settings, not defaults compiled into the plugin:

```lua
local transport_retry = {
  max_attempts = 8, max_rounds = 6,
  backoff_multiplier = 2, initial_delay_ms = 500, max_delay_ms = 30000,
  round_cooldown_ms = 60000, jitter_bps = 1000,
  response_headers_timeout_ms = 120000,
}
local args = require("openai-provider").retry_args(transport_retry)
-- Append both args to this instance's native command exactly once.
```

`retry_args` returns `{ "--retry-policy", nefor.json.encode(policy) }` without
mutating or filling the table. Native parsing rejects missing/unknown fields,
noninteger or negative values, zero counts/header timeout, reversed delay bounds,
jitter outside 0–10000 basis points, and unrepresentable budgets/timers. Zero
local delays and zero jitter are explicit supported settings. All public Rust
`run_chat_stream*` functions require a checked `&RetryPolicy` immediately after
the client; `RetryPolicy::from_json` is the public construction boundary.

Each send consumes one attempt from one shared per-round counter: connection or
header failure, transient HTTP 429/all 5xx, and SSE transport loss **before** any
text, reasoning continuation or tool-call state. Permanent quota/billing denials,
other HTTP errors and semantic/malformed in-band failures do not replay. Only an
exhausted round that saw a transient 429 may open another round; that evidence is
retained across mixed retryable failures but reset at each new round. No agent
turn, completed tool or conversation context is restarted.

Inner delays are `initial_delay_ms * backoff_multiplier^(failed_attempt - 1)`,
capped locally at `max_delay_ms` before additive jitter. `backoff_multiplier` is
a required finite number >= 1 (including fractional values); 1 keeps delays
constant. Fractional millisecond results truncate to whole milliseconds. The
starter selects 2; the plugin has no multiplier default.
`jitter_bps` adds uniformly sampled jitter (1000 means up to 10%); the local inner
cap still applies. Exhausted eligible rounds use one `round_cooldown_ms` plus
additive jitter instead of an additional inner wait. Valid Retry-After seconds
or HTTP dates are a minimum server not-before deadline, never shortened by the
local cap or a zero/past header; body-read time counts toward that deadline.
The selected 8×6 policy therefore permits exactly 48 sends, 42 inner waits and 5
round waits under persistent transient 429, with no final wait. Pure 5xx or
connection failure gets one round only. This is a per-transport-invocation bound,
not an elapsed deadline or an overall bound across independent auth or
unsupported-tools fallback invocations. ChatGPT transport and tool timeouts are
separate.

Cancellation interrupts both waits, header acquisition, error/non-SSE body
reads and SSE consumption. Header timeout applies anew to every send.
`completion.event` with `event="retry"` and legacy `<prefix>.stream.retry` carry
`phase`, `round`, `max_rounds`, `attempt`, `max_attempts`, `next_round`,
`next_attempt`, `total_attempts`, `delay_ms`, `category`, optional `status`, and a
safe `message`. Categories are `rate_limit`, `server`, `transport`, and
`stream_transport`; notices never contain raw server bodies. Both routes produce
visible toasts, with canonical notices correlated only to pending current-session
requests. Budget errors add `retry` metadata with the same counters/category and
`reason="attempts_exhausted"|"rounds_exhausted"`; cancellation has no exhaustion
metadata. Nonbudget errors keep their existing classification.

## Opt-in raw wire logging

Set `NEFOR_OPENAI_WIRE_LOG=1` in the environment inherited by Nefor to emit raw
OpenAI-provider HTTP/SSE records through the normal plugin stderr -> aggregate
Nefor log sink. It is disabled by default; `true`, `yes`, `on`, and `enabled`
(case-insensitive) also enable it.

> **Sensitive data warning:** raw wire records contain complete request JSON
> (prompts, conversation messages, tool definitions/results, continuation
> shapes) and raw model response bytes. Enable this only for local diagnosis,
> protect the aggregate log accordingly, and disable it afterward. API keys,
> Authorization values, cookies, and other credential-bearing headers are never
> recorded even in this mode.

Each JSON record is logged under tracing target `openai_provider::wire` and has
`session_id`, `request_id`, monotonic `sequence`, optional `attempt`, and an
`event`. A call starts with `begin`; each HTTP attempt records `request`, then
`response` (status plus an allowlist of correlation/rate-limit/retry headers).
Non-2xx bodies use `response_body`; successful streams use `stream_data` for
each byte chunk in arrival order, with both lossy text and exact `body_hex`.
`retry`, `end`, `error`, and `cancel` make lifecycle outcomes explicit. Request
headers are described only by fixed non-secret content-negotiation headers;
the request builder and credential headers are never debug-formatted.

## Example configurations

Select the endpoint/model and an explicit transport policy per native instance.

| Provider       | `--base-url`                  | Example `--model`                   | Auth                                    |
| -------------- | ----------------------------- | ----------------------------------- | --------------------------------------- |
| Ollama (local) | `http://localhost:11434`      | `qwen2.5-coder:7b`                  | none                                    |
| Groq           | `https://api.groq.com/openai` | `llama-3.3-70b-versatile`           | `GROQ_API_KEY` env -> `--api-key`       |
| OpenRouter     | `https://openrouter.ai/api`   | `meta-llama/llama-3.3-70b-instruct` | `OPENROUTER_API_KEY` env -> `--api-key` |
| OpenAI         | `https://api.openai.com`      | `gpt-4o-mini`                       | `OPENAI_API_KEY` env -> `--api-key`     |
| vLLM (local)   | `http://localhost:8000`       | (whatever you served)               | none                                    |

Example spawn (Lua):

```lua
local command = {
  bin("openai-provider"), "--name", "ollama",
  "--base-url", "http://localhost:11434", "--model", "phi4-mini:latest",
}
for _, arg in ipairs(require("openai-provider").retry_args(transport_retry)) do
  command[#command + 1] = arg
end
ncp.spawn {
  name    = "ollama",
  command = command,
  from_plugin = ollama.from_plugin,
  to_plugin   = ollama.to_plugin,
}
```

## Wire shape -- request

```json
POST /v1/chat/completions
Content-Type: application/json
Authorization: Bearer <key>   // only when --api-key (or OPENAI_PROVIDER_API_KEY) is set

{
  "model": "qwen2.5-coder:7b",
  "messages": [
    { "role": "user",      "content": "hi" },
    { "role": "assistant", "content": "hello back" },
    { "role": "user",      "content": "what's 2+2?" }
  ],
  "stream": true,
  "stream_options": { "include_usage": true }
}
```

## Wire shape -- response

Server-Sent Events. Each frame is `data: {...}\n\n`:

```
data: {"choices":[{"delta":{"content":"4"},"index":0}]}

data: {"choices":[{"delta":{"content":" -- "},"index":0}]}

data: {"choices":[{"delta":{"content":"easy."},"index":0}]}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}]}

data: {"usage":{"prompt_tokens":42,"completion_tokens":3,"total_tokens":45}}

data: [DONE]

```

Some servers ride multiple semantic fields on the same chunk. The parser preserves content, refusal, reasoning extensions, every tool-call fragment, finish reason, and usage in deterministic order. Standard streamed refusal, malformed JSON, in-band error envelopes, unknown finish reasons, and incomplete tool calls terminate the turn explicitly instead of becoming empty success.

## Events emitted (with `<prefix> = --name .`)

- `<prefix>.hello` `{ version, provider, model, base_url }` -- once after `ready_ok`.
- `<prefix>.ready` -- once after hello.
- `<prefix>.auth.status` `{ state, message? }` -- once immediately after `ready` (initial auth posture), then on every state transition (`auth.set` accepted, `login_requested` rejected, `logout_requested` handled, HTTP 401 mid-request). `state` is one of `"connected" | "login_required" | "error"`; `message` is present when `state == "error"`.
- `<prefix>.stream.delta` `{ id, text }` -- per token chunk.
- `<prefix>.stream.end` `{ id, text, model, duration_ms, finish_reason }` -- turn finalization. `text` is the accumulated assistant string.
- `<prefix>.session.stats` `{ model, turns, cumulative_input_tokens, cumulative_output_tokens, last_turn_input_tokens, last_turn_output_tokens, last_turn_context_tokens, last_turn_duration_ms }` -- emitted after every `stream.end`. `last_turn_context_tokens` mirrors `last_turn_input_tokens` (no caching here).
- `chat.tool.start` / `chat.tool.end` -- tool-call lifecycle while completing a turn.
- `<prefix>.turn.error` `{ message }` -- on network failure, non-2xx response, or after an interrupt (with `message: "interrupted"`).
- `<prefix>.models.list` / `<prefix>.model.changed` -- model inventory/status responses.
- `<prefix>.goodbye` `{ reason }` -- on shutdown.

## Events consumed

- `<prefix>.chat.create` / `.chat.append` / `.chat.complete` / `.chat.delete` -- current chat-scoped API.
- `<prefix>.model.set`, `<prefix>.models.list_requested` -- model selection/listing.
- `tool.register`, `tool.result` -- maintain the tool catalog and receive tool outputs.
- `<prefix>.prompt` `{ text }` -- legacy default-chat compatibility: append `text` as user message, fire request, stream back.
- `<prefix>.interrupt` -- cancel the in-flight request. The turn finalizes with `finish_reason: "interrupted"` and a `turn.error { message: "interrupted" }`.
- `<prefix>.reset` -- clear legacy conversation history (no events emitted).
- `<prefix>.auth.set` `{ token }` -- adopt `token` as the bearer for subsequent requests; transition to `connected`; emit `<prefix>.auth.status`. The token's source is recorded as "auth.set" so `<prefix>.logout_requested` knows it can be cleared. Empty tokens are ignored (no state change, no status emitted).
- `<prefix>.login_requested` -- openai-provider has **no built-in OAuth/device-code flow**. The plugin transitions to `error` and emits `<prefix>.auth.status { state: "error", message: "openai-provider has no built-in login flow -- wire up an auth plugin (e.g. anthropic-auth) and have it push <prefix>.auth.set events" }`. The error stays until something pushes a token via `<prefix>.auth.set`. This is intentional: the plugin tells the user exactly what's needed instead of pretending it can log in.
- `<prefix>.logout_requested` -- behaviour depends on where the current token came from:
  - **Token came from `auth.set`** (some auth plugin pushed it): clear the token, transition to `login_required`, emit `<prefix>auth.status { state: "login_required" }`.
  - **Token came from `--api-key` / `OPENAI_PROVIDER_API_KEY` (or no token at all)**: refuse -- emit `<prefix>auth.status { state: "error", message: "no login to revoke -- credentials come from --api-key (or OPENAI_PROVIDER_API_KEY env var); restart the plugin without it to clear" }`. The stored token is **not** cleared; clearing would just make subsequent requests fail without being able to recover.

## Auth state transitions

```
            startup
               |
               v
   +---------------------------+    --api-key (or OPENAI_PROVIDER_API_KEY) set?
   | Connected (env)           | <-- yes
   +---------------------------+
               |
               |  no
               v
   +---------------------------+
   | LoginRequired             |
   +---------------------------+
               |
               | <prefix>.auth.set { token }
               v
   +---------------------------+
   | Connected (auth-set)      |
   +---------------------------+
               |
               | <prefix>.logout_requested  ->  back to LoginRequired
               | HTTP 401 mid-request        ->  Error
               | <prefix>.login_requested    ->  Error (no flow available)

   Error state recovery: only <prefix>.auth.set returns to Connected.
```

env-vs-auth-set bookkeeping is what makes logout safe. The plugin tracks `TokenSource::Env` vs `TokenSource::AuthSet` per token; logout only clears `AuthSet` tokens.

## Adapter

`lua/openai-provider/init.lua` is a **factory** that returns chat-contract transforms for a given provider name. The translation logic itself doesn't change between providers; only the prefix it matches against does.

```lua
local mk = require("openai_provider_adapter").make
local ollama = mk("ollama")          -- transforms scoped to ollama.*
local groq   = mk("groq")            -- transforms scoped to groq.*
```

Each pair maps `<prefix>.stream.delta` -> `chat.stream.delta`, `<prefix>.stream.end` -> `chat.stream.end`, `<prefix>.session.stats` -> `chat.session.stats`, `<prefix>.auth.status` -> `chat.auth.status` (injecting `provider = name` so chat can group by provider), `<prefix>.turn.error` -> `chat.message.append { role = "system" }`, and drops the internal `<prefix>.hello` / `<prefix>.ready` / `<prefix>.goodbye` lifecycle events. In the other direction it maps `chat.input.submit` -> `<prefix>.prompt`, `chat.interrupt` -> `<prefix>.interrupt`, `chat.reset` -> `<prefix>.reset`, plus the auth-targeted events `chat.auth.set` -> `<prefix>.auth.set`, `chat.login_requested` -> `<prefix>.login_requested`, `chat.logout_requested` -> `<prefix>.logout_requested`. The auth-targeted events carry a `provider` field; the adapter forwards only when `provider == name` and drops otherwise, so the right plugin reacts when multiple providers are wired up. The `chat.interrupt` mapping is what makes the ESC interrupt path work end-to-end through the chat surface.

## Multi-instance pattern

Wire one provider into the chat surface at a time. Spawning two providers (e.g. `ollama` and `groq`) means **both** would translate their `<prefix>.stream.*` events to `chat.stream.*`, so the chat surface would render interleaved deltas from both. A future router plugin could fan `chat.input.submit` out to a chosen provider based on a model-selector UI; until that exists, pick one.

## Output admission and invocation lifetime

All NCP output uses SDK checked admission (16 MiB wire bytes including LF).
Admission succeeds before enqueue; it does not guarantee stdout write, peer
receipt, or request acceptance. Output construction and upstream response
accumulation are not bounded by this wire limit.

Completion and chat turns own bounded progress publishers. Synchronous stream
callbacks hand off to a 256-item invocation-local queue; queue-full is a terminal
invocation failure, not a silent dropped delta. The publisher preserves order
and stops the invocation on rejected progress, tool calls, or completed output.
It drops the active HTTP stream, removes abandoned tool-result waiters, and
releases only that request/turn's registry generation. Earlier bus output,
provider requests, and external tool effects are not rolled back or replayed.

With a live writer, a rejected invocation emits at most one small existing error:

- Direct completion: `<prefix>.completion.event`, `event="error"`, with the
  caller's exact `request_id`. There is no separate `completion.error` event or
  replacement `id` correlation.
- Chat completion: `<prefix>.chat.error` with the exact `chat_id`.
- Legacy prompt: the existing `<prefix>.turn.error` shape, not a fabricated
  completion request identity.

These fallback events add `error_category="output_admission"` for frame rejection
or `"output_backpressure"` for callback queue-full, and a constant diagnostic:
`provider output admission failed; earlier or external effects may have occurred`.
They never include the rejected payload or truncate correlation identities.
Hard-cancelled turns/requests retain no-terminal suppression. After successful
fallback admission and local cleanup, the same chat/request identity may be used
again. A late old task cannot clear a newer chat turn's busy slot.

If the fallback cannot be admitted (including an overlong identity), or the
writer is closed, the plugin stops instead of recursively falling back. Required
uncorrelated/control output rejection also stops the plugin rather than inventing
a completion. Terminal stdin framing/UTF-8/I/O faults and writer closure supervise,
cancel, and join outstanding invocation work before local exit; bounded malformed
JSON remains parse noise. Capability loss leaves accepted effects unknown to the
application owner. Trusted task panics remain local process failures, not successful
completions. No restart or continuation policy is introduced.
