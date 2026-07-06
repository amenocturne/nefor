-- lua/libs/compositors/provider.lua — engine-side actor for
-- OpenAI-compatible providers. Threads the openai-provider plugin
-- lib's translation primitives with starter-owned `agentic-loop`
-- orchestrator state.
--
-- The mock-plugin Rust binary speaks the same provider wire-protocol
-- (`<prefix>.chat.create`, `<prefix>.stream.delta`, …), so it uses the
-- exact same actor spec with a different command; no separate wrapper
-- needed — callers point `command` at the mock binary.
--
-- ## Translation (delegated to the lib)
--
--   <prefix>.stream.delta          → chat.stream.delta
--   <prefix>.stream.end            → chat.stream.end
--   <prefix>.stream.reasoning_*    → chat.stream.reasoning_*
--   <prefix>.stream.retry          → chat.toast
--   <prefix>.session.stats         → chat.session.stats
--   <prefix>.auth.status           → chat.auth.status (+ provider)
--   <prefix>.models.listed         → chat.models.listed (+ provider)
--   <prefix>.model.set_ack         → chat.model.set_ack (+ provider)
--   <prefix>.reasoning.set_ack     → chat.reasoning.set_ack (+ provider)
--   <prefix>.turn.error            → chat.message.append (system)
--   <prefix>.turn.error {chat_id}  → <prefix>.chat.error
--   <prefix>.hello                 → chat.model.set_ack (model fanout)
--   <prefix>.ready                 → drop (after auth.set injection)
--   <prefix>.goodbye               → drop
--
--   chat.input.submit              → drop (lib doesn't accept UI-shaped prompts)
--   chat.interrupt_all             → drop (single-chat cancel via chat.interrupt)
--   chat.interrupt                 → <prefix>.interrupt
--   chat.reset                     → <prefix>.reset
--   chat.auth.set                  → <prefix>.auth.set (target match)
--   chat.login_requested           → <prefix>.login_requested
--   chat.logout_requested          → <prefix>.logout_requested
--   chat.model.list_requested      → <prefix>.models.list_requested
--   chat.model.set                 → <prefix>.model.set
--   chat.reasoning.set             → <prefix>.reasoning.set
--
-- ## Orchestrator coupling (lives in this file)
--
-- Stream deltas on the lead's prefix-bound kernel chats fire the
-- agentic-loop's public stream/reasoning observers (the CLI surface
-- reads those); `<prefix>.chat.complete.result` feeds the history
-- mirror. The lib returns these envelopes' bodies unchanged; this file
-- pattern-matches on the prefixed kind.
--
-- ## Replay window
--
-- When `env.replay` is set, the lib's `replay_rebuild(env, name)`
-- handles the entire rebuild path (chat.create re-feed, chat.append
-- re-feed with ownership, tool.result → assistant chat.append
-- synthesis). This file just delegates.

local M = {}

