-- starter/compositors/tools.lua — engine-side actors for the tools domain.
-- Exposes two spawn specs:
--
--   tools.gate_spec(gate_name, command)
--     Wraps the tool-gate Rust binary. Threads the plugin lib's
--     translation primitives with the dump-to-file swap and AGENTS.md
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
--   * On `<gate>.hello`: republish the hello.
--   * On `tool.result`: when the output exceeds the inline budget,
--     dump-to-file via the plugin lib's `maybe_dump_output` (full
--     payload to disk, summary in `body.output`), then republish the
--     (possibly rewritten) body.
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

  -- Per-envelope inbound logic. Pulled into a local so the batched
  -- from_plugin loop reads as a one-liner.
  local function handle_inbound(env)
    if env.type ~= "event" or type(env.body) ~= "table" then
      translator.publish(env.body, nil)
      return
    end

    -- Dump-to-file swap for outputs past the inline budget. Returns
    -- the original body when below budget or on dump failure. The
    -- (possibly rewritten) body is republished so consumers (the mag
    -- bridge, agentic-loop's transcript tap) see it.
    if translator.is_tool_result(env) then
      translator.publish(gate_lib.maybe_dump_output(env.body, nil), nil)
      return
    end

    -- Default (hello included): republish verbatim.
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
