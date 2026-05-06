-- starter/openai-provider/init.lua — wrapper actor for the
-- openai-provider Rust binary.
--
-- Exposes a constructor that takes the wire-protocol identity
-- (`name`, e.g. "ollama" or "openai") and the runtime arguments
-- (binary path, base URL, model, extra args). Returns the actor spec
-- ready for `actor.spawn(...)`.
--
-- ## Translation
--
-- The wrapper owns the protocol translation between the binary's
-- native shape (`<prefix>.chat.create`, `<prefix>.stream.delta`, …)
-- and the canonical bus shape the agentic-loop + TUI consume:
--
--   * `from_plugin` (binary → bus):
--       <prefix>.chat.complete.result  → graph.node_result via
--                                        agentic-loop pending lookup
--       <prefix>.chat.error            → graph.node_result.error
--                                        (closes the in-flight node)
--       <prefix>.stream.delta          → chat.stream.delta
--       <prefix>.stream.end            → chat.stream.end
--       <prefix>.stream.reasoning_*    → chat.stream.reasoning_*
--       <prefix>.session.stats         → chat.session.stats
--       <prefix>.auth.status           → chat.auth.status (+ provider)
--       <prefix>.models.listed         → chat.models.listed (+ provider)
--       <prefix>.model.set_ack         → chat.model.set_ack (+ provider)
--       <prefix>.turn.error            → chat.message.append (system)
--       <prefix>.hello                 → chat.model.set_ack (model fanout)
--       <prefix>.ready                 → drop (after auth.set injection)
--       <prefix>.goodbye               → drop
--
--   * `to_plugin`   (bus → binary):
--       chat.input.submit              → <prefix>.prompt
--       chat.interrupt                 → <prefix>.interrupt
--       chat.reset                     → <prefix>.reset
--       chat.auth.set                  → <prefix>.auth.set (target match)
--       chat.login_requested           → <prefix>.login_requested
--       chat.logout_requested          → <prefix>.logout_requested
--       chat.model.list_requested      → <prefix>.models.list_requested
--       chat.model.set                 → <prefix>.model.set (with active
--                                        chat_id from agentic-loop's
--                                        current_state)
--
-- The translation is deliberately preserved verbatim from the prior
-- `agentic_workflow.for_provider()` factory — the heavy state lookup
-- (chat_id_to_key, pending, chat_id_stream_visible) lives on the
-- agentic-loop actor and the wrapper just calls the helpers.

local M = {}

