-- libs/lead-workflow/init.lua — lead-workflow actor (mechanism).
--
-- Config-agnostic: derives every path from env/globals the engine sets
-- (NEFOR_CONFIG_DIR, NEFOR_DATA_DIR, NEFOR_REVIEW_HOOK), never from its
-- own file location, so it runs identically from the shared lua tree.
-- Downstream configs `require("libs.lead-workflow")`; the starter keeps a
-- one-line re-export shim at starter/lead-workflow/init.lua only because
-- starter/init.lua's spawn site still says `require("lead-workflow")`.
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

local mag            = require("mag")
local mag_eval      = require("libs.lead-workflow.mag-eval")
local sessions      = require("sessions")
local envelope      = require("core.envelope")
local replay_window = require("core.history_replay")

local emit_as = envelope.emit_as
local emit    = envelope.emit

local dependency_module_roots = {}

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

  -- Active graph runs keyed by run_id. Kept as a compact lead-facing
  -- view; agentic-loop remains the owner of actual pending graph relay.
  active_runs = {},
  completed_runs = {},
  completed_run_limit = 10,

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
local graph_status
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

-- Profile resolution over an artifact's actors. Any actor authoring
-- params.profile gets its resolved provider/model/reasoning_effort recorded
-- in the params overlay keyed by the actor id. The nefor.actors library places
-- the profile on its namespaced llm capability. Llm actors must
-- carry :profile or raw reasoning_effort so the lead always makes an explicit
-- reasoning-depth decision. Returns overlay or nil + error text.
local function resolve_profiles(actors)
  local overlay = {}
  local profiles
  local profiles_err
  for _, actor in ipairs(actors or {}) do
    local params = type(actor.params) == "table" and actor.params or {}
    local profile_name = params.profile
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
    elseif actor.foreign == "nefor.factory.llm" and not has_raw_effort then
      return nil, "llm actor '" .. tostring(actor.id) ..
        "' is missing required :profile. " ..
        "Set :profile in the MAG library wrapper that constructs this " ..
        "nefor.factory.llm actor."
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

  state.active_plan = {
    plan_id           = plan_id,
    content           = plan,
    submitted_at      = submitted_at,
    pending_firing_id = (not immediately_approved) and firing_id or nil,
    status            = immediately_approved and "approved" or "pending",
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
register_active_run = function(run_id, actors, terminal, firing_id, run_name)
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
  local ts = now_ms()
  state.active_runs[run_id] = {
    run_id = run_id,
    run_name = run_name,
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
  end
  state.active_runs[run_id] = nil
  if state.active_run_id == run_id then state.active_run_id = nil end
  archive_run(run)
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
  run.status = "running"
  run.updated_at = now_ms()
end

-- Track one kernel actor lifecycle transition against its run's node table —
-- the same truth the chat surface's live panel tracks
-- (starter/chat/run_panel.lua): spawned → pending, ready → running, killed →
-- killed. The event's run_id names the run. A mid-run spawn (`mag.apply`)
-- appends a node the dispatch-time modification didn't carry.
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
-- state. Mirrors the chat surface's run panel (starter/chat/run_panel.lua).
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
  if type(result.text) == "string" and #result.text > 0 then
    return result.text
  end
  -- The encode fallback excludes transcript_delta (factories/llm.lua): the
  -- conversation record is not part of the relayed answer.
  local bare = {}
  for k, v in pairs(result) do
    if k ~= "transcript_delta" then bare[k] = v end
  end
  local ok, encoded = pcall(json.encode, bare)
  if ok and type(encoded) == "string" then return encoded end
  return nil
end

-- Relay a kernel run's completion to the model as a fresh orchestrator turn
-- (agentic-loop's deferred-queue + flush → new user-role turn).
-- `format_deferred` frames the sink content. The output path stays on the
-- visible graph-result block instead of being duplicated in the model input.
local function relay_kernel_completion(run_id, ok, content, err)
  local al = require("agentic-loop")
  if type(al.relay_run_completion) ~= "function" then return end
  if ok then
    local content_available = type(content) == "string" and content:find("%S") ~= nil
    al.relay_run_completion({
      run_id            = run_id,
      status            = "success",
      content_available = content_available,
      output            = content_available and content or nil,
    })
  else
    al.relay_run_completion({
      run_id = run_id,
      status = "failed",
      error  = err,
    })
  end
end

-- Append the transcript run-result block (`chat.graph_result.append`) for a
-- closed kernel run. Carries status + run id/name + the sink's output
-- PATH; the output CONTENT is deliberately omitted — it arrives separately as
-- the relayed fresh turn (relay_kernel_completion), so duplicating it here
-- would double-render it. `run` is read after finish_run, whose node-status
-- finalisation is exactly what the block should show.
local function emit_mag_result_block(run, status, output_path, err)
  local block = {
    kind     = "chat.graph_result.append",
    run_id   = run.run_id,
    run_name = run.run_name,
    status   = status,
    nodes    = ordered_node_summaries(run),
  }
  -- Wall time from dispatch to terminal result — the cleanest duration
  -- source: the run object already stamps dispatched_at at
  -- register_active_run, so no cross-actor lookup into the sidebar's
  -- run-panel timestamps is needed.
  if type(run.dispatched_at) == "number" then
    block.duration_ms = now_ms() - run.dispatched_at
  end
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
-- state with the sink's output PATH, appends the visible run-result block,
-- then relays the completion to the model as a fresh turn (item 2 parity).
-- The relayed content is the sink's final result carried INLINE on the reply
-- (`body.result` — the model needs the answer, not a path); the persisted
-- output file is the fallback when the reply predates the inline result.
local function handle_mag_run_result(body)
  local run_id = body.run_id or body.in_reply_to
  if type(run_id) ~= "string" then return end
  local run = state.active_runs[run_id]
  if not run then return end
  -- "killed" closes a run the control plane terminated out from under us
  -- (a mag.kill_run this actor didn't issue — terminate-graph archives
  -- before the reply lands, so those never reach here). Surfaced like a
  -- failure so the lead learns its dispatched run died.
  if body.status == "failed" or body.status == "killed" then
    local err = body.error
      or (body.status == "killed" and "run killed" or "mag run failed")
    finish_run(run_id, "failed", nil, err)
    emit_mag_result_block(run, "failed", nil, err)
    relay_kernel_completion(run_id, false, nil, err)
    return
  end
  local results = {}
  if type(run.terminal) == "string" and body.output_path ~= nil then
    results[run.terminal] = { output = { output_path = body.output_path } }
  end
  finish_run(run_id, "completed", results, nil)
  emit_mag_result_block(run, "success", body.output_path, nil)
  local content = mag_result_text(body.result)
    or read_output_file(body.output_path)
  relay_kernel_completion(run_id, true, content, nil)
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
    emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id, terminate = true })
  end
  nefor.log.info("lead-workflow: interrupt_all — terminated active dispatched runs", {
    count = #ids,
  })
  return #ids
end

-- Completeness for the general cancel route: a `tool.cancel` addressed to a
-- `mag` execute DISPATCH firing (the correlation that acked "executing")
-- propagates into that run just like the interrupt_all entry point — and, being
-- a dispatched run, TERMINATES it (`terminate = true`) so the run ends failed
-- rather than letting its agent llm re-fire to a phantom success. The mag
-- execute firing is acked at dispatch, so the kernel rarely emits a cancel for
-- it — this covers the case where it does, matching mag-eval.cancel's shape for
-- blocking firings. Returns true when a matching run was found.
local function interrupt_run_by_dispatch_firing(firing_id)
  if type(firing_id) ~= "string" then return false end
  local hit = false
  for run_id, run in pairs(state.active_runs) do
    if run.dispatch_firing_id == firing_id then
      emit_as(SOURCE_NAME, "mag", { kind = "mag.interrupt_run", run_id = run_id, terminate = true })
      hit = true
    end
  end
  return hit
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

  -- Kernel kill machinery: the kernel reaps the run's live actors through
  -- the fold (kill handlers run — provider cancels fire) and settles the
  -- pending execute as `mag.run_result status:"killed"`. The run is
  -- archived here first, so that terminal reply finds no tracked run and
  -- drops (no double relay).
  emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
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

  -- Every tracked run is a kernel run (register_active_run fires only on
  -- the mag execute path), so session-end termination rides the kernel
  -- kill machinery: end_run reaps the constellation through the fold and
  -- the dying actors' provider-cancel envelopes reach the bus.
  for _, run_id in ipairs(run_ids) do
    emit_as(SOURCE_NAME, "mag", { kind = "mag.kill_run", run_id = run_id })
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
        "Write, compile, and execute MAG programs on the actor kernel. " ..
        "Use action='write' to create/update a .mag file in the workspace. " ..
        "Use action='compile' (default) to compile and preview the actor " ..
        "modification. Use action='execute' to compile, validate, and run. " ..
        "Graph and agent semantics live in namespaced MAG libraries. " ..
        "A program requires nefor.actors, nefor.graph, nefor.contracts, and " ..
        "nefor.artifact; construct an agent fragment, add an explicit initial " ..
        "message with nefor.graph.finish, then return " ..
        "(nefor.artifact.compile program). Agent config requires :id, " ..
        ":model, :profile, :provider, :system, :tools, and :da-policy. " ..
        "Pass compiler-checked semantic type witnesses separately from runtime " ..
        "wire tags; use (type-tag nefor.contracts.Task), wire \"task\", and " ..
        "an output such as (type-tag nefor.contracts.FinalAnswer). The result boundary is " ..
        "structural metadata, not a sink actor. Agent loops are unbounded; " ..
        "stop early via interrupt/kill. The injected lib/patterns.md is the " ..
        "canonical complete example: use literal (require \"...\") forms and " ..
        "never copy historical session files or use removed import/bare-helper syntax. " ..
        "For a one-off shell expression whose result you just need back, " ..
        "use mag-eval instead — no file, no workspace ceremony.\n\n" ..
        "When to dispatch a graph vs work directly: first identify what " ..
        "is on the critical path for your next decision vs what is " ..
        "self-contained sidecar work. Anything multi-file, multi-step, " ..
        "or long-horizon runs as a graph; keep only glances and " ..
        "single-file tweaks local. Write each agent's :system prompt " ..
        "self-contained — goal, relevant paths, constraints, expected " ..
        "output shape; agents do not see your conversation. Give " ..
        "parallel builders disjoint write sets. After dispatch, do not " ..
        "poll graph-status — run outcomes (completion, failure, " ..
        "interruption) are delivered to you by the runtime. Integrate " ..
        "results as they arrive and prepare the next dispatch; do not " ..
        "redo delegated work locally.",
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
local function begin_mag_load(firing_id, action, args, ws)
  local graph_name = args.file:gsub("%.mag$", ""):gsub("/", "-"):sub(1, 20)
  local run_id = "mag-" .. tostring(graph_name) .. "-" .. tostring(now_ms())
  local load_id = run_id .. "-load"

  state.pending_mag_load[load_id] = {
    action     = action,
    firing_id  = firing_id,
    file       = args.file,
    run_id     = run_id,
    run_name   = graph_name,
    session_id = sessions.current_id(),
  }

  emit_as(SOURCE_NAME, "mag", {
    kind       = "mag.load",
    id         = load_id,
    source_dir = ws,
    module_roots = module_roots_for(ws),
    entry      = args.file,
  })
end

-- Resume a pending compile/execute once its `mag.load` reply arrives. The
-- reply's registry has already refreshed state.kernel_factories
-- (capture_kernel_factories runs first). Compile renders the preview from the
-- modification. Execute validates — factories, write gate, sink, profiles —
-- and only then sends `mag.execute` with the resolved session_id, run_id, and
-- params overlay. Validation failure acks the firing with an error and drops
-- the pending entry — nothing runs.
--
-- Profile params resolved lead-side from the configured registry are threaded
-- to the actors via `params_overlay` on
-- mag.execute — a per-actor-id param patch the kernel merges before spawn
-- (actor params are kernel-opaque, so an overlay is legitimate control-plane
-- input, ir.md). The overlay keys on the ACTOR ids that author params.profile;
-- for the agent template those are its namespaced llm actors (eval_agent
-- lowers the agent's :profile onto them).
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

  -- Execute: validators over the modification's actors.
  if not validate_factories(actors, pending.firing_id) then return end

  -- Approval gate for write-capable programs (safe mode only; auto/yolo
  -- bypass — the tool-gate enforces runtime permissions there).
  if actors_have_writers(actors) and state.gate_mode == "safe"
      and not has_approved_plan() then
    emit_tool_result_err(pending.firing_id,
      "Program contains write-capable agents. Submit a plan via write-review " ..
      "and get approval before executing.")
    return
  end

  local terminal_id, result_err = result_actor(modification)
  if not terminal_id then
    emit_tool_result_err(pending.firing_id, "mag execute: " .. result_err)
    return
  end

  local overlay, profile_err = resolve_profiles(actors)
  if not overlay then
    emit_tool_result_err(pending.firing_id, "mag execute: " .. profile_err)
    return
  end

  -- The modification rides INLINE on the execute rather than relying on
  -- the plugin's resident program: concurrent loaders share the plugin
  -- (the turn spawner loads the lead's own turn-program through the same
  -- mag.load surface), so "execute whatever was loaded last" would race.
  local exec = {
    kind         = "mag.execute",
    id           = pending.run_id,
    run_id       = pending.run_id,
    run_name     = pending.run_name,
    session_id   = pending.session_id,
    artifact     = artifact,
  }
  if next(overlay) ~= nil then
    exec.params_overlay = overlay
  end
  emit_as(SOURCE_NAME, "mag", exec)

  register_active_run(pending.run_id, actors, terminal_id, pending.firing_id, pending.run_name)

  emit_tool_result_ok(pending.firing_id, {
    status  = "executing",
    run_id  = pending.run_id,
    hash    = body.hash,
    engine  = "mag-kernel",
    message = "Program submitted to the MAG actor kernel. Results arrive automatically when the run " ..
      "completes. STOP here — do not call any more tools until results arrive.",
  })
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

  if action ~= "compile" and action ~= "execute" then
    emit_tool_result_err(firing_id,
      "mag: unknown action '" .. tostring(action) ..
      "' (valid: write, compile, execute)")
    return
  end

  -- Compile and execute both go through the mag plugin's load handshake;
  -- the mag.loaded reply resolves them (resume_pending_load).
  begin_mag_load(firing_id, action, args, ws)
end

local TOOL_HANDLERS = {
  ["graph-status"]    = graph_status,
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

  -- Double-Esc: interrupt every live dispatched run. The lead is typically
  -- IDLE here (its `mag` execute dispatches are fire-and-forget), so the
  -- agentic-loop's own interrupt sees nothing — this is the entry point that
  -- reaches the detached runs the user is actually trying to stop. Live path
  -- only (nefor-tui emits it; replay never does).
  if kind == "chat.interrupt_all" then
    if replay_window.active() then return end
    interrupt_active_runs()
    mag_eval.interrupt_all_runs()
    return
  end

  -- The gate forwards a `tool.cancel` for one of our firings here when a
  -- graceful interrupt cancels it. Two firing shapes propagate down:
  --   * a mag-eval blocking firing → its dispatched sub-run (mag-eval.cancel)
  --   * a `mag` execute dispatch firing → its fire-and-forget run
  -- Both resolve to `mag.interrupt_run`, killing the run's in-flight bash. Live
  -- path only.
  if kind == "lead-workflow.tool.cancel" then
    if replay_window.active() then return end
    mag_eval.cancel(body.id)
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

  -- mag-eval's slice of the bus protocol (its own loads, runs, session-end
  -- cleanup). Consumes only envelopes correlated to a pending eval.
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
    capture_kernel_factories(body)
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
  nefor.bus.on_event("sessions.session_end", function(_entry)
    terminate_active_graph()
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
    reset = function()
      state.active_run_id = nil
      state.active_runs = {}
      state.completed_runs = {}
      state.active_plan = nil
      state.gate_mode = "safe"
      state.kernel_factories = {}
      state.pending_mag_load = {}
      dependency_module_roots = {}
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
  mag_eval.configure({ dependency_module_roots = copy_roots(roots) })
end

return M
