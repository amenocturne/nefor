-- What a node of a workflow means for approval, as declared by its factory.
--
-- A workflow is approved as a unit before it runs: whoever applies it folds
-- over the nodes the kernel would spawn and asks what each means under the
-- applying agent's approval policy (`libs.tool.approval`). The kernel does
-- not decide that, but each factory owns the answer for its own kind of
-- node, as plain data in its declaration's `approval` field. Every factory
-- must declare one; `validate` runs when a factory is registered, so a kind
-- of node with no stated meaning cannot exist.
--
--   { kind = "fixed", noun = "sink",
--     decisions = { ReadOnly = { "safe" }, Default = { "safe" } } }
--     The node means the same whatever its params: one decision per approval
--     policy, `{ result }` or `{ result, reason }`.
--
--   { kind = "capability", noun = "shell script", tool = "shell.script",
--     arguments = { "script" }, shown = "script",
--     runtime_bound = { script = { "unsure", "..." }, cwd = "ignored", timeout = "ignored" } }
--     The node calls a capability, and means what the tool of that name says
--     a call with the node's `arguments` params means. `shown` is the param
--     an error shows for the node. `runtime_bound` says, for EVERY declared
--     param, what it means when a dynamic workflow binds that param to a
--     value known only at run time: a decision, or "ignored" when the param
--     is no part of what the tool decides from.
--
--   { kind = "agent", noun = "agent", policy = "tool_approval_policy",
--     wider = { "denied", "..." } }
--     The node makes an agent's tool calls with the approval policy in the
--     `policy` param. Each of those calls is decided when it is made; the
--     node itself is safe unless that policy is wider than the applying
--     agent's, which is the `wider` decision.

local approval = require("libs.tool.approval")

local M = {}

local function decision_problem(value)
  if type(value) ~= "table" then return "must be { result } or { result, reason }" end
  return approval.check_decision(value[1], value[2])
end

local function known(fields, value, context)
  for key in pairs(value) do
    if not fields[key] then return context .. " has unknown field " .. tostring(key) end
  end
  return nil
end

-- nil when `decl.approval` is a complete declaration, otherwise what is wrong.
function M.validate(decl)
  local a = decl.approval
  if type(a) ~= "table" then
    return "declaration.approval is not defined; a factory must say what its nodes mean under every approval policy"
  end
  if type(a.noun) ~= "string" or a.noun == "" then return "declaration.approval.noun must name the kind of node" end
  if a.kind == "fixed" then
    local problem = known({ kind = true, noun = true, decisions = true }, a, "declaration.approval")
    if problem then return problem end
    if type(a.decisions) ~= "table" then return "declaration.approval.decisions must map every approval policy" end
    for policy in pairs(a.decisions) do
      if not approval.is_policy(policy) then
        return "declaration.approval.decisions names unknown approval policy " .. tostring(policy)
      end
    end
    for _, policy in ipairs(approval.POLICIES) do
      if a.decisions[policy] == nil then
        return "declaration.approval.decisions does not define the " .. policy .. " policy"
      end
      problem = decision_problem(a.decisions[policy])
      if problem then return "declaration.approval.decisions." .. policy .. ": " .. problem end
    end
    return nil
  end
  if a.kind == "capability" then
    local problem = known({ kind = true, noun = true, tool = true, arguments = true,
      shown = true, runtime_bound = true }, a, "declaration.approval")
    if problem then return problem end
    if type(a.tool) ~= "string" or a.tool == "" then return "declaration.approval.tool must name the capability's tool" end
    local params = decl.params or {}
    if type(a.arguments) ~= "table" or #a.arguments == 0 then
      return "declaration.approval.arguments must list the params the tool decides from"
    end
    for _, name in ipairs(a.arguments) do
      if params[name] == nil then return "declaration.approval.arguments names undeclared param " .. tostring(name) end
    end
    if params[a.shown] == nil then return "declaration.approval.shown must name a declared param" end
    if type(a.runtime_bound) ~= "table" then
      return "declaration.approval.runtime_bound must say what each param means when bound at run time"
    end
    for name in pairs(a.runtime_bound) do
      if params[name] == nil then return "declaration.approval.runtime_bound names undeclared param " .. tostring(name) end
    end
    for name in pairs(params) do
      local bound = a.runtime_bound[name]
      if bound == nil then
        return "declaration.approval.runtime_bound does not define param " .. tostring(name)
      end
      if bound ~= "ignored" then
        problem = decision_problem(bound)
        if problem then return "declaration.approval.runtime_bound." .. name .. ": " .. problem end
      end
    end
    return nil
  end
  if a.kind == "agent" then
    local problem = known({ kind = true, noun = true, policy = true, wider = true }, a, "declaration.approval")
    if problem then return problem end
    if (decl.params or {})[a.policy] == nil then return "declaration.approval.policy must name a declared param" end
    problem = decision_problem(a.wider)
    if problem then return "declaration.approval.wider: " .. problem end
    return nil
  end
  return "declaration.approval.kind must be fixed, capability or agent"
end

-- A node that changes nothing outside its run, whatever policy applies it.
function M.inert(noun)
  local decisions = {}
  for _, policy in ipairs(approval.POLICIES) do decisions[policy] = { approval.safe } end
  return { kind = "fixed", noun = noun, decisions = decisions }
end

return M
