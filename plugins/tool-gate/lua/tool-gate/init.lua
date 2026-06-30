-- Plugin lib for the tool-gate binary. Translation primitives + two
-- side-effect bridges (huge-output dump-to-file, instruction reminders).

local envelope         = require("core.envelope")
local spawn_graph      = require("libs.spawn-graph")
local tool_output_dump = require("tool-gate.tool_output_dump")
local agents_md        = require("tool-gate.agents_md")

local M = {}

---@param body table|nil
---@return boolean
local function is_tool_result(body)
  return type(body) == "table" and body.kind == "tool.result"
end

-- Build a translator scoped to a particular gate name.
-- `kinds` exposes the canonical kind strings so callers don't re-derive them.
---@param gate_name string
function M.translator(gate_name)
  assert(type(gate_name) == "string" and #gate_name > 0,
    "tool-gate.translator: gate_name required")

  local hello_kind  = gate_name .. ".hello"
  local invoke_kind = spawn_graph.SPAWN_GRAPH_SOURCE .. ".tool.invoke"
  local outbound_invoke_kind = gate_name .. ".tool.invoke"

  local t = {
    gate_name = gate_name,
    kinds = {
      hello                  = hello_kind,
      spawn_graph_invoke     = invoke_kind,
      outbound_tool_invoke   = outbound_invoke_kind,
      tool_result            = "tool.result",
      tool_advertise         = gate_name .. ".tools.advertise",
    },
  }

  function t.is_hello(env)
    return type(env) == "table"
      and env.type == "event"
      and type(env.body) == "table"
      and env.body.kind == hello_kind
  end

  function t.is_spawn_graph_invoke(env)
    return type(env) == "table"
      and env.type == "event"
      and type(env.body) == "table"
      and env.body.kind == invoke_kind
  end

  function t.is_tool_result(env)
    return type(env) == "table"
      and env.type == "event"
      and is_tool_result(env.body)
  end

  function t.is_outbound_tool_invoke(env)
    return type(env) == "table"
      and env.type == "event"
      and type(env.body) == "table"
      and env.body.kind == outbound_invoke_kind
  end

  function t.advertise_body()
    return spawn_graph.advertise_body(gate_name)
  end

  -- Publish under the gate's identity. Thin wrapper so callers don't
  -- import envelope themselves.
  function t.publish(body, target)
    envelope.emit_as(gate_name, target, body)
  end

  -- Deliver under engine identity to a specific peer.
  function t.emit(target, body)
    envelope.emit(target, body)
  end

  -- Emit under an arbitrary `from`. Used for the closing
  -- `tool.result { id = firing_id }` that must look like it came from
  -- the tool-executor reasoner.
  function t.emit_as(from, target, body)
    envelope.emit_as(from, target, body)
  end

  return t
end

---@class ParsedSpawnGraphInvoke
---@field name string
---@field invoke_id string
---@field args table

---@param body table
---@return ParsedSpawnGraphInvoke|nil parsed
---@return string|nil err
function M.parse_spawn_graph_invoke(body)
  if type(body) ~= "table" then
    return nil, "body not a table"
  end
  local name = body.name
  local invoke_id = body.id
  if name ~= "spawn_graph" then
    return nil, "not a spawn_graph invoke (name=" .. tostring(name) .. ")"
  end
  if type(invoke_id) ~= "string" then
    return nil, "missing or non-string id"
  end
  -- Empty args is legitimate (binary may forward an invoke whose args
  -- was an empty object); downstream queue_sub_graph needs a table.
  local args = body.args
  if args == nil then args = {} end
  if type(args) ~= "table" then
    local ok, encoded = pcall(nefor.json.encode, args)
    local raw = ok and encoded or tostring(args)
    return nil, "spawn_graph: args must be a JSON object; got " ..
      type(args) .. ". Raw args: " .. raw
  end
  return { name = name, invoke_id = invoke_id, args = args }, nil
end

-- Tool.result body the model sees after queueing a sub-graph. The
-- `[spawn_graph(run_id=…) result]` tag is load-bearing: the model
-- recognises it when the real result lands later as a user message.
---@param invoke_id string
---@param run_id string
---@return table
function M.spawn_graph_ack_body(invoke_id, run_id)
  return {
    kind   = "tool.result",
    id     = invoke_id,
    output = "Submitted sub-graph run_id=" .. run_id ..
             ". Acknowledge briefly to the user, or chain another " ..
             "tool call. The real result will arrive later as a " ..
             "user message tagged `[spawn_graph(run_id=" .. run_id ..
             ") result]`.",
  }