-- Construct the actor spec. Pluggable provider name + command let
-- both `starter/init.lua` (production) and tests/CI configurations
-- reuse the same wrapper without a separate plugin folder.
function M.spawn_spec(name, command, opts)
  assert(type(name) == "string" and #name > 0,
    "openai-provider.spawn_spec: name required")
  assert(type(command) == "table",
    "openai-provider.spawn_spec: command must be a table")
  opts = opts or {}
  local static_token = opts.static_token  -- optional

  local prefix = name .. "."
  local result_kind = prefix .. "chat.complete.result"
  local stream_delta_kind = prefix .. "stream.delta"
  local stream_end_kind   = prefix .. "stream.end"
  local stream_reasoning_delta_kind = prefix .. "stream.reasoning_delta"
  local stream_reasoning_end_kind   = prefix .. "stream.reasoning_end"
  local session_stats_kind = prefix .. "session.stats"

  -- Lazy bind to agentic-loop. The wrapper module is required from
  -- init.lua before agentic-loop's spawn line in a strict reading,
  -- but this constructor runs synchronously at spawn time so the
  -- order doesn't matter — `actor.spawn(require("agentic-loop"))`
  -- runs first in init.lua.
  local agentic_loop  -- bound on first envelope

  local function al()
    if agentic_loop == nil then
      agentic_loop = require("agentic-loop")
    end
    return agentic_loop
  end

  -- ----------------------------------------------------------------
  -- inner_from — chat ownership + stream gating + spawn flush.
  -- Source: agentic_workflow.for_provider's inner_from.
  -- ----------------------------------------------------------------
  local function inner_from(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind

    -- Stream-side kinds carry chat_id; gate sub-graph streams from
    -- reaching nefor-tui. D-26, D-28.
    if kind == stream_delta_kind
        or kind == stream_end_kind
        or kind == stream_reasoning_delta_kind
        or kind == stream_reasoning_end_kind
        or kind == session_stats_kind then
      local chat_id = env.body.chat_id
      if type(chat_id) == "string"
          and al().peek_pending_for_chat(chat_id) ~= nil
          and not al().stream_visible(chat_id) then
        return nil
      end

      -- D-31: first stream.delta / stream.reasoning_delta from a
      -- stream-visible chat releases queued sub-graph dispatches.
      if (kind == stream_delta_kind or kind == stream_reasoning_delta_kind)
          and type(chat_id) == "string"
          and al().stream_visible(chat_id) then
        al().flush_pending_dispatches()
      end

      -- Fire stream/reasoning observers AFTER the gate check so
      -- callers only see stream-visible deltas.
      if type(chat_id) == "string" and al().stream_visible(chat_id) then
        if kind == stream_delta_kind then
          local txt = env.body.text or env.body.delta or ""
          if type(txt) == "string" then al().fire_stream_observers(txt) end
        elseif kind == stream_reasoning_delta_kind then
          local txt = env.body.text or env.body.delta or ""
          if type(txt) == "string" then al().fire_reasoning_observers(txt) end
        end
      end

      return env
    end

    -- Provider-side failure path: openai-provider emits chat.error
    -- when chat.create / chat.append / chat.complete cant land.
    -- agentic_workflow ships chat.create / chat.append (system) /
    -- chat.append (user) / chat.complete in sequence per turn — when
    -- the first one fails, all four emit chat.error. Pass the FIRST
    -- through; silently drop the rest.
    if kind == prefix .. "chat.error" then
      local chat_id = env.body.chat_id
      if type(chat_id) == "string" then
        local entry = al().take_pending_for_chat(chat_id)
        if entry then
          local emsg = tostring(env.body.message or "provider error")
          nefor.log.warn("openai-provider <- chat.error closing node", {
            provider = name, chat_id = chat_id,
            run_id = entry.run_id, node_id = entry.node_id, error = emsg,
          })
          local nefor_engine_send = nefor.engine.send
          local payload = nefor.json.encode({
            type = "event", from = "engine", ts = nefor.engine.now(),
            body = {
              kind = "graph.node_result",
              run_id = entry.run_id, node_id = entry.node_id,
              firing_id = entry.firing_id,
              error = emsg,
            },
          })
          for _, peer in ipairs(nefor.engine.plugins()) do
            nefor_engine_send(payload, peer)
          end
          return env
        end
      end
      -- No pending — duplicate from cascade. Drop.
      return nil
    end

    if kind ~= result_kind then return env end
    local chat_id = env.body.chat_id
    if type(chat_id) ~= "string" then return env end
    local entry = al().peek_pending_for_chat(chat_id)
    if not entry then return env end

    local out = env.body.output
    local was_stream_visible = al().stream_visible(chat_id)
    al().take_pending_for_chat(chat_id)

    -- D-31 backup flush: covers wrap firings that go straight from
    -- chat.complete → tool-call result with zero deltas.
    if was_stream_visible then
      al().flush_pending_dispatches()
    end

    -- Emit graph.node_result via the engine binding. We can't reuse
    -- envelope.emit_broadcast directly because that's loaded by
    -- agentic-loop and the wrapper is supposed to stay light; but
    -- envelope.emit/emit_broadcast IS available everywhere via the
    -- lib path so let's use it.
    local envelope = require("lib.envelope")
    if type(out) == "table" then
      nefor.log.info("openai-provider <- chat.complete.result", {
        provider = name, chat_id = chat_id,
        run_id = entry.run_id, node_id = entry.node_id,
        text_len = type(out.text) == "string" and #out.text or 0,
        text_preview = type(out.text) == "string" and string.sub(out.text, 1, 80) or nil,
        finish_reason = out.finish_reason,
      })
      envelope.emit_broadcast({
        kind = "graph.node_result",
        run_id = entry.run_id, node_id = entry.node_id,
        firing_id = entry.firing_id,
        output = out,
        next_state = { chat_id = chat_id },
      })
    else
      nefor.log.warn("openai-provider <- chat.complete.result with non-object output", {
        provider = name, chat_id = chat_id, out_type = type(out),
      })
      envelope.emit_broadcast({
        kind = "graph.node_result",
        run_id = entry.run_id, node_id = entry.node_id,
        firing_id = entry.firing_id,
        error = "provider returned non-object output",
      })
    end
    return nil
  end

  -- ----------------------------------------------------------------
  -- outer_from — chat-contract translation.
  -- Source: agentic_workflow.for_provider's outer_from.
  -- ----------------------------------------------------------------
  local injected_static = false

  local function outer_from(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local k = env.body.kind
    if type(k) ~= "string" then return env end

    if k == prefix .. "stream.delta" then
      env.body.kind = "chat.stream.delta"
    elseif k == prefix .. "stream.reasoning_delta" then
      env.body.kind = "chat.stream.reasoning_delta"
    elseif k == prefix .. "stream.reasoning_end" then
      env.body.kind = "chat.stream.reasoning_end"
    elseif k == prefix .. "stream.end" then
      env.body.kind = "chat.stream.end"
      env.body.finish_reason = nil
    elseif k == prefix .. "session.stats" then
      env.body.kind = "chat.session.stats"
    elseif k == prefix .. "auth.status" then
      env.body.kind = "chat.auth.status"
      env.body.provider = name
    elseif k == prefix .. "models.listed" then
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
    elseif k == prefix .. "chat.error" then
      local msg = tostring(env.body.message or "provider error")
      env.body = {
        kind = "chat.message.append",
        role = "system",
        text = "Error: " .. msg,
      }
    elseif k == prefix .. "hello" then
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
    elseif k == prefix .. "ready" or k == prefix .. "goodbye" then
      if k == prefix .. "ready"
          and static_token ~= nil
          and not injected_static
          and nefor and nefor.engine and nefor.engine.send and nefor.json then
        injected_static = true
        local payload = nefor.json.encode({
          type = "event", from = "engine", ts = nefor.engine.now(),
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

  -- ----------------------------------------------------------------
  -- outer_to — bus → binary. chat.* → <prefix>.*.
  -- Source: agentic_workflow.for_provider's outer_to.
  -- ----------------------------------------------------------------
  local function outer_to(env)
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
      env.body = { kind = prefix .. "auth.set", token = token }
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
      local cfg_state = al().config()
      -- Propagate the active orchestrator chat_id so the provider can
      -- retarget that chat.
      local active_chat_id
      local current_state = al()._internals and al()._internals.state.current_state
      if type(current_state) == "table" and type(current_state.chat_id) == "string" then
        active_chat_id = current_state.chat_id
      end
      env.body = {
        kind    = prefix .. "model.set",
        model   = model,
        chat_id = active_chat_id,
      }
    end
    return env
  end

  -- ----------------------------------------------------------------
  -- compose: inner runs first on ingress; outer last on egress.
  -- ----------------------------------------------------------------
  local function from_plugin(env)
    local e = env
    e = inner_from(e)
    if e == nil then return nil end
    e = outer_from(e)
    return e
  end

  local function to_plugin(env)
    return outer_to(env)
  end

  return {
    name        = name,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,  -- stateless on the actor side
  }
end

return M