-- opts.hooks lets downstream callers splice per-envelope logic into the
-- two translation seams without forking this file. Both hooks are
-- optional; absent hooks leave the pipeline byte-for-byte identical to
-- the no-hook path.
--
--   intercept_inbound(env, helpers) — runs in from_plugin between
--     translator.maybe_inject_static_token and translator.outbound.
--     May mutate env.body in place (e.g. rewriting body.text or
--     body.kind) and may emit synthetic envelopes via
--     helpers.emit_synthetic(from, body). Return false to drop the env;
--     any other return value continues normal processing.
--
--   intercept_to_plugin(env) — runs in to_plugin inside the
--     non-replay branch, before translator.inbound. Same drop / continue
--     semantics. Replay envelopes always take the lib path.
--
-- helpers = { kinds = translator.kinds, name = <provider name>,
--             emit_synthetic = <function(from, body)> }. emit_synthetic
-- writes a wire-shape event envelope to the bus so callers don't need
-- to know the framing.
--
--   translator_lib (optional) — Lua module name to require for the
--     translator. Defaults to "openai-provider". Set to your provider's
--     lua dir name when the binary emits the same wire kinds but lives
--     under a different require path (e.g. "chatgpt-provider").
--
--   agentic_loop (required) — the agentic-loop module table, injected
--     by the caller. Eliminates the require-time cycle between this
--     compositor and agentic-loop.
function M.spawn_spec(name, command, opts)
  if type(name) ~= "string" or #name == 0 then
    error("provider.spawn_spec: name required, got " .. type(name))
  end
  if type(command) ~= "table" then
    error("provider.spawn_spec: command must be a table, got " .. type(command))
  end
  opts = opts or {}
  local hooks = opts.hooks or {}
  local al = opts.agentic_loop
  if al == nil then
    error("provider.spawn_spec: opts.agentic_loop is required")
  end

  local provider_lib = require(opts.translator_lib or "openai-provider")
  local translator = provider_lib.translator(name)
  local kinds = translator.kinds

  local function emit_synthetic(from, body)
    nefor.engine.send(nefor.json.encode({
      type = "event",
      from = from,
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  local hook_helpers = {
    kinds          = kinds,
    name           = name,
    emit_synthetic = emit_synthetic,
  }

  local function clone_table(t)
    if type(t) ~= "table" then return t end
    local out = {}
    for k, v in pairs(t) do
      out[k] = type(v) == "table" and clone_table(v) or v
    end
    return out
  end

  local function publish_history_create(body)
    if type(body.chat_id) ~= "string" then return end
    emit_synthetic(name, {
      kind             = "chat.history.create",
      provider         = name,
      chat_id          = body.chat_id,
      model            = body.model,
      system           = body.system,
      tools            = clone_table(body.tools),
      reasoning_effort = body.reasoning_effort,
    })
  end

  local function publish_history_message(chat_id, message)
    if type(chat_id) ~= "string" or type(message) ~= "table" then return end
    emit_synthetic(name, {
      kind     = "chat.history.message",
      provider = name,
      chat_id  = chat_id,
      message  = clone_table(message),
    })
  end

  local function normalize_tool_calls(tcs)
    if type(tcs) ~= "table" then return nil end
    local out = {}
    for _, tc in ipairs(tcs) do
      if type(tc) == "table" then
        if type(tc["function"]) == "table" then
          table.insert(out, clone_table(tc))
        elseif type(tc.id) == "string" and type(tc.name) == "string" then
          local arguments = tc.arguments
          if type(arguments) ~= "string" then
            local ok, encoded = pcall(nefor.json.encode, arguments == nil and {} or arguments)
            arguments = ok and encoded or "{}"
          end
          table.insert(out, {
            id = tc.id,
            type = "function",
            ["function"] = {
              name = tc.name,
              arguments = arguments,
            },
          })
        end
      end
    end
    if #out == 0 then return nil end
    return out
  end

  local function assistant_message_from_output(output)
    if type(output) ~= "table" then return nil end
    local text = type(output.text) == "string" and output.text or ""
    local tcs = normalize_tool_calls(output.tool_calls)
    local has_text = #text > 0
    local has_tcs = type(tcs) == "table" and #tcs > 0
    if not has_text and not has_tcs then return nil end
    local message = { role = "assistant", content = text }
    if has_tcs then message.tool_calls = tcs end
    return message
  end

  local function handle_orchestrator_outbound(body)
    local k = body.kind
    if type(k) ~= "string" then return body end

    if k == "chat.stream.delta"
        or k == "chat.stream.end"
        or k == "chat.stream.reasoning_delta"
        or k == "chat.stream.reasoning_end"
        or k == "chat.session.stats" then
      local chat_id = body.chat_id
      if type(chat_id) == "string" and al.stream_visible(chat_id) then
        if k == "chat.stream.delta" then
          local txt = body.text or body.delta or ""
          if type(txt) == "string" then al.fire_stream_observers(txt) end
        elseif k == "chat.stream.reasoning_delta" then
          local txt = body.text or body.delta or ""
          if type(txt) == "string" then al.fire_reasoning_observers(txt) end
        end
      end
      return body
    end

    -- chat.error / chat.complete.result keep their prefixed kinds on
    -- the bus: the mag plugin's provider bridge correlates them back to
    -- the kernel request by chat_id.
    -- chat.complete.result additionally mirrors the assistant message
    -- into the history stream before passing through.
    if k == kinds.chat_complete_result then
      local chat_id = body.chat_id
      if type(chat_id) ~= "string" then return body end
      publish_history_message(chat_id, assistant_message_from_output(body.output))
      return body
    end

    return body
  end

  -- from_plugin (binary → bus) — four steps per envelope:
  --   1. translator.maybe_inject_static_token: bus-quiet auth.set
  --      injection on first ready (no-op otherwise).
  --   2. translator.outbound: kind rename, or nil for ready/goodbye.
  --   3. handle_orchestrator_outbound: agentic-loop coupling for
  --      stream / chat.error / chat.complete.result.
  --   4. publish via translator.publish (preserves env.from).
  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      -- Static-token injection runs even when outbound drops the body
      -- (ready/goodbye both return nil from outbound).
      translator.maybe_inject_static_token(env, opts)

      if hooks.intercept_inbound then
        if hooks.intercept_inbound(env, hook_helpers) == false then
          goto continue
        end
      end

      if type(env.body) == "table"
          and env.body.kind == kinds.turn_error
          and type(env.body.chat_id) == "string" then
        env.body.kind = kinds.chat_error
        env.body.message = env.body.message or env.body.error or "provider error"
      end

      local body = translator.outbound(env)
      if body ~= nil then
        body = handle_orchestrator_outbound(body)
        if body ~= nil then
          translator.publish(env.from or name, body)
        end
      end
      ::continue::
    end
  end

  -- to_plugin (bus → binary) — per-envelope:
  --   1. env.replay: hand off to lib.replay_rebuild (full rebuild path).
  --   2. translator.inbound: kind rename, drop UI-shaped prompts,
  --      target-filter provider-scoped envelopes.
  --   3. translator.deliver to the peer's stdin.
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      local kind = type(env.body) == "table" and env.body.kind or nil
      if env.replay or kind == "sessions.replay.end" then
        provider_lib.replay_rebuild(env, name)
      else
        if hooks.intercept_to_plugin then
          if hooks.intercept_to_plugin(env) == false then
            goto continue
          end
        end
        if type(env.body) == "table"
            and env.body.kind == "chat.reasoning.set"
            and env.body.provider == nil then
          local cfg = al.config and al.config() or nil
          if type(cfg) == "table" and cfg.provider == name then
            env.body.provider = name
          end
        end

        local body = translator.inbound(env)
        if body ~= nil then
          if body.kind == kinds.chat_create then
            publish_history_create(body)
          elseif body.kind == kinds.chat_append then
            publish_history_message(body.chat_id, body.message)
          end
          translator.deliver(body)
        end
      end
      ::continue::
    end
  end

  return {
    name        = name,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

return M
