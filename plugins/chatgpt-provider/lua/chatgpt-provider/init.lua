local oa = require("openai-provider")
local json_data = require("core.json_data")
local tool = require("libs.tool")
local approval = require("libs.tool.approval")

local function copy(value)
  return json_data.copy(value or {})
end

local function field(label, source, path, kind, omit)
  return {
    label = label,
    select = { source = source, path = path },
    kind = kind,
    omit = omit,
  }
end

local function composed_primary(parts)
  return { parts = parts }
end

local function literal(text)
  return { text = text }
end

local function content_display(label, primary, fields)
  return {
    compact = {
      label = label,
      primary = type(primary) == "string"
        and field(primary, "args", primary, "scalar", "missing") or primary,
    },
    expanded = { label = label, fields = fields },
    result = {
      kind = "content",
      fields = {
        {
          label = "text",
          select = { source = "result", path = "text" },
          kind = "text",
          omit = "missing",
          max_lines = 40,
          max_bytes = 12000,
        },
        field("results", "result", "results", "structured", "missing"),
      },
    },
    lifecycle = "delayed",
  }
end

local function query_schema()
  return {
    type = "object",
    properties = {
      query = { type = "string", minLength = 1, description = "Search query." },
      recency_days = {
        type = "integer", minimum = 0,
        description = "Only include results from this many recent days.",
      },
      domains = {
        type = "array",
        items = { type = "string", minLength = 1 },
        description = "Optional domain allowlist.",
      },
    },
    required = { "query" },
    additionalProperties = false,
  }
end

local function object_schema(properties, required)
  return {
    type = "object",
    properties = properties,
    required = required,
    additionalProperties = false,
  }
end

local citation_guidance = " When citing web sources in a user-facing answer, use regular Markdown links with URLs returned by these tools. Raw OpenAI citation markers are not rendered in this interface, so do not use them as citations. Keep opaque source IDs for tool navigation."

-- Model-facing definitions for the routed web reads. Argument validation is
-- the constructor's closed-schema check; `handle(name, args, call)` only lowers
-- already-valid arguments into the private provider command.
local function definitions(handle)
  local function define(spec)
    spec.execution = { kind = "routed" }
    local name = spec.name
    spec.approval = { decide = approval.reads() }
    spec.execute = function(args, call) return handle(name, args, call) end
    return tool.define(spec)
  end
  return {
    define {
      name = "web_search",
      description = "Search the public web. Use separate calls for separate queries." .. citation_guidance,
      parameters = query_schema(),
      display = content_display("web search", "query", {
        field("query", "args", "query", "scalar", "missing"),
        field("recency days", "args", "recency_days", "scalar", "missing"),
        field("domains", "args", "domains", "list", "missing"),
      }),
    },
    define {
      name = "web_open",
      description = "Open a URL or web result reference, optionally near a zero-indexed line number." .. citation_guidance,
      parameters = object_schema({
        url = { type = "string", minLength = 1, description = "URL or web result reference." },
        line = { type = "integer", minimum = 0, description = "Optional zero-indexed line number." },
      }, { "url" }),
      display = content_display("web open page",
        field("page", "args", "url", "path", "missing"), {
        field("page", "args", "url", "path", "missing"),
        field("line (zero-indexed)", "args", "line", "scalar", "missing"),
      }),
    },
    define {
      name = "web_click",
      description = "Open a numbered link from a URL or provider-issued web result reference." .. citation_guidance,
      parameters = object_schema({
        url = { type = "string", minLength = 1, description = "URL or web result reference." },
        link = { type = "integer", minimum = 0, description = "Numbered link from the opened page." },
      }, { "url", "link" }),
      display = content_display("web click", composed_primary({
        field("target", "args", "url", "path", "missing"),
        literal(" link "),
        field("link", "args", "link", "scalar", "missing"),
      }), {
        field("page", "args", "url", "path", "missing"),
        field("link", "args", "link", "scalar", "missing"),
      }),
    },
    define {
      name = "web_find",
      description = "Find text within a URL or provider-issued web result reference." .. citation_guidance,
      parameters = object_schema({
        url = { type = "string", minLength = 1, description = "URL or web result reference." },
        pattern = { type = "string", minLength = 1, description = "Text to find." },
      }, { "url", "pattern" }),
      display = content_display("web find", composed_primary({
        literal("“"),
        field("pattern", "args", "pattern", "scalar", "missing"),
        literal("” in "),
        field("target", "args", "url", "path", "missing"),
      }), {
        field("pattern", "args", "pattern", "scalar", "missing"),
        field("page", "args", "url", "path", "missing"),
      }),
    },
    define {
      name = "web_image_search",
      description = "Search the public web for images. This remains an image query on the provider wire." .. citation_guidance,
      parameters = query_schema(),
      display = content_display("web image search", "query", {
        field("query", "args", "query", "scalar", "missing"),
        field("recency days", "args", "recency_days", "scalar", "missing"),
        field("domains", "args", "domains", "list", "missing"),
      }),
    },
    define {
      name = "web_screenshot",
      description = "Request a zero-indexed PDF page screenshot. Open the PDF with web_open first, then pass the provider-issued PDF reference returned in its output; direct PDF URLs may not resolve. The endpoint currently returns plaintext and opaque references, not proven image media." .. citation_guidance,
      parameters = object_schema({
        url = { type = "string", minLength = 1, description = "Provider-issued PDF reference returned by web_open." },
        page = { type = "integer", minimum = 0, description = "Zero-indexed PDF page number." },
      }, { "url", "page" }),
      display = content_display("web screenshot", composed_primary({
        field("target", "args", "url", "path", "missing"),
        literal(" page "),
        field("page", "args", "page", "scalar", "missing"),
      }), {
        field("page reference", "args", "url", "path", "missing"),
        field("page (zero-indexed)", "args", "page", "scalar", "missing"),
      }),
    },
  }
