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
-- Other tools: always defer to the user (no per-tool policy yet). They
-- still get popped up; the validator is just the routing seam.
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

local json = nefor.json

local envelope     = require("core.envelope")
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

-- Resolved on first use. nil => not probed; true => available;
-- false => probe failed (no da on PATH). The probe is one fork+exec
-- with --version; cheap to defer to first call.
local da_available = nil

local function probe_da()
  if da_available ~= nil then return da_available end
  local r = nefor.process.run { cmd = "da", args = { "--version" } }
  da_available = (type(r) == "table" and r.code == 0) or false
  if not da_available then
    nefor.log.warn("tool-validator: `da` not on PATH; bash invocations " ..
                   "will defer to the user popup. Install via `cargo " ..
                   "install dabin` (or upstream's `just install`).")
  end
  return da_available
end

local function emit_response(id, decision)
  emit(nil, {
    kind     = "tool.permission_response",
    id       = id,
    decision = decision,
  })
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
-- Spawn / unavailability is treated as defer.
local function classify_bash(command)
  if type(command) ~= "string" or #command == 0 then return "defer" end
  if not probe_da() then return "defer" end
  local r = nefor.process.run {
    cmd   = "da",
    args  = DA_ARGS,
    stdin = command,
  }
  if type(r) ~= "table" then return "defer" end
  if r.code == 0 then return "approve" end
  if r.code == 2 then return "deny" end
  return "defer"
end

local function handle_permission_request(body)
  local id = body.id
  if type(id) ~= "string" or #id == 0 then return end
  local tool = body.tool or body.name
  local args = body.args

  if tool == "bash" then
    local cmd = (type(args) == "table" and args.command) or nil
    local verdict = classify_bash(cmd)
    if verdict == "approve" then
      emit_response(id, "approve")
      return
    end
    if verdict == "deny" then
      emit_response(id, "deny")
      return
    end
    -- defer: fall through to popup.
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

  local payload = entry.payload
  if type(payload) ~= "string" or payload == "" then return end
  local ok, decoded = pcall(json.decode, payload)
  if not ok or type(decoded) ~= "table" or type(decoded.body) ~= "table" then return end
  local body = decoded.body
  if body.kind ~= "chat.tool.permission_request" then return end
  handle_permission_request(body)
end

return {
  name        = SOURCE_NAME,
  receive_msg = receive_msg,
  send_msg    = function(_) end,

  _internals = {
    classify_bash             = classify_bash,
    handle_permission_request = handle_permission_request,
    reset = function() da_available = nil end,
  },
}
