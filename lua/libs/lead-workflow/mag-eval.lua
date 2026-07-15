-- libs/lead-workflow/mag-eval.lua — the one-off MAG expression tool (mechanism).
--
-- `mag-eval` takes one MAG expression source string, compiles it through the
-- mag plugin (the same load handshake the `mag` tool uses), wraps it in the
-- library-defined Artifact shape, and executes it inline on the kernel. Born
-- decomposed: one schema, no action modes — write/compile/execute
-- workflows stay on the `mag` tool.
--
-- Settlement routes by CALLER, using the opaque outer `caller_id` preserved
-- by tool-gate (and the firing id only for direct/internal calls):
--
--   * The LEAD's own calls submit through lead-workflow's standard active-run
--     path. The tool receives the same structured executing acknowledgment as
--     `mag action="execute"`; lifecycle, control, archival, interruption,
--     session cleanup, result rendering, and deferred completion are all
--     owned by that one path.
--   * A GRAPH AGENT's calls remain attached. This module owns only those
--     executing runs, because their terminal output must settle the original
--     capability rather than relay to the lead.
--
-- Compile and pre-execute validation failures stay synchronous at the call
-- site. A canceled pending load is removed before a late compiler response can
-- launch work.

local mag = require("mag")
local sessions = require("sessions")
local envelope = require("core.envelope")

local emit_as = envelope.emit_as

local SOURCE_NAME = "lead-workflow"

local M = {}

local dependency_module_roots = {}

local function copy_roots(roots)
  local copy = {}
  for i = 1, #roots do copy[i] = roots[i] end
  return copy
end

local function validate_roots(roots)
  if type(roots) ~= "table" then
    error("lead-workflow mag-eval: dependency_module_roots must be a list", 3)
  end
  local count = 0
  for key, value in pairs(roots) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 then
      error("lead-workflow mag-eval: dependency_module_roots must be a dense list", 3)
    end
    count = count + 1
    if type(value) ~= "string" or value == "" then
      error("lead-workflow mag-eval: dependency_module_roots entries must be non-empty strings", 3)
    end
  end
  for index = 1, count do
    if roots[index] == nil then
      error("lead-workflow mag-eval: dependency_module_roots must be a dense list", 3)
    end
  end
end

local submit_run

function M.configure(opts)
  if opts == nil then opts = {} end
  if type(opts) ~= "table" then
    error("lead-workflow mag-eval: configure options must be a table", 2)
  end
  local roots = opts.dependency_module_roots
  if roots == nil then roots = {} end
  validate_roots(roots)
  dependency_module_roots = copy_roots(roots)
  if opts.submit_run ~= nil and type(opts.submit_run) ~= "function" then
    error("lead-workflow mag-eval: submit_run must be a function", 2)
  end
  submit_run = opts.submit_run
end

