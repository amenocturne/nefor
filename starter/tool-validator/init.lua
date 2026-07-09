-- starter/tool-validator/init.lua — single-decision tool permission validator.
--
-- Sits between tool-gate and the chat surface so popups only appear for
-- tool calls a human actually needs to see. Subscribes to
-- `chat.tool.permission_request` (tool-gate's --prompt output) and is the
-- ONLY consumer of that envelope; chat/update.lua now listens to
-- `chat.tool.popup_request` instead.
--
-- For every gated invocation, the validator emits exactly one of:
--   * `tool.permission_response { id, decision = "approve" }` — auto-pass
--   * `tool.permission_response { id, decision = "deny" }`    — auto-block
--   * `chat.tool.popup_request   { id, tool, args }`          — defer to user
--
-- ## Per-tool policies
--
-- `bash`: passes the command through `da` (https://github.com/amenocturne/da),
-- a bash-command classifier with explicit policy flags. da reads the
-- command on stdin and exits 0 / 1 / 2 for approve / defer / deny. We
-- bind a fixed policy stack matching upstream's CC hook example:
--
--   --read-only --macos-only --help-bypass
--   --git read,add,commit,restore-staged,tag,fetch,pull,push
--   --cargo local
--
-- `mag`: outer MAG dispatch is auto-approved by tool-gate. lead-workflow
-- validates write-capable programs before `mag.execute` and rejects execution
-- unless a write-review plan is approved.
--
-- `edit_file` / `write_file`: auto-approved only while lead-workflow
-- has an approved plan. Without approval these are denied instead of popped
-- up, so direct file mutation cannot bypass the plan gate.
--
-- Other tools: defer to the user (popup) unless the agent is read-only.
--
-- ## Read-only agents
--
-- When `read_only = true` rides through the permission_request, the
-- validator never shows a popup. bash goes through da with the strict
-- policy (git read-only); approve on exit 0, deny on anything else.
-- Non-bash tools are auto-approved (tool-gate already restricts them
-- to the role's allowlist). This means read-only agents run fully
-- autonomously with no user interaction.
--
-- ## Failure modes
--
-- `da` is installed by the plugin manager. If it is missing or cannot be
-- probed, fail loudly: the runtime is mis-installed and bash classification
-- must not silently degrade.

local envelope     = require("core.envelope")
local event        = require("core.event")
local replay_window = require("core.history_replay")

local emit = envelope.emit

local SOURCE_NAME = "tool-validator"
local gate_mode = "safe"

-- da policy stack. Mirrors the README example, minus --mkdir-cwd
-- (which is `--path`-bound; the agent's cwd is the engine's cwd, not
-- per-call, so the path scope is ambiguous and we'd false-defer on
-- legitimate mkdirs).
-- `dp jira issue` (Jira read) and `confluence` (wiki read) are the team's
-- read-only skill CLIs — auto-approve them so a jira/wiki query is frictionless
-- via bash. Writes (`dp jira create`, …) aren't matched, so they fall through
-- to the prompt. jq needs no rule; it is already a read-only binary.
local DA_ARGS = {
  "--read-only",
  "--macos-only",
  "--help-bypass",
  "--git",   "read,add,commit,restore-staged,tag,fetch,pull,push",
  "--cargo", "local",
  "--allow", "dp jira issue",
  "--allow", "confluence",
}

local DA_ARGS_STRICT_READONLY = {
  "--read-only",
  "--macos-only",
  "--help-bypass",
  "--git", "read",
  "--allow", "dp jira issue",
  "--allow", "confluence",
}

-- Resolved on first use. The cache holds the resolved cmd path
-- (e.g. /Users/x/.local/share/nefor/bin/da) when da is reachable.
-- nil => not probed yet.
local da_cmd = nil

-- Find da via two paths, in priority order:
--   1. <data_root>/bin/da — the private install `just install-nefor`
--      drops into ~/.local/share/nefor/bin/. Keeps da off the user's
--      PATH but reachable from the engine.
--   2. PATH lookup of bare `da` — fallback for users who installed it
--      themselves (e.g. `cargo install dabin`).
-- Either path is probed via `da --version`; whichever succeeds wins.
local function probe_da()
  if da_cmd ~= nil then return da_cmd end

  local function try(cmd)
    local r = nefor.process.run { cmd = cmd, args = { "--version" } }
    if type(r) == "table" and r.code == 0 then return cmd end
    return nil
  end

  local data_root = (nefor.fs and nefor.fs.data_root and nefor.fs.data_root()) or nil
  local private = data_root and (data_root .. "/bin/da") or nil
  if private and try(private) then
    da_cmd = private
    return da_cmd
  end

  if try("da") then
    da_cmd = "da"
    return da_cmd
  end

  error("tool-validator: `da` not found at " ..
        (private or "<data_root>/bin/da") .. " or on PATH; re-run " ..
        "`just install-nefor` to install it under the libexec dir.")
end

local function emit_response(id, decision, reason, args)
  local body = {
    kind     = "tool.permission_response",
    id       = id,
    decision = decision,
  }
  if type(reason) == "string" and #reason > 0 then
    body.reason = reason
  end
  if type(args) == "table" then
    body.args = args
  end
  emit(nil, body)
end

local function emit_popup(body)
  -- Forward verbatim to the chat surface under the new envelope kind.
  emit(nil, {
    kind = "chat.tool.popup_request",
    id   = body.id,
    tool = body.tool or body.name,
    args = body.args,
  })
end

-- Classify a bash command through da. Returns one of:
--   "approve" | "deny" | "defer"
-- `da` probe/spawn failure is a runtime install error and raises.
local function classify_bash(command, read_only)
  if type(command) ~= "string" or #command == 0 then return "defer" end
  local cmd = probe_da()
  local policy = read_only and DA_ARGS_STRICT_READONLY or DA_ARGS
  local r = nefor.process.run {
    cmd   = cmd,
    args  = policy,
    stdin = command,
  }
  if type(r) ~= "table" then
    error("tool-validator: `da` classifier returned a non-table result")
  end
  if r.code == 0 then return "approve" end
  if r.code == 2 then return "deny" end
  if read_only then return "deny" end
  return "defer"
end

local function has_approved_plan()
  local ok, lw = pcall(require, "lead-workflow")
  if not ok or type(lw) ~= "table" then return false end
  local internals = lw._internals
  local st = type(internals) == "table" and internals.state or nil
  local plan = type(st) == "table" and st.active_plan or nil
  return type(plan) == "table" and plan.status == "approved"
end

local function auto_denial_reason(tool)
  return "permission_denied[auto]: tool `" .. tostring(tool) .. "` requires human approval. " ..
         "Recovery: switch to /safe and approve the request manually, or revise the task to use read-only/auto-approved tools."
end

local function defer_or_deny(body)
  if gate_mode == "yolo" then
    emit_response(body.id, "approve")
  elseif gate_mode == "auto" then
    emit_response(body.id, "deny", auto_denial_reason(body.tool or body.name))
  else
    emit_popup(body)
  end
end

local function handle_permission_request(body)
  local id = body.id
  if type(id) ~= "string" or #id == 0 then return end
  local tool = body.tool or body.name
  if type(tool) ~= "string" or #tool == 0 then return end

  if gate_mode == "yolo" then
    emit_response(id, "approve")
    return
  end

  local args = body.args
  local is_ro = body.read_only == true

  if tool == "edit_file" or tool == "write_file" then
    if is_ro then
      emit_response(id, "deny", tool .. " is not available to read-only agents")
      return
    end
    if has_approved_plan() then
      emit_response(id, "approve")
    else
      emit_response(id, "deny", tool .. " requires an approved plan")
    end
    return
  end

  if tool == "bash" then
    local cmd = (type(args) == "table" and args.command) or nil
    local verdict = classify_bash(cmd, is_ro)
    if verdict == "approve" then
      emit_response(id, "approve")
      return
    end
    if verdict == "deny" or is_ro then
      emit_response(id, "deny")
      return
    end
    -- defer: fall through to popup (non-read-only agents only).
  elseif is_ro then
    emit_response(id, "approve")
    return
  end

  defer_or_deny(body)
end

local function receive_msg(entry)
  if entry.origin == "step" and entry.target ~= nil then return end
  -- Replay path: tool-gate doesn't re-emit permission_request envelopes
  -- on resume (the resolved permission_response is in the bus log), so
  -- the validator has nothing to do during replay. Guard anyway against
  -- a future replay shape change.
  if replay_window.active() then return end

  local evt = event.decode(entry)
  if evt == nil then return end
  local body = evt.body
  if evt.kind == "tool-gate.mode_changed" then
    local mode = body.mode
    if mode == "normal" then mode = "safe" end
    if mode == "safe" or mode == "auto" or mode == "yolo" then gate_mode = mode end
    return
  end
  if evt.kind ~= "chat.tool.permission_request" then return end
  handle_permission_request(body)
end

return {
  name        = SOURCE_NAME,
  receive_msg = receive_msg,
  send_msg    = function(_) end,

  _internals = {
    classify_bash             = classify_bash,
    handle_permission_request = handle_permission_request,
    set_mode = function(mode)
      if mode == "normal" then mode = "safe" end
      if mode == "safe" or mode == "auto" or mode == "yolo" then gate_mode = mode end
    end,
    get_mode = function() return gate_mode end,
    has_approved_plan = has_approved_plan,
    reset = function() da_cmd = nil; gate_mode = "safe" end,
  },
}
