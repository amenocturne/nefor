-- starter/reasoners/init.lua — bundle entry for Lua-resident reasoners.
--
-- Each Lua-resident reasoner type (responder, terminal, tool-executor,
-- adapter, provider-wrapper, dummy) lives behind a single dispatcher
-- actor. Inbound dispatch comes in as the canonical tool contract:
-- `tool.invoke { id=firing_id, name=<reasoner>, args: { run_id, node_id,
-- args, inputs, prev_state } }`. The handler chain is the same as the
-- prior `<token>.run_node` dispatch — args.* extraction is the only
-- shape change.
--
-- Replies go out as `tool.result { id=firing_id, result | error }`.
-- For reasoners that thread state across cycle re-firings, `next_state`
-- lives INSIDE `result` (the Rust scheduler's `synthesize_node_result`
-- extracts it from there per the wire-protocol spec coordination
-- point 1).
--
-- ## Peer tracking
--
-- The reasoner-graph Rust binary maintains a peer-set keyed by `from`
-- on inbound envelopes. Before dispatching a `tool.invoke { name=X }`,
-- the scheduler checks whether X is in the peer set; if not the firing
-- synth-fails with `reasoner '<X>' not connected`. Lua-resident
-- reasoners don't run as separate processes, so we make them visible to
-- the peer-set by emitting one no-op `<name>.ready` envelope per
-- reasoner, with `from = <name>`, on the very first
-- `reasoner-graph.ready`. This is purely a peer-set seed — the kind
-- string itself is never consumed by anyone.

local json = nefor.json

local envelope    = require("lib.envelope")
local ids         = require("lib.ids")

local emit_as        = envelope.emit_as
local emit_to        = envelope.emit_to
local next_id        = envelope.next_id

-- Forward-declare; populated after agentic_loop module is required.
local agentic_loop

-- ------------------------------------------------------------------
-- tool.result helpers
-- ------------------------------------------------------------------

local function send_tool_result_ok(reasoner_type, firing_id, output, next_state)
  local result = {}
  if type(output) == "table" then
    for k, v in pairs(output) do result[k] = v end
  else
    result.value = output
  end
  if next_state ~= nil then result.next_state = next_state end
  emit_as(reasoner_type, nil, {
    kind   = "tool.result",
    id     = firing_id,
    result = result,
  })
end

local function send_tool_result_err(reasoner_type, firing_id, err)
  emit_as(reasoner_type, nil, {
    kind  = "tool.result",
    id    = firing_id,
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

  send_tool_result_ok("adapter", firing_id, { messages = messages }, nil)
  return "_already_replied"
end

-- ------------------------------------------------------------------
-- handler: terminal (D-30 sorted-id concat)
-- ------------------------------------------------------------------

local function terminal_run_node(body)
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

  send_tool_result_ok("terminal", firing_id, final, nil)
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

-- Peer-set seed: emit one envelope per reasoner type with `from = <name>`
-- so the reasoner-graph binary's `track_peer` records each as a connected
-- peer. The kind itself is decorative; only `from` is read.
local function seed_peer_set()
  for _, name in ipairs(lua_resident_types()) do
    emit_as(name, nil, { kind = name .. ".ready" })
  end
end

-- ------------------------------------------------------------------
-- receive_msg — react to tool.invoke { name in handlers } +
-- reasoner-graph.ready
-- ------------------------------------------------------------------

local registered = false

-- Extract the dispatch body from a tool.invoke envelope. The Rust
-- scheduler packs `{ run_id, node_id, args, inputs, prev_state }` into
-- `body.args`; firing_id is `body.id`. Flatten back into the shape the
-- per-handler functions expect (firing_id at root alongside the args
-- subobject keys).
local function unwrap_invoke_body(invoke)
  local args = invoke.args or {}
  return {
    run_id     = args.run_id,
    node_id    = args.node_id,
    firing_id  = invoke.id,
    args       = args.args,
    inputs     = args.inputs,
    prev_state = args.prev_state,
  }
end

local function receive_msg(entry)
  -- Skip per-peer broadcast fan-out entries (see agentic-loop's
  -- receive_msg for the rationale — this same filter lives there too).
  -- Without this, a single reasoner-graph dispatch like a tool.invoke
  -- runs the handler N times (once per peer the broker fans the
  -- broadcast out to), minting N chat_ids and ringing the provider N
  -- times.
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

  -- (1) Seed peer-set on first reasoner-graph.ready.
  if not registered and kind == "reasoner-graph.ready" then
    seed_peer_set()
    registered = true
    return
  end

  -- (2) Dispatch tool.invoke { name in handlers }. Anything else on the
  -- bus is for someone else (real tools, spawn_graph routed to
  -- reasoner-graph itself, etc.) — ignore silently.
  if kind ~= "tool.invoke" then return end
  local name = body.name
  if type(name) ~= "string" then return end
  local handler = handlers[name]
  if handler == nil then return end

  local unwrapped = unwrap_invoke_body(body)
  local err = handler(unwrapped)
  if err == "_already_replied" then return end
  if err ~= nil then
    send_tool_result_err(name, unwrapped.firing_id, err)
    return
  end
  -- Provider-firing handlers return nil — the response comes later via
  -- the provider wrapper's tool.result emission keyed off chat_id.
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