local function module_roots_for(ws)
  local roots = copy_roots(dependency_module_roots)
  roots[#roots + 1] = ws .. "/lib"
  return roots
end

local state = {
  -- Monotone per-session counter for run names (`eval-1`, `eval-2`, …).
  seq = 0,
  -- In-flight compile handshakes, keyed by the mag.load request id.
  pending_loads = {},
  -- Attached graph-agent runs awaiting terminal mag.run_result, by run_id.
  attached_runs = {},
}

M.schema = {
  name        = "mag-eval",
  display = { label = "mag-eval", primary = { arg = "intent" }, result = { kind = "content" } },
  description =
    "Evaluate one MAG graph-fragment expression on the actor kernel. " ..
    "A command is (nefor.shell.command \"step\" \"rg -n TODO src/\"); " ..
    "pipe fragments with (nefor.graph.connect left right). Commands run " ..
    "until they exit — long-running " ..
    "work (servers, review UIs, watch loops) needs no backgrounding, no " ..
    "`&`, no polling; run it in the foreground. " ..
    "nefor.shell.command-with-options opts into a wall-clock bound. " ..
    "As the lead, the call acks immediately with the run name and the " ..
    "terminal output arrives as a run-completion notification, like any " ..
    "dispatched graph; as a graph agent, the output returns as the tool " ..
    "result. Multi-step or multi-file work runs as a .mag program via the " ..
    "mag tool instead.",
  parameters  = {
    type = "object",
    properties = {
      intent = { type = "string", description = "Main operation in 1-5 words." },
      expr = {
        type        = "string",
        description = "MAG expression source to evaluate.",
      },
    },
    required = { "intent", "expr" },
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

-- Tool handler: persist the expression and start the compile handshake. Lead
-- firings stay open through validation/submission; attached firings stay open
-- through terminal completion.
function M.handle(firing_id, args, metadata)
  local intent = args and args.intent
  local words = 0
  if type(intent) == "string" then for _ in intent:gmatch("%S+") do words = words + 1 end end
  if type(intent) ~= "string" or intent:match("^%s*$") or words < 1 or words > 5 then
    tool_err(firing_id, "mag-eval: 'intent' must contain 1-5 words")
    return
  end
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
  local source = table.concat({
    '(require "nefor.artifact")\n',
    '(require "nefor.graph")\n',
    '(require "nefor.shell")\n',
    '(let [fragment ', expr, '\n',
    '      initial (nefor.shell.start-message fragment)\n',
    '      program (nefor.graph.finish fragment ',
    '                (as (List nefor.graph.Message) [initial]) ',
    '                (as (List nefor.graph.Rule) []))]\n',
    '  (nefor.artifact.compile program))\n',
  })
  fh:write(source)
  fh:close()

  -- The gate-minted firing id is source correlation only. Classification uses
  -- the preserved outer capability id; direct/internal calls fall back to the
  -- firing id because no rewrite occurred.
  local caller_id = type(metadata) == "table" and metadata.caller_id or nil
  if type(caller_id) ~= "string" then caller_id = firing_id end
  local routing = "attached"
  local al_ok, al = pcall(require, "agentic-loop")
  if al_ok and type(al.lead_scoped_id) == "function"
      and al.lead_scoped_id(caller_id) == true then
    routing = "lead"
  end

  local run_id = "mag-" .. run_name .. "-" .. tostring(now_ms())
  local load_id = run_id .. "-load"
  state.pending_loads[load_id] = {
    firing_id  = firing_id,
    run_id     = run_id,
    run_name   = run_name,
    session_id = session_id,
    routing    = routing,
  }
  emit_as(SOURCE_NAME, "mag", {
    kind       = "mag.load",
    id         = load_id,
    source_dir = ws,
    module_roots = module_roots_for(ws),
    entry      = rel,
  })
end

local function take(map, key)
  if type(key) ~= "string" then return nil end
  local entry = map[key]
  map[key] = nil
  return entry
end

-- The compile reply delegates lead submissions to init.lua's standard
-- validation/active-run path. Attached graph-agent submissions execute here
-- and remain owned here until their terminal result settles the capability.
local function on_loaded(body)
  local pending = take(state.pending_loads, body.in_reply_to)
  if not pending then return false end
  if type(body.artifact) ~= "table" then
    tool_err(pending.firing_id, "mag-eval: mag.loaded reply carried no artifact")
    return true
  end
  if type(submit_run) ~= "function" then
    tool_err(pending.firing_id, "mag-eval: submission path is unavailable")
    return true
  end
  if pending.routing == "lead" then
    submit_run(pending, body, false)
    return true
  end
  if not submit_run(pending, body, true) then return true end
  state.attached_runs[pending.run_id] = pending
  return true
end

-- A compile rejection: fail the firing with the compiler's message.
local function on_error(body)
  local pending = take(state.pending_loads, body.in_reply_to)
  if not pending then return false end
  tool_err(pending.firing_id, "mag-eval: compilation failed:\n" .. tostring(body.message))
  return true
end

-- Terminal replies here belong only to attached graph-agent evals.
local function on_run_result(body)
  local pending = take(state.attached_runs, body.run_id or body.in_reply_to)
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

-- Graceful interrupt of every attached eval run. Lead-called evals live in
-- init.lua's standard active-run table and are terminated by its interrupt
-- path; attached graph-agent evals remain capability-scoped here.
function M.interrupt_all_runs()
  for run_id in pairs(state.attached_runs) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id })
  end
end

-- Gate-forwarded cancel for one open firing. Pending compiles are removed so
-- late compiler replies are ignored. Attached runs lose their capability
-- ownership before they are terminated, so a late run result cannot emit a
-- duplicate source result after the gate has settled the outer invocation.
function M.cancel(firing_id)
  if type(firing_id) ~= "string" then
    return false
  end
  local hit = false
  for load_id, pending in pairs(state.pending_loads) do
    if pending.firing_id == firing_id then
      state.pending_loads[load_id] = nil
      hit = true
    end
  end
  for run_id, pending in pairs(state.attached_runs) do
    if pending.firing_id == firing_id then
      state.attached_runs[run_id] = nil
      emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id, terminate = true })
      hit = true
    end
  end
  return hit
end

-- Session-end fails pending/attached capabilities. Lead-submitted runs are not
-- represented here; init.lua's standard active-run cleanup owns them.
local function on_session_end()
  for _, pending in pairs(state.pending_loads) do
    tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
  end
  state.pending_loads = {}
  for run_id, pending in pairs(state.attached_runs) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
    tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
  end
  state.attached_runs = {}
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
M._internals = {
  state = state,
  reset = function()
    dependency_module_roots = {}
    state.seq = 0
    state.pending_loads = {}
    state.attached_runs = {}
  end,
}

return M
