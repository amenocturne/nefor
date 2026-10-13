-- libs/tool-validator — command classification and the "ask the user" seam.
--
-- Two things live here.
--
-- ## Command classification
--
-- `classify_command(policy, command)` says what running a shell command means
-- for a caller with approval policy `policy`: `safe`, `unsure` or `denied`
-- (`libs.tool.approval`), with a reason for the last two. It is the one owner
-- of that question. The tools that run commands (`shell.script`,
-- `process.exec`) and the workflow tool that inspects a workflow's command
-- nodes (`mag-apply`) all call it from their decision functions; the tool
-- gate turns the result into run / ask / deny.
--
-- The command goes through a classifier executable, normally `da`
-- (https://github.com/amenocturne/da). The contract this module is written
-- against: the classifier reads the command on stdin, takes policy flags as
-- arguments, and exits 0 to approve, 1 to defer, 2 to deny. Each approval
-- policy has its own flags:
--
--   ReadOnly  --read-only --macos-only --help-bypass --git read
--   Default   --read-only --macos-only --help-bypass
--             --git read,add,commit,restore-staged,tag,fetch,pull,push
--             --cargo local  [+ model_flag]
--
-- `classify_argv(policy, argv)` answers the same question for a program run
-- from an argument vector without a shell. The vector is written as the
-- equivalent POSIX command, every element single-quoted (`core.shell`), and
-- classified as that text, so a program and a script are judged by one rule
-- set. A vector that cannot be written that way (empty, or not strings) is
-- denied.
--
-- ## Asking the user
--
-- The gate asks with `chat.tool.permission_request` when its decision table
-- says so. The actor built here is the only consumer of that envelope and
-- forwards it to the chat surface as `chat.tool.popup_request { id, tool,
-- args, reason?, approval_policy? }`. It decides nothing: which calls are
-- asked about is the gate's table, and what a call means is its tool's
-- decision function. A composition that wants another way of asking replaces
-- this actor.
--
-- ## Build options
--
-- `build{ classifier, classifier_args, model_flag, command_fastpaths }`
-- returns the actor spec with `classify_command` and `classify_argv` on it:
--   * `classifier` — the classifier executable, normally resolved by
--     `nefor-pm.bin`. Required.
--   * `classifier_args` — `{ [policy] = { flags } }`, replacing that policy's
--     default flags above outright.
--   * `model_flag` — the name of the classifier's opt-in model flag. When set
--     it is appended to the default `Default` flags and to no other policy's.
--     Leave it unset for a classifier release that has no such flag.
--   * `command_fastpaths` — list of `function(command, policy) -> bool`. If
--     any returns true the command is safe before the classifier is asked.
--     A downstream config passes its narrow approvals here instead of forking
--     this file.
--
-- ## Failure modes
--
-- A missing or unusable classifier, or one that exits with a status outside
-- the contract, makes the command `denied` with an actionable reason, so a
-- command nobody could classify never passes as merely unsure.

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

-- build{ classifier, classifier_args, model_flag, command_fastpaths } -> actor spec.
-- State (the probed classifier path) is per-build so instances don't share.
local function build(opts)
  opts = opts or {}

  local command_fastpaths = opts.command_fastpaths or {}

  if type(opts.classifier) ~= "string" or opts.classifier == "" then
    error("tool-validator: build requires a non-empty `classifier` executable path", 0)
  end

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
      error("configured command classifier is unusable at " .. opts.classifier ..
        (detail and detail ~= "" and ": " .. detail or ""), 0)
    end
    da_cmd = opts.classifier
    return da_cmd
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

  -- The gate asks; the chat surface shows the question.
  local function handle_permission_request(body)
    local id = body.id
    if type(id) ~= "string" or #id == 0 then return end
    local tool = body.tool or body.name
    if type(tool) ~= "string" or #tool == 0 then return end
    emit(nil, {
      kind = "chat.tool.popup_request",
      id   = id,
      tool = tool,
      args = body.args,
      reason = body.reason,
      approval_policy = body.approval_policy,
    })
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

    classify_command = classify_command,
    classify_argv    = classify_argv,

    _internals = {
      handle_permission_request = handle_permission_request,
      reset = function() da_cmd = nil end,
    },
  }
end

return {
  build = build,
}
