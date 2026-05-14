-- starter/tool-gate/init.lua — wrapper actor for the tool-gate Rust
-- binary.
--
-- Constructor takes the gate name and the binary command (with the
-- caller-supplied --prompt / --default args).
--
-- ## from_plugin (binary → bus)
--
--   * On first `<gate>.hello`: advertise the spawn_graph virtual tool
--     and republish the hello envelope.
--   * On `spawn-graph-tool.tool.invoke`: queue dispatch (D-31), emit
--     immediate ack, drop the gate-forwarded envelope before it
--     tries to deliver to a nonexistent peer (D-22).
--   * On `tool.result`: correlate by tool_id back to the
--     tool-executor pending entry, accumulate, emit a closing
--     tool.result { id=firing_id } when all tool results land. The
--     original `tool.result` is also republished so other consumers
--     (provider wrappers tracking their own firings, agentic-loop)
--     see it.
--   * Otherwise: republish verbatim.
--
-- ## to_plugin (bus → binary)
--
-- No translation — deliver verbatim, skipping replay-window
-- envelopes and self-emissions. The original wrapper had no
-- `to_plugin` (default identity); we install one explicitly so the
-- replay-window guard and the self-skip live in this module.

local json = nefor.json

local envelope = require("lib.envelope")
local graph_lib = require("lib.graph")

local SPAWN_GRAPH_SOURCE = graph_lib.SPAWN_GRAPH_SOURCE

local M = {}

local function publish(from, body)
  nefor.engine.send(json.encode({
    type = "event",
    from = from,
    ts   = nefor.engine.now(),
    body = body,
  }))
end

function M.spawn_spec(gate_name, command)
  gate_name = gate_name or "tool-gate"
  assert(type(command) == "table", "tool-gate.spawn_spec: command must be a table")

  local hello_kind = gate_name .. ".hello"
  local invoke_kind = SPAWN_GRAPH_SOURCE .. ".tool.invoke"

  local advertised = false
  local agentic_loop  -- bound lazily

  local function al()
    if agentic_loop == nil then
      agentic_loop = require("agentic-loop")
    end
    return agentic_loop
  end

  -- Per-envelope inbound logic — kept as a local so the batched
  -- `from_plugin(envs)` callback can iterate without re-indenting the
  -- decision tree.
  local function handle_inbound(env)
    if env.type ~= "event" or type(env.body) ~= "table" then
      -- Republish verbatim non-event envelopes (defensive — events are
      -- the only shape we should see here per NCP).
      publish(env.from or gate_name, env.body)
      return
    end

    -- Spawn-graph: advertise on first hello.
    if not advertised and env.body.kind == hello_kind then
      advertised = true
      envelope.emit(gate_name, graph_lib.advertise_body(gate_name))
      publish(env.from or gate_name, env.body)
      return
    end

    -- Spawn-graph: intercept gate-forwarded tool invoke.
    if env.body.kind == invoke_kind then
      local name = env.body.name
      local invoke_id = env.body.id
      local args = env.body.args or {}
      if name ~= "spawn_graph" or type(invoke_id) ~= "string" then
        -- Drop the malformed envelope; nothing to forward.
        return
      end

      local run_id, err = al().queue_sub_graph(args, invoke_id)
      if not run_id then
        envelope.emit(nil, {
          kind  = "tool.result",
          id    = invoke_id,
          error = err or "spawn_graph: dispatch failed",
        })
        return
      end

      envelope.emit(nil, {
        kind   = "tool.result",
        id     = invoke_id,
        output = "Submitted sub-graph run_id=" .. run_id ..
                 ". Acknowledge briefly to the user, or chain another " ..
                 "tool call. The real result will arrive later as a " ..
                 "user message tagged `[spawn_graph(run_id=" .. run_id ..
                 ") result]`.",
      })
      return
    end

    -- tool.result correlation for tool-executor firings.
    if env.body.kind == "tool.result" then
      local tool_id = env.body.id
      if type(tool_id) == "string" then
        local ref, entry = al().take_pending_for_tool(tool_id)
        if ref then
          local model_call_id = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].id) or tool_id
          entry.tool_results[ref.idx] = {
            id     = model_call_id,
            name   = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].name) or "",
            output = env.body.output,
            error  = env.body.error,
          }
          local raw_err = env.body.error
          local err_bool = raw_err == true
              or (type(raw_err) == "string" and raw_err ~= "")
          local out_str = type(env.body.output) == "string" and env.body.output or ""
          al().fire_tool_end_observers(model_call_id, out_str, err_bool)
          envelope.emit_to("nefor-tui", {
            kind   = "chat.tool.end",
            id     = model_call_id,
            output = out_str,
            error  = err_bool,
          })
          entry.pending_count = entry.pending_count - 1

          if entry.pending_count == 0 then
            al().clear_pending_key(ref.key)
            envelope.emit_as("tool-executor", nil, {
              kind   = "tool.result",
              id     = entry.firing_id,
              result = { tool_results = entry.tool_results },
            })
          end
        end
      end
      -- Always republish the original tool.result so other consumers
      -- see it (the wrapper consumes only its bookkeeping side).
      publish(env.from or gate_name, env.body)
      return
    end

    -- Default: republish verbatim.
    publish(env.from or gate_name, env.body)
  end

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      handle_inbound(env)
    end
  end

  -- to_plugin: deliver verbatim, skip during replay + self-emissions.
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if not env.replay and env.from ~= gate_name then
        -- Strip framework-only fields (`replay`, …) when encoding for
        -- the wire; the protocol parser rejects unknown envelope
        -- fields.
        nefor.engine.deliver(gate_name, json.encode({
          type = env.type,
          from = env.from,
          ts   = env.ts,
          body = env.body,
        }))
      end
    end
  end

  return {
    name        = gate_name,
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

return M
