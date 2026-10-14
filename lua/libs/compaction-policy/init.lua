-- Threshold decisions and accepted-fact replay; callers own effects and scope selection.
local M = {}
local json_data = require("core.json_data")
local function finite(n)
  return type(n) == "number" and n == n and n > -math.huge and n < math.huge
end
local function null(value)
  return value == nil or (type(nefor) == "table" and type(nefor.json) == "table"
    and type(nefor.json.is_null) == "function" and nefor.json.is_null(value))
end
function M.validate(raw)
  if null(raw) then return nil, "compaction_policy must be explicit; use { kind = \"Disabled\" }" end
  if type(raw) ~= "table" or json_data.is_array(raw) then
    return nil, "compaction_policy must be a closed threshold object or { kind = \"Disabled\" }"
  end
  if raw.kind == "Disabled" then
    for key in pairs(raw) do
      if key ~= "kind" then return nil, "Disabled compaction_policy has unknown field " .. tostring(key) end
    end
    return { kind = "Disabled" }
  end
  for key in pairs(raw) do
    if key ~= "soft_percent" and key ~= "step_percent" and key ~= "hard_percent" then
      return nil, "compaction_policy has unknown field " .. tostring(key)
    end
  end
  if not finite(raw.soft_percent) or not finite(raw.step_percent) or not finite(raw.hard_percent)
      or raw.soft_percent <= 0 or raw.soft_percent >= raw.hard_percent
      or raw.hard_percent > 100 or raw.step_percent <= 0 then
    return nil, "compaction_policy requires finite 0 < soft_percent < hard_percent <= 100 and step_percent > 0"
  end
  return { kind = "Thresholds", soft_percent = raw.soft_percent,
    step_percent = raw.step_percent, hard_percent = raw.hard_percent }
end
-- JSON-facing settings remain raw; this copies only validated fields.
function M.raw(policy)
  if policy.kind == "Disabled" then return { kind = "Disabled" } end
  return { soft_percent = policy.soft_percent, step_percent = policy.step_percent,
    hard_percent = policy.hard_percent }
end
function M.lower(value)
  if type(value) ~= "table" or value.constructor == nil then return M.validate(value) end
  for key in pairs(value) do
    if key ~= "constructor" and key ~= "value" then return nil, "invalid CompactionPolicy field " .. tostring(key) end
  end
  if value.constructor == "Disabled" and null(value.value) then return M.validate({ kind = "Disabled" }) end
  if value.constructor == "Thresholds" and not null(value.value) then return M.validate(value.value) end
  return nil, "invalid CompactionPolicy variant"
end
local function integer(n)
  return finite(n) and n >= 0 and n == math.floor(n)
end
function M.preflight(policy, state, observation, regular_window)
  if policy.kind == "Disabled" then return { kind = "Allow" } end
  if not finite(regular_window) or regular_window <= 0 then
    return nil, "compaction_policy requires a positive regular_window"
  end
  if type(observation) ~= "table" or observation.kind ~= "completed_response"
      or not integer(observation.total_tokens) then return { kind = "Allow" } end
  for _, key in ipairs({ "input_tokens", "output_tokens" }) do
    if observation[key] ~= nil and not integer(observation[key]) then return { kind = "Allow" } end
  end
  local percent = 100 * (observation.total_tokens / regular_window)
  if percent >= policy.hard_percent then return { kind = "Compact", trigger = "hard" } end
  if percent < policy.soft_percent or state.outstanding_notice ~= nil then return { kind = "Allow" } end
  -- Remainder avoids overflowing the milestone index for tiny positive steps.
  local milestone = percent - math.fmod(percent - policy.soft_percent, policy.step_percent)
  if milestone >= policy.hard_percent or (state.notified_through ~= nil and milestone <= state.notified_through) then
    return { kind = "Allow" }
  end
  return { kind = "Notice", notice_data = { milestone_percent = milestone,
    measured_percent = percent, total_tokens = observation.total_tokens, regular_window = regular_window } }
end
local function equal(a, b)
  if type(a) ~= "table" or type(b) ~= "table" then return a == b end
  for k, v in pairs(a) do if not equal(v, b[k]) then return false end end
  for k in pairs(b) do if a[k] == nil then return false end end
  return true
end
local function compatible(notice, scope)
  return notice ~= nil and notice.provider == scope.provider and notice.model == scope.model
    and equal(notice.history_id, scope.history_id)
end
function M.self_eligibility(policy, state, capability)
  if capability.kind ~= "supported" then return nil, "compaction_unsupported" end
  local notice = (state.notices or {})[state.delivered_notice_id]
  if policy.kind == "Disabled" or notice == nil or notice.epoch_id ~= state.epoch_id
      or not compatible(notice, capability) then return nil, "compaction_notification_required" end
  return { kind = "Allowed" }
end
function M.reduce(state, fact)
  local out = json_data.copy(state)
  local kind = fact.kind
  if kind == "conversation_created" or kind == "context_compaction_completed" then
    return { epoch_id = kind == "conversation_created" and fact.event_id or fact.request_id, notices = {} }
  end
  if fact.epoch_id ~= out.epoch_id then return out end
  out.notices = out.notices or {}
  if kind == "context_notice_appended" then
    local notice = { notice_id = fact.notice_id, epoch_id = fact.epoch_id,
      milestone_percent = fact.milestone_percent, message_id = fact.message_id,
      provider = fact.provider, model = fact.model, history_id = fact.history_id, request_ids = {} }
    out.notices[fact.notice_id] = notice
    out.outstanding_notice = notice
  elseif kind == "context_notice_included" or kind == "context_notice_delivered" then
    local notice = out.notices[fact.notice_id]
    if notice == nil or not compatible(notice, fact) then return out end
    if kind == "context_notice_included" then
      local found = false
      for _, id in ipairs(notice.request_ids) do if id == fact.request_id then found = true end end
      if not found then notice.request_ids[#notice.request_ids + 1] = fact.request_id end
      out.notified_through = math.max(out.notified_through or 0, notice.milestone_percent)
    else
      local included = false
      for _, id in ipairs(notice.request_ids) do if id == fact.request_id then included = true end end
      if not included then return out end
      out.delivered_notice_id = fact.notice_id
      if out.outstanding_notice and out.outstanding_notice.notice_id == fact.notice_id then out.outstanding_notice = nil end
    end
  end
  return out
end
return M
