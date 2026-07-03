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
-- Web review resolves from the browser review tool's saved markdown,
-- mapped back into the same approved/rejected/discarded contract.
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
--   * `write-review` (alias `submit-plan`) — args:
--       { plan = <string>, view = "inline"|"web" }
--     Stores the plan in `state.active_plan`, broadcasts
--     `lead-workflow.plan.submitted { plan, submitted_at }` for the
--     chat surface to render the yellow review block. Does NOT emit
--     tool.result — the agentic-loop blocks until the user verdict
--     resolves the deferred ack.
--
--   * `mag` — write, compile, and execute MAG workflow graphs.
--
--   * `mag-env` — initialise and return the MAG workspace path.
--
--   * `graph-status` — report active/recent graph run state.
--
--   * `terminate-graph` — cancel an active graph run by run_id.
--
-- ## Termination on session exit
--
-- Subscribes to `sessions.session_end`. If any lead-owned graph runs are
-- active, emits one `graph.cancel { run_id }` per run and archives each as
-- canceled. Also clears `state.active_plan` so a resumed/new session starts
-- with no carry-over approval.

local json = nefor.json

local mag            = require("mag")
local sessions      = require("sessions")
local envelope      = require("core.envelope")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit    = envelope.emit

local state = {
  -- The in-flight graph's run_id; nil when no graph is running.
  ---@type string|nil
  active_run_id = nil,

  -- Active graph runs keyed by run_id. Kept as a compact lead-facing
  -- view; agentic-loop remains the owner of actual pending graph relay.
  active_runs = {},
  completed_runs = {},
  completed_run_limit = 10,
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

  -- Factory names from the kernel registry, snapshotted from `mag.hello` (at
  -- plugin startup) and refreshed by every `mag.loaded` reply. The single
  -- source of truth for valid reasoner/factory types on both execute paths
  -- (see validate_reasoners). Deleting the old static KNOWN_REASONERS allowlist
  -- ([[task-nefor-mag-cutover-reasoner-graph]]).
  kernel_factories = {},

  -- Kernel-path executes awaiting their `mag.load` reply, keyed by the load
  -- request id. The synchronous handshake: `mag.load` is sent, the reply is
  -- awaited, factory validation runs against the reply's registry, then
  -- `mag.execute` is sent (resume_pending_execute). One entry per in-flight
  -- handshake; cleared when the load reply resolves it.
  pending_kernel_execute = {},
}

local SOURCE_NAME = "lead-workflow"

local register_active_run
local graph_status
local terminate_graph
local now_ms
local emit_verdict_approved
local emit_verdict_rejected
local emit_verdict_discarded

local VALID_PROFILES = { fast = true, standard = true, deep = true, max = true }

-- Per-execute switch: run `mag` execute on the actor kernel (mag.load +
-- mag.execute over the bus) instead of the reasoner-graph path. Default off;
-- cutover flips it ([[task-nefor-mag-cutover-reasoner-graph]]). Read live so a
-- config/env change takes effect without reload.
local function mag_execute_on_kernel()
  local ok, cfg = pcall(require, "config")
  local active = ok and type(cfg) == "table" and cfg.active or nil
  return type(active) == "table" and active.mag_execute_kernel == true
end

local function profile_config(name)
  if not VALID_PROFILES[name] then return nil end
  local ok, cfg = pcall(require, "config")
  local active = ok and type(cfg) == "table" and cfg.active or nil
  local profiles = type(active) == "table" and active.orchestration_profiles or nil
  local resolved = type(profiles) == "table" and profiles[name] or nil
  if type(resolved) == "table" then return resolved end
  local provider = type(active) == "table" and active.default_provider or nil
  local model = type(active) == "table" and active.default_model or nil
  local effort = ({ fast = "low", standard = "medium", deep = "high", max = "xhigh" })[name]
  return { provider = provider, model = model, reasoning_effort = effort }
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

-- The kernel's factory registry is the single source-of-truth for valid
-- reasoner/factory types on BOTH execute paths. It arrives on `mag.hello` (at
-- plugin startup) and every `mag.loaded` reply (the `factories` array) and is
-- cached in state.kernel_factories. `validate_reasoners` rejects any node whose
-- reasoner is not a registered factory. The old hand-synced KNOWN_REASONERS
-- allowlist is deleted ([[task-nefor-mag-cutover-reasoner-graph]]).
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

local function validate_reasoners(nodes, firing_id)
  local set = state.kernel_factories
  if type(set) ~= "table" or next(set) == nil then
    return true -- no registry snapshot yet; runtime spawn is the backstop
  end
  for _, node in ipairs(nodes or {}) do
    if set[node.reasoner] ~= true then
      emit_tool_result_err(firing_id,
        "mag execute: node '" .. tostring(node.id) .. "' uses unknown reasoner type '" ..
        tostring(node.reasoner) .. "'. Known types: " .. table.concat(sorted_keys(set), ", ") .. ".")
      return false
    end
  end
  return true
end

local function sh_quote(value)
  return "'" .. tostring(value):gsub("'", "'\\''") .. "'"
end

local function data_root()
  if nefor and nefor.fs and type(nefor.fs.data_root) == "function" then
    local ok, root = pcall(nefor.fs.data_root)
    if ok and type(root) == "string" and root ~= "" then return root end
  end
  local override = os.getenv("NEFOR_DATA_DIR")
  if override ~= nil and override ~= "" then return override end
  local xdg = os.getenv("XDG_DATA_HOME")
  if xdg ~= nil and xdg ~= "" then return xdg .. "/nefor" end
  local home = os.getenv("HOME") or ""
  if home == "" then return nil end
  return home .. "/.local/share/nefor"
