-- starter/lead-workflow/init.lua — lead-workflow actor.
--
-- Owns three pieces of state on top of the lead's chat-side agentic-loop
-- (see `agentic-loop/init.lua`):
--
--   1. Currently-executing graphs — their run_ids plus a small status
--      snapshot so the lead can answer status questions and cancel
--      specific runs.
--   2. The currently-in-flight plan slot — one plan at a time.
--      Ephemeral: lives in process memory, never replayed across
--      session boundaries. Flushed when the user types anything
--      non-verdict, or at session-end.
--
-- ## Plan approval contract (blocking write-review)
--
-- `write-review` is a BLOCKING tool. The lead calls it; this actor
-- records the plan and the firing_id in `state.active_plan` but does
-- NOT emit `tool.result`. The agentic-loop is now waiting for the tool
-- to complete — the lead's turn is effectively paused.
--
-- One of three user actions resolves the deferred ack:
--
--   * `/approve [reason]`      — emit tool.result = "approved, proceed
--                                with implementation". status="approved".
--   * `/reject [reason]`       — emit tool.result = "rejected: <reason>,
--                                revise". status="rejected".
--   * any other review reply   — emit tool.result = "user replied with
--                                a comment, plan discarded — address
--                                their reply: <text>". active_plan is
--                                cleared.
--
-- After the verdict resolves, the approval is single-use per turn: the
-- next genuine user message clears `state.active_plan`. Combined with
-- the no-replay rule (state.active_plan does not survive session
-- restart), this means writer dispatches always need a fresh approval
-- across session boundaries.
--
-- ## Tools the lead invokes
--
-- Advertised to tool-gate as a virtual source `lead-workflow`. The
-- gate forwards `tool-gate.tool.invoke` → `lead-workflow.tool.invoke`;
-- this actor handles the forwarded envelopes and emits `tool.result`
-- back. The gate's id-rewriting machinery handles the round-trip.
--
--   * `dispatch-graph` — args:
--       { name = <kebab-case string>, nodes = [{ id, role, agent_args, dependencies? }, ...] }
--     Builds a reasoner-graph spec from the role-keyed nodes (looking
--     up `role -> reasoner config` from `lead-workflow.role.AGENT_CONFIGS`
--     when available; falling back to a default `agent`-reasoner shape
--     when the role module is not yet loaded), submits it via the same
--     `tool.invoke{name=spawn_graph}` shape `agentic-loop`'s
--     `submit_orchestrator_run` uses. Returns the minted run_id.
--     Writer roles (read_only=false) require `state.active_plan.status
--     == "approved"`; read-only roles dispatch freely.
--
--   * `write-review` (alias `submit-plan`) — args:
--       { plan = <string> }
--     Stores the plan in `state.active_plan`, broadcasts
--     `lead-workflow.plan.submitted { plan, submitted_at }` for the
--     chat surface to render the yellow review block. Does NOT emit
--     tool.result — the agentic-loop blocks until the user verdict
--     resolves the deferred ack.
--
--   * `graph-status` — args:
--       { run_id? = <string>, include_completed? = <boolean> }
--     Returns active graph status, or one graph when run_id is supplied.
--
--   * `terminate-graph` — args:
--       { run_id? = <string> }
--     Cancels one active graph. If run_id is omitted and exactly one graph
--     is active, cancels that graph; if multiple are active, asks the lead
--     to specify run_id.
--
-- ## Termination on session exit
--
-- Subscribes to `sessions.session_end`. If active graphs exist,
-- emits `<reasoner-graph>.cancel { run_id }` and appends a system
-- message `[Graph terminated by user — session exit]` to chat history
-- so the model sees it on the next turn. Also clears `state.active_plan`
-- so a resumed/new session starts with no carry-over approval.

local envelope      = require("core.envelope")
local event         = require("core.event")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit    = envelope.emit

local state = {
  -- In-flight graph run_ids; empty when no graphs are running.
  ---@type table<string, boolean>
  active_run_ids = {},

  -- Graph status snapshots keyed by run_id. This is live-session control
  -- state, not durable history; replay deliberately does not rebuild it.
  ---@type table<string, table>
  graph_runs = {},

  -- Per-firing lookup for node result status.
  ---@type table<string, { run_id: string, node_id: string }>
  firing_to_node = {},

  -- The single in-flight plan slot. Lifetime is one verdict turn:
  -- created by write-review, decided by /approve or /reject, flushed
  -- on the next user message after the verdict (or immediately when
  -- the user comments instead of voting). Not replayed across session
  -- boundaries — each session starts with no approval.
  --
  -- Shape when non-nil:
  --   {
  --     content           = string,  -- the plan text
  --     submitted_at      = number,  -- engine.now() at submit time
  --     pending_firing_id = string|nil, -- write-review firing waiting
  --                                       for verdict; nil after resolved
  --     status            = "pending"|"approved"|"rejected",
  --     reason            = string|nil, -- /reject reason, if given
  --   }
  ---@type table|nil
  active_plan = nil,
  gate_mode = "safe",
}

local SOURCE_NAME = "lead-workflow"

local RETRY_FANOUT = {
  ["in"] = "generic-control.RetryDecision",
  out = {
    "generic-control.Retry",
    "generic-control.Pass",
    "generic-control.Exhausted",
  },
}

local RETRY_EDGE_TYPES = {
  retry = "generic-control.Retry",
  pass = "generic-control.Pass",
  exhausted = "generic-control.Exhausted",
}

-- Look up `lead-workflow.role.AGENT_CONFIGS[role]`. Tolerates the role
-- module not being loaded: returns nil instead of erroring at module
-- load. Each call retries the require so a later install picks up.
local function role_config(role)
  local ok, mod = pcall(require, "lead-workflow.role")
  if not ok or type(mod) ~= "table" then return nil end
  local configs = mod.AGENT_CONFIGS
  if type(configs) ~= "table" then return nil end
  return configs[role]
end

local function emit_tool_result_ok(firing_id, output)
  emit_as(SOURCE_NAME, nil, {
    kind   = "tool.result",
    id     = firing_id,
    output = output,
  })
end

local function emit_tool_result_err(firing_id, err)
  emit_as(SOURCE_NAME, nil, {
    kind  = "tool.result",
    id    = firing_id,
    error = tostring(err),
  })
end

