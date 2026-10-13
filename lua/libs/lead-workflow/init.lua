-- libs/lead-workflow/init.lua — lead-workflow actor (mechanism).
--
-- Config-agnostic: derives every path from env/globals the engine sets
-- (NEFOR_CONFIG_DIR, NEFOR_DATA_DIR, NEFOR_REVIEW_HOOK), never from its
-- own file location, so it runs identically from the shared lua tree.
-- Config compositions load this mechanism directly through
-- `libs.lead-workflow`.
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
-- Inline review resolves when the user replies in chat:
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
--   * `write-review` — args: { plan = <string> }
--     Stores the plan in `state.active_plan`, broadcasts
--     `lead-workflow.plan.submitted { plan, submitted_at }` for the
--     chat surface to render the yellow review block. Does NOT emit
--     tool.result — the agentic-loop blocks until the user verdict
--     resolves the deferred ack.
--
--   * `mag` — write, compile, and apply MAG workflow graphs.
--
--   * `mag-status` — report active/recent graph run state.
--
--   * `mag-terminate` — cancel an active graph run by run_id.
--
-- ## Termination on session exit
--
-- Subscribes to `sessions.session_end`. If any lead-owned kernel runs are
-- active, emits one `mag.kill_run { run_id }` per run and archives each as
-- canceled. Also clears `state.active_plan` so a resumed/new session starts
-- with no carry-over approval.

local json = nefor.json

local mag            = require("libs.mag-workspace")
local run_projection = require("libs.mag-run-projection")
local RunRegistry    = require("libs.lead-workflow.run-registry")
local chat_emitter   = require("libs.chat-emitter")
local sessions       = require("libs.sessions")
local envelope       = require("core.envelope")
local replay_window  = require("core.replay_window")
local results_lib    = require("libs.agentic-loop.results")
local tool_display   = require("libs.chat.tool_display")
local tool           = require("libs.tool")
local approval       = require("libs.tool.approval")
local shell          = require("core.shell")
local model_snapshot = require("libs.model-snapshot")

local function display_field(label, source, path, kind, extra)
  local value = { label = label, select = { source = source, path = path }, kind = kind }
  for key, item in pairs(extra or {}) do value[key] = item end
  return value
end

local function run_label_field(required)
  return {
    label = "run",
    select = {
      source = "result",
      path = "invocation_label",
      fallback = { source = "args", path = "run_id" },
    },
    kind = "scalar",
    omit = required and nil or "missing",
  }
end

local function display_contract(label, primary, fields, result_kind, result_text, result_fields, lifecycle)
  return {
    compact = { label = label, primary = primary },
    expanded = { label = label, fields = tool_display.fields(fields) },
    result = { kind = result_kind, text = result_text, fields = tool_display.fields(result_fields) },
    lifecycle = lifecycle,
  }
end

local current_invocation
local publication_failure = {}
local function emit_as(from, target, body)
  if not current_invocation then return envelope.emit_as(from, target, body) end
  local ok, err, phase = envelope.checked_emit_as(from, target, body)
  if not ok then
    if phase ~= "encoding" and not (nefor.engine.error_kind and nefor.engine.error_kind(err)) then
      error(err, 0)
    end
    error({ tag = publication_failure, cause = err }, 0)
  end
  return true
end
local function emit(target, body) return emit_as("engine", target, body) end

local monotonic_now_ms = function()
  if type(nefor.now_ms) == "function" then return nefor.now_ms() end
  return nil
end

local run_registry = RunRegistry.new({
  terminal_limit = 64,
  tombstone_limit = 256,
  mint_id = function()
    return "mag-run-" .. envelope.uuid_lite()
  end,
  now = function()
    if nefor.engine and type(nefor.engine.now) == "function" then
      return nefor.engine.now()
    end
    return os.date("!%Y-%m-%dT%H:%M:%SZ")
  end,
})

local dependency_module_roots = {}
local project_build = nil
local agent_system = nil
local ambient_context = nil
local resolve_model_snapshot = nil
-- What a capability call means under an approval policy, as its tool says:
-- `decide_capability(tool_name, policy, args) -> result, reason`, or nothing
-- when no tool by that name says. mag-apply decides a workflow's capability
-- nodes (commands, worktrees) from it, so a node and the tool with the same
-- capability are judged by one definition.
local decide_capability = nil

local SYNC_COMPLETION_GRACE_MS = 3000
local TERMINATION_CONFIRM_TIMEOUT_MS = 30000
local grace_scheduler
local termination_scheduler

local function default_grace_scheduler(delay_ms, callback)
  local seconds = math.max(delay_ms, 1) / 1000
  local timer = nefor.process.spawn({
    cmd = "sh",
    args = { "-c", "sleep " .. tostring(seconds) },
    on_exit = callback,
  })
  return function()
    pcall(function() timer:kill() end)
  end
end

local function copy_roots(roots)
  local copy = {}
  for i = 1, #roots do copy[i] = roots[i] end
  return copy
end

local function validate_dependency_module_roots(roots)
  if type(roots) ~= "table" then
    error("lead-workflow: dependency_module_roots must be a list", 3)
  end
  local count = 0
  for key, value in pairs(roots) do
    if type(key) ~= "number" or key % 1 ~= 0 or key < 1 then
      error("lead-workflow: dependency_module_roots must be a dense list", 3)
    end
    count = count + 1
    if type(value) ~= "string" or value == "" then
      error("lead-workflow: dependency_module_roots entries must be non-empty strings", 3)
    end
  end
  for index = 1, count do
    if roots[index] == nil then
      error("lead-workflow: dependency_module_roots must be a dense list", 3)
    end
  end
end

