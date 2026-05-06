-- starter/tool-gate/init.lua — wrapper actor for the tool-gate Rust
-- binary.
--
-- Constructor takes the gate name and the binary command (with the
-- caller-supplied --prompt / --default args). Returns an actor spec
-- ready for `actor.spawn(...)`.
--
-- ## Translation
--
-- Source: agentic_workflow.for_tool_gate (no to_plugin in the
-- original; only from_plugin).
--
--   * On first `<gate>.hello`: advertise the spawn_graph virtual tool.
--   * On `spawn-graph-tool.tool.invoke`: queue dispatch (D-31), emit
--     immediate ack, drop the gate-forwarded envelope before it
--     tries to deliver to a nonexistent peer (D-22).
--   * On `tool.result`: correlate by tool_id back to the
--     tool-executor pending entry, accumulate, emit graph.node_result
--     when all tool results land.

local envelope = require("lib.envelope")
local graph_lib = require("lib.graph")

local SPAWN_GRAPH_SOURCE = graph_lib.SPAWN_GRAPH_SOURCE

local M = {}

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

  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end

    -- Spawn-graph: advertise on first hello.
    if not advertised and env.body.kind == hello_kind then
      advertised = true
      envelope.emit(gate_name, graph_lib.advertise_body(gate_name))
      return env
    end

    -- Spawn-graph: intercept gate-forwarded tool invoke.
    if env.body.kind == invoke_kind then
      local name = env.body.name
      local invoke_id = env.body.id
      local args = env.body.args or {}
      if name ~= "spawn_graph" or type(invoke_id) ~= "string" then
        return nil
      end

      local run_id, err = al().queue_sub_graph(args, invoke_id)
      if not run_id then
        envelope.emit(nil, {
          kind  = "tool.result",
          id    = invoke_id,
          error = err or "spawn_graph: dispatch failed",
        })
        return nil
      end

      -- D-27: immediate ack so the model's wrap-firing-2 can start
      -- right away.
      envelope.emit(nil, {
        kind   = "tool.result",
        id     = invoke_id,
        output = "Submitted sub-graph run_id=" .. run_id ..
                 ". Acknowledge briefly to the user, or chain another " ..
                 "tool call. The real result will arrive later as a " ..
                 "user message tagged `[spawn_graph(run_id=" .. run_id ..
                 ") result]`.",
      })
      return nil
    end

    -- tool.result correlation for tool-executor firings.
    if env.body.kind == "tool.result" then
      local tool_id = env.body.id
      if type(tool_id) ~= "string" then return env end
      local ref, entry = al().take_pending_for_tool(tool_id)
      if not ref then return env end

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
        envelope.emit_broadcast({
          kind      = "graph.node_result",
          run_id    = entry.run_id,
          node_id   = entry.node_id,
          firing_id = entry.firing_id,
          output    = { tool_results = entry.tool_results },
        })
      end
      return env
    end

    return env
  end

  return {
    name        = gate_name,
    command     = command,
    from_plugin = from_plugin,
    receive_msg = function(_) end,
  }
end

return M
