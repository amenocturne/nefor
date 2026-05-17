-- starter/lead-workflow/init.lua — lead-workflow actor.
--
-- Owns two pieces of state on top of the lead's chat-side agentic-loop
-- (see `agentic-loop/init.lua`):
--
--   1. The currently-executing graph (if any) — its run_id, so a
--      session-end can cancel it cleanly.
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
--   * any other user message   — emit tool.result = "user replied with
--                                a comment, plan discarded — address
--                                their reply: <text>". active_plan is
--                                cleared. The same chat.input.submit
--                                ALSO lands as a regular user.message
--                                via agentic-loop's normal path, so the
--                                lead's next inference has both.
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
--       { nodes = [{ id, role, agent_args, dependencies? }, ...] }
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
-- ## Termination on session exit
--
-- Subscribes to `sessions.session_end`. If active graphs exist,
-- emits `<reasoner-graph>.cancel { run_id }` and appends a system
-- message `[Graph terminated by user — session exit]` to chat history
-- so the model sees it on the next turn. Also clears `state.active_plan`
-- so a resumed/new session starts with no carry-over approval.

local json = nefor.json

local envelope      = require("core.envelope")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit    = envelope.emit

local state = {
  -- In-flight graph run_ids; empty when no graphs are running.
  ---@type table<string, boolean>
  active_run_ids = {},

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
}

local SOURCE_NAME = "lead-workflow"

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

  -- First pass: per-spec well-formedness (id + role + agent_args.prompt).
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
    if type(spec.agent_args) ~= "table"
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
  local terminal_err = validate_terminal_count(node_specs)
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

    nodes[#nodes + 1] = {
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
                   and state.active_plan.status == "approved"
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
  elseif state.active_plan.status == "pending" then
    plan_state = "the active plan is awaiting user approval"
  elseif state.active_plan.status == "rejected" then
    plan_state = "the active plan was rejected by the user"
  else
    plan_state = "no approved plan in this turn"
  end
  return string.format(
    "dispatch-graph: write-capable roles require an approved plan, but %s. "
    .. "Offending node(s): %s. Either restrict this dispatch to read-only "
    .. "roles (explorer/reviewer/critic/reflector — investigation), or "
    .. "submit a plan with write-review and wait for the user's /approve "
    .. "before dispatching the implementation graph.",
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
  state.active_run_ids[run_id] = true

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
    nodes  = n,
    notice = string.format(
      "Graph submitted (async, %d node%s, run_id=%s). " ..
      "If you have more independent tasks to dispatch, call dispatch-graph " ..
      "again NOW in this same turn — do not wait. Once all dispatches are " ..
      "done, acknowledge briefly to the user and stop. Results arrive " ..
      "later as `[spawn_graph(run_id=...) result]` messages. " ..
      "Do NOT re-dispatch the same task.",
      n, n == 1 and "" or "s", run_id),
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

  local submitted_at = nefor.engine.now()

  state.active_plan = {
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
    -- the plan, ack the deferred firing with the comment text inlined,
    -- and clear active_plan. The same chat.input.submit also rides
    -- through agentic-loop as a normal user.message, so the lead's
    -- next inference sees the user's text on both channels — the
    -- redundancy is harmless and the tool.result keeps the agent from
    -- assuming the plan still applies.
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

-- Replay reducers — UI re-emission only. Plan state is ephemeral
-- per session (see header doc): we do NOT rebuild state.active_plan
-- from the bus log, since carrying an approval into a new session
-- would let a writer dispatch run without a fresh user verdict.

local function reduce_plan_submitted(body)
  -- Re-emit the chat-surface envelope so the yellow review block
  -- reappears after /resume. chat.lua keys plan entries by submission
  -- order; no plan_id is needed.
  emit_as(SOURCE_NAME, nil, {
    kind         = "chat.plan.append",
    text         = body.plan,
    submitted_at = body.submitted_at,
  })
end

local function reduce_plan_approved(_body)
  -- No-op. The chat surface tracks plan status from chat.plan.append +
  -- its own verdict envelopes; the actor's state.active_plan is not
  -- rebuilt from replay.
end

local function terminate_active_graph()
  -- Session boundary flushes the plan slot unconditionally — no
  -- approval survives across sessions. If a write-review was in-flight
  -- at session-end, the deferred firing is abandoned; the agentic-loop
  -- state is torn down with the session so there's nothing to ack into.
  state.active_plan = nil

  if next(state.active_run_ids) == nil then return end
  local ids_to_cancel = state.active_run_ids
  state.active_run_ids = {}

  -- Broadcast (target = nil) rather than target reasoner-graph: every
  -- in-flight agent reasoner under this run also needs to see the
  -- envelope so it can interrupt its provider stream + close its
  -- firing (sub-graph cancel propagation). The reasoner-graph
  -- binary still receives the broadcast and processes it the same way.
  for run_id in pairs(ids_to_cancel) do
    emit_as(SOURCE_NAME, nil, { kind = "graph.cancel", run_id = run_id })
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
                  description = "Sub-agent role. Drives the system prompt, model, and " ..
                    "tool allowlist. Roles available in this starter: explorer " ..
                    "(read-only investigation), builder (writes code; requires an " ..
                    "approved plan), reviewer (read-only critique).",
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
              },
              required = { "id", "role", "agent_args" },
            },
          },
        },
        required = { "nodes" },
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

  local ok, decoded = pcall(json.decode, entry.payload)
  if not ok then return end
  local body = decoded.body
  local kind = body.kind

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
      state.active_plan = nil
      advertised = false
    end,
  },
}
