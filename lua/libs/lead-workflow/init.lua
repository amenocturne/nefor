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
--   * `graph-status` — report active/recent graph run state.
--
--   * `terminate-graph` — cancel an active graph run by run_id.
--
-- ## Termination on session exit
--
-- Subscribes to `sessions.session_end`. If any lead-owned kernel runs are
-- active, emits one `mag.kill_run { run_id }` per run and archives each as
-- canceled. Also clears `state.active_plan` so a resumed/new session starts
-- with no carry-over approval.

local json = nefor.json

local mag            = require("libs.mag-workspace")
local mag_eval       = require("libs.lead-workflow.mag-eval")
local RunRegistry    = require("libs.lead-workflow.run-registry")
local chat_emitter   = require("libs.chat-emitter")
local sessions       = require("libs.sessions")
local envelope       = require("core.envelope")
local replay_window  = require("core.replay_window")

local emit_as = envelope.emit_as
local emit    = envelope.emit

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
  monotonic_ms = function()
    return nil
  end,
})

local dependency_module_roots = {}
local agent_defaults = nil

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
  roots[#roots + 1] = ws .. "/lib"
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

  -- One anti-polling timestamp per graph-status target. The all-runs snapshot
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

  -- `mag` tool invocations awaiting their `mag.load` reply, keyed by the load
  -- request id. Both compile and execute go through this handshake: `mag.load`
  -- is sent, the `mag.loaded` reply (carrying the modification + the registry)
  -- resolves it (resume_pending_load) — compile renders the preview, execute
  -- validates and sends `mag.execute`. A `mag.error` reply fails the firing
  -- with the compiler message. One entry per in-flight handshake.
  pending_mag_load = {},

  -- Kernel runs are concurrent: every kernel lifecycle event
  -- (`mag.actor_spawned` / `mag.actor_ready` / `mag.actor_killed` /
  -- `mag.run_complete`) carries its run_id, and the tracker keys straight
  -- into state.active_runs by it — overlapping runs track independently,
  -- no "the in-flight run" singleton.
}

local SOURCE_NAME = "lead-workflow"

local register_active_run
local submit_loaded_run
local invalidate_pending_mag_loads
local graph_status
local await_run
local terminate_graph
local now_ms
local emit_verdict_approved
local emit_verdict_rejected
local emit_verdict_discarded

