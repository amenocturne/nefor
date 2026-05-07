-- starter/openai-provider/init.lua — wrapper actor for the
-- openai-provider Rust binary.
--
-- ## Translation
--
-- The wrapper owns the protocol translation between the binary's
-- native shape (`<prefix>.chat.create`, `<prefix>.stream.delta`, …)
-- and the canonical bus shape the agentic-loop + TUI consume.
--
-- Post wrapper-callback refactor, `from_plugin` and `to_plugin` are
-- side-effecting callbacks: the wrapper decides explicitly whether to
-- publish onto the bus (`nefor.engine.send`) or deliver to the peer
-- (`nefor.engine.deliver`). The framework calls the callback with the
-- parsed envelope and ignores the return value.
--
-- ## from_plugin (binary → bus)
--
-- Reactions to provider-emitted envelopes:
--   <prefix>.chat.complete.result  → tool.result { id=firing_id,
--                                    result: { ...output,
--                                              next_state: { chat_id } } }
--   <prefix>.chat.error            → tool.result { id=firing_id, error }
--                                    (closes the in-flight node)
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
-- ## to_plugin (bus → binary)
--
-- Translations the wrapper applies before delivering to the binary:
--   chat.input.submit              → <prefix>.prompt
--   chat.interrupt                 → <prefix>.interrupt
--   chat.reset                     → <prefix>.reset
--   chat.auth.set                  → <prefix>.auth.set (target match)
--   chat.login_requested           → <prefix>.login_requested
--   chat.logout_requested          → <prefix>.logout_requested
--   chat.model.list_requested      → <prefix>.models.list_requested
--   chat.model.set                 → <prefix>.model.set (with active
--                                    chat_id from agentic-loop's
--                                    current_state)
--
-- During a session replay (`sessions.replay.*` framing), `to_plugin`
-- takes a separate cross-process-resume rebuild path (see the inline
-- comment on `to_plugin` below). The provider binary is brand new on
-- every nefor process restart, so its per-chat_id history table is
-- empty; without rebuild on /resume the model replies with no memory
-- of the prior conversation. We re-feed `<prefix>.chat.create` and
-- `<prefix>.chat.append` envelopes into the binary verbatim, and
-- synthesize a `<prefix>.chat.append { role=assistant }` from each
-- replayed `tool.result` we own (the canonical close envelope carries
-- the assistant text + tool_calls but the live `chat.complete` path
-- pushes them inside the binary, so on replay — where chat.complete
-- is intentionally NOT re-delivered — there's no other channel for
-- the assistant turn to land in history). Other envelopes (chat.complete,
-- streaming deltas, tool.invoke, …) drop on the floor — they would
-- either re-trigger external side effects or close orchestrator nodes
-- that don't exist in the fresh process.

local json = nefor.json

local envelope = require("lib.envelope")

local M = {}

local function publish(from, body)
  nefor.engine.send(json.encode({
    type = "event",
    from = from,
    ts   = nefor.engine.now(),
    body = body,
  }))
end

