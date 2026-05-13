-- starter/compositors/provider.lua — engine-side actor for
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
--   <prefix>.session.stats         → chat.session.stats
--   <prefix>.auth.status           → chat.auth.status (+ provider)
--   <prefix>.models.listed         → chat.models.listed (+ provider)
--   <prefix>.model.set_ack         → chat.model.set_ack (+ provider)
--   <prefix>.turn.error            → chat.message.append (system)
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
--   chat.model.set                 → <prefix>.model.set (this file adds chat_id
--                                                       from agentic-loop state)
--
-- ## Orchestrator coupling (lives in this file)
--
-- `<prefix>.chat.complete.result` and `<prefix>.chat.error` require an
-- agentic-loop lookup (pending entry by chat_id) + a synthesized
-- canonical `tool.result` envelope on the bus. The lib returns these
-- envelopes' bodies unchanged; this file pattern-matches on the
-- prefixed kind and does the lookup itself.
--
-- Stream-delta gating (suppressed sub-graph chats) and pending-dispatch
-- flushing are pure orchestrator state — the lib doesn't see them.
--
-- ## Replay window
--
-- When `env.replay` is set, the lib's `replay_rebuild(env, name)`
-- handles the entire rebuild path (chat.create re-feed, chat.append
-- re-feed with ownership, tool.result → assistant chat.append
-- synthesis). This file just delegates.

local envelope = require("core.envelope")
local provider_lib = require("openai-provider")

local M = {}

function M.spawn_spec(name, command, opts)
  if type(name) ~= "string" or #name == 0 then
    error("provider.spawn_spec: name required, got " .. type(name))
  end
  if type(command) ~= "table" then
    error("provider.spawn_spec: command must be a table, got " .. type(command))
  end
  opts = opts or {}

  local translator = provider_lib.translator(name)
  local kinds = translator.kinds

  -- Lazy-bind to agentic-loop — module load order can require this
  -- file before agentic-loop's spawn line in init.lua.
  local agentic_loop
  local function al()
    if agentic_loop == nil then
      agentic_loop = require("agentic-loop")
    end
    return agentic_loop
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
      -- Stream-suppression gate. `stream_suppressed`
      -- collapses tracked pending entries whose reasoner type isn't in
      -- STREAM_VISIBLE_TYPES (sub-graph responder, etc.) with the
      -- explicit stream-hidden registration the agent reasoner installs
      -- for each sub-firing's chat_id (so the user doesn't see the
      -- sub-agent's internal-turn streams interleaved with the lead's
      -- response).
      if type(chat_id) == "string" and al().stream_suppressed(chat_id) then
        return nil
      end

      if (k == "chat.stream.delta" or k == "chat.stream.reasoning_delta")
          and type(chat_id) == "string"
          and al().stream_visible(chat_id) then
        al().flush_pending_dispatches()
      end

      if type(chat_id) == "string" and al().stream_visible(chat_id) then
        if k == "chat.stream.delta" then
          local txt = body.text or body.delta or ""
          if type(txt) == "string" then al().fire_stream_observers(txt) end
        elseif k == "chat.stream.reasoning_delta" then
          local txt = body.text or body.delta or ""
          if type(txt) == "string" then al().fire_reasoning_observers(txt) end
        end
      end
      return body
    end

    -- chat.error (lib left the prefixed kind alone): if a pending node
    -- is open for this chat_id, close it with an error tool.result;
    -- otherwise drop the prefixed envelope on the floor.
    if k == kinds.chat_error then
      local chat_id = body.chat_id
      if type(chat_id) == "string" then
        local entry = al().take_pending_for_chat(chat_id)
        if entry then
          local emsg = tostring(body.message or "provider error")
          nefor.log.warn("provider <- chat.error closing node", {
            provider = name, chat_id = chat_id,
            run_id = entry.run_id, node_id = entry.node_id, error = emsg,
          })
          envelope.emit_as(entry.reasoner or "reasoners", nil, {
            kind  = "tool.result",
            id    = entry.firing_id,
            error = emsg,
          })
          return body
        end
      end
      return nil
    end

    -- chat.complete.result: if a pending node is open for this
    -- chat_id, synthesize a canonical tool.result. Drop the prefixed
    -- envelope after synthesis (it's the binary-shaped result, not
    -- the canonical one consumers want). Pass through if no pending
    -- entry is found.
    if k == kinds.chat_complete_result then
      local chat_id = body.chat_id
      if type(chat_id) ~= "string" then return body end
      local entry = al().peek_pending_for_chat(chat_id)
      if not entry then return body end

      local out = body.output
      local was_stream_visible = al().stream_visible(chat_id)
      al().take_pending_for_chat(chat_id)

      if was_stream_visible then
        al().flush_pending_dispatches()
      end

      local from_id = entry.reasoner or "reasoners"
      if type(out) == "table" then
        nefor.log.info("provider <- chat.complete.result", {
          provider = name, chat_id = chat_id,
          run_id = entry.run_id, node_id = entry.node_id,
          text_len = type(out.text) == "string" and #out.text or 0,
          text_preview = type(out.text) == "string" and string.sub(out.text, 1, 80) or nil,
          finish_reason = out.finish_reason,
        })
        local result = {}
        for rk, rv in pairs(out) do result[rk] = rv end
        result.next_state = { chat_id = chat_id }
        envelope.emit_as(from_id, nil, {
          kind   = "tool.result",
          id     = entry.firing_id,
          result = result,
        })
      else
        nefor.log.warn("provider <- chat.complete.result with non-object output", {
          provider = name, chat_id = chat_id, out_type = type(out),
        })
        envelope.emit_as(from_id, nil, {
          kind  = "tool.result",
          id    = entry.firing_id,
          error = "provider returned non-object output",
        })
      end
      return nil
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

      local body = translator.outbound(env)
      if body ~= nil then
        body = handle_orchestrator_outbound(body)
        if body ~= nil then
          translator.publish(env.from or name, body)
        end
      end
    end
  end

  -- to_plugin (bus → binary) — per-envelope:
  --   1. env.replay: hand off to lib.replay_rebuild (full rebuild path).
  --   2. translator.inbound: kind rename, drop UI-shaped prompts,
  --      target-filter provider-scoped envelopes.
  --   3. chat.model.set fix-up: attach the active chat_id from
  --      agentic-loop's current_state (the lib returns the bare
  --      provider+model body — orchestrator state lives here).
  --   4. translator.deliver to the peer's stdin.
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if env.replay then
        provider_lib.replay_rebuild(env, name)
      else
        local body = translator.inbound(env)
        if body ~= nil then
          if body.kind == kinds.model_set then
            local active_chat_id
            local current_state = al().current_state()
            if type(current_state) == "table"
                and type(current_state.chat_id) == "string" then
              active_chat_id = current_state.chat_id
            end
            body.chat_id = active_chat_id
          end
          translator.deliver(body)
        end
      end
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
