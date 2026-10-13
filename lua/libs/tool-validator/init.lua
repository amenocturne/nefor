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
-- `write_file`: whole-file writes and exact replacements share one policy.
-- The tool is approved only while lead-workflow has an approved plan and is
-- denied to read-only agents. Without approval it is denied instead of popped
-- up, so neither form can bypass the plan gate.
--
-- Other tools: defer to the user (popup) unless the agent is read-only.
--
-- Agent capability is defined solely by the invocation allowlist. A request is
-- read-only exactly when every allowed tool belongs to the composition's
-- canonical read-only inventory; no separate wire flag participates.
--
-- ## Registration seam
--
-- `build{ shell_classifier, read_only_tools, auto_approve_tools, shell_fastpaths,
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
-- The composition supplies `shell_classifier`, normally resolved by
-- `nefor-pm.bin`. Missing or unusable classifiers deny the affected request
-- with an actionable reason so tool-gate never retains an unresolved prompt.

local envelope     = require("core.envelope")
local event        = require("core.event")
local replay_window = require("core.replay_window")

local approval     = require("libs.tool.approval")
local shell        = require("core.shell")

local emit = envelope.emit

local SOURCE_NAME = "tool-validator"

-- Classifier flags per approval policy. `da` reads the command on stdin and
-- answers with its exit status: 0 approves, 1 defers, 2 denies.
--
-- Default is the wide stack: it mirrors da's README example, minus
-- --mkdir-cwd (which is `--path`-bound; the agent's cwd is the engine's cwd,
-- not per-call, so the path scope is ambiguous and we'd false-defer on
-- legitimate mkdirs). ReadOnly is the strict read-only stack.
local DEFAULT_CLASSIFIER_ARGS = {
  [approval.ReadOnly] = {
    "--read-only",
    "--macos-only",
    "--help-bypass",
    "--git", "read",
  },
  [approval.Default] = {
    "--read-only",
    "--macos-only",
    "--help-bypass",
    "--git",   "read,add,commit,restore-staged,tag,fetch,pull,push",
    "--cargo", "local",
  },
}

local function copy_list(list)
  local out = {}
  for index, value in ipairs(list) do out[index] = value end
  return out
end

-- The classifier arguments for every policy. `opts.classifier_args[policy]`
-- replaces a policy's stack outright. `opts.model_flag` names the classifier's
-- opt-in model flag; when set it is added to the default Default stack only,
-- so the ReadOnly stack never consults the model.
local function classifier_args(opts)
  local configured = opts.classifier_args or {}
  if type(configured) ~= "table" then
    error("tool-validator: `classifier_args` must map an approval policy to a list of arguments", 0)
  end
  for policy in pairs(configured) do
    if not approval.is_policy(policy) then
      error("tool-validator: `classifier_args` names unknown approval policy `" .. tostring(policy) .. "`", 0)
    end
  end
  if opts.model_flag ~= nil and (type(opts.model_flag) ~= "string" or opts.model_flag == "") then
    error("tool-validator: `model_flag` must be a non-empty string", 0)
  end
  local by_policy = {}
  for _, policy in ipairs(approval.POLICIES) do
    local args = configured[policy]
    if args == nil then
      args = copy_list(DEFAULT_CLASSIFIER_ARGS[policy])
      if policy == approval.Default and opts.model_flag ~= nil then
        args[#args + 1] = opts.model_flag
      end
    else
      if type(args) ~= "table" then
        error("tool-validator: `classifier_args." .. policy .. "` must be a list of strings", 0)
      end
      for _, argument in ipairs(args) do
        if type(argument) ~= "string" then
          error("tool-validator: `classifier_args." .. policy .. "` must be a list of strings", 0)
        end
      end
      args = copy_list(args)
    end
    by_policy[policy] = args
  end
  return by_policy
end

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

-- build{ shell_classifier, read_only_tools, auto_approve_tools, shell_fastpaths, process_fastpaths } -> actor spec.
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
  local command_fastpaths = opts.command_fastpaths or {}
  local process_fastpaths = opts.process_fastpaths or {}

  if type(opts.classifier) ~= "string" or opts.classifier == "" then
    error("tool-validator: build requires a non-empty `classifier` executable path", 0)
  end

  local gate_mode = "safe"
  -- Resolved on first use. Holds the composition-provided package binary path
  -- (e.g. /Users/x/.local/share/nefor/plugins/da/bin/da) when da is reachable.
  -- nil => not probed yet.
  local da_cmd = nil

  -- Probe the composition-resolved dependency once. Package installation and
  -- builds happen during bootstrap, never while a permission is pending.
  local function probe_da()
    if da_cmd ~= nil then return da_cmd end
    local r = nefor.process.run { cmd = opts.classifier, args = { "--version" } }
    if type(r) ~= "table" or r.code ~= 0 then
      local detail = type(r) == "table" and (r.stderr or r.stdout) or nil
      error("configured shell classifier is unusable at " .. opts.classifier ..
        (detail and detail ~= "" and ": " .. detail or ""), 0)
    end
    da_cmd = opts.classifier
    return da_cmd
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

  local args_by_policy = classifier_args(opts)

  -- The seam to the classifier executable: run it on one command with one
  -- policy's arguments and return its exit status. Everything the validator
  -- knows about the classifier's command line is here and in `probe_da`.
  local function run_classifier(policy, command)
    local result = nefor.process.run {
      cmd   = probe_da(),
      args  = args_by_policy[policy],
      stdin = command,
    }
    if type(result) ~= "table" then
      error("the command classifier returned a non-table result", 0)
    end
    return result.code, result.stderr
  end

  -- What running `command` means for a caller with approval policy `policy`:
  -- `safe`, `unsure` or `denied`, with a reason for the last two. The exit
  -- contract is 0 = approve, 1 = defer, 2 = deny. A classifier that cannot be
  -- run, or that exits with anything else, denies with an actionable reason:
  -- a command nobody could classify must not pass as merely unsure.
  -- `command_fastpaths` predicates `function(command, policy) -> bool` get
  -- first refusal so a config can approve narrow, self-limited commands
  -- without widening a classifier stack.
  local function classify_command(policy, command)
    if not approval.is_policy(policy) then
      error("tool-validator.classify_command: unknown approval policy `" .. tostring(policy) .. "`", 2)
    end
    if type(command) ~= "string" or command == "" then
      return approval.unsure, "there is no command text to classify"
    end
    for _, pred in ipairs(command_fastpaths) do
      if pred(command, policy) then return approval.safe end
    end
    local ran, code, stderr = pcall(run_classifier, policy, command)
    if not ran then
      return approval.denied, "tool_classifier_unavailable: " .. tostring(code) ..
        ". Recovery: reinstall or repair the composition-managed command classifier, then retry."
    end
    if code == 0 then return approval.safe end
    if code == 1 then
      if policy == approval.ReadOnly then return approval.unsure, "not shown to be read-only" end
      return approval.unsure, "not shown to be safe"
    end
    if code == 2 then return approval.denied, "forbidden by the command classifier" end
    return approval.denied, "tool_classifier_unavailable: the command classifier failed with exit code " ..
      tostring(code) .. ((stderr or "") ~= "" and ": " .. stderr or "") ..
      ". Recovery: reinstall or repair the composition-managed command classifier, then retry."
  end

  -- The same question for a program run from an argument vector without a
  -- shell: the vector is written as the equivalent shell command, every
  -- element quoted, and classified as that command.
  local function classify_argv(policy, argv)
    local command, problem = shell.quote_argv(argv)
    if not command then return approval.denied, problem end
    return classify_command(policy, command)
  end

  -- Transitional mapping for the permission-request path below.
  local function classify_shell_script(command, read_only)
    local result, reason = classify_command(read_only and approval.ReadOnly or approval.Default, command)
    if reason and reason:find("tool_classifier_unavailable", 1, true) then error(reason, 0) end
    if result == approval.safe then return "approve" end
    if result == approval.denied then return "deny" end
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
      local classified, verdict = pcall(classify_shell_script, script, is_ro)
      if not classified then
        emit_response(id, "deny", tostring(verdict))
        return
      end
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

    classify_command = classify_command,
    classify_argv    = classify_argv,

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
