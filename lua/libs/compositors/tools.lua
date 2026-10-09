-- lua/libs/compositors/tools.lua — engine-side actors for the tools domain.
-- Exposes two spawn specs:
--
--   tools.gate_spec(gate_name, command)
--     Wraps the tool-gate Rust binary. Threads the plugin lib's
--     translation primitives with the dump-to-file swap and AGENTS.md
--     emission ordering.
--
--   tools.capability_tools_spec { name, command, gate, tools }
--     Wraps a capability-provider binary as a tool source: Lua tool
--     definitions (`libs.tool`) are advertised to the gate under the actor
--     name, invocations are validated in Lua, and handlers reach the binary
--     through `<name>.capability.invoke` / `<name>.capability.cancel`.
--
--   tools.basic_actor_spec { max_read_bytes = N, gate = "tool-gate" }
--     The basic-tools capability binary with the `libs.basic-tools` tools.
--     The composition must supply the read_file maximum explicitly.
--
--   tools.git_worktree_actor_spec { gate = "tool-gate" }
--     The git-worktree capability binary with the `libs.git-worktree` tools,
--     used by MAG's worktree factories.

local json = nefor.json

local actor        = require("core.actor")
local envelope     = require("core.envelope")
local gate_lib         = require("tool-gate")
local tool             = require("libs.tool")
local chat_emitter     = require("libs.chat-emitter")
local mag_run_bindings = require("libs.mag-run-bindings")

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
--     dedicated instruction notices before forwarding. Authoritative
--     structured MAG provenance is mandatory; invalid scope logs a
--     non-content warning and suppresses only the notice.
--   * Then forward the envelope verbatim to the binary's stdin.
function M.gate_spec(gate_name, command)
  gate_name = gate_name or "tool-gate"
  if type(command) ~= "table" then
    error("tools.gate_spec: command must be a table, got " .. type(command))
  end

  local translator = gate_lib.translator(gate_name)
  local run_bindings = mag_run_bindings.new()

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
        if type(env.body) == "table" and env.body.kind == "mag.run_started" then
          run_bindings.bind(env.body, env.from)
        end
        if type(env.body) == "table"
            and env.body.kind == translator.kinds.tool_advertise then
          gate_lib.record_tool_contexts_from_advertise(env.body)
        end
        local invocation = type(env.body) == "table" and env.body.invocation or nil
        if env.from ~= "mag" or (type(invocation) == "table" and (
            invocation.actor_id ~= env.body.from
            or invocation.capability_id ~= env.body.id
            or not run_bindings.validate(invocation))) then
          invocation = nil
        end
        local emitter = chat_emitter.instruction(
          invocation,
          function(body) envelope.emit(nil, body) end
        )
        if not emitter.valid() and type(env.body) == "table"
            and env.body.kind == translator.kinds.tool_invoke then
          nefor.log.warn("tool-gate: instruction notice suppressed for invalid scope", {
            tool = env.body.name,
            reason = emitter.validation_error(),
          })
        end
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
    to_plugin_readonly = true,
    receive_msg = function(_) end,
  }
end

-- A capability provider binary wrapped as a tool source.
--
-- `tools(capability)` returns `libs.tool` definitions; `capability.invoke(call,
-- name, args)` delivers `<name>.capability.invoke { id, capability, args }` to
-- the binary and `capability.cancel(call)` delivers `<name>.capability.cancel
-- { id }`. The binary answers `capability.result { id, output | error }` and
-- may report `capability.stream { id, stream, text }`; both settle or update
-- the pending call, which emits `tool.result` / `tool.stream` as this actor.
--
-- to_plugin only runs once the binary is ready, so advertising on
-- `<gate>.hello` there implies the capabilities can be served.
function M.capability_tools_spec(opts)
  if type(opts) ~= "table" then error("tools.capability_tools_spec: options must be a table", 2) end
  local name, command, gate = opts.name, opts.command, opts.gate
  if type(name) ~= "string" or name == "" then
    error("tools.capability_tools_spec: name must be a non-empty string", 2)
  end
  if type(command) ~= "table" then error("tools.capability_tools_spec: command must be a table", 2) end
  if type(gate) ~= "string" or gate == "" then
    error("tools.capability_tools_spec: gate must be a non-empty string", 2)
  end
  if type(opts.tools) ~= "function" then
    error("tools.capability_tools_spec: tools must be a function(capability)", 2)
  end

  local pending = {}
  local function deliver(body)
    nefor.engine.deliver(name, json.encode({
      type = "event", from = name, ts = nefor.engine.now(), body = body,
    }))
  end
  local capability = {}
  function capability.invoke(call, capability_name, args)
    pending[call.id] = call
    deliver({ kind = name .. ".capability.invoke", id = call.id,
      capability = capability_name, args = args })
  end
  function capability.cancel(call)
    pending[call.id] = nil
    deliver({ kind = name .. ".capability.cancel", id = call.id })
  end

  local source = tool.source { name = name, tools = opts.tools(capability) }

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      local body = env.body
      if type(body) == "table" and body.kind == "capability.result" then
        local call = pending[body.id]
        pending[body.id] = nil
        if call then
          if body.error ~= nil then call.err(body.error) else call.ok(body.output) end
        end
      elseif type(body) == "table" and body.kind == "capability.stream" then
        local call = pending[body.id]
        if call then call.stream(body.stream, body.text) end
      elseif type(body) == "table" then
        nefor.engine.send(json.encode({
          type = "event", from = env.from or name, ts = nefor.engine.now(), body = body,
        }))
      end
    end
  end

  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      local body = env.body
      if not env.replay and env.from ~= name and type(body) == "table" then
        if body.kind == gate .. ".hello" then
          source.advertise(gate)
        else
          source.receive(body)
        end
      end
    end
  end

  return {
    name        = name,
    command     = command,
    source      = source,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    to_plugin_readonly = true,
    receive_msg = function(_) end,
  }
end

function M.basic_actor_spec(options)
  local config = require("config")
  options = options or {}
  local max_read_bytes = type(options.max_read_bytes) == "number"
    and math.tointeger(options.max_read_bytes) or nil
  if max_read_bytes == nil or max_read_bytes < 1 then
    error("tools.basic_actor_spec: max_read_bytes must be a positive integer")
  end
  return M.capability_tools_spec {
    name = "basic-tools",
    command = {
      config.bin("basic-tools"),
      "--read-file-max-bytes", tostring(max_read_bytes),
    },
    gate = options.gate or "tool-gate",
    tools = function(capability)
      return require("libs.basic-tools").tools {
        max_read_bytes = max_read_bytes,
        invoke = capability.invoke,
        cancel = capability.cancel,
      }
    end,
  }
end

function M.git_worktree_actor_spec(options)
  local config = require("config")
  options = options or {}
  return M.capability_tools_spec {
    name = "git-worktree",
    command = { config.bin("git-worktree") },
    gate = options.gate or "tool-gate",
    tools = function(capability)
      return require("libs.git-worktree").tools { invoke = capability.invoke }
    end,
  }
end

return M
