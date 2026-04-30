# Provider plugins

Two flavours of "what produces assistant output for nefor-chat" coexist on the bus:

- **Agent harnesses** wrap an external tool that itself runs the agent loop (planning, tool calls, file edits). `mock-plugin` is the reference: it spawns `claude -p`, parses stream-json, and republishes it as `cc.*` events. The harness contributes very little intelligence — Claude does the work — but it owns history, tool plumbing, and resume.
- **Raw LLM providers** call a chat-completions endpoint directly and stream text back. No agent loop, no tool calls (in v1). `openai-provider` is the reference: one generic crate that talks to any OpenAI-compatible `/v1/chat/completions` endpoint (Ollama, Groq, OpenRouter, OpenAI, vLLM…). Spawn the same binary multiple times under different plugin names — each spawn picks its identity from per-instance CLI flags.

This doc covers the second category.

## Generic OpenAI-compatible LLM (`openai-provider`)

Crate at `plugins/openai-provider/`. Binary: `openai-provider`.

### One binary, N instances

NCP can spawn the same executable any number of times under different plugin names. `openai-provider` is built around that: each spawn takes a `--name` CLI flag and uses that string as the **event-kind prefix** for everything it emits and consumes. So:

- `--name ollama` → emits `ollama.hello`, `ollama.stream.delta`, `ollama.stream.end`, `ollama.session.stats`, `ollama.turn.error`, `ollama.goodbye`; consumes `ollama.prompt`, `ollama.interrupt`, `ollama.reset`.
- `--name groq` → same shape, but `groq.*`.
- `--name openrouter` → `openrouter.*`. Etc.

Two providers run as two separate `openai-provider` processes, each owning its own conversation history. Their events never collide on the bus because the prefixes differ.

Configuration is via CLI flags (not env vars) because the engine's `nefor.plugins.spawn` API does not propagate per-instance env to children — args ride the command line straight through. `--api-key` is the one exception: it falls back to the `OPENAI_PROVIDER_API_KEY` env var so secrets can stay out of `init.lua`.

The Lua adapter (`starter/openai_provider_adapter.lua`) is a factory: `make("ollama")` returns the `from_plugin` / `to_plugin` pair for the `ollama.*` namespace, `make("groq")` for `groq.*`, and so on. Same module, parameterised at instantiation.

### What it does

On each `<prefix>.prompt`:

1. Append the user message to the in-memory conversation history.
2. POST `{base_url}/v1/chat/completions` with `{model, messages, stream: true, stream_options: {include_usage: true}}`.
3. Parse the SSE response: each `data: {…}\n\n` frame becomes either a `Delta` (token text), `Finish` (stop reason), or `Usage` (token counts).
4. For every delta, emit `<prefix>.stream.delta { id, text }` on the bus.
5. On stream end, push the accumulated assistant text to history; emit `<prefix>.stream.end { id, model, duration_ms, finish_reason, text }`; emit `<prefix>.session.stats`.

`<prefix>.interrupt` cancels the in-flight HTTP request via a `CancellationToken`. The chat-contract adapter maps `chat.interrupt` → `<prefix>.interrupt`, so an ESC keypress in nefor-chat aborts the active turn. `<prefix>.reset` clears history.

### What it doesn't do (v1 scope)

- **Tool calls** — chat-completions only. The model can produce text describing what it would do, but no `tool_calls` parsing.
- **Vision / images** — `messages[*].content` is a plain string in v1. Multi-modal content arrays are not constructed.
- **Persistence** — history lives in process memory. Restarting the plugin starts a fresh conversation.
- **Resume** — there is no `<prefix>.resume` analogue. Restart = blank slate.
- **Streaming concurrency** — one turn at a time per process. Concurrent prompts come back as `<prefix>.turn.error { "busy" }`.

### Configuration

Four CLI flags, all optional:

