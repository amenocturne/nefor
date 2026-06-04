-- tests/lua/lead-workflow/workflow_test.lua — focused tests for the
-- team lead-workflow actor.

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

nefor = nefor or {
  bus = {},
  json = {
    encode = function(_) return "{}" end,
    decode = function(_) return nil end,
  },
  engine = {
    now = function() return "2026-06-01T00:00:00.000Z" end,
    send = function(_) end,
  },
}

package.preload["core.envelope"] = function()
  return {
    emit = function(_, _) end,
    emit_as = function(_, _, _) end,
  }
end

package.preload["core.event"] = function()
  return {
    decode = function(_) return nil end,
  }
end

package.preload["core.history_replay"] = function()
  return {
    active = function() return false end,
  }
end

local lw = require("lead-workflow")

local function reset()
  if lw._internals and lw._internals.reset then lw._internals.reset() end
end

reset()

local rejection = lw.gate_against_unapproved_plan({
  {
    id = "mystery",
    role = "unknown-role",
    agent_args = { prompt = "do work" },
  },
})
assert_true(type(rejection) == "string", "unknown role is rejected")
assert_true(
  rejection:find("unknown role `unknown-role`", 1, true) ~= nil,
  "rejection names the unknown role"
)

print("lead_workflow_test: all assertions passed")
