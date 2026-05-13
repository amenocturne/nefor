-- starter/lead-workflow/init.lua — lead-workflow actor.
--
-- Owns three pieces of state on top of the lead's existing chat-side
-- agentic-loop (see `agentic-loop/init.lua`):
--
--   1. The currently-executing graph (if any) — its run_id, so a
--      session-end can cancel it cleanly.
--   2. The active plan + planApproved flag — persisted to the session
--      log via two custom envelopes (`lead-workflow.plan.submitted`,
--      `lead-workflow.plan.approved`) and replayed on /resume.
--   3. Per-firing await-approval state — which firing is blocked on
--      which plan_id, so a `/approve` / `/reject ...` user submit can
--      be routed back to the right tool.invoke.
--
-- ## Tools the lead invokes
--
-- Advertised to tool-gate as a virtual source `lead-workflow`. The
-- gate forwards `tool-gate.tool.invoke` → `lead-workflow.tool.invoke`,
-- this actor handles the forwarded envelopes and emits `tool.result`
-- back. The gate's id-rewriting machinery handles the round-trip.
--
--   * `dispatch-graph` — args:
--       { nodes = [{ id, role, agent_args, dependencies? }, ...] }
--     Builds a reasoner-graph spec from the role-keyed nodes (looking
--     up `role -> reasoner config` from `lead-workflow.role.AGENT_CONFIGS`
--     when available; falling back to a default `agent`-reasoner shape
--     when the role module is not yet loaded), submits it via the same
--     `tool.invoke{name=spawn_graph}` shape `agentic-loop`'s
--     `submit_orchestrator_run` uses. Returns the minted run_id.
--
--   * `write-review` (alias `submit-plan`) — args:
--       { plan = <string> }
--     Persists the plan to `<DATA>/plans/<plan_id>.md`, broadcasts
--     `lead-workflow.plan.submitted { plan_id, plan, submitted_at }`,
--     returns `{ plan_id }`.
--
--   * `await-approval` — args:
--       { plan_id = <string> }
--     Stores the firing as the pending approval-callback. When a later
--     `chat.input.submit { text }` matches `/approve` or `/reject ...`
--     for the currently-active plan, this actor:
--       (a) emits `lead-workflow.plan.approved { plan_id, approved,
--           approval_reason? }`,
--       (b) replies `tool.result { id=firing_id, output={...} }` so
--           the lead's tool call returns.
--
-- ## Persistence + replay
--
-- Both `lead-workflow.plan.submitted` and `lead-workflow.plan.approved`
-- envelopes ride the regular bus, so sessions persists them
-- automatically. On /resume the replay window re-fires them; this
-- actor's reducer handles them identically in live and replay paths
-- (multi-plan: the latest plan_id wins; an `approved` updates the
-- matching plan).
--
-- ## Termination on session exit
--
-- Subscribes to `sessions.session_end`. If `state.active_run_id ~= nil`,
-- emits `<reasoner-graph>.cancel { run_id }` and appends a system
-- message `[Graph terminated by user — session exit]` to chat history
-- so the model sees it on the next turn.

local json = nefor.json

local envelope      = require("core.envelope")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit_to = envelope.emit_to
local next_id = envelope.next_id

local state = {
  -- The in-flight graph's run_id; nil when no graph is running.
  ---@type string|nil
  active_run_id = nil,

  -- Most-recently-submitted plan + its approval state. Latest plan_id
  -- wins; a `lead-workflow.plan.approved { plan_id }` only updates if
  -- the plan_id matches active_plan.plan_id. (Older plans are
  -- considered superseded.)
  ---@type table|nil
  active_plan = nil,

  -- await-approval bookkeeping. firing_id -> { plan_id }. When a user
  -- input matches `/approve` or `/reject ...`, the matching firing is
  -- resolved + cleared.
  pending_approvals = {},

  -- Track which plan_ids we've already persisted so the replay path
  -- doesn't re-write the file (the in-process file write is best-effort
  -- and lossless to skip on replay).
  persisted_plans = {},
}

local SOURCE_NAME = "lead-workflow"

-- Delegates to `nefor.fs.data_root()` — the engine's canonical resolved
-- data directory (CLI flag > `NEFOR_DATA_DIR` env var > XDG default).
local function compute_data_root()
  return nefor.fs.data_root()
