-- libs/lead-workflow/mag-eval.lua — the one-off MAG expression tool (mechanism).
--
-- `mag-eval` takes one MAG expression source string, compiles it through the
-- mag plugin (the same load handshake the `mag` tool uses — the compiler's
-- shell defaults fill in ids and the terminal), executes it inline on the
-- kernel, and answers the tool firing with the terminal node's output. Born
-- decomposed: one schema, no action modes — write/compile/execute workflows
-- stay on the `mag` tool.
--
-- The firing is BLOCKING by design: no tool.result is emitted at execute
-- time; the pending firing settles when the run's terminal `mag.run_result`
-- arrives (the plugin's ActiveExecute settle machinery correlates it to the
-- execute id — this module keys `run_id` → firing). The result relays as the
-- tool result itself, never as a separate injected turn — for the model, a
-- mag-eval call reads exactly like a shell invocation. Runs are observable
-- like any other: lifecycle events stream on the bus (the chat run panel
-- tracks them) under the readable run_name `eval-<n>`.
--
-- Self-contained by design (Unix-shaped): owns its pending state and its
-- slice of the bus protocol end-to-end. init.lua only registers the schema,
-- the tool handler, and the `on_bus` tap — it shares no internals with the
-- `mag` tool's pending-load machinery.

local mag = require("mag")
local sessions = require("sessions")
local envelope = require("core.envelope")

local emit_as = envelope.emit_as

local SOURCE_NAME = "lead-workflow"

local M = {}

local state = {
  -- Monotone per-session counter for run names (`eval-1`, `eval-2`, …).
  seq = 0,
  -- In-flight compile handshakes, keyed by the mag.load request id.
  pending_loads = {},
  -- Executing runs awaiting their terminal mag.run_result, keyed by run_id.
  pending_runs = {},
}

M.schema = {
  name        = "mag-eval",
  description =
    "Evaluate one MAG expression on the actor kernel and return the " ..
    "terminal node's output. `->` is the pipe: a node's output becomes the " ..
    "next node's stdin, e.g. ((bash \"rg -n TODO src/\") -> (bash \"sort\")). " ..
    "A bare (bash \"cmd\") runs a single command.",
  parameters  = {
    type = "object",
    properties = {
      expr = {
        type        = "string",
        description = "MAG expression source to evaluate.",
      },
    },
    required = { "expr" },
  },
}

local function now_ms()
  if nefor.engine and type(nefor.engine.now) == "function" then
    return nefor.engine.now()
  end
  return os.time() * 1000
end

local function tool_ok(firing_id, output)
  emit_as(SOURCE_NAME, nil, { kind = "tool.result", id = firing_id, output = output })
end

local function tool_err(firing_id, err)
  emit_as(SOURCE_NAME, nil, { kind = "tool.result", id = firing_id, error = tostring(err) })
end

local function sh_quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function mkdir_p(path)
  if nefor.fs and type(nefor.fs.mkdir_p) == "function" then
    local ok = pcall(nefor.fs.mkdir_p, path)
    if ok then return true end
  end
  local ok = os.execute("mkdir -p " .. sh_quote(path) .. " >/dev/null 2>&1")
  return ok == true or ok == 0
end

-- The relay text for a completed run: the terminal message's text (a bash
-- chain's stdout, an agent's final answer), the encoded payload otherwise,
-- with the persisted output file as the last fallback.
local function run_output_text(body)
  local r = body.result
  if type(r) == "string" then return r end
  if type(r) == "table" then
    if type(r.text) == "string" then return r.text end
    if type(r.final_answer) == "string" then return r.final_answer end
    local ok, encoded = pcall(nefor.json.encode, r)
    if ok and type(encoded) == "string" then return encoded end
  end
  if type(body.output_path) == "string" and #body.output_path > 0 then
    local fh = io.open(body.output_path, "r")
    if fh then
      local content = fh:read("*a")
      fh:close()
      if content then return content end
    end
  end
  return "(run completed with no output)"
end

