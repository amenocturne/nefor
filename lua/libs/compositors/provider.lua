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
-- Replay never reissues provider work. The compositor may fold its own durable,
-- provider-namespaced usage contributions while replay envelopes remain hidden
-- from the provider process. Live invocations carry only correlation and provider
-- options; this actor reads the manager-owned conversation view and delivers the
-- expanded native request directly to its process.

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
  local usage_config = opts.usage
  local usage_values = {}
  local usage_value_versions = {}
  local latest_subscription_snapshot = nil
  local pending_usage_exposure = nil
  local usage_queries = {}
  local usage_subscriptions = {}
  local usage_contributions = {}
  local poisoned_usage = {}
  local contribution_rules_by_kind = {}
  local cancelled_requests = {}
  local contribution_totals = {}
  local request_usage_versions = {}
  local session_generation = 0
  -- A model selection this provider has been asked to adopt but has not
  -- acknowledged yet. The binary reports a refusal as an untargeted turn error;
  -- correlating it here is the only place that knows a selection was in flight,
  -- and turns an anonymous system message into a settled selection failure.
  local pending_model_set = nil

  local function emit_synthetic(from, body)
    nefor.engine.send(nefor.json.encode({
      type = "event",
      from = from,
      ts   = nefor.engine.now(),
      body = body,
    }))
  end

  local valid_usage_kinds = { subscription = true, monetary = true, free = true, unknown = true }

  local function copy_usage(value)
    if type(value) ~= "table" or not valid_usage_kinds[value.kind] then
      return { kind = "unknown" }
    end
    return json_data.copy(value)
  end

  local function usage_ids()
    local ids, seen = {}, {}
    for _, descriptor in ipairs((usage_config and usage_config.exposures) or {}) do
      if type(descriptor.usage_id) ~= "string"
          or descriptor.usage_id:sub(1, #name + 1) ~= name .. "/" then
        error("provider.spawn_spec: usage exposure must be namespaced by provider")
      end
      if seen[descriptor.usage_id] then
        error("provider.spawn_spec: duplicate usage exposure")
      end
      seen[descriptor.usage_id] = true
      ids[#ids + 1] = descriptor.usage_id
      if usage_values[descriptor.usage_id] == nil then
        usage_values[descriptor.usage_id] = copy_usage(descriptor.initial)
      end
    end
    table.sort(ids)
    return ids
  end

  local function publish_usage_values(subscription_id, values)
    if #values == 0 then return end
    emit_synthetic(name, {
      kind = "conversation.usage.update.reported",
      subscription_id = subscription_id,
      values = values,
    })
  end

  local function notify_usage(usage_id)
    local subscription_ids = {}
    for subscription_id in pairs(usage_subscriptions) do
      subscription_ids[#subscription_ids + 1] = subscription_id
    end
    table.sort(subscription_ids)
    for _, subscription_id in ipairs(subscription_ids) do
      local wanted = usage_subscriptions[subscription_id]
      if wanted[usage_id] then
        publish_usage_values(subscription_id, {
          { usage_id = usage_id, usage = copy_usage(usage_values[usage_id]) },
        })
      end
    end
  end

  local suppress_usage_notifications = false

  local function set_usage(usage_id, usage)
    usage_values[usage_id] = copy_usage(usage)
    usage_value_versions[usage_id] = (usage_value_versions[usage_id] or 0) + 1
    if not suppress_usage_notifications then notify_usage(usage_id) end
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
    if type(body) ~= "table" or body.event ~= "usage" then return end
    local usage = type(body.usage) == "table" and body.usage or {}
    local context_input_tokens = body.context_input_tokens
      or usage.context_input_tokens or usage.input_tokens or usage.prompt_tokens
    if type(context_input_tokens) ~= "number" then return end
    emit_synthetic(name, {
      kind = "conversation.provider.context_usage",
      provider = name,
      request_id = body.request_id,
      conversation_id = pending_requests[body.request_id]
        and pending_requests[body.request_id].conversation_id or nil,
      context_input_tokens = context_input_tokens,
      accuracy = body.context_input_accuracy or usage.context_input_accuracy or "authoritative",
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
      request_additions = clone_table(opts.request_additions),
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

    pending_requests[body.request_id] = {
      conversation_id = body.conversation_id,
      session_generation = session_generation,
    }
    request_usage_versions[body.request_id] = {}
    for _, rule in ipairs((usage_config and usage_config.contributions) or {}) do
      request_usage_versions[body.request_id][rule.usage_id] = usage_value_versions[rule.usage_id] or 0
    end
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
    cancelled_requests[request_id] = session_generation
    request_usage_versions[request_id] = nil
    local ok, native = pcall(translator.cancel_completion, request_id)
    if ok and type(native) == "table" then translator.deliver(native) end
    return true
  end

  local function canonical_decimal(value)
    local text, allow_exponent
    if type(value) == "number" then
      if value ~= value or value < 0 or value == math.huge then return nil end
      text, allow_exponent = tostring(value), true
    elseif type(value) == "string" then
      text = value
    else
      return nil
    end
    local mantissa, exponent = text:match("^(%d+%.?%d*)[eE]([%+%-]?%d+)$")
    if mantissa and not allow_exponent then return nil end
    if mantissa then
      local whole, fraction = mantissa:match("^(%d+)%.?(%d*)$")
      local digits = whole .. fraction
      local point = #whole + tonumber(exponent)
      if point <= 0 then
        text = "0." .. string.rep("0", -point) .. digits
      elseif point >= #digits then
        text = digits .. string.rep("0", point - #digits)
      else
        text = digits:sub(1, point) .. "." .. digits:sub(point + 1)
      end
    end
    if not text:match("^%d+%.?%d*$") then return nil end
    local whole, fraction = text:match("^(%d+)%.?(%d*)$")
    whole = whole:gsub("^0+", "")
    if whole == "" then whole = "0" end
    fraction = fraction:gsub("0+$", "")
    return fraction == "" and whole or (whole .. "." .. fraction)
  end

  local function decimal_add(left, right)
    left, right = canonical_decimal(left), canonical_decimal(right)
    if not left or not right then return nil end
    local function parts(value)
      local whole, fraction = value:match("^(%d+)%.?(%d*)$")
      return whole, fraction
    end
    local lw, lf = parts(left)
    local rw, rf = parts(right)
    local scale = math.max(#lf, #rf)
    lf, rf = lf .. string.rep("0", scale - #lf), rf .. string.rep("0", scale - #rf)
    local width = math.max(#lw, #rw)
    lw, rw = string.rep("0", width - #lw) .. lw, string.rep("0", width - #rw) .. rw
    local digits, carry = {}, 0
    local ldigits, rdigits = lw .. lf, rw .. rf
    for index = #ldigits, 1, -1 do
      local sum = tonumber(ldigits:sub(index, index)) + tonumber(rdigits:sub(index, index)) + carry
      digits[#digits + 1], carry = tostring(sum % 10), math.floor(sum / 10)
    end
    if carry > 0 then digits[#digits + 1] = tostring(carry) end
    local reversed = {}
    for index = #digits, 1, -1 do reversed[#reversed + 1] = digits[index] end
    local result = table.concat(reversed)
    if scale > 0 then
      if #result <= scale then result = string.rep("0", scale + 1 - #result) .. result end
      result = result:sub(1, -scale - 1) .. "." .. result:sub(-scale)
    end
    return canonical_decimal(result)
  end

  local function extract_subscription(rule, snapshot)
    if type(rule.extract) ~= "function" then return { kind = "unknown" } end
    local ok, usage = pcall(rule.extract, clone_table(snapshot))
    return ok and copy_usage(usage) or { kind = "unknown", reason = "usage_extraction_failed" }
  end

  local function process_native_usage(body)
    local rule = usage_config and usage_config.subscription
    if type(rule) ~= "table" or type(body) ~= "table" then return false end
    if body.kind == rule.updated_kind then
      local snapshot = clone_table(body)
      snapshot.kind = nil
      latest_subscription_snapshot = snapshot
      set_usage(rule.usage_id, extract_subscription(rule, snapshot))
      for request_id, query in pairs(usage_queries) do
        if query[rule.usage_id] then
          query.values[#query.values + 1] = { usage_id = rule.usage_id,
            usage = copy_usage(usage_values[rule.usage_id]) }
          emit_synthetic(name, { kind = "conversation.usage.snapshot.reported",
            request_id = request_id, values = query.values })
          usage_queries[request_id] = nil
        end
      end
      return true
    end
    if body.kind == rule.error_kind then
      local unknown = { kind = "unknown", reason = body.message or "provider_error" }
      latest_subscription_snapshot = nil
      set_usage(rule.usage_id, unknown)
      for request_id, query in pairs(usage_queries) do
        if query[rule.usage_id] then
          query.values[#query.values + 1] = { usage_id = rule.usage_id,
            usage = copy_usage(unknown) }
          emit_synthetic(name, { kind = "conversation.usage.snapshot.reported",
            request_id = request_id, values = query.values })
          usage_queries[request_id] = nil
        end
      end
      return true
    end
    return false
  end

  local function request_native_usage()
    local rule = usage_config and usage_config.subscription
    if type(rule) == "table" and type(rule.request_kind) == "string" then
      translator.deliver({ kind = rule.request_kind })
    end
  end

  local function poison_usage(usage_id, reason)
    if poisoned_usage[usage_id] == nil then poisoned_usage[usage_id] = reason end
    set_usage(usage_id, { kind = "unknown", reason = poisoned_usage[usage_id] })
  end

  local function rule_key(rule)
    return rule.event_kind .. "|" .. rule.usage_id
  end

  local function fold_recorded_contribution(body)
    if type(body) ~= "table" then return false end
    local rule = contribution_rules_by_kind[body.kind]
    if rule == nil then return false end
    local usage_id = rule.usage_id
    local amount = canonical_decimal(body.amount)
    if body.usage_id ~= usage_id
        or type(body.contribution_id) ~= "string" or body.contribution_id == ""
        or body.currency ~= rule.currency or amount == nil then
      poison_usage(usage_id, "malformed_contribution_record")
      return true
    end
    if poisoned_usage[usage_id] ~= nil then return true end
    local ledger_key = rule_key(rule) .. "|" .. body.contribution_id
    local fingerprint = amount .. "|" .. body.currency
    local prior = usage_contributions[ledger_key]
    if prior ~= nil then
      if prior ~= fingerprint then poison_usage(usage_id, "contribution_id_conflict") end
      return true
    end
    usage_contributions[ledger_key] = fingerprint
    local total = contribution_totals[usage_id]
      and decimal_add(contribution_totals[usage_id], amount) or amount
    if total == nil then
      poison_usage(usage_id, "malformed_contribution_record")
      return true
    end
    contribution_totals[usage_id] = total
    set_usage(usage_id, { kind = "monetary", amount = total, currency = rule.currency })
    return true
  end

  -- Completion identity is scoped to a contribution rule, so distinct exposed
  -- ledgers may independently account for the same completion.
  local function record_configured_usage(body)
    local usage = type(body) == "table" and body.event == "usage" and body.usage or nil
    local extensions = type(usage) == "table" and usage.extensions or nil
    for _, rule in ipairs((usage_config and usage_config.contributions) or {}) do
      local usage_id = rule.usage_id
      if usage ~= nil and poisoned_usage[usage_id] == nil then
        local amount = type(extensions) == "table"
          and canonical_decimal(extensions[rule.extension]) or nil
        if amount == nil then
          poison_usage(usage_id, "authoritative_cost_missing")
        else
          local completion_id = body.completion_id
          if type(completion_id) ~= "string" or completion_id == "" then
            poison_usage(usage_id, "missing_completion_id")
          else
            local byok_valid = true
            if rule.byok_extension ~= nil then
              local byok = extensions[rule.byok_extension]
              if byok == true then
                poison_usage(usage_id, "byok_cost_not_supported")
                byok_valid = false
              elseif byok == nil then
                poison_usage(usage_id, "byok_status_missing")
                byok_valid = false
              elseif byok ~= false then
                poison_usage(usage_id, "invalid_byok_status")
                byok_valid = false
              end
            end
            if byok_valid then
              local ledger_key = rule_key(rule) .. "|" .. completion_id
              local fingerprint = amount .. "|" .. rule.currency
              local prior = usage_contributions[ledger_key]
              if prior ~= nil then
                if prior ~= fingerprint then poison_usage(usage_id, "completion_id_conflict") end
              else
                usage_contributions[ledger_key] = fingerprint
                local total = contribution_totals[usage_id]
                  and decimal_add(contribution_totals[usage_id], amount) or amount
                if total == nil then
                  poison_usage(usage_id, "invalid_cost")
                else
                  contribution_totals[usage_id] = total
                  set_usage(usage_id,
                    { kind = "monetary", amount = total, currency = rule.currency })
                  emit_synthetic(name, {
                    kind = rule.event_kind,
                    usage_id = usage_id,
                    contribution_id = completion_id,
                    request_id = body.request_id,
                    cancelled = cancelled_requests[body.request_id] == session_generation or nil,
                    amount = amount,
                    currency = rule.currency,
                  })
                end
              end
            end
          end
        end
      end
    end
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
      if pending_usage_exposure and type(env.body) == "table"
          and env.body.kind == kinds.ready then
        emit_synthetic(name, { kind = "conversation.usage.expose",
          usage_ids = pending_usage_exposure })
        if usage_config and usage_config.subscribe then
          emit_synthetic("chat-surface", { kind = "conversation.usage.subscribe",
            subscription_id = usage_config.subscribe.subscription_id,
            usage_ids = usage_config.subscribe.usage_ids })
        end
      end

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

      if type(env.body) == "table" and process_native_usage(env.body) then
        goto continue
      end
      if fold_recorded_contribution(env.body) then goto continue end

      if hooks.intercept_inbound then
        if hooks.intercept_inbound(env, hook_helpers) == false then
          goto continue
        end
      end

      if type(env.body) == "table"
          and env.body.kind == kinds.model_set_ack
          and pending_model_set ~= nil
          and env.body.model == pending_model_set.model then
        -- A superseded model.set may acknowledge after a newer request. Only
        -- the matching ack settles the request this compositor still owns;
        -- otherwise a later rejection would lose its correlation and surface
        -- as an unrelated provider error.
        pending_model_set = nil
      elseif type(env.body) == "table"
          and env.body.kind == kinds.turn_error
          and type(env.body.chat_id) ~= "string"
          and pending_model_set ~= nil then
        local refused = pending_model_set
        pending_model_set = nil
        emit_synthetic(name, {
          kind = "chat.model.set_failed",
          provider = name,
          model = refused.model,
          error = tostring(env.body.message or env.body.error or "provider error"),
        })
        goto continue
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
        if type(request_id) == "string"
            and ((pending_requests[request_id]
              and pending_requests[request_id].session_generation == session_generation)
              or cancelled_requests[request_id] == session_generation) then
          translated.kind = nil
          publish_context_usage(translated)
          record_configured_usage(translated)
          if translated.event == "completed" or translated.event == "result" then
            for _, rule in ipairs((usage_config and usage_config.contributions) or {}) do
              local started_version = request_usage_versions[request_id]
                and request_usage_versions[request_id][rule.usage_id] or 0
              if request_usage_versions[request_id] ~= nil
                and (usage_value_versions[rule.usage_id] or 0) == started_version then
                poison_usage(rule.usage_id, "authoritative_usage_missing")
              end
            end
          end
          if pending_requests[request_id] then report(translated) end
          if translated.event == "completed" or translated.event == "result"
              or translated.event == "failed" or translated.event == "error"
              or translated.event == "interrupted" then
            pending_requests[request_id] = nil
            cancelled_requests[request_id] = nil
            request_usage_versions[request_id] = nil
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
      if env.replay then
        -- Provider calls remain inert during replay. Only this actor's durable
        -- records may reconstruct the ledger it owns.
        if env.from == name then
          suppress_usage_notifications = true
          fold_recorded_contribution(env.body)
          suppress_usage_notifications = false
        end
      elseif kind == "sessions.replay.end" then
        for _, rule in ipairs((usage_config and usage_config.contributions) or {}) do
          if contribution_totals[rule.usage_id] ~= nil then notify_usage(rule.usage_id) end
        end
      else
        if type(env.body) == "table" and env.body.kind == "sessions.session_start" then
          session_generation = session_generation + 1
          pending_requests = {}
          usage_contributions = {}
          poisoned_usage = {}
          contribution_totals = {}
          cancelled_requests = {}
          request_usage_versions = {}
          usage_queries = {}
          latest_subscription_snapshot = nil
          for _, descriptor in ipairs((usage_config and usage_config.exposures) or {}) do
            set_usage(descriptor.usage_id, descriptor.initial)
          end
        end
        if hooks.intercept_to_plugin then
          if hooks.intercept_to_plugin(env) == false then
            goto continue
          end
        end
        if type(env.body) == "table" and env.body.kind == "conversation.usage.query.forwarded"
            and env.body.owner == name then
          local values, waits_for_native = {}, {}
          local native_id = usage_config and usage_config.subscription
            and usage_config.subscription.usage_id
          for _, usage_id in ipairs(env.body.usage_ids or {}) do
            if usage_id == native_id then
              waits_for_native[usage_id] = true
            else
              values[#values + 1] = { usage_id = usage_id,
                usage = copy_usage(usage_values[usage_id]) }
            end
          end
          if next(waits_for_native) then
            if latest_subscription_snapshot then
              values[#values + 1] = { usage_id = native_id,
                usage = extract_subscription(usage_config.subscription,
                  latest_subscription_snapshot) }
              emit_synthetic(name, { kind = "conversation.usage.snapshot.reported",
                request_id = env.body.request_id, values = values })
            else
              waits_for_native.values = values
              usage_queries[env.body.request_id] = waits_for_native
              request_native_usage()
            end
          elseif #values > 0 then
            emit_synthetic(name, { kind = "conversation.usage.snapshot.reported",
              request_id = env.body.request_id, values = values })
          end
          goto continue
        end
        if type(env.body) == "table" and env.body.kind == "conversation.usage.subscribe.forwarded"
            and env.body.owner == name then
          local wanted = usage_subscriptions[env.body.subscription_id] or {}
          usage_subscriptions[env.body.subscription_id] = wanted
          local values = {}
          for _, usage_id in ipairs(env.body.usage_ids or {}) do
            wanted[usage_id] = true
            values[#values + 1] = { usage_id = usage_id,
              usage = copy_usage(usage_values[usage_id]) }
          end
          publish_usage_values(env.body.subscription_id, values)
          request_native_usage()
          goto continue
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
          if body.kind == kinds.model_set then
            pending_model_set = { model = body.model }
          end
          translator.deliver(body)
        end
      end
      ::continue::
    end
  end

  if usage_config ~= nil and type(usage_config) ~= "table" then
    error("provider.spawn_spec: opts.usage must be a table")
  end
  if usage_config and usage_config.exposures ~= nil
      and type(usage_config.exposures) ~= "table" then
    error("provider.spawn_spec: usage.exposures must be an array")
  end
  if usage_config and usage_config.contributions ~= nil
      and type(usage_config.contributions) ~= "table" then
    error("provider.spawn_spec: usage.contributions must be an array")
  end
  local exposed_ids = usage_ids()
  local subscription = usage_config and usage_config.subscription
  if subscription ~= nil then
    if type(subscription) ~= "table"
        or usage_values[subscription.usage_id] == nil
        or type(subscription.request_kind) ~= "string"
        or type(subscription.updated_kind) ~= "string"
        or type(subscription.error_kind) ~= "string"
        or type(subscription.extract) ~= "function" then
      error("provider.spawn_spec: invalid subscription usage rule")
    end
  end
  local contribution_rules_by_usage_id = {}
  for _, rule in ipairs((usage_config and usage_config.contributions) or {}) do
    if type(rule) ~= "table" or usage_values[rule.usage_id] == nil
        or type(rule.extension) ~= "string" or rule.extension == ""
        or type(rule.currency) ~= "string" or rule.currency == ""
        or (rule.byok_extension ~= nil
          and (type(rule.byok_extension) ~= "string" or rule.byok_extension == ""))
        or type(rule.event_kind) ~= "string" or rule.event_kind == "" then
      error("provider.spawn_spec: invalid monetary contribution rule")
    end
    if contribution_rules_by_kind[rule.event_kind] ~= nil then
      error("provider.spawn_spec: duplicate contribution event_kind")
    end
    if contribution_rules_by_usage_id[rule.usage_id] ~= nil then
      error("provider.spawn_spec: duplicate contribution usage_id")
    end
    contribution_rules_by_kind[rule.event_kind] = rule
    contribution_rules_by_usage_id[rule.usage_id] = rule
  end
  if #exposed_ids > 0 then pending_usage_exposure = exposed_ids end

  return {
    name        = name,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    to_plugin_readonly = true,
    receive_msg = function(_) end,
    _internals = {
      pending_requests = function() return pending_requests end,
      usage_value = function(usage_id) return copy_usage(usage_values[usage_id]) end,
      fold_recorded_contribution = fold_recorded_contribution,
      record_configured_usage = record_configured_usage,
    },
  }
end

return M