end

local function read_agentic_kit_path(key)
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  local fh = io.open(config_dir .. "/agentic-kit.json", "r")
  if not fh then return nil end
  local raw = fh:read("*a")
  fh:close()
  local ok, decoded = pcall(json.decode, raw)
  if ok and type(decoded) == "table" and type(decoded[key]) == "string" then
    return decoded[key]
  end
  return raw:match('"' .. key .. '"%s*:%s*"([^"]+)"')
end

local function review_hook_path()
  local override = os.getenv("NEFOR_REVIEW_HOOK")
  if override ~= nil and override ~= "" then return override end
  local kit = read_agentic_kit_path("agentic_kit")
  if type(kit) == "string" and kit ~= "" then
    return kit .. "/agents/nefor/scripts/review-hook.sh"
  end
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  return config_dir .. "/../scripts/review-hook.sh"
end

local function mkdir_p(path)
  if nefor and nefor.fs and type(nefor.fs.mkdir_p) == "function" then
    local ok = pcall(nefor.fs.mkdir_p, path)
    if ok then return true end
  end
  local ok = os.execute("mkdir -p " .. sh_quote(path) .. " >/dev/null 2>&1")
  return ok == true or ok == 0
end

local function write_text(path, content)
  local fh, err = io.open(path, "w")
  if not fh then return nil, tostring(err) end
  fh:write(content)
  fh:close()
  return true, nil
end

local function web_review_dirs()
  local root = data_root()
  if not root then return nil, nil, "no Nefor data root available" end
  local save_dir = os.getenv("REVIEW_SAVE_DIR")
  if save_dir == nil or save_dir == "" then save_dir = root .. "/reviews/plans" end
  return save_dir, root .. "/reviews/tmp", nil
end

local function run_web_review(plan)
  local save_dir, scratch_dir, dir_err = web_review_dirs()
  if dir_err then return nil, dir_err end
  if not mkdir_p(save_dir) then return nil, "failed to create review save dir: " .. save_dir end
  if not mkdir_p(scratch_dir) then return nil, "failed to create review scratch dir: " .. scratch_dir end

  local stamp = tostring(now_ms()):gsub("[^%w%-_]", "-")
  local plan_path = scratch_dir .. "/write-review-plan-" .. stamp .. ".md"
  local input_path = scratch_dir .. "/write-review-input-" .. stamp .. ".json"
  local ok, err = write_text(plan_path, plan)
  if not ok then return nil, "failed to write review plan: " .. tostring(err) end
  ok, err = write_text(input_path, json.encode({ plan_path = plan_path, content = plan }))
  if not ok then return nil, "failed to write review hook input: " .. tostring(err) end

  local cmd = "REVIEW_SAVE_DIR=" .. sh_quote(save_dir) .. " " ..
    sh_quote(review_hook_path()) .. " < " .. sh_quote(input_path) .. " 2>&1"
  local pipe = io.popen(cmd, "r")
  if not pipe then return nil, "failed to launch review hook" end
  local raw = pipe:read("*a") or ""
  local close_ok, close_reason, close_code = pipe:close()
  if close_ok ~= true then
    return nil, "review hook failed (" .. tostring(close_reason) .. ":" .. tostring(close_code) .. "): " .. raw
  end

  local decoded_ok, decoded = pcall(json.decode, raw)
  if not decoded_ok or type(decoded) ~= "table" then
    return nil, "review hook returned invalid JSON: " .. raw
  end
  decoded.save_dir = decoded.save_dir or save_dir
  return decoded, nil
end

local function resolve_web_review(firing_id, result)
  if type(result) ~= "table" then
    state.active_plan = nil
    emit_verdict_discarded(firing_id, "Web review returned no verdict.")
    return
  end
  local status = result.status
  local comments = result.comments
  local plan = state.active_plan
  if type(plan) == "table" then
    plan.pending_firing_id = nil
    plan.reason = comments
  end

  if status == "approved" then
    if type(plan) == "table" then plan.status = "approved" end
    emit_as(SOURCE_NAME, nil, {
      kind             = "lead-workflow.plan.approved",
      plan_id          = type(plan) == "table" and plan.plan_id or nil,
      submitted_at     = type(plan) == "table" and plan.submitted_at or nil,
      approved         = true,
      approval_reason  = comments,
    })
    emit_verdict_approved(firing_id, comments)
    return
  end

  if status == "changes_needed" then
    if type(plan) == "table" then plan.status = "rejected" end
    emit_as(SOURCE_NAME, nil, {
      kind             = "lead-workflow.plan.approved",
      plan_id          = type(plan) == "table" and plan.plan_id or nil,
      submitted_at     = type(plan) == "table" and plan.submitted_at or nil,
      approved         = false,
      approval_reason  = comments,
    })
    emit_verdict_rejected(firing_id, comments)
    return
  end

  state.active_plan = nil
  emit_verdict_discarded(firing_id, comments or "Web review was closed without a verdict.")
end

