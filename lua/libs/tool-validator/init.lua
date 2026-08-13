-- libs/tool-validator — single-decision tool permission validator (base).
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
-- `shell.script`: passes the command through `da` (https://github.com/amenocturne/da),
-- a shell-script classifier with explicit policy flags. da reads the
-- command on stdin and exits 0 / 1 / 2 for approve / defer / deny. We
-- bind a fixed policy stack matching upstream's CC hook example:
--
--   --read-only --macos-only --help-bypass
--   --git read,add,commit,restore-staged,tag,fetch,pull,push
--   --cargo local
--
-- `edit_file`: auto-approved for non-read-only agents. The lead prompt
-- still requires reading the target file before editing, but the tool
-- is not size-limited here; the lead sometimes needs to make broad
-- existing-file edits without paying graph overhead.
--
-- `write_file`: auto-approved only while lead-workflow has an approved
-- plan. Without approval it is denied instead of popped up, so direct
-- file creation/overwrite cannot bypass the plan gate.
--
-- Other tools: defer to the user (popup) unless the agent is read-only.
--
-- Agent capability is defined solely by the invocation allowlist. A request is
-- read-only exactly when every allowed tool belongs to the composition's
-- canonical read-only inventory; no separate wire flag participates.
--
-- ## Registration seam
--
-- `build{ read_only_tools, auto_approve_tools, shell_fastpaths,
-- process_fastpaths }` returns the actor spec. `read_only_tools` is the
-- canonical inventory supplied by the composition; the remaining seams are
-- config-owned policy:
--   * `auto_approve_tools` — tool names auto-approved unconditionally
--     (e.g. a typed read-only wrapper that is schema-limited to safe
--     actions). Checked after yolo, before the edit/write/shell/process rules.
--   * `shell_fastpaths` — list of `function(script, read_only) -> bool`.
--     If any returns true the shell script is approved before `da` is
--     consulted. The predicate encodes its own read_only gating.
--   * `process_fastpaths` — list of `function(argv, args, read_only) -> bool`.
--     Predicates inspect the original argument vector; process arguments are
--     never joined into shell text. Unknown process shapes fail closed.
-- A downstream config passes its policy (e.g. a `mirror-projects` read
-- fast-path) instead of forking this file.
--
-- ## Failure modes
--
-- `da` is installed by the plugin manager. If it is missing or cannot be
-- probed, fail loudly: the runtime is mis-installed and shell-script classification
-- must not silently degrade.

local envelope     = require("core.envelope")
local event        = require("core.event")
local replay_window = require("core.replay_window")

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

local function has_approved_plan()
  local ok, lw = pcall(require, "libs.lead-workflow")
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

-- build{ read_only_tools, auto_approve_tools, shell_fastpaths, process_fastpaths } -> actor spec.
-- State
-- (da_cmd cache, gate_mode) is per-build so instances don't share.
local function build(opts)
  opts = opts or {}

  local read_only_tools = {}
  for _, name in ipairs(opts.read_only_tools or {}) do
    read_only_tools[name] = true
  end
  local auto_approve = {}
  for _, name in ipairs(opts.auto_approve_tools or {}) do
    auto_approve[name] = true
  end
  local shell_fastpaths = opts.shell_fastpaths or {}
  local process_fastpaths = opts.process_fastpaths or {}

  local gate_mode = "safe"
  -- Resolved on first use. Holds the resolved cmd path
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

  -- Classify a shell script through da. Returns one of:
  --   "approve" | "deny" | "defer"
  -- Config `shell_fastpaths` predicates get first refusal so a config can
  -- approve narrow, self-limited commands (e.g. read-only typed-CLI
  -- subcommands) without permitting the wider `da` surface.
  -- `da` probe/spawn failure is a runtime install error and raises.
  local function classify_shell_script(command, read_only)
    if type(command) ~= "string" or #command == 0 then return "defer" end
    for _, pred in ipairs(shell_fastpaths) do
      if pred(command, read_only) then return "approve" end
    end
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

  local function valid_argv(argv)
    if type(argv) ~= "table" then return false end
    local count = 0
    for key, value in pairs(argv) do
      if type(key) ~= "number" or key < 1 or key % 1 ~= 0
          or type(value) ~= "string" then
        return false
      end
      count = count + 1
    end
    if count == 0 then return false end
    for index = 1, count do
      if argv[index] == nil then return false end
    end
    return true
  end

  local function classify_process(args, read_only)
    if type(args) ~= "table" or not valid_argv(args.argv) then return "deny" end
    if type(args.cwd) ~= "string" or #args.cwd == 0 then return "deny" end
    local timeout = args.timeout
    if type(timeout) ~= "table" or type(timeout.present) ~= "boolean"
        or type(timeout.milliseconds) ~= "number"
        or timeout.milliseconds % 1 ~= 0 then
      return "deny"
    end
    if timeout.present then
      if timeout.milliseconds < 1 then return "deny" end
    elseif timeout.milliseconds ~= 0 then
      return "deny"
    end
    for _, pred in ipairs(process_fastpaths) do
      if pred(args.argv, args, read_only) then return "approve" end
    end
    return "deny"
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

  local function validate_allowlist(allowlist, tool)
    if allowlist == nil then return true, false end
    if type(allowlist) ~= "table" then return false, false end

    local count = 0
    local contains_tool = false
    for key, name in pairs(allowlist) do
      if type(key) ~= "number" or key < 1 or key % 1 ~= 0 or type(name) ~= "string" then
        return false, false
      end
      count = count + 1
      if name == tool then contains_tool = true end
    end
    for index = 1, count do
      if allowlist[index] == nil then return false, false end
    end
    return contains_tool, true
  end

  local function allowlist_is_read_only(allowlist)
    if type(allowlist) ~= "table" then return false end
    for _, tool in ipairs(allowlist) do
      if read_only_tools[tool] ~= true then
        return false
      end
    end
    return true
  end

  local function handle_permission_request(body)
    local id = body.id
    if type(id) ~= "string" or #id == 0 then return end
    local tool = body.tool or body.name
    if type(tool) ~= "string" or #tool == 0 then return end

    local capability_allows = validate_allowlist(body.allowlist, tool)
    if not capability_allows then
      emit_response(id, "deny", "tool invocation has malformed capability data or excludes the requested tool")
      return
    end

    if gate_mode == "yolo" then
      emit_response(id, "approve")
      return
    end

    if auto_approve[tool] then
      emit_response(id, "approve")
      return
    end

    local args = body.args
    local is_ro = allowlist_is_read_only(body.allowlist)

    if tool == "edit_file" then
      if is_ro then
        emit_response(id, "deny", "edit_file is not available to read-only agents")
        return
      end
      emit_response(id, "approve")
      return
    end

    if tool == "write_file" then
      if is_ro then
        emit_response(id, "deny", "write_file is not available to read-only agents")
        return
      end
      if has_approved_plan() then
        emit_response(id, "approve")
      else
        emit_response(id, "deny", "write_file requires an approved plan")
      end
      return
    end

    if tool == "shell.script" then
      local script = (type(args) == "table" and args.script) or nil
      local verdict = classify_shell_script(script, is_ro)
      if verdict == "approve" then
        emit_response(id, "approve")
        return
      end
      -- A classifier denial describes risk, not the user's decision. Safe
      -- mode still presents it; auto mode denies because no human is present.
      defer_or_deny(body)
      return
    end

    if tool == "process.exec" then
      local verdict = classify_process(args, is_ro)
      if verdict == "approve" then
        emit_response(id, "approve")
      else
        defer_or_deny(body)
      end
      return
    elseif is_ro then
      emit_response(id, "approve")
      return
    end

    defer_or_deny(body)
  end

  local function set_mode(mode)
    if mode == "normal" then mode = "safe" end
    if mode == "safe" or mode == "auto" or mode == "yolo" then gate_mode = mode end
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
    if evt.kind == "sessions.session_end" then
      gate_mode = "safe"
      return
    end
    if evt.kind == "tool-gate.mode_changed" then
      set_mode(body.mode)
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
      classify_shell_script     = classify_shell_script,
      classify_process          = classify_process,
      handle_permission_request = handle_permission_request,
      set_mode                  = set_mode,
      get_mode                  = function() return gate_mode end,
      has_approved_plan         = has_approved_plan,
      reset = function() da_cmd = nil; gate_mode = "safe" end,
    },
  }
end

return {
  build = build,
}