| Flag | Default | Notes |
|---|---|---|
| `--name <NAME>` | `openai` | Per-instance identity. Used as the event-kind prefix (`<name>.hello`, `<name>.stream.delta`, …). |
| `--base-url <URL>` | `http://localhost:11434` | OpenAI-compatible base. Trailing slash trimmed automatically; the plugin appends `/v1/chat/completions`. |
| `--model <MODEL>` | `qwen2.5-coder:7b` | Model id passed verbatim in the request body. |
| `--api-key <KEY>` | — | Initial bearer token. Falls back to the `OPENAI_PROVIDER_API_KEY` env var (so secrets can stay out of `init.lua`). When set, plugin starts in `auth_state = connected`; when unset, `login_required` (chat will prompt the user to `/login`). For local providers like Ollama that don't actually need credentials, leave it unset and ignore the `login_required` status — request still goes through fine. |

Why CLI flags and not env vars: the engine's `nefor.plugins.spawn` API does not propagate per-instance env to children. CLI args ride the command line straight through. `--api-key` keeps an env-var fallback because real users want to set secrets through the shell, not by editing `init.lua`.

### Example configurations

The defaults match a local Ollama install. Override the four flags per spawn for everything else.

| Provider | `--base-url` | Example `--model` | Auth |
|---|---|---|---|
| Ollama (local) | `http://localhost:11434` | `qwen2.5-coder:7b` | none |
| Groq | `https://api.groq.com/openai` | `llama-3.3-70b-versatile` | `GROQ_API_KEY` env → `--api-key` |
| OpenRouter | `https://openrouter.ai/api` | `meta-llama/llama-3.3-70b-instruct` | `OPENROUTER_API_KEY` env → `--api-key` |
| OpenAI | `https://api.openai.com` | `gpt-4o-mini` | `OPENAI_API_KEY` env → `--api-key` |
| vLLM (local) | `http://localhost:8000` | (whatever you served) | none |

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

Full multi-instance recipe: `starter/openai-providers-example.lua`.

### Wire shape — request

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

### Wire shape — response

Server-Sent Events. Each frame is `data: {…}\n\n`:

```
data: {"choices":[{"delta":{"content":"4"},"index":0}]}

data: {"choices":[{"delta":{"content":" — "},"index":0}]}

data: {"choices":[{"delta":{"content":"easy."},"index":0}]}

data: {"choices":[{"delta":{},"finish_reason":"stop","index":0}]}

data: {"usage":{"prompt_tokens":42,"completion_tokens":3,"total_tokens":45}}

data: [DONE]

```

Some servers ride `usage` on the same chunk that closes the choices array; the parser handles both.

### Events emitted (with `<prefix> = --name .`)

- `<prefix>hello` `{ version, provider, model, base_url }` — once after `ready_ok`.
- `<prefix>ready` — once after hello.
- `<prefix>auth.status` `{ state, message? }` — once immediately after `ready` (initial auth posture), then on every state transition (`auth.set` accepted, `login_requested` rejected, `logout_requested` handled, HTTP 401 mid-request). `state` is one of `"connected" | "login_required" | "error"`; `message` is present when `state == "error"`.
- `<prefix>stream.delta` `{ id, text }` — per token chunk.
- `<prefix>stream.end` `{ id, text, model, duration_ms, finish_reason }` — turn finalization. `text` is the accumulated assistant string.
- `<prefix>session.stats` `{ model, turns, cumulative_input_tokens, cumulative_output_tokens, last_turn_input_tokens, last_turn_output_tokens, last_turn_context_tokens, last_turn_duration_ms }` — emitted after every `stream.end`. `last_turn_context_tokens` mirrors `last_turn_input_tokens` (no caching here).
- `<prefix>turn.error` `{ message }` — on network failure, non-2xx response, or after an interrupt (with `message: "interrupted"`).
- `<prefix>goodbye` `{ reason }` — on shutdown.

### Events consumed