local function now_ms()
  if nefor.engine and nefor.engine.now then
    return nefor.engine.now()
  end
  return nil
end

local function sorted_keys(map)
  local keys = {}
  for k in pairs(map or {}) do keys[#keys + 1] = k end
  table.sort(keys)
  return keys
end

local GRAPH_NAME_MAX = 20

local function validate_graph_name(name)
  if type(name) ~= "string" or #name == 0 then
    return nil, "dispatch-graph: args.name must be a non-empty kebab-case string"
  end
  if #name > GRAPH_NAME_MAX then
    return nil, "dispatch-graph: args.name must be at most " .. GRAPH_NAME_MAX .. " characters"
  end
  if not name:match("^[a-z][a-z0-9%-]*$") or name:match("%-$")
      or name:match("%-%-") then
    return nil,
      "dispatch-graph: args.name must be kebab-case lowercase words, e.g. `fix-graph-names`"
  end
  return name, nil
end

local function ensure_graph_run(run_id)
  if type(run_id) ~= "string" or #run_id == 0 then return nil end
  local run = state.graph_runs[run_id]
  if run == nil then
    run = {
      run_id = run_id,
      status = "unknown",
      name = nil,
      total_nodes = 0,
      nodes = {},
      started_at = nil,
      updated_at = now_ms(),
    }
    state.graph_runs[run_id] = run
  end
  return run
end

local function ensure_graph_node(run, node_id)
  if type(run) ~= "table" or type(node_id) ~= "string" or #node_id == 0 then
    return nil
  end
  local node = run.nodes[node_id]
  if node == nil then
    node = { id = node_id, status = "pending" }
    run.nodes[node_id] = node
  end
  return node
end

local function graph_summary(run)
  local nodes = {}
  for _, node_id in ipairs(sorted_keys(run.nodes or {})) do
    local n = run.nodes[node_id]
    nodes[#nodes + 1] = {
      id = node_id,
      reasoner = n.reasoner,
      status = n.status,
      firing_id = n.firing_id,
      last_tool = n.last_tool,
      error = n.error,
    }
  end
  return {
    run_id = run.run_id,
    name = run.name,
    status = run.status,
    total_nodes = run.total_nodes,
    started_at = run.started_at,
    updated_at = run.updated_at,
    completed_at = run.completed_at,
    nodes = nodes,
    result_status = run.result_status,
    results = run.results,
  }
end

local function active_run_count()
  local count, only = 0, nil
  for run_id in pairs(state.active_run_ids or {}) do
    count = count + 1
    only = run_id
  end
  return count, only
end

local mark_graph_cancelled

local function cancel_active_graphs()
  local ids = state.active_run_ids
  state.active_run_ids = {}
  local n = 0
  for run_id in pairs(ids or {}) do
    if mark_graph_cancelled then mark_graph_cancelled(run_id) end
    emit_as(SOURCE_NAME, nil, { kind = "graph.cancel", run_id = run_id })
    n = n + 1
  end
  return n
end

local function spec_edges(spec)
  local edges = {}
  if type(spec.dependencies) == "table" then
    for _, dep_id in ipairs(spec.dependencies) do
      edges[#edges + 1] = { from = dep_id, to = spec.id }
    end
  end
  if type(spec.routes) == "table" then
    for route, target_id in pairs(spec.routes) do
      if RETRY_EDGE_TYPES[route] ~= nil and type(target_id) == "string" and #target_id > 0 then
        edges[#edges + 1] = {
          from = spec.id,
          to = target_id,
          type = RETRY_EDGE_TYPES[route],
        }
      end
    end
  end
  return edges
end

-- Tool: dispatch-graph.
-- Validate that the role-keyed node spec designates exactly one explicit
-- graph-level terminal node. Reasoner-graph treats that node's result as
-- the graph's return value for the lead/orchestrator. The graph must be one
-- connected directed graph; disconnected components and nodes with no route
-- to the terminal are rejected at the lead-facing layer.
local function validate_graph_shape(node_specs, terminal_id)
  if type(terminal_id) ~= "string" or #terminal_id == 0 then
    return "dispatch-graph: args.terminal must designate exactly one terminal node id"
  end

  local ids = {}
  for _, spec in ipairs(node_specs) do ids[spec.id] = true end
  if not ids[terminal_id] then
    return "dispatch-graph: terminal references unknown node id `" .. tostring(terminal_id) .. "`"
  end

  for _, spec in ipairs(node_specs) do
    if spec.role == "retry" and (type(spec.routes) ~= "table"
        or type(spec.routes.retry) ~= "string"
        or type(spec.routes.pass) ~= "string"
        or type(spec.routes.exhausted) ~= "string") then
      return "dispatch-graph: retry node `" .. tostring(spec.id) ..
        "` requires routes = { retry = <node_id>, pass = <node_id>, exhausted = <node_id> }"
    end
  end

  -- Connectedness: each `dispatch-graph` call must be ONE connected directed
  -- graph. Same-scope parallel branches should be joined with deterministic
  -- fan-in nodes such as accumulate; truly separate runs should be separate
  -- dispatch-graph calls.
  local parent = {}
  for _, spec in ipairs(node_specs) do parent[spec.id] = spec.id end
  local function find(x)
    while parent[x] ~= x do
      parent[x] = parent[parent[x]]
      x = parent[x]
    end
    return x
  end
  for _, spec in ipairs(node_specs) do
    for _, edge in ipairs(spec_edges(spec)) do
      if not ids[edge.from] then
        return "dispatch-graph: node `" .. tostring(spec.id) ..
          "` references unknown dependency/route source `" .. tostring(edge.from) .. "`"
      end
      if not ids[edge.to] then
        return "dispatch-graph: node `" .. tostring(spec.id) ..
          "` references unknown dependency/route target `" .. tostring(edge.to) .. "`"
      end
      local a, b = find(edge.from), find(edge.to)
      if a ~= b then parent[a] = b end
    end
  end
  local components = {}
  for _, spec in ipairs(node_specs) do
    local r = find(spec.id)
    components[r] = components[r] or {}
    components[r][#components[r] + 1] = spec.id
  end
  local component_strs = {}
  for _, ids_in_component in pairs(components) do
    component_strs[#component_strs + 1] = "[" .. table.concat(ids_in_component, ", ") .. "]"
  end
  if #component_strs > 1 then
    return string.format(
      "dispatch-graph: graph has %d disconnected components: %s. Each "
      .. "dispatch-graph call must be one connected graph. If these "
      .. "branches belong to one task scope, connect them with a "
      .. "deterministic fan-in node such as accumulate; if they are "
      .. "genuinely separate goals, submit separate dispatch-graph calls.",
      #component_strs, table.concat(component_strs, " "))
  end

  local forward = {}
  for _, spec in ipairs(node_specs) do forward[spec.id] = {} end
  for _, spec in ipairs(node_specs) do
    for _, edge in ipairs(spec_edges(spec)) do
      if forward[edge.from] then forward[edge.from][#forward[edge.from] + 1] = edge.to end
    end
  end
  for _, spec in ipairs(node_specs) do
    if spec.id ~= terminal_id then
      local stack, seen, reachable = { spec.id }, {}, false
      while #stack > 0 do
        local id = table.remove(stack)
        if id == terminal_id then reachable = true; break end
        if not seen[id] then
          seen[id] = true
          for _, next_id in ipairs(forward[id] or {}) do stack[#stack + 1] = next_id end
        end
      end
      if not reachable then
        return "dispatch-graph: node `" .. tostring(spec.id) .. "` has no route to terminal `" .. terminal_id .. "`"
      end
    end
  end

  return nil
end

-- Build a reasoner-graph spec from the lead's role-keyed node list.
-- Each input node:
--   { id, role, agent_args = { prompt, ... }, dependencies? = { upstream_ids } }
--
-- For each node we resolve `role -> { system_prompt, tool_allowlist,
-- model? }` from `lead-workflow.role.AGENT_CONFIGS`. If unavailable
-- we fall back to passing the role straight through as an `agent`
-- reasoner with the caller's `agent_args` only — the agent reasoner
-- will fail the firing if `prompt` is missing.
local function build_graph_spec(node_specs, terminal_id)
  if type(node_specs) ~= "table" or #node_specs == 0 then
    return nil, "dispatch-graph: nodes list must be a non-empty array"
  end

  -- First pass: per-spec well-formedness (id + role + role-specific args).
  -- Validate here so the model gets a clear, fast error instead of the
  -- agent reasoner's downstream "args.prompt must be a non-empty string"
  -- — which it sees only after the graph dispatches and an agent fires.
  -- Strict providers (OpenAI Responses API) server-side-validate the
  -- schema and reject the malformed call before it reaches us; lenient
  -- ones (ollama) pass it through, so the lead-layer check is the
  -- backstop.
  for _, spec in ipairs(node_specs) do
    if type(spec) ~= "table" or type(spec.id) ~= "string"
        or type(spec.role) ~= "string" then
      return nil,
        "dispatch-graph: each node must carry { id: string, role: string }"
    end
    if spec.role == "accumulate" then
      -- No args required.
    elseif spec.role == "retry" then
      local args = type(spec.args) == "table" and spec.args or {}
      if args.max_attempts ~= nil and tonumber(args.max_attempts) == nil then
        return nil, "dispatch-graph: retry node `" .. spec.id ..
          "` args.max_attempts must be numeric when provided"
      end
    elseif spec.role == "bash_command" then
      local args = type(spec.args) == "table" and spec.args or {}
      if type(args.command) ~= "string" or #args.command == 0 then
        return nil, "dispatch-graph: bash_command node `" .. spec.id ..
          "` requires args.command"
      end
    elseif type(spec.agent_args) ~= "table"
        or type(spec.agent_args.prompt) ~= "string"
        or #spec.agent_args.prompt == 0 then
      return nil, string.format(
        "dispatch-graph: node `%s` is missing a non-empty " ..
        "`agent_args.prompt` — every sub-agent needs a task instruction. " ..
        "Upstream dependency outputs are auto-appended after the prompt, " ..
        "so phrase it as instructions that reference those inputs.", spec.id)
    end
  end

  -- Structural: exactly one terminal node. Validated before translation
  -- so the error names role-level node ids the lead recognises, not
  -- post-translation reasoner-graph internals.
  local terminal_err = validate_graph_shape(node_specs, terminal_id)
  if terminal_err then return nil, terminal_err end

  -- Resolve workspace root once; injected into every node's agent_args
  -- so file tools operate relative to the user's cwd.
  local workspace_cwd
  do
    local h = io.popen("pwd 2>/dev/null")
    if h then
      workspace_cwd = h:read("*l")
      h:close()
    end
  end

  local nodes = {}
  local edges = {}
  for _, spec in ipairs(node_specs) do
    local node
    if spec.role == "accumulate" then
      node = { id = spec.id, reasoner = "accumulate", args = type(spec.args) == "table" and spec.args or {}, role = spec.role }
    elseif spec.role == "retry" then
      node = {
        id       = spec.id,
        reasoner = "retry",
        args     = type(spec.args) == "table" and spec.args or {},
        fanout   = RETRY_FANOUT,
        role     = spec.role,
      }
    elseif spec.role == "bash_command" then
      node = { id = spec.id, reasoner = "bash_command", args = spec.args or {}, role = spec.role }
    else
      local agent_args = {}
      if type(spec.agent_args) == "table" then
        for k, v in pairs(spec.agent_args) do agent_args[k] = v end
      end
      if workspace_cwd and agent_args.cwd == nil then
        agent_args.cwd = workspace_cwd
      end

      local cfg = role_config(spec.role)
      if type(cfg) == "table" then
        if type(cfg.system_prompt) == "string" and #cfg.system_prompt > 0
            and type(agent_args.system_prompt) ~= "string" then
          agent_args.system_prompt = cfg.system_prompt
        end
        if type(cfg.tools) == "table" and type(agent_args.tool_allowlist) ~= "table" then
          agent_args.tool_allowlist = cfg.tools
        elseif type(cfg.tool_allowlist) == "table" and type(agent_args.tool_allowlist) ~= "table" then
          agent_args.tool_allowlist = cfg.tool_allowlist
        end
        if type(cfg.model) == "string" and type(agent_args.model) ~= "string" then
          agent_args.model = cfg.model
        end
        if cfg.read_only == true then
          agent_args.read_only = true
        end
      end

      node = {
        id       = spec.id,
        reasoner = "agent",
        args     = agent_args,
        -- Carries the lead's role label (explorer / builder / reviewer /
        -- …) through to the chat surface so the graph_result block can
        -- show role per node rather than the generic `agent` reasoner.
        -- reasoner-graph parses by `id` + `reasoner` only and silently
        -- ignores unknown fields; the field round-trips through
        -- agentic-loop's pending_runs without touching the wire.
        role     = spec.role,
      }
    end
    nodes[#nodes + 1] = node

    for _, edge in ipairs(spec_edges(spec)) do
      edges[#edges + 1] = edge
    end
  end

  -- Lua's empty `{}` serialises to JSON `{}` (object), not `[]` (array).
  -- reasoner-graph requires `edges` to be an array when present, so omit
  -- the key entirely when there are no edges — reasoner-graph defaults
  -- missing `edges` to no-edges, which is what we want for single-node
  -- graphs and any chain-shape where deps are encoded purely in
  -- `dependencies`.
  if #edges == 0 then
    return { terminal = terminal_id, nodes = nodes }
  end
  return { terminal = terminal_id, nodes = nodes, edges = edges }
end

-- Approval gate: write-capable roles (builder, tester, prompt-engineer,
-- any role explicitly tagged `read_only = false`) require an approved
-- plan before the lead can dispatch them. Read-only investigation
-- (explorer, reviewer, critic, reflector) is always allowed.
--
-- Mode semantics:
--   safe — require the human approval turn.
--   auto — do not block on humans; downstream tool-validator still blocks
--          actually dangerous tool calls inside the graph.
--   yolo — bypass all approval gates.
--
-- Returns nil on pass, an error string on rejection.
local function writer_denial_message(prefix, plan_state, writers)
  return string.format(
    "%s: write-capable roles require an approved plan with human approval, but %s. " ..
    "Offending node(s): %s. Recovery: switch to /safe, submit a plan with " ..
    "write-review, and wait for the user's /approve before dispatching " ..
    "the implementation graph; or restrict this dispatch to read-only " ..
    "roles (explorer/reviewer/critic/reflector).",
    prefix, plan_state, table.concat(writers, ", "))
end

local function writer_gate_state(node_specs)
  local approved = type(state.active_plan) == "table"
                   and state.active_plan.status == "approved"
  local writers = {}
  for _, spec in ipairs(node_specs or {}) do
    local cfg = role_config(spec.role)
    if type(cfg) == "table" and cfg.read_only == false then
      writers[#writers + 1] = spec.role .. " (node `" .. tostring(spec.id) .. "`)"
    end
  end
  if #writers == 0 then return approved, nil, nil end
  local plan_state
  if type(state.active_plan) ~= "table" then
    plan_state = "no plan submitted yet"
  elseif state.active_plan.status == "pending" then
    plan_state = "the active plan is awaiting user approval"
  elseif state.active_plan.status == "rejected" then
    plan_state = "the active plan was rejected by the user"
  else
    plan_state = "no approved plan in this turn"
  end
  return approved, writers, plan_state
end

local function gate_against_unapproved_plan(node_specs)
  if state.gate_mode == "auto" or state.gate_mode == "yolo" then return nil end
  local approved, writers, plan_state = writer_gate_state(node_specs)
  if approved or writers == nil then return nil end
  return writer_denial_message("dispatch-graph", plan_state, writers)
end
local function dispatch_graph(firing_id, args)
  local graph_name, name_err = validate_graph_name(args and args.name)
  if name_err then
    emit_tool_result_err(firing_id, name_err)
    return
  end

  local nodes = args and args.nodes
  local graph, err = build_graph_spec(nodes, args and args.terminal)
  if err then
    emit_tool_result_err(firing_id, err)
    return
  end

  local gate_err = gate_against_unapproved_plan(nodes)
  if gate_err then
    emit_tool_result_err(firing_id, gate_err)
    return
  end

  -- Route through agentic-loop's sub-graph queue rather than emitting
  -- the tool.invoke directly. queue_sub_graph mints the run_id AND
  -- registers it in pending_runs — without that registration the
  -- run-close handler in agentic-loop has nothing to match against,
  -- the `[spawn_graph result]` system message is never appended, the
  -- deferred-relay text is never queued, and the lead's next chat
  -- turn never sees the sub-graph's findings (so the lead has to
  -- guess and chats / redispatches instead of acting on results).
  --
  -- flush_pending_dispatches pushes the dispatch out NOW. The
  -- agentic-loop side flushes on wrap-stream delta / chat.complete
  -- normally, but the dispatch-graph tool runs entirely Lua-side
  -- without going through that path.
  local al = require("agentic-loop")
  local run_id = al.queue_sub_graph(
    { graph = graph, name = graph_name, on_node_failure = "abort" }, firing_id)
  if type(run_id) ~= "string" then
    emit_tool_result_err(firing_id,
      "dispatch-graph: agentic-loop refused the graph (queue_sub_graph returned nil)")
    return
  end
  al.flush_pending_dispatches()
  state.active_run_ids[run_id] = true
  local run = ensure_graph_run(run_id)
  if run then
    run.name = graph_name
    run.status = "queued"
    run.total_nodes = #graph.nodes
    run.updated_at = now_ms()
    for _, node in ipairs(graph.nodes or {}) do
      local tracked = ensure_graph_node(run, node.id)
      if tracked then
        tracked.reasoner = node.role or node.reasoner
        tracked.status = "pending"
      end
    end
  end

  -- The ack body carries an explicit async-contract instruction. Without
  -- it, smaller models (qwen2.5:7b observed in practice) read the bare
  -- {run_id, nodes} response as "tool returned nothing useful" and
  -- re-call dispatch-graph for the same task, producing duplicate
  -- sub-graph runs. The wording deliberately avoids "or chain another
  -- tool call" — that phrase nudges the model toward immediate
  -- redispatch — and reserves "you may dispatch a different task" as
  -- the only sanctioned chained-call path.
  local n = #graph.nodes
  emit_tool_result_ok(firing_id, {
    run_id = run_id,
    name   = graph_name,
    nodes  = n,
    notice = string.format(
      "Graph `%s` submitted (async, %d node%s, run_id=%s). " ..
      "Acknowledge briefly, then wait for graph results or call " ..
      "graph-status if the user asks for progress. Do NOT re-dispatch " ..
      "the same task.",
      graph_name, n, n == 1 and "" or "s", run_id),
  })
end

local function graph_status(firing_id, args)
  local run_id = args and args.run_id
  if run_id ~= nil then
    local run = state.graph_runs[run_id]
    if type(run) ~= "table" then
      emit_tool_result_err(firing_id, "graph-status: unknown run_id `" .. tostring(run_id) .. "`")
      return
    end
    emit_tool_result_ok(firing_id, graph_summary(run))
    return
  end

  local include_completed = args and args.include_completed == true
  local runs = {}
  for _, id in ipairs(sorted_keys(state.graph_runs)) do
    local run = state.graph_runs[id]
    if include_completed or state.active_run_ids[id] then
      runs[#runs + 1] = graph_summary(run)
    end
  end
  emit_tool_result_ok(firing_id, {
    active_count = active_run_count(),
    runs = runs,
  })
end

mark_graph_cancelled = function(run_id)
  local run = ensure_graph_run(run_id)
  if not run then return end
  run.status = "cancelled"
  run.updated_at = now_ms()
  run.completed_at = run.updated_at
  for _, node in pairs(run.nodes or {}) do
    if node.status == "pending" or node.status == "running" then
      node.status = "cancelled"
    end
  end
end

local function terminate_graph(firing_id, args)
  local run_id = args and args.run_id
  if type(run_id) ~= "string" or #run_id == 0 then
    local count, only = active_run_count()
    if count == 0 then
      emit_tool_result_ok(firing_id, {
        status = "noop",
        notice = "No active graph is running.",
      })
      return
    end
    if count > 1 then
      emit_tool_result_err(firing_id,
        "terminate-graph: multiple graphs are active; call graph-status, then pass run_id")
      return
    end
    run_id = only
  end

  if not state.active_run_ids[run_id] then
    emit_tool_result_err(firing_id, "terminate-graph: run_id `" .. tostring(run_id) .. "` is not active")
    return
  end

  state.active_run_ids[run_id] = nil
  mark_graph_cancelled(run_id)
  emit_as(SOURCE_NAME, nil, { kind = "graph.cancel", run_id = run_id })
  emit_tool_result_ok(firing_id, {
    status = "cancelled",
    run_id = run_id,
    notice = "Graph cancellation requested. Subsequent late node results for this run should be ignored.",
  })
end

-- Tool: write-review (alias submit-plan).
--
-- Blocking semantics: stores the plan + the firing_id, emits the chat-
-- surface envelope, then returns WITHOUT calling emit_tool_result_ok.
-- The agentic-loop now sits idle waiting for the deferred tool.result.
-- handle_chat_input resolves the ack when the user types /approve,
-- /reject, or any other text.
local function submit_plan(firing_id, args)
  local plan = args and args.plan
  if type(plan) ~= "string" or #plan == 0 then
    emit_tool_result_err(firing_id, "write-review: args.plan must be a non-empty string")
    return
  end

  local submitted_at = nefor.engine.now()
  local plan_id = "plan-" .. tostring(firing_id)

  if state.gate_mode == "yolo" then
    emit_tool_result_ok(firing_id, {
      status = "approved",
      notice = "YOLO mode: write-review approval gate bypassed; proceed with implementation.",
    })
    return
  elseif state.gate_mode == "auto" then
    state.active_plan = {
      plan_id           = plan_id,
      content           = plan,
      submitted_at      = submitted_at,
      pending_firing_id = nil,
      status            = "approved",
      reason            = "auto mode",
    }
    emit_as(SOURCE_NAME, nil, {
      kind         = "lead-workflow.plan.submitted",
      plan_id      = plan_id,
      plan         = plan,
      submitted_at = submitted_at,
    })
    emit_as(SOURCE_NAME, nil, {
      kind             = "lead-workflow.plan.approved",
      plan_id          = plan_id,
      submitted_at     = submitted_at,
      approved         = true,
      approval_reason  = "auto mode",
    })
    emit_tool_result_ok(firing_id, {
      status = "approved",
      notice = "AUTO mode: plan recorded and approval gate auto-resolved; proceed with implementation.",
    })
    return
  end

  -- Calling write-review while another plan is in-flight discards the
  -- earlier one. The earlier firing_id is dead-acked so the agentic-
  -- loop doesn't leak the deferred entry (this happens when an agent
  -- mis-orders calls or the test driver fires a second submit before
  -- a verdict; not a normal happy-path).
  if type(state.active_plan) == "table"
      and state.active_plan.pending_firing_id ~= nil
      and state.active_plan.pending_firing_id ~= firing_id then
    emit_tool_result_err(state.active_plan.pending_firing_id,
      "write-review: superseded by a newer plan submitted in the same turn")
  end

  state.active_plan = {
    plan_id           = plan_id,
    content           = plan,
    submitted_at      = submitted_at,
    pending_firing_id = firing_id,
    status            = "pending",
    reason            = nil,
  }

  -- Broadcast the plan-submission envelope so the chat surface can
  -- render the yellow review block. This is for UI only — the actor
  -- state above is the source of truth; the envelope is not consumed
  -- back into actor state and does not survive across sessions.
  emit_as(SOURCE_NAME, nil, {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = plan_id,
    plan         = plan,
    submitted_at = submitted_at,
  })

  -- No tool.result here — the call is blocking. handle_chat_input emits
  -- the deferred ack when the verdict arrives.
end

-- Resolve the deferred write-review ack with an approval payload.
-- Tool.result text is structured for the model: a directive, not just
-- a status code. /approve carries no reason field by spec; if the user
-- typed `/approve <text>`, the trailing text rides along as a `note`.
local function emit_verdict_approved(firing_id, note)
  local notice = "Plan approved by user. Proceed with the implementation " ..
                 "now — call dispatch-graph for the implementation graph " ..
                 "as the next tool call. The approval is valid for this " ..
                 "turn only."
  local out = { status = "approved", notice = notice }
  if type(note) == "string" and #note > 0 then out.note = note end
  emit_tool_result_ok(firing_id, out)
end

local function emit_verdict_rejected(firing_id, reason)
  local why = (type(reason) == "string" and #reason > 0)
    and ("\n\n--- reason ---\n" .. reason) or ""
  local notice = "Plan rejected by user." .. why .. "\n\n" ..
    "Revise the plan to address the feedback, then call write-review " ..
    "again. If the rejection reason is unclear, ask the user a " ..
    "clarifying question instead of re-submitting blindly. Do NOT " ..
    "dispatch the rejected plan."
  local out = { status = "rejected", notice = notice }
  if type(reason) == "string" and #reason > 0 then out.reason = reason end
  emit_tool_result_ok(firing_id, out)
end

local function emit_verdict_discarded(firing_id, comment)
  local notice = "User replied with a comment instead of a verdict; the " ..
    "submitted plan is discarded. Treat the user's reply as the next " ..
    "turn's input — answer questions, incorporate feedback, and submit " ..
    "a fresh plan via write-review when ready. Do NOT dispatch the " ..
    "discarded plan."
  local out = { status = "discarded", notice = notice }
  if type(comment) == "string" and #comment > 0 then out.comment = comment end
  emit_tool_result_ok(firing_id, out)
end

-- chat.review.respond watcher — /approve and /reject patterns.
-- Match `/approve` or `/approve <reason>` and `/reject <reason>`. The
-- patterns are lenient: surrounding whitespace is stripped. Returns
-- (verdict, reason) or nil if the text doesn't match.
local function parse_approval_command(text)
  if type(text) ~= "string" then return nil end
  local trimmed = text:match("^%s*(.-)%s*$") or ""
  local approve_reason = trimmed:match("^/approve%s*(.*)$")
  if approve_reason then
    if approve_reason == "" then return true, nil end
    return true, approve_reason
  end
  local reject_reason = trimmed:match("^/reject%s*(.*)$")
  if reject_reason then
    if reject_reason == "" then return false, nil end
    return false, reject_reason
  end
  return nil
end

local function handle_chat_input(body)
  local verdict, reason = parse_approval_command(body.text)
  local plan = state.active_plan

  -- /approve or /reject
  if verdict ~= nil then
    -- No-op when no pending plan to vote on. The user's message stays
    -- a plain chat input (it'll be handled by agentic-loop as a regular
    -- user.message); we just don't bind a verdict to it.
    if type(plan) ~= "table" or plan.status ~= "pending" then return end

    local firing_id = plan.pending_firing_id
    plan.pending_firing_id = nil
    plan.status = verdict and "approved" or "rejected"
    plan.reason = reason

    emit_as(SOURCE_NAME, nil, {
      kind             = "lead-workflow.plan.approved",
      plan_id          = plan.plan_id,
      submitted_at     = plan.submitted_at,
      approved         = verdict,
      approval_reason  = reason,
    })

    if verdict then
      emit_verdict_approved(firing_id, reason)
    else
      emit_verdict_rejected(firing_id, reason)
    end
    return
  end

  -- Non-verdict text.
  if type(plan) ~= "table" then return end

  if plan.status == "pending" then
    -- Comment arrived while the plan was awaiting a verdict. Discard
    -- the plan and ack the deferred firing with the comment text inlined.
    local firing_id = plan.pending_firing_id
    state.active_plan = nil
    emit_verdict_discarded(firing_id, body.text)
    return
  end

  -- Plan was already decided (approved / rejected). The next user
  -- message ends the verdict's validity window; flush so any further
  -- writer dispatch needs a fresh plan + approval cycle.
  state.active_plan = nil
end

local function handle_user_turn_after_verdict(_body)
  local plan = state.active_plan
  if type(plan) == "table" and plan.status ~= "pending" then
    state.active_plan = nil
  end
end

-- Replay reducers. Plan state is ephemeral
-- per session (see header doc): we do NOT rebuild state.active_plan
-- from the bus log, since carrying an approval into a new session
-- would let a writer dispatch run without a fresh user verdict.

local function reduce_plan_submitted(body)
  -- Live write-review feedback emits the chat-surface envelope once.
  -- Replay uses the persisted chat.plan.append envelope directly; regenerating
  -- it here appends historical plans at the tail on reattach.
  emit_as(SOURCE_NAME, nil, {
    kind         = "chat.plan.append",
    plan_id      = body.plan_id,
    text         = body.plan,
    submitted_at = body.submitted_at,
  })
end

local function reduce_plan_approved(_body)
  -- No-op. The chat surface tracks plan status from chat.plan.append +
  -- its own verdict envelopes; the actor's state.active_plan is not
  -- rebuilt from replay.
end

local function handle_graph_run_started(body)
  local run = ensure_graph_run(body.run_id)
  if not run then return end
  if type(body.name) == "string" and #body.name > 0 then
    run.name = body.name
  end
  run.status = "running"
  run.total_nodes = tonumber(body.total_nodes) or run.total_nodes or 0
  run.started_at = run.started_at or now_ms()
  run.updated_at = now_ms()
  state.active_run_ids[run.run_id] = true
end

local function handle_graph_node_fired(body)
  local run = ensure_graph_run(body.run_id)
  if not run then return end
  local node = ensure_graph_node(run, body.node_id)
  if not node then return end
  node.status = "running"
  node.reasoner = body.reasoner or node.reasoner
  node.firing_id = body.firing_id
  node.updated_at = now_ms()
  run.updated_at = node.updated_at
  if type(body.firing_id) == "string" and #body.firing_id > 0 then
    state.firing_to_node[body.firing_id] = {
      run_id = run.run_id,
      node_id = body.node_id,
    }
  end
end

local function handle_graph_node_tool_invoke(body)
  local run = ensure_graph_run(body.run_id)
  if not run then return end
  local node = ensure_graph_node(run, body.node_id)
  if not node then return end
  node.last_tool = body.tool_name
  node.last_tool_args = body.tool_args
  node.updated_at = now_ms()
  run.updated_at = node.updated_at
end

local function handle_tool_result_for_status(body)
  local id = body.id
  if type(id) ~= "string" or #id == 0 then return end

  local run = state.graph_runs[id]
  if type(run) == "table" and type(body.result) == "table" and body.result.status ~= nil then
    run.status = body.result.status
    run.result_status = body.result.status
    run.results = body.result.results
    run.completed_at = now_ms()
    run.updated_at = run.completed_at
    state.active_run_ids[id] = nil
    for _, node in pairs(run.nodes or {}) do
      if node.status == "pending" or node.status == "running" then
        node.status = "complete"
      end
    end
    return
  end

  local binding = state.firing_to_node[id]
  if type(binding) ~= "table" then return end
  local bound_run = state.graph_runs[binding.run_id]
  if type(bound_run) ~= "table" then return end
  local node = ensure_graph_node(bound_run, binding.node_id)
  if not node then return end
  if body.error ~= nil then
    node.status = "error"
    node.error = body.error
  else
    node.status = "complete"
  end
  node.updated_at = now_ms()
  bound_run.updated_at = node.updated_at
  state.firing_to_node[id] = nil
end

local function terminate_active_graph()
  -- Session boundary flushes the plan slot unconditionally — no
  -- approval survives across sessions. If a write-review was in-flight
  -- at session-end, the deferred firing is abandoned; the agentic-loop
  -- state is torn down with the session so there's nothing to ack into.
  state.active_plan = nil

  if next(state.active_run_ids) == nil then return end
  local ids_to_cancel = state.active_run_ids
  cancel_active_graphs()
  for run_id in pairs(ids_to_cancel) do
    nefor.log.info("lead-workflow: graph terminated on session-end", { run_id = run_id })
  end
end

local function maybe_clear_active_run(run_id)
  state.active_run_ids[run_id] = nil
end

-- tools.advertise on first <gate>.hello (best-effort; the actor still
-- works without the gate being up — tests drive tool.invoke envelopes
-- synthetically).

local advertised = false

local function lead_workflow_tool_schemas()
  return {
    {
      name        = "dispatch-graph",
      description =
        "Dispatch ONE connected directed graph of sub-agent and deterministic nodes and return its " ..
        "run_id. Use deterministic fan-in nodes such as accumulate to connect " ..
        "parallel branches that belong to one task scope. Split into separate " ..
        "dispatch-graph calls only when the work is genuinely separate and should " ..
        "return as separate runs. The graph's result.results is a dict keyed by " ..
        "the explicit graph-level terminal node id. Disconnected components and " ..
        "invalid/no/multi-terminal graphs are rejected.",
      parameters  = {
        type = "object",
        properties = {
          terminal = {
            type = "string",
            description = "Exactly one terminal node id for this graph. The terminal node output is the graph result returned to the lead/orchestrator; any output-producing node may be terminal.",
          },
          name = {
            type = "string",
            description = "Short kebab-case graph name for agent use and sidebar display. Lowercase letters/digits/hyphens only, max 20 chars, e.g. fix-graph-names.",
          },
          nodes = {
            type = "array",
            description =
              "Node specs: sub-agent roles use { id, role, agent_args, dependencies? }; " ..
              "deterministic nodes use role = accumulate, retry, or bash_command with args/routes as needed. " ..
              "Use `dependencies` and retry `routes` to wire up the connected directed graph. " ..
              "Each dependency's structured-finalize output is auto-composed " ..
              "into the dependent node's prompt as context.",
            -- OpenAI's Responses API server-side validates tool
            -- schemas and rejects arrays without `items`. Lenient
            -- providers (ollama) tolerate the omission; strict ones
            -- 400 with "array schema missing items".
            items = {
              type = "object",
              properties = {
                id           = {
                  type = "string",
                  description = "Caller-minted node id, unique within this graph. " ..
                    "Used to reference the node in other nodes' `dependencies`.",
                },
                role         = {
                  type = "string",
                  description = "Node role. Sub-agent roles: explorer, builder, reviewer. Deterministic roles: accumulate, retry, bash_command.",
                },
                args         = {
                  type        = "object",
                  description = "Args for deterministic nodes. retry accepts max_attempts (default 3, hard-capped below 7). bash_command requires command and optional cwd. accumulate takes no required args.",
                },
                agent_args   = {
                  type        = "object",
                  description = "Per-node task spec for the sub-agent. `prompt` is " ..
                    "REQUIRED — it's the task this node performs. Upstream " ..
                    "dependency outputs are auto-appended after the prompt as " ..
                    "context, so phrase the prompt as instructions that reference " ..
                    "those inputs (e.g. \"Using the [explorer_n1] output below, …\").",
                  properties  = {
                    prompt = {
                      type        = "string",
                      description = "The task for this node, as a natural-language " ..
                        "instruction. Required, non-empty. Upstream dependency " ..
                        "outputs are auto-appended after this prompt.",
                    },
                  },
                  required = { "prompt" },
                },
                dependencies = {
                  type        = "array",
                  description = "Ids of upstream nodes whose outputs this node " ..
                    "depends on. The graph waits for every listed dependency to " ..
                    "complete before dispatching this node; each dep's output is " ..
                    "available to this node as a `[<dep_id>]\\n<output>` block " ..
                    "appended after its prompt.",
                  items = { type = "string" },
                },
                routes = {
                  type        = "object",
                  description = "retry-only branch targets: { retry, pass, exhausted } mapping to node ids. The selected branch fires; unselected branches are suppressed.",
                  properties  = {
                    retry     = { type = "string" },
                    pass      = { type = "string" },
                    exhausted = { type = "string" },
                  },
                },
              },
              required = { "id", "role" },
            },
          },
        },
        required = { "name", "nodes", "terminal" },
      },
    },
    {
      name        = "write-review",
      description =
        "Submit a plan for user review. BLOCKING — the call does not " ..
        "return until the user responds. /approve resolves it with " ..
        "an approval directive (then dispatch the implementation). " ..
        "/reject resolves it with a rejection + reason (revise and " ..
        "call write-review again). Any other user reply resolves it " ..
        "as 'discarded' (treat the reply as fresh input). The approval " ..
        "is valid for one turn only — flushed by the next non-verdict " ..
        "user message and across session boundaries.",
      parameters  = {
        type = "object",
        properties = { plan = { type = "string" } },
        required = { "plan" },
      },
    },
    {
      name        = "graph-status",
      description =
        "Return status for active graph runs, or for one run_id when provided. " ..
        "Use this before answering user status questions instead of guessing.",
      parameters  = {
        type = "object",
        properties = {
          run_id = {
            type = "string",
            description = "Optional run_id to inspect. Omit to list active runs.",
          },
          include_completed = {
            type = "boolean",
            description = "When run_id is omitted, include completed/cancelled runs from this session.",
          },
        },
      },
    },
    {
      name        = "terminate-graph",
      description =
        "Cancel an active graph run. Pass run_id when multiple graphs are active. " ..
        "If exactly one graph is active, run_id may be omitted.",
      parameters  = {
        type = "object",
        properties = {
          run_id = {
            type = "string",
            description = "Active run_id to cancel. Optional only when exactly one graph is active.",
          },
        },
      },
    },
  }
end

local function advertise_tools(gate_name)
  if advertised then return end
  advertised = true
  emit_as(SOURCE_NAME, nil, {
    kind   = (gate_name or "tool-gate") .. ".tools.advertise",
    source = SOURCE_NAME,
    tools  = lead_workflow_tool_schemas(),
  })
end

local TOOL_HANDLERS = {
  ["dispatch-graph"]  = dispatch_graph,
  ["write-review"]    = submit_plan,
  ["submit-plan"]     = submit_plan,
  ["graph-status"]    = graph_status,
  ["terminate-graph"] = terminate_graph,
}

local function handle_tool_invoke(body)
  local firing_id = body.id
  if type(firing_id) ~= "string" then return end
  local name = body.name
  local handler = TOOL_HANDLERS[name]
  if not handler then
    emit_tool_result_err(firing_id, "lead-workflow: unknown tool '" .. tostring(name) .. "'")
    return
  end
  local ok, err = pcall(handler, firing_id, body.args or {})
  if not ok then
    emit_tool_result_err(firing_id,
      "lead-workflow." .. tostring(name) .. ": handler raised: " .. tostring(err))
  end
end

local function receive_msg(entry)
  if entry.origin == "step" and entry.target ~= nil then return end

  local evt = event.decode(entry)
  if evt == nil then return end
  local body = evt.body
  local kind = evt.kind

  -- Tool invocations from the gate. Live path only — during replay the
  -- gate doesn't re-issue invokes (replay_window suppresses to_plugin
  -- delivery on the gate wrapper), but we guard explicitly to be safe.
  if kind == "lead-workflow.tool.invoke" then
    if replay_window.active() then return end
    handle_tool_invoke(body)
    return
  end

  -- Plan envelopes: live feedback paints the review block. Replay relies on
  -- the persisted chat.plan.append event to restore chat order.
  if kind == "lead-workflow.plan.submitted" then
    if replay_window.active() then return end
    reduce_plan_submitted(body)
    return
  end
  if kind == "lead-workflow.plan.approved" then
    reduce_plan_approved(body)
    return
  end

  if kind == "tool-gate.mode_changed" then
    local mode = body.mode
    if mode == "normal" then mode = "safe" end
    if mode == "safe" or mode == "auto" or mode == "yolo" then state.gate_mode = mode end
    return
  end

  -- Skip the rest during replay — chat input + run-close watching are
  -- live-only concerns (they drive new bus emissions which sessions
  -- shouldn't double-record).
  if replay_window.active() then return end

  if kind == "graph.run_started" then
    handle_graph_run_started(body)
    return
  end
  if kind == "graph.node.fired" then
    handle_graph_node_fired(body)
    return
  end
  if kind == "graph.node.tool.invoke" then
    handle_graph_node_tool_invoke(body)
    return
  end

  if kind == "chat.interrupt_all" then
    cancel_active_graphs()
    return
  end

  if kind == "chat.review.respond" then
    handle_chat_input(body)
    return
  end
  if kind == "chat.input.submit" then
    handle_user_turn_after_verdict(body)
    return
  end

  -- Slash commands `/approve [reason]` and `/reject [reason]` arrive
  -- as `chat.command` envelopes (the chat surface routes any unknown
  -- slash through this generic kind). We synthesise the same shape
  -- handle_chat_input expects so the existing parser handles both
  -- entry points identically.
  if kind == "chat.command" then
    local name = body.name
    if name == "approve" or name == "reject" then
      local args = body.args or ""
      local text = "/" .. name
      if type(args) == "string" and #args > 0 then
        text = text .. " " .. args
      end
      handle_chat_input({ text = text })
    end
    return
  end

  -- Watch for the orchestrator's run-close envelope so we can clear
  -- active_run_ids when graphs finish naturally (mirrors how
  -- agentic-loop tracks pending_runs). The wire shape is
  -- `tool.result { id=run_id, result: { status, results } }`.
  if kind == "tool.result" then
    handle_tool_result_for_status(body)
    local id = body.id
    if type(id) == "string"
        and type(body.result) == "table"
        and body.result.status ~= nil then
      maybe_clear_active_run(id)
    end
    return
  end

  -- Tool-gate hello — advertise our tools on first sight. Narrowed
  -- to `tool-gate.hello` specifically: matching any `*.hello` would
  -- mean the first non-gate plugin to say hello (e.g. nefor-combinators)
  -- silently locks the advertised flag and tool-gate never sees the
  -- ad. The advertise_tools target must always be tool-gate.
  if kind == "tool-gate.hello" then
    advertise_tools("tool-gate")
    return
  end
end

-- Bus subscriptions — session_end + replay markers.
if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.session_end", function(_entry)
    terminate_active_graph()
  end)
end

return {
  name        = "lead-workflow",
  receive_msg = receive_msg,
  send_msg    = function(_) end,

  -- Public: pre-execution gate check for `dispatch-graph` args. Returns
  -- nil if the call would be allowed, or a rejection-reason string if
  -- it would be auto-rejected by `dispatch_graph` (writer roles without
  -- an approved plan). The tool-validator uses this to suppress the
  -- approval popup for invocations that are guaranteed to fail —
  -- otherwise the UX is "agent calls tool → user clicks approve →
  -- chat shows rejection", which feels broken. Same semantics as the
  -- internal check; safe to call multiple times per invocation.
  gate_against_unapproved_plan = gate_against_unapproved_plan,

  _internals = {
    state = state,
    SOURCE_NAME = SOURCE_NAME,
    -- Direct handler hooks for the test driver. Tests fire envelopes
    -- through receive_msg; these helpers exist only when the test
    -- needs to skip the wire-decode boilerplate.
    handle_tool_invoke    = handle_tool_invoke,
    handle_chat_input     = handle_chat_input,
    reduce_plan_submitted = reduce_plan_submitted,
    reduce_plan_approved  = reduce_plan_approved,
    parse_approval_command = parse_approval_command,
    terminate_active_graph = terminate_active_graph,
    reset = function()
      state.active_run_ids = {}
      state.graph_runs = {}
      state.firing_to_node = {}
      state.active_plan = nil
      state.gate_mode = "safe"
      advertised = false
    end,
  },
}