end

---@param invoke_id string
---@param err string|nil
---@return table
function M.spawn_graph_error_body(invoke_id, err)
  return {
    kind  = "tool.result",
    id    = invoke_id,
    error = err or "spawn_graph: dispatch failed",
  }
end

-- Swap huge tool.result outputs for an on-disk dump + summary string.
-- Below budget or on bodies without a string id: return body unchanged.
-- On dump failure: log warn and forward the original output.
-- Mutates a shallow copy so the caller's envelope isn't aliased.
---@param body table
---@param chat_id string|nil
---@return table rewritten_body
---@return string|nil dump_path
function M.maybe_dump_output(body, chat_id)
  if not is_tool_result(body) then return body, nil end
  if type(body.id) ~= "string" then return body, nil end
  if not tool_output_dump.should_dump(body.output) then
    return body, nil
  end

  local summary, path, dump_err = tool_output_dump.dump(
    chat_id, body.id, body.output,
    { tool = body.name }
  )
  if summary then
    nefor.log.info("tool-gate: dumped huge tool output to file", {
      tool_id = body.id, path = path, bytes = #summary,
    })
    local rewritten = {}
    for k, v in pairs(body) do rewritten[k] = v end
    rewritten.output = summary
    return rewritten, path
  end

  nefor.log.warn("tool-gate: dump failed; forwarding original output", {
    tool_id = body.id, error = dump_err,
  })
  return body, nil
end

-- Chat-side payload from a tool.result body. When the result carries
-- an error string we surface it as the rendered output so the chat
-- block shows WHY the call errored ("denied by gate policy", "unknown
-- tool", …) instead of an empty line.
---@param body table
---@return string payload_output
---@return boolean err_bool
function M.tool_result_payload(body)
  local raw_err = body.error
  local err_bool = raw_err == true
    or (type(raw_err) == "string" and raw_err ~= "")
  local payload_output
  if type(body.output) == "string" then
    payload_output = tool_output_dump.image_media_summary(body.output) or body.output
  elseif type(body.output) == "table" and type(body.output.text) == "string" then
    payload_output = body.output.text
  elseif type(body.output) == "table"
      and body.output.type == "media"
      and type(body.output.media_type) == "string"
      and body.output.media_type:match("^image/") then
    payload_output = tool_output_dump.image_media_summary(body.output)
  elseif type(raw_err) == "string" then
    payload_output = raw_err
  else
    payload_output = ""
  end
  return payload_output, err_bool
end

-- Record internal context metadata from private tools.advertise bodies.
-- The model-facing tool.register strips this field; wrappers use it to
-- derive normalized tool-call locality without knowing per-tool args.
---@param body table|nil
---@return integer count
function M.record_tool_contexts_from_advertise(body)
  local ok, count_or_err = pcall(
    agents_md.record_tool_contexts_from_advertise,
    body
  )
  if not ok then
    nefor.log.warn("tool-gate: context advertise parse errored", {
      error = tostring(count_or_err),
    })
    return 0
  end
  return count_or_err
end

-- Instruction reminder side-effect bridge: on a folder-touching outbound
-- tool.invoke, emit a low-authority reminder listing available instruction
-- files. Contents are not loaded. Discovery primitives live in
-- libs.instruction-files; tool-gate.agents_md keeps this bridge's state
-- behind the historical import path. pcall-guarded so a transient filesystem
-- failure doesn't crash the caller; returns the count emitted (0 on no-op or
-- failure).
--
-- `emitter` is a scoped chat-emitter (libs.chat-emitter) that already
-- carries the correct chat_id — callers never thread chat_id manually.
---@param translator table
---@param env table
---@param emitter table  -- chat-emitter scoped instance
---@return integer count
function M.agents_md_emit_for_invoke(translator, env, emitter)
  if not translator.is_outbound_tool_invoke(env) then return 0 end
  local body = env.body
  local ok, count_or_err = pcall(
    agents_md.emit_reminders_for_tool_call,
    body.name, body.args, emitter
  )
  if not ok then
    nefor.log.warn("tool-gate: agents_md.emit_reminders_for_tool_call errored", {
      tool = body.name, error = tostring(count_or_err),
    })
    return 0
  end
  return count_or_err
end

return M