end

local WEB_COMMANDS = {
  web_search = "search_query",
  web_image_search = "image_query",
  web_open = "open",
  web_click = "click",
  web_find = "find",
  web_screenshot = "screenshot",
}

local function nonempty(value)
  return type(value) == "string" and value ~= ""
end

-- Lower schema-validated arguments into the provider's command shape.
local function lower_operation(name, args)
  if name == "web_search" or name == "web_image_search" then
    local command = { q = args.query }
    if args.recency_days ~= nil then command.recency = args.recency_days end
    if args.domains ~= nil then command.domains = copy(args.domains) end
    return command
  end
  if name == "web_open" then return { ref_id = args.url, lineno = args.line } end
  if name == "web_click" then return { ref_id = args.url, id = args.link } end
  if name == "web_find" then return { ref_id = args.url, pattern = args.pattern } end
  return { ref_id = args.url, pageno = args.page }
end

local function stable_scope(invocation)
  if type(invocation) ~= "table" then return nil end
  local scope = {}
  for _, key in ipairs({ "conversation_id", "root_conversation_id", "run_scope", "session_id" }) do
    if nonempty(invocation[key]) then scope[key] = invocation[key] end
  end
  if next(scope) == nil then return nil end
  return scope
end

local function translator(name, options)
  local t = oa.translator(name)
  local prefix = name .. "."
  local base_outbound = t.outbound
  local base_inbound = t.inbound
  options = options or {}
  local trusted_tool_gate = options.tool_gate

  t.kinds.completion_request = prefix .. "completion.request"
  t.kinds.completion_cancel = prefix .. "completion.cancel"
  t.kinds.completion_event = prefix .. "completion.event"
  t.kinds.stream_tool_call_delta = prefix .. "stream.tool_call_delta"
  t.kinds.usage_requested = prefix .. "usage.requested"
  t.kinds.usage_updated = prefix .. "usage.updated"
  t.kinds.usage_error = prefix .. "usage.error"
  t.kinds.web_request = prefix .. "web.request"
  t.kinds.web_cancel = prefix .. "web.cancel"
  t.kinds.web_result = prefix .. "web.result"

  t.complete = function(request, context)
    assert(type(request) == "table", "chatgpt-provider.complete: request required")
    assert(type(request.request_id) == "string" and #request.request_id > 0,
      "chatgpt-provider.complete: request_id required")
    assert(type(context) == "table" and type(context.messages) == "table",
      "chatgpt-provider.complete: conversation context required")
    local body = copy(request)
    body.provider = nil
    body.watermark = nil
    body.messages = {}
    for index, message in ipairs(context.messages) do body.messages[index] = t.context_message(message) end
    local lowered_context = copy(context)
    local checkpoint = context.compaction and context.compaction.checkpoint
    if not t.checkpoint_compatible(checkpoint, name, request.model, request.provider_options) then
      lowered_context.compaction = nil
    end
    lowered_context.messages = {}
    for index, message in ipairs(context.messages) do lowered_context.messages[index] = t.context_message(message) end
    lowered_context.tail_messages = {}
    for index, message in ipairs(context.tail_messages or {}) do
      lowered_context.tail_messages[index] = t.context_message(message)
    end
    body.conversation_context = lowered_context
    body.kind = t.kinds.completion_request
    return body
  end

  t.cancel_completion = function(request_id)
    assert(type(request_id) == "string" and #request_id > 0,
      "chatgpt-provider.cancel: request_id required")
    return { kind = t.kinds.completion_cancel, request_id = request_id }
  end

  -- The routed web tools. A valid invocation's handler lowers it into the
  -- private web request that `inbound` returns for delivery to the provider
  -- process; invalid arguments settle from this source without reaching Rust.
  local lowered = nil
  local function handle(tool_name, args, call)
    local body = call.request
    local invocation = type(body.invocation) == "table" and body.invocation or nil
    local invocation_provider = invocation and invocation.provider or nil
    if nonempty(invocation_provider) and invocation_provider ~= name then
      call.err("routed web tools owned by `" .. name
        .. "` cannot execute an invocation for provider `" .. invocation_provider .. "`")
      return
    end
    lowered = {
      kind = t.kinds.web_request,
      id = call.id,
      caller_id = body.caller_id,
      invoking_from = body.from,
      name = tool_name,
      model = body.model or (invocation and invocation.model),
      scope = stable_scope(invocation),
      invocation = invocation and copy(invocation) or nil,
      settings = { allowed_callers = { "direct" }, external_web_access = true },
      commands = { [WEB_COMMANDS[tool_name]] = { lower_operation(tool_name, args) } },
    }
  end
  t.tool_source = tool.source {
    name = name,
    tools = definitions(handle),
    emit = function(body) t.publish(name, body) end,
  }

  local compaction_format = "chatgpt.responses.compaction.v1"
  t.compaction_capability = function(provider)
    if provider ~= name then
      return { kind = "unsupported", reason = "provider does not match adapter" }
    end
    return { kind = "supported" }
  end
  t.checkpoint_compatible = function(checkpoint, provider, model)
    return type(checkpoint) == "table" and provider == name
      and checkpoint.provider == provider and type(model) == "string" and model ~= ""
      and checkpoint.model == model and checkpoint.format == compaction_format
      and type(checkpoint.artifact) == "table" and type(checkpoint.artifact.items) == "table"
      and #checkpoint.artifact.items > 0
  end

  t.compact_context = function(input)
    local context, request = input.context, input.request
    if type(context) ~= "table" or type(context.messages) ~= "table" then
      return nil, "conversation context is missing complete messages"
    end
    if type(request) ~= "table" or not nonempty(request.request_id)
        or request.provider ~= name or not nonempty(request.model) then
      return nil, "conversation compaction request identity is missing"
    end
    local chat_id = "conversation-compact:" .. request.request_id
    local plan = {
      chat_id = chat_id,
      create = {
        kind = prefix .. "chat.create", chat_id = chat_id,
        conversation_id = request.conversation_id, model = request.model,
        provider_options = request.provider_options ~= nil and copy(request.provider_options) or nil,
      },
      messages = {},
      compact = { kind = prefix .. "chat.compact", chat_id = chat_id, trigger = request.trigger },
      delete = { kind = prefix .. "chat.delete", chat_id = chat_id },
    }
    local selected = context.compaction
    local checkpoint = type(selected) == "table" and selected.checkpoint or nil
    local messages = context.messages
    if t.checkpoint_compatible(checkpoint, request.provider, request.model, request.provider_options) then
      plan.restore = {
        kind = prefix .. "chat.compaction.restore", chat_id = chat_id,
        model_context_artifact = copy(checkpoint.artifact),
      }
      messages = context.tail_messages or {}
    end
    for index, message in ipairs(messages) do plan.messages[index] = t.context_message(message) end
    return plan
  end

  t.compaction_outcome = function(body, request)
    local outcome = { request_id = request.request_id, conversation_id = request.conversation_id }
    local checkpoint = { provider = name, model = body.model, format = compaction_format,
      artifact = copy(body.model_context_artifact) }
    if body.kind == t.kinds.chat_compaction_commit
        and t.checkpoint_compatible(checkpoint, request.provider, request.model, request.provider_options) then
      outcome.kind = "completed"
      outcome.checkpoint = checkpoint
      outcome.compatibility = { provider = name, model = request.model, format = compaction_format }
    else
      outcome.kind = "failed"
      outcome.error = body.kind == t.kinds.chat_compaction_commit
        and { code = "compaction_checkpoint_invalid", message = "Native checkpoint is incompatible or empty" }
        or require("core.error").normalize(body.error or body.message,
          "provider_compaction_failed", "context compaction failed")
    end
    return outcome
  end
  t.cancel_compaction = function(plan)
    return {{ kind = prefix .. "chat.cancel", chat_id = plan.chat_id }}
  end
  t.cleanup_compaction = function(plan) return { plan.delete } end

  t.inbound = function(env)
    local body = type(env) == "table" and env.body or nil
    if type(body) == "table" then
      if body.kind == t.kinds.completion_request or body.kind == "ProviderRequest" then return nil end
      -- The gate asks what a web read means before deciding to run it; the
      -- answer is published by the tool source, nothing reaches the provider.
      if body.kind == prefix .. "tool.prepare" then
        if not nonempty(trusted_tool_gate) or env.from ~= trusted_tool_gate then return nil end
        t.tool_source.prepare(body)
        return nil
      end
      if body.kind == prefix .. "tool.invoke" then
        if not nonempty(trusted_tool_gate) or env.from ~= trusted_tool_gate then return nil end
        lowered = nil
        t.tool_source.invoke(body)
        local request = lowered
        lowered = nil
        return request
      end
      if body.kind == prefix .. "tool.cancel" and nonempty(body.id) then
        if not nonempty(trusted_tool_gate) or env.from ~= trusted_tool_gate then return nil end
        local call = t.tool_source.cancel(body)
        local request = call and call.request
        if not request or type(request.invocation) ~= "table" then return nil end
        return {
          kind = t.kinds.web_cancel,
          id = body.id,
          caller_id = request.caller_id,
          invoking_from = request.from,
          model = request.model or request.invocation.model,
          invocation = copy(request.invocation),
        }
      end
    end
    return base_inbound(env)
  end

  t.outbound = function(env)
    local body = type(env) == "table" and env.body or nil
    local kind = type(body) == "table" and body.kind or nil
    if type(kind) ~= "string" then return base_outbound(env) end

    if kind == t.kinds.web_result then
      if not nonempty(body.id) then return nil end
      local result = {}
      if body.error ~= nil then result.error = body.error end
      if type(body.output) == "table" then
        result.output = copy(body.output)
      elseif body.error == nil then
        result.error = "web result missing output"
      end
      if body.provider_state ~= nil then result.provider_state = copy(body.provider_state) end
      t.tool_source.result(body.id, result)
      return nil
    end
    if kind == t.kinds.completion_event then
      if type(body.request_id) ~= "string" or #body.request_id == 0 then return nil end
      return copy(body)
    end
    if kind == t.kinds.usage_updated or kind == t.kinds.usage_error then return nil end

    local request_id = body.request_id or body.chat_id
    if type(request_id) ~= "string" or #request_id == 0 then return base_outbound(env) end
    local event
    if kind == t.kinds.completion_event then event = body.event
    elseif kind == t.kinds.stream_delta then event = "text_delta"
    elseif kind == t.kinds.stream_reasoning_delta then event = "reasoning_delta"
    elseif kind == t.kinds.stream_reasoning_end then event = "reasoning_completed"
    elseif kind == t.kinds.stream_tool_call_delta then event = "tool_call_delta"
    elseif kind == t.kinds.stream_retry then event = "retry"
    elseif kind == t.kinds.session_stats then event = "usage"
    elseif kind == t.kinds.turn_error or kind == t.kinds.chat_error then return nil
    elseif kind == t.kinds.chat_complete_result then
      event = body.finish_reason == "error" and "failed"
        or body.finish_reason == "interrupted" and "interrupted" or "completed"
    elseif kind == t.kinds.stream_end then return nil
    else return base_outbound(env) end

    local translated = copy(body)
    translated.kind = t.kinds.completion_event
    translated.request_id = request_id
    translated.chat_id = nil
    translated.event = event
    if kind == t.kinds.chat_complete_result then
      translated.result = copy(body.output)
      translated.output = nil
      translated.finish_reason = nil
      translated.error = translated.result.error or body.error
    end
    return translated
  end

  return t
end

return { translator = translator }