-- Construct the actor spec. Pluggable provider name + command let
-- both `starter/init.lua` (production) and tests/CI configurations
-- reuse the same wrapper.
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

  -- Lazy-bind to agentic-loop because module load order can require it
  -- before agentic-loop's spawn line in init.lua.
  local agentic_loop  -- bound on first envelope

  local function al()
    if agentic_loop == nil then
      agentic_loop = require("agentic-loop")
    end
    return agentic_loop
  end

  -- ----------------------------------------------------------------
  -- inner_from — chat ownership + stream gating + spawn flush.
  -- Returns a body table to publish, or nil to drop the envelope.
  -- ----------------------------------------------------------------
  local function inner_from(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env.body end
    local kind = env.body.kind

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

      if (kind == stream_delta_kind or kind == stream_reasoning_delta_kind)
          and type(chat_id) == "string"
          and al().stream_visible(chat_id) then
        al().flush_pending_dispatches()
      end

      if type(chat_id) == "string" and al().stream_visible(chat_id) then
        if kind == stream_delta_kind then
          local txt = env.body.text or env.body.delta or ""
          if type(txt) == "string" then al().fire_stream_observers(txt) end
        elseif kind == stream_reasoning_delta_kind then
          local txt = env.body.text or env.body.delta or ""
          if type(txt) == "string" then al().fire_reasoning_observers(txt) end
        end
      end

      return env.body
    end

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
          envelope.emit_as(entry.reasoner or "reasoners", nil, {
            kind  = "tool.result",
            id    = entry.firing_id,
            error = emsg,
          })
          return env.body
        end
      end
      return nil
    end

    if kind ~= result_kind then return env.body end
    local chat_id = env.body.chat_id
    if type(chat_id) ~= "string" then return env.body end
    local entry = al().peek_pending_for_chat(chat_id)
    if not entry then return env.body end

    local out = env.body.output
    local was_stream_visible = al().stream_visible(chat_id)
    al().take_pending_for_chat(chat_id)

    if was_stream_visible then
      al().flush_pending_dispatches()
    end

    local from_id = entry.reasoner or "reasoners"
    if type(out) == "table" then
      nefor.log.info("openai-provider <- chat.complete.result", {
        provider = name, chat_id = chat_id,
        run_id = entry.run_id, node_id = entry.node_id,
        text_len = type(out.text) == "string" and #out.text or 0,
        text_preview = type(out.text) == "string" and string.sub(out.text, 1, 80) or nil,
        finish_reason = out.finish_reason,
      })
      local result = {}
      for k, v in pairs(out) do result[k] = v end
      result.next_state = { chat_id = chat_id }
      envelope.emit_as(from_id, nil, {
        kind   = "tool.result",
        id     = entry.firing_id,
        result = result,
      })
    else
      nefor.log.warn("openai-provider <- chat.complete.result with non-object output", {
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

  -- ----------------------------------------------------------------
  -- outer_from — chat-contract translation. Returns a body to publish
  -- (mutated copy) or nil to drop. Does NOT mutate env.body in place.
  -- ----------------------------------------------------------------
  local injected_static = false

  local function outer_from(env, body)
    if env.type ~= "event" or type(body) ~= "table" then return body end
    local k = body.kind
    if type(k) ~= "string" then return body end

    if k == prefix .. "stream.delta" then
      body.kind = "chat.stream.delta"
    elseif k == prefix .. "stream.reasoning_delta" then
      body.kind = "chat.stream.reasoning_delta"
    elseif k == prefix .. "stream.reasoning_end" then
      body.kind = "chat.stream.reasoning_end"
    elseif k == prefix .. "stream.end" then
      body.kind = "chat.stream.end"
      body.finish_reason = nil
    elseif k == prefix .. "session.stats" then
      body.kind = "chat.session.stats"
    elseif k == prefix .. "auth.status" then
      body.kind = "chat.auth.status"
      body.provider = name
    elseif k == prefix .. "models.listed" then
      body.kind = "chat.models.listed"
      body.provider = name
    elseif k == prefix .. "model.set_ack" then
      body.kind = "chat.model.set_ack"
      body.provider = name
    elseif k == prefix .. "turn.error" then
      local msg = tostring(body.message or "(unknown)")
      if msg == "interrupted" then
        body = {
          kind = "chat.message.append",
          role = "system",
          text = "[interrupted]",
        }
      else
        body = {
          kind = "chat.message.append",
          role = "system",
          text = "Error: " .. msg,
        }
      end
    elseif k == prefix .. "chat.error" then
      local msg = tostring(body.message or "provider error")
      body = {
        kind = "chat.message.append",
        role = "system",
        text = "Error: " .. msg,
      }
    elseif k == prefix .. "hello" then
      local model = body.model
      if type(model) == "string" and #model > 0 then
        body = {
          kind     = "chat.model.set_ack",
          provider = name,
          model    = model,
        }
        return body
      end
      return nil
    elseif k == prefix .. "ready" or k == prefix .. "goodbye" then
      if k == prefix .. "ready"
          and static_token ~= nil
          and not injected_static then
        injected_static = true
        local payload = json.encode({
          type = "event", from = "engine", ts = nefor.engine.now(),
          body = {
            kind  = prefix .. "auth.set",
            token = static_token,
          },
        })
        -- auth.set is a targeted control envelope to the provider —
        -- deliver direct (don't pollute the bus log).
        nefor.engine.deliver(name, payload)
      end
      return nil
    end
    return body
  end

  -- ----------------------------------------------------------------
  -- Per-envelope inbound logic — composed inner_from + outer_from +
  -- publish. Side-effecting; result returned to the iterating caller
  -- below for diagnostic clarity (it could be void), the iterator
  -- ignores it.
  -- ----------------------------------------------------------------
  local function handle_inbound(env)
    -- Deep-copy body to avoid mutating the caller's table when
    -- outer_from rewrites kinds.
    local body_copy = {}
    if type(env.body) == "table" then
      for k, v in pairs(env.body) do body_copy[k] = v end
    else
      return
    end
    local working = {
      type = env.type,
      from = env.from,
      body = body_copy,
    }

    local inner = inner_from(working)
    if inner == nil then return end

    local final_body = outer_from(working, inner)
    if final_body == nil then return end

    publish(env.from or name, final_body)
  end

  -- ----------------------------------------------------------------
  -- from_plugin callback: batched. Iterates envs, applies translation
  -- per envelope. Framework ignores return value.
  -- ----------------------------------------------------------------
  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      handle_inbound(env)
    end
  end

  -- ----------------------------------------------------------------
  -- Cross-process resume: rebuild the binary's per-chat_id history
  -- table from the recorded session log. Sessions replays the recorded
  -- step-origin envelopes; we filter to the ones that carry chat
  -- state for THIS provider and deliver them to the binary.
  --
  -- Owned chat_ids — populated when `<prefix>.chat.create` is
  -- delivered (live or replay). Used to discriminate replayed
  -- `tool.result` envelopes (which carry `result.next_state.chat_id`)
  -- so only the matching wrapper synthesizes the assistant chat.append.
  -- Mock-plugin chats vs openai-provider chats coexist on the same
  -- bus; without ownership filtering both wrappers would react to
  -- every replayed tool.result and corrupt each other's state.
  -- ----------------------------------------------------------------
  local owned_chat_ids = {}

  local function deliver_body(body)
    nefor.engine.deliver(name, json.encode({
      type = "event",
      from = "engine",
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  local function handle_replay(env)
    local body = env.body
    local k = body.kind
    if type(k) ~= "string" then return end

    -- chat.create: skip if we already created this chat in-process —
    -- the binary's `chats.create` errors on duplicate ids. Cross-
    -- process resume after a fresh nefor start has an empty owned set
    -- so first-seen chat.create gets through; in-process /resume of a
    -- chat we already created is a no-op for the binary's state, so
    -- dropping the duplicate is correct.
    if k == prefix .. "chat.create" then
      local cid = body.chat_id
      if type(cid) == "string" and owned_chat_ids[cid] then return end
      if type(cid) == "string" then owned_chat_ids[cid] = true end
      deliver_body(body)
      return
    end
    -- chat.append: re-feed verbatim only if we own the chat. Without
    -- the ownership gate every wrapper (mock-plugin + openai-provider
    -- + …) would deliver every replayed chat.append to its own binary,
    -- and a chat.append for an unknown chat_id emits chat.error.
    if k == prefix .. "chat.append" then
      local cid = body.chat_id
      if type(cid) ~= "string" or not owned_chat_ids[cid] then return end
      deliver_body(body)
      return
    end

    -- tool.result: synthesize an assistant `<prefix>.chat.append` so
    -- the assistant turn lands in history. The wrapper's live
    -- `inner_from` emits `tool.result` with
    -- `result.next_state.chat_id` set; that's the discriminator. Skip
    -- error-shaped results (no assistant content to record) and
    -- chat_ids we don't own.
    if k == "tool.result" then
      if body.error ~= nil then return end
      local result = body.result
      if type(result) ~= "table" then return end
      local ns = result.next_state
      local cid = type(ns) == "table" and ns.chat_id or nil
      if type(cid) ~= "string" or not owned_chat_ids[cid] then return end

      local text = type(result.text) == "string" and result.text or ""
      local tcs = result.tool_calls
      local has_text = #text > 0
      local has_tcs = type(tcs) == "table" and #tcs > 0
      if not has_text and not has_tcs then return end

      local message = { role = "assistant", content = text }
      if has_tcs then message.tool_calls = tcs end
      deliver_body({
        kind    = prefix .. "chat.append",
        chat_id = cid,
        message = message,
      })
      return
    end

    -- Everything else drops. chat.complete would re-trigger streaming;
    -- canonical chat.* (input.submit, model.set, …) would race the
    -- live agentic-loop, which already has its own replay gate.
  end

  -- ----------------------------------------------------------------
  -- Per-envelope outbound logic — chat.* → <prefix>.* + deliver. Pulled
  -- out of the to_plugin body so the batched callback below can iterate
  -- without re-indenting.
  -- ----------------------------------------------------------------
  local function handle_outbound(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return end

    if env.replay then
      handle_replay(env)
      return
    end

    -- Don't deliver back to self.
    if env.from == name then return end

    -- Deep-copy body to avoid mutating the caller's table.
    local body = {}
    for k, v in pairs(env.body) do body[k] = v end

    local k = body.kind
    if type(k) ~= "string" then
      -- Pass through — non-typed envelope, deliver as-is. Strip
      -- framework-only fields (`replay`, …) when encoding for the wire.
      nefor.engine.deliver(name, json.encode({
        type = env.type,
        from = env.from,
        ts   = env.ts,
        body = env.body,
      }))
      return
    end

    -- Track live-path chat.create so a subsequent in-process /resume
    -- (which replays the same chat.create through `handle_replay`) can
    -- recognise the chat_id as already-created and skip the duplicate
    -- delivery (the binary's `chats.create` errors on duplicate ids).
    -- chat.create is prefix-namespaced so we filter by our prefix; the
    -- envelope falls through to the default deliver below.
    if k == prefix .. "chat.create" and type(body.chat_id) == "string" then
      owned_chat_ids[body.chat_id] = true
    end

    if k == "chat.input.submit" or k == "chat.interrupt_all" then
      -- Phase 3+ orchestration: agentic-loop owns the user-prompt path
      -- (builds the orchestrator graph and fires `tool.invoke`s). The
      -- provider binary doesn't see `<prefix>.prompt` — prompts arrive
      -- via canonical `tool.invoke{name=<provider>, args}`. Drop on
      -- delivery so this wrapper doesn't accidentally re-introduce
      -- the legacy chat.input → prompt fan-out. Same for the
      -- interrupt-all fanout — the agentic-loop owns cancel.
      return
    elseif k == "chat.interrupt" then
      body.kind = prefix .. "interrupt"
    elseif k == "chat.reset" then
      body.kind = prefix .. "reset"
    elseif k == "chat.auth.set" then
      if body.provider ~= name then return end
      local token = body.token
      body = { kind = prefix .. "auth.set", token = token }
    elseif k == "chat.login_requested" then
      if body.provider ~= name then return end
      body = { kind = prefix .. "login_requested" }
    elseif k == "chat.logout_requested" then
      if body.provider ~= name then return end
      body = { kind = prefix .. "logout_requested" }
    elseif k == "chat.model.list_requested" then
      if body.provider ~= name then return end
      body = { kind = prefix .. "models.list_requested" }
    elseif k == "chat.model.set" then
      if body.provider ~= name then return end
      local model = body.model
      local active_chat_id
      local current_state = al()._internals and al()._internals.state.current_state
      if type(current_state) == "table" and type(current_state.chat_id) == "string" then
        active_chat_id = current_state.chat_id
      end
      body = {
        kind    = prefix .. "model.set",
        model   = model,
        chat_id = active_chat_id,
      }
    end

    nefor.engine.deliver(name, json.encode({
      type = env.type,
      from = "engine",
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  -- ----------------------------------------------------------------
  -- to_plugin callback: batched. Iterates envs, applies translation +
  -- replay rebuild per envelope. Framework ignores return value.
  -- ----------------------------------------------------------------
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      handle_outbound(env)
    end
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
