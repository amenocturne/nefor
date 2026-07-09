-- libs/lead-workflow/mag-eval.lua — the one-off MAG expression tool (mechanism).
--
-- `mag-eval` takes one MAG expression source string, compiles it through the
-- mag plugin (the same load handshake the `mag` tool uses — the compiler's
-- shell defaults fill in ids and the terminal), and executes it inline on the
-- kernel. Born decomposed: one schema, no action modes — write/compile/execute
-- workflows stay on the `mag` tool.
--
-- Settlement routes by CALLER, decided from the firing id's scope
-- (agentic-loop.lead_scoped_id — ids are `<scope>/cap-N`), never by a
-- model-facing mode:
--
--   * The LEAD's own calls DETACH: the tool result answers as soon as the
--     run is submitted ("eval-N dispatched"), and the terminal output relays
--     back through `agentic-loop.relay_run_completion` — the same deferred
--     user-role turn a dispatched `mag` graph produces. One shape for every
--     kernel run, long or short: a review UI that waits minutes on the user
--     and a sub-second `rg` both dispatch, show on the run panel (lifecycle
--     events stream under the readable run_name `eval-<n>`), and come back
--     as a run-completion notification. Nothing blocks the lead's turn, so
--     commands may run unbounded (basic-tools bash has no default timeout;
--     `(bash "cmd" {:timeout_ms N})` opts into a bound).
--   * A GRAPH AGENT's calls BLOCK: the firing settles when the run's
--     terminal `mag.run_result` arrives, and the output IS the tool result.
--     The agent's graph node is itself the waiting structure — a sub-run has
--     no relay channel, and detaching would strand its output at the lead.
--
-- A compile error always fails the tool call itself, whoever called: the
-- model authored the expression and needs the message at the callsite.
--
-- Self-contained by design (Unix-shaped): owns its pending state and its
-- slice of the bus protocol end-to-end. init.lua only registers the schema,
-- the tool handler, and the `on_bus` tap — it shares no internals with the
-- `mag` tool's pending-load machinery. Caller routing and the relay ride
-- agentic-loop's PUBLIC seams (lead_scoped_id, relay_run_completion),
-- exactly as lead-workflow's dispatched runs do.

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
    "Evaluate one MAG expression on the actor kernel. `->` is the pipe: a " ..
    "node's output becomes the next node's stdin, e.g. " ..
    "((bash \"rg -n TODO src/\") -> (bash \"sort\")). A bare (bash \"cmd\") " ..
    "runs a single command. Commands run until they exit — long-running " ..
    "work (servers, review UIs, watch loops) needs no backgrounding, no " ..
    "`&`, no polling; run it in the foreground. " ..
    "(bash \"cmd\" {:timeout_ms 60000}) opts into a wall-clock bound. " ..
    "As the lead, the call acks immediately with the run name and the " ..
    "terminal output arrives as a run-completion notification, like any " ..
    "dispatched graph; as a graph agent, the output returns as the tool " ..
    "result. Multi-step or multi-file work runs as a .mag program via the " ..
    "mag tool instead.",
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

  -- Caller routing (header): a lead-scoped firing detaches; anything else
  -- (a graph agent's call, or a context with no lead loop at all) blocks.
  local detached = false
  local al_ok, al = pcall(require, "agentic-loop")
  if al_ok and type(al.lead_scoped_id) == "function" then
    detached = al.lead_scoped_id(firing_id) == true
  end

  local run_id = "mag-" .. run_name .. "-" .. tostring(now_ms())
  local load_id = run_id .. "-load"
  state.pending_loads[load_id] = {
    firing_id  = firing_id,
    run_id     = run_id,
    run_name   = run_name,
    session_id = session_id,
    detached   = detached,
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
-- the backstop for factories and wiring. A detached (lead-called) firing
-- settles HERE — the run is dispatched, its output arrives later as a
-- run-completion notification; a blocking firing stays open until the run's
-- terminal reply.
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
  if pending.detached then
    tool_ok(pending.firing_id,
      pending.run_name .. " dispatched (run_id " .. pending.run_id .. "). " ..
      "It is running on the kernel; the terminal output will arrive as a " ..
      "run-completion notification.")
  end
  return true
end

-- A compile rejection: fail the firing with the compiler's message.
local function on_error(body)
  local pending = take(state.pending_loads, body.in_reply_to)
  if not pending then return false end
  tool_err(pending.firing_id, "mag-eval: compilation failed:\n" .. tostring(body.message))
  return true
end

-- The run's terminal reply. Detached (lead-called): relay the outcome
-- through agentic-loop's run-completion channel — the firing already
-- settled at dispatch. Blocking (graph agent): settle the open firing with
-- the terminal output as the tool result.
local function on_run_result(body)
  local pending = take(state.pending_runs, body.run_id or body.in_reply_to)
  if not pending then return false end
  if pending.detached then
    local al = require("agentic-loop")
    if type(al.relay_run_completion) ~= "function" then return true end
    if body.status == "completed" then
      al.relay_run_completion({
        run_id = pending.run_id,
        status = "success",
        output = run_output_text(body),
      })
    else
      al.relay_run_completion({
        run_id = pending.run_id,
        status = "failed",
        error  = body.error
          or (body.status == "killed" and "run killed" or "mag run failed"),
      })
    end
    return true
  end
  if body.status == "completed" then
    tool_ok(pending.firing_id, run_output_text(body))
  else
    local err = body.error
      or (body.status == "killed" and "run killed" or "mag run failed")
    tool_err(pending.firing_id, "mag-eval: " .. tostring(err))
  end
  return true
end

-- Graceful interrupt of EVERY in-flight eval run (the double-Esc path;
-- init.lua calls this from its chat.interrupt_all handling, alongside the
-- `mag` tool's own dispatched-run interrupts). A detached firing settled at
-- dispatch, so the gate has no capability left to cancel — this entry point
-- is how the interrupt reaches the runs themselves. `mag.interrupt_run`
-- cancels an in-flight bash (its child process group dies) and settles the
-- run as failed; the terminal `mag.run_result` then flows back through
-- on_run_result (relayed for detached firings, the tool result for blocking
-- ones).
function M.interrupt_all_runs()
  for run_id in pairs(state.pending_runs) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id })
  end
end

-- Gate-forwarded cancel for one open firing (`lead-workflow.tool.cancel
-- { id = firing_id }`): only a pending COMPILE can still hold the firing —
-- interrupt the runs it would have produced; a dispatched run's cancel
-- travels the interrupt path above. Returns true when something matched.
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

-- A session ending mid-eval must not strand an open firing: fail every
-- firing still blocked (compile handshakes, plus blocking runs — detached
-- firings already settled) and kill the in-flight runs (the kernel reaps
-- their actors through the fold). Never consumes the envelope — init.lua's
-- own session-end handling still runs.
local function on_session_end()
  for _, pending in pairs(state.pending_loads) do
    tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
  end
  state.pending_loads = {}
  for run_id, pending in pairs(state.pending_runs) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
    if not pending.detached then
      tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
    end
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
