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
3. `<prefix>.chat.complete` POSTs `{base_url}/v1/chat/completions` with streaming enabled.
4. SSE frames become `<prefix>.stream.delta` and `<prefix>.stream.end`; token accounting becomes `<prefix>.session.stats`.
5. `<prefix>.chat.delete` drops the in-memory chat.

The provider consumes `tool.register`, sends model `tool_calls` to the registered tool's `<plugin>.tool.invoke` endpoint, waits for matching `tool.result`, emits `chat.tool.start` / `chat.tool.end`, and loops until the model returns final text. This works with gated tools because `tool-gate` advertises itself as the tool entry point.

`<prefix>.interrupt` cancels an in-flight HTTP request via a `CancellationToken`. The legacy default-chat path maps `chat.interrupt` -> `<prefix>.interrupt`, so an ESC keypress in the chat surface aborts the active turn. `<prefix>.reset` clears legacy default-chat history.

## What it doesn't do

- **Vision / images** -- this provider does not construct multimodal OpenAI message content. Image media returned by tools is converted to an explicit "model does not support image input" error in the text-only flow.
- **Persistence** -- chat history lives in process memory. Restarting the plugin starts fresh.
- **Resume** -- there is no `<prefix>.resume` analogue. Restart = blank slate.

Legacy `<prefix>.prompt`, `<prefix>.interrupt`, and `<prefix>.reset` remain as default-chat compatibility events; new integrations should prefer `<prefix>.chat.*`.

## Configuration

Four CLI flags, all optional:

| Flag               | Default                  | Notes                                                                                                                                                                                                                                                                                                                                                                                                         |
| ------------------ | ------------------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--name <NAME>`    | `openai`                 | Per-instance identity. Used as the event-kind prefix (`<name>.hello`, `<name>.stream.delta`, ...).                                                                                                                                                                                                                                                                                                            |
| `--base-url <URL>` | `http://localhost:11434` | OpenAI-compatible base. Trailing slash trimmed automatically; the plugin appends `/v1/chat/completions`.                                                                                                                                                                                                                                                                                                      |
| `--model <MODEL>`  | --                       | Model id passed verbatim in the request body. Optional -- omitting it means `chat.create` without a model fails with `NoModelConfigured` until one is set via `/model`.                                                                                                                                                                                                                                       |
| `--api-key <KEY>`  | --                       | Initial bearer token. Falls back to the `OPENAI_PROVIDER_API_KEY` env var (so secrets can stay out of `init.lua`). When set, plugin starts in `auth_state = connected`; when unset, `login_required` (chat will prompt the user to `/login`). For local providers like Ollama that don't actually need credentials, leave it unset and ignore the `login_required` status -- request still goes through fine. |

Why CLI flags and not env vars: the engine's `nefor.plugins.spawn` API does not propagate per-instance env to children. CLI args ride the command line straight through. `--api-key` keeps an env-var fallback because real users want to set secrets through the shell, not by editing `init.lua`.

## Example configurations

The defaults match a local Ollama install. Override the four flags per spawn for everything else.

| Provider       | `--base-url`                  | Example `--model`                   | Auth                                    |
| -------------- | ----------------------------- | ----------------------------------- | --------------------------------------- |
| Ollama (local) | `http://localhost:11434`      | `qwen2.5-coder:7b`                  | none                                    |
| Groq           | `https://api.groq.com/openai` | `llama-3.3-70b-versatile`           | `GROQ_API_KEY` env -> `--api-key`       |
| OpenRouter     | `https://openrouter.ai/api`   | `meta-llama/llama-3.3-70b-instruct` | `OPENROUTER_API_KEY` env -> `--api-key` |
| OpenAI         | `https://api.openai.com`      | `gpt-4o-mini`                       | `OPENAI_API_KEY` env -> `--api-key`     |
| vLLM (local)   | `http://localhost:8000`       | (whatever you served)               | none                                    |

Example spawn (Lua):

```lua
ncp.spawn {
  name    = "ollama",
  command = {
    bin("openai-provider"),
    "--name",     "ollama",
    "--base-url", "http://localhost:11434",
    "--model",    "phi4-mini:latest",
  },
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
