-- starter/compositors/tools.lua — engine-side actors for the tools domain.
-- Exposes two spawn specs:
--
--   tools.gate_spec(gate_name, command)
--     Wraps the tool-gate Rust binary. Threads the plugin lib's
--     translation primitives with starter-owned agentic-loop state for
--     tool-executor firings, spawn_graph virtual tool, and AGENTS.md
--     emission ordering.
--
--   tools.basic_actor_spec
--     Default actor spec for the basic-tools Rust binary. Sources the
--     plugin lib directly — no orchestrator coupling.

local json = nefor.json

local actor        = require("core.actor")
local envelope     = require("core.envelope")
local gate_lib     = require("tool-gate")
local chat_emitter = require("libs.chat-emitter")

local M = {}

-- ## from_plugin (binary → bus)
--
--   * On first `<gate>.hello`: publish the spawn_graph virtual-tool
--     advertise envelope and republish the hello.
--   * On `spawn-graph-tool.tool.invoke`: parse the invoke, queue the
--     sub-graph through agentic-loop, emit the synthesised ack
--     `tool.result`, and DROP the gate-forwarded envelope before
--     targeting tries to deliver to a non-existent peer.
--   * On `tool.result`: when the output exceeds the inline budget,
--     dump-to-file via the plugin lib's `maybe_dump_output` (full
--     payload to disk, summary in `body.output`). Then correlate the
--     tool_id to the tool-executor pending entry; on a hit, decrement
--     the firing's pending count, fire tool-end observers, emit the
--     chat-side `chat.tool.end`, and (when all results arrived) the
--     closing `tool.result { id = firing_id }`. The original
--     tool.result is republished so other consumers see it.
--   * Otherwise: republish verbatim.
--
-- ## to_plugin (bus → binary)
--
--   * Skip self-emissions and envelopes flagged `env.replay`.
--   * On private `<gate>.tools.advertise`: record internal tool context
--     metadata before forwarding it to the binary.
--   * On outbound `<gate>.tool.invoke`: derive normalized folders via
--     the plugin lib's context registry and emit any not-yet-shown
--     instruction-file reminders BEFORE the invoke is forwarded to the
--     binary (ordering is load-bearing for chat history).
--   * Then forward the envelope verbatim to the binary's stdin.
function M.gate_spec(gate_name, command)
  gate_name = gate_name or "tool-gate"
  if type(command) ~= "table" then
    error("tools.gate_spec: command must be a table, got " .. type(command))
  end

  local translator = gate_lib.translator(gate_name)
  local advertised = false
  local agentic_loop  -- bound lazily

  local function al()
    if agentic_loop == nil then
      agentic_loop = require("agentic-loop")
    end
    return agentic_loop
  end

  -- Per-envelope inbound logic. Pulled into a local so the batched
  -- from_plugin loop reads as a one-liner.
  local function handle_inbound(env)
    if env.type ~= "event" or type(env.body) ~= "table" then
      translator.publish(env.body, nil)
      return
    end

    -- First hello: advertise the spawn_graph virtual tool. The
    -- advertise envelope is targeted at the gate's stdin (peer
    -- delivery, from=engine) so the binary registers spawn_graph as
    -- one of its known tools. Then republish the hello to the bus.
    if not advertised and translator.is_hello(env) then
      advertised = true
      translator.emit(gate_name, translator.advertise_body())
      translator.publish(env.body, nil)
      return
    end

    -- Spawn-graph: intercept the gate-forwarded invoke and queue a
    -- sub-graph through agentic-loop. The orchestrator coupling lives
    -- here in starter code; the plugin lib only parses + builds bodies.
    if translator.is_spawn_graph_invoke(env) then
      local parsed, parse_err = gate_lib.parse_spawn_graph_invoke(env.body)
      if not parsed then
        -- Best-effort invoke_id surfacing: the parse failure may have
        -- been about a missing/non-string `id`, in which case there's
        -- no canonical tool_id to bind the error result to. We still
        -- emit so callers see a structured failure rather than silent
        -- swallowing; downstream consumers gate on type(body.id) before
        -- correlating.
        local invoke_id = type(env.body) == "table" and env.body.id or nil
        translator.emit(nil, gate_lib.spawn_graph_error_body(invoke_id, parse_err))
        return
      end
      local run_id, err = al().queue_sub_graph(parsed.args, parsed.invoke_id)
      if not run_id then
        translator.emit(nil, gate_lib.spawn_graph_error_body(parsed.invoke_id, err))
        return
      end
      translator.emit(nil, gate_lib.spawn_graph_ack_body(parsed.invoke_id, run_id))
      return
    end

    -- tool.result correlation for tool-executor firings.
    if translator.is_tool_result(env) then
      -- Dump-to-file swap for outputs past the inline budget. Returns
      -- the original body when below budget or on dump failure.
      local body = gate_lib.maybe_dump_output(env.body, nil)
      local tool_id = body.id
      if type(tool_id) == "string" then
        local ref, entry = al().take_pending_for_tool(tool_id)
        if ref then
          local model_call_id =
            (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].id)
            or tool_id
          local payload_output, err_bool = gate_lib.tool_result_payload(body)
          entry.tool_results[ref.idx] = {
            id     = model_call_id,
            name   = (entry.tool_calls[ref.idx] and entry.tool_calls[ref.idx].name) or "",
            output = body.output,
            error  = body.error,
          }
          al().fire_tool_end_observers(model_call_id, payload_output, err_bool)
          envelope.emit("nefor-tui", {
            kind   = "chat.tool.end",
            id     = model_call_id,
            output = payload_output,
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
      -- Always republish the original (possibly dump-rewritten) body
      -- so other consumers see it.
      translator.publish(body, nil)
      return
    end

    -- Default: republish verbatim.
    translator.publish(env.body, nil)
  end

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      handle_inbound(env)
    end
  end

  -- to_plugin: AGENTS.md hook before forwarding; skip replay +
  -- self-emissions; framework-only fields (e.g. `replay`) stripped
  -- before json-encoding because the protocol parser rejects them.
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if not env.replay and env.from ~= gate_name then
        if type(env.body) == "table"
            and env.body.kind == translator.kinds.tool_advertise then
          gate_lib.record_tool_contexts_from_advertise(env.body)
        end
        local invoke_chat_id = type(env.body) == "table" and env.body.chat_id or nil
        local emitter = chat_emitter.scoped(
          invoke_chat_id,
          function(body) envelope.emit(nil, body) end
        )
        gate_lib.agents_md_emit_for_invoke(translator, env, emitter)
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

-- Default actor spec for the basic-tools Rust binary. Tools register
-- against tool-gate; no orchestrator coupling at this layer. The
-- binary speaks the canonical tool contract directly (no translation
-- needed), so the spec is the generic identity-passthrough shape from
-- core.actor.
function M.basic_actor_spec()
  local config = require("config")
  return actor.identity_spec("basic-tools", {
    config.bin("basic-tools"),
    "--gate", "tool-gate",
  })
end

return M