local function orchestration_profiles()
  local ok, cfg = pcall(require, "config")
  if not ok or type(cfg) ~= "table" or type(cfg.active) ~= "table" then
    return nil, "config.active.orchestration_profiles is missing"
  end
  local profiles = cfg.active.orchestration_profiles
  if type(profiles) ~= "table" then
    return nil, "config.active.orchestration_profiles must be a table"
  end
  for name in pairs(profiles) do
    if type(name) ~= "string" or #name == 0 then
      return nil, "config.active.orchestration_profiles keys must be non-empty strings"
    end
  end
  local names = {}
  for name in pairs(profiles) do names[#names + 1] = name end
  table.sort(names)
  for _, name in ipairs(names) do
    local profile = profiles[name]
    if type(profile) ~= "table" then
      return nil, "configured profile '" .. name .. "' must be a table"
    end
    for _, field in ipairs({ "provider", "model", "reasoning_effort" }) do
      if type(profile[field]) ~= "string" or #profile[field] == 0 then
        return nil, "configured profile '" .. name ..
          "' requires a non-empty string " .. field
      end
    end
  end
  return profiles, nil
end

local function emit_tool_result_ok(firing_id, output)
  emit_as(SOURCE_NAME, nil, {
    kind   = "tool.result",
    id     = firing_id,
    output = output,
  })
end

local function emit_await_outcome(firing_id, outcome)
  local body = { kind = "tool.result", id = firing_id }
  if type(outcome) == "table" and outcome.output ~= nil then
    body.output = outcome.output
  else
    body.error = type(outcome) == "table" and outcome.error or "await-run failed"
    if type(outcome) == "table" then
      body.error_code = outcome.error_code
      body.run_id = outcome.run_id
      body.status = outcome.status
      body.run_name = outcome.run_name
      body.terminal = outcome.terminal
    end
  end
  emit_as(SOURCE_NAME, nil, body)
end

local function emit_tool_result_err(firing_id, err)
  emit_as(SOURCE_NAME, nil, {
    kind  = "tool.result",
    id    = firing_id,
    error = tostring(err),
  })
end

-- The kernel's foreign-contract registry is the source of truth for actor
-- capabilities. Library validation already checks the complete contracts;
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

local function validate_factories(actors, firing_id)
  local set = state.kernel_factories
  if type(set) ~= "table" or next(set) == nil then
    return true -- no registry snapshot yet; runtime spawn is the backstop
  end
  for _, actor in ipairs(actors or {}) do
    if set[actor.foreign] ~= true then
      emit_tool_result_err(firing_id,
        "mag execute: actor '" .. tostring(actor.id) .. "' uses unknown foreign '" ..
        tostring(actor.foreign) .. "'. Known capabilities: " ..
        table.concat(sorted_keys(set), ", ") .. ".")
      return false
    end
  end
  return true
end

-- Write-capable detection over a modification's actors: any actor whose
-- params.tools names a write-capable tool makes the program write-capable and
-- subject to the plan-approval gate. Shell and read-tool actors run freely —
-- the tool-gate and da-policies enforce runtime permissions.
local WRITE_TOOLS = { ["fs/edit"] = true, ["edit_file"] = true, ["write_file"] = true }

local function actors_have_writers(actors)
  for _, actor in ipairs(actors or {}) do
    local tools = type(actor.params) == "table" and actor.params.tools or nil
    if type(tools) == "table" then
      for _, t in ipairs(tools) do
        if WRITE_TOOLS[t] then return true end
      end
    end
  end
  return false
end

local function result_actor(modification)
  local result = type(modification.result) == "table" and modification.result.from or nil
  if type(result) ~= "table" or type(result.actor) ~= "string" then
    return nil, "artifact has no structural result boundary"
  end
  return result.actor, nil
end

local function compose_agent_system(base, positional_overlay)
  if type(positional_overlay) == "string" and positional_overlay:match("%S") then
    return base .. "\n\n---\n\n" .. positional_overlay
  end
  return base
end

local function is_llm_actor(actor)
  return actor.foreign == "nefor.factory.llm"
      or actor.foreign == "nefor.factory.structured-output"
end

local function selected_profile(value)
  if type(value) == "string" then return value end
  if type(value) == "table" and value.present == true then
    return value.value
  end
  return nil
end

-- Resolve runtime-owned LLM parameters over an artifact's actors. A raw actor
-- may select an explicit profile or reasoning effort; composition-provided
-- ready agents omit both and receive the configured defaults. The runtime
-- prepends the shared base prompt to each actor's positional system overlay.
local function resolve_agent_params(actors)
  local overlay = {}
  local profiles
  local profiles_err
  for _, actor in ipairs(actors or {}) do
    local params = type(actor.params) == "table" and actor.params or {}
    local profile_name = selected_profile(params.profile)
    local has_raw_effort = params.reasoning_effort ~= nil
    if profile_name ~= nil and has_raw_effort then
      return nil, "actor '" .. tostring(actor.id) ..
        "' sets both profile and reasoning_effort; use profile only"
    end
    if type(profile_name) == "string" and #profile_name > 0 then
      if profiles == nil and profiles_err == nil then
        profiles, profiles_err = orchestration_profiles()
      end
      if profiles_err ~= nil then
        return nil, "actor '" .. tostring(actor.id) .. "' uses profile '" ..
          profile_name .. "', but " .. profiles_err
      end
      local resolved = profiles[profile_name]
      if resolved == nil then
        local configured = sorted_keys(profiles)
        local suffix = #configured > 0 and table.concat(configured, ", ") or "none"
        return nil, "actor '" .. tostring(actor.id) ..
          "' has unknown profile '" .. profile_name ..
          "'. Configured profiles: " .. suffix .. "."
      end
      overlay[actor.id] = {
        provider = resolved.provider,
        model = resolved.model,
        reasoning_effort = resolved.reasoning_effort,
      }
    elseif is_llm_actor(actor) and not has_raw_effort then
      if agent_defaults == nil then
        return nil, "llm actor '" .. tostring(actor.id) ..
          "' is missing required :profile. " ..
          "Set :profile in the MAG library wrapper or configure agent_defaults."
      end
      overlay[actor.id] = {
        provider = agent_defaults.provider,
        model = agent_defaults.model,
        reasoning_effort = agent_defaults.reasoning_effort,
      }
    end
    if is_llm_actor(actor)
        and agent_defaults ~= nil then
      overlay[actor.id] = overlay[actor.id] or {}
      overlay[actor.id].system = compose_agent_system(agent_defaults.system, params.system)
    end
  end
  return overlay, nil
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
    plan.pending_correlation = nil
    plan.pending_run_id = nil
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
local function submit_plan(firing_id, args, metadata)
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
      notice = "YOLO mode: write-review approval gate bypassed; proceed with implementation."
    else
      notice = "AUTO mode: write-review approval gate bypassed; proceed with implementation."
    end
    emit_tool_result_ok(firing_id, {
      status = "approved",
      notice = notice,
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

-- Track a submitted run for graph-status. `actors` is the modification's
-- actor list ({ id, factory, … }); node summaries carry the factory under the
-- `reasoner` key, matching what the chat surface renders (chat/run_panel.lua
-- maps kernel factory → reasoner the same way).
register_active_run = function(run_id, actors, terminal, firing_id, run_name, session_id,
    dispatcher_id)
  local nodes_order, nodes = {}, {}
  for _, actor in ipairs(actors or {}) do
    local id = tostring(actor.id or "")
    if id ~= "" then
      nodes_order[#nodes_order + 1] = id
      nodes[id] = {
        id = id,
        role = actor.foreign,
        reasoner = actor.foreign,
        status = "pending",
      }
    end
  end
  local run = run_registry:register({
    run_id = run_id,
    run_name = run_name,
    session_id = session_id,
    terminal = terminal,
    dispatch_firing_id = firing_id,
    dispatcher_id = dispatcher_id,
    nodes_order = nodes_order,
    nodes = nodes,
  })
  state.active_run_id = run_id
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
-- resident done actor back to running. The event's run_id names the run. A
-- mid-run spawn (`mag.apply`) appends a node the dispatch-time modification
-- didn't carry.
local function mark_mag_actor(body, status)
  local run = mag_event_run(body)
  local actor_id, factory = body.id, body.factory
  if not run or type(actor_id) ~= "string" or actor_id == "" then return end
  local ts = now_ms()
  run.updated_at = ts
  local node = run.nodes[actor_id]
  if not node then
    node = {
      id = actor_id,
      role = factory,
      reasoner = factory,
      status = "pending",
    }
    run.nodes[actor_id] = node
    run.nodes_order[#run.nodes_order + 1] = actor_id
  end
  if type(factory) == "string" then
    node.reasoner = factory
    node.role = node.role or factory
  end
  if status == "running" then
    node.status = "running"
    node.started_at = node.started_at or ts
  elseif status == "killed" then
    node.status = "killed"
    node.completed_at = node.completed_at or ts
  elseif status == "done" then
    -- A completed run's teardown sweep (mag.actor_killed reason
    -- "run_complete"): the node finished, it wasn't terminated. Terminal
    -- states (killed from a mid-run kill) keep their truth.
    if node.status == "pending" or node.status == "running" then
      node.status = "done"
      node.completed_at = node.completed_at or ts
    end
  end
end

-- `mag.run_complete` (the sink's terminal signal, ahead of the terminal
-- mag.run_result): the kernel emits no per-actor "done", so every actor still
-- pending/running flips to done here; killed actors keep their terminal
-- state. Mirrors the chat surface's run panel (examples/nefor-agent/chat/run_panel.lua).
local function mark_mag_run_complete(body)
  local run = mag_event_run(body)
  if not run then return end
  local ts = now_ms()
  run.updated_at = ts
  for _, id in ipairs(run.nodes_order or {}) do
    local node = run.nodes[id]
    if node and (node.status == "pending" or node.status == "running") then
      node.status = "done"
      node.completed_at = node.completed_at or ts
    end
  end
end

-- Snapshot the kernel registry's factory names from a mag.hello / mag.loaded
-- body. This is the source of truth for factory validation
-- (validate_factories).
local function capture_kernel_factories(body)
  if type(body.foreign_contracts) ~= "table" then return end
  local set = {}
  for _, contract in ipairs(body.foreign_contracts) do
    local identity = type(contract) == "table" and contract.identity or nil
    if type(identity) == "string" then set[identity] = true end
  end
  state.kernel_factories = set
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
      and semantic.name == "nefor.contracts.FinalAnswer"
      and type(result.value) == "table"
      and type(result.value.content) == "string" then
    return result.value.content
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
local function relay_kernel_completion(run_id, run_name, ok, content, err)
  local al = require("libs.agentic-loop")
  if type(al.relay_run_completion) ~= "function" then return end
  if ok then
    local content_available = type(content) == "string" and content:find("%S") ~= nil
    al.relay_run_completion({
      run_id            = run_id,
      run_name          = run_name,
      status            = "success",
      content_available = content_available,
      output            = content_available and content or nil,
    })
  else
    al.relay_run_completion({
      run_id = run_id,
      run_name = run_name,
      status = "failed",
      error  = err,
    })
  end
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

-- Close a kernel run on its terminal mag.run_result. Updates run/graph-status
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

  local settled, transitioned = run_registry:settle(run_id, body)
  if not transitioned then return end
  run = settled.run
  if state.active_run_id == run_id then refresh_active_run_id() end

  local ts = now_ms()
  local failed = body.status == "failed" or body.status == "killed"
  local err = body.error
    or (body.status == "killed" and "run killed" or "mag run failed")
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
    if node and (node.status == "pending" or node.status == "running") then
      node.status = failed and (body.status == "killed" and "killed" or "error") or "done"
      node.completed_at = node.completed_at or ts
      if failed then node.error = err end
    end
  end

  for _, firing_id in ipairs(settled.waiters) do
    emit_await_outcome(firing_id, settled.outcome)
  end

  if not run_registry:claim_delivery(run_id) then return end
  local root_owned = run.dispatcher_id == nil
  if failed then
    if root_owned then
      emit_mag_result_block(run, "failed", nil, err)
      -- A TUI termination is already a user decision. Settlement and visibility
      -- still happen, but feeding the kill back as a task would restart the lead.
      if run.terminate_reason ~= "user-tui-termination" then
        relay_kernel_completion(run_id, run.run_name, false, nil, err)
      end
    end
    return
  end
  if root_owned then
    emit_mag_result_block(run, "success", body.output_path, nil)
    local content = mag_result_text(body.result)
      or read_output_file(body.output_path)
    relay_kernel_completion(run_id, run.run_name, true, content, nil)
  end
end

-- Double-Esc entry point (`chat.interrupt_all`). The `mag` execute tool is
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
-- covers file-based execute and lead-called eval identically.
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
    return { session_id = session_id, principal = principal, direct = true }
  end

  local invocation, validation_error = chat_emitter.validate_invocation(metadata.invocation)
  if not invocation then
    return nil, "invalid invocation provenance: " .. tostring(validation_error)
  end
  if sessions.current_id() ~= invocation.session_id then
    return nil, "invocation session is no longer active"
  end
  return {
    session_id = invocation.session_id,
    principal = invocation.principal,
    invocation = invocation,
    dispatcher_id = invocation.principal == "subagent" and invocation.actor_id or nil,
    conversation_id = invocation.conversation_id,
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

terminate_graph = function(firing_id, args, metadata)
  local run_id = args and args.run_id
  if type(run_id) ~= "string" or run_id == "" then
    emit_tool_result_err(firing_id, "terminate-graph: args.run_id must be a non-empty active graph run id")
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
      status = lookup_err and lookup_err.error_code or "not_active",
      notice = lookup_err and lookup_err.error or "graph run is already terminal",
    })
    return
  end

  local first_request = run.phase ~= "terminating"
  run_registry:mark_terminating(run_id, "terminate-graph")
  if first_request then
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
  end
  emit_tool_result_ok(firing_id, {
    canceled = true,
    run_id = run_id,
    status = "terminating",
    notice = first_request and "termination requested; awaiting canonical MAG terminal confirmation"
      or "termination was already requested; awaiting canonical MAG terminal confirmation",
    run = summarize_run(run),
  })
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
    emit_tool_result_err(firing_id, "graph-status: " .. tostring(context_error))
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
      "graph-status blocked for " .. target_label .. ": you called it less than " ..
      GRAPH_STATUS_COOLDOWN_SECONDS .. "s ago. " ..
      "Graph results arrive automatically — do not poll. " ..
      "Stop calling graph-status and wait for the result to arrive, or address the user.")
    return
  end
  if cooldown_count >= GRAPH_STATUS_COOLDOWN_LIMIT then
    state.graph_status_cooldowns[oldest_key] = nil
  end
  state.graph_status_cooldowns[target_key] = now

  if type(run_id) == "string" and run_id ~= "" then
    if authorized_run then
      emit_tool_result_ok(firing_id, { active = authorized_run.phase ~= "terminal", run = summarize_run(authorized_run) })
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

local function terminate_active_graph(session_id)
  invalidate_pending_mag_loads(nil)
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

local function lead_workflow_tool_schemas()
  return {
    {
      name        = "graph-status",
      display = { label = "Graph status", primary = { arg = "run_id" }, result = { kind = "content" } },
      description =
        "Report active graph runs, or one active/recent completed run " ..
        "when run_id is provided. One-shot snapshot for when you or the " ..
        "user need to know what's in flight — never call it in a " ..
        "polling loop; run outcomes are delivered to you when they land.",
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
      name        = "await-run",
      display = { label = "Await run", primary = { arg = "run_id" }, result = { kind = "content" } },
      description =
        "Block until a previously acknowledged detached MAG run reaches its canonical " ..
        "terminal result. The root lead may address same-session runs globally; a non-root " ..
        "agent may address only runs that exact actor directly dispatched. This attaches to " ..
        "the existing run and does not poll graph-status or cancel it. Use this when your next " ..
        "step depends on completion. A run reaches terminal state only when its processes exit; " ..
        "awaiting a persistent foreground server or watcher therefore waits indefinitely. Never " ..
        "launch such a process as a normal run and await it. Cancellation detaches only this " ..
        "waiter; use terminate-graph separately to stop the run.",
      parameters  = {
        type = "object",
        properties = {
          run_id = {
            type = "string",
            description = "Stable opaque run_id returned by mag execute or a lead mag-eval dispatch.",
          },
        },
        required = { "run_id" },
      },
    },
    {
      name        = "terminate-graph",
      display = { label = "Terminate graph", primary = { arg = "run_id" }, result = { kind = "content" } },
      description = "Request termination of exactly one active graph run by explicit run_id; the run remains terminating until canonical MAG confirmation.",
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
      display = { label = "Review plan", primary = { arg = "view" }, result = { kind = "content" } },
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
      display = { label = "MAG", primary = { arg = "file" }, arguments = { { label = "action", arg = "action" } }, result = { kind = "content" } },
      description =
        "Write, compile, and execute MAG programs on the actor kernel. " ..
        "Use action='write' to create/update a .mag file in the workspace. " ..
        "Use action='compile' (default) to compile and preview the actor " ..
        "modification. Use action='execute' to compile, validate, and run. " ..
        "Graph and agent semantics live in namespaced MAG libraries. " ..
        "A program requires nefor.actors, nefor.graph, nefor.contracts, and " ..
        "nefor.artifact; construct source, agent, and output nodes, connect " ..
        "them through one flat edge list in a Graph -> Graph function, then " ..
        "pass that function to nefor.artifact.compile. Compile applies it to " ..
        "empty-graph for a fresh run. Use the agent constructor shown " ..
        "by the injected canonical contract. " ..
        "Pass compiler-checked semantic type witnesses separately from runtime " ..
        "wire tags; use (type-tag nefor.contracts.Task), wire \"task\", and " ..
        "an output such as (type-tag nefor.contracts.FinalAnswer). Exactly one " ..
        "concrete output<T> identity node marks the result boundary. Graph " ..
        "operations are pure, retrieve no stored graph, and never mutate a live run. Agent loops are unbounded; " ..
        "stop early via interrupt/kill. The injected lib/patterns.md is the " ..
        "canonical complete example: use literal (require \"...\") forms and " ..
        "never copy historical session files or use removed import/bare-helper syntax. " ..
        "For a one-off shell expression whose result you just need back, " ..
        "use mag-eval instead — no file, no workspace ceremony.",
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
    mag_eval.schema,
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
-- mag: write a .mag file, or compile/execute it through the mag plugin. The
-- workspace path and library shapes are ambient (agentic-loop injects them
-- into the lead's system prompt each turn), so there is no discovery tool.

-- Compile/execute both run a synchronous load handshake against the mag
-- plugin: `mag.load` is sent, the `mag.loaded` reply carries the lowered
-- modification {actors, messages, kills, rules}, the hash, and the kernel
-- registry's factory names. resume_pending_load then either renders the
-- compile preview or validates and sends `mag.execute`. A `mag.error` reply
-- (compile failure) fails the firing with the compiler message
-- (fail_pending_load). Lifecycle events (mag.run_started, actor spawn/ready,
-- mag.run_complete) stream on the bus; the terminal mag.run_result (carrying
-- the sink's output PATH) closes the run and relays a fresh model turn in
-- receive_msg.
local function begin_mag_load(firing_id, action, args, ws, provenance)
  local graph_name = args.file:gsub("%.mag$", ""):gsub("/", "-"):sub(1, 20)
  local run_id = action == "execute" and run_registry:mint_run_id()
    or ("mag-load-" .. envelope.uuid_lite())
  local load_id = run_id .. "-load"

  state.pending_mag_load[load_id] = {
    action     = action,
    firing_id  = firing_id,
    file       = args.file,
    run_id     = run_id,
    run_name   = graph_name,
    session_id = provenance.session_id,
    dispatcher_id = provenance.dispatcher_id,
    conversation_id = provenance.conversation_id,
  }

  emit_as(SOURCE_NAME, "mag", {
    kind       = "mag.load",
    id         = load_id,
    source_dir = ws,
    module_roots = module_roots_for(ws),
    entry      = args.file,
  })
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

-- Validate and submit one loaded artifact through the standard active-run
-- channel. Both file-based `mag execute` and `mag-eval` use this
-- path, so lifecycle/control/rendering/archival/cleanup have one owner.
submit_loaded_run = function(pending, body, error_prefix)
  local artifact = body.artifact
  local modification = type(artifact) == "table" and artifact.data or nil
  if type(modification) ~= "table" then
    emit_tool_result_err(pending.firing_id,
      error_prefix .. ": mag.loaded reply carried no artifact data")
    return false
  end
  local actors = modification.actors or {}
  if not validate_factories(actors, pending.firing_id) then return false end
  if actors_have_writers(actors) and state.gate_mode == "safe"
      and not has_approved_plan() then
    emit_tool_result_err(pending.firing_id,
      "Program contains write-capable agents. Submit a plan via write-review " ..
      "and get approval before executing.")
    return false
  end
  local terminal_id, result_err = result_actor(modification)
  if not terminal_id then
    emit_tool_result_err(pending.firing_id, error_prefix .. ": " .. result_err)
    return false
  end
  local overlay, params_err = resolve_agent_params(actors)
  if not overlay then
    emit_tool_result_err(pending.firing_id, error_prefix .. ": " .. params_err)
    return false
  end
  local exec = {
    kind = "mag.execute",
    id = pending.run_id,
    run_id = pending.run_id,
    run_name = pending.run_name,
    session_id = pending.session_id,
    principal = "subagent",
    conversation_id = pending.conversation_id,
    artifact = artifact,
  }
  if next(overlay) ~= nil then exec.params_overlay = overlay end
  register_active_run(pending.run_id, actors, terminal_id,
    pending.firing_id, pending.run_name, pending.session_id, pending.dispatcher_id)
  emit_as(SOURCE_NAME, "mag", exec)
  emit_tool_result_ok(pending.firing_id, {
    status = "executing",
    run_id = pending.run_id,
    run_name = pending.run_name,
    hash = body.hash,
    engine = "mag-kernel",
    message = "Program submitted to the MAG actor kernel. Use await-run with this run_id when " ..
      "your next step depends on completion; do not poll graph-status. The normal completion " ..
      "notification remains independent.",
  })
  return true
end

-- Resume a pending compile/execute once its `mag.load` reply arrives. The
-- reply's registry has already refreshed state.kernel_factories
-- (capture_kernel_factories runs first). Compile renders the preview from the
-- modification. Execute validates — factories, write gate, sink, agent params —
-- and only then sends `mag.execute` with the resolved session_id, run_id, and
-- params overlay. Validation failure acks the firing with an error and drops
-- the pending entry — nothing runs.
--
-- Agent params resolved lead-side from the configured registry/defaults are
-- threaded to the actors via `params_overlay` on
-- mag.execute — a per-actor-id param patch the kernel merges before spawn
-- (actor params are kernel-opaque, so an overlay is legitimate control-plane
-- input, ir.md). The overlay keys on the namespaced LLM actor ids.
local function resume_pending_load(body)
  local load_id = body.in_reply_to
  local pending = type(load_id) == "string" and state.pending_mag_load[load_id] or nil
  if not pending then return end
  state.pending_mag_load[load_id] = nil

  local artifact = body.artifact
  local modification = type(artifact) == "table" and artifact.data or nil
  if type(modification) ~= "table" then
    emit_tool_result_err(pending.firing_id,
      "mag " .. pending.action .. ": mag.loaded reply carried no artifact data")
    return
  end
  local actors = modification.actors or {}

  if pending.action == "compile" then
    emit_tool_result_ok(pending.firing_id, {
      status  = "compiled",
      preview = mag.preview(modification, body.hash, body.factories),
      hash    = body.hash,
      message = "Program compiled successfully. Review the preview above. " ..
        "Call mag with action='execute' to run it.",
    })
    return
  end

  submit_loaded_run(pending, body, "mag execute")
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

local function mag_handler(firing_id, args, metadata)
  local provenance, provenance_error = resolve_invocation(metadata, "lead")
  if not provenance then
    emit_tool_result_err(firing_id, "mag: " .. provenance_error)
    return
  end
  local session_id = provenance.session_id

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

  if action ~= "compile" and action ~= "execute" then
    emit_tool_result_err(firing_id,
      "mag: unknown action '" .. tostring(action) ..
      "' (valid: write, compile, execute)")
    return
  end

  -- Compile and execute both go through the mag plugin's load handshake;
  -- the mag.loaded reply resolves them (resume_pending_load).
  -- File-based MAG execution is detached for every caller. Non-root authority
  -- is recorded against the kernel-stamped actor that made this invocation.
  begin_mag_load(firing_id, action, args, ws, provenance)
end

local TOOL_HANDLERS = {
  ["graph-status"]    = graph_status,
  ["await-run"]       = await_run,
  ["terminate-graph"] = terminate_graph,
  ["write-review"]    = submit_plan,
  ["submit-plan"]     = submit_plan,
  ["mag"]             = mag_handler,
  ["mag-eval"]        = mag_eval.handle,
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
  handler(firing_id, body.args or {}, {
    caller_id = body.caller_id,
    from = body.from,
    invocation = body.invocation,
  })
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

  if kind == "chat.workflows.terminate_requested" then
    if replay_window.active() or decoded.from ~= "nefor-tui" then return end
    mark_user_terminated_runs(body)
    return
  end

  -- Double-Esc: interrupt every live dispatched run. The lead is typically
  -- IDLE here (its `mag` execute dispatches are fire-and-forget), so the
  -- agentic-loop's own interrupt sees nothing — this is the entry point that
  -- reaches the detached runs the user is actually trying to stop. Live path
  -- only (nefor-tui emits it; replay never does).
  if kind == "chat.interrupt_all" then
    if replay_window.active() then return end
    clear_pending_plan(state.active_plan)
    interrupt_active_runs()
    return
  end

  -- Gate-forwarded cancel addresses the source's inner firing id. For evals it
  -- either removes a pending compile or matches a submitted standard run's
  -- dispatch_firing_id.
  if kind == "lead-workflow.tool.cancel" then
    if replay_window.active() then return end
    local plan = state.active_plan
    if type(plan) == "table" and (plan.pending_firing_id == body.id
        or plan.pending_correlation == body.id) then
      clear_pending_plan(plan)
    end
    if run_registry:detach_waiter(body.id) then return end
    mag_eval.cancel(body.id)
    invalidate_pending_mag_loads(body.id)
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

  -- Refresh the registry before either eval or file-load validation consumes a
  -- loaded artifact.
  if kind == "mag.loaded" then capture_kernel_factories(body) end

  -- mag-eval owns pending compile correlation. Submitted evals join the
  -- standard active-run path from its load hook.
  if mag_eval.on_bus(kind, body) then return end

  -- MAG kernel path.
  -- mag.hello advertises the factory registry at plugin startup — the startup
  -- validation snapshot.
  if kind == "mag.hello" then
    capture_kernel_factories(body)
    return
  end
  -- mag.loaded is the load-reply half of the synchronous handshake: refresh
  -- the registry, then resume the awaiting compile (preview) or execute
  -- (validate → mag.execute).
  if kind == "mag.loaded" then
    resume_pending_load(body)
    return
  end
  -- mag.error correlated to an in-flight load is a compile failure.
  if kind == "mag.error" then
    fail_pending_load(body)
    return
  end
  if kind == "mag.run_result" then
    handle_mag_run_result(body)
    return
  end
  -- Kernel actor lifecycle: keep the tracked run's node statuses at the same
  -- truth the chat surface's live panel tracks, so graph-status and the
  -- run-result block report done/killed rather than dispatch-time pending.
  if kind == "mag.run_started" then
    mark_mag_run_started(body)
    return
  end
  if kind == "mag.actor_spawned" then
    mark_mag_actor(body, "pending")
    return
  end
  if kind == "mag.actor_ready" then
    mark_mag_actor(body, "running")
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
    -- bookkeeping, not death: graph-status/result blocks report done. All
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

local M = {
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
    run_registry = run_registry,
    await_run = await_run,
    graph_status_cooldown_limit = GRAPH_STATUS_COOLDOWN_LIMIT,
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
      state.pending_mag_load = {}
      dependency_module_roots = {}
      agent_defaults = nil
      mag_eval._internals.reset()
      advertised = false
    end,
  },
}

function M.configure(opts)
  if opts == nil then opts = {} end
  if type(opts) ~= "table" then
    error("lead-workflow: configure options must be a table", 2)
  end
  local roots = opts.dependency_module_roots
  if roots == nil then roots = {} end
  validate_dependency_module_roots(roots)
  dependency_module_roots = copy_roots(roots)
  local defaults = opts.agent_defaults
  if defaults ~= nil then
    if type(defaults) ~= "table" then
      error("lead-workflow: agent_defaults must be a table", 2)
    end
    for _, field in ipairs({ "provider", "model", "reasoning_effort", "system" }) do
      if type(defaults[field]) ~= "string" or #defaults[field] == 0 then
        error("lead-workflow: agent_defaults." .. field ..
          " must be a non-empty string", 2)
      end
    end
    agent_defaults = {
      provider = defaults.provider,
      model = defaults.model,
      reasoning_effort = defaults.reasoning_effort,
      system = defaults.system,
    }
  else
    agent_defaults = nil
  end
  mag_eval.configure({
    dependency_module_roots = copy_roots(roots),
    resolve_invocation = resolve_invocation,
    mint_run_id = function() return run_registry:mint_run_id() end,
    submit_run = function(pending, body)
      return submit_loaded_run(pending, body, "mag-eval")
    end,
  })
end

-- Install the standard lead-run submitter even when the composition uses the
-- module defaults and never calls configure explicitly.
mag_eval.configure({
  resolve_invocation = resolve_invocation,
  mint_run_id = function() return run_registry:mint_run_id() end,
  submit_run = function(pending, body)
    return submit_loaded_run(pending, body, "mag-eval")
  end,
})

return M
