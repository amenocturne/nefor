-- starter/openai-providers-example.lua — recipe for wiring one or more
-- openai-provider instances into the starter engine config.
--
-- This file is documentation, not loaded by the engine. Copy the snippets
-- below into `starter/init.lua` to spawn the providers you want.
--
-- IMPORTANT: openai-provider is for CLOUD OpenAI-compatible APIs (Groq,
-- OpenRouter, OpenAI, vLLM, …). For local Ollama, use the sibling
-- `ollama-provider` binary instead — it speaks Ollama's native /api/chat
-- endpoint (NDJSON streaming, native tool-call shape) which works
-- correctly on every model, while Ollama's /v1/chat/completions shim
-- breaks on certain local models due to chat-template rendering bugs.
-- The Ollama recipe below remains as a reference for hosted Ollama
-- proxies that ONLY expose the OpenAI-compatible shim, but for any
-- local Ollama you should prefer ollama-provider (already wired in
-- init.lua).
--
-- The same openai-provider binary handles every cloud OpenAI-compatible
-- provider. Each spawn picks its identity via the `--name` CLI flag —
-- that name becomes the per-instance event-kind prefix (`groq.*`, …)
-- so multiple providers can coexist on the bus without colliding.
--
-- Why CLI flags and not env: the engine's `nefor.plugins.spawn` does not
-- propagate per-instance env to children. CLI args ride straight through.
-- For real secrets, `--api-key` falls back to the OPENAI_PROVIDER_API_KEY
-- env var so you can keep keys out of init.lua.
--
-- --------------------------------------------------------------------
-- IMPORTANT: only ONE provider should be wired into nefor-chat at a time.
-- The chat-contract adapter rewrites the provider's `<prefix>.stream.*`
-- events into `chat.stream.*`; if two providers were both wired up they
-- would both translate, and nefor-chat would interleave their deltas.
--
-- For now: pick one. Future: a router plugin could fan out
-- `chat.input.submit` to a chosen provider based on a model selector.
-- Out of scope here.
-- --------------------------------------------------------------------
--
-- Per-instance working directory: the engine sets each plugin's cwd to
-- <plugin_root>/<spawn_name>/ and refuses to spawn if it doesn't exist.
-- Because every spawn below uses the same `openai-provider` binary but a
-- distinct `name` (ollama, groq, …), each instance needs an empty stub
-- directory under `plugins/`. Create it once with:
--
--     mkdir -p plugins/<name> && touch plugins/<name>/.gitkeep
--
-- (`plugins/ollama/` ships in-tree as the worked example.)
--
-- Recipe — paste into init.lua, after the existing
-- `local agentic_workflow = require("agentic_workflow")` block. Pick
-- whichever provider you want; only un-comment ONE spawn.
--[[

local mk_adapter = require("agentic_workflow").for_provider

------------------------------------------------------------------
-- Local Ollama (no API key, default base URL)
------------------------------------------------------------------
local ollama = mk_adapter("ollama", { static_token = "ollama-local" })
ncp.spawn {
  name        = "ollama",
  command     = {
    bin("openai-provider"),
    "--name",     "ollama",
    "--base-url", "http://localhost:11434",
    "--model",    "qwen2.5-coder:7b",
  },
  from_plugin = ollama.from_plugin,
  to_plugin   = ollama.to_plugin,
}

------------------------------------------------------------------
-- Groq cloud (fast hosted inference)
------------------------------------------------------------------
local groq = mk_adapter("groq")
ncp.spawn {
  name        = "groq",
  command     = {
    bin("openai-provider"),
    "--name",     "groq",
    "--base-url", "https://api.groq.com/openai",
    "--model",    "llama-3.3-70b-versatile",
    "--api-key",  os.getenv("GROQ_API_KEY") or "",
  },
  from_plugin = groq.from_plugin,
  to_plugin   = groq.to_plugin,
}

------------------------------------------------------------------
-- OpenRouter (model marketplace)
------------------------------------------------------------------
local openrouter = mk_adapter("openrouter")
ncp.spawn {
  name        = "openrouter",
  command     = {
    bin("openai-provider"),
    "--name",     "openrouter",
    "--base-url", "https://openrouter.ai/api",
    "--model",    "meta-llama/llama-3.3-70b-instruct",
    "--api-key",  os.getenv("OPENROUTER_API_KEY") or "",
  },
  from_plugin = openrouter.from_plugin,
  to_plugin   = openrouter.to_plugin,
}

------------------------------------------------------------------
-- OpenAI direct
------------------------------------------------------------------
local openai = mk_adapter("openai")
ncp.spawn {
  name        = "openai",
  command     = {
    bin("openai-provider"),
    "--name",     "openai",
    "--base-url", "https://api.openai.com",
    "--model",    "gpt-4o-mini",
    "--api-key",  os.getenv("OPENAI_API_KEY") or "",
  },
  from_plugin = openai.from_plugin,
  to_plugin   = openai.to_plugin,
}

------------------------------------------------------------------
-- vLLM (local, OpenAI-compatible server, no auth)
------------------------------------------------------------------
local vllm = mk_adapter("vllm")
ncp.spawn {
  name        = "vllm",
  command     = {
    bin("openai-provider"),
    "--name",     "vllm",
    "--base-url", "http://localhost:8000",
    "--model",    "<your-vllm-model-id>",
  },
  from_plugin = vllm.from_plugin,
  to_plugin   = vllm.to_plugin,
}

--]]
--
-- Notes:
--   - All five providers above run as the same `openai-provider` binary —
--     no separate crate per provider. Identity is per-spawn.
--   - The adapter factory parameterises only the event-kind prefix; the
--     translation semantics (stream.delta → chat.stream.delta, etc.) are
--     identical across providers.
--   - Defaults if flags omitted: --name=openai,
--     --base-url=http://localhost:11434, --model=qwen2.5-coder:7b.
--     --api-key has no default; it falls back to the OPENAI_PROVIDER_API_KEY
--     env var so secrets can stay out of init.lua.
--
-- Auth state:
--   - The plugin owns its bearer token. On startup, if --api-key (or its
--     OPENAI_PROVIDER_API_KEY env-var fallback) is set, the plugin starts
--     in `connected`; otherwise `login_required`.
--   - External auth plugins push tokens via `chat.auth.set { provider, token }`.
--     The adapter forwards only to the matching provider (by spawn name).
--   - openai-provider has NO built-in OAuth flow. `/login` (chat.login_requested)
--     transitions to `error` with a message pointing at an external auth plugin.
--   - For local Ollama: omit --api-key and ignore the `login_required`
--     status — Ollama serves requests without auth anyway.
--
-- Limitations in v1:
--   - No tool calls (raw chat completions only).
--   - No vision / multi-modal input.
--   - History is in-memory; restarting the plugin loses prior turns.
--   - One turn at a time per process; concurrent `<prefix>.prompt` events
--     get rejected with `<prefix>.turn.error { message = "busy" }`.
--   - No built-in login flow (see auth state above).
return {}
