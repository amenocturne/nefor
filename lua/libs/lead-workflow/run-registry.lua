-- Pure ownership and waiter registry for detached lead MAG runs.
-- Bus emission, rendering, and kernel control remain in lead-workflow/init.lua.

local M = {}
M.__index = M

local copy = require("core.json_data").copy

local function remove_value(list, value)
  for index = #list, 1, -1 do
    if list[index] == value then
      table.remove(list, index)
      return
    end
  end
end

local function valid_handle(run_id)
  return type(run_id) == "string"
    and #run_id > 0
    and #run_id <= 256
    and run_id:match("^mag%-run%-%w[%w_-]*$") ~= nil
end

local function typed_error(code, message, run_id, status)
  return {
    error_code = code,
    error = "await-run[" .. code .. "]: " .. message,
    run_id = run_id,
    status = status,
  }
end

local function authority_error(code, message, run_id)
  return {
    error_code = code,
    error = "run-control[" .. code .. "]: " .. message,
    run_id = run_id,
    status = "denied",
  }
end

local function canonical_outcome(body, run)
  local status = body.status
  if status == "completed" then
    local output = copy(body)
    output.kind = nil
    output.in_reply_to = nil
    output.run_id = run.run_id
    output.run_name = output.run_name or run.run_name
    output.invocation_label = output.invocation_label or run.invocation_label
    output.status = "completed"
    return { output = output }
  end
  local code = status == "killed" and "await_run_killed" or "await_run_failed"
  local fallback = status == "killed" and "run was killed" or "run failed"
  local outcome = typed_error(code, tostring(body.error or fallback), run.run_id, status)
  outcome.run_name = run.run_name
  outcome.invocation_label = run.invocation_label
  outcome.terminal = copy(body)
  return outcome
end

function M.new(opts)
  opts = opts or {}
  return setmetatable({
    terminal_limit = opts.terminal_limit or 64,
    tombstone_limit = opts.tombstone_limit or 256,
    mint_id = assert(opts.mint_id, "run-registry: mint_id is required"),
    now = opts.now or function() return nil end,
    monotonic_ms = opts.monotonic_ms or function() return nil end,
    runs = {},
    active_runs = {},
    completed_runs = {},
    terminal_order = {},
    tombstones = {},
    tombstone_order = {},
    waiter_runs = {},
    dispatcher_runs = {},
    run_dispatchers = {},
  }, M)
end

function M:mint_run_id()
  local run_id = self.mint_id()
  assert(valid_handle(run_id), "run-registry: mint_id returned an invalid handle")
  assert(self.runs[run_id] == nil and self.tombstones[run_id] == nil,
    "run-registry: mint_id returned a duplicate handle")
  return run_id
end