-- Tool handler: persist the expression as a workspace source file and start
-- the compile handshake. The firing stays open until the run settles.
function M.handle(firing_id, args)
  local expr = args and args.expr
  if type(expr) ~= "string" or #expr == 0 then
    tool_err(firing_id, "mag-eval: requires a non-empty 'expr' string")
    return
  end
  local session_id = sessions.current_id()
  if not session_id then
    tool_err(firing_id, "mag-eval: no active session")
    return
  end
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  local ws, ws_err = mag.init_workspace(session_id, config_dir)
  if not ws then
    tool_err(firing_id, "mag-eval: workspace init failed: " .. tostring(ws_err))
    return
  end

  state.seq = state.seq + 1
  local run_name = "eval-" .. tostring(state.seq)
  local rel = "eval/" .. run_name .. ".mag"
  mkdir_p(ws .. "/eval")
  local fh, open_err = io.open(ws .. "/" .. rel, "w")
  if not fh then
    tool_err(firing_id, "mag-eval: cannot write " .. rel .. ": " .. tostring(open_err))
    return
  end
  fh:write(expr)
  fh:close()

  local run_id = "mag-" .. run_name .. "-" .. tostring(now_ms())
  local load_id = run_id .. "-load"
  state.pending_loads[load_id] = {
    firing_id  = firing_id,
    run_id     = run_id,
    run_name   = run_name,
    session_id = session_id,
  }
  emit_as(SOURCE_NAME, "mag", {
    kind       = "mag.load",
    id         = load_id,
    source_dir = ws,
    entry      = rel,
  })
end

local function take(map, key)
  if type(key) ~= "string" then return nil end
  local entry = map[key]
  map[key] = nil
  return entry
end

-- The compile reply: execute the lowered modification inline on the kernel
-- (the control plane reaches kernel ops directly — docs/ir.md; inline rather
-- than resident, so concurrent loaders never race). Kernel-side validation is
-- the backstop for factories and wiring.
local function on_loaded(body)
  local pending = take(state.pending_loads, body.in_reply_to)
  if not pending then return false end
  if type(body.modification) ~= "table" then
    tool_err(pending.firing_id, "mag-eval: mag.loaded reply carried no modification")
    return true
  end
  emit_as(SOURCE_NAME, "mag", {
    kind         = "mag.execute",
    id           = pending.run_id,
    run_id       = pending.run_id,
    run_name     = pending.run_name,
    session_id   = pending.session_id,
    modification = body.modification,
  })
  state.pending_runs[pending.run_id] = pending
  return true
end

-- A compile rejection: fail the firing with the compiler's message.
local function on_error(body)
  local pending = take(state.pending_loads, body.in_reply_to)
  if not pending then return false end
  tool_err(pending.firing_id, "mag-eval: compilation failed:\n" .. tostring(body.message))
  return true
end

-- The run's terminal reply settles the blocked firing: the terminal output IS
-- the tool result.
local function on_run_result(body)
  local pending = take(state.pending_runs, body.run_id or body.in_reply_to)
  if not pending then return false end
  if body.status == "completed" then
    tool_ok(pending.firing_id, run_output_text(body))
  else
    local err = body.error
      or (body.status == "killed" and "run killed" or "mag run failed")
    tool_err(pending.firing_id, "mag-eval: " .. tostring(err))
  end
  return true
end

-- Graceful interrupt (the double-Esc path): the gate forwards
-- `lead-workflow.tool.cancel { id = firing_id }` when the lead's interrupt
-- cancels a mag-eval capability. Find the sub-run this firing dispatched and
-- interrupt IT — `mag.interrupt_run` cancels the sub-run's in-flight bash (its
-- child process group dies) and settles the sub-run as failed. The sub-run's
-- terminal `mag.run_result` then flows back through `on_run_result` as the tool
-- result, which drops at the lead's already-settled correlation. pending_runs
-- is KEPT so on_run_result cleans it up — this only propagates the cancel down.
-- Returns true when a matching sub-run was found.
function M.cancel(firing_id)
  if type(firing_id) ~= "string" then
    return false
  end
  local hit = false
  for run_id, pending in pairs(state.pending_runs) do
    if pending.firing_id == firing_id then
      emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id })
      hit = true
    end
  end
  return hit
end

-- A session ending mid-eval must not strand the blocked firing: fail every
-- pending firing and kill the in-flight runs (the kernel reaps their actors
-- through the fold). Never consumes the envelope — init.lua's own
-- session-end handling still runs.
local function on_session_end()
  for _, pending in pairs(state.pending_loads) do
    tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
  end
  state.pending_loads = {}
  for run_id, pending in pairs(state.pending_runs) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
    tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
  end
  state.pending_runs = {}
  return false
end

-- The bus tap init.lua wires into receive_msg (live path, after the replay
-- guard). Returns true when the envelope was one of ours — correlated to a
-- pending eval load or run — so the caller stops; everything else, including
-- other consumers' mag.* traffic, passes through untouched.
function M.on_bus(kind, body)
  if kind == "mag.loaded" then return on_loaded(body) end
  if kind == "mag.error" then return on_error(body) end
  if kind == "mag.run_result" then return on_run_result(body) end
  if kind == "sessions.session_end" then return on_session_end() end
  return false
end

-- Test seam.
M._internals = { state = state }

return M
