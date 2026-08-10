-- libs/lead-workflow/mag-eval.lua — the one-off MAG expression tool (mechanism).
--
-- `mag-eval` takes one MAG expression source string, compiles it through the
-- mag plugin (the same load handshake the `mag` tool uses), wraps it in the
-- library-defined Artifact shape, and executes it inline on the kernel. Born
-- decomposed: one schema, no action modes — write/compile/execute
-- workflows stay on the `mag` tool.
--
-- Every caller submits through lead-workflow's standard detached active-run
-- path. The tool receives the same structured executing acknowledgment as
-- `mag action="execute"`; lifecycle, control, archival, interruption, session
-- cleanup, result rendering, and explicit `await-run` completion are all owned
-- by that one path. Subagent provenance records the dispatching actor so only
-- that actor can control or await its run.
--
-- Compile and pre-execute validation failures stay synchronous at the call
-- site. A canceled pending load is removed before a late compiler response can
-- launch work.

local mag = require("libs.mag-workspace")
local sessions = require("libs.sessions")
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
local mint_run_id
local resolve_invocation

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
  if opts.mint_run_id ~= nil and type(opts.mint_run_id) ~= "function" then
    error("lead-workflow mag-eval: mint_run_id must be a function", 2)
  end
  if opts.resolve_invocation ~= nil and type(opts.resolve_invocation) ~= "function" then
    error("lead-workflow mag-eval: resolve_invocation must be a function", 2)
  end
  submit_run = opts.submit_run
  mint_run_id = opts.mint_run_id or mint_run_id
  resolve_invocation = opts.resolve_invocation or resolve_invocation
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
}

M.schema = {
  name        = "mag-eval",
  display = { label = "mag-eval", primary = { arg = "intent" }, result = { kind = "content" } },
  description =
    "Evaluate one MAG node expression on the actor kernel. " ..
    "A command is (nefor.shell.command \"step\" \"rg -n TODO src/\"); " ..
    "compose multi-node work in a .mag graph. Commands run until process exit, " ..
    "so awaiting a persistent foreground process such as an HTTP server waits " ..
    "indefinitely. Long-lived servers and watchers must use an appropriate retained " ..
    "or background lifecycle facility when available, or one bounded command that " ..
    "starts the process, waits for readiness, performs the dependent work, and tears " ..
    "it down. Never launch a server as a normal run and await its completion. " ..
    "nefor.shell.command-with-options opts into a wall-clock bound. " ..
    "The call acknowledges immediately with a stable run_id; " ..
    "call await-run with that handle when your next step depends on terminal " ..
    "output. The normal owner-scoped run-completion notification remains independent. " ..
    "Multi-step or multi-file work runs as a .mag program via the " ..
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

local function normalize_run_name(intent)
  local normalized = intent:gsub("[%z\1-\31\127]", " ")
    :gsub("%s+", " ")
    :match("^%s*(.-)%s*$")
  if normalized == "" then return nil end
  return normalized
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

-- Tool handler: persist the expression and start the compile handshake. The
-- firing stays open only through validation and submission.
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
  local provenance, provenance_error
  if type(resolve_invocation) == "function" then
    provenance, provenance_error = resolve_invocation(metadata)
  else
    local session_id = sessions.current_id()
    if session_id then provenance = { session_id = session_id, principal = "subagent", direct = true } end
    provenance_error = "no active session"
  end
  if not provenance then
    tool_err(firing_id, "mag-eval: " .. tostring(provenance_error))
    return
  end
  local session_id = provenance.session_id
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  local ws, ws_err = mag.init_workspace(session_id, config_dir)
  if not ws then
    tool_err(firing_id, "mag-eval: workspace init failed: " .. tostring(ws_err))
    return
  end

  state.seq = state.seq + 1
  local eval_name = "eval-" .. tostring(state.seq)
  local run_name = normalize_run_name(intent)
  local rel = "eval/" .. eval_name .. ".mag"
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
    '(let [start (nefor.graph.source "eval-input" (type-tag Unit) nil)\n',
    '      operation ', expr, '\n',
    '      result (nefor.graph.output-for "eval-output" operation)\n',
    '      topology (fn [[graph nefor.graph.Graph]] -> nefor.graph.Graph\n',
    '                 (nefor.graph.add-edges graph ',
    '                   [(nefor.graph.edge start operation) ',
    '                    (nefor.graph.edge operation result)]))]\n',
    '  (nefor.artifact.compile topology))\n',
  })
  fh:write(source)
  fh:close()

  if type(mint_run_id) ~= "function" then
    tool_err(firing_id, "mag-eval: run-id allocator is unavailable")
    return
  end
  local run_id = mint_run_id()
  local load_id = run_id .. "-load"
  state.pending_loads[load_id] = {
    firing_id  = firing_id,
    run_id     = run_id,
    run_name   = run_name,
    session_id = session_id,
    dispatcher_id = provenance.dispatcher_id,
    conversation_id = provenance.conversation_id,
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

-- The compile reply delegates submission to init.lua's standard
-- validation/active-run path.
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
  submit_run(pending, body)
  return true
end

-- A compile rejection: fail the firing with the compiler's message.
local function on_error(body)
  local pending = take(state.pending_loads, body.in_reply_to)
  if not pending then return false end
  tool_err(pending.firing_id, "mag-eval: compilation failed:\n" .. tostring(body.message))
  return true
end

-- Gate-forwarded cancel for one open firing. Pending compiles are removed so
-- late compiler replies are ignored. Submitted runs are owned by the standard
-- registry and canceled through their dispatch firing id in init.lua.
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
  return hit
end

-- Session-end fails pending compile capabilities. Submitted runs are owned by
-- init.lua's standard active-run cleanup.
local function on_session_end()
  for _, pending in pairs(state.pending_loads) do
    tool_err(pending.firing_id, "mag-eval: session ended before the run settled")
  end
  state.pending_loads = {}
  return false
end

-- The bus tap init.lua wires into receive_msg (live path, after the replay
-- guard). Returns true when the envelope was one of ours — correlated to a
-- pending eval load — so the caller stops; everything else, including
-- other consumers' mag.* traffic, passes through untouched.
function M.on_bus(kind, body)
  if kind == "mag.loaded" then return on_loaded(body) end
  if kind == "mag.error" then return on_error(body) end
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
  end,
}

return M
