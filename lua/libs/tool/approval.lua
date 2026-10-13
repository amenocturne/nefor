-- Approval vocabulary shared by tools, the workflow runner and the MAG kernel.
--
-- An approval policy says what an agent may do with its tools. Policies form
-- one sequence from the narrowest to the widest; this list is that order for
-- Lua, and it repeats the constructor order of `ToolApprovalPolicy` in
-- `mag/lib/nefor/contracts.mag` (tests/lua/tool/approval_test.lua checks that
-- the two agree). Compare policies only through this module.
--
-- A tool's decision function answers, for one call under one policy, with one
-- of three results: `safe` (run it), `unsure` (it could not be shown to be
-- safe) or `denied` (it must not run). What the gate does with a result under
-- a policy and a permission mode is the gate's table
-- (plugins/tool-gate/src/policy.rs), not this module's.
--
-- This module has no dependencies so the MAG kernel's VM can load it too.

local M = {}

M.ReadOnly = "ReadOnly"
M.Default = "Default"

-- Narrowest first.
M.POLICIES = { M.ReadOnly, M.Default }

local RANK = {}
for index, name in ipairs(M.POLICIES) do RANK[name] = index end

function M.is_policy(value) return RANK[value] ~= nil end

-- Position in the order, or nil for a value that is not a policy.
function M.rank(policy) return RANK[policy] end

-- -1, 0 or 1 as `left` is narrower than, equal to or wider than `right`.
function M.compare(left, right)
  local a, b = RANK[left], RANK[right]
  if a == nil or b == nil then
    error("approval.compare: unknown approval policy `" .. tostring(a == nil and left or right) .. "`", 2)
  end
  if a < b then return -1 end
  if a > b then return 1 end
  return 0
end

function M.wider(left, right) return M.compare(left, right) > 0 end

function M.min(left, right)
  if M.compare(left, right) <= 0 then return left end
  return right
end

-- The policy name of a lowered MAG `ToolApprovalPolicy` value
-- (`{ constructor = "ReadOnly" }`), or nil and a message.
function M.from_mag(value)
  local name = type(value) == "table" and value.constructor or nil
  if not M.is_policy(name) then
    return nil, "expected a ToolApprovalPolicy (" .. table.concat(M.POLICIES, " | ") .. ")"
  end
  return name
end

M.safe = "safe"
M.unsure = "unsure"
M.denied = "denied"

-- Least to most severe.
M.RESULTS = { M.safe, M.unsure, M.denied }

local SEVERITY = {}
for index, name in ipairs(M.RESULTS) do SEVERITY[name] = index end

function M.is_result(value) return SEVERITY[value] ~= nil end

-- The more severe of two results: denied dominates, then unsure, then safe.
function M.combine(left, right)
  local a, b = SEVERITY[left], SEVERITY[right]
  if a == nil or b == nil then
    error("approval.combine: unknown result `" .. tostring(a == nil and left or right) .. "`", 2)
  end
  if a >= b then return left end
  return right
end

-- nil when `result` (with `reason`) is a valid decision, otherwise what is
-- wrong with it. `unsure` and `denied` must say why.
function M.check_decision(result, reason)
  if result == nil then return "it gives no result" end
  if not M.is_result(result) then
    return "`" .. tostring(result) .. "` is not one of " .. table.concat(M.RESULTS, ", ")
  end
  if result ~= M.safe and (type(reason) ~= "string" or reason == "") then
    return "`" .. result .. "` needs a reason"
  end
  if reason ~= nil and type(reason) ~= "string" then return "the reason must be a string" end
  return nil
end

-- The text a tool that is always denied for a ReadOnly agent adds to its
-- description for such an agent.
function M.read_only_rejection(reason)
  return "Your approval policy is ReadOnly, so every call to this tool is rejected: " .. reason .. "."
end

return M
