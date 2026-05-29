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
-- `dispatch-graph`: asks lead-workflow whether the args would be
-- auto-rejected (writer roles without an approved plan). On a sure
-- rejection we deny here so the user never sees a popup for an
-- invocation that's about to be turned down — without this the UX
-- would be "agent calls tool → popup → user approves → chat shows
-- rejection". The rejection reason rides through tool-gate's
-- permission_response.reason → tool.result.error so the agent learns
-- exactly what to do next.
--
-- `edit_file` / `write_file`: auto-approved only while lead-workflow
-- has an approved plan. `edit_file` receives the small-edit policy
-- below before it reaches basic-tools. Without approval these are
-- denied instead of popped up, so direct file mutation cannot bypass
-- the plan gate.
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
-- If `da` isn't on PATH, the bash path silently degrades to "defer" so
-- the user keeps full control. The error is logged once at startup so
-- the operator sees it, but runtime keeps working.
--
-- If `nefor.process.run` fails to spawn (e.g. da binary moved), same
-- thing: defer. Better to bother the user than to auto-approve an
-- unclassified command.

local envelope     = require("core.envelope")
local event        = require("core.event")
local replay_window = require("core.history_replay")

local emit = envelope.emit

local SOURCE_NAME = "tool-validator"

-- da policy stack. Mirrors the README example, minus --mkdir-cwd
-- (which is `--path`-bound; the agent's cwd is the engine's cwd, not
-- per-call, so the path scope is ambiguous and we'd false-defer on
-- legitimate mkdirs).
local DA_ARGS = {
  "--read-only",
  "--macos-only",
  "--help-bypass",
  "--git",   "read,add,commit,restore-staged,tag,fetch,pull,push",
  "--cargo", "local",
}

local DA_ARGS_STRICT_READONLY = {
  "--read-only",
  "--macos-only",
  "--help-bypass",
  "--git", "read",
}

-- Resolved on first use. The cache holds the resolved cmd path
-- (e.g. /Users/x/.local/share/nefor/bin/da) when da is reachable, or
-- false when the probe failed. nil => not probed yet.
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

  da_cmd = false
  nefor.log.warn("tool-validator: `da` not found at " ..
                 (private or "<data_root>/bin/da") .. " or on PATH; bash " ..
                 "invocations will defer to the user popup. Re-run " ..
                 "`just install-nefor` to install it under the libexec dir.")
  return da_cmd
end

local SMALL_EDIT_POLICY = {
  require_unique_match = true,
  max_changed_lines    = 40,
  max_bytes_delta      = 4096,
}

local function copy_table(t)
  local out = {}
  for k, v in pairs(t or {}) do out[k] = v end
  return out
end

local function with_policy(args, policy)
  local out = copy_table(args)
  out.policy = policy
  return out
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

-- Pre-execution gate check for `dispatch-graph`. Asks lead-workflow
-- whether the args would be auto-rejected; if so, returns the rich
-- rejection reason so the popup can be skipped. Returns:
--   nil          — args look fine, fall through to popup
--   string reason — auto-deny with this message
-- Tolerates lead-workflow not being loaded (returns nil) so the
-- validator stays useful even when the lead-workflow actor isn't
-- spawned (e.g. minimal test setups).
local function classify_dispatch_graph(args)
  local ok, lw = pcall(require, "lead-workflow")
  if not ok or type(lw) ~= "table" then return nil end
  local check = lw.gate_against_unapproved_plan
  if type(check) ~= "function" then return nil end
  local nodes = args and args.nodes
  if type(nodes) ~= "table" then return nil end
  local rejection = check(nodes)
  if type(rejection) == "string" and #rejection > 0 then return rejection end
  return nil
end

-- Classify a bash command through da. Returns one of:
--   "approve" | "deny" | "defer"
-- Spawn / unavailability is treated as defer.
local function classify_bash(command, read_only)
  if type(command) ~= "string" or #command == 0 then return "defer" end
  local cmd = probe_da()
  if not cmd then return "defer" end
  local policy = read_only and DA_ARGS_STRICT_READONLY or DA_ARGS
  local r = nefor.process.run {
    cmd   = cmd,
    args  = policy,
    stdin = command,
  }
  if type(r) ~= "table" then return "defer" end
  if r.code == 0 then return "approve" end
  if r.code == 2 then return "deny" end
  if read_only then return "deny" end
  return "defer"
end

local function handle_permission_request(body)
  local id = body.id
  if type(id) ~= "string" or #id == 0 then return end
  local tool = body.tool or body.name
  local args = body.args
  local is_ro = body.read_only == true

  if tool == "edit_file" or tool == "write_file" then
    if is_ro then
      emit_response(id, "deny", tool .. " is not available to read-only agents")
      return
    end
    if has_approved_plan() then
      if tool == "edit_file" then
        emit_response(id, "approve", nil, with_policy(args, SMALL_EDIT_POLICY))
      else
        emit_response(id, "approve")
      end
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
  elseif tool == "dispatch-graph" then
    local rejection = classify_dispatch_graph(args)
    if rejection ~= nil then
      emit_response(id, "deny", rejection)
      return
    end
    -- Auto-approve when every node uses a read-only role.
    local all_readonly = true
    local READ_ONLY_ROLES = { explorer = true, reviewer = true, critic = true, reflector = true }
    local nodes = type(args) == "table" and args.nodes or {}
    if type(nodes) == "table" then
      for _, n in ipairs(nodes) do
        if type(n) == "table" and not READ_ONLY_ROLES[n.role] then
          all_readonly = false
          break
        end
      end
    end
    if all_readonly then
      emit_response(id, "approve")
      return
    end
    -- write-capable roles: fall through to popup.
  end

  emit_popup(body)
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
  if evt.kind ~= "chat.tool.permission_request" then return end
  handle_permission_request(body)
end

return {
  name        = SOURCE_NAME,
  receive_msg = receive_msg,
  send_msg    = function(_) end,

  _internals = {
    classify_bash             = classify_bash,
    classify_dispatch_graph   = classify_dispatch_graph,
    handle_permission_request = handle_permission_request,
    reset = function() da_cmd = nil end,
  },
}
