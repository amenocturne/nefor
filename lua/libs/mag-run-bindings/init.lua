-- Authoritative MAG run bindings reconstructed from mag.run_started events.
-- Notice payloads are claims; consumers must match them against this registry.

local M = {}

local function nonempty(value)
  return type(value) == "string" and value ~= ""
end

local function valid_principal(value)
  return value == "lead" or value == "subagent"
end

local function same(a, b)
  return a.session_id == b.session_id
    and a.run_id == b.run_id
    and a.run_scope == b.run_scope
    and a.principal == b.principal
end

function M.new()
  local by_scope = {}
  local by_run = {}
  local poisoned_scopes = {}
  local poisoned_runs = {}
  local R = {}

  function R.bind(event, source)
    if source ~= "mag" or type(event) ~= "table" then return false end
    local binding = {
      session_id = event.session_id,
      run_id = event.run_id,
      run_scope = event.scope,
      principal = event.principal,
    }
    if not nonempty(binding.session_id)
        or not nonempty(binding.run_id)
        or not nonempty(binding.run_scope)
        or not valid_principal(binding.principal) then
      return false
    end
    local run_key = binding.session_id .. ":" .. binding.run_id
    if poisoned_scopes[binding.run_scope] or poisoned_runs[run_key] then return false end
    local scoped = by_scope[binding.run_scope]
    local run = by_run[run_key]
    if (scoped and not same(scoped, binding)) or (run and not same(run, binding)) then
      poisoned_scopes[binding.run_scope] = true
      poisoned_runs[run_key] = true
      by_scope[binding.run_scope] = nil
      by_run[run_key] = nil
      return false
    end
    -- Exact replay is idempotent. Contradictory duplicate bindings poison both keys.
    by_scope[binding.run_scope] = binding
    by_run[run_key] = binding
    return true
  end

  function R.validate(invocation, session_id)
    if type(invocation) ~= "table"
        or not nonempty(invocation.session_id)
        or not nonempty(invocation.run_id)
        or not nonempty(invocation.run_scope)
        or not nonempty(invocation.actor_id)
        or not nonempty(invocation.capability_id)
        or not valid_principal(invocation.principal)
        or (session_id ~= nil and invocation.session_id ~= session_id) then
      return false
    end
    if invocation.capability_id:sub(1, #invocation.run_scope + 1)
        ~= invocation.run_scope .. "/" then
      return false
    end
    local run_key = invocation.session_id .. ":" .. invocation.run_id
    if poisoned_scopes[invocation.run_scope] or poisoned_runs[run_key] then return false end
    local scoped = by_scope[invocation.run_scope]
    local run = by_run[run_key]
    return scoped ~= nil and run ~= nil
      and same(scoped, invocation) and same(run, invocation)
  end

  return R
end

return M