local function module_roots_for(ws)
  local roots = copy_roots(dependency_module_roots)
  roots[#roots + 1] = ws
  return roots
end

local state = {
  -- The in-flight graph's run_id; nil when no graph is running.
  ---@type string|nil
  active_run_id = nil,

  -- Compatibility aliases onto the focused registry. The registry owns
  -- active/terminal state, retention, waiter correlations, and provenance.
  active_runs = run_registry.active_runs,
  completed_runs = run_registry.completed_runs,
  completed_run_limit = 64,

  -- One anti-polling timestamp per mag-status target. The all-runs snapshot
  -- has its own key, independent of every explicit run id.
  graph_status_cooldowns = {},

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

  -- Factory names from the kernel registry, snapshotted from `mag.hello` (at
  -- plugin startup) and refreshed by every `mag.loaded` reply. The single
  -- source of truth for valid factory types (see validate_factories).
  kernel_factories = {},

  -- What each kernel factory's nodes mean for approval, by factory identity,
  -- from the same `mag.hello` / `mag.loaded` bodies. mag-apply assesses a
  -- workflow's nodes from it; a factory missing here cannot be assessed.
  factory_approvals = {},

  -- `mag` tool invocations awaiting their `mag.load` reply, keyed by the load
  -- request id. Compile and apply go through this handshake:
  -- `mag.load` is sent, then the immutable envelope is previewed and either
  -- executed inline as a fresh program or applied inline as a delta
  -- to one live run.
  pending_mag_load = {},

  -- Compiled apply invocations awaiting the kernel's correlated `mag.applied`
  -- acknowledgment. Compilation and application are separate failure
  -- boundaries, so a firing moves here only after a valid delta was loaded.
  pending_mag_apply = {},

  -- Invocations awaiting the short terminal-result grace deadline, keyed by
  -- exact run_id. Timeout emits the unchanged async acknowledgment;
  -- terminal settlement claims the same slot for sync delivery.
  completion_grace_waiters = {},

  -- Exact-run synchronous terminate calls, keyed by run_id. Each entry owns
  -- its firing and defensive timeout until canonical terminal settlement.
  termination_waiters = {},

  -- Kernel runs are concurrent: every kernel lifecycle event
  -- (`mag.actor_spawned` / `mag.actor_ready` / `mag.actor_killed` /
  -- `mag.run_complete`) carries its run_id, and the tracker keys straight
  -- into state.active_runs by it — overlapping runs track independently,
  -- no "the in-flight run" singleton.
  mag_authority_lost = false,
}

local SOURCE_NAME = "lead-workflow"

local register_active_run
local submit_loaded_run
local fail_pending_apply
local invalidate_pending_mag_loads
local invalidate_pending_mag_applies
local graph_status
local await_run
local terminate_graph
local now_ms
local emit_verdict_approved
local emit_verdict_rejected
local emit_verdict_discarded
local begin_completion_grace
local cancel_completion_grace
local expire_completion_grace
local take_termination_waiters
local expire_termination_waiter

-- The tool source is built once the handlers below exist; every result,
-- immediate or deferred, settles through it by invocation id.
local tool_source

local function emit_tool_result(body)
  if body.kind == "tool.result" and current_invocation and current_invocation.id == body.id then
    if current_invocation.settled then return end
    current_invocation.settled = true
  end
  local admitted = envelope.checked_emit_as(SOURCE_NAME, nil, body)
  if admitted then return end
  local fallback = envelope.checked_emit_as(SOURCE_NAME, nil, { kind = "tool.result", id = body.id,
    error = "lead-workflow: response could not be published; prior effects are unknown",
    error_code = "output_rejected" })
  if not fallback then
    require("libs.agentic-loop").capability_lost(SOURCE_NAME,
      "Lead-workflow response authority unavailable; accepted effects are unknown")
  end
end

local function emit_tool_result_ok(firing_id, output, completion_delivery)
  tool_source.result(firing_id, { output = output, completion_delivery = completion_delivery })
end

local function emit_await_outcome(firing_id, outcome, completion_delivery)
  local fields = { completion_delivery = completion_delivery }
  if type(outcome) == "table" and outcome.output ~= nil then
    fields.output = outcome.output
  else
    fields.error = type(outcome) == "table" and outcome.error or "mag-await failed"
    if type(outcome) == "table" then
      fields.error_code = outcome.error_code
      fields.run_id = outcome.run_id
      fields.status = outcome.status
      fields.run_name = outcome.run_name
      fields.invocation_label = outcome.invocation_label
      fields.terminal = outcome.terminal
    end
  end
  tool_source.result(firing_id, fields)
end

local function emit_tool_result_err(firing_id, err)
  tool_source.result(firing_id, { error = tostring(err) })
end

local TERMINATION_CONFIRMATION_STATUSES = {
  terminated = true,
  killed = true,
  cancelled = true,
}

local function emit_termination_outcome(firing_id, run, terminal)
  local status = type(terminal) == "table" and terminal.status or nil
  if TERMINATION_CONFIRMATION_STATUSES[status] then
    emit_tool_result_ok(firing_id, {
      canceled = true,
      run_id = run.run_id,
      invocation_label = run.invocation_label,
      status = status,
      workflow_tree = run.terminal_workflow_tree,
      notice = "canonical termination confirmed",
    })
    return
  end
  emit_tool_result_err(firing_id,
    "mag-terminate: incompatible canonical terminal status for " .. run.run_id ..
    ": " .. tostring(status or "unknown"))
end

-- The kernel's factory registry is the source of truth for actors. Library
-- validation already checks the complete contracts;
-- this control-plane pass keeps its pre-execute error localized.
--
-- When no snapshot exists yet (mag plugin not up, or an older plugin that does
-- not advertise factories), validation is skipped and the kernel's own
-- spawn-time "unknown factory" failure is the backstop — a graceful degrade,
-- never a false rejection.
local function sorted_keys(set)
  local keys = {}
  for k in pairs(set) do keys[#keys + 1] = k end
  table.sort(keys)
  return keys
end

local function validate_factories(inventory, firing_id)
  local set = state.kernel_factories
  if type(set) ~= "table" or next(set) == nil then return true end
  for _, entry in ipairs(inventory or {}) do
    local actor = entry.actor
    if set[actor.factory] ~= true then
      emit_tool_result_err(firing_id,
        "mag apply: actor '" .. tostring(entry.address) .. "' uses unknown factory '" ..
        tostring(actor.factory) .. "'. Known factories: " ..
        table.concat(sorted_keys(set), ", ") .. ".")
      return false
    end
  end
  return true
end

-- Write-capable detection over a modification's actors: any actor whose
-- params.tools names a write-capable tool makes the program write-capable and
-- subject to the plan-approval gate. Shell and read-tool actors run freely —
-- the tool gate and its configured validator enforce runtime permissions.
local WRITE_TOOLS = { ["fs/edit"] = true, ["write_file"] = true }

local function actors_have_writers(inventory)
  for _, entry in ipairs(inventory or {}) do
    local actor = entry.actor
    local tools = type(actor.params) == "table" and actor.params.tools or nil
    if type(tools) == "table" then
      for _, t in ipairs(tools) do
        if WRITE_TOOLS[t] then return true end
      end
    end
  end
  return false
end

local function result_boundary(modification)
  local boundary = type(modification.result) == "table" and modification.result.from or nil
  if type(boundary) ~= "table" or type(boundary.leaves) ~= "table"
      or type(boundary.through) ~= "table" or #boundary.leaves + #boundary.through == 0 then
    return nil, "artifact has no structural result boundary"
  end
  return "result", nil
end

-- The top-level key of a caller's workspace path, from its invocation. This
-- is the one place that decides it; today it is the caller's run.
local function workspace_key(invocation)
  return invocation.run_id
end

-- A calling node's workspace belongs to the logical node that directly
-- contains the calling actor's row in the run tree. The rule knows nothing
-- about what kind of node that is; the path is the sidebar projection's.
local function node_workspace(session_id, run, invocation)
  local path = run_projection.enclosing_path(run, invocation.actor_id)
  if not path or #path == 0 then
    return nil, "actor " .. tostring(invocation.actor_id) .. " has no workflow node in run "
      .. tostring(run.run_id)
  end
  return mag.node_workspace(session_id, workspace_key(invocation), path)
end

local function compose_agent_system(base, positional_overlay, workspace)
  local authored = base
  if type(positional_overlay) == "string" and positional_overlay:match("%S") then
    authored = base .. "\n\n---\n\n" .. positional_overlay
  end
  if ambient_context == nil then return authored end
  return ambient_context:compose(authored, workspace)
end

local function is_llm_actor(actor)
  return actor.factory == "nefor.factory.llm"
      or actor.factory == "nefor.factory.structured-output"
end

-- Initial actors as run nodes, keyed by actor id. Operation templates have no
-- actor id until they materialize and appear through actor lifecycle events.
local function initial_nodes(inventory)
  local nodes_order, nodes = {}, {}
  for _, entry in ipairs(inventory or {}) do
    local actor = entry.actor
    local id = entry.kind == "initial" and tostring(actor.id or "") or ""
    if id ~= "" then
      nodes_order[#nodes_order + 1] = id
      nodes[id] = {
        id = id,
        role = actor.factory,
        reasoner = actor.factory,
        spec = actor,
        status = "pending",
      }
    end
  end
  return nodes_order, nodes
end

-- Each model actor's prompt names its node's workspace. An initial actor's node
-- path is known now. A template actor's path exists only once it materializes,
-- and one overlay serves every instance, so it is told where its folder will be.
local function compose_agent_params(inventory, session_id, run_id, logical_nodes)
  local overlay = {}
  if agent_system == nil then return overlay end
  local nodes_order, nodes = initial_nodes(inventory)
  local run = { run_id = run_id, logical_nodes = logical_nodes or {},
    nodes_order = nodes_order, nodes = nodes }
  -- The provenance fields this run's kernel will stamp on that actor's calls.
  local function invocation_of(actor_id)
    return { session_id = session_id, run_id = run_id, actor_id = actor_id }
  end
  for _, entry in ipairs(inventory or {}) do
    local actor = entry.actor
    local params = type(actor.params) == "table" and actor.params or {}
    if is_llm_actor(actor) then
      local workspace = {}
      if entry.kind == "initial" then
        workspace.workspace = node_workspace(session_id, run, invocation_of(actor.id))
      else
        workspace.workspace_parent = mag.run_dir(session_id, workspace_key(invocation_of(nil)))
      end
      overlay[entry.address] = {
        system = compose_agent_system(agent_system, params.system, workspace),
      }
    end
  end
  return overlay
end

local function has_approved_plan()
  return type(state.active_plan) == "table"
         and state.active_plan.status == "approved"
end

-- ## Workflow approval
--
-- A workflow is approved as a unit, when an agent applies it. mag-apply's
-- decision function folds over every node the kernel would spawn for the
-- compiled workflow and asks what each means under the applying agent's
-- approval policy. Once the gate lets the call run, the workflow's own nodes
-- are not asked about again when they fire.

local MAX_DETAIL = 200

local function clip(text)
  text = tostring(text)
  if #text > MAX_DETAIL then return text:sub(1, MAX_DETAIL) .. "…" end
  return text
end

local function node_name(node)
  local actor = node.actor
  if node.kind == "template" then
    return tostring(actor.slot or actor.id) .. " (template of " .. tostring(node.operation.id) .. ")"
  end
  return tostring(actor.id)
end

-- What an error shows of a node's param: a string as it is, an argument
-- vector as the command it stands for.
local function shown(value)
  if type(value) == "table" then
    return "`" .. clip(shell.quote_argv(value) or "(malformed argument vector)") .. "`"
  end
  return "`" .. clip(value) .. "`"
end

-- One node under `policy`: result, why, what kind of node, its detail, and
-- its name. What a kind of node means is not decided here: each kernel
-- factory declares it (`state.factory_approvals`, from the MAG runtime), and
-- a capability node means what the tool of that capability says. Anything
-- that is not defined raises; nothing is assumed safe, unsure or denied.
local function assess_node(policy, node)
  local actor = node.actor
  local params = type(actor.params) == "table" and actor.params or {}
  local declared = state.factory_approvals[actor.factory]
  if type(declared) ~= "table" then
    error("the MAG runtime does not say what a `" .. tostring(actor.factory) .. "` node means for approval", 0)
  end
  local noun = declared.noun

  if declared.kind == "fixed" then
    local decision = declared.decisions[policy]
    if type(decision) ~= "table" then
      error("its factory does not say what it means under the " .. policy .. " policy", 0)
    end
    return decision[1], decision[2], noun
  end

  if declared.kind == "agent" then
    local name = node_name(node):gsub("%.run%-tool", "", 1)
    local node_policy, problem = approval.from_mag(params[declared.policy])
    if node_policy == nil then error("its approval policy is not one: " .. problem, 0) end
    if approval.wider(node_policy, policy) then
      return declared.wider[1], declared.wider[2] .. " (" .. policy .. ")",
        noun, "with approval policy " .. node_policy, name
    end
    return approval.safe, nil, noun, nil, name
  end

  if declared.kind == "capability" then
    local detail = shown(params[declared.shown])
    -- A param bound at run time means what the factory says it means.
    local bound_result, bound_reason
    for _, binding in ipairs(type(actor.parameter_bindings) == "table" and actor.parameter_bindings or {}) do
      local param = type(binding) == "table" and type(binding.path) == "table" and binding.path[1] or nil
      local meaning = declared.runtime_bound[param]
      if meaning == nil then
        error("its factory does not say what binding `" .. tostring(param) .. "` at run time means", 0)
      end
      if meaning ~= "ignored" and (bound_result == nil
          or approval.combine(bound_result, meaning[1]) ~= bound_result) then
        bound_result, bound_reason = meaning[1], meaning[2]
      end
    end
    if bound_result ~= nil then return bound_result, bound_reason, noun, detail end
    if type(decide_capability) ~= "function" then
      error("this configuration did not say what capabilities mean (lead-workflow configure_approval)", 0)
    end
    local args = {}
    for _, name in ipairs(declared.arguments) do args[name] = params[name] end
    local result, reason = decide_capability(declared.tool, policy, args)
    return result, reason, noun, detail
  end

  error("its factory declares the unknown approval kind `" .. tostring(declared.kind) .. "`", 0)
end

-- Fold a decoded workflow into one result for `policy`: denied dominates,
-- then unsure, then safe. Every node that is not safe is listed in the
-- reason, not only the first. A node that cannot be assessed is an error,
-- not a result: then the function returns nil and a message listing every
-- such node.
local function assess_workflow(policy, decoded)
  if not approval.is_policy(policy) then
    return nil, "the call names no approval policy (got `" .. tostring(policy) .. "`)"
  end
  local state_ = mag.fold_nodes(decoded, { result = approval.safe, offenders = {}, failures = {} },
    function(sofar, node)
      local ok, result, why, kind, detail, name = pcall(assess_node, policy, node)
      if not ok then
        sofar.failures[#sofar.failures + 1] = "- node `" .. node_name(node) .. "`: " .. tostring(result)
        return sofar
      end
      local problem = approval.check_decision(result, why)
      if problem then
        sofar.failures[#sofar.failures + 1] = "- node `" .. node_name(node)
          .. "`: its decision is invalid: " .. problem
        return sofar
      end
      sofar.result = approval.combine(sofar.result, result)
      if result ~= approval.safe then
        sofar.offenders[#sofar.offenders + 1] = "- node `" .. (name or node_name(node)) .. "` ("
          .. tostring(kind) .. ")" .. (detail and (" " .. detail) or "") .. ": " .. why
      end
      return sofar
    end)
  if #state_.failures > 0 then
    return nil, "the workflow could not be assessed:\n" .. table.concat(state_.failures, "\n")
  end
  if state_.result == approval.safe then return approval.safe end
  local count = #state_.offenders
  return state_.result, "the workflow declares " .. count .. (count == 1 and " node" or " nodes")
    .. " not shown to be allowed under the " .. policy .. " approval policy:\n"
    .. table.concat(state_.offenders, "\n")
end

-- Tool: write-review.
--
-- Blocking semantics: stores the plan + the firing_id, emits the chat-
-- surface envelope, then returns WITHOUT calling emit_tool_result_ok.
-- The agentic-loop now sits idle waiting for the deferred tool.result.
-- handle_chat_input resolves the ack when the user types /approve,
-- /reject, or any other text.
local function submit_plan(firing_id, args, metadata)
  local plan = args.plan
  if #plan == 0 then
    emit_tool_result_err(firing_id, "write-review: plan must not be empty")
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
  local plan_id = "plan-" .. tostring(firing_id)
  local yolo_approved = (state.gate_mode == "yolo")
  local auto_approved = (state.gate_mode == "auto")
  local immediately_approved = yolo_approved or auto_approved

  local invocation = type(metadata) == "table" and metadata.invocation or nil
  state.active_plan = {
    plan_id             = plan_id,
    content             = plan,
    submitted_at        = submitted_at,
    pending_firing_id   = (not immediately_approved) and firing_id or nil,
    pending_correlation = (not immediately_approved) and
      type(invocation) == "table" and invocation.capability_id or nil,
    pending_run_id      = (not immediately_approved) and
      type(invocation) == "table" and invocation.run_id or nil,
    status              = immediately_approved and "approved" or "pending",
    reason              = nil,
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

  if immediately_approved then
    emit_as(SOURCE_NAME, nil, {
      kind     = "lead-workflow.plan.approved",
      plan_id  = plan_id,
      approved = true,
    })
    local notice
    if yolo_approved then
      notice = "Plan approved automatically (yolo mode)."
    else
      notice = "Plan approved automatically (auto mode)."
    end
    emit_tool_result_ok(firing_id, {
      status = "approved",
      notice = notice,
    })
    return
  end

  -- No tool.result here — the call is blocking. handle_chat_input emits
  -- the deferred ack when the verdict arrives.
end

-- Resolve the deferred write-review ack with an approval payload.
-- Tool.result text is structured for the model: a directive, not just
-- a status code. /approve carries no reason field by spec; if the user
-- typed `/approve <text>`, the trailing text rides along as a `note`.
emit_verdict_approved = function(firing_id, note)
  local out = { status = "approved", notice = "Plan approved by the user. The approval is valid for this turn only." }
  if type(note) == "string" and #note > 0 then out.note = note end
  emit_tool_result_ok(firing_id, out)
end

emit_verdict_rejected = function(firing_id, reason)
  local out = { status = "rejected", notice = "Plan rejected by the user." }
  if type(reason) == "string" and #reason > 0 then out.reason = reason end
  emit_tool_result_ok(firing_id, out)
end

emit_verdict_discarded = function(firing_id, comment)
  local out = { status = "discarded",
    notice = "The user replied without a verdict; the plan is discarded and the reply is new input." }
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
    -- user.message); we just don't bind a verdict to it. If the plan was
    -- already decided (for example YOLO auto-approval), the slash verdict
    -- still ends the approval validity window.
    if type(plan) ~= "table" then return end
    if plan.status ~= "pending" then
      state.active_plan = nil
      return
    end

    local firing_id = plan.pending_firing_id
    plan.pending_firing_id = nil
    plan.pending_correlation = nil
    plan.pending_run_id = nil
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

now_ms = function()
  if nefor.engine and type(nefor.engine.now) == "function" then
    return nefor.engine.now()
  end
  return os.time()
end

-- Track a submitted run for mag-status. Definition inventory is used for
-- validation and overlays, but only concrete initial actors become runtime
-- nodes here; operation templates appear only after materialization emits an
-- actor lifecycle event. Node summaries carry the factory under `reasoner`,
-- matching what the chat surface renders (chat/run_panel.lua).
register_active_run = function(run_id, inventory, terminal, firing_id, run_name, session_id,
    dispatcher_id, owner_resume, request_ids)
  local nodes_order, nodes = initial_nodes(inventory)
  local run = run_registry:register({
    run_id = run_id,
    run_name = run_name,
    session_id = session_id,
    terminal = terminal,
    dispatch_firing_id = firing_id,
    dispatcher_id = dispatcher_id,
    owner_resume = owner_resume,
    request_ids = request_ids,
    nodes_order = nodes_order,
    nodes = nodes,
  })
  state.active_run_id = run_id
  local al_ok, al = pcall(require, "libs.agentic-loop")
  if al_ok and type(al.acquire_request_obligation) == "function" then
    al.acquire_request_obligation(run.request_ids, "run:" .. run_id, session_id)
  end
  return run
end

local function ordered_node_summaries(run)
  local out = {}
  for _, id in ipairs(run.nodes_order or {}) do
    local node = run.nodes and run.nodes[id]
    if node then
      out[#out + 1] = {
        id = node.id,
        role = node.role,
        reasoner = node.reasoner,
        status = node.status,
        started_at = node.started_at,
        completed_at = node.completed_at,
        last_tool = node.last_tool,
        last_tool_args = node.last_tool_args,
        chat_id = node.chat_id,
        output_path = node.output_path,
        output_relpath = node.output_relpath,
        error = node.error,
      }
    end
  end
  return out
end

local function summarize_run(run)
  if type(run) ~= "table" then return nil end
  return {
    run_id = run.run_id,
    invocation_label = run.invocation_label,
    run_name = run.run_name,
    status = run.status,
    dispatched_at = run.dispatched_at,
    updated_at = run.updated_at,
    terminal = run.terminal,
    nodes = ordered_node_summaries(run),
    result = run.result,
    error = run.error,
    cancel_reason = run.cancel_reason,
  }
end

local function refresh_active_run_id()
  local latest_id, latest_at
  for run_id, run in pairs(state.active_runs) do
    local at = tostring(run.dispatched_at or "")
    if latest_at == nil or at > latest_at or (at == latest_at and run_id > latest_id) then
      latest_id, latest_at = run_id, at
    end
  end
  state.active_run_id = latest_id
end

-- The tracked run a kernel lifecycle event belongs to. Every kernel event
-- carries its run_id (runs are concurrent — overlapping runs must track
-- independently); an event for a run we don't track (already closed, or not
-- ours) resolves to nil and is ignored.
local function mag_event_run(body)
  local run_id = body.run_id
  return type(run_id) == "string" and state.active_runs[run_id] or nil
end

-- `mag.run_started`: mark the run running.
local function mark_mag_run_started(body)
  local run = mag_event_run(body)
  if not run then return end
  run_registry:mark_running(run.run_id)
end

-- Track one kernel actor lifecycle transition against its run's node table —
-- the same per-firing truth the chat surface tracks: spawned → pending,
-- ready/busy → running, idle → done, killed → killed. A later firing moves a
-- idle actor back to running. The event's run_id names the run. A
-- mid-run spawn (`mag.apply`) appends a node the dispatch-time modification
-- didn't carry.
local function mark_mag_actor(body, status)
  local run = mag_event_run(body)
  local actor_id, factory = body.id, body.factory
  if not run or type(actor_id) ~= "string" or actor_id == "" then return end
  local ts = monotonic_now_ms() or 0
  run.updated_at = ts
  local node = run.nodes[actor_id]
  if not node then
    node = {
      id = actor_id,
      role = factory,
      reasoner = factory,
      spec = body.spec or {},
      status = "pending",
    }
    run.nodes[actor_id] = node
    run.nodes_order[#run.nodes_order + 1] = actor_id
  end
  if type(factory) == "string" then
    node.reasoner = factory
    node.role = node.role or factory
  end
  if status == "ready" then
    node.status = "pending"
  elseif status == "running" or status == "working" then
    node.status = "running"
    node.started_at = node.started_at or ts
    node.activation_started_at_ms = node.activation_started_at_ms or ts
  elseif status == "idle" then
    node.status = "done"
    node.settled_at_ms = ts
    node.activation_started_at_ms = nil
  elseif status == "killed" then
    node.status = "killed"
    node.completed_at = node.completed_at or ts
  elseif status == "done" then
    -- A completed run's teardown sweep (mag.actor_killed reason
    -- "run_complete"): the node finished, it wasn't terminated. Terminal
    -- states (killed from a mid-run kill) keep their truth.
    if node.status == "pending" or node.status == "running"
        or node.status == "working" or node.status == "idle" then
      node.status = "done"
      node.completed_at = node.completed_at or ts
      node.settled_at_ms = ts
      node.activation_started_at_ms = nil
    end
  end
  -- Ready means resident, not active. The lead's compact status vocabulary
  -- uses pending/running/done while busy/idle boundaries contribute the same
  -- active-time intervals as the sidebar's idle/working states.
  run.group_activity = run_projection.advance_group_activity(run, run.nodes, ts)
end

-- `mag.run_complete` (the sink's terminal signal, ahead of the terminal
-- mag.run_result): the kernel emits no per-actor "done", so every actor still
-- pending/running flips to done here; killed actors keep their terminal
-- state. Mirrors the chat surface's run panel (examples/nefor-agent/chat/run_panel.lua).
local function mark_mag_run_complete(body)
  local run = mag_event_run(body)
  if not run then return end
  local ts = monotonic_now_ms() or 0
  run.updated_at = ts
  for _, id in ipairs(run.nodes_order or {}) do
    local node = run.nodes[id]
    if node and (node.status == "pending" or node.status == "running"
        or node.status == "working" or node.status == "idle") then
      node.status = "done"
      node.completed_at = node.completed_at or ts
      node.settled_at_ms = node.settled_at_ms or ts
      node.activation_started_at_ms = nil
    end
  end
  run.group_activity = run_projection.advance_group_activity(run, run.nodes, ts)
end

-- Snapshot the kernel registry's factory names from a mag.hello / mag.loaded
-- body. This is the source of truth for factory validation
-- (validate_factories).
local function capture_kernel_factories(body)
  if type(body.factory_contracts) ~= "table" then return end
  local set = {}
  for _, contract in ipairs(body.factory_contracts) do
    local identity = type(contract) == "table" and contract.identity or nil
    if type(identity) == "string" then set[identity] = true end
  end
  state.kernel_factories = set
  -- What each factory's nodes mean for approval, as the factories declare.
  if type(body.factory_approvals) == "table" then
    state.factory_approvals = body.factory_approvals
  end
end

-- The lead reads paths; the model needs content. Read the sink's output file so
-- the run-completion turn carries the actual result, not just a path. Best
-- effort — an unreadable path yields an empty relayed result while the visible
-- graph-result block still retains the artifact location.
local function read_output_file(path)
  if type(path) ~= "string" or path == "" then return nil end
  local fh = io.open(path, "r")
  if not fh then return nil end
  local content = fh:read("*a")
  fh:close()
  return content
end

-- The relay text for a run result's inline `result` (the sink's final answer
-- riding mag.run_result): its text when it carries one, the encoded payload
-- otherwise. nil when there is no usable result — the caller falls back to
-- reading the persisted output file.
local function mag_result_text(result)
  if type(result) ~= "table" then return nil end
  local semantic = result.semantic_type
  if type(semantic) == "table"
      and semantic.name == "nefor.contracts.TextAnswer"
      and type(result.value) == "string" then
    return result.value
  end
  if type(result.text) == "string" and #result.text > 0 then
    return result.text
  end
  local ok, encoded = pcall(json.encode, result)
  if ok and type(encoded) == "string" then return encoded end
  return nil
end

-- Relay a kernel run's completion to the model as a fresh orchestrator turn
-- (agentic-loop's deferred-queue + flush → new user-role turn).
-- `format_deferred` frames the sink content. The output path stays on the
-- visible graph-result block instead of being duplicated in the model input.
local function completion_payload(run_id, run_name, ok, content, err)
  if ok then
    local content_available = type(content) == "string" and content:find("%S") ~= nil
    return {
      run_id = run_id,
      run_name = run_name,
      status = "success",
      content_available = content_available,
      output = content_available and content or nil,
    }
  end
  return {
    run_id = run_id,
    run_name = run_name,
    status = "failed",
    error = err,
  }
end

local function relay_kernel_completion(run, ok, content, err)
  local al = require("libs.agentic-loop")
  if type(al.relay_run_completion) ~= "function" then return false end
  local completion = completion_payload(run.run_id, run.run_name, ok, content, err)
  completion.workflow_tree = run.terminal_workflow_tree
  completion.request_ids = run.request_ids
  return al.relay_run_completion(completion)
end

local function settle_request_run(run)
  local al_ok, al = pcall(require, "libs.agentic-loop")
  if al_ok and type(al.settle_request_obligation) == "function" then
    al.settle_request_obligation(run.request_ids, "run:" .. run.run_id)
  end
end

local function record_request_run_failure(run, body, err)
  local al_ok, al = pcall(require, "libs.agentic-loop")
  if not al_ok or type(al.record_request_outcome) ~= "function" then return end
  local status = body.status == "killed" and "interrupted" or "error"
  al.record_request_outcome(run.request_ids, status, {
    code = body.status == "killed" and "interrupted" or "mag_run_failed",
    message = tostring(err),
    run_id = run.run_id,
  })
end

local function resume_run_owner(run, completion)
  local owner = run.owner_resume
  if type(owner) ~= "table" or type(owner.run_id) ~= "string"
      or type(owner.actor_id) ~= "string" then
    return false
  end
  emit_as(SOURCE_NAME, "mag", {
    kind = "mag.resume_actor",
    run_id = owner.run_id,
    actor_id = owner.actor_id,
    conversation_id = owner.conversation_id,
    message = {
      role = "user",
      content = results_lib.format_deferred(completion),
      input_cause = "internal_async_completion",
    },
  })
  return true
end

-- Append the transcript run-result block (`chat.graph_result.append`) for a
-- closed kernel run. Carries status + run id/name + the sink's output
-- PATH; the output CONTENT is deliberately omitted — it arrives separately as
-- the relayed fresh turn (relay_kernel_completion), so duplicating it here
-- would double-render it. `run` is read after the registry's canonical
-- transition and node finalization, exactly the state the block should show.
local function emit_mag_result_block(run, status, output_path, err)
  local block = {
    kind     = "chat.graph_result.append",
    run_id   = run.run_id,
    run_name = run.run_name,
    status   = status,
    nodes    = ordered_node_summaries(run),
    invocation_label = run.invocation_label,
    invocation_kind = run.invocation_kind,
  }
  if status == "success" then
    if type(output_path) == "string" and #output_path > 0 then
      block.output = "output_path: " .. output_path
    end
  elseif type(err) == "string" and #err > 0 then
    block.error = err
  end
  if type(run.duration_ms) == "number" then block.duration_ms = run.duration_ms end
  emit("nefor-tui", block)
end

-- Close a kernel run on its terminal mag.run_result. Updates run/mag-status
-- state with the sink's output PATH, appends the visible run-result block,
-- then relays the completion to the model as a fresh turn (item 2 parity).
-- The relayed content is the sink's final result carried INLINE on the reply
-- (`body.result` — the model needs the answer, not a path); the persisted
-- output file is the fallback when the reply predates the inline result.
local function clear_pending_plan(plan)
  if type(plan) ~= "table" or plan.status ~= "pending" then return false end
  plan.pending_firing_id = nil
  plan.pending_correlation = nil
  plan.pending_run_id = nil
  state.active_plan = nil
  return true
end

local function clear_pending_plan_for_dead_run(run_id)
  local plan = state.active_plan
  if type(plan) ~= "table" or plan.pending_run_id ~= run_id then return false end
  return clear_pending_plan(plan)
end

local function handle_mag_run_result(body)
  local run_id = body.run_id or body.in_reply_to
  if type(run_id) ~= "string" then return end
  if body.status == "killed" or body.status == "failed" then
    clear_pending_plan_for_dead_run(run_id)
  end
  local run = state.active_runs[run_id]
  if not run then return end
  local terminal_ms = monotonic_now_ms() or 0
  run.active_at_terminal = run_projection.active_groups(run, run.nodes)

  -- A parent terminal result proves that every nested completion previously
  -- resumed into that owner was consumed and the owner itself settled.
  for _, child_run in ipairs(run.owner_delivery_from or {}) do
    settle_request_run(child_run)
  end
  run.owner_delivery_from = {}

  local settled, transitioned = run_registry:settle(run_id, body)
  if not transitioned then return end
  run = settled.run
  if state.active_run_id == run_id then refresh_active_run_id() end

  local ts = terminal_ms
  local failed = body.status == "failed" or body.status == "killed"
  local err = body.error
    or (body.status == "killed" and "run killed" or "mag run failed")
  if failed then record_request_run_failure(run, body, err) end
  local results = {}
  if not failed and type(run.terminal) == "string" and body.output_path ~= nil then
    results[run.terminal] = { output = { output_path = body.output_path } }
  end
  run.result = failed and nil or results
  run.error = failed and err or nil
  if type(results) == "table" then
    for node_id, value in pairs(results) do
      local node = run.nodes and run.nodes[node_id]
      if node then
        node.status = "done"
        node.completed_at = node.completed_at or ts
        if type(value.output) == "table" then
          node.output_path = value.output.output_path
          node.output_relpath = value.output.output_relpath
        end
      end
    end
  end
  for _, id in ipairs(run.nodes_order or {}) do
    local node = run.nodes and run.nodes[id]
    if node and (node.status == "pending" or node.status == "running"
        or node.status == "working" or node.status == "idle") then
      node.status = failed and (body.status == "killed" and "killed" or "error") or "done"
      node.completed_at = node.completed_at or ts
      node.settled_at_ms = node.settled_at_ms or ts
      if failed then node.error = err end
    end
  end
  run.group_activity = run_projection.advance_group_activity(run, run.nodes, ts)
  run.terminal_workflow_tree = run_projection.format_tree(run, ts, true)
  if type(settled.outcome) == "table" and type(settled.outcome.output) == "table" then
    settled.outcome.output.workflow_tree = run.terminal_workflow_tree
  elseif type(settled.outcome) == "table" and type(settled.outcome.error) == "string" then
    settled.outcome.error = settled.outcome.error .. "\nWorkflow:\n" .. run.terminal_workflow_tree
  end

  for _, firing_id in ipairs(settled.waiters) do
    emit_await_outcome(firing_id, settled.outcome)
  end

  local grace_waiter = cancel_completion_grace(run_id)
  if grace_waiter then
    emit_await_outcome(grace_waiter.firing_id, settled.outcome, "sync")
  end

  local termination_waiters = take_termination_waiters(run_id)
  for _, waiter in ipairs(termination_waiters or {}) do
    emit_termination_outcome(waiter.firing_id, run, body)
  end

  if not run_registry:claim_delivery(run_id) then return end
  local root_owned = run.dispatcher_id == nil
  if root_owned and not grace_waiter and not termination_waiters then
    emit_mag_result_block(run, failed and "failed" or "success",
      failed and nil or body.output_path, failed and err or nil)
  end
  if grace_waiter or termination_waiters or #settled.waiters > 0 then
    settle_request_run(run)
    return
  end
  local content = not failed and (mag_result_text(body.result)
    or read_output_file(body.output_path)) or nil
  local completion = completion_payload(run_id, run.run_name, not failed, content, err)
  if not root_owned then
    local owner = run.owner_resume and state.active_runs[run.owner_resume.run_id] or nil
    if owner then
      -- Install the pending receipt before routing; a kernel rejection must
      -- settle this obligation explicitly rather than leave it on a dead actor.
      owner.owner_delivery_from[#owner.owner_delivery_from + 1] = run
      run.owner_delivery_acked = false
    end
    if not owner or not resume_run_owner(run, completion) then
      nefor.log.warn("lead-workflow: detached result owner is no longer resumable", {
        run_id = run_id,
        owner_run_id = run.owner_resume and run.owner_resume.run_id,
        owner_actor_id = run.owner_resume and run.owner_resume.actor_id,
      })
      local al_ok, al = pcall(require, "libs.agentic-loop")
      if al_ok and type(al.fail_request) == "function" then
        for _, request_id in ipairs(run.request_ids or {}) do
          al.fail_request(request_id, {
            code = "completion_undeliverable",
            message = "detached result owner is no longer resumable",
            run_id = run_id,
          })
        end
      end
      settle_request_run(run)
    end
    return
  end
  if failed then
    -- A TUI termination is already a user decision. Settlement and visibility
    -- still happen, but feeding the kill back as a task would restart the lead.
    if run.terminate_reason ~= "user-tui-termination" then
      relay_kernel_completion(run, false, nil, err)
    else
      local al_ok, al = pcall(require, "libs.agentic-loop")
      if al_ok and type(al.interrupt_request) == "function" then
        for _, request_id in ipairs(run.request_ids or {}) do al.interrupt_request(request_id) end
      end
    end
    settle_request_run(run)
    return
  end
  relay_kernel_completion(run, true, content, nil)
  settle_request_run(run)
end

local function handle_mag_pre_start_error(body)
  local run_id = body.in_reply_to
  local run = type(run_id) == "string" and state.active_runs[run_id] or nil
  if not run or run.phase ~= "queued" or run.dispatch_firing_id == nil then return false end
  local message = tostring(body.message or "MAG rejected the run before it started")
  local rejected, transitioned = run_registry:reject(run_id, message)
  if not transitioned then return false end
  refresh_active_run_id()
  local al = require("libs.agentic-loop")
  al.record_request_outcome(run.request_ids, "error", {
    code = "execute_rejected", message = message, run_id = run_id,
  })
  for _, firing_id in ipairs(rejected.waiters) do emit_await_outcome(firing_id, rejected.outcome) end
  local grace = cancel_completion_grace(run_id)
  if grace then emit_await_outcome(grace.firing_id, rejected.outcome, "sync") end
  for _, waiter in ipairs(take_termination_waiters(run_id) or {}) do
    emit_tool_result_err(waiter.firing_id, message)
  end
  if not grace and #rejected.waiters == 0 then
    emit_mag_result_block(run, "failed", nil, message)
    relay_kernel_completion(run, false, nil, "Execute rejected before starting: " .. message)
  end
  settle_request_run(run)
  return true
end

-- Double-Esc entry point (`chat.interrupt_all`). A fresh `mag apply` is
-- FIRE-AND-FORGET: it dispatches a detached sub-run into state.active_runs and
-- acks "executing" at once, so the lead's turn completes and goes idle while
-- the sub-run keeps churning. The agentic-loop's own interrupt only sees a run
-- the lead is BLOCKED on (current_run_id), so those detached runs would sail on
-- untouched — the incident. lead-workflow owns state.active_runs, so it
-- interrupts EACH live dispatched run directly.
--
-- TERMINATING interrupt (`terminate = true`): a dispatched run is ephemeral —
-- its only output is the relayed result, so an interrupt must STOP it, not
-- gracefully cancel one tool and let the run's agent llm re-fire. A GRACEFUL
-- interrupt here reproduced the incident: it cancelled the agent's bash, the
-- agent llm absorbed that as a normal tool failure and answered "Completed",
-- and the sub-run settled `completed` — relaying success for an interrupted
-- run. Terminate instead cancels the in-flight work (the bash's process group
-- dies, a nested sub-run is interrupted down the chain) AND ends the run
-- FAILED so no actor re-fires. The terminal failed `mag.run_result` relays
-- into the lead's next turn (handle_mag_run_result → relay_kernel_completion)
-- carrying "interrupted by user" as a failure — never a silent disappearance
-- and never a phantom success. Idempotent: a run with nothing in flight
-- cancels 0 and still ends failed; a run already reaped is a kernel no-op.
local function interrupt_active_runs()
  local ids = {}
  for run_id, _ in pairs(state.active_runs) do ids[#ids + 1] = run_id end
  table.sort(ids)
  for _, run_id in ipairs(ids) do
    run_registry:mark_terminating(run_id, "interrupt-all")
    emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id, terminate = true })
  end
  nefor.log.info("lead-workflow: interrupt_all — terminated active dispatched runs", {
    count = #ids,
  })
  return #ids
end

-- Completeness for the general cancel route: a `tool.cancel` addressed to a
-- submitted dispatch firing propagates into its standard active run. This
-- covers fresh file-based apply and lead-called eval identically.
local function interrupt_run_by_dispatch_firing(firing_id)
  if type(firing_id) ~= "string" then return false end
  local hit = false
  for run_id, run in pairs(state.active_runs) do
    if run.dispatch_firing_id == firing_id then
      run_registry:mark_terminating(run_id, "dispatch-canceled")
      emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id, terminate = true })
      hit = true
    end
  end
  return hit
end

local function resolve_invocation(metadata, direct_default_principal)
  local has_invocation = type(metadata) == "table" and metadata.invocation ~= nil
  if not has_invocation then
    local session_id = sessions.current_id()
    if type(session_id) ~= "string" or session_id == "" then
      return nil, "no active session"
    end
    local caller_id = type(metadata) == "table" and metadata.caller_id or nil
    local principal = direct_default_principal or "subagent"
    local al_ok, al = pcall(require, "libs.agentic-loop")
    if type(caller_id) == "string" and al_ok and type(al.lead_scoped_id) == "function"
        and al.lead_scoped_id(caller_id) == true then
      principal = "lead"
    end
    local request_ids = {}
    if principal == "lead" and al_ok and type(al.current_request_ids) == "function" then
      request_ids = al.current_request_ids()
    end
    return {
      session_id = session_id,
      principal = principal,
      direct = true,
      request_ids = request_ids,
    }
  end

  local invocation, validation_error = chat_emitter.validate_invocation(metadata.invocation)
  if not invocation then
    return nil, "invalid invocation provenance: " .. tostring(validation_error)
  end
  if sessions.current_id() ~= invocation.session_id then
    return nil, "invocation session is no longer active"
  end
  local request_ids = {}
  if invocation.principal == "subagent" then
    local owner_run = run_registry:get(invocation.run_id)
    request_ids = owner_run and owner_run.request_ids or {}
  else
    local al_ok, al = pcall(require, "libs.agentic-loop")
    if al_ok and type(al.current_request_ids) == "function" then
      request_ids = al.current_request_ids()
    end
  end
  return {
    session_id = invocation.session_id,
    principal = invocation.principal,
    invocation = invocation,
    dispatcher_id = invocation.principal == "subagent" and invocation.actor_id or nil,
    conversation_id = invocation.conversation_id,
    owner_run_id = invocation.principal == "subagent" and invocation.run_id or nil,
    request_ids = request_ids,
  }
end

local function run_control_context(metadata)
  local provenance, err = resolve_invocation(metadata)
  if not provenance then return nil, err end
  return {
    session_id = provenance.session_id,
    dispatcher_id = provenance.dispatcher_id,
    owning_run_id = provenance.invocation and provenance.invocation.run_id or nil,
  }, nil
end

local function authorize_control_target(context, run_id)
  if context.dispatcher_id ~= nil and context.owning_run_id == run_id then
    return nil, RunRegistry.authority_error("run_control_self",
      "an agent cannot control its owning run", run_id)
  end
  return run_registry:authorize(run_id, context.session_id, context.dispatcher_id)
end

await_run = function(firing_id, args, metadata)
  local context, provenance_error = run_control_context(metadata)
  if not context then
    emit_await_outcome(firing_id, RunRegistry.typed_error(
      "await_run_session_ended", provenance_error, args and args.run_id))
    return
  end
  local run, err = authorize_control_target(context, args and args.run_id)
  local result
  if not run then
    result = { immediate = err }
  elseif run.phase == "terminal" then
    result = { immediate = run.canonical }
  else
    run.waiters[firing_id] = true
    run_registry.waiter_runs[firing_id] = run.run_id
    result = { waiting = true, run = run }
  end
  if result.immediate then emit_await_outcome(firing_id, result.immediate) end
end

local function schedule_timer(scheduler, delay_ms, callback)
  return (scheduler or default_grace_scheduler)(delay_ms, callback)
end

take_termination_waiters = function(run_id)
  local waiters = state.termination_waiters[run_id]
  if not waiters then return nil end
  state.termination_waiters[run_id] = nil
  for _, waiter in ipairs(waiters) do
    if type(waiter.cancel_timer) == "function" then waiter.cancel_timer() end
  end
  return waiters
end

expire_termination_waiter = function(run_id, firing_id)
  local waiters = state.termination_waiters[run_id]
  if not waiters then return false end
  for index, waiter in ipairs(waiters) do
    if waiter.firing_id == firing_id then
      table.remove(waiters, index)
      if #waiters == 0 then state.termination_waiters[run_id] = nil end
      emit_tool_result_err(firing_id,
        "mag-terminate: timed out awaiting canonical terminal confirmation for " .. run_id)
      return true
    end
  end
  return false
end

terminate_graph = function(firing_id, args, metadata)
  local run_id = args.run_id
  if run_id == "" then
    emit_tool_result_err(firing_id, "mag-terminate: run_id must not be empty")
    return
  end

  local context, context_error = run_control_context(metadata)
  if not context then
    emit_tool_result_ok(firing_id, {
      canceled = false, run_id = run_id, status = "run_control_invalid_provenance",
      notice = context_error,
    })
    return
  end
  local run, lookup_err = authorize_control_target(context, run_id)
  if not run or run.phase == "terminal" then
    emit_tool_result_ok(firing_id, {
      canceled = false,
      run_id = run_id,
      invocation_label = run and run.invocation_label or nil,
      status = lookup_err and lookup_err.error_code or "not_active",
      notice = lookup_err and lookup_err.error or "graph run is already terminal",
    })
    return
  end

  local first_request = run.phase ~= "terminating"
  run_registry:mark_terminating(run_id, "mag-terminate")
  if first_request then
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
  end
  local waiter = { firing_id = firing_id }
  local waiters = state.termination_waiters[run_id] or {}
  state.termination_waiters[run_id] = waiters
  waiters[#waiters + 1] = waiter
  waiter.cancel_timer = schedule_timer(termination_scheduler,
    TERMINATION_CONFIRM_TIMEOUT_MS, function()
      expire_termination_waiter(run_id, firing_id)
    end)
end

local GRAPH_STATUS_COOLDOWN_SECONDS = 60
local GRAPH_STATUS_COOLDOWN_LIMIT = 256
local GRAPH_STATUS_ALL_KEY = "all-runs"
local graph_status_now = os.time

local function graph_status_target(run_id)
  if type(run_id) == "string" and run_id ~= "" then
    return "run:" .. run_id, "run_id " .. run_id
  end
  return GRAPH_STATUS_ALL_KEY, "all runs"
end

local function prune_graph_status_cooldowns(now)
  local cooldowns = state.graph_status_cooldowns
  local count = 0
  local oldest_key
  local oldest_at
  for key, checked_at in pairs(cooldowns) do
    if (now - checked_at) >= GRAPH_STATUS_COOLDOWN_SECONDS then
      cooldowns[key] = nil
    else
      count = count + 1
      if oldest_at == nil or checked_at < oldest_at
          or (checked_at == oldest_at and key < oldest_key) then
        oldest_key = key
        oldest_at = checked_at
      end
    end
  end
  return count, oldest_key
end

graph_status = function(firing_id, args, metadata)
  local context, context_error = run_control_context(metadata)
  if not context then
    emit_tool_result_err(firing_id, "mag-status: " .. tostring(context_error))
    return
  end
  local now = graph_status_now()
  local run_id = args and args.run_id
  local authorized_run
  local lookup_err
  if type(run_id) == "string" and run_id ~= "" then
    authorized_run, lookup_err = authorize_control_target(context, run_id)
    if lookup_err and (lookup_err.error_code == "run_control_unauthorized"
        or lookup_err.error_code == "run_control_self") then
      emit_tool_result_ok(firing_id, {
        active = false,
        run_id = run_id,
        status = lookup_err.error_code,
        notice = lookup_err.error,
        error_code = lookup_err.error_code,
      })
      return
    end
  end
  local target_key, target_label = graph_status_target(run_id)
  local cooldown_count, oldest_key = prune_graph_status_cooldowns(now)
  if state.graph_status_cooldowns[target_key] ~= nil then
    emit_tool_result_err(firing_id,
      "mag-status blocked for " .. target_label .. ": you called it less than " ..
      GRAPH_STATUS_COOLDOWN_SECONDS .. "s ago. " ..
      "Graph results arrive automatically — do not poll. " ..
      "Stop calling mag-status and wait for the result to arrive, or address the user.")
    return
  end
  if cooldown_count >= GRAPH_STATUS_COOLDOWN_LIMIT then
    state.graph_status_cooldowns[oldest_key] = nil
  end
  state.graph_status_cooldowns[target_key] = now

  if type(run_id) == "string" and run_id ~= "" then
    if authorized_run then
      emit_tool_result_ok(firing_id, {
        active = authorized_run.phase ~= "terminal",
        run_id = authorized_run.run_id,
        invocation_label = authorized_run.invocation_label,
        run = summarize_run(authorized_run),
      })
      return
    end
    emit_tool_result_ok(firing_id, {
      active = false,
      run_id = run_id,
      status = lookup_err and lookup_err.error_code or "unknown",
      notice = lookup_err and lookup_err.error or "graph run not found",
      error_code = lookup_err and lookup_err.error_code or nil,
    })
    return
  end

  local active, recent = {}, {}
  local visible = context.dispatcher_id and run_registry:direct_runs(context.dispatcher_id) or nil
  if visible then
    for _, run in ipairs(visible) do
      local summary = summarize_run(run)
      if run.phase == "terminal" then recent[#recent + 1] = summary
      else active[#active + 1] = summary end
    end
  else
    for _, run in pairs(state.active_runs) do active[#active + 1] = summarize_run(run) end
    for _, run in ipairs(state.completed_runs) do recent[#recent + 1] = summarize_run(run) end
  end
  table.sort(active, function(a, b) return tostring(a.dispatched_at) < tostring(b.dispatched_at) end)
  table.sort(recent, function(a, b) return tostring(a.dispatched_at) < tostring(b.dispatched_at) end)
  emit_tool_result_ok(firing_id, { active = active, recent = recent })
end

local function reconcile_mag_authority_loss(reason)
  if state.mag_authority_lost then return 0 end
  state.mag_authority_lost = true
  local message = tostring(reason or "MAG execution authority was lost")
  local al_ok, al = pcall(require, "libs.agentic-loop")
  if al_ok and type(al.mag_authority_lost) == "function" then
    -- Force the request-level unknown outcome before any obligation release can
    -- recheck and publish a previously recorded success.
    al.mag_authority_lost({
      code = "mag_authority_lost",
      message = message,
      authority = "mag",
      outcome = "unknown",
    })
  end

  for load_id, pending in pairs(state.pending_mag_load) do
    state.pending_mag_load[load_id] = nil
    emit_tool_result_err(pending.firing_id, "mag: authority lost: " .. message)
  end
  for request_id, pending in pairs(state.pending_mag_apply) do
    state.pending_mag_apply[request_id] = nil
    require("libs.agentic-loop").settle_request_obligation(
      pending.attached_request_ids, "run:" .. pending.run_id)
    emit_tool_result_err(pending.firing_id, "mag apply: authority lost: " .. message)
  end
  for run_id in pairs(state.completion_grace_waiters) do
    local waiter = cancel_completion_grace(run_id)
    if waiter then emit_tool_result_err(waiter.firing_id, "mag: authority lost: " .. message) end
  end
  for run_id, waiters in pairs(state.termination_waiters) do
    take_termination_waiters(run_id)
    for _, waiter in ipairs(waiters) do
      emit_tool_result_err(waiter.firing_id,
        "mag-terminate: MAG authority was lost before terminal confirmation")
    end
  end

  local lost = run_registry:lose_authority(message)
  state.active_runs = run_registry.active_runs
  state.completed_runs = run_registry.completed_runs
  refresh_active_run_id()
  for _, settlement in ipairs(lost.waiter_settlements) do
    emit_await_outcome(settlement.firing_id, settlement.outcome)
  end
  for _, run in ipairs(lost.runs) do settle_request_run(run) end

  return #lost.runs
end

local function terminate_active_graph(session_id)
  invalidate_pending_mag_loads(nil)
  invalidate_pending_mag_applies(nil)
  for run_id in pairs(state.completion_grace_waiters) do
    cancel_completion_grace(run_id)
  end
  for run_id, waiters in pairs(state.termination_waiters) do
    take_termination_waiters(run_id)
    for _, waiter in ipairs(waiters) do
      emit_tool_result_err(waiter.firing_id,
        "mag-terminate: session ended before canonical terminal confirmation")
    end
  end
  state.active_plan = nil
  state.graph_status_cooldowns = {}

  session_id = session_id or sessions.current_id()
  if type(session_id) ~= "string" or session_id == "" then return end
  local ended = run_registry:end_session(session_id)
  state.active_runs = run_registry.active_runs
  state.completed_runs = run_registry.completed_runs
  refresh_active_run_id()

  for _, settlement in ipairs(ended.waiter_settlements) do
    emit_await_outcome(settlement.firing_id, settlement.outcome)
  end
  for _, run_id in ipairs(ended.active_run_ids) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
    nefor.log.info("lead-workflow: graph terminated on session-end", { run_id = run_id })
  end
end

-- tools.advertise on first <gate>.hello (best-effort; the actor still
-- works without the gate being up — tests drive tool.invoke envelopes
-- synthetically).

local advertised = false

-- Replaced in tool_definitions by the fold over the compiled workflow.
local MAG_APPLY_DECIDE = function() error("mag-apply's decision is not wired", 0) end

local function lead_workflow_tool_specs()
  return {
    {
      name        = "mag-status",
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      display = display_contract("graph status", run_label_field(false), {}, "content", nil, { display_field("state", "result", "$", "structured") }),
      description =
        "Returns the current state of MAG workflow runs: all active runs and recently completed ones, " ..
        "or a single run when run_id is given. It returns immediately; it doesn't wait for runs and " ..
        "isn't intended for polling.",
      parameters  = {
        type = "object",
        additionalProperties = false,
        properties = {
          run_id = {
            type = "string",
            description = "Optional graph run id. Omit to list active runs and recent completed summaries.",
          },
        },
      },
    },
    {
      name        = "mag-await",
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      display = display_contract("await run", run_label_field(true), {}, "content", nil, { display_field("outcome", "result", "$", "structured") }),
      description =
        "Waits until a run you started finishes and returns its result. A run finishes only when all " ..
        "its processes exit, so waiting on a run that never ends, such as a server, never returns. " ..
        "Cancelling this wait doesn't stop the run.",
      parameters  = {
        type = "object",
        additionalProperties = false,
        properties = {
          run_id = {
            type = "string",
            description = "Stable opaque run_id returned by a fresh mag-apply dispatch.",
          },
        },
        required = { "run_id" },
      },
    },
    {
      name        = "mag-terminate",
      -- Stopping a run the caller itself started undoes nothing outside it.
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      display = display_contract("terminate graph", run_label_field(true), {}, "content", nil, { display_field("outcome", "result", "$", "structured") }),
      description =
        "Stops one active run, identified by run_id, and returns once the run confirms it has stopped. " ..
        "You won't also receive that run's completion notification. If the run doesn't confirm within " ..
        string.format("%g", TERMINATION_CONFIRM_TIMEOUT_MS / 1000) .. " seconds, an error is returned.",
      parameters  = {
        type = "object",
        additionalProperties = false,
        properties = {
          run_id = {
            type = "string",
            description = "Required active graph run id to cancel. No implicit active-run fallback is supported.",
          },
        },
        required = { "run_id" },
      },
    },
    {
      name        = "write-review",
      approval = {
        ReadOnly = {
          decide = { "denied", "it asks the user to approve a plan to write" },
          describe = approval.read_only_rejection("it asks the user to approve a plan to write"),
        },
        -- Submitting a plan changes nothing; the user's verdict is the approval.
        Default = { decide = "safe", describe = tool.nothing },
      },
      display = display_contract("write review", nil, {}, "receipt", "AWAITING USER REVIEW", { display_field("status", "result", "$", "status", { sensitive = "omit" }) }),
      description =
        "Submits a plan for the user's review and returns their decision: approved, rejected with " ..
        "a reason, or discarded when the user replies with something else, in which case that reply " ..
        "is new input. Waits until the user responds; depending on the session's mode, the plan may " ..
        "be approved immediately. An approval is valid for the current turn only.",
      parameters  = {
        type = "object",
        additionalProperties = false,
        properties = {
          plan = { type = "string", description = "The plan to review." },
        },
        required = { "plan" },
      },
    },
    {
      name = "mag-write-file",
      -- Writes only the caller's own MAG workspace: nothing runs.
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      display = display_contract("mag write file", display_field("file", "args", "file", "path"),
        {}, "receipt", "MAG source written", {}),
      description =
        "Writes or edits a MAG source file in your own MAG workspace without running it.\n\n" ..
        "- new_string only: becomes the file's complete contents, creating or replacing the file.\n" ..
        "- old_string and new_string: replaces the single exact occurrence of old_string. If it occurs " ..
        "zero or several times, the file is unchanged and an error is returned.\n\n" ..
        "new_string may be empty. Paths are relative to your workspace, which no other caller can " ..
        "reach through these tools; absolute paths, .. segments and symlinks are rejected.",
      parameters = { type = "object", additionalProperties = false, properties = {
        file = { type = "string", pattern = "\\.(mag|magl)$", description = "MAG source path ending in .mag or .magl, normalized and relative to your MAG workspace." },
        new_string = { type = "string", description = "Complete contents or replacement text; may be empty." },
        old_string = { type = "string", description = "Optional non-empty exact text that must occur once." },
      }, required = { "file", "new_string" } },
    },
    {
      name = "mag-preview",
      approval = {
        ReadOnly = { decide = "safe", describe = tool.nothing },
        Default = { decide = "safe", describe = tool.nothing },
      },
      display = display_contract("mag preview", display_field("file", "args", "file", "path"),
        {}, "content", nil, { display_field("workflow", "result", "workflow_tree", "text",
          { max_lines = 120, max_bytes = 12000 }) }, "delayed"),
      description =
        "Validates and compiles a MAG source file in your own MAG workspace without running it, " ..
        "and returns the complete node tree the workflow would run. Compilation errors are returned " ..
        "as errors.",
      parameters = { type = "object", additionalProperties = false, properties = {
        file = { type = "string", pattern = "\\.(mag|magl)$", description = "Existing MAG source path ending in .mag or .magl, normalized and relative to your MAG workspace." },
      }, required = { "file" } },
    },
    {
      name = "mag-apply",
      -- Decided by folding over the compiled workflow (assess_workflow); the
      -- decision function is attached in tool_definitions.
      approval = {
        ReadOnly = {
          decide = MAG_APPLY_DECIDE,
          describe = "Your approval policy is ReadOnly: a workflow that declares nodes that are not "
            .. "read-only, or agents with a wider approval policy than yours, is rejected.",
        },
        Default = { decide = MAG_APPLY_DECIDE, describe = tool.nothing },
      },
      display = display_contract("mag apply", display_field("file", "args", "file", "path"),
        {}, "receipt", "MAG graph applied", {
          display_field("status", "result", "status", "status", { omit = "missing" }),
          display_field("run", "result", "run_id", "scalar", { omit = "missing" }),
          display_field("workflow", "result", "workflow_tree", "text",
            { omit = "missing", max_lines = 120, max_bytes = 12000 }),
        }, "delayed"),
      description =
        "Runs MAG code. This is the easiest way to execute it: pass the code as content together " ..
        "with a file name, and this tool writes the file, validates and compiles it, and runs the " ..
        "workflow in one call. If validation or compilation fails, nothing runs and the error is " ..
        "returned.\n\n" ..
        "- file and content: writes content to file, replacing any existing contents, then runs it.\n" ..
        "- file only: runs the workflow already in that file.\n\n" ..
        "After a failed validation or compilation the file keeps what was written, so it can be fixed " ..
        "with a selective edit and run again. A run that finishes within " ..
        string.format("%g", SYNC_COMPLETION_GRACE_MS / 1000) .. " seconds returns its output, or its " ..
        "failure together with the workflow tree. A longer run returns a run_id, the workflow tree, " ..
        "and how its result will reach you.",
      parameters = { type = "object", additionalProperties = false, properties = {
        file = { type = "string", pattern = "\\.(mag|magl)$", description = "MAG source path ending in .mag or .magl, normalized and relative to your MAG workspace." },
        content = { type = "string", description = "Optional complete source written to file before running, replacing any existing contents." },
      }, required = { "file" } },
    },
  }
end

local function advertise_tools(gate_name)
  if advertised then return end
  advertised = true
  tool_source.advertise(gate_name or "tool-gate")
end

-- MAG tool handlers.
--
-- The file tools write, compile, and apply .mag programs through the plugin,
-- each inside the calling node's own workspace. That workspace's path and the
-- library shapes are ambient in every model actor's system prompt, so there is no
-- discovery tool.

-- Compile and apply both run a synchronous load handshake against the mag
-- plugin: `mag.load` is sent, the `mag.loaded` reply carries the lowered
-- versioned program or delta envelope, its hash, and the kernel
-- registry's factory names. resume_pending_load then renders the compile
-- preview, submits a fresh internal `mag.execute` with the inline program, or
-- submits a live-run internal `mag.apply`.
-- A `mag.error` reply (compile failure) fails the firing with the compiler message
-- (fail_pending_load). Lifecycle events (mag.run_started, actor spawn/ready,
-- mag.run_complete) stream on the bus; the terminal mag.run_result (carrying
-- the sink's output PATH) closes the run and relays a fresh model turn in
-- receive_msg.
local function begin_mag_load(firing_id, action, args, ws, provenance)
  local graph_name = args.file:gsub("%.mag$", ""):gsub("/", "-"):sub(1, 20)
  local run_id
  if action == "apply" then run_id = args.run_id or run_registry:mint_run_id()
  else run_id = "mag-load-" .. envelope.uuid_lite() end
  -- An assessed apply runs the artifact it was assessed on, as a fresh run.
  local invocation_kind = action
  if action == "assess" then
    run_id = run_registry:mint_run_id()
    invocation_kind = "apply"
  end
  local load_id = "mag-load-" .. envelope.uuid_lite()

  state.pending_mag_load[load_id] = {
    action     = action,
    firing_id  = firing_id,
    file       = args.file,
    run_id     = run_id,
    target_run_id = args.run_id,
    run_name   = graph_name,
    invocation_kind = invocation_kind,
    invocation_label = args.file,
    session_id = provenance.session_id,
    dispatcher_id = provenance.dispatcher_id,
    conversation_id = provenance.conversation_id,
    owner_run_id = provenance.owner_run_id,
    request_ids = provenance.request_ids,
  }

  local admitted = envelope.checked_emit_as(SOURCE_NAME, "mag", mag.compile_request(load_id, ws, args.file,
    module_roots_for(ws), project_build))
  if not admitted then
    state.pending_mag_load[load_id] = nil
    emit_tool_result_err(firing_id, "MAG compile request could not be published")
    return nil
  end
  return state.pending_mag_load[load_id]
end


-- Invalidate file-based compile handshakes as one state transition. A firing
-- id removes only that invocation's load; nil clears every pending file load
-- at the session boundary. Once removed, both late mag.loaded and mag.error
-- are uncorrelated no-ops and cannot submit or settle the source again.
invalidate_pending_mag_loads = function(firing_id)
  local hit = false
  for load_id, pending in pairs(state.pending_mag_load) do
    if firing_id == nil or pending.firing_id == firing_id then
      state.pending_mag_load[load_id] = nil
      hit = true
    end
  end
  return hit
end

invalidate_pending_mag_applies = function(firing_id)
  local hit = false
  for request_id, pending in pairs(state.pending_mag_apply) do
    if firing_id == nil or pending.firing_id == firing_id then
      state.pending_mag_apply[request_id] = nil
      hit = true
    end
  end
  return hit
end

local function async_run_ack(pending, body)
  local message
  if pending.dispatcher_id == nil then
    message = "Program submitted to the MAG actor kernel. This acknowledgment is not " ..
      "completion; stop this turn and wait for the normal owner-scoped completion " ..
      "notification. Do not call mag-status merely to wait. Synchronously terminal " ..
      "sibling findings remain usable. Compose dependent work into one MAG graph or bounded " ..
      "operation when it must continue within one workflow."
  else
    message = "Program submitted to the MAG actor kernel. This acknowledgment is not " ..
      "completion. Use mag-await with this run_id when dependent work requires the terminal " ..
      "result; do not poll mag-status. Synchronously terminal sibling findings remain usable."
  end
  return {
    status = "executing",
    run_id = pending.run_id,
    run_name = pending.run_name,
    hash = body.hash,
    engine = "mag-kernel",
    workflow_tree = pending.workflow_tree,
    message = message,
  }
end

local function expire_completion_grace(run_id)
  local waiter = state.completion_grace_waiters[run_id]
  if not waiter then return false end
  state.completion_grace_waiters[run_id] = nil
  emit_tool_result_ok(waiter.firing_id, waiter.ack, "async")
  return true
end

begin_completion_grace = function(run, ack)
  local waiter = { firing_id = run.dispatch_firing_id, ack = ack }
  state.completion_grace_waiters[run.run_id] = waiter
  local schedule = grace_scheduler or default_grace_scheduler
  waiter.cancel_timer = schedule(SYNC_COMPLETION_GRACE_MS, function()
    expire_completion_grace(run.run_id)
  end)
end

cancel_completion_grace = function(run_id)
  local waiter = state.completion_grace_waiters[run_id]
  if not waiter then return nil end
  state.completion_grace_waiters[run_id] = nil
  if type(waiter.cancel_timer) == "function" then waiter.cancel_timer() end
  return waiter
end

local function cancel_completion_grace_by_firing(firing_id)
  for run_id, waiter in pairs(state.completion_grace_waiters) do
    if waiter.firing_id == firing_id then
      cancel_completion_grace(run_id)
      return true
    end
  end
  return false
end

-- Validate and submit one loaded file artifact through the standard active-run
-- channel, so lifecycle/control/rendering/archival/cleanup have one owner.
submit_loaded_run = function(pending, body, error_prefix)
  local decoded, decode_error = mag.decode_artifact(body.artifact)
  local function reject(message)
    emit_tool_result_err(pending.firing_id, message)
    return false
  end
  if not decoded or decoded.kind ~= "program" then
    return reject(error_prefix .. ": " .. tostring(decode_error or "requires a program envelope"))
  end
  local modification = decoded.modification
  local inventory = mag.actor_inventory(decoded)
  if not validate_factories(inventory, pending.firing_id) then return false end
  if actors_have_writers(inventory) and state.gate_mode == "safe" and not has_approved_plan() then
    return reject("This program contains write-capable agents and needs an approved plan first.")
  end
  local terminal_id, result_err = result_boundary(modification)
  if not terminal_id then return reject(error_prefix .. ": " .. result_err) end
  if type(resolve_model_snapshot) ~= "function" then
    return reject(error_prefix .. ": no model snapshot resolver is configured")
  end
  local resolved, snapshot = pcall(resolve_model_snapshot)
  if not resolved then return reject(error_prefix .. ": model snapshot resolver failed: " .. tostring(snapshot)) end
  local snapshot_error
  snapshot, snapshot_error = model_snapshot.copy(snapshot)
  if snapshot == nil then return reject(error_prefix .. ": invalid model snapshot: " .. tostring(snapshot_error)) end
  local overlay = compose_agent_params(inventory, pending.session_id, pending.run_id,
    modification.nodes)
  local exec = {
    kind = "mag.execute", id = pending.run_id, run_id = pending.run_id,
    run_name = pending.run_name, session_id = pending.session_id,
    principal = "subagent", invocation_kind = pending.invocation_kind,
    invocation_label = pending.invocation_label, conversation_id = pending.conversation_id,
    artifact = body.artifact, model_snapshot = snapshot,
  }
  if next(overlay) ~= nil then exec.params_overlay = overlay end
  local owner_resume
  if pending.dispatcher_id ~= nil then
    local owner_actor_id, replacements = pending.dispatcher_id:gsub("%.run%-tool$", ".llm")
    owner_resume = { run_id = pending.owner_run_id,
      actor_id = replacements == 1 and owner_actor_id or nil,
      conversation_id = pending.conversation_id }
  end
  local run = register_active_run(pending.run_id, inventory, terminal_id,
    pending.firing_id, pending.run_name, pending.session_id, pending.dispatcher_id,
    owner_resume, pending.request_ids)
  run.logical_nodes = modification.nodes or {}
  run.routes = modification.routes or {}
  run.workflow_tree = pending.workflow_tree or mag.workflow_tree(run.logical_nodes)
  run.invocation_label = pending.invocation_label or pending.run_name
  run.invocation_kind = pending.invocation_kind
  begin_completion_grace(run, async_run_ack(pending, body))
  local admitted, err = envelope.checked_emit_as(SOURCE_NAME, "mag", exec)
  if not admitted then
    handle_mag_pre_start_error({ in_reply_to = exec.id, message = "Execute not published: " .. tostring(err) })
    return false
  end
  return true
end

local function submit_loaded_apply(pending, body)
  local decoded, decode_error = mag.decode_artifact(body.artifact)
  if not decoded or decoded.kind ~= "delta" then
    emit_tool_result_err(pending.firing_id,
      "mag apply: " .. tostring(decode_error or "requires a delta envelope"))
    return false
  end
  local inventory = mag.actor_inventory(decoded)
  if not validate_factories(inventory, pending.firing_id) then return false end
  if actors_have_writers(inventory) and state.gate_mode == "safe" and not has_approved_plan() then
    emit_tool_result_err(pending.firing_id,
      "This modification contains write-capable agents and needs an approved plan first.")
    return false
  end
  local target = state.active_runs[pending.run_id]
  local overlay = compose_agent_params(inventory, pending.session_id, pending.run_id,
    run_projection.merge_declared(target and target.logical_nodes, decoded.modification.nodes))
  local request_id = "mag-apply-" .. envelope.uuid_lite()
  state.pending_mag_apply[request_id] = {
    firing_id = pending.firing_id,
    run_id = pending.run_id,
    hash = body.hash,
    workflow_tree = pending.workflow_tree,
    request_ids = pending.request_ids,
  }
  state.pending_mag_apply[request_id].attached_request_ids = {}
  if target then
    local seen = {}
    for _, id in ipairs(target.request_ids or {}) do seen[id] = true end
    for _, id in ipairs(pending.request_ids or {}) do
      if not seen[id] then
        target.request_ids[#target.request_ids + 1] = id
        state.pending_mag_apply[request_id].attached_request_ids[#state.pending_mag_apply[request_id].attached_request_ids + 1] = id
        seen[id] = true
      end
    end
    require("libs.agentic-loop").acquire_request_obligation(
      pending.request_ids, "run:" .. pending.run_id, pending.session_id)
  end
  local apply = { kind = "mag.apply", id = request_id, run_id = pending.run_id,
    source = "lead-workflow.mag.apply", artifact = body.artifact }
  if next(overlay) ~= nil then apply.params_overlay = overlay end
  local admitted, err = envelope.checked_emit_as(SOURCE_NAME, "mag", apply)
  if not admitted then
    fail_pending_apply({ in_reply_to = request_id, message = "Apply not published: " .. tostring(err) })
    return false
  end
  return true
end

local function detach_rejected_apply(pending)
  local target = state.active_runs[pending.run_id]
  local remove = {}
  for _, id in ipairs(pending.attached_request_ids or {}) do remove[id] = true end
  if target then
    local retained = {}
    for _, id in ipairs(target.request_ids or {}) do
      if not remove[id] then retained[#retained + 1] = id end
    end
    target.request_ids = retained
  end
  require("libs.agentic-loop").settle_request_obligation(
    pending.attached_request_ids, "run:" .. pending.run_id)
end

local function resolve_pending_apply(body)
  local request_id = body.in_reply_to
  local pending = type(request_id) == "string"
    and state.pending_mag_apply[request_id] or nil
  if not pending then return false end
  state.pending_mag_apply[request_id] = nil
  if body.ok == true then
    emit_tool_result_ok(pending.firing_id, {
      status = "applied",
      run_id = pending.run_id,
      hash = pending.hash,
      engine = "mag-kernel",
      workflow_tree = pending.workflow_tree,
      message = "Modification applied atomically to the live MAG run.",
    })
  else
    detach_rejected_apply(pending)
    emit_tool_result_err(pending.firing_id,
      "mag apply rejected: " .. tostring(body.error or "unknown rejection"))
  end
  return true
end

fail_pending_apply = function(body)
  local request_id = body.in_reply_to
  local pending = type(request_id) == "string"
    and state.pending_mag_apply[request_id] or nil
  if not pending then return false end
  state.pending_mag_apply[request_id] = nil
  detach_rejected_apply(pending)
  emit_tool_result_err(pending.firing_id,
    "mag apply failed: " .. tostring(body.message or "unknown error"))
  return true
end

-- Resume a pending compile/apply once its `mag.load` reply arrives. The
-- reply's registry has already refreshed state.kernel_factories
-- (capture_kernel_factories runs first). Compile renders the preview from the
-- modification. A targetless apply validates a fresh-run artifact; an apply
-- with target_run_id validates a delta artifact. Only then is it submitted.
-- Validation failure acks the firing with an error and drops the pending entry.
--
-- The composed system prompt is threaded to agents through `params_overlay`
-- on the kernel's fresh-run or live-run path. Model selection is already concrete in the
-- compiled artifact; the runtime does not reinterpret config-owned values.
local function resume_pending_load(body)
  local load_id = body.in_reply_to
  local pending = type(load_id) == "string" and state.pending_mag_load[load_id] or nil
  if not pending then return end
  state.pending_mag_load[load_id] = nil

  local artifact = body.artifact
  local decoded, decode_error = mag.decode_artifact(artifact)
  if not decoded then
    emit_tool_result_err(pending.firing_id,
      "mag " .. pending.action .. ": " .. tostring(decode_error))
    return
  end

  local workflow_tree = mag.workflow_tree(decoded.modification.nodes)
  pending.workflow_tree = workflow_tree

  if pending.action == "preview" then
    emit_tool_result_ok(pending.firing_id, {
      workflow_tree = workflow_tree,
    })
    return
  end

  -- The decision for a mag-apply call: fold over the compiled workflow under
  -- the caller's policy, keep the compiled artifact on the call, and answer.
  -- If the gate lets the call run, execute submits exactly this artifact.
  if pending.action == "assess" then
    local call = pending.call
    pending.call = nil
    local result, reason = assess_workflow(pending.policy, decoded)
    if result == nil then
      -- Failing to assess is an error for the call, never a decision.
      emit_tool_result_err(pending.firing_id, "mag-apply: " .. reason)
      return
    end
    call.assessed = { pending = pending, body = body }
    call.decided(result, reason)
    return
  end

  if pending.action == "apply" and pending.target_run_id ~= nil then
    submit_loaded_apply(pending, body)
    return
  end

  submit_loaded_run(pending, body, "mag apply")
end

-- A `mag.error` reply to an in-flight load: the compiler rejected the
-- program. Fail the firing with the compiler's message so the lead can fix
-- the source and retry.
local function fail_pending_load(body)
  local load_id = body.in_reply_to
  local pending = type(load_id) == "string" and state.pending_mag_load[load_id] or nil
  if not pending then return end
  state.pending_mag_load[load_id] = nil
  nefor.log.error("lead-workflow: MAG compilation failed", { message = tostring(body.message) })
  emit_tool_result_err(pending.firing_id,
    "compilation failed:\n" .. tostring(body.message))
end

-- The workspace of the calling node, from the provenance the kernel stamped on
-- the call: its key's folder, then the logical node that contains the calling
-- actor.
local function caller_workspace(provenance)
  -- Interim: the lead's run id changes every turn and its turn run is not in
  -- this registry, so it keeps one session folder until its key is decided.
  if provenance.principal == "lead" then
    local ws = mag.lead_workspace(provenance.session_id)
    if not ws then return nil, "no data root available" end
    return ws
  end
  local invocation = provenance.invocation
  if type(invocation) ~= "table" then
    return nil, "a delegated caller's workspace requires invocation provenance"
  end
  local run = run_registry:get(invocation.run_id)
  if not run or run.session_id ~= provenance.session_id then
    return nil, "the calling node's run is not known: " .. tostring(invocation.run_id)
  end
  return node_workspace(provenance.session_id, run, invocation)
end

local function mag_workspace(firing_id, args, metadata, tool_name)
  local provenance, provenance_error = resolve_invocation(metadata, "lead")
  if not provenance then
    emit_tool_result_err(firing_id, tool_name .. ": " .. provenance_error)
    return nil
  end
  local ws, ws_err = caller_workspace(provenance)
  if not ws then
    emit_tool_result_err(firing_id, tool_name .. ": workspace unavailable: " .. tostring(ws_err))
    return nil
  end
  local _, path_error = mag.resolve_file(ws, args.file)
  if path_error then
    emit_tool_result_err(firing_id, tool_name .. ": " .. path_error)
    return nil
  end
  return ws, provenance
end

local function mag_write_file(firing_id, args, metadata)
  local ws = mag_workspace(firing_id, args, metadata, "mag-write-file")
  if not ws then return end
  local receipt, error = mag.write_file(ws, args.file, args.new_string, args.old_string)
  if not receipt then
    emit_tool_result_err(firing_id, "mag-write-file: " .. tostring(error))
  else
    if receipt.operation == "overwritten" then
      receipt.message = "Overwrote the entire file with the provided content."
    end
    emit_tool_result_ok(firing_id, receipt)
  end
end

-- An existing source file compiles as its workspace's project. A workspace is
-- created by its first write, so a missing one is only a missing file.
local function existing_source(ws, file)
  local _, missing = mag.require_file(ws, file)
  if missing then return nil, missing end
  return mag.ensure_project(ws)
end

local function mag_preview(firing_id, args, metadata)
  local ws, provenance = mag_workspace(firing_id, args, metadata, "mag-preview")
  if not ws then return end
  local _, error = existing_source(ws, args.file)
  if error then emit_tool_result_err(firing_id, "mag-preview: " .. error); return end
  begin_mag_load(firing_id, "preview", args, ws, provenance)
end

-- TODO: Reconsider live workflow modification in favor of resuming workflows
-- with completed-node outputs. Live modification is not sufficiently supported
-- and may be removed; this tool deliberately exposes only fresh dispatches.
--
-- mag-apply runs in three steps, in the order every tool call takes:
--   prepare  writes `content` to the file (or checks that the file exists);
--   decide   compiles the file and folds over the compiled workflow's nodes
--            under the caller's approval policy (assess_workflow);
--   execute  runs the artifact the decision was made on.
-- A file written by prepare stays when the gate then refuses the call, so
-- the author can fix it and apply again.
local function mag_apply_prepare(firing_id, args, metadata)
  local ws = mag_workspace(firing_id, args, metadata, "mag-apply")
  if not ws then return end
  local _, source_error
  if args.content ~= nil then
    _, source_error = mag.write_file(ws, args.file, args.content)
  else
    _, source_error = existing_source(ws, args.file)
  end
  if source_error then emit_tool_result_err(firing_id, "mag-apply: " .. source_error) end
end

local function mag_apply_assess(firing_id, args, metadata, call)
  local ws, provenance = mag_workspace(firing_id, args, metadata, "mag-apply")
  if not ws then return end
  local pending = begin_mag_load(firing_id, "assess", args, ws, provenance)
  if not pending then return end
  pending.call = call
  pending.policy = call.policy
end

-- Runs only what the gate approved: the artifact assess_workflow saw. A call
-- that was never assessed has nothing to run.
local function mag_apply(firing_id, _args, _metadata, call)
  local assessed = call and call.assessed or nil
  if assessed == nil then
    emit_tool_result_err(firing_id, "mag-apply: this call was not approved by the tool gate")
    return
  end
  call.assessed = nil
  if sessions.current_id() ~= assessed.pending.session_id then
    emit_tool_result_err(firing_id, "mag-apply: the session that approved this workflow has ended")
    return
  end
  submit_loaded_run(assessed.pending, assessed.body, "mag apply")
end

local TOOL_HANDLERS = {
  ["mag-status"]     = graph_status,
  ["mag-await"]      = await_run,
  ["mag-terminate"]  = terminate_graph,
  ["write-review"]   = submit_plan,
  ["mag-write-file"] = mag_write_file,
  ["mag-preview"]    = mag_preview,
  ["mag-apply"]      = mag_apply,
}

local function invoke_owned_handler(handler, firing_id, args, body, call)
  local previous_invocation = current_invocation
  local invocation = { id = firing_id }
  current_invocation = invocation
  local ok, err = pcall(handler, firing_id, args, {
    caller_id = body.caller_id,
    from = body.from,
    invocation = body.invocation,
  }, call)
  current_invocation = previous_invocation
  if ok then return end
  -- Only the local encoding boundary and typed engine admission errors are
  -- expected here. Trusted invariant failures still escape to the actor boundary.
  local error_kind
  if type(err) == "table" and err.tag == publication_failure then
    error_kind = "publication_failed"
  else
    error_kind = nefor.engine.error_kind and nefor.engine.error_kind(err)
  end
  if error_kind == nil then error(err, 0) end
  invalidate_pending_mag_loads(firing_id)
  local apply_settled = false
  for request_id, pending in pairs(state.pending_mag_apply) do
    if pending.firing_id == firing_id then
      apply_settled = fail_pending_apply({ in_reply_to = request_id,
        message = "Invocation publication failed; prior effects are unknown" }) or apply_settled
    end
  end
  run_registry:detach_waiter(firing_id)
  cancel_completion_grace_by_firing(firing_id)
  for run_id, waiters in pairs(state.termination_waiters) do
    for index = #waiters, 1, -1 do
      local waiter = waiters[index]
      if waiter.firing_id == firing_id then
        if type(waiter.cancel_timer) == "function" then waiter.cancel_timer() end
        table.remove(waiters, index)
      end
    end
    if #waiters == 0 then state.termination_waiters[run_id] = nil end
  end
  local plan = state.active_plan
  if plan and plan.pending_firing_id == firing_id then clear_pending_plan(plan) end
  if not apply_settled and not invocation.settled then
    emit_tool_result_err(firing_id,
      "lead-workflow: invocation publication failed; prior effects are unknown (" .. error_kind .. ")")
  end
end

-- Handlers keep firing-id addressing so deferred settlement (plan verdicts,
-- run completion, termination confirmation) can resolve later by id.
local function tool_definitions()
  local function owned(handler)
    return function(args, call)
      invoke_owned_handler(handler, call.id, args, {
        caller_id = call.request.caller_id,
        from = call.request.from,
        invocation = call.request.invocation,
      }, call)
    end
  end
  local definitions = {}
  for _, spec in ipairs(lead_workflow_tool_specs()) do
    spec.execute = owned(assert(TOOL_HANDLERS[spec.name], spec.name))
    spec.prepare = tool.nothing
    if spec.name == "mag-apply" then
      spec.prepare = owned(mag_apply_prepare)
      local assess = owned(mag_apply_assess)
      -- Answered when the compile reply arrives (resume_pending_load); a
      -- failed compile ends the call with the compiler's message instead.
      local function decide(args, call)
        assess(args, call)
        return tool.deferred
      end
      spec.approval.ReadOnly.decide = decide
      spec.approval.Default.decide = decide
    end
    definitions[#definitions + 1] = tool.define(spec)
  end
  return definitions
end

tool_source = tool.source { name = SOURCE_NAME, tools = tool_definitions(), emit = emit_tool_result, propagate_errors = true }

local function handle_tool_invoke(body)
  tool_source.invoke(body)
end

local function mark_user_terminated_runs(body)
  if body.scope == "one" then
    run_registry:mark_terminating(body.run_id, "user-tui-termination")
    return
  end
  if body.scope ~= "all" then return end
  local ids = {}
  for run_id in pairs(state.active_runs) do ids[#ids + 1] = run_id end
  table.sort(ids)
  for _, run_id in ipairs(ids) do
    run_registry:mark_terminating(run_id, "user-tui-termination")
  end
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

  -- The gate asks what a call means before it decides whether to run it.
  if kind == "lead-workflow.tool.prepare" then
    if replay_window.active() then return end
    tool_source.prepare(body)
    return
  end

  if kind == "chat.workflows.terminate_requested" then
    if replay_window.active() or decoded.from ~= "nefor-tui" then return end
    mark_user_terminated_runs(body)
    return
  end

  -- Double-Esc: interrupt every live dispatched run. The lead is typically
  -- IDLE here (its fresh `mag apply` dispatches are fire-and-forget), so the
  -- agentic-loop's own interrupt sees nothing — this is the entry point that
  -- reaches the detached runs the user is actually trying to stop. Live path
  -- only (nefor-tui emits it; replay never does).
  if kind == "chat.interrupt_all" then
    if replay_window.active() then return end
    clear_pending_plan(state.active_plan)
    interrupt_active_runs()
    return
  end

  -- Gate-forwarded cancel addresses the source's inner firing id.
  if kind == "lead-workflow.tool.cancel" then
    if replay_window.active() then return end
    tool_source.cancel(body)
    local plan = state.active_plan
    if type(plan) == "table" and (plan.pending_firing_id == body.id
        or plan.pending_correlation == body.id) then
      clear_pending_plan(plan)
    end
    if run_registry:detach_waiter(body.id) then return end
    for run_id, waiters in pairs(state.termination_waiters) do
      for index, waiter in ipairs(waiters) do
        if waiter.firing_id == body.id then
          if type(waiter.cancel_timer) == "function" then waiter.cancel_timer() end
          table.remove(waiters, index)
          if #waiters == 0 then state.termination_waiters[run_id] = nil end
          return
        end
      end
    end
    cancel_completion_grace_by_firing(body.id)
    invalidate_pending_mag_loads(body.id)
    invalidate_pending_mag_applies(body.id)
    interrupt_run_by_dispatch_firing(body.id)
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

  -- Refresh the registry before file-load validation consumes a loaded artifact.
  if kind == "mag.loaded" then capture_kernel_factories(body) end

  -- MAG kernel path.
  -- mag.hello advertises the factory registry at plugin startup — the startup
  -- validation snapshot.
  if kind == "mag.hello" then
    capture_kernel_factories(body)
    return
  end
  -- mag.loaded is the load-reply half of the synchronous handshake: refresh
  -- the registry, then resume the awaiting compile (preview) or apply
  -- (validate → fresh mag.execute or live mag.apply internally).
  if kind == "mag.loaded" then
    resume_pending_load(body)
    return
  end
  if kind == "mag.applied" then
    resolve_pending_apply(body)
    return
  end
  if (kind == "ncp.delivery_failed" and body.peer ~= "mag")
      or ((kind == "engine.plugin_transport_failed" or kind == "engine.plugin_delivery_lost")
        and body.plugin ~= "mag") then
    local peer = body.peer or body.plugin
    if type(peer) ~= "string" then return end
    if peer ~= "tool-gate" and peer ~= "basic-tools" and peer ~= "git-worktree"
        and peer ~= SOURCE_NAME then return end
    local identified = false
    if kind == "ncp.delivery_failed" and body.extent == "none" then
      for _, correlation in ipairs(body.correlations or {}) do
        -- Source and id are supplied by the failed egress, never parsed from
        -- opaque actor identities. The gate owns inner-to-outer rewriting.
        local invoke_kind = (peer == "basic-tools" or peer == "git-worktree")
          and peer .. ".capability.invoke" or peer .. ".tool.invoke"
        if correlation.kind == invoke_kind and type(correlation.id) == "string" then
          identified = true
          emit_as(peer, nil, { kind = "tool.result", id = correlation.id,
            error = "Invocation was not delivered; earlier effects are unknown" })
        end
      end
    end
    if not identified then
      require("libs.agentic-loop").capability_lost(peer,
        "Capability delivery failed; accepted outcomes are unknown")
      -- Do not invent run terminals while the MAG authority survives.
      interrupt_active_runs()
    end
    return
  end
  if kind == "ncp.delivery_failed" and body.peer == "mag" then
    if replay_window.active() then return end
    if body.extent == "none" then
      local identified = false
      for _, correlation in ipairs(body.correlations or {}) do
        local rejection = { in_reply_to = correlation.id, message = body.message }
        if type(correlation.id) == "string" and (correlation.kind == "mag.apply"
            or correlation.kind == "mag.execute" or correlation.kind == "mag.load"
            or correlation.kind == "mag.build") then identified = true end
        if correlation.kind == "mag.apply" then
          fail_pending_apply(rejection)
        elseif correlation.kind == "mag.execute" then
          handle_mag_pre_start_error(rejection)
        elseif correlation.kind == "mag.load" or correlation.kind == "mag.build" then
          fail_pending_load(rejection)
        end
      end
      if not identified then
        reconcile_mag_authority_loss("MAG delivery outcome cannot be correlated; accepted effects are unknown")
      end
    else
      reconcile_mag_authority_loss("MAG delivery failed; accepted effects are unknown")
    end
    return
  end
  if (kind == "engine.plugin_transport_failed" or kind == "engine.plugin_delivery_lost")
      and body.plugin == "mag" then
    if not replay_window.active() then
      reconcile_mag_authority_loss("MAG transport capability was lost; accepted outcomes are unknown")
    end
    return
  end

  -- mag.error may reject either an in-flight load or a submitted execute
  -- before begin_run. The load owners consume their correlations first; a
  -- queued registry entry retains an admission rejection, not an invented
  -- executor result; its grace timer cannot emit a false executing acknowledgment.
  if kind == "mag.error" then
    if fail_pending_apply(body) then return end
    if handle_mag_pre_start_error(body) then return end
    fail_pending_load(body)
    return
  end
  if kind == "mag.run_result" then
    handle_mag_run_result(body)
    return
  end
  if kind == "mag.actor_resumed" then
    local owner = run_registry:get(body.run_id)
    for _, delivered in ipairs(owner and owner.owner_delivery_from or {}) do
      if not delivered.owner_delivery_acked and delivered.owner_resume.actor_id == body.actor_id then
        -- The kernel serializes resume receipts for each run/actor pair.
        delivered.owner_delivery_acked = true
        if body.accepted ~= true then
          local al = require("libs.agentic-loop")
          for _, request_id in ipairs(delivered.request_ids or {}) do
            al.fail_request(request_id, {
              code = "completion_undeliverable",
              message = "Kernel rejected detached completion delivery to its owner",
              run_id = delivered.run_id,
            })
          end
          settle_request_run(delivered)
        end
        break
      end
    end
    return
  end
  -- Kernel actor lifecycle: keep the tracked run's node statuses at the same
  -- truth the chat surface's live panel tracks, so mag-status and the
  -- run-result block report done/killed rather than dispatch-time pending.
  if kind == "mag.run_started" then
    mark_mag_run_started(body)
    return
  end
  -- The same declarations the sidebar folds: a run's logical nodes grow as
  -- templates materialize, and a workspace follows its node's path.
  if kind == "mag.nodes_declared" then
    local run = mag_event_run(body)
    if run and type(body.nodes) == "table" then
      run.logical_nodes = run_projection.merge_declared(run.logical_nodes, body.nodes)
    end
    return
  end
  if kind == "mag.actor_spawned" then
    mark_mag_actor(body, "pending")
    return
  end
  if kind == "mag.actor_ready" then
    mark_mag_actor(body, "ready")
    return
  end
  if kind == "mag.actor_busy" then
    mark_mag_actor(body, "running")
    return
  end
  if kind == "mag.actor_idle" then
    mark_mag_actor(body, "done")
    return
  end
  if kind == "mag.actor_killed" then
    -- Teardown after a successful completion (reason "run_complete") is
    -- bookkeeping, not death: mag-status/result blocks report done. All
    -- other reasons (mid-run kill, run_failed, kill_run, session reap)
    -- report killed as before.
    mark_mag_actor(body, body.reason == "run_complete" and "done" or "killed")
    return
  end
  if kind == "mag.run_complete" then
    mark_mag_run_complete(body)
    return
  end

  -- Tool-gate hello — advertise our tools on first sight. Narrowed
  -- to `tool-gate.hello` specifically: matching any `*.hello` would
  -- mean the first non-gate plugin to say hello silently locks the
  -- advertised flag and tool-gate never sees the ad. The
  -- advertise_tools target must always be tool-gate.
  if kind == "tool-gate.hello" then
    advertise_tools("tool-gate")
    return
  end
end

-- Bus subscriptions — session lifecycle.
if nefor.bus and nefor.bus.on_event then
  nefor.bus.on_event("sessions.session_end", function(entry)
    state.gate_mode = "safe"
    local session_id
    if type(entry) == "table" and type(entry.payload) == "string" then
      local ok, decoded = pcall(json.decode, entry.payload)
      if ok and type(decoded) == "table" and type(decoded.body) == "table" then
        session_id = decoded.body.session_id
      end
    end
    terminate_active_graph(session_id)
  end)
end

local function cancel_request(request_id, _err)
  local count = 0
  for _, pending in pairs(state.pending_mag_load) do
    for _, id in ipairs(pending.request_ids or {}) do
      if id == request_id then
        invalidate_pending_mag_loads(pending.firing_id)
        count = count + 1
        break
      end
    end
  end
  for run_id, run in pairs(state.active_runs) do
    for _, owned_request_id in ipairs(run.request_ids or {}) do
      if owned_request_id == request_id then
        run_registry:mark_terminating(run_id, "request-failed")
        emit_as(SOURCE_NAME, "mag", {
          kind = "mag.interrupt_run",
          run_id = run_id,
          terminate = true,
        })
        count = count + 1
        break
      end
    end
  end
  return count
end

local M = {
  name        = "lead-workflow",
  receive_msg = receive_msg,
  send_msg    = function(_) end,

  has_approved_plan = has_approved_plan,
  cancel_request = cancel_request,
  reconcile_mag_authority_loss = reconcile_mag_authority_loss,

  assess_workflow = assess_workflow,

  _internals = {
    tool_source = tool_source,
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
    register_active_run = register_active_run,
    summarize_run = summarize_run,
    graph_status = graph_status,
    terminate_graph = terminate_graph,
    run_registry = run_registry,
    await_run = await_run,
    graph_status_cooldown_limit = GRAPH_STATUS_COOLDOWN_LIMIT,
    sync_completion_grace_ms = SYNC_COMPLETION_GRACE_MS,
    expire_completion_grace = expire_completion_grace,
    set_grace_scheduler = function(scheduler)
      grace_scheduler = scheduler
    end,
    termination_confirm_timeout_ms = TERMINATION_CONFIRM_TIMEOUT_MS,
    expire_termination_waiter = expire_termination_waiter,
    set_termination_scheduler = function(scheduler)
      termination_scheduler = scheduler
    end,
    set_monotonic_now_ms = function(now_fn)
      monotonic_now_ms = now_fn or function()
        if type(nefor.now_ms) == "function" then return nefor.now_ms() end
        return nil
      end
    end,
    set_graph_status_now = function(now_fn)
      graph_status_now = now_fn or os.time
    end,
    reset = function()
      state.active_run_id = nil
      run_registry:reset()
      state.active_runs = run_registry.active_runs
      state.completed_runs = run_registry.completed_runs
      state.graph_status_cooldowns = {}
      graph_status_now = os.time
      state.active_plan = nil
      state.gate_mode = "safe"
      state.kernel_factories = {}
      state.factory_approvals = {}
      state.pending_mag_load = {}
      state.pending_mag_apply = {}
      for run_id in pairs(state.completion_grace_waiters) do
        cancel_completion_grace(run_id)
      end
      state.completion_grace_waiters = {}
      grace_scheduler = nil
      for run_id in pairs(state.termination_waiters) do
        take_termination_waiters(run_id)
      end
      state.termination_waiters = {}
      termination_scheduler = nil
      state.mag_authority_lost = false
      monotonic_now_ms = function()
        if type(nefor.now_ms) == "function" then return nefor.now_ms() end
        return nil
      end
      dependency_module_roots = {}
      project_build = nil
      agent_system = nil
      ambient_context = nil
      resolve_model_snapshot = nil
      decide_capability = nil
      advertised = false
    end,
  },
}

-- configure_approval{ decide_capability }: what each capability means under
-- an approval policy (see `libs.tool`'s `decision`).
-- Separate from `configure` because a composition builds its tool sources
-- after this actor is configured.
function M.configure_approval(opts)
  if type(opts) ~= "table" or type(opts.decide_capability) ~= "function" then
    error("lead-workflow: configure_approval needs a decide_capability function", 2)
  end
  decide_capability = opts.decide_capability
end

function M.configure(opts)
  if opts == nil then opts = {} end
  if type(opts) ~= "table" then
    error("lead-workflow: configure options must be a table", 2)
  end
  project_build = mag.project_build_options(opts.project_build)
  local roots = opts.dependency_module_roots
  if roots == nil then roots = {} end
  validate_dependency_module_roots(roots)
  dependency_module_roots = copy_roots(roots)
  if opts.ambient_context ~= nil then
    if type(opts.ambient_context) ~= "table"
        or type(opts.ambient_context.compose) ~= "function" then
      error("lead-workflow: ambient_context must provide compose(base, opts)", 2)
    end
    ambient_context = opts.ambient_context
  else
    ambient_context = nil
  end
  if opts.agent_system ~= nil then
    if type(opts.agent_system) ~= "string" or #opts.agent_system == 0 then
      error("lead-workflow: agent_system must be a non-empty string", 2)
    end
    agent_system = opts.agent_system
  else
    agent_system = nil
  end
  if opts.resolve_model_snapshot ~= nil then
    if type(opts.resolve_model_snapshot) ~= "function" then
      error("lead-workflow: resolve_model_snapshot must be a function", 2)
    end
    resolve_model_snapshot = opts.resolve_model_snapshot
  end
end

return M