local function has_approved_plan()
  return type(state.active_plan) == "table"
         and state.active_plan.status == "approved"
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
  local view = args and args.view
  if view ~= "inline" and view ~= "web" then
    emit_tool_result_err(firing_id, "write-review: args.view must be `inline` or `web`")
    return
  end
  if state.gate_mode == "auto" then
    emit_tool_result_err(firing_id,
      "permission_denied[auto]: write-review requires human approval, and /auto never opens a pending approval. " ..
      "Recovery: switch to /safe to review and approve a plan manually, or continue with read-only investigation.")
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

  state.active_plan = {
    plan_id           = plan_id,
    content           = plan,
    submitted_at      = submitted_at,
    pending_firing_id = (not yolo_approved) and firing_id or nil,
    status            = yolo_approved and "approved" or "pending",
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

  if yolo_approved then
    emit_as(SOURCE_NAME, nil, {
      kind     = "lead-workflow.plan.approved",
      plan_id  = plan_id,
      approved = true,
    })
    emit_tool_result_ok(firing_id, {
      status = "approved",
      notice = "YOLO mode: write-review approval gate bypassed; proceed with implementation.",
    })
    return
  end

  if view == "web" then
    local result, err = run_web_review(plan)
    if err then
      state.active_plan = nil
      emit_tool_result_err(firing_id, "write-review web review failed: " .. tostring(err))
      return
    end
    resolve_web_review(firing_id, result)
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
  local notice = "Plan approved by user. Proceed with the implementation " ..
                 "now — use mag to execute the implementation graph " ..
                 "as the next tool call. The approval is valid for this " ..
                 "turn only."
  local out = { status = "approved", notice = notice }
  if type(note) == "string" and #note > 0 then out.note = note end
  emit_tool_result_ok(firing_id, out)
end

emit_verdict_rejected = function(firing_id, reason)
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

emit_verdict_discarded = function(firing_id, comment)
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

register_active_run = function(run_id, graph, terminal, firing_id)
  local nodes_order, nodes = {}, {}
  for _, node in ipairs((type(graph) == "table" and graph.nodes) or {}) do
    local id = tostring(node.id or "")
    if id ~= "" then
      nodes_order[#nodes_order + 1] = id
      nodes[id] = {
        id = id,
        role = node.role or node.reasoner,
        reasoner = node.reasoner,
        status = "pending",
      }
    end
  end
  local ts = now_ms()
  state.active_runs[run_id] = {
    run_id = run_id,
    status = "queued",
    dispatched_at = ts,
    updated_at = ts,
    terminal = terminal,
    dispatch_firing_id = firing_id,
    nodes_order = nodes_order,
    nodes = nodes,
  }
  state.active_run_id = run_id
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
        firing_id = node.firing_id,
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

local function archive_run(run)
  local summary = summarize_run(run)
  if not summary then return end
  state.completed_runs[#state.completed_runs + 1] = summary
  while #state.completed_runs > (state.completed_run_limit or 10) do
    table.remove(state.completed_runs, 1)
  end
end

local function refresh_active_run_id()
  local latest_id, latest_at
  for run_id, run in pairs(state.active_runs) do
    local at = tonumber(run.dispatched_at) or 0
    if latest_at == nil or at > latest_at then
      latest_id, latest_at = run_id, at
    end
  end
  state.active_run_id = latest_id
end

local function archive_canceled_run(run_id, reason)
  local run = state.active_runs[run_id]
  if type(run) ~= "table" then return nil end
  local ts = now_ms()
  run.status = "canceled"
  run.updated_at = ts
  run.cancel_reason = reason
  for _, id in ipairs(run.nodes_order or {}) do
    local node = run.nodes and run.nodes[id]
    if node then
      if node.status == "pending" or node.status == "running" then
        node.status = "canceled"
        node.completed_at = node.completed_at or ts
      end
      if node.firing_id then state.firing_to_node[node.firing_id] = nil end
    end
  end
  state.active_runs[run_id] = nil
  if state.active_run_id == run_id then refresh_active_run_id() end
  archive_run(run)
  return summarize_run(run)
end

local function finish_run(run_id, status, results, explicit_error)
  local run = state.active_runs[run_id]
  if type(run) ~= "table" then return end
  local ts = now_ms()
  run.status = status or (explicit_error and "failed" or "completed")
  run.updated_at = ts
  run.result = results
  run.error = explicit_error
  if type(results) == "table" then
    for node_id, value in pairs(results) do
      local node = run.nodes and run.nodes[node_id]
      if node then
        node.status = "done"
        node.completed_at = node.completed_at or ts
        if type(value) == "table" and value.error ~= nil then
          node.status = "error"
          node.error = value.error
        elseif type(value) == "table" and type(value.output) == "table" then
          node.output_path = value.output.output_path
          node.output_relpath = value.output.output_relpath
        end
      end
    end
  end
  for _, id in ipairs(run.nodes_order or {}) do
    local node = run.nodes and run.nodes[id]
    if node and node.status == "running" then
      node.status = explicit_error and "error" or "done"
      node.completed_at = node.completed_at or ts
      if explicit_error then node.error = explicit_error end
    end
    if node and node.firing_id then state.firing_to_node[node.firing_id] = nil end
  end
  state.active_runs[run_id] = nil
  if state.active_run_id == run_id then state.active_run_id = nil end
  archive_run(run)
end

local function mark_run_started(body)
  local run_id = body.run_id
  local run = type(run_id) == "string" and state.active_runs[run_id] or nil
  if not run then return end
  run.status = "running"
  run.updated_at = now_ms()
end

local function mark_node_fired(body)
  local run_id, node_id, firing_id = body.run_id, body.node_id, body.firing_id
  local run = type(run_id) == "string" and state.active_runs[run_id] or nil
  local node = run and run.nodes and run.nodes[node_id]
  if not node then return end
  local ts = now_ms()
  run.status = "running"
  run.updated_at = ts
  node.status = "running"
  node.reasoner = body.reasoner or node.reasoner
  node.firing_id = firing_id
  node.started_at = node.started_at or ts
  if type(firing_id) == "string" and firing_id ~= "" then
    state.firing_to_node[firing_id] = { run_id = run_id, node_id = node_id }
  end
end

local function mark_node_tool(body)
  local run = type(body.run_id) == "string" and state.active_runs[body.run_id] or nil
  local node = run and run.nodes and run.nodes[body.node_id]
  if not node then return end
  run.updated_at = now_ms()
  node.last_tool = body.tool_name or body.name
  node.last_tool_args = body.tool_args or body.args
end

local function mark_node_chat_bound(body)
  local run = type(body.run_id) == "string" and state.active_runs[body.run_id] or nil
  local node = run and run.nodes and run.nodes[body.node_id]
  if not node then return end
  run.updated_at = now_ms()
  node.chat_id = body.chat_id
end

local function mark_firing_result(body)
  local id = body.id
  local map = type(id) == "string" and state.firing_to_node[id] or nil
  if not map then return end
  local run = state.active_runs[map.run_id]
  local node = run and run.nodes and run.nodes[map.node_id]
  state.firing_to_node[id] = nil
  if not node then return end
  local ts = now_ms()
  run.updated_at = ts
  node.completed_at = ts
  if body.error ~= nil then
    node.status = "error"
    node.error = body.error
  else
    node.status = "done"
    if type(body.result) == "table" then
      node.output_path = body.result.output_path
      node.output_relpath = body.result.output_relpath
    end
  end
end

-- Snapshot the kernel registry's factory names from a mag.hello / mag.loaded
-- body. This is the source of truth for reasoner/factory validation
-- (validate_reasoners).
local function capture_kernel_factories(body)
  if type(body.factories) ~= "table" then return end
  local set = {}
  for _, name in ipairs(body.factories) do
    if type(name) == "string" then set[name] = true end
  end
  state.kernel_factories = set
end

-- The lead reads paths; the model needs content. Read the sink's output file so
-- the run-completion turn carries the actual result, not just a path. Best
-- effort — an unreadable path yields nil and the turn still relays the path.
local function read_output_file(path)
  if type(path) ~= "string" or path == "" then return nil end
  local fh = io.open(path, "r")
  if not fh then return nil end
  local content = fh:read("*a")
  fh:close()
  return content
end

-- Relay a kernel run's completion to the model as a fresh orchestrator turn,
-- via the SAME mechanism agentic-loop's sub-graph completions use
-- (deferred-queue + flush → new user-role turn). Parity: the reasoner-graph
-- path gets this relay from agentic-loop's close_sub_graph; the kernel path
-- drives execution outside that queue, so the lead injects the completion turn
-- itself. `format_deferred` frames it; the output carries the sink PATH plus the
-- content read from it.
local function relay_kernel_completion(run_id, ok, output_path, content, err)
  local al = require("agentic-loop")
  if type(al.relay_run_completion) ~= "function" then return end
  if ok then
    local parts = {}
    if type(output_path) == "string" and #output_path > 0 then
      parts[#parts + 1] = "output_path: " .. output_path
    end
    parts[#parts + 1] = tostring(content or "")
    al.relay_run_completion({
      run_id = run_id,
      status = "success",
      output = table.concat(parts, "\n\n"),
    })
  else
    al.relay_run_completion({
      run_id = run_id,
      status = "failed",
      error  = err,
    })
  end
end

-- Append the transcript run-result block for a closed kernel run, the kernel
-- equivalent of the reasoner-graph path's close_sub_graph → chat.graph_result
-- .append (agentic-loop). Carries status + run id/name + the sink's output
-- PATH; the output CONTENT is deliberately omitted — it arrives separately as
-- the relayed fresh turn (relay_kernel_completion), so duplicating it here
-- would double-render it. `run` is read after finish_run, whose node-status
-- finalisation is exactly what the block should show.
local function emit_mag_result_block(run, status, output_path, err)
  local block = {
    kind   = "chat.graph_result.append",
    run_id = run.run_id,
    status = status,
    nodes  = ordered_node_summaries(run),
  }
  if status == "success" then
    if type(output_path) == "string" and #output_path > 0 then
      block.output = "output_path: " .. output_path
    end
  elseif type(err) == "string" and #err > 0 then
    block.error = err
  end
  emit("nefor-tui", block)
end

-- Close a kernel run on its terminal mag.run_result. Updates run/graph-status
-- state with the sink's output PATH (control-plane semantics — path, not data),
-- appends the visible run-result block, then relays the completion to the model
-- as a fresh turn (item 2 parity).
local function handle_mag_run_result(body)
  local run_id = body.run_id or body.in_reply_to
  if type(run_id) ~= "string" then return end
  local run = state.active_runs[run_id]
  if not run then return end
  if body.status == "failed" then
    finish_run(run_id, "failed", nil, body.error or "mag run failed")
    emit_mag_result_block(run, "failed", nil, body.error or "mag run failed")
    relay_kernel_completion(run_id, false, nil, nil, body.error or "mag run failed")
    return
  end
  local results = {}
  if type(run.terminal) == "string" and body.output_path ~= nil then
    results[run.terminal] = { output = { output_path = body.output_path } }
  end
  finish_run(run_id, "completed", results, nil)
  emit_mag_result_block(run, "success", body.output_path, nil)
  relay_kernel_completion(run_id, true, body.output_path,
    read_output_file(body.output_path), nil)
end

terminate_graph = function(firing_id, args)
  local run_id = args and args.run_id
  if type(run_id) ~= "string" or run_id == "" then
    emit_tool_result_err(firing_id, "terminate-graph: args.run_id must be a non-empty active graph run id")
    return
  end

  if type(state.active_runs[run_id]) ~= "table" then
    emit_tool_result_ok(firing_id, {
      canceled = false,
      run_id = run_id,
      status = "not_found",
      notice = "active graph run not found; no graphs were canceled",
    })
    return
  end

  emit_as(SOURCE_NAME, nil, { kind = "graph.cancel", run_id = run_id })
  local summary = archive_canceled_run(run_id, "terminate-graph")
  emit_tool_result_ok(firing_id, { canceled = true, run_id = run_id, run = summary })
end

graph_status = function(firing_id, args)
  local now = os.time()
  local cooldown = 60
  if state.last_graph_status_at and (now - state.last_graph_status_at) < cooldown then
    emit_tool_result_err(firing_id,
      "graph-status blocked: you called it less than " .. cooldown .. "s ago. " ..
      "Graph results arrive automatically — do not poll. " ..
      "Stop calling graph-status and wait for the result to arrive, or address the user.")
    return
  end
  state.last_graph_status_at = now

  local run_id = args and args.run_id
  if type(run_id) == "string" and run_id ~= "" then
    local run = state.active_runs[run_id]
    if run then
      emit_tool_result_ok(firing_id, { active = true, run = summarize_run(run) })
      return
    end
    for i = #state.completed_runs, 1, -1 do
      local summary = state.completed_runs[i]
      if summary.run_id == run_id then
        emit_tool_result_ok(firing_id, { active = false, run = summary })
        return
      end
    end
    emit_tool_result_ok(firing_id, { active = false, run_id = run_id, status = "unknown", notice = "graph run not found" })
    return
  end

  local active = {}
  for _, run in pairs(state.active_runs) do active[#active + 1] = summarize_run(run) end
  table.sort(active, function(a, b) return tostring(a.dispatched_at) < tostring(b.dispatched_at) end)
  local recent = {}
  for _, summary in ipairs(state.completed_runs) do recent[#recent + 1] = summary end
  emit_tool_result_ok(firing_id, { active = active, recent = recent })
end

local function terminate_active_graph()
  -- Session boundary flushes the plan slot unconditionally — no
  -- approval survives across sessions. If a write-review was in-flight
  -- at session-end, the deferred firing is abandoned; the agentic-loop
  -- state is torn down with the session so there's nothing to ack into.
  state.active_plan = nil

  local run_ids = {}
  for run_id, _ in pairs(state.active_runs) do run_ids[#run_ids + 1] = run_id end
  table.sort(run_ids)
  if #run_ids == 0 then return end

  -- Broadcast (target = nil) rather than target reasoner-graph: every
  -- in-flight agent reasoner under this run also needs to see the
  -- envelope so it can interrupt its provider stream + close its
  -- firing (sub-graph cancel propagation). The reasoner-graph
  -- binary still receives the broadcast and processes it the same way.
  for _, run_id in ipairs(run_ids) do
    emit_as(SOURCE_NAME, nil, { kind = "graph.cancel", run_id = run_id })
    archive_canceled_run(run_id, "session-end")
    -- Previously this emitted a "[Graph terminated by user — session
    -- exit]" chat.message.append for user feedback, but the message
    -- went into the bus log and leaked into the NEXT session's chat
    -- when /new replayed bus state. The cancel itself (above) is the
    -- functional close; the user already knows they ended the
    -- session. Logging only.
    nefor.log.info("lead-workflow: graph terminated on session-end", { run_id = run_id })
  end
end

-- tools.advertise on first <gate>.hello (best-effort; the actor still
-- works without the gate being up — tests drive tool.invoke envelopes
-- synthetically).

local advertised = false

local function lead_workflow_tool_schemas()
  return {
    {
      name        = "graph-status",
      description = "Report active graph runs, or one active/recent completed run when run_id is provided.",
      parameters  = {
        type = "object",
        properties = {
          run_id = {
            type = "string",
            description = "Optional graph run id. Omit to list active runs and recent completed summaries.",
          },
        },
      },
    },
    {
      name        = "terminate-graph",
      description = "Cancel exactly one active graph run by explicit run_id and archive it as canceled.",
      parameters  = {
        type = "object",
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
        properties = {
          plan = { type = "string" },
          view = { type = "string", enum = { "inline", "web" } },
        },
        required = { "plan", "view" },
      },
    },
    {
      name        = "mag",
      description =
        "Write, compile, and execute MAG workflow graphs. " ..
        "Use action='write' to create/update a .mag file in the workspace. " ..
        "Use action='compile' (default) to compile and preview. " ..
        "Use action='execute' to compile and submit for execution.",
      parameters  = {
        type = "object",
        properties = {
          action = {
            type        = "string",
            enum        = { "write", "compile", "execute" },
            description = "write: create/update a .mag file. compile: compile and preview (default). execute: compile and submit.",
          },
          file = {
            type        = "string",
            description = "Path to the .mag file, relative to the MAG workspace.",
          },
          content = {
            type        = "string",
            description = "File content (required for action=write).",
          },
        },
        required = { "file" },
      },
    },
    {
      name        = "mag-env",
      description = "Get the MAG workspace directory path. Creates and seeds " ..
        "the workspace if it doesn't exist yet.",
      parameters  = {
        type = "object",
        properties = {},
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

-- MAG tool handlers.
--
-- mag-env: initialise and return the MAG workspace path.
-- mag: compile a .mag file, optionally execute the resulting graph.

local function mag_env_handler(firing_id, _args)
  local session_id = sessions.current_id()
  if not session_id then
    emit_tool_result_err(firing_id, "mag-env: no active session")
    return
  end
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  local ws, err = mag.init_workspace(session_id, config_dir)
  if not ws then
    emit_tool_result_err(firing_id, "mag-env: " .. tostring(err))
    return
  end
  emit_tool_result_ok(firing_id, {
    workspace = ws,
    message = "MAG workspace ready at: " .. ws ..
      "\nLibrary files seeded in lib/. Write .mag files here, then use the mag tool to compile and execute.",
  })
end

-- Kernel execute path (behind the mag_execute_on_kernel switch): a synchronous
-- load → validate → execute handshake. `mag.load` is sent, its reply is awaited
-- (resume_pending_execute, triggered by the mag.loaded event), factory
-- validation runs against the reply's registry, and only then is `mag.execute`
-- sent. The kernel then loads the compiled program and runs it on the actor
-- kernel rather than submitting to reasoner-graph. Lifecycle events
-- (mag.run_started, actor spawn/ready, mag.run_complete) stream on the bus; the
-- terminal mag.run_result (carrying the sink's output PATH) closes the run and
-- relays a fresh model turn in receive_msg.
--
-- Profile params resolved lead-side (fast/standard/deep/max → provider/model/
-- reasoning_effort) are threaded to the actors via `params_overlay` on
-- mag.execute — a per-actor-id param patch the kernel merges before spawn (actor
-- params are kernel-opaque, so an overlay is legitimate control-plane input,
-- ir.md). `overlay` is built lead-side in the profile-resolution loop.
local function mag_execute_kernel(firing_id, args, ws, graph_spec, sink_id, graph_name, overlay)
  local session_id = sessions.current_id()
  local run_id = "mag-" .. tostring(graph_name) .. "-" .. tostring(now_ms())
  local load_id = run_id .. "-load"

  -- Stash the execute context; resume_pending_execute completes it when the
  -- load reply arrives (keyed by the load request id, echoed as in_reply_to).
  state.pending_kernel_execute[load_id] = {
    firing_id  = firing_id,
    run_id     = run_id,
    run_name   = graph_name,
    session_id = session_id,
    graph_spec = graph_spec,
    sink_id    = sink_id,
    nodes      = graph_spec.nodes,
    overlay    = overlay,
  }

  emit_as(SOURCE_NAME, "mag", {
    kind       = "mag.load",
    id         = load_id,
    source_dir = ws,
    entry      = args.file,
  })
end

-- Resume a kernel execute once its `mag.load` reply arrives. The reply's
-- registry has already refreshed state.kernel_factories (capture_kernel_factories
-- runs first); validate every node's factory against it, then send mag.execute
-- with the resolved session_id, run_id, and params overlay. Validation failure
-- acks the firing with an error and drops the pending execute — nothing runs.
local function resume_pending_execute(body)
  local load_id = body.in_reply_to
  local pending = type(load_id) == "string" and state.pending_kernel_execute[load_id] or nil
  if not pending then return end
  state.pending_kernel_execute[load_id] = nil

  if not validate_reasoners(pending.nodes, pending.firing_id) then
    return
  end

  local exec = {
    kind       = "mag.execute",
    id         = pending.run_id,
    run_id     = pending.run_id,
    run_name   = pending.run_name,
    session_id = pending.session_id,
  }
  if type(pending.overlay) == "table" and next(pending.overlay) ~= nil then
    exec.params_overlay = pending.overlay
  end
  emit_as(SOURCE_NAME, "mag", exec)

  register_active_run(pending.run_id, pending.graph_spec, pending.sink_id, pending.firing_id)

  emit_tool_result_ok(pending.firing_id, {
    status  = "executing",
    run_id  = pending.run_id,
    engine  = "mag-kernel",
    message = "Graph submitted to the MAG actor kernel. Results arrive automatically when the run " ..
      "completes. STOP here — do not call any more tools until results arrive.",
  })
end

local function mag_handler(firing_id, args)
  if type(args.file) ~= "string" or #args.file == 0 then
    emit_tool_result_err(firing_id, "mag: requires a non-empty 'file' argument")
    return
  end
  if args.file:sub(1, 1) == "/" then
    emit_tool_result_err(firing_id, "mag: absolute paths not allowed: " .. args.file)
    return
  end
  if args.file:find("%.%.") then
    emit_tool_result_err(firing_id, "mag: path traversal not allowed: " .. args.file)
    return
  end

  local session_id = sessions.current_id()
  if not session_id then
    emit_tool_result_err(firing_id, "mag: no active session")
    return
  end
  local config_dir = os.getenv("NEFOR_CONFIG_DIR") or "."
  local ws, ws_err = mag.init_workspace(session_id, config_dir)
  if not ws then
    emit_tool_result_err(firing_id, "mag: workspace init failed: " .. tostring(ws_err))
    return
  end
  local file_path = ws .. "/" .. args.file
  local action = args.action or "compile"

  -- Write action: create/update a .mag file in the workspace.
  if action == "write" then
    if type(args.content) ~= "string" then
      emit_tool_result_err(firing_id, "mag write: requires 'content' string")
      return
    end
    local dir = file_path:match("(.+)/[^/]+$")
    if dir then mkdir_p(dir) end
    local fh, open_err = io.open(file_path, "w")
    if not fh then
      emit_tool_result_err(firing_id, "mag write: cannot create " .. args.file .. ": " .. tostring(open_err))
      return
    end
    fh:write(args.content)
    fh:close()
    emit_tool_result_ok(firing_id, {
      status  = "written",
      file    = args.file,
      path    = file_path,
      message = "File written: " .. args.file,
    })
    return
  end

  -- Compile.
  local ir, err = mag.compile(file_path, ws)
  if not ir then
    emit_tool_result_err(firing_id, "compilation failed:\n" .. tostring(err))
    return
  end

  -- Preview (compile action, default).
  if action == "compile" then
    local preview = mag.preview(ir)
    emit_tool_result_ok(firing_id, {
      status  = "compiled",
      preview = preview,
      hash    = ir.hash,
      message = "Graph compiled successfully. Review the preview above. " ..
        "Call mag with action='execute' to run this graph.",
    })
    return
  end

  -- Execute: check approval gate for write-capable nodes.
  -- Only agent nodes with explicit write tools (edit_file, write_file)
  -- count as writers. Shell nodes and read-tool agents are allowed
  -- freely — the tool-gate and da-policies enforce runtime permissions.
  local WRITE_TOOLS = { ["fs/edit"] = true, ["edit_file"] = true, ["write_file"] = true }
  local has_writers = false
  for _, node in ipairs(ir.nodes or {}) do
    local tools = type(node.args) == "table" and node.args.tools or nil
    if type(tools) == "table" then
      for _, t in ipairs(tools) do
        if WRITE_TOOLS[t] then
          has_writers = true
          break
        end
      end
    end
    if has_writers then break end
  end

  if has_writers and state.gate_mode == "safe" then
    if not has_approved_plan() then
      emit_tool_result_err(firing_id,
        "Graph contains write-capable agents. Submit a plan via write-review " ..
        "and get approval before executing.")
      return
    end
  end

  -- Reasoner/factory validation catches typos + missing factories before
  -- submission, against the kernel registry (validate_reasoners). On the kernel
  -- path this runs post-load against the load reply's registry
  -- (resume_pending_execute), so it is skipped here. On the legacy
  -- reasoner-graph path, validate now against the startup registry snapshot
  -- (from mag.hello); when no snapshot exists it degrades to the runtime
  -- spawn-time check.
  local on_kernel = mag_execute_on_kernel()
  if not on_kernel then
    if not validate_reasoners(ir.nodes, firing_id) then return end
  end

  -- Resolve per-node profiles to reasoning_effort before submission.
  -- Agent/llm nodes with :profile get provider/model/reasoning_effort
  -- from orchestration_profiles. Nodes without a profile are rejected
  -- so the lead always makes an explicit reasoning-depth decision.
  --
  -- The resolution mutates node.args (carried into the legacy reasoner-graph
  -- program) AND records a per-node params overlay keyed by node id, which the
  -- kernel path threads onto mag.execute (params_overlay) — the kernel loads
  -- the .mag directly and never sees the mutated node.args, so the overlay is
  -- how profile params reach its actors. node.id == the actor id (the loader
  -- keeps ids; only the sink is renamed, and sinks are never profiled).
  local overlay = {}
  local PROFILED_REASONERS = { agent = true, llm = true }
  for _, node in ipairs(ir.nodes or {}) do
    if PROFILED_REASONERS[node.reasoner] and type(node.args) == "table" then
      local profile_name = node.args.profile
      local has_raw_effort = node.args.reasoning_effort ~= nil
      if profile_name ~= nil and has_raw_effort then
        emit_tool_result_err(firing_id,
          "mag execute: node '" .. tostring(node.id) ..
          "' sets both profile and reasoning_effort; use profile only")
        return
      end
      if type(profile_name) == "string" and #profile_name > 0 then
        if not VALID_PROFILES[profile_name] then
          emit_tool_result_err(firing_id,
            "mag execute: node '" .. tostring(node.id) ..
            "' has unknown profile '" .. profile_name ..
            "' (valid: fast, standard, deep, max)")
          return
        end
        local resolved = profile_config(profile_name)
        if type(resolved) == "table" then
          local patch = {}
          if type(resolved.reasoning_effort) == "string" then
            node.args.reasoning_effort = resolved.reasoning_effort
            patch.reasoning_effort = resolved.reasoning_effort
          end
          if type(resolved.provider) == "string" and #resolved.provider > 0 then
            node.args.provider = resolved.provider
            patch.provider = resolved.provider
          end
          if type(resolved.model) == "string" and #resolved.model > 0 then
            node.args.model = resolved.model
            patch.model = resolved.model
          end
          if next(patch) ~= nil then overlay[node.id] = patch end
        end
        node.args.profile = nil
      elseif not has_raw_effort then
        emit_tool_result_err(firing_id,
          "mag execute: node '" .. tostring(node.id) ..
          "' is missing required :profile (fast, standard, deep, or max)")
        return
      end
    end
  end

  -- Sink validation: exactly one sink node, it must have inputs and no
  -- outputs, and it becomes the terminal automatically.
  local sink_ids = {}
  for _, node in ipairs(ir.nodes or {}) do
    if node.reasoner == "sink" or node.reasoner == "terminal" then
      sink_ids[#sink_ids + 1] = node.id
    end
  end
  if #sink_ids == 0 then
    emit_tool_result_err(firing_id,
      "mag execute: graph has no sink node. Every graph must end with exactly one " ..
      "(node \"sink\" {:path \"/tmp/output.md\"} : Input -> Input) that collects the final output.")
    return
  end
  if #sink_ids > 1 then
    emit_tool_result_err(firing_id,
      "mag execute: graph has multiple sink nodes (" .. table.concat(sink_ids, ", ") ..
      "). Every graph must have exactly one sink.")
    return
  end
  local sink_id = sink_ids[1]

  local has_input, has_output = false, false
  for _, edge in ipairs(ir.edges or {}) do
    if edge.to == sink_id then has_input = true end
    if edge.from == sink_id then has_output = true end
  end
  if not has_input then
    emit_tool_result_err(firing_id,
      "mag execute: sink node '" .. sink_id .. "' has no inputs. " ..
      "Connect at least one upstream node to the sink.")
    return
  end
  if has_output then
    emit_tool_result_err(firing_id,
      "mag execute: sink node '" .. sink_id .. "' has outgoing edges. " ..
      "The sink must be the final node with no outputs.")
    return
  end

  -- Auto-set sink :path when missing.
  for _, node in ipairs(ir.nodes or {}) do
    if node.id == sink_id then
      if type(node.args) ~= "table" then node.args = {} end
      if type(node.args.path) ~= "string" or #node.args.path == 0 then
        node.args.path = "/tmp/nefor-output-" .. (ir.hash or "graph") .. ".md"
      end
      break
    end
  end

  -- Infer read_only for agent nodes without write tools.
  for _, node in ipairs(ir.nodes or {}) do
    if node.reasoner == "agent" and type(node.args) == "table" then
      local has_write = false
      local tools = node.args.tools
      if type(tools) == "table" then
        for _, t in ipairs(tools) do
          if WRITE_TOOLS[t] then has_write = true; break end
        end
      end
      if not has_write then
        node.args.read_only = true
      end
    end
  end

  local graph_spec = {
    terminal = sink_id,
    nodes    = ir.nodes,
    edges    = ir.edges,
  }
  local graph_name = args.file:gsub("%.mag$", ""):gsub("/", "-"):sub(1, 20)

  -- Per-execute switch: run on the MAG actor kernel instead of reasoner-graph.
  if on_kernel then
    mag_execute_kernel(firing_id, args, ws, graph_spec, sink_id, graph_name, overlay)
    return
  end

  -- Submit graph through agentic-loop's sub-graph queue (reasoner-graph path).
  local al = require("agentic-loop")
  local run_id = al.queue_sub_graph(
    { graph = graph_spec, on_node_failure = "abort", name = graph_name },
    firing_id)
  if type(run_id) ~= "string" then
    emit_tool_result_err(firing_id,
      "mag: agentic-loop refused the graph (queue_sub_graph returned nil)")
    return
  end
  register_active_run(run_id, graph_spec, sink_id, firing_id)
  al.flush_pending_dispatches()

  emit_tool_result_ok(firing_id, {
    status  = "executing",
    run_id  = run_id,
    hash    = ir.hash,
    message = "Graph submitted for execution. Results will arrive automatically when nodes complete. " ..
      "STOP here — do not call any more tools until results arrive. " ..
      "Do not investigate the same topic with your own tools while the graph is running.",
  })
end

local TOOL_HANDLERS = {
  ["graph-status"]    = graph_status,
  ["terminate-graph"] = terminate_graph,
  ["write-review"]    = submit_plan,
  ["submit-plan"]     = submit_plan,
  ["mag"]             = mag_handler,
  ["mag-env"]         = mag_env_handler,
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

  -- MAG kernel path (behind the mag_execute_on_kernel switch).
  -- mag.hello advertises the factory registry at plugin startup — the startup
  -- validation snapshot for both paths.
  if kind == "mag.hello" then
    capture_kernel_factories(body)
    return
  end
  -- mag.loaded is the load-reply half of the synchronous handshake: refresh the
  -- registry, then resume the awaiting execute (validate → mag.execute).
  if kind == "mag.loaded" then
    capture_kernel_factories(body)
    resume_pending_execute(body)
    return
  end
  if kind == "mag.run_result" then
    handle_mag_run_result(body)
    return
  end

  if kind == "graph.run_started" then
    mark_run_started(body)
    return
  end
  if kind == "graph.node.fired" then
    mark_node_fired(body)
    return
  end
  if kind == "graph.node.tool.invoke" then
    mark_node_tool(body)
    return
  end
  if kind == "graph.node.chat.bound" then
    mark_node_chat_bound(body)
    return
  end

  -- Watch for graph and per-node close envelopes so graph-status can
  -- report current node state and archive compact summaries on close.
  if kind == "tool.result" then
    local id = body.id
    if type(id) == "string"
        and type(body.result) == "table"
        and body.result.status ~= nil then
      finish_run(id, body.result.status, body.result.results, body.error)
    else
      mark_firing_result(body)
    end
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
  nefor.bus.on_event("sessions.session_end", function(_entry)
    terminate_active_graph()
  end)
end

return {
  name        = "lead-workflow",
  receive_msg = receive_msg,
  send_msg    = function(_) end,

  has_approved_plan = has_approved_plan,

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
    register_active_run = register_active_run,
    summarize_run = summarize_run,
    graph_status = graph_status,
    terminate_graph = terminate_graph,
    run_web_review = run_web_review,
    reset = function()
      state.active_run_id = nil
      state.active_runs = {}
      state.completed_runs = {}
      state.firing_to_node = {}
      state.active_plan = nil
      state.gate_mode = "safe"
      state.kernel_factories = {}
      state.pending_kernel_execute = {}
      advertised = false
    end,
  },
}
