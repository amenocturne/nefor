-- examples/nefor-agent/lead_role_test.lua — smoke tests for the lead-workflow role
-- loader. Driven from
-- `crates/nefor/tests/starter_lead_role_test.rs`.
--
-- The loader has no bus dependency — these tests just exercise that
-- prompts get read off disk and the exported tables are shaped right.

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ",
      tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function normalized(text)
  return (text:gsub("%s+", " "))
end

local function assert_contains(text, fragment, msg)
  assert_true(normalized(text):find(normalized(fragment), 1, true) ~= nil, msg or ("missing semantic contract: " .. fragment))
end

local function assert_excludes(text, fragment, msg)
  assert_true(normalized(text):find(normalized(fragment), 1, true) == nil, msg or ("unexpected domain-specific contract: " .. fragment))
end

local policy = require("libs.model-context-policy")
assert_eq(policy.item_limit, 32 * 1024, "canonical policy retains the agreed item limit")
assert_eq(policy.continuation_limit, 96 * 1024, "canonical policy retains the agreed aggregate limit")

-- Module loads without error.
local lead_role = require("libs.lead-workflow.role")

-- LEAD_SYSTEM_PROMPT is the complete lead file, read verbatim off disk.
assert_true(type(lead_role.LEAD_SYSTEM_PROMPT) == "string", "LEAD_SYSTEM_PROMPT is a string")
assert_true(#lead_role.LEAD_SYSTEM_PROMPT > 0, "LEAD_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.LEAD_SYSTEM_PROMPT:find("^%[lead%-workflow%.role: prompt"),
  "LEAD_SYSTEM_PROMPT is the real prompt, not a missing-file placeholder"
)
assert_true(
  lead_role.LEAD_SYSTEM_PROMPT:find("lead orchestrator", 1, true) ~= nil,
  "LEAD_SYSTEM_PROMPT carries the root lead role"
)

assert_true(
  lead_role.LEAD_SYSTEM_PROMPT:find("Bounded model context", 1, true)
    < lead_role.LEAD_SYSTEM_PROMPT:find("## Tools", 1, true),
  "generated model-context policy precedes lead tool descriptions"
)
assert_contains(lead_role.LEAD_SYSTEM_PROMPT, "32768 bytes per result", "lead policy renders canonical item limit")
assert_contains(lead_role.LEAD_SYSTEM_PROMPT, "98304 bytes combined", "lead policy renders canonical continuation limit")
assert_contains(lead_role.LEAD_SYSTEM_PROMPT, "zero-based half-open byte range", "lead policy documents range convention")
assert_contains(lead_role.LEAD_SYSTEM_PROMPT, "read_file(path=<canonical path>, offset=start, max_bytes=end-start)", "lead policy documents retrieval syntax")

-- WORKER_SYSTEM_PROMPT is the complete delegated-agent file, read verbatim.
-- It is a full standalone prompt and does not claim the root role.
assert_true(type(lead_role.WORKER_SYSTEM_PROMPT) == "string", "WORKER_SYSTEM_PROMPT is a string")
assert_true(#lead_role.WORKER_SYSTEM_PROMPT > 0, "WORKER_SYSTEM_PROMPT is non-empty")
assert_true(
  not lead_role.WORKER_SYSTEM_PROMPT:find("^%[lead%-workflow%.role: prompt"),
  "WORKER_SYSTEM_PROMPT is the real prompt, not a missing-file placeholder"
)
assert_true(
  not lead_role.WORKER_SYSTEM_PROMPT:find("lead orchestrator", 1, true),
  "WORKER_SYSTEM_PROMPT does not claim the root role"
)

-- Semantic orchestration contracts. These assertions intentionally check
-- durable behavioral clauses rather than snapshotting complete prompt prose.
local lead = lead_role.LEAD_SYSTEM_PROMPT
local worker = lead_role.WORKER_SYSTEM_PROMPT

assert_contains(lead, "complete user request is your scope", "lead owns the complete request")
assert_contains(lead, "final user-facing claim", "lead retains the final claim")
assert_contains(lead, "scope must be narrower than yours", "lead enforces recursive scope contraction")
assert_contains(lead, "problem context, goal, relevant inputs or paths, constraints, expected output, and success evidence", "lead requires complete assignments")
assert_contains(lead, "siblings so they can run concurrently", "lead exposes sibling concurrency")
assert_contains(lead, "wait for required inputs", "lead preserves dependencies")
assert_contains(lead, "review, verification, and applicable correction routes", "lead builds outcome-complete workflows")
assert_contains(lead, "results as evidence rather than authority", "lead integrates worker evidence")
assert_contains(lead, "Calibrate the final completion claim to the evidence", "lead calibrates completion")
assert_contains(lead, "one general worker for contextual operations", "lead avoids permanent worker identities")

assert_contains(worker, "caller defines the scope you own", "worker ownership is caller-relative")
assert_contains(worker, "contextual operations, not permanent agent identities", "worker operations are contextual")
assert_contains(worker, "scope must be narrower than yours", "worker delegation contracts recursively")
assert_contains(worker, "problem context, goal, relevant inputs or paths, constraints, expected output, and evidence of success", "worker requires complete assignments")
assert_contains(worker, "siblings so they can run concurrently", "worker dispatches independent siblings")
assert_contains(worker, "wait for required inputs", "worker preserves dependencies")
assert_contains(worker, "results as evidence rather than authority", "worker integrates child evidence")
assert_contains(worker, "partial or narrow check does not prove a broader outcome", "worker calibrates completion")

for _, excluded in ipairs({
  "Mirror", "knowledge base", "Obsidian", "agent-browser", "youtube", "/Users/skril",
}) do
  assert_excludes(lead, excluded, "lead prompt remains domain-neutral: " .. excluded)
  assert_excludes(worker, excluded, "worker prompt remains domain-neutral: " .. excluded)
end
