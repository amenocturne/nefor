local Registry = require("registry")
local create = require("factories.worktree-create")
local open = require("factories.worktree-open")

local function assert_eq(actual, expected, message)
  if actual ~= expected then
    error(string.format("%s: expected %s, got %s", message, tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(value, message)
  if not value then error(message, 2) end
end

local function capture()
  local messages = {}
  return messages, function(message) messages[#messages + 1] = message end
end

local function find(messages, kind)
  for _, message in ipairs(messages) do
    if message.kind == kind then return message end
  end
  return nil
end

local function unit()
  return {
    shape = "single",
    messages = {{ from = "mag.control", tag = "mag.Unit", message = { kind = "mag.Unit" } }},
  }
end

local function reply(ref, result, err)
  return { kind = "reply", ref = ref, result = result, error = err }
end

do
  local registry = Registry.new({ require_preview = false })
  assert_true(registry:register({ declaration = create.declaration, construct = create.construct }),
    "create registers")
  assert_true(registry:register({ declaration = open.declaration, construct = open.construct }),
    "open registers")
  assert_eq(create.declaration.name, "worktree-create", "create identity")
  assert_eq(open.declaration.name, "worktree-open", "open identity")
  assert_eq(create.declaration.outputs[1], "nefor.worktree.Ready", "create ready output")
  assert_eq(create.declaration.outputs[2], "nefor.worktree.Failed", "create failure output")
  assert_true(create.declaration.params.base ~= nil, "create requires base")
  assert_true(open.declaration.params.base == nil, "open has no base")
end

do
  local _, emit = capture()
  local missing, err = create.construct("wt", { repository="/repo", path="/wt", branch="topic" }, emit)
  assert_true(missing == nil and err:find("base", 1, true), "create rejects missing base")
end

do
  local messages, emit = capture()
  local instance = create.construct("wt", {
    repository="/repo", path="/wt", branch="topic", base="main",
  }, emit)
  assert_true(find(messages, "mag.ready") ~= nil, "create emits ready")

  local pending = instance.deliver(unit())
  assert_eq(pending.status, "pending", "create waits for capability")
  local invoke = find(messages, "capability.invoke")
  assert_eq(invoke.capability, "git_worktree_create", "create capability")
  assert_eq(invoke.request.args.path, "/wt", "create forwards path")
  assert_eq(invoke.request.args.base, "main", "create forwards base")

  local completion = instance.deliver(reply(invoke.ref, {
    ok=true,
    worktree={ repository="/repo", path="/wt", branch="topic", head="abc" },
  }))
  assert_eq(completion.status, "ok", "create completes")
  local ready = find(messages, "nefor.worktree.Ready")
  assert_eq(ready.from, "wt", "worktree output is signed")
  assert_eq(ready.value.path, "/wt", "worktree value carries path")
  assert_eq(ready.value.head, "abc", "worktree value carries head")
end

do
  local messages, emit = capture()
  local instance = open.construct("wt", {
    repository="/repo", path="/wt", branch="topic",
  }, emit)
  instance.deliver(unit())
  local invoke = find(messages, "capability.invoke")
  assert_eq(invoke.capability, "git_worktree_open", "open capability")
  assert_true(invoke.request.args.base == nil, "open forwards no base")

  local completion = instance.deliver(reply(invoke.ref, {
    ok=false,
    error={ operation="open", kind="branch-mismatch", message="wrong branch" },
  }))
  assert_eq(completion.status, "failed", "domain error fails completion")
  assert_eq(completion.failure, "nefor.worktree.Failed", "domain error uses declared wire")
  assert_eq(completion.value.kind, "branch-mismatch", "domain error preserves kind")
  assert_eq(completion.value.message, "wrong branch", "domain error preserves message")
end

do
  local messages, emit = capture()
  local instance = create.construct("wt", {
    repository="/repo", path="/wt", branch="topic", base="main",
  }, emit)
  instance.deliver(unit())
  local invoke = find(messages, "capability.invoke")
  instance.handle_kill()
  assert_true(instance.deliver(reply(invoke.ref, {
    ok=true,
    worktree={ repository="/repo", path="/wt", branch="topic", head="abc" },
  })) == nil, "reply after kill is voided")
  assert_true(find(messages, "nefor.worktree.Ready") == nil, "kill emits no worktree output")
end

print("mag-kernel worktree_test: all assertions passed")
