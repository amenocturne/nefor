-- starter/openai_provider_adapter.lua — generic openai-provider ↔ chat-contract
-- adapter factory.
--
-- openai-provider emits a per-instance native event namespace (`<name>.*`)
-- determined by the `--name` CLI flag on the spawned process. nefor-chat
-- consumes `chat-contract v0.1` (`chat.*`). This module is the bridge,
-- applied as `from_plugin` / `to_plugin` transforms on each spawn.
--
-- Because the same openai-provider binary can be spawned multiple times under
-- different plugin names (one per provider), this module exports a
-- FACTORY: call `make("ollama")` to get a transform pair tied to the
-- `ollama.*` namespace, `make("groq")` for `groq.*`, and so on.
--
-- Usage:
--   local mk = require("openai_provider_adapter").make
--   local ollama = mk("ollama")
--   ncp.spawn {
--     name        = "ollama",
--     command     = {
--       bin("openai-provider"),
--       "--name",     "ollama",
--       "--base-url", "http://localhost:11434",
--       "--model",    "qwen2.5-coder:7b",
--     },
--     from_plugin = ollama.from_plugin,
--     to_plugin   = ollama.to_plugin,
--   }
--
-- Optional `opts.static_token`: when set, the adapter watches for the
-- provider's first `<prefix>.ready` (post-handshake event-level ready) and
-- injects a `<prefix>.auth.set { token = static_token }` back to the same
-- plugin. Useful for backends with no real auth flow (e.g. local Ollama)
-- where any non-empty token unblocks the openai-provider's auth gate.

local M = {}

-- Build a `{from_plugin, to_plugin}` transform pair scoped to a single
-- openai-provider instance whose event-kind prefix is `name .. "."`.
--
-- `opts` (optional table):
--   * `static_token` — string to push back as `<prefix>.auth.set` once the
--     plugin's first `<prefix>.ready` is observed. Skipped if nil/empty.
function M.make(name, opts)
  assert(type(name) == "string" and #name > 0,
         "openai_provider_adapter.make: provider name must be a non-empty string")
  opts = opts or {}
  local static_token = opts.static_token
  if static_token ~= nil then
    assert(type(static_token) == "string" and #static_token > 0,
           "openai_provider_adapter.make: opts.static_token must be a non-empty string")
  end
  local prefix = name .. "."
  -- Latch so we only inject the static-token auth.set once even if the plugin
  -- somehow re-emits ready (shouldn't happen post-handshake, but be defensive).
  local injected_static = false

  -- Events FROM openai-provider, mapped onto the chat contract.
  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local k = env.body.kind
    if type(k) ~= "string" then return env end

    if k == prefix .. "stream.delta" then
      -- Plugin-side carries an `id` for correlation; chat-contract
      -- doesn't, but the extra field is harmless to nefor-chat (extra
      -- keys are ignored).
      env.body.kind = "chat.stream.delta"
    elseif k == prefix .. "stream.end" then
      -- Keep model + duration_ms for the per-turn footer; drop the
      -- finish_reason since chat-contract doesn't render it.
      env.body.kind = "chat.stream.end"
      env.body.finish_reason = nil
    elseif k == prefix .. "session.stats" then
      env.body.kind = "chat.session.stats"
    elseif k == prefix .. "auth.status" then
      -- Inject `provider = name` so chat can group statuses by provider.
      env.body.kind = "chat.auth.status"
      env.body.provider = name
    elseif k == prefix .. "models.listed" then
      -- Inject `provider = name` so chat can label which provider this list
      -- came from when rendering. `models` array passes through unchanged.
      env.body.kind = "chat.models.listed"
      env.body.provider = name
    elseif k == prefix .. "model.set_ack" then
      env.body.kind = "chat.model.set_ack"
      env.body.provider = name
    elseif k == prefix .. "turn.error" then
      local msg = tostring(env.body.message or "(unknown)")
      if msg == "interrupted" then
        env.body = {
          kind = "chat.message.append",
          role = "system",
          text = "[interrupted]",
        }
      else
        env.body = {
          kind = "chat.message.append",
          role = "system",
          text = "Error: " .. msg,
        }
      end
    elseif k == prefix .. "hello" then
      -- Translate `<prefix>.hello { model = ... }` into a synthetic
      -- `chat.model.set_ack { provider, model }` so nefor-chat seeds
      -- `active_model_per_provider` (and the statusline) before the first
      -- turn — without this the model column is empty until the user runs
      -- `/model <name>` once.
      local model = env.body.model
      if type(model) == "string" and #model > 0 then
        env.body = {
          kind     = "chat.model.set_ack",
          provider = name,
          model    = model,
        }
        return env
      end
      return nil
    elseif k == prefix .. "ready"
        or k == prefix .. "goodbye" then
      -- Internal lifecycle — drop, nefor-chat doesn't need to see them.
      -- For the first `<prefix>.ready` we side-effect a synthetic
      -- `<prefix>.auth.set` back to this plugin if the caller wired up a
      -- `static_token`. Lets local-only backends (Ollama) be unlocked
      -- without an env var on the provider crate.
      if k == prefix .. "ready"
          and static_token ~= nil
          and not injected_static
          and nefor and nefor.engine and nefor.engine.send and nefor.json then
        injected_static = true
        -- NCP §3 requires `from` + `ts` on every wire envelope; the engine
        -- forwards `nefor.engine.send` payloads verbatim, so the adapter
        -- stamps them itself. Without this the provider's stdin parser
        -- rejects the line and silently drops it.
        local payload = nefor.json.encode({
          type = "event",
          from = "engine",
          ts   = nefor.engine.now(),
          body = {
            kind  = prefix .. "auth.set",
            token = static_token,
          },
        })
        nefor.engine.send(payload, name)
      end
      return nil
    end
    return env
  end

  -- Events TO openai-provider, translated from chat-contract emissions.
  -- The `provider` field on chat.auth.* / chat.login_requested /
  -- chat.logout_requested tells the adapter which spawn this event is
  -- targeted at. Events with a non-matching `provider` field are
  -- dropped (nil) so the wrong plugin doesn't react.
  local function to_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local k = env.body.kind
    if type(k) ~= "string" then return env end

    if k == "chat.input.submit" then
      env.body.kind = prefix .. "prompt"
    elseif k == "chat.interrupt" then
      env.body.kind = prefix .. "interrupt"
    elseif k == "chat.reset" then
      env.body.kind = prefix .. "reset"
    elseif k == "chat.auth.set" then
      if env.body.provider ~= name then return nil end
      local token = env.body.token
      env.body = {
        kind = prefix .. "auth.set",
        token = token,
      }
    elseif k == "chat.login_requested" then
      if env.body.provider ~= name then return nil end
      env.body = { kind = prefix .. "login_requested" }
    elseif k == "chat.logout_requested" then
      if env.body.provider ~= name then return nil end
      env.body = { kind = prefix .. "logout_requested" }
    elseif k == "chat.model.list_requested" then
      if env.body.provider ~= name then return nil end
      env.body = { kind = prefix .. "models.list_requested" }
    elseif k == "chat.model.set" then
      if env.body.provider ~= name then return nil end
      local model = env.body.model
      env.body = { kind = prefix .. "model.set", model = model }
    end
    return env
  end

  return {
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
  }
end

return M
