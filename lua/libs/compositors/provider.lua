-- lua/libs/compositors/provider.lua — engine-side actor for
-- OpenAI-compatible providers. Threads the openai-provider plugin
-- lib's translation primitives with conversation-manager projections and
-- starter-owned control-plane state.
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
-- reads those). The lib returns these envelopes' bodies unchanged; this file
-- pattern-matches on the prefixed kind.
--
-- Replay is inert at this boundary: provider requests carry complete
-- conversation-manager context and never rebuild process-local chats.

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
  local pending_compactions = {}

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
    if k == kinds.chat_complete_result then
      local chat_id = body.chat_id
      if type(chat_id) ~= "string" then return body end
      return body
    end

    return body
  end

  local function publish_compaction_failed(change, message)
    emit_synthetic(name, {
      kind = "conversation.context.compact.failed",
      request_id = change.compaction and change.compaction.request_id,
      conversation_id = change.conversation_id,
      error = { code = "provider_compaction_failed", message = tostring(message) },
    })
  end

  local function begin_compaction(change)
    if change.provider ~= name then return true end
    local plan, message = translator.compact_context(change)
    if not plan then
      publish_compaction_failed(change, message or "context compaction is unsupported")
      return true
    end
    pending_compactions[plan.chat_id] = {
      request_id = change.compaction.request_id,
      conversation_id = change.conversation_id,
      delete = plan.delete,
    }
    translator.deliver(plan.create)
    if plan.restore then translator.deliver(plan.restore) end
    for _, message_body in ipairs(plan.messages or {}) do
      translator.deliver({
        kind = kinds.chat_append,
        chat_id = plan.chat_id,
        message = clone_table(message_body),
      })
    end
    translator.deliver(plan.compact)
    return true
  end

  local function finish_compaction(body)
    local pending = type(body.chat_id) == "string" and pending_compactions[body.chat_id] or nil
    if not pending then return false end
    pending_compactions[body.chat_id] = nil
    if pending.delete then translator.deliver(pending.delete) end
    if body.kind == kinds.chat_compaction_commit then
      local checkpoint = {
        provider = name,
        format = "chatgpt.responses.compaction.v1",
        model = body.model,
        artifact = clone_table(body.model_context_artifact),
      }
      emit_synthetic(name, {
        kind = "conversation.context.compact.complete",
        request_id = pending.request_id,
        conversation_id = pending.conversation_id,
        checkpoint = checkpoint,
        compatibility = {
          provider = name,
          format = checkpoint.format,
          model = checkpoint.model,
        },
      })
    else
      emit_synthetic(name, {
        kind = "conversation.context.compact.failed",
        request_id = pending.request_id,
        conversation_id = pending.conversation_id,
        error = {
          code = "provider_compaction_failed",
          message = tostring(body.message or body.error or "context compaction failed"),
        },
      })
    end
    return true
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

      if type(env.body) == "table" then
        local body = env.body
        if pending_compactions[body.chat_id]
            and (body.kind == kinds.chat_compaction_commit
              or body.kind == kinds.chat_error
              or body.kind == kinds.turn_error) then
          finish_compaction(body)
          goto continue
        end
        if pending_compactions[body.chat_id] then goto continue end
      end

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
  --   1. env.replay: drop; conversation-manager reconstructs durable context.
  --   2. translator.inbound: kind rename, drop UI-shaped prompts,
  --      target-filter provider-scoped envelopes.
  --   3. translator.deliver to the peer's stdin.
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      local kind = type(env.body) == "table" and env.body.kind or nil
      if env.replay or kind == "sessions.replay.end" then
        -- Provider processes are request-scoped. Conversation-manager
        -- reconstructs universal context, so replay never rebuilds provider
        -- chat tables or reissues completed requests.
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

        if type(env.body) == "table"
            and env.body.kind == "conversation.projection.delta"
            and type(env.body.change) == "table"
            and env.body.change.kind == "context_compaction_pending" then
          local change = clone_table(env.body.change)
          change.conversation_id = env.body.conversation_id
          begin_compaction(change)
          goto continue
        end

        local body = translator.inbound(env)
        if body ~= nil then
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
