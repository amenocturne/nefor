-- Shared provider round lifecycle for provider-boundary factories.
-- Classification policy stays in the thin consumer (`llm` or
-- `structured-output`); correlation, tool calls, cancellation and drain
-- behavior live here once. Canonical conversation history is emitted as facts;
-- provider actors reconstruct their private request view from that shared state.

local kinds = require("kinds")
local conversation_facts = require("conversation-facts")
local json_data = require("core.json_data")
local policy_api = require("libs.compaction-policy")
local approval = require("libs.tool.approval")
local M = {}

local DIAGNOSTIC_LIMIT = 160

local function bounded(value)
  value = tostring(value or "")
  local length = utf8.len(value)
  if length == nil then return "<invalid UTF-8 diagnostic>" end
  if length <= DIAGNOSTIC_LIMIT then return value end
  local boundary = utf8.offset(value, DIAGNOSTIC_LIMIT + 1)
  return value:sub(1, boundary - 1) .. "…"
end

local function argument_diagnostic(arguments)
  if type(arguments) == "table" then
    if json_data.is_array(arguments) then return "arguments decoded to a JSON array" end
    return nil
  end
  if type(arguments) == "string" then
    if arguments == "" then return "function.arguments was empty" end
    local ok, decoded = pcall(nefor.json.decode, arguments)
    if not ok then
      local diagnostic = type(nefor.json.parse) == "function" and nefor.json.parse(arguments).error or nil
      return "function.arguments was malformed JSON: " .. bounded(decoded), nil, diagnostic
    end
    if type(decoded) == "table" and not json_data.is_array(decoded) then return nil, decoded end
    if type(decoded) == "table" then return "arguments decoded to a JSON array" end
    if type(decoded) == "userdata" then return "arguments decoded to JSON null" end
    return "arguments decoded to a JSON " .. type(decoded)
  end
  if type(arguments) == "userdata" then return "arguments decoded to JSON null" end
  return "arguments decoded to a JSON " .. type(arguments)
end

local function inspect_tool_calls(tool_calls)
  local calls, invalid = {}, {}
  for index, raw in ipairs(tool_calls or {}) do
    raw = raw or {}
    local fn = type(raw["function"]) == "table" and raw["function"] or {}
    local call = {
      id = raw.id,
      name = raw.name or fn.name,
      args = raw.args,
    }
    if call.args == nil then call.args = raw.arguments end
    if call.args == nil then call.args = fn.arguments end
    local reason, decoded, diagnostic = argument_diagnostic(call.args)
    if decoded ~= nil then call.args = decoded end
    calls[index] = call
    if reason then
      invalid[#invalid + 1] = {
        id = call.id or "<missing>", name = call.name or "<missing>", reason = reason,
        code = diagnostic and diagnostic.code or "invalid_argument_shape",
        line = diagnostic and diagnostic.line, column = diagnostic and diagnostic.column,
      }
    end
  end
  return calls, invalid
end

local function correction_message(invalid)
  local diagnostics = {}
  for _, item in ipairs(invalid) do
    diagnostics[#diagnostics + 1] = string.format("call id=%s name=%s: %s",
      bounded(item.id), bounded(item.name), bounded(item.reason))
  end
  return "Your attempted tool call batch was not executed because function.arguments "
    .. "must be a JSON object. " .. table.concat(diagnostics, "; ") .. ". "
    .. "Emit a new tool call with a JSON object matching the advertised tool schema."
end

local function validate_seed_history(id, factory_name, seed)
  local copied = {}
  if seed == nil then return copied end
  if type(seed) ~= "table" then
    return nil, string.format("%s '%s': params.history must be an array of transcript messages (got %s)",
      factory_name, tostring(id), type(seed))
  end
  local keys = 0
  for _ in pairs(seed) do keys = keys + 1 end
  if keys ~= #seed then
    return nil, string.format("%s '%s': params.history must be an array of transcript messages, not a map",
      factory_name, tostring(id))
  end
  for i = 1, #seed do
    local entry = seed[i]
    if type(entry) ~= "table" then
      return nil, string.format("%s '%s': params.history[%d] must be a role-tagged message table",
        factory_name, tostring(id), i)
    end
    if type(entry.role) ~= "string" or entry.role == "" then
      return nil, string.format("%s '%s': params.history[%d] is missing a role",
        factory_name, tostring(id), i)
    end
    if entry.tool_calls ~= nil and type(entry.tool_calls) ~= "table" then
      return nil, string.format("%s '%s': params.history[%d].tool_calls must be an array",
        factory_name, tostring(id), i)
    end
    copied[i] = entry
  end
  return copied
end

local function encode_args(args)
  if type(args) == "string" then return args end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.encode) == "function" then
    local ok, encoded = pcall(nefor.json.encode, args or {})
    if ok and type(encoded) == "string" then return encoded end
  end
  return args
end

