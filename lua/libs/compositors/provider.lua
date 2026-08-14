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
-- Replay is inert at this boundary. Live invocations carry only correlation
-- and provider options; this actor reads the manager-owned conversation view
-- and delivers the expanded native request directly to its process.

local M = {}
local json_data = require("core.json_data")
local error_value = require("core.error")

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
--
--   conversations (required) — conversation-manager's runtime read facet.
--     context(conversation_id) supplies provider-neutral history and
--     watermark(conversation_id) optionally verifies invoke ordering. The
--     expanded context is delivered only to this provider process.
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
  local conversations = opts.conversations
  if type(conversations) ~= "table" or type(conversations.context) ~= "function" then
    error("provider.spawn_spec: opts.conversations read facet is required")
  end

  local provider_lib = require(opts.translator_lib or "openai-provider")
  local translator = provider_lib.translator(name)
  local kinds = translator.kinds
  local pending_compactions = {}
  local pending_requests = {}

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

  local clone_table = json_data.copy

  local function read_context(conversation_id)
    local ok, context = pcall(conversations.context, conversations, conversation_id)
    if not ok or type(context) ~= "table" then return nil end
    return context
  end

  local function read_watermark(conversation_id, context)
    if type(conversations.watermark) == "function" then
      local ok, watermark = pcall(conversations.watermark, conversations, conversation_id)
      if ok and type(watermark) == "number" then return watermark end
    end
    return type(context) == "table" and context.watermark or nil
  end

  local function report(body)
    local reported = clone_table(body or {})
    reported.kind = "conversation.provider.event.reported"
    reported.provider = name
    emit_synthetic(name, reported)
  end

  local function publish_context_usage(body)
    if type(body) ~= "table" or body.event ~= "usage"
        or type(body.context_input_tokens) ~= "number" then return end
    emit_synthetic(name, {
      kind = "conversation.provider.context_usage",
      provider = name,
      request_id = body.request_id,
      conversation_id = pending_requests[body.request_id]
        and pending_requests[body.request_id].conversation_id or nil,
      context_input_tokens = body.context_input_tokens,
      accuracy = body.context_input_accuracy or "authoritative",
    })
  end

  local function fail_request(body, code, message)
    report({
      request_id = body and body.request_id,
      conversation_id = body and body.conversation_id,
      event = "failed",
      error = { code = code, message = message },
      message = message,
    })
  end

  local function begin_request(body)
    if body.provider ~= name then return false end
    if type(body.request_id) ~= "string" or body.request_id == ""
        or type(body.conversation_id) ~= "string" or body.conversation_id == "" then
      fail_request(body, "invalid_provider_invocation", "provider invocation needs request_id and conversation_id")
      return true
    end
    if pending_requests[body.request_id] then
      fail_request(body, "duplicate_provider_invocation", "provider request is already in flight")
      return true
    end

    local context = read_context(body.conversation_id)
    if not context then
      fail_request(body, "conversation_not_found", "conversation context is unavailable")
      return true
    end
    local watermark = read_watermark(body.conversation_id, context)
    if type(body.watermark) == "number"
        and (type(watermark) ~= "number" or watermark < body.watermark) then
      fail_request(body, "conversation_watermark_stale", "conversation context has not reached the invocation watermark")
      return true
    end

    local request = {
      request_id = body.request_id,
      conversation_id = body.conversation_id,
      model = body.model,
      reasoning_effort = body.reasoning_effort,
      tools = clone_table(body.tools),
      output_schema = clone_table(body.output_schema),
      max_corrections = body.max_corrections,
      invocation = clone_table(body.invocation),
    }
    local ok, native = pcall(translator.complete, request, context)
    if not ok or type(native) ~= "table" then
      fail_request(body, "provider_request_lowering_failed", ok and "provider returned no request" or tostring(native))
      return true
    end

    pending_requests[body.request_id] = { conversation_id = body.conversation_id }
    local delivered, delivery_error = pcall(translator.deliver, native)
    if not delivered then
      pending_requests[body.request_id] = nil
      fail_request(body, "provider_request_delivery_failed", tostring(delivery_error))
    end
    return true
  end

  local function cancel_request(body)
    if body.provider ~= nil and body.provider ~= name then return false end
    local request_id = body.request_id
    local pending = type(request_id) == "string" and pending_requests[request_id] or nil
    if not pending then return body.provider == name end
    pending_requests[request_id] = nil
    local ok, native = pcall(translator.cancel_completion, request_id)
    if ok and type(native) == "table" then translator.deliver(native) end
    return true
  end

  local function publish_compaction_failed(change, failure)
    emit_synthetic(name, {
      kind = "conversation.context.compact.failed",
      request_id = change.compaction and change.compaction.request_id,
      conversation_id = change.conversation_id,
      error = error_value.normalize(failure, "provider_compaction_failed",
        "context compaction failed"),
    })
  end

  local function begin_compaction(change)
    local selected_provider = change.provider
      or (type(change.compaction) == "table" and change.compaction.provider)
    if selected_provider ~= name then return true end
    local context = read_context(change.conversation_id)
    if not context then
      publish_compaction_failed(change, "conversation context is unavailable")
      return true
    end
    local lowered = clone_table(change)
    lowered.context = context
    local lowered_ok, plan, message = pcall(translator.compact_context, lowered)
    if not lowered_ok or not plan then
      publish_compaction_failed(change, lowered_ok
        and (message or "context compaction is unsupported") or plan)
      return true
    end
    pending_compactions[plan.chat_id] = {
      request_id = change.compaction.request_id,
      conversation_id = change.conversation_id,
      delete = plan.delete,
    }
    local delivered, delivery_error = pcall(function()
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
    end)
    if not delivered then
      pending_compactions[plan.chat_id] = nil
      publish_compaction_failed(change, delivery_error)
    end
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
        error = error_value.normalize(body.error or body.message,
          "provider_compaction_failed", "context compaction failed"),
      })
    end
    return true
  end

  -- from_plugin (binary → bus) — four steps per envelope:
  --   1. translator.maybe_inject_static_token: bus-quiet auth.set
  --      injection on first ready (no-op otherwise).
  --   2. translator.outbound: kind rename, or nil for ready/goodbye.
  --   3. publish via translator.publish (preserves env.from).
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

      local translated = translator.outbound(env)
      if type(translated) == "table" and translated.kind == kinds.completion_event then
        local request_id = translated.request_id
        if type(request_id) == "string" and pending_requests[request_id] then
          translated.kind = nil
          publish_context_usage(translated)
          report(translated)
          if translated.event == "completed" or translated.event == "result"
              or translated.event == "failed" or translated.event == "error"
              or translated.event == "interrupted" then
            pending_requests[request_id] = nil
          end
        end
        goto continue
      end

      if translated ~= nil then
        translator.publish(env.from or name, translated)
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
        if type(env.body) == "table" and env.body.kind == "conversation.provider.invoke" then
          begin_request(env.body)
          goto continue
        end
        if type(env.body) == "table" and env.body.kind == "conversation.provider.cancel" then
          cancel_request(env.body)
          goto continue
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
    _internals = {
      pending_requests = function() return pending_requests end,
    },
  }
end

return M