end

local function plans_dir()
  local root = compute_data_root()
  if not root then return nil end
  return root .. "/plans"
end

local function ensure_dir(path)
  nefor.fs.mkdir_p(path)
end

local function plan_path_for(plan_id)
  local dir = plans_dir()
  if not dir then return nil end
  return dir .. "/" .. plan_id .. ".md"
end

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

-- Tool: dispatch-graph.
-- Validate that the role-keyed node spec has exactly one terminal
-- (sink) node — one node id that no other node lists in its
-- `dependencies`. Reasoner-graph treats the sole terminal node's result
-- as the graph's return value; without exactly one, that contract is
-- ambiguous (zero = cycle, more than one = no canonical result), so
-- dispatch-graph rejects at the lead-facing layer rather than letting
-- reasoner-graph receive an ill-shaped DAG. Reasoner-graph itself stays
-- a primitive — the role-aware shape is enforced here.
--
-- Returns nil on success, an error string on failure.
local function validate_terminal_count(node_specs)
  local has_successor = {}
  for _, spec in ipairs(node_specs) do
    if type(spec.dependencies) == "table" then
      for _, dep_id in ipairs(spec.dependencies) do
        has_successor[dep_id] = true
      end
    end
  end

  local sink_count = 0
  for _, spec in ipairs(node_specs) do
    if not has_successor[spec.id] then sink_count = sink_count + 1 end
  end
  if sink_count == 0 then
    return "dispatch-graph: graph has 0 terminal nodes — every node is "
        .. "depended on by another. Likely cause: a cycle in dependencies. "
        .. "Break the cycle, or move loop-guard logic into a single "
        .. "counter-node graph."
  end

  -- Connectedness: each `dispatch-graph` call must be ONE connected DAG.
  -- Disconnected components = N independent tasks bundled into one run,
  -- which loses the UX wins of parallel sidebar rows + independent
  -- tool.result returns. Lead should call dispatch-graph N times instead.
  -- Union-find over dependency edges; count distinct roots.
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
    if type(spec.dependencies) == "table" then
      for _, dep_id in ipairs(spec.dependencies) do
        if parent[dep_id] ~= nil then
          local a, b = find(spec.id), find(dep_id)
          if a ~= b then parent[a] = b end
        end
      end
    end
  end
  local components = {}
  for _, spec in ipairs(node_specs) do
    local r = find(spec.id)
    components[r] = components[r] or {}
    components[r][#components[r] + 1] = spec.id
  end
  local component_strs = {}
  for _, ids in pairs(components) do
    component_strs[#component_strs + 1] = "[" .. table.concat(ids, ", ") .. "]"
  end
  if #component_strs > 1 then
    return string.format(
      "dispatch-graph: graph has %d disconnected components: %s. Each "
      .. "independent task must be dispatched as its own dispatch-graph "
      .. "call so it gets its own run_id, appears as a separate row in "
      .. "the UI, and its result comes back independently. Combine "
      .. "nodes into one graph only when they share data dependencies.",
      #component_strs, table.concat(component_strs, " "))
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
local function build_graph_spec(node_specs)
  if type(node_specs) ~= "table" or #node_specs == 0 then
    return nil, "dispatch-graph: nodes list must be a non-empty array"
  end

  -- First pass: per-spec well-formedness (id + role present).
  for _, spec in ipairs(node_specs) do
    if type(spec) ~= "table" or type(spec.id) ~= "string"
        or type(spec.role) ~= "string" then
      return nil,
        "dispatch-graph: each node must carry { id: string, role: string }"
    end
  end

  -- Structural: exactly one terminal node. Validated before translation
  -- so the error names role-level node ids the lead recognises, not
  -- post-translation reasoner-graph internals.
  local terminal_err = validate_terminal_count(node_specs)
  if terminal_err then return nil, terminal_err end

  local nodes = {}
  local edges = {}
  for _, spec in ipairs(node_specs) do
    local agent_args = {}
    if type(spec.agent_args) == "table" then
      for k, v in pairs(spec.agent_args) do agent_args[k] = v end
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
    end

    nodes[#nodes + 1] = {
      id       = spec.id,
      reasoner = "agent",
      args     = agent_args,
    }

    if type(spec.dependencies) == "table" then
      for _, dep_id in ipairs(spec.dependencies) do
        edges[#edges + 1] = { from = dep_id, to = spec.id }
      end
    end
  end

  -- Lua's empty `{}` serialises to JSON `{}` (object), not `[]` (array).
  -- reasoner-graph requires `edges` to be an array when present, so omit
  -- the key entirely when there are no edges — reasoner-graph defaults
  -- missing `edges` to no-edges, which is what we want for single-node
  -- graphs and any chain-shape where deps are encoded purely in
  -- `dependencies`.
  if #edges == 0 then
    return { nodes = nodes }
  end
  return { nodes = nodes, edges = edges }
end

-- Approval gate: write-capable roles (builder, tester, prompt-engineer,
-- any role explicitly tagged `read_only = false`) require an approved
-- plan before the lead can dispatch them. Read-only investigation
-- (explorer, reviewer, critic, reflector) is always allowed.
--
-- Returns nil on pass, an error string on rejection.
local function gate_against_unapproved_plan(node_specs)
  local approved = type(state.active_plan) == "table"
                   and state.active_plan.approved == true
  if approved then return nil end
  local writers = {}
  for _, spec in ipairs(node_specs) do
    local cfg = role_config(spec.role)
    if type(cfg) == "table" and cfg.read_only == false then
      writers[#writers + 1] = spec.role .. " (node `" .. tostring(spec.id) .. "`)"
    end
  end
  if #writers == 0 then return nil end
  local plan_state
  if type(state.active_plan) ~= "table" then
    plan_state = "no plan submitted yet"
  elseif state.active_plan.approved == nil then
    plan_state = "plan `" .. tostring(state.active_plan.plan_id)
                 .. "` is awaiting user approval"
  elseif state.active_plan.approved == false then
    plan_state = "plan `" .. tostring(state.active_plan.plan_id)
                 .. "` was rejected by the user"
  else
    plan_state = "unknown plan state"
  end
  return string.format(
    "dispatch-graph: write-capable roles require an approved plan, but %s. "
    .. "Offending node(s): %s. Either restrict this dispatch to read-only roles "
    .. "(explorer/reviewer/critic/reflector — investigation), or call write-review "
    .. "+ await-approval first, then dispatch the implementation graph.",
    plan_state, table.concat(writers, ", "))
end

local function dispatch_graph(firing_id, args)
  local nodes = args and args.nodes
  local graph, err = build_graph_spec(nodes)
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
    { graph = graph, on_node_failure = "abort" }, firing_id)
  if type(run_id) ~= "string" then
    emit_tool_result_err(firing_id,
      "dispatch-graph: agentic-loop refused the graph (queue_sub_graph returned nil)")
    return
  end
  al.flush_pending_dispatches()
  state.active_run_id = run_id

  emit_tool_result_ok(firing_id, {
    run_id = run_id,
    nodes  = #graph.nodes,
  })
end

-- Tool: write-review (alias submit-plan).
local function persist_plan(plan_id, plan_text)
  if state.persisted_plans[plan_id] then return end
  state.persisted_plans[plan_id] = true
  local dir = plans_dir()
  if not dir then return end
  ensure_dir(dir)
  local path = plan_path_for(plan_id)
  if not path then return end
  local fh, oerr = io.open(path, "w")
  if not fh then
    nefor.log.warn("lead-workflow: failed to open plan file", {
      plan_id = plan_id, path = path, error = tostring(oerr),
    })
    return
  end
  fh:write(plan_text)
  fh:close()
end

local function submit_plan(firing_id, args)
  local plan = args and args.plan
  if type(plan) ~= "string" or #plan == 0 then
    emit_tool_result_err(firing_id, "write-review: args.plan must be a non-empty string")
    return
  end

  local plan_id = next_id("plan")
  local submitted_at = nefor.engine.now()

  -- Best-effort file write (live path only — replayed envelopes are
  -- already on disk somewhere, no need to overwrite). We treat the
  -- envelope as the source of truth on resume; the file is a
  -- user-facing artefact.
  if not replay_window.active() then
    persist_plan(plan_id, plan)
  end

  -- Update local state. The bus envelope below carries the same data;
  -- our reducer (handle_plan_submitted) is what records it on replay.
  -- Direct local mutation here mirrors what the reducer would do, so
  -- the live path doesn't double-process.
  state.active_plan = {
    plan_id      = plan_id,
    text         = plan,
    submitted_at = submitted_at,
    approved     = nil,
  }

  -- Broadcast the plan-submission envelope. Sessions persists it; on
  -- /resume replay re-fires it through this actor's bus subscription
  -- and the reducer rebuilds active_plan.
  emit_as(SOURCE_NAME, nil, {
    kind         = "lead-workflow.plan.submitted",
    plan_id      = plan_id,
    plan         = plan,
    submitted_at = submitted_at,
  })

  emit_tool_result_ok(firing_id, { plan_id = plan_id })
end

-- Tool: await-approval.
local function await_approval(firing_id, args)
  local plan_id = args and args.plan_id
  if type(plan_id) ~= "string" or #plan_id == 0 then
    emit_tool_result_err(firing_id, "await-approval: args.plan_id must be a non-empty string")
    return
  end

  -- Edge cases: if the plan is already approved/rejected (e.g. user
  -- approved while the lead was still processing), resolve immediately.
  if type(state.active_plan) == "table"
      and state.active_plan.plan_id == plan_id
      and state.active_plan.approved ~= nil then
    emit_tool_result_ok(firing_id, {
      plan_id  = plan_id,
      approved = state.active_plan.approved,
      reason   = state.active_plan.approval_reason,
    })
    return
  end

  state.pending_approvals[firing_id] = { plan_id = plan_id }
end

-- Resolve any pending await-approval firings that match the given
-- plan_id with the given approval verdict.
local function resolve_pending_approvals(plan_id, approved, reason)
  local resolved = {}
  for fid, pending in pairs(state.pending_approvals) do
    if pending.plan_id == plan_id then
      resolved[#resolved + 1] = fid
    end
  end
  for _, fid in ipairs(resolved) do
    state.pending_approvals[fid] = nil
    emit_tool_result_ok(fid, {
      plan_id  = plan_id,
      approved = approved,
      reason   = reason,
    })
  end
end

-- chat.input.submit watcher — /approve and /reject patterns.
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
  if type(state.active_plan) ~= "table" then return end
  if state.active_plan.approved ~= nil then return end
  local verdict, reason = parse_approval_command(body.text)
  if verdict == nil then return end

  local plan_id = state.active_plan.plan_id

  -- Update local state inline (the live path mirrors what the reducer
  -- does on replay).
  state.active_plan.approved = verdict
  state.active_plan.approval_reason = reason

  -- Broadcast the approval envelope so sessions persists it for replay.
  local body_out = {
    kind     = "lead-workflow.plan.approved",
    plan_id  = plan_id,
    approved = verdict,
  }
  if reason ~= nil then body_out.approval_reason = reason end
  emit_as(SOURCE_NAME, nil, body_out)

  resolve_pending_approvals(plan_id, verdict, reason)
end

-- Replay reducers — same shape as live but no side effects (no file
-- write, no resolution callbacks since pending_approvals didn't
-- survive the process exit).

local function reduce_plan_submitted(body)
  local plan_id = body.plan_id
  if type(plan_id) ~= "string" then return end
  state.active_plan = {
    plan_id      = plan_id,
    text         = body.plan,
    submitted_at = body.submitted_at,
    approved     = nil,
  }

  -- Re-emit the chat-surface envelope on every plan.submitted handling,
  -- live and replay both. chat.lua's reducer keys plan entries by
  -- plan_id and is idempotent, so a duplicate (live persistence + replay
  -- re-emit on the next /resume) is a no-op visually. Without this the
  -- yellow plan box never reappears after /resume even though the
  -- actor's active_plan state is restored.
  emit_as(SOURCE_NAME, nil, {
    kind         = "chat.plan.append",
    plan_id      = plan_id,
    text         = body.plan,
    submitted_at = body.submitted_at,
  })
end

local function reduce_plan_approved(body)
  local plan_id = body.plan_id
  if type(plan_id) ~= "string" then return end
  if type(state.active_plan) ~= "table" then return end
  if state.active_plan.plan_id ~= plan_id then return end
  state.active_plan.approved = body.approved
  state.active_plan.approval_reason = body.approval_reason
end

local function terminate_active_graph()
  if state.active_run_id == nil then return end
  local run_id = state.active_run_id
  state.active_run_id = nil

  -- Broadcast (target = nil) rather than target reasoner-graph: every
  -- in-flight agent reasoner under this run also needs to see the
  -- envelope so it can interrupt its provider stream + close its
  -- firing (sub-graph cancel propagation). The reasoner-graph
  -- binary still receives the broadcast and processes it the same way.
  emit_as(SOURCE_NAME, nil, { kind = "graph.cancel", run_id = run_id })
  emit_to("nefor-tui", {
    kind = "chat.message.append",
    role = "system",
    text = "[Graph terminated by user — session exit]",
  })
  -- Also clear pending approvals; the lead's await-approval tool calls
  -- shouldn't survive the session boundary (they're per-firing, and the
  -- firings are gone).
  state.pending_approvals = {}
end

-- Run-close watcher — clear active_run_id when the in-flight graph
-- finishes on its own.
local function maybe_clear_active_run(run_id)
  if state.active_run_id ~= nil and state.active_run_id == run_id then
    state.active_run_id = nil
  end
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
        "Dispatch ONE connected sub-graph of role-keyed sub-agents and return its " ..
        "run_id. For N independent tasks call dispatch-graph N times — each call " ..
        "gets its own run_id, appears as a separate row in the UI, and returns its " ..
        "result independently when finished. The graph's result.results is a dict " ..
        "keyed by terminal (sink) node id; multi-sink within ONE connected DAG is " ..
        "fine (e.g. explore → build, explore → test = two sinks sharing a root). " ..
        "Disconnected components and cycles are rejected.",
      parameters  = {
        type = "object",
        properties = {
          nodes = {
            type = "array",
            description =
              "Role-keyed node specs: { id, role, agent_args, dependencies? }. " ..
              "Use `dependencies` (array of node ids) to wire up the DAG. " ..
              "Each dependency's structured-finalize output is auto-composed " ..
              "into the dependent node's prompt as context.",
          },
        },
        required = { "nodes" },
      },
    },
    {
      name        = "write-review",
      description = "Submit a plan for user review. Persists the plan and surfaces it to the chat.",
      parameters  = {
        type = "object",
        properties = { plan = { type = "string" } },
        required = { "plan" },
      },
    },
    {
      name        = "await-approval",
      description = "Block until the user approves or rejects the plan with the given plan_id.",
      parameters  = {
        type = "object",
        properties = { plan_id = { type = "string" } },
        required = { "plan_id" },
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
  ["await-approval"]  = await_approval,
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
  handler(firing_id, body.args or {})
end

local function receive_msg(entry)
  if entry.origin == "step" and entry.target ~= nil then return end

  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local body = decoded.body
  local kind = body.kind
  if type(kind) ~= "string" then return end

  -- Tool invocations from the gate. Live path only — during replay the
  -- gate doesn't re-issue invokes (replay_window suppresses to_plugin
  -- delivery on the gate wrapper), but we guard explicitly to be safe.
  if kind == "lead-workflow.tool.invoke" then
    if replay_window.active() then return end
    handle_tool_invoke(body)
    return
  end

  -- Plan envelopes — used live AND on replay to rebuild state.
  -- `env.replay` rides per-envelope from sessions's replay path, but
  -- our reducer is identical for both, so we just consume both.
  if kind == "lead-workflow.plan.submitted" then
    reduce_plan_submitted(body)
    return
  end
  if kind == "lead-workflow.plan.approved" then
    reduce_plan_approved(body)
    return
  end

  -- Skip the rest during replay — chat input + run-close watching are
  -- live-only concerns (they drive new bus emissions which sessions
  -- shouldn't double-record).
  if replay_window.active() then return end

  if kind == "chat.input.submit" then
    handle_chat_input(body)
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
  -- active_run_id when the graph finishes naturally (mirrors how
  -- agentic-loop tracks pending_runs). The wire shape is
  -- `tool.result { id=run_id, result: { status, results } }`.
  if kind == "tool.result" then
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
    plan_path_for         = plan_path_for,
    plans_dir             = plans_dir,
    terminate_active_graph = terminate_active_graph,
    reset = function()
      state.active_run_id = nil
      state.active_plan = nil
      state.pending_approvals = {}
      state.persisted_plans = {}
      advertised = false
    end,
  },
}
