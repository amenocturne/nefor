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
--   <prefix>.hello                 → forwarded + chat.model.set_ack when model is present
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
local tool = require("libs.tool")

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

  -- The gate's public tool registry by tool name, kept for the per-policy
  -- text a tool adds to its description (`policy_descriptions`).
  local registered_tools = {}
  local function request_tool_specs(body)
    local specs = tool.request_tool_specs(body.tools, registered_tools,
      json_data.copy(body.tool_specs), body.approval_policy)
    if body.tool_specs == nil and #specs == 0 then return nil end
    if type(nefor.json.mark_array) == "function" then nefor.json.mark_array(specs) end
    return specs
  end

  local provider_lib = require(opts.translator_lib or "openai-provider")
  local tool_gate = opts.tool_gate
  local translator = provider_lib.translator(name, { tool_gate = tool_gate })
  -- Provider-owned tools are a `libs.tool` source built by the translator.
  local provider_tools = translator.tool_source
  if provider_tools ~= nil and (type(tool_gate) ~= "string" or tool_gate == "") then
    error("provider.spawn_spec: opts.tool_gate is required for provider-owned tools")
  end
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

  local response_usage = require("libs.chat.context_usage")

  local function publish_context_usage(body)
    local pending = pending_requests[body.request_id]
    if not pending then return end
    if body.event ~= "usage" and body.event ~= "completed" and body.event ~= "result" then return end
    if body.event == "usage" and body.latest_response_usage == nil
        and body.billing_components == nil then
      -- Generic usage may arrive before the response outcome is known. Stage
      -- it under this request; failure drops it without publishing occupancy.
      pending.response_usage = response_usage.from_response(body.usage or body)
    end
    local measured = response_usage.measurement(body.latest_response_usage)
    if not measured and (body.event == "completed" or body.event == "result")
        and body.billing_components == nil then
      measured = response_usage.from_response(body.usage
        or (type(body.result) == "table" and (body.result.usage or body.result)) or body)
        or pending.response_usage
    end
    if not measured then return end
    body.latest_response_usage = measured
    body.model = body.model or pending.model
    emit_synthetic(name, {
      kind = "conversation.provider.context_usage", provider = name,
      request_id = body.request_id, conversation_id = pending.conversation_id,
      model = body.model, latest_response_usage = measured,
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

    -- Every provider request is an agent's and names that agent's approval
    -- policy; nothing is assumed for one that does not.
    if not tool.approval.is_policy(body.approval_policy) then
      fail_request(body, "invalid_provider_invocation",
        "provider invocation names no approval policy (got `" .. tostring(body.approval_policy) .. "`)")
      return true
    end

    local request = {
      request_id = body.request_id,
      conversation_id = body.conversation_id,
      request_additions = clone_table(opts.request_additions),
      model = body.model,
      reasoning_effort = body.reasoning_effort,
      provider_options = clone_table(body.provider_options),
      tools = clone_table(body.tools),
      output_schema = clone_table(body.output_schema),
      -- The one place a request's tool definitions meet the agent's
      -- approval policy: tools with text for that policy get it appended.
      tool_specs = request_tool_specs(body),
      invocation = clone_table(body.invocation),
    }
    local ok, native = pcall(translator.complete, request, context)
    if not ok or type(native) ~= "table" then
      fail_request(body, "provider_request_lowering_failed", ok and "provider returned no request" or tostring(native))
      return true
    end

    if body.notice_id ~= nil then
      local notice = context.policy_state and context.policy_state.notices
        and context.policy_state.notices[body.notice_id]
      local text
      for _, message in ipairs(context.messages or {}) do
        if notice and message.id == notice.message_id then
          local chunks = {}
          for _, chunk in ipairs(message.chunks or {}) do
            if chunk.kind == "text" then chunks[#chunks + 1] = chunk.data end
          end
          text = table.concat(chunks)
        end
      end
      local function contains(value)
        if type(value) == "string" then return text and text ~= "" and value:find(text, 1, true) ~= nil end
        if type(value) == "table" then
          for _, item in pairs(value) do if contains(item) then return true end end
        end
        return false
      end
      local submitted = native.conversation_context
      local submitted_messages = submitted and (submitted.compaction and submitted.tail_messages or submitted.messages)
        or native.messages
      if not notice or notice.epoch_id ~= body.epoch_id or not contains(submitted_messages) then
        fail_request(body, "notice_not_in_submitted_context", "Canonical notice is absent from submitted model context")
        return true
      end
    end

    pending_requests[body.request_id] = {
      conversation_id = body.conversation_id,
      session_generation = session_generation,
      model = body.model,
    }
    request_usage_versions[body.request_id] = {}
    for _, rule in ipairs((usage_config and usage_config.contributions) or {}) do
      request_usage_versions[body.request_id][rule.usage_id] = usage_value_versions[rule.usage_id] or 0
    end
    if body.notice_id ~= nil then
      emit_synthetic(name, { kind = "conversation.provider.context_included", provider = name,
        request_id = body.request_id, conversation_id = body.conversation_id, model = body.model,
        context_watermark = context.watermark, history_id = clone_table(context.history_id),
        notice_id = body.notice_id, epoch_id = body.epoch_id })
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

  local settled_compactions, cleanup_resources = {}, {}
  local function adapter_capability(provider, model, options)
    local ok, capability = pcall(translator.compaction_capability, provider, model, options)
    if not ok then
      return nil, error_value.normalize(capability, "compaction_capability_failed", "Adapter capability evaluation failed")
    end
    if type(capability) ~= "table" or (capability.kind ~= "supported"
        and capability.kind ~= "unsupported" and capability.kind ~= "unknown") then
      return nil, { code = "compaction_capability_invalid", message = "Adapter returned an invalid capability" }
    end
    return capability
  end
  local function cleanup_failed(pending, failure)
    cleanup_resources[pending.plan.chat_id] = nil
    emit_synthetic(name, { kind = "conversation.context.compact.cleanup_failed",
      request_id = pending.request.request_id, conversation_id = pending.request.conversation_id,
      resource_id = pending.plan.chat_id,
      error = error_value.normalize(failure, "compaction_cleanup_failed",
        "Temporary compaction resource could not be released") })
  end
  local function publish_compaction_failed(request, failure)
    local normalized = error_value.normalize(failure, "provider_compaction_failed", "context compaction failed")
    if type(failure) == "table" then normalized.outcome = failure.outcome end
    emit_synthetic(name, {
      kind = "conversation.context.compact.failed",
      request_id = request.request_id, conversation_id = request.conversation_id,
      epoch_id = request.epoch_id, history_id = clone_table(request.history_id),
      provider = request.provider, model = request.model,
      error = normalized,
    })
  end

  local function release_compaction(pending, cancel)
    pending.cancelled = cancel
    cleanup_resources[pending.plan.chat_id] = pending
    local ok, failure = pcall(function()
      if cancel then
        for _, body in ipairs(translator.cancel_compaction(pending.plan, pending.request.request_id) or {}) do
          translator.deliver(body)
        end
      end
    end)
    local cleaned, cleanup_error = pcall(function()
      for _, body in ipairs(translator.cleanup_compaction(pending.plan, pending.request.request_id) or {}) do
        translator.deliver(body)
      end
    end)
    if not ok or not cleaned then
      cleanup_failed(pending, not cleaned and cleanup_error or failure)
    end
  end

  local function settle_failed(chat_id, failure, cancel)
    local pending = pending_compactions[chat_id]
    if not pending then return end
    pending_compactions[chat_id] = nil
    settled_compactions[pending.request.request_id] = true
    publish_compaction_failed(pending.request, failure)
    release_compaction(pending, cancel)
  end

  local function begin_compaction(change)
    -- The manager supplies an accepted immutable prefix. Never read current
    -- history here: sibling tool results may already have extended the tail.
    local input = change.input or { request = change.request or change.compaction, context = change.context }
    local request = clone_table(input.request or {})
    request.conversation_id = request.conversation_id or change.conversation_id
    request.provider = request.provider or change.provider
    request.model = request.model or change.model
    if request.provider ~= name then return true end
    if settled_compactions[request.request_id] then return true end
    for _, pending in pairs(pending_compactions) do
      if pending.request.request_id == request.request_id then return true end
    end
    local capability, capability_error = adapter_capability(request.provider, request.model, request.provider_options)
    if not capability then
      settled_compactions[request.request_id] = true
      publish_compaction_failed(request, capability_error)
      return true
    end
    if capability.kind ~= "supported" then
      settled_compactions[request.request_id] = true
      publish_compaction_failed(request, { code = capability.kind == "unsupported" and "compaction_unsupported" or "compaction_capability_unknown", message = capability.reason })
      return true
    end
    if type(input.context) ~= "table" then
      settled_compactions[request.request_id] = true
      publish_compaction_failed(request, { code = "compaction_context_missing",
        message = "Accepted compaction requires frozen canonical context" })
      return true
    end
    local lowered_ok, plan, failure = pcall(translator.compact_context,
      { request = request, context = clone_table(input.context) })
    if not lowered_ok or not plan then
      settled_compactions[request.request_id] = true
      publish_compaction_failed(request, lowered_ok and failure or plan)
      return true
    end
    if type(plan.chat_id) ~= "string" or plan.chat_id == "" or pending_compactions[plan.chat_id] then
      settled_compactions[request.request_id] = true
      publish_compaction_failed(request, { code = "compaction_plan_invalid", message = "Temporary chat identity is invalid" })
      return true
    end
    pending_compactions[plan.chat_id] = { request = request, plan = plan,
      request_id = request.request_id, conversation_id = request.conversation_id }
    local delivered, delivery_error = pcall(function()
      translator.deliver(plan.create)
      if plan.restore then translator.deliver(plan.restore) end
      for _, message in ipairs(plan.messages or {}) do
        translator.deliver({ kind = kinds.chat_append, chat_id = plan.chat_id, message = clone_table(message) })
      end
      translator.deliver(plan.compact)
    end)
    if not delivered then settle_failed(plan.chat_id, delivery_error, true) end
    return true
  end

  local function finish_compaction(body)
    local pending = type(body.chat_id) == "string" and pending_compactions[body.chat_id] or nil
    if not pending then return false end
    -- Native wire uses a unique temporary chat identity, not a request ID.
    if body.request_id ~= nil and body.request_id ~= pending.request.request_id then return false end
    local ok, outcome = pcall(translator.compaction_outcome, clone_table(body), pending.request)
    if not ok then settle_failed(body.chat_id, outcome, false); return true end
    if type(outcome) ~= "table" or outcome.request_id ~= pending.request.request_id
        or outcome.conversation_id ~= pending.request.conversation_id then
      settle_failed(body.chat_id, { code = "compaction_outcome_invalid", message = "Adapter outcome correlation mismatch" }, false)
      return true
    end
    if outcome.kind ~= "completed" then
      settle_failed(body.chat_id, outcome.error, false)
      return true
    end
    local checkpoint, compatibility = outcome.checkpoint, outcome.compatibility
    local compatible_ok, compatible = pcall(function()
      return type(compatibility) == "table" and type(checkpoint) == "table"
        and compatibility.provider == pending.request.provider and compatibility.model == pending.request.model
        and checkpoint.provider == pending.request.provider and checkpoint.model == pending.request.model
        and type(checkpoint.format) == "string" and checkpoint.format ~= ""
        and compatibility.format == checkpoint.format
        and translator.checkpoint_compatible(checkpoint, pending.request.provider,
          pending.request.model, pending.request.provider_options)
    end)
    if not compatible_ok or not compatible then
      settle_failed(body.chat_id, { code = "compaction_checkpoint_invalid", message = "Adapter checkpoint is incompatible" }, false)
      return true
    end
    pending_compactions[body.chat_id] = nil
    settled_compactions[pending.request.request_id] = true
    outcome.epoch_id = pending.request.epoch_id
    outcome.history_id = clone_table(pending.request.history_id)
    outcome.provider, outcome.model = pending.request.provider, pending.request.model
    outcome.kind = "conversation.context.compact.complete"
    emit_synthetic(name, outcome)
    -- Canonical settlement is emitted first; resource cleanup cannot turn a
    -- completed checkpoint into failure or hide it behind a delivery exception.
    release_compaction(pending, false)
    return true
  end

  local catalog, catalog_known, preparation_waiters = {}, false, {}
  local preparation_serial = 0
  local function prepare_compaction(input)
    if input.provider ~= name then
      return { kind = "Error", error = { code = "compaction_provider_mismatch", message = "Provider does not match adapter" } }
    end
    local capability, failure = adapter_capability(name, input.model, input.provider_options)
    if not capability then return { kind = "Error", error = failure } end
    local enabled = input.policy ~= nil and input.policy ~= nefor.json.null
      and not (type(input.policy) == "table" and input.policy.kind == "Disabled")
    if enabled and capability.kind ~= "supported" then
      return { kind = "Error", error = { code = capability.kind == "unsupported" and "compaction_unsupported" or "compaction_capability_unknown", message = capability.reason } }
    end
    local window = catalog[input.model]
    if not enabled or window ~= nil then
      return { kind = "Ready", regular_window = window, capability = capability }
    end
    if catalog_known then
      return { kind = "Error", error = { code = "compaction_capacity_missing",
        message = "Regular context window is unavailable for selected model" } }
    end
    local key = input.request_id or input.model
    if not preparation_waiters[key] then
      preparation_serial = preparation_serial + 1
      preparation_waiters[key] = { request_id = input.request_id or (name .. ":compaction-prepare:" .. preparation_serial),
        input = clone_table(input) }
      local first = true
      for waiter_key in pairs(preparation_waiters) do if waiter_key ~= key then first = false end end
      local ok, failure = true, nil
      if first then ok, failure = pcall(translator.deliver, { kind = kinds.models_list_requested }) end
      if not ok then
        preparation_waiters[key] = nil
        return { kind = "Error", error = error_value.normalize(failure, "compaction_catalog_failed", "Catalog delivery failed") }
      end
    end
    return { kind = "Waiting", request_id = preparation_waiters[key].request_id }
  end

  local function fail_preparation(failure)
    local waiting = preparation_waiters
    preparation_waiters = {}
    for _, waiter in pairs(waiting) do
      emit_synthetic(name, { kind = "conversation.compaction.prepared", provider = name,
        request_id = waiter.request_id, outcome = { kind = "Error",
          error = error_value.normalize(failure, "compaction_catalog_failed", "Catalog preparation failed") } })
    end
  end

  local function observe_catalog(body)
    catalog, catalog_known = {}, true
    for model, window in pairs(body.context_windows or {}) do
      if type(window) == "number" and window > 0 and window < math.huge then catalog[model] = window end
    end
    local capabilities = {}
    for _, model in ipairs(body.models or {}) do
      local capability, failure = adapter_capability(name, model)
      capabilities[model] = capability or { kind = "Error", error = failure }
    end
    emit_synthetic(name, { kind = "conversation.compaction.catalog", provider = name,
      context_windows = clone_table(catalog), capabilities = capabilities })
    local waiting = preparation_waiters
    preparation_waiters = {}
    for _, waiter in pairs(waiting) do
      emit_synthetic(name, { kind = "conversation.compaction.prepared", provider = name,
        request_id = waiter.request_id, outcome = prepare_compaction(waiter.input) })
    end
  end

  -- from_plugin (binary → bus) — provider lifecycle/usage handling followed
  -- by translator-owned public event projection.
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
        if body.kind == kinds.models_listed then observe_catalog(body) end
        if body.kind == kinds.turn_error and body.chat_id == nil and pending_model_set == nil then
          fail_preparation(body.error or body.message)
        end
        if cleanup_resources[body.chat_id] then
          if body.kind == name .. ".chat.deleted" then
            cleanup_resources[body.chat_id] = nil
            goto continue
          elseif body.kind == kinds.chat_error or body.kind == kinds.turn_error then
            -- A cancelled compactor may still emit its interrupted terminal
            -- while delete is in flight. That is not a cleanup rejection.
            if not cleanup_resources[body.chat_id].cancelled then
              cleanup_failed(cleanup_resources[body.chat_id], body.error or body.message)
            end
            goto continue
          end
        end
        if body.kind == kinds.chat_compaction_commit then
          finish_compaction(body)
          goto continue
        end
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

      if type(env.body) == "table" and env.body.kind == kinds.model_set_ack
          and (pending_model_set == nil or env.body.model ~= pending_model_set.model) then goto continue end

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
      -- Hello is process liveness, not a model-selection event. Preserve the
      -- provider-owned announcement even when the translator cannot derive a
      -- public acknowledgement from it (for example ChatGPT's model-less
      -- hello while its catalog is fetched asynchronously).
      if type(env.body) == "table" and env.body.kind == kinds.hello then
        translator.publish(env.from or name, clone_table(env.body))
      end
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
      -- The gate's public registry: kept for the per-policy tool descriptions.
      if not env.replay and type(env.body) == "table" and env.body.kind == "tool.register"
          and type(env.body.tools) == "table" then
        registered_tools = {}
        for _, spec in ipairs(env.body.tools) do
          if type(spec) == "table" and type(spec.name) == "string" then
            registered_tools[spec.name] = spec
          end
        end
      end
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
        local body = env.body
        if kind == "conversation.compaction.prepare.request" and body.provider == name then
          local outcome = prepare_compaction(body)
          if outcome.kind ~= "Waiting" then
            emit_synthetic(name, { kind = "conversation.compaction.prepared", provider = name,
              request_id = body.request_id, outcome = outcome })
          end
          goto continue
        elseif type(body) == "table" and ((kind == "ncp.delivery_failed" and body.peer == name)
            or ((kind == "engine.plugin_transport_failed" or kind == "engine.plugin_delivery_lost")
              and body.plugin == name)) then
          local ids, compactions, model, identified, incomplete = {}, {}, false, false, false
          if kind == "ncp.delivery_failed" and body.extent == "none" then
            for _, correlation in ipairs(body.correlations or {}) do
              if correlation.chat_id then
                identified = true
                if pending_compactions[correlation.chat_id] then compactions[correlation.chat_id] = true end
              end
              if correlation.kind == kinds.models_list_requested then
                identified = true
                fail_preparation({ code = "compaction_catalog_failed", message = "Catalog request delivery failed" })
              end
              if correlation.kind == kinds.model_set then
                identified = true
                incomplete = incomplete or type(correlation.model) ~= "string"
                model = pending_model_set ~= nil and correlation.model == pending_model_set.model
              end
              if correlation.kind == kinds.completion_request and type(correlation.request_id) == "string" then
                identified = true
                if pending_requests[correlation.request_id] then ids[correlation.request_id] = true end
              end
            end
          end
          for _, correlation in ipairs(body.correlations or {}) do
            local resource = cleanup_resources[correlation.chat_id]
            if resource then cleanup_failed(resource, { code = "compaction_cleanup_failed", message = "Cleanup delivery failed" }) end
          end
          local unknown = not identified or incomplete
            or (type(body.count) == "number" and body.count > #(body.correlations or {}))
          if unknown then
            fail_preparation({ code = "compaction_catalog_failed", message = "Provider transport lost during catalog preparation" })
            local resources = cleanup_resources
            cleanup_resources = {}
            for _, resource in pairs(resources) do
              cleanup_failed(resource, { code = "compaction_cleanup_failed", message = "Provider transport lost during cleanup" })
            end
            for id in pairs(pending_requests) do ids[id] = true end
            for id in pairs(pending_compactions) do compactions[id] = true end
            model = true
          end
          for id in pairs(compactions) do
            settle_failed(id, { code = "provider_delivery_failed", outcome = "unknown",
              message = "Provider compaction delivery failed; prior effects are unknown" }, true)
          end
          if model and pending_model_set then
            local pending = pending_model_set
            pending_model_set = nil
            emit_synthetic(name, { kind = "chat.model.set_failed", provider = name,
              model = pending.model, outcome = unknown and "unknown" or "rejected",
              error = "Provider model selection delivery failed" })
          end
          for id in pairs(ids) do
            local pending = pending_requests[id]
            pending_requests[id] = nil
            cancelled_requests[id] = nil
            request_usage_versions[id] = nil
            report({ request_id = id, conversation_id = pending.conversation_id, event = "error",
              error = { code = "provider_delivery_failed", outcome = unknown and "unknown" or "rejected",
                message = "Provider delivery failed; prior effects are unknown" } })
          end
          goto continue
        end
        if provider_tools ~= nil and type(env.body) == "table"
            and env.body.kind == tool_gate .. ".hello" then
          provider_tools.advertise(tool_gate)
        end
        if type(env.body) == "table" and env.body.kind == "sessions.session_start" then
          fail_preparation({ code = "compaction_session_changed", message = "Session changed during preparation" })
          local chats = {}
          for chat_id in pairs(pending_compactions) do chats[#chats + 1] = chat_id end
          for _, chat_id in ipairs(chats) do
            settle_failed(chat_id, { code = "compaction_session_changed", message = "Session changed during compaction" }, true)
          end
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
        if type(body) == "table" and (body.kind == "conversation.context.compact.cancelled"
            or (body.kind == "conversation.projection.delta" and type(body.change) == "table"
              and body.change.kind == "context_compaction_failed")) then
          local request_id = body.request_id or body.change.request_id
          for chat_id, pending in pairs(pending_compactions) do
            if pending.request.request_id == request_id and pending.request.conversation_id == body.conversation_id then
              pending_compactions[chat_id] = nil
              settled_compactions[request_id] = true
              release_compaction(pending, true)
              break
            end
          end
          goto continue
        end
        if type(body) == "table" and body.kind == "conversation.context.compact.cancel" then
          for chat_id, pending in pairs(pending_compactions) do
            if pending.request.request_id == body.request_id
                and pending.request.conversation_id == body.conversation_id then
              settle_failed(chat_id, { code = "compaction_interrupted", message = body.reason or "Compaction interrupted" }, true)
              break
            end
          end
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
    -- NCP diagnostics are deliberately barred from physical egress. Observe
    -- them locally through the actor bus instead of the suppressed wrapper.
    receive_msg = function(entry)
      local ok, env = pcall(nefor.json.decode, entry.payload or "")
      if ok and type(env) == "table" and type(env.body) == "table"
          and env.body.kind == "ncp.delivery_failed" and env.body.peer == name and not env.replay
          and not require("core.replay_window").active() then
        to_plugin({ env })
      end
    end,
    compaction = {
      prepare = prepare_compaction,
      capability = translator.compaction_capability,
      checkpoint_compatible = translator.checkpoint_compatible,
    },
    _internals = {
      pending_requests = function() return pending_requests end,
      usage_value = function(usage_id) return copy_usage(usage_values[usage_id]) end,
      fold_recorded_contribution = fold_recorded_contribution,
      record_configured_usage = record_configured_usage,
    },
  }
end

return M