function M.construct(id, params, emit, options)
  -- Intrinsic tools belong to this instance, not the reusable authored params.
  params = json_data.copy(params or {})
  options = options or {}
  local factory_name = options.name or "provider-boundary"
  local provider = params.provider
  if type(provider) ~= "string" or provider == "" then
    return nil, string.format("%s '%s': params.provider is required", factory_name, tostring(id))
  end
  local policy, policy_error = policy_api.lower(params.compaction_policy)
  if not policy then return nil, policy_error end
  local compaction = options.compaction
  -- The agent's approval policy. Its name rides on every provider request so
  -- the request's tool descriptions can say what that policy means for each
  -- tool. An agent without one cannot be constructed.
  local approval_policy, policy_error = approval.from_mag(params.tool_approval_policy)
  if not approval_policy then
    return nil, string.format("%s '%s': params.tool_approval_policy: %s",
      factory_name, tostring(id), policy_error)
  end
  params.tools = json_data.copy(params.tools or {})
  params.tool_specs = json_data.copy(params.tool_specs or {})
  for _, name in ipairs(params.tools) do
    if name == "self_compact" then return nil, "configured tool conflicts with intrinsic self_compact" end
  end
  for _, spec in ipairs(params.tool_specs) do
    if spec.name == "self_compact" then return nil, "configured tool conflicts with intrinsic self_compact" end
  end
  params.tools[#params.tools + 1] = "self_compact"
  params.tool_specs[#params.tool_specs + 1] = {
    name = "self_compact", owner = "mag-runtime", execution = { kind = "routed" },
    description = "Compact the current context after a context-usage notice has been received. Takes no arguments. Do not combine with submit_output.",
    parameters = { type = "object", properties = {}, additionalProperties = false },
  }
  local seed_history, history_error = validate_seed_history(id, factory_name, params.history)
  if not seed_history then return nil, history_error end
  -- The seed is recorded into the canonical conversation on first firing. Do
  -- not leave another transcript reachable through the actor's params table.
  params.history = nil
  local system_prompt = params.system
  params.system = nil

  local conversation = options.conversation
  if type(conversation) ~= "table" or type(conversation.id) ~= "string"
      or type(conversation.emit) ~= "function" then
    return nil, string.format("%s '%s': conversation dependency is required", factory_name, tostring(id))
  end
  if type(compaction) ~= "table" then
    return nil, "provider boundary requires deps.compaction"
  end
  local provenance = {}
  for key, value in pairs(conversation.provenance or {}) do provenance[key] = value end
  provenance.provider = provider
  provenance.model = params.model
  if type(params.provider_options) == "table" then
    provenance.provider_options = json_data.copy(params.provider_options)
  end

  local function sign(message)
    message.from = id
    return message
  end

  local instance = { id = id }
  local state = {}
  local seq = 0
  local pending = nil
  local compact_wait = nil
  local boundary_wait = nil
  local generation = 0
  local handles = {}
  local delivery_requests = {}
  local held_batch = nil
  local active_batch = nil
  local prepared = nil
  local invoke_provider

  local draining = false
  local turn_active = false
  local awaiting_continuation = false
  local steered_messages = {}
  local streamed_message_id = nil
  local streamed_text = ""
  local terminal_metadata = {}
  local provider_round_metadata = {}
  local provider_round_completion = {}
  local facts = nil
  local firing_sequence = 0
  local native_exchanges = {}
  local native_exchange_order = {}
  local native_exchange_sequence = 0
  local conversation_created = conversation.is_root
  local seed_recorded = false
  local submission_ids = params.submission_ids
  params.submission_ids = nil
  local input_cause = params.input_cause
  params.input_cause = nil
  local authored_prompt = params.authored_prompt
  params.authored_prompt = nil
  local submission_identity_recorded = false

  local function start_firing()
    prepared = nil
    firing_sequence = firing_sequence + 1
    local turn_id = conversation.turn_id
    if firing_sequence > 1 then
      turn_id = turn_id .. ":firing:" .. tostring(firing_sequence)
    end
    facts = conversation_facts.new({
      conversation_id = conversation.id,
      turn_id = turn_id,
      provenance = provenance,
      emit = conversation.emit,
    })
    if not conversation_created then
      facts:create(provenance)
      conversation_created = true
    end
    if not seed_recorded then
      if type(system_prompt) == "string" and system_prompt ~= "" then
        facts:seed_message({ role = "system", content = system_prompt })
      end
      for _, message in ipairs(seed_history) do facts:seed_message(message) end
      system_prompt = nil
      seed_history = nil
      seed_recorded = true
    end
    facts:start_turn()
    streamed_message_id = nil
    streamed_text = ""
    terminal_metadata = {}
  end

  local function merge_terminal(detail)
    local merged = {}
    for key, value in pairs(terminal_metadata) do merged[key] = value end
    for key, value in pairs(detail or {}) do merged[key] = value end
    local result = merged.result
    if type(result) == "table" then
      for _, key in ipairs({ "model", "duration_ms", "usage", "finish_reason", "latest_response_usage" }) do
        if merged[key] == nil then merged[key] = result[key] end
      end
    end
    local usage = merged.usage
    if type(usage) ~= "table"
        and (type(merged.prompt_tokens) == "number"
          or type(merged.context_input_tokens) == "number") then
      usage = {
        prompt_tokens = merged.prompt_tokens,
        completion_tokens = merged.completion_tokens,
        context_input_tokens = merged.context_input_tokens,
      }
    end
    if type(usage) == "table" then
      if usage.input_tokens == nil then usage.input_tokens = usage.prompt_tokens end
      if usage.output_tokens == nil then usage.output_tokens = usage.completion_tokens end
      if usage.context_input_tokens == nil then
        usage.context_input_tokens = terminal_metadata.context_input_tokens
      end
      merged.usage = usage
    end
    if merged.latest_response_usage ~= nil then
      merged.completion = json_data.copy(merged.completion or {})
      merged.completion.latest_response_usage = json_data.copy(merged.latest_response_usage)
      merged.completion.provider = provider
    end
    return merged
  end

  local function object_arguments(value)
    if type(value) == "table" and not json_data.is_array(value) then
      return json_data.copy(value)
    end
    return {}
  end

  local function merge_arguments(current, incoming)
    local merged = object_arguments(current)
    for key, value in pairs(object_arguments(incoming)) do merged[key] = value end
    return merged
  end

  local function ensure_streamed_message()
    if streamed_message_id == nil then
      streamed_message_id = facts:start_message("assistant")
      streamed_text = ""
    end
    return streamed_message_id
  end

  local function native_key(tool_call_id)
    local request_id = pending and pending.request_id or "unbound"
    return request_id .. ":" .. tostring(#tool_call_id) .. ":" .. tool_call_id
  end

  local function start_native_exchange(value)
    local tool_call_id = value.tool_call_id
    local tool_name = value.name
    if type(tool_call_id) ~= "string" or tool_call_id == ""
        or type(tool_name) ~= "string" or tool_name == "" then
      return nil
    end
    local key = native_key(tool_call_id)
    local existing = native_exchanges[key]
    if existing then
      if existing.status == "open" then
        existing.arguments = merge_arguments(existing.arguments, value.arguments)
      end
      return existing
    end
    native_exchange_sequence = native_exchange_sequence + 1
    local message_id = ensure_streamed_message()
    local exchange = {
      key = key,
      exchange_id = message_id .. ":native-exchange:" .. tostring(native_exchange_sequence),
      message_id = message_id,
      tool_call_id = tool_call_id,
      name = tool_name,
      arguments = object_arguments(value.arguments),
      status = "open",
    }
    native_exchanges[key] = exchange
    native_exchange_order[#native_exchange_order + 1] = key
    facts:start_tool_exchange(
      exchange.message_id, exchange.exchange_id, exchange.tool_call_id, exchange.name)
    return exchange
  end

  local function settle_native_exchange(exchange, result, error)
    if not exchange or exchange.status ~= "open" then return false end
    facts:append_tool_arguments(exchange.exchange_id, exchange.arguments)
    facts:complete_tool_call(
      exchange.exchange_id, exchange.tool_call_id, exchange.name, exchange.arguments)
    if error ~= nil then
      facts:record_tool_error(exchange.exchange_id, bounded(error))
    else
      facts:record_tool_result(exchange.exchange_id, result)
    end
    exchange.status = "settled"
    return true
  end

  local function handle_native_observation(value)
    if value.kind == "tool_execution_started" then
      return start_native_exchange(value) ~= nil
    end
    if value.kind ~= "tool_execution_completed" and value.kind ~= "tool_execution_failed" then
      return false
    end
    local exchange = start_native_exchange(value)
    if not exchange or exchange.status ~= "open" then return true end
    if type(value.arguments) == "table" and not json_data.is_array(value.arguments) then
      exchange.arguments = object_arguments(value.arguments)
    end
    if value.kind == "tool_execution_failed" then
      settle_native_exchange(exchange, nil, value.error or "provider tool execution failed")
    elseif value.result ~= nil then
      settle_native_exchange(exchange, value.result, nil)
    end
    return true
  end

  local function settle_open_native(error)
    for _, key in ipairs(native_exchange_order) do
      local exchange = native_exchanges[key]
      if exchange and exchange.status == "open" then
        settle_native_exchange(exchange, nil, error)
      end
    end
  end

  local function reset_native_exchanges()
    native_exchanges = {}
    native_exchange_order = {}
    native_exchange_sequence = 0
  end

  local function interrupt_stream(reason)
    if streamed_message_id == nil then return end
    settle_open_native(reason)
    facts:interrupt_message(streamed_message_id, { reason = reason })
    streamed_message_id = nil
    streamed_text = ""
  end

  local function discard_stream(detail)
    settle_open_native("provider attempt discarded before native tool execution completed")
    if streamed_message_id ~= nil then
      facts:interrupt_message(streamed_message_id, detail or {}, "discarded")
      streamed_message_id = nil
      streamed_text = ""
    end
    reset_native_exchanges()
  end

  function state:append(message, completion)
    if message.role == "assistant" and completion == nil then
      completion = provider_round_completion
    end
    if message.role == "assistant" and streamed_message_id then
      local content = message.content
      if type(content) == "string" and content ~= "" then
        if streamed_text == "" then
          facts:content(streamed_message_id, "text", content)
        elseif content:sub(1, #streamed_text) == streamed_text
            and #content > #streamed_text then
          facts:content(streamed_message_id, "text", content:sub(#streamed_text + 1))
        end
      end
      facts:finish_message(streamed_message_id, message, completion)
      streamed_message_id = nil
      streamed_text = ""
    else
      facts:message(message, completion)
    end
  end
  function state:complete_streamed_message(message, completion)
    if streamed_message_id == nil then return false end
    self:append(message, completion)
    return true
  end
  function state:emit(message) emit(sign(message)) end
  function state:is_draining() return draining end
  -- True while a provider round has an open streamed assistant message. A
  -- classifier must close it even when the round produced no text, otherwise
  -- the turn cannot reach a terminal fact.
  function state:is_streaming() return streamed_message_id ~= nil end
  function state:finish(message, terminal_detail)
    turn_active = false
    awaiting_continuation = false
    facts:complete_turn(merge_terminal(terminal_detail or {
      result = message.result,
      value = message.value,
      semantic_type_id = message.semantic_type_id,
    }))
    self:emit(message)
    self:emit({ kind = kinds.complete })
  end
  function state:finish_many(messages, terminal_detail)
    turn_active = false
    awaiting_continuation = false
    facts:complete_turn(merge_terminal(terminal_detail or {}))
    for _, message in ipairs(messages or {}) do self:emit(message) end
    self:emit({ kind = kinds.complete })
  end
  function state:fail(detail)
    settle_open_native(detail or "provider failed")
    interrupt_stream("provider_failed")
    turn_active = false
    awaiting_continuation = false
    facts:fail_turn(merge_terminal({ error = detail }))
    self:emit({ kind = kinds.failed, failure = kinds.Failed, value = { error = detail } })
  end

  local function record_input(input)
    local function append(message)
      if not submission_identity_recorded and message.role == "user" then
        message.submission_ids = submission_ids
        message.input_cause = input_cause
        message.authored_prompt = authored_prompt
        submission_identity_recorded = true
      end
      state:append(message)
    end
    if type(input) == "string" then
      append({ role = "user", content = input })
    elseif type(input) == "table" and type(input.messages) == "table" then
      for _, message in ipairs(input.messages) do append(message) end
    elseif type(input) == "table" and input.role ~= nil then
      append(input)
    elseif type(input) == "table" and input.text ~= nil then
      append({ role = "user", content = input.text })
    end
  end

  local function build_request()
    return {
      conversation_id = conversation.id,
      model = params.model,
      tools = params.tools,
      reasoning_effort = params.reasoning_effort,
      provider_options = params.provider_options,
      output_schema = params.schema,
      tool_specs = params.tool_specs,
      approval_policy = approval_policy,
    }
  end

  -- Each continuation owns a generation. Detaching cannot undo canonical writes,
  -- but it prevents a late receipt from re-entering a dead provider boundary.
  local function await_operation(operation, args, done)
    local live_generation = generation
    local handle
    local settled = false
    handle = compaction[operation](args, function(outcome)
      if settled then return end
      settled = true
      handles[handle] = nil
      if live_generation ~= generation then return end
      if outcome.kind == "Ok" then done(outcome.value)
      else done(nil, outcome.error) end
    end)
    handles[handle] = true
    return handle
  end

  local function read_context(min_watermark, done)
    await_operation("read", { conversation_id = conversation.id, provider = provider,
      model = params.model, min_watermark = min_watermark }, done)
  end

  local function canonical_view(done)
    await_operation("barrier", { conversation_id = conversation.id }, function(receipt, err)
      if not receipt then done(nil, err); return end
      read_context(receipt.watermark, done)
    end)
  end

  local function request_compact(view, trigger, cutoff, call_id, resume)
    local wait = { trigger = trigger, resume = resume }
    compact_wait = wait
    local function finish(receipt, err)
      if compact_wait ~= wait then return end
      compact_wait = nil
      if resume then resume(receipt, err)
      elseif err then state:fail(err)
      else invoke_provider(true) end
    end
    wait.handle = await_operation("request_compaction", {
      conversation_id = conversation.id, epoch_id = view.policy_state.epoch_id,
      history_id = view.history_id, context_watermark = view.watermark,
      history_cutoff = cutoff, provider = provider, model = params.model,
      provider_options = params.provider_options, trigger = trigger, tool_call_id = call_id },
      function(receipt, err)
        if not receipt then finish(nil, err); return end
        if receipt.status ~= "compacted" or receipt.request_id ~= wait.handle.id then
          finish(nil, "compaction_terminal_required"); return
        end
        canonical_view(function(committed, failure)
          if not committed or committed.pending_compaction
              or committed.policy_state.epoch_id ~= wait.handle.id then
            finish(nil, failure or "compaction_commit_not_visible")
          else finish(receipt) end
        end)
      end)
    wait.request_id = wait.handle.id
    return true
  end

  invoke_provider = function(after_commit)
    if draining or compact_wait or boundary_wait then return false end
    boundary_wait = "WaitingWrites"
    settle_open_native("provider request replaced before native tool execution completed")
    reset_native_exchanges()
    local function fail(err)
      boundary_wait = nil
      state:fail(err)
    end
    local function submit(view)
      if view.pending_compaction then fail("compaction_recovery_required"); return end
      boundary_wait = nil
      seq = seq + 1
      pending = { request_id = id .. "@r" .. tostring(seq) }
      local request = build_request()
      request.context_watermark = view.watermark
      local notice = view.policy_state.outstanding_notice
      if notice then
        pending.notice = notice
        request.notice_id, request.epoch_id = notice.notice_id, notice.epoch_id
      end
      state:emit({ kind = "capability.invoke", class = "provider", capability = provider,
        request = request, ref = pending.request_id })
    end
    local function preflight(view, err)
      if not view then fail(err); return end
      if view.pending_compaction then fail("compaction_recovery_required"); return end
      local decision, failure = policy_api.preflight(policy, view.policy_state,
        view.latest_response_usage, prepared.regular_window)
      if not decision then fail(failure); return end
      if decision.kind == "Compact" then
        boundary_wait = nil
        if after_commit then fail("compaction_no_progress"); return end
        request_compact(view, "hard", view.context.history_length)
      elseif decision.kind == "Notice" then
        boundary_wait = "WaitingNoticeAppend"
        local data = decision.notice_data
        local text = string.format("Context usage is %g%% of the regular %g-token window (%g tokens). You may now request self_compact({}) to compact this context. Automatic compaction runs before another model request at %g%%.",
          data.measured_percent, data.regular_window, data.total_tokens, policy.hard_percent)
        await_operation("append_notice", { conversation_id = conversation.id,
          expected_watermark = view.watermark, epoch_id = view.policy_state.epoch_id,
          history_id = view.history_id, provider = provider, model = params.model,
          notice_data = data, text = text }, function(receipt, rejection)
            if not receipt then fail(rejection); return end
            boundary_wait = "WaitingReadAfterNotice"
            read_context(receipt.watermark, function(fresh, read_error)
              if not fresh then fail(read_error) else submit(fresh) end
            end)
          end)
      else submit(view) end
    end
    await_operation("barrier", { conversation_id = conversation.id }, function(receipt, err)
      if not receipt then fail(err); return end
      local function read()
        boundary_wait = "WaitingRead"
        read_context(receipt.watermark, preflight)
      end
      if prepared then read(); return end
      boundary_wait = "WaitingPrepare"
      await_operation("prepare", { provider = provider, model = params.model,
        provider_options = params.provider_options, policy = policy_api.raw(policy) }, function(ready, failure)
          if not ready or ready.kind ~= "Ready" then fail(failure or "compaction_prepare_failed"); return end
          prepared = ready
          read()
        end)
    end)
    return true
  end
  function state:retry(reason)
    facts:retry(reason or "provider_retry")
    return invoke_provider()
  end

  local function append_steered_messages()
    if #steered_messages == 0 then return false end
    for _, message in ipairs(steered_messages) do state:append(message) end
    steered_messages = {}
    return true
  end

  local function emit_failure(detail)
    settle_open_native(detail or "provider failed")
    interrupt_stream("provider_failed")
    if options.on_error then
      options.on_error(state, detail)
    else
      state:fail(detail)
    end
  end

  function state:record_calls(result, calls)
    local wire_calls = {}
    for i, call in ipairs(calls) do
      wire_calls[i] = { id = call.id, type = "function", ["function"] = {
        name = call.name, arguments = encode_args(call.args) } }
    end
    self:append({ role = "assistant", content = type(result.text) == "string" and result.text or "",
      tool_calls = wire_calls })
  end

  local function emit_tool_calls(result, calls)
    state:record_calls(result, calls)
    local semantic_calls = {}
    for i, call in ipairs(calls) do
      semantic_calls[i] = { name = call.name, arguments = call.args }
    end
    awaiting_continuation = true
    -- The request-local tools this actor offered its model are its own: the
    -- tool surface never sees them, so name them beside the calls for it to
    -- include when it tells the model which tools it can call.
    local request_local_tools = nil
    for _, spec in ipairs(params.tool_specs or {}) do
      request_local_tools = request_local_tools or {}
      request_local_tools[#request_local_tools + 1] = spec.name
    end
    state:emit({ kind = "generic-tool.ToolCalls",
      value = { calls = semantic_calls }, calls = calls,
      request_local_tools = request_local_tools })
    state:emit({ kind = kinds.complete })
  end

  local function resume_delivery(request)
    if request.delivery_failed then return end
    if request.inclusion and not request.notice_acknowledged then
      if not request.bound then return end
      if not request.success then
        local resume = request.resume
        request.resume = nil
        if resume then delivery_requests[request.request_id] = nil; resume() end
        return
      end
      if request.ack_handle then return end
      request.ack_handle = await_operation("acknowledge_notice", request.inclusion, function(receipt, err)
        if not receipt then
          request.delivery_failed = true
          request.resume = nil
          delivery_requests[request.request_id] = nil
          pending = nil
          state:fail(err or "compaction_notice_ack_rejected")
          return
        end
        request.notice_acknowledged = true
        resume_delivery(request)
      end)
      return
    end
    local resume = request.resume
    request.resume = nil
    if resume then
      delivery_requests[request.request_id] = nil
      resume()
    end
  end

  function instance.deliver(activation)
    activation = activation or {}
    if activation.kind == "reply" then
      if pending == nil or activation.ref ~= pending.request_id then return nil end
      local completed_request = pending
      pending = nil
      local function process_terminal()
        if activation.error ~= nil then
          settle_open_native(activation.error)
          emit_failure(activation.error)
          return nil
        end
        local result = activation.result
        local round_detail = {}
        for key, value in pairs(provider_round_metadata) do round_detail[key] = value end
        for key, value in pairs(type(result) == "table" and result or {}) do
          round_detail[key] = value
        end
        if round_detail.latest_response_usage == nil and type(result) == "table"
            and result.finish_reason ~= "error" and result.finish_reason ~= "interrupted"
            and result.billing_components == nil then
          round_detail.latest_response_usage = require("libs.chat.context_usage").from_response(result.usage)
        end
        provider_round_completion = merge_terminal(round_detail)
        -- Completion metadata is round-local. Terminal billing may accumulate,
        -- but a prior response measurement cannot reappear after a checkpoint.
        provider_round_completion.latest_response_usage = round_detail.latest_response_usage
        provider_round_completion.completion = json_data.copy(provider_round_completion.completion or {})
        provider_round_completion.completion.latest_response_usage = round_detail.latest_response_usage
        provider_round_metadata = {}
        settle_open_native("provider completion ended before native tool execution completed")
        if type(result) == "table" and result.finish_reason == "error" then
          local detail = result.error
          if type(detail) ~= "string" or detail == "" then
            detail = "provider returned finish_reason \"error\" with no detail"
          end
          emit_failure(detail)
          return nil
        end
        if type(result) ~= "table" or result.finish_reason ~= "interrupted" then
          facts:response_usage(completed_request.request_id, provider, params.model,
            provider_round_completion.latest_response_usage)
        end
        if type(result) == "table" and type(result.tool_calls) == "table" and #result.tool_calls > 0 then
          local calls, invalid = inspect_tool_calls(result.tool_calls)
          if #invalid > 0 then
            if streamed_message_id then
              interrupt_stream("invalid_tool_call")
            elseif type(result.text) == "string" and result.text ~= "" then
              state:append({ role = "assistant", content = result.text })
            end
            if draining then
              state:fail("actor drained with malformed tool arguments; rejected batch was not executed")
              return nil
            end
            if options.diagnostic then options.diagnostic({ kind = "malformed_tool_call", calls = invalid, executed = false }) end
            state:append({ role = "system", content = correction_message(invalid), visibility = "diagnostic" })
            state:retry("invalid_tool_call_arguments")
            return nil
          end
          local self_calls, terminal = {}, false
          for _, call in ipairs(calls) do
            if call.name == "self_compact" then self_calls[#self_calls + 1] = call end
            if call.name == "submit_output" then terminal = true end
          end
          if #self_calls > 0 then
            if terminal then
              state:record_calls(result, calls)
              for _, call in ipairs(calls) do
                state:append({ role = "tool", tool_call_id = call.id, name = call.name,
                  content = "[tool error] compaction_terminal_conflict", error = "compaction_terminal_conflict" })
              end
              state:retry("compaction_terminal_conflict")
              return nil
            end
            if draining then
              state:record_calls(result, calls)
              for _, call in ipairs(calls) do
                state:append({ role = "tool", tool_call_id = call.id, name = call.name,
                  content = "[tool error] actor_drained", error = "actor_drained" })
              end
              state:fail("actor drained before self compaction")
              return nil
            end
            boundary_wait = "WaitingSelfRead"
            canonical_view(function(view, err)
              boundary_wait = nil
              if not view then state:fail(err); return end
              if view.pending_compaction then state:fail("compaction_recovery_required"); return end
              local ready = prepared
              local capability = json_data.copy(ready.capability)
              capability.provider, capability.model = provider, params.model
              capability.history_id = view and view.history_id
              local eligible, reason = policy_api.self_eligibility(policy,
                view and view.policy_state or {}, capability)
              local external, valid, errors = {}, {}, {}
              for _, call in ipairs(calls) do
                if call.name ~= "self_compact" then external[#external + 1] = call
                elseif next(call.args) ~= nil then errors[call.id] = "invalid_arguments"
                elseif not eligible then errors[call.id] = reason
                else valid[#valid + 1] = call end
              end
              -- Result-draft edits are ordinary siblings, but cannot be dispatched
              -- until the complete batch has passed the terminal-conflict check.
              if options.on_tool_calls then options.on_tool_calls(state, result, external) end
              held_batch = { calls = calls, external = external, valid = valid, errors = errors,
                cutoff = view and view.context.history_length }
              state:record_calls(result, calls)
              awaiting_continuation = true
              local semantic = nefor.json.mark_array({})
              for i, call in ipairs(external) do semantic[i] = { name = call.name, arguments = call.args } end
              local request_local_tools = {}
              for _, spec in ipairs(params.tool_specs) do
                request_local_tools[#request_local_tools + 1] = spec.name
              end
              state:emit({ kind = "generic-tool.ToolCalls", value = { calls = semantic }, calls = external,
                request_local_tools = request_local_tools })
              state:emit({ kind = kinds.complete })
            end)
            return nil
          end
          if options.on_tool_calls and options.on_tool_calls(state, result, calls) then return nil end
          if draining then
            state:record_calls(result, calls)
            for _, call in ipairs(calls) do
              state:append({ role = "tool", tool_call_id = call.id, name = call.name,
                content = "[tool error] Actor drained before tool continuation; call was not executed.",
                error = "actor_drained" })
            end
            state:fail("actor drained before tool continuation")
            return nil
          end
          emit_tool_calls(result, calls)
          return nil
        end
        if options.steerable and #steered_messages > 0 then
          if options.on_steered_final then options.on_steered_final(state, result) end
          append_steered_messages()
          if draining then
            state:fail("actor drained before steered continuation")
          else
            invoke_provider()
          end
        else
          options.on_final(state, result)
        end
        return nil
      end
      local result = activation.result
      if activation.error == nil and (type(result) ~= "table"
          or (result.finish_reason ~= "error" and result.finish_reason ~= "interrupted")) then
        completed_request.success = true
      end
      completed_request.resume = process_terminal
      delivery_requests[completed_request.request_id] = completed_request
      resume_delivery(completed_request)
      return nil
    end

    if draining or boundary_wait or compact_wait or next(delivery_requests) ~= nil then return nil end
    local continuation = awaiting_continuation
    awaiting_continuation = false
    if not continuation then start_firing() end
    if not turn_active then turn_active = true end
    local input = ((activation.messages or {})[1] or {}).message
    if held_batch then
      local batch = held_batch
      active_batch = batch
      held_batch = nil
      local ordinary = {}
      for _, message in ipairs((input or {}).messages or {}) do ordinary[message.tool_call_id] = message end
      local missing
      for _, call in ipairs(batch.external) do
        if ordinary[call.id] == nil then missing = "compaction_sibling_result_missing" end
      end
      local function finish_batch(receipt, failure)
        if batch.finished then return end
        batch.finished = true
        active_batch = nil
        boundary_wait = nil
        if type(failure) == "table" then failure = failure.code or failure.message or "compaction_failed" end
        for _, call in ipairs(batch.calls) do
          local message = ordinary[call.id]
          if not message then
            local error = batch.errors[call.id] or failure
            if call.name ~= "self_compact" then error = error or "compaction_sibling_result_missing" end
            message = { role = "tool", tool_call_id = call.id, name = call.name,
              content = error and ("[tool error] " .. tostring(error)) or nefor.json.encode(receipt), error = error }
          end
          if not batch.siblings_recorded or not ordinary[call.id] then state:append(message) end
        end
        append_steered_messages()
        if failure then state:fail(failure) else invoke_provider(receipt ~= nil) end
      end
      if missing then finish_batch(nil, missing); return { status = "pending" } end
      if #batch.valid == 0 then finish_batch(nil, nil); return { status = "pending" } end
      -- Sibling results must be canonical before compaction, but the unfinished
      -- self exchange (and its whole assistant batch) stays outside the cutoff.
      for _, call in ipairs(batch.external) do state:append(ordinary[call.id]) end
      batch.siblings_recorded = true
      boundary_wait = "WaitingSiblingWrites"
      canonical_view(function(view, err)
        if not view then finish_batch(nil, err); return end
        if view.pending_compaction then finish_batch(nil, "compaction_recovery_required"); return end
        local capability = json_data.copy(prepared.capability)
        capability.provider, capability.model, capability.history_id = provider, params.model, view.history_id
        local eligible, reason = policy_api.self_eligibility(policy, view.policy_state, capability)
        if not eligible then finish_batch(nil, reason); return end
        boundary_wait = nil
        request_compact(view, "self", batch.cutoff, batch.valid[1].id, finish_batch)
      end)
      return { status = "pending" }
    end
    record_input(input)
    append_steered_messages()
    if not continuation and options.on_turn_start then options.on_turn_start(state) end
    invoke_provider()
    return { status = "pending" }
  end

  local function interrupt_held_batch(reason)
    local batch = held_batch or active_batch
    if not batch or batch.finished then return end
    batch.finished = true
    for _, call in ipairs(batch.calls) do
      if not batch.siblings_recorded or call.name == "self_compact" then state:append({ role = "tool", tool_call_id = call.id, name = call.name,
        content = "[tool error] " .. reason, error = reason }) end
    end
    held_batch, active_batch = nil, nil
  end

  local function cancel_wait(reason)
    generation = generation + 1
    for handle in pairs(handles) do compaction.detach(handle) end
    handles = {}
    boundary_wait = nil
    delivery_requests = {}
    local wait = compact_wait
    compact_wait = nil
    if not wait then return end
    compaction.cancel_compaction({ request_id = wait.request_id,
      conversation_id = conversation.id, reason = reason }, function(outcome)
        if outcome.kind == "Error" and options.diagnostic then
          options.diagnostic({ kind = "compaction_cancel_rejected", request_id = wait.request_id,
            conversation_id = conversation.id, error = outcome.error })
        end
      end)
    if wait.resume then wait.resume(nil, "compaction_interrupted") end
  end

  function instance.handle_kill()
    local was_active = turn_active
    draining = true
    interrupt_held_batch("compaction_interrupted")
    cancel_wait("actor_killed")
    held_batch = nil
    pending = nil
    turn_active = false
    awaiting_continuation = false
    interrupt_stream("actor_killed")
    if facts and was_active then facts:interrupt_turn({ reason = "actor_killed" }) end
  end

  function instance.handle_drain()
    if draining then return end
    draining = true
    interrupt_held_batch("compaction_interrupted")
    if pending == nil then cancel_wait("actor_drained") end
    held_batch = nil
    if pending == nil then
      local was_active = turn_active
      awaiting_continuation = false
      turn_active = false
      interrupt_stream("actor_drained")
      if facts and was_active then facts:interrupt_turn({ reason = "actor_drained" }) end
      state:emit({ kind = kinds.complete })
    end
  end

  function instance.handle_steer(message)
    if draining or not options.steerable or type(message) ~= "table" then return false end
    if type(message.role) ~= "string" or message.role == "" then return false end
    steered_messages[#steered_messages + 1] = message
    return true
  end

  function instance.handle_observation(observation)
    local value = observation and observation.value
    local observed_request = pending
    if not observed_request and observation then
      for _, request in pairs(delivery_requests) do
        if request.native_id ~= nil and (request.native_id == observation.id
            or (value and request.native_id == value.request_id)) then
          observed_request = request; break
        end
      end
    end
    if observed_request == nil then return false end
    if observation.binding == "conversation" and type(value) == "table" and value.kind == "context_included" then
      -- Router observations are correlated by the bus id; the provider uses
      -- that id, while replies echo our opaque local ref.
      local request = observed_request
      local correlated = value.request_id == request.request_id
        or (observation.id ~= nil and observation.id == value.request_id)
      if not request.notice or not correlated
          or value.notice_id ~= request.notice.notice_id or value.epoch_id ~= request.notice.epoch_id then return false end
      if request.bind_handle then return true end
      request.native_id = value.request_id
      local inclusion = { conversation_id = conversation.id, notice_id = value.notice_id,
        epoch_id = value.epoch_id, request_id = value.request_id,
        context_watermark = value.context_watermark, provider = provider, model = params.model,
        history_id = value.history_id }
      request.inclusion = inclusion
      request.bind_handle = await_operation("bind_notice", inclusion, function(receipt, err)
        if not receipt then
          request.delivery_failed = true
          request.resume = nil
          delivery_requests[request.request_id] = nil
          pending = nil
          state:fail(err or "compaction_notice_bind_rejected")
        else request.bound = true; resume_delivery(request) end
      end)
      return true
    end
    if observation.binding == "conversation" and type(value) == "table" then
      if value.kind == "response_completed" or value.event == "response_completed" then
        observed_request.success = true
        resume_delivery(observed_request)
        return true
      end
      if pending == nil then return false end
      if handle_native_observation(value) then return true end
      if value.kind == "attempt_discarded" then
        discard_stream(value)
      elseif value.kind == "retry" or (value.kind == "retry_decision" and value.retry == true) then
        facts:retry(value.error or value.message or value.retry_reason or "provider_retry", value)
      elseif value.kind == "usage" then
        -- Exact completed-response evidence belongs to the included native
        -- round, even when the enclosing operation subsequently fails.
        if require("libs.chat.context_usage").measurement(value.latest_response_usage) then
          observed_request.success = true
          resume_delivery(observed_request)
        end
        provider_round_metadata = value
        if value.latest_response_usage ~= nil then
          terminal_metadata.latest_response_usage = value.latest_response_usage
        end
        terminal_metadata.usage = value.usage or value.result or value
        terminal_metadata.context_input_tokens = value.context_input_tokens
          or (type(value.usage) == "table" and value.usage.context_input_tokens)
          or terminal_metadata.context_input_tokens
        terminal_metadata.model = value.model or terminal_metadata.model
        terminal_metadata.duration_ms = value.duration_ms or terminal_metadata.duration_ms
      elseif value.kind == "interrupted" then
        settle_open_native(value.error or value.message or "provider completion interrupted")
        interrupt_stream("provider_interrupted")
        facts:interrupt_turn(merge_terminal(value))
      elseif value.kind == "failed" or value.kind == "error" then
        settle_open_native(value.error or value.message or "provider completion failed")
        interrupt_stream("provider_failed")
        facts:fail_turn(merge_terminal(value))
      end
      return true
    end
    if observation.binding ~= "transcript" or type(value) ~= "table"
        or type(value.text) ~= "string" or value.text == "" then
      return false
    end
    local chunk_kind = value.kind == "reasoning" and "reasoning" or "text"
    if streamed_message_id == nil then
      streamed_message_id = facts:start_message("assistant")
    end
    facts:content(streamed_message_id, chunk_kind, value.text)
    if chunk_kind == "text" then streamed_text = streamed_text .. value.text end
    return true
  end

  state:emit({ kind = kinds.ready })
  return instance
end

function M.answer_text(result)
  if type(result) == "string" then return result end
  if type(result) ~= "table" then return nil end
  if type(result.text_answer) == "string" then return result.text_answer end
  if type(result.text) == "string" then return result.text end
  if type(result.result) == "string" then return result.result end
  return nil
end

return M