function M:add_tombstone(run_id, session_id)
  if self.tombstones[run_id] == nil then
    self.tombstone_order[#self.tombstone_order + 1] = run_id
  end
  self.tombstones[run_id] = { run_id = run_id, session_id = session_id, phase = "expired" }
  while #self.tombstone_order > self.tombstone_limit do
    local displaced = table.remove(self.tombstone_order, 1)
    self.tombstones[displaced] = nil
  end
end

function M:expire(run)
  self.runs[run.run_id] = nil
  self.active_runs[run.run_id] = nil
  local dispatcher = self.run_dispatchers[run.run_id]
  if dispatcher then
    local owned = self.dispatcher_runs[dispatcher]
    if owned then
      owned[run.run_id] = nil
      if next(owned) == nil then self.dispatcher_runs[dispatcher] = nil end
    end
    self.run_dispatchers[run.run_id] = nil
  end
  remove_value(self.terminal_order, run.run_id)
  remove_value(self.completed_runs, run)
  self:add_tombstone(run.run_id, run.session_id)
end

function M:enforce_terminal_limit(session_id)
  local owned = {}
  for _, run_id in ipairs(self.terminal_order) do
    local run = self.runs[run_id]
    if run and run.phase == "terminal" and run.session_id == session_id then
      owned[#owned + 1] = run
    end
  end
  while #owned > self.terminal_limit do
    local displaced = table.remove(owned, 1)
    self:expire(displaced)
  end
end

function M:register(spec)
  assert(type(spec) == "table", "run-registry: register spec is required")
  assert(type(spec.session_id) == "string" and spec.session_id ~= "",
    "run-registry: session_id is required")
  local run_id = spec.run_id or self:mint_run_id()
  assert(valid_handle(run_id), "run-registry: invalid run_id")
  assert(self.runs[run_id] == nil and self.tombstones[run_id] == nil,
    "run-registry: duplicate run_id")
  local run = {
    run_id = run_id,
    run_name = spec.run_name,
    session_id = spec.session_id,
    phase = "queued",
    status = "queued",
    dispatched_at = self.now(),
    duration_started_ms = self.monotonic_ms(),
    updated_at = self.now(),
    terminal = spec.terminal,
    dispatch_firing_id = spec.dispatch_firing_id,
    dispatcher_id = spec.dispatcher_id,
    owner_resume = spec.owner_resume and copy(spec.owner_resume) or nil,
    nodes_order = spec.nodes_order or {},
    nodes = spec.nodes or {},
    waiters = {},
    canonical = nil,
    relay_delivered = false,
  }
  self.runs[run_id] = run
  self.active_runs[run_id] = run
  if spec.dispatcher_id ~= nil then
    assert(type(spec.dispatcher_id) == "string" and spec.dispatcher_id ~= "",
      "run-registry: dispatcher_id must be a non-empty string")
    local owned = self.dispatcher_runs[spec.dispatcher_id]
    if not owned then
      owned = {}
      self.dispatcher_runs[spec.dispatcher_id] = owned
    end
    owned[run_id] = true
    self.run_dispatchers[run_id] = spec.dispatcher_id
  end
  return run
end

function M:authorize(run_id, session_id, dispatcher_id)
  local run, err = self:lookup(run_id, session_id)
  if not run then return nil, err end
  if dispatcher_id ~= nil and self.run_dispatchers[run_id] ~= dispatcher_id then
    return nil, authority_error("run_control_unauthorized",
      "non-root agents may control only detached runs they directly dispatched", run_id)
  end
  return run, nil
end

function M:direct_runs(dispatcher_id)
  local out = {}
  for run_id in pairs(self.dispatcher_runs[dispatcher_id] or {}) do
    local run = self.runs[run_id]
    if run then out[#out + 1] = run end
  end
  return out
end

function M:get(run_id)
  return self.runs[run_id]
end

function M:mark_running(run_id)
  local run = self.active_runs[run_id]
  if not run or run.phase ~= "queued" then return false end
  run.phase = "running"
  run.status = "running"
  run.updated_at = self.now()
  return true
end

function M:mark_terminating(run_id, reason)
  local run = self.active_runs[run_id]
  if not run then return nil end
  if run.phase ~= "terminating" then
    run.phase = "terminating"
    run.status = "terminating"
    run.terminate_reason = reason
    run.updated_at = self.now()
  end
  return run
end

function M:lookup(run_id, session_id)
  if not valid_handle(run_id) then
    return nil, typed_error("await_run_malformed",
      "run_id must be a non-empty opaque MAG run handle", run_id)
  end
  local run = self.runs[run_id]
  if run then
    if run.session_id ~= session_id then
      return nil, typed_error("await_run_wrong_session",
        "the run belongs to a different session", run_id)
    end
    return run, nil
  end
  local tombstone = self.tombstones[run_id]
  if tombstone then
    if tombstone.session_id ~= session_id then
      return nil, typed_error("await_run_wrong_session",
        "the run belongs to a different session", run_id)
    end
    return nil, typed_error("await_run_expired",
      "the retained terminal outcome has expired; dispatch a new run", run_id, "expired")
  end
  return nil, typed_error("await_run_unknown",
    "no lead-dispatched run is known for this handle", run_id)
end

function M:await(run_id, session_id, firing_id, dispatcher_id)
  local run, err = self:authorize(run_id, session_id, dispatcher_id)
  if not run then return { immediate = err } end
  if run.phase == "terminal" then
    return { immediate = run.canonical }
  end
  run.waiters[firing_id] = true
  self.waiter_runs[firing_id] = run_id
  return { waiting = true, run = run }
end

function M:detach_waiter(firing_id)
  local run_id = self.waiter_runs[firing_id]
  if not run_id then return false end
  self.waiter_runs[firing_id] = nil
  local run = self.runs[run_id]
  if run then run.waiters[firing_id] = nil end
  return true
end

function M:settle(run_id, body)
  local run = self.active_runs[run_id]
  if not run then return nil, false end
  if body.status ~= "completed" and body.status ~= "failed" and body.status ~= "killed" then
    return nil, false
  end
  run.phase = "terminal"
  run.terminal_status = body.status
  run.status = body.status == "completed" and "completed" or "failed"
  run.updated_at = self.now()
  run.canonical_body = copy(body)
  run.canonical = canonical_outcome(body, run)
  run.result = nil
  run.output_path = nil
  run.error = nil
  local finished_ms = self.monotonic_ms()
  if type(run.duration_started_ms) == "number" and type(finished_ms) == "number" then
    run.duration_ms = math.max(0, finished_ms - run.duration_started_ms)
  end
  local waiters = {}
  for firing_id in pairs(run.waiters) do waiters[#waiters + 1] = firing_id end
  table.sort(waiters)
  for _, firing_id in ipairs(waiters) do self.waiter_runs[firing_id] = nil end
  run.waiters = {}
  self.active_runs[run_id] = nil
  self.completed_runs[#self.completed_runs + 1] = run
  self.terminal_order[#self.terminal_order + 1] = run_id
  self:enforce_terminal_limit(run.session_id)
  return { run = run, waiters = waiters, outcome = run.canonical }, true
end

function M:claim_delivery(run_id)
  local run = self.runs[run_id]
  if not run or run.phase ~= "terminal" or run.relay_delivered then return false end
  run.relay_delivered = true
  return true
end

function M:end_session(session_id)
  local run_ids = {}
  for run_id, run in pairs(self.runs) do
    if run.session_id == session_id then run_ids[#run_ids + 1] = run_id end
  end
  table.sort(run_ids)
  local active, waiter_settlements = {}, {}
  for _, run_id in ipairs(run_ids) do
    local run = self.runs[run_id]
    if run.phase ~= "terminal" then
      active[#active + 1] = run_id
      local outcome = typed_error("await_run_session_ended",
        "the owning session ended before the run reached a terminal result", run_id)
      local waiters = {}
      for firing_id in pairs(run.waiters) do waiters[#waiters + 1] = firing_id end
      table.sort(waiters)
      for _, firing_id in ipairs(waiters) do
        self.waiter_runs[firing_id] = nil
        waiter_settlements[#waiter_settlements + 1] = {
          firing_id = firing_id,
          outcome = outcome,
        }
      end
    end
    self:expire(run)
  end
  return { active_run_ids = active, waiter_settlements = waiter_settlements }
end

function M:reset()
  self.runs = {}
  self.active_runs = {}
  self.completed_runs = {}
  self.terminal_order = {}
  self.tombstones = {}
  self.tombstone_order = {}
  self.waiter_runs = {}
  self.dispatcher_runs = {}
  self.run_dispatchers = {}
end

M.valid_handle = valid_handle
M.typed_error = typed_error
M.authority_error = authority_error

return M