- `<prefix>prompt` `{ text }` — append `text` as user message, fire request, stream back.
- `<prefix>interrupt` — cancel the in-flight request. The turn finalizes with `finish_reason: "interrupted"` and a `turn.error { message: "interrupted" }`.
- `<prefix>reset` — clear conversation history (no events emitted).
- `<prefix>auth.set` `{ token }` — adopt `token` as the bearer for subsequent requests; transition to `connected`; emit `<prefix>auth.status`. The token's source is recorded as "auth.set" so `<prefix>logout_requested` knows it can be cleared. Empty tokens are ignored (no state change, no status emitted).
- `<prefix>login_requested` — openai-provider has **no built-in OAuth/device-code flow**. The plugin transitions to `error` and emits `<prefix>auth.status { state: "error", message: "openai-provider has no built-in login flow — wire up an auth plugin (e.g. anthropic-auth) and have it push <prefix>.auth.set events" }`. The error stays until something pushes a token via `<prefix>auth.set`. This is intentional: the plugin tells the user exactly what's needed instead of pretending it can log in.
- `<prefix>logout_requested` — behaviour depends on where the current token came from:
  - **Token came from `auth.set`** (some auth plugin pushed it): clear the token, transition to `login_required`, emit `<prefix>auth.status { state: "login_required" }`.
  - **Token came from `--api-key` / `OPENAI_PROVIDER_API_KEY` (or no token at all)**: refuse — emit `<prefix>auth.status { state: "error", message: "no login to revoke — credentials come from --api-key (or OPENAI_PROVIDER_API_KEY env var); restart the plugin without it to clear" }`. The stored token is **not** cleared; clearing would just make subsequent requests fail without being able to recover.

### Auth state transitions

```
            startup
               │
               ▼
   ┌───────────────────────┐    --api-key (or OPENAI_PROVIDER_API_KEY) set?
   │ Connected (env)       │ ◀── yes
   └───────────────────────┘
               │
               │  no
               ▼
   ┌───────────────────────┐
   │ LoginRequired         │
   └───────────────────────┘
               │
               │ <prefix>.auth.set { token }
               ▼
   ┌───────────────────────┐
   │ Connected (auth-set)  │
   └───────────────────────┘
               │
               │ <prefix>.logout_requested  →  back to LoginRequired
               │ HTTP 401 mid-request       →  Error
               │ <prefix>.login_requested   →  Error (no flow available)

   Error state recovery: only <prefix>.auth.set returns to Connected.
```

env-vs-auth-set bookkeeping is what makes logout safe. The plugin tracks `TokenSource::Env` vs `TokenSource::AuthSet` per token; logout only clears `AuthSet` tokens.

### Adapter

`starter/openai_provider_adapter.lua` is a **factory** that returns chat-contract transforms for a given provider name. The translation logic itself doesn't change between providers; only the prefix it matches against does.

```lua
local mk = require("openai_provider_adapter").make
local ollama = mk("ollama")          -- transforms scoped to ollama.*
local groq   = mk("groq")            -- transforms scoped to groq.*
```

Each pair maps `<prefix>stream.delta` → `chat.stream.delta`, `<prefix>stream.end` → `chat.stream.end`, `<prefix>session.stats` → `chat.session.stats`, `<prefix>auth.status` → `chat.auth.status` (injecting `provider = name` so chat can group by provider), `<prefix>turn.error` → `chat.message.append { role = "system" }`, and drops the internal `<prefix>hello` / `<prefix>ready` / `<prefix>goodbye` lifecycle events. In the other direction it maps `chat.input.submit` → `<prefix>prompt`, `chat.interrupt` → `<prefix>interrupt`, `chat.reset` → `<prefix>reset`, plus the auth-targeted events `chat.auth.set` → `<prefix>auth.set`, `chat.login_requested` → `<prefix>login_requested`, `chat.logout_requested` → `<prefix>logout_requested`. The auth-targeted events carry a `provider` field; the adapter forwards only when `provider == name` and drops otherwise, so the right plugin reacts when multiple providers are wired up. The `chat.interrupt` mapping is what makes the ESC interrupt path work end-to-end through nefor-chat.

### Multi-instance pattern

Wire one provider into nefor-chat at a time. Spawning two providers (e.g. `ollama` and `groq`) means **both** would translate their `<prefix>.stream.*` events to `chat.stream.*`, so nefor-chat would render interleaved deltas from both. A future router plugin could fan `chat.input.submit` out to a chosen provider based on a model-selector UI; until that exists, pick one.

See `starter/openai-providers-example.lua` for the full multi-instance recipe (commented out — uncomment the spawn you want).
