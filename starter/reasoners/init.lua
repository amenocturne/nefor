-- starter/reasoners/init.lua — bundle entry for Lua-resident reasoners.
--
-- Each Lua-resident reasoner type (responder, terminal, tool-executor,
-- adapter, provider-wrapper, dummy) lives in a sibling folder with its
-- own actor spec. This module returns one combined actor that
-- dispatches incoming `<token>.run_node` envelopes to the right
-- handler — sourced from `agentic_workflow.lua`'s `handlers` table.
--
-- The agentic-loop actor is the consumer; the reasoners are the
-- producers (of node_result envelopes). They share orchestrator state
-- via `agentic-loop`'s exported helpers (track_provider_firing,
-- track_tool_executor, etc.), so this actor stays stateless.
--
-- Why bundled (not one actor per reasoner type): reasoner-graph
-- registers each type once via `reasoner-graph.register_reasoner`, and
-- the dispatch is a switch on the `<token>.run_node` envelope kind.
-- Six tiny actors with overlapping switch-on-kind logic adds noise
-- without buying separation.

local json = nefor.json

local envelope    = require("lib.envelope")
local ids         = require("lib.ids")

local emit           = envelope.emit
local emit_to        = envelope.emit_to
local emit_broadcast = envelope.emit_broadcast
local next_id        = envelope.next_id

local pending_key    = ids.pending_key

-- Forward-declare; populated after agentic_loop module is required.
local agentic_loop

-- ------------------------------------------------------------------
-- ack + node_result helpers
-- ------------------------------------------------------------------

local function send_ack(reasoner, run_id, firing_id)
  emit_broadcast({
    kind = reasoner .. ".run_node.ack",
    run_id = run_id,
    firing_id = firing_id,
  })
end

local function send_node_result_ok(run_id, node_id, firing_id, output, next_state)
  local body = {
    kind = "graph.node_result",
    run_id = run_id,
    node_id = node_id,
    firing_id = firing_id,
    output = output,
  }
  if next_state ~= nil then body.next_state = next_state end
  emit_broadcast(body)
end

local function send_node_result_err(run_id, node_id, firing_id, err)
  emit_broadcast({
    kind = "graph.node_result",
    run_id = run_id,
    node_id = node_id,
    firing_id = firing_id,
    error = tostring(err),
  })
end

-- ------------------------------------------------------------------
-- handler: dummy / provider-wrapper / responder
-- ------------------------------------------------------------------
--
-- Owns invariants:
--   * D-26 (sub-graph stream gating) — chat_id_stream_visible flag set
--     via agentic_loop.track_provider_firing.
--   * D-29 (responder tools=false)   — tools off for sub-graph responders.
--
-- Three-step chat_id precedence:
--   1. prev_state.chat_id (cyclic re-fire within one run).
--   2. args.seed_chat_id (cross-run bootstrap from chat orchestrator).
--   3. mint a fresh id.
local function provider_run_node(reasoner_type, body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local args = body.args or {}
  local inputs = body.inputs or {}
  local prev_state = body.prev_state

  local cfg = agentic_loop.config()
  local provider = (type(args) == "table" and args.provider) or cfg.provider
  if type(provider) ~= "string" or provider == "" then
    return "no provider configured (set args.provider or config.provider)"
  end

  local chat_id
  local need_create = false
  local chat_id_source
  if type(prev_state) == "table" and type(prev_state.chat_id) == "string" then
    chat_id = prev_state.chat_id
    chat_id_source = "prev_state"
  elseif type(args) == "table" and type(args.seed_chat_id) == "string" then
    chat_id = args.seed_chat_id
    chat_id_source = "seed"
  else
    chat_id = next_id("chat")
    need_create = true
    chat_id_source = "fresh"
  end

  nefor.log.info("reasoners.provider_run_node: chat_id resolved", {
    reasoner = reasoner_type,
    run_id = run_id, node_id = node_id, firing_id = firing_id,
    provider = provider, chat_id = chat_id, source = chat_id_source,
    need_create = need_create,
    has_args_prompt = type(args) == "table" and type(args.prompt) == "string" and #args.prompt or 0,
    has_args_system = type(args) == "table" and type(args.system) == "string" and #args.system or 0,
  })

  agentic_loop.track_provider_firing(reasoner_type, run_id, node_id, firing_id, provider, chat_id)

  if need_create then
    local create_body = { kind = provider .. ".chat.create", chat_id = chat_id }
    local model = (type(args) == "table" and args.model) or cfg.model
    if type(model) == "string" and #model > 0 then
      create_body.model = model
    end
    -- D-29: sub-graph responder nodes must produce text, not tool calls.
    if reasoner_type == "responder" then
      create_body.tools = false
    end
    emit_to(provider, create_body)
  end

  if need_create then
    if type(args) == "table" and type(args.system) == "string" and #args.system > 0 then
      emit_to(provider, {
        kind    = provider .. ".chat.append",
        chat_id = chat_id,
        message = { role = "system", content = args.system },
      })
    end
  end

  for dep_id, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      local out = dep_entry.output
      if type(out) == "table" and type(out.messages) == "table" then
        for _, msg in ipairs(out.messages) do
          emit_to(provider, {
            kind    = provider .. ".chat.append",
            chat_id = chat_id,
            message = msg,
          })
        end
      elseif type(out) == "table" and type(out.text) == "string" then
        emit_to(provider, {
          kind    = provider .. ".chat.append",
          chat_id = chat_id,
          message = { role = "user", content = out.text },
        })
      elseif type(out) == "string" then
        emit_to(provider, {
          kind    = provider .. ".chat.append",
          chat_id = chat_id,
          message = { role = "user", content = out },
        })
      end
    end
  end

  -- prev_state on first firing arrives as serde_json `null`, decoded
  -- by mlua to a NULL sentinel (lightuserdata) — NOT Lua nil. Test
  -- the positive shape: cycle re-fires set prev_state to a table;
  -- anything else means first firing.
  local first_firing = (type(prev_state) ~= "table")
  if first_firing then
    local prompt = (type(args) == "table" and type(args.prompt) == "string") and args.prompt or ""
    if #prompt > 0 then
      emit_to(provider, {
        kind    = provider .. ".chat.append",
        chat_id = chat_id,
        message = { role = "user", content = prompt },
      })
    end
  end

  emit_to(provider, { kind = provider .. ".chat.complete", chat_id = chat_id })
  return nil
end

-- ------------------------------------------------------------------
-- handler: tool-executor
-- ------------------------------------------------------------------

local function tool_executor_run_node(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  local calls
  for _, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      local out = dep_entry.output
      if type(out) == "table" then
        if type(out.tool_calls) == "table" then
          calls = out.tool_calls
          break
        elseif #out > 0 then
          calls = out
          break
        end
      end
    end
  end

  if type(calls) ~= "table" or #calls == 0 then
    return "tool-executor received no tool calls in inputs"
  end

  local tool_ids = {}
  for i = 1, #calls do
    tool_ids[i] = next_id("tool")
  end

  agentic_loop.track_tool_executor(run_id, node_id, firing_id, calls, tool_ids)

  for i, call in ipairs(calls) do
    local tool_id = tool_ids[i]
    local call_name = (type(call) == "table" and (call.name or call.tool)) or ""
    local call_args = (type(call) == "table" and (call.arguments or call.args)) or {}
    local model_call_id = (type(call) == "table" and call.id) or tool_id
    agentic_loop.fire_tool_start_observers(model_call_id, call_name, call_args)
    emit_to("nefor-tui", {
      kind  = "chat.tool.start",
      id    = model_call_id,
      name  = call_name,
      input = call_args,
    })
    emit_to("tool-gate", {
      kind = "tool-gate.tool.invoke",
      id   = tool_id,
      name = call_name,
      args = call_args,
    })
  end
  return nil
end

-- ------------------------------------------------------------------
-- handler: adapter (pure Lua; ToolResults → ProviderIn)
-- ------------------------------------------------------------------

local function adapter_run_node(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  local results
  for _, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and type(dep_entry.output) == "table" then
      if type(dep_entry.output.tool_results) == "table" then
        results = dep_entry.output.tool_results
        break
      end
    end
  end

  local messages = {}
  if type(results) == "table" then
    for _, r in ipairs(results) do
      local content
      if type(r.output) == "string" then
        content = r.output
      elseif r.output ~= nil then
        content = json.encode(r.output)
      elseif type(r.error) == "string" then
        content = "[tool error] " .. r.error
      else
        content = ""
      end
      messages[#messages + 1] = {
        role         = "tool",
        content      = content,
        tool_call_id = r.id,
      }
    end
  end

  send_ack("adapter", run_id, firing_id)
  send_node_result_ok(run_id, node_id, firing_id, { messages = messages }, nil)
  return "_already_replied"
end

-- ------------------------------------------------------------------
-- handler: terminal (D-30 sorted-id concat)
-- ------------------------------------------------------------------

local function terminal_run_node(body)
  local run_id = body.run_id
  local node_id = body.node_id
  local firing_id = body.firing_id
  local inputs = body.inputs or {}

  local ordered_ids = {}
  for upstream_id, dep_entry in pairs(inputs) do
    if type(dep_entry) == "table" and dep_entry.output ~= nil then
      ordered_ids[#ordered_ids + 1] = upstream_id
    end
  end
  table.sort(ordered_ids)

  local final
  if #ordered_ids == 0 then
    final = { text = "" }
  elseif #ordered_ids == 1 then
    final = inputs[ordered_ids[1]].output
  else
    local parts = {}
    for _, uid in ipairs(ordered_ids) do
      local out = inputs[uid].output
      local txt = (type(out) == "table" and out.text) or ""
      parts[#parts + 1] = "## " .. tostring(uid) .. "\n" .. tostring(txt)
    end
    final = { text = table.concat(parts, "\n\n") }
  end

  send_ack("terminal", run_id, firing_id)
  send_node_result_ok(run_id, node_id, firing_id, final, nil)
  return "_already_replied"
end

-- ------------------------------------------------------------------
-- handlers registry
-- ------------------------------------------------------------------

local handlers = {
  ["dummy"]            = function(body) return provider_run_node("dummy", body) end,
  ["provider-wrapper"] = function(body) return provider_run_node("provider-wrapper", body) end,
  ["responder"]        = function(body) return provider_run_node("responder", body) end,
  ["tool-executor"]    = tool_executor_run_node,
  ["adapter"]          = adapter_run_node,
  ["terminal"]         = terminal_run_node,
}

local function lua_resident_types()
  local out = {}
  for name, _ in pairs(handlers) do out[#out + 1] = name end
  return out
end

local function emit_register_reasoner(name)
  local payload = json.encode({
    type = "event",
    from = "engine",
    ts   = nefor.engine.now(),
    body = {
      kind = "reasoner-graph.register_reasoner",
      name = name,
    },
  })
  nefor.engine.send(payload, "reasoner-graph")
end

-- ------------------------------------------------------------------
-- receive_msg — react to <token>.run_node + reasoner-graph.ready
-- ------------------------------------------------------------------

local registered = false

local function receive_msg(entry)
  -- Skip per-peer broadcast fan-out entries (see agentic-loop's
  -- receive_msg for the rationale — this same filter lives there too).
  -- Without this, a single reasoner-graph dispatch like
  -- `provider-wrapper.run_node` runs the handler N times (once per
  -- peer the broker fans the broadcast out to), minting N chat_ids
  -- and ringing the provider N times.
  if entry.origin == "step" and entry.target ~= nil then return end

  -- Lazy bind agentic-loop on first dispatch — both modules are
  -- required from init.lua and would otherwise form a require cycle
  -- (agentic-loop imports lib/, reasoners imports agentic-loop).
  if agentic_loop == nil then
    agentic_loop = require("agentic-loop")
  end

  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local body = decoded.body
  local kind = body.kind
  if type(kind) ~= "string" then return end

  -- Skip during replay — graph already ran; re-dispatch would
  -- duplicate every side effect.
  if agentic_loop.is_replay_mode() then return end

  -- (1) Register Lua-resident types on first reasoner-graph.ready.
  if not registered and kind == "reasoner-graph.ready" then
    for _, name in ipairs(lua_resident_types()) do
      emit_register_reasoner(name)
    end
    registered = true
    return
  end

  -- (2) Dispatch <token>.run_node.
  local token = kind:match("^([^.]+)%.run_node$")
  if token then
    local handler = handlers[token]
    if not handler then
      send_ack(token, body.run_id, body.firing_id)
      send_node_result_err(body.run_id, body.node_id, body.firing_id,
        "no Lua adapter for reasoner type `" .. token .. "`")
      return
    end

    local err = handler(body)
    if err == "_already_replied" then return end
    if err ~= nil then
      send_ack(token, body.run_id, body.firing_id)
      send_node_result_err(body.run_id, body.node_id, body.firing_id, err)
      return
    end
    send_ack(token, body.run_id, body.firing_id)
  end
end

return {
  name        = "reasoners",
  receive_msg = receive_msg,
  send_msg    = function(_) end,
  -- Test-only escape hatch: re-arm registration so test cases can
  -- replay the boot dance.
  _internals = {
    reset = function() registered = false end,
    handlers = handlers,
  },
}
