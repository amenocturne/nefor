-- tests/lua/mag-kernel/flow_test.lua — factory-level unit tests for the
-- flow-control primitives: sink, human.
--
-- Driven from `engine/tests/starter_mag_kernel_test.rs` (same harness as
-- factory_test.lua): a bare Lua VM with a stub `nefor.log` and package.path
-- pointed at `plugins/mag/lua/mag-kernel/`.
--
-- These tests exercise the factories directly — construct an instance, feed it
-- activation messages, assert what it emits. No routing, no fold, no cycle
-- execution (a sibling owns routing; full-cycle e2e comes later).

local Registry = require("registry")
local sink     = require("factories.sink")
local human    = require("factories.human")

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

-- Capturing emit sink standing in for the kernel's outbound.
local function capture()
  local out = {}
  return out, function(message) out[#out + 1] = message end
end

local function find_kind(msgs, kind)
  for _, m in ipairs(msgs) do
    if m.kind == kind then return m end
  end
  return nil
end

local function count_kind(msgs, kind)
  local n = 0
  for _, m in ipairs(msgs) do
    if m.kind == kind then n = n + 1 end
  end
  return n
end

-- Build a single-input graph activation (routing.lua, the kernel⇄factory
-- contract): one delivered { from, tag, message } triple. The factories under
-- test are single-input, so this is the activation shape they see.
local function single(from, tag, message)
  return { shape = "single", messages = { { from = from, tag = tag, message = message } } }
end

-- ==================================================================
-- both factories declare well-formed contracts and register
-- ==================================================================

do
  local reg = Registry.new()
  for _, mod in ipairs({ sink, human }) do
    local decl, err = reg:register({ declaration = mod.declaration, construct = mod.construct })
    assert_true(decl ~= nil and err == nil,
      "factory " .. tostring(mod.declaration.name) .. " registers cleanly: " .. tostring(err))
  end

  -- sink is terminal: no outputs.
  assert_eq(#reg:declaration("sink").outputs, 0, "sink declares no outputs (terminal)")

  -- human declares a union approve/reject exit and the drain signal.
  local hd = reg:declaration("human")
  assert_eq(hd.outputs[1], "human.Approved", "human output 1")
  assert_eq(hd.outputs[2], "human.Rejected", "human output 2")
  assert_eq(hd.signals[1], "drain", "human declares the drain signal")
end

-- ==================================================================
-- sink: persists via the injected writer and emits run-complete
-- ==================================================================

do
  local msgs, emit = capture()

  -- The kernel-injected persistence seam, stubbed as a capturing writer. It
  -- travels in `deps`, distinct from the authored (empty) `params`.
  local persisted = {}
  local writer = function(final) persisted[#persisted + 1] = final end

  local inst = sink.construct("sink", {}, emit, { writer = writer })
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil and ready.from == "sink", "sink emits an id-signed ready")

  local done_completion = inst.deliver(single("up", "generic-provider.FinalAnswer",
    { kind = "generic-provider.FinalAnswer", text = "the final answer" }))
  assert_eq(done_completion.status, "ok", "synchronous sink returns a successful completion")

  assert_eq(#persisted, 1, "sink persisted the final output via the injected writer")
  assert_eq(persisted[1].text, "the final answer", "writer received the final output")

  local done = find_kind(msgs, "mag.RunComplete")
  assert_true(done ~= nil, "sink emits the run-complete signal")
  assert_eq(done.from, "sink", "run-complete is id-signed")
  assert_eq(done.persisted, true, "run-complete flags that the output was persisted")
  assert_eq(done.result.text, "the final answer", "run-complete carries the result")
end

-- ==================================================================
-- sink: deps.writer is injected through registry:construct — kernel-side
-- capabilities travel in `deps`, threaded past the authored params
-- ==================================================================

do
  local reg = Registry.new()
  reg:register({ declaration = sink.declaration, construct = sink.construct })

  local msgs, emit = capture()
  local persisted = {}
  local writer = function(final) persisted[#persisted + 1] = final end

  -- registry:construct(name, id, params, emit, deps) — params authored empty,
  -- the writer injected via deps.
  local inst, cerr = reg:construct("sink", "sink", {}, emit, { writer = writer })
  assert_true(inst ~= nil and cerr == nil, "sink constructs through the registry with deps")

  inst.deliver(single("up", "generic-provider.FinalAnswer",
    { kind = "generic-provider.FinalAnswer", text = "through the registry" }))

  assert_eq(#persisted, 1, "registry-threaded deps.writer received the final output")
  assert_eq(persisted[1].text, "through the registry",
    "the writer injected via registry:construct persisted the result")
  local done = find_kind(msgs, "mag.RunComplete")
  assert_eq(done.persisted, true, "run-complete flags persisted when the deps writer is wired")
end

-- ==================================================================
-- sink: without an injected writer, persistence is skipped and flagged
-- (a mis-wired kernel is observable, not silent)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = sink.construct("sink", {}, emit, {}) -- no writer in deps
  inst.deliver(single("up", "generic-provider.FinalAnswer",
    { kind = "generic-provider.FinalAnswer", text = "x" }))
  local done = find_kind(msgs, "mag.RunComplete")
  assert_true(done ~= nil, "sink still signals completion without a writer")
  assert_eq(done.persisted, false, "run-complete flags persisted=false when no writer wired")
end

-- ==================================================================
-- human: request out, reply in — approval round-trip
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = human.construct("gate", { prompt = "Approve the plan?" }, emit)
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil and ready.from == "gate", "human emits an id-signed ready")

  -- A subject arrives → an approval request goes out; completion is deferred.
  local pending = inst.deliver(single("up", "generic-provider.FinalAnswer", { text = "proposed plan" }))
  assert_eq(pending.status, "pending", "an async gate defers completion on the subject")
  local req = find_kind(msgs, "mag.ApprovalRequest")
  assert_true(req ~= nil, "human emits an approval request for the subject")
  assert_eq(req.from, "gate", "request is id-signed")
  assert_eq(req.correlation, "gate", "request carries a correlation handle")
  assert_eq(req.prompt, "Approve the plan?", "request carries the configured prompt")
  assert_eq(req.subject.text, "proposed plan", "request carries the subject")
  assert_true(find_kind(msgs, "human.Approved") == nil, "no output before the reply")

  -- The reply arrives (stubbed chat surface) as a second graph delivery tagged
  -- mag.ApprovalReply → the gate resolves to Approved and signals async success.
  local resolved = inst.deliver(single("chat", "mag.ApprovalReply", { approved = true, content = "plan ok" }))
  assert_true(resolved == nil, "the reply delivery returns nil — completion arrives via mag.complete")
  local approved = find_kind(msgs, "human.Approved")
  assert_true(approved ~= nil, "reply resolves the gate to an approved output")
  assert_eq(approved.from, "gate", "approved output is id-signed")
  assert_eq(approved.content, "plan ok", "approved output carries the human's content")
  assert_eq(approved.subject.text, "proposed plan", "approved output carries the subject")
  local complete = find_kind(msgs, "mag.complete")
  assert_true(complete ~= nil and complete.from == "gate",
    "the resolved gate signals async success with an id-signed mag.complete")
end

-- ==================================================================
-- human: a rejecting reply resolves to the human.Rejected exit
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = human.construct("gate", {}, emit)
  inst.deliver(single("up", "generic-provider.FinalAnswer", { text = "risky plan" }))
  inst.deliver(single("chat", "mag.ApprovalReply", { approved = false, reason = "too risky" }))
  local rejected = find_kind(msgs, "human.Rejected")
  assert_true(rejected ~= nil, "a non-approving reply takes the rejected exit")
  assert_eq(rejected.reason, "too risky", "rejected output carries the reason")
  assert_true(find_kind(msgs, "human.Approved") == nil, "no approved output on rejection")
  assert_true(find_kind(msgs, "mag.complete") ~= nil,
    "a resolved rejection still signals async completion (the gate finished its work)")
end

-- ==================================================================
-- human: a reply with no request outstanding is ignored (not an activation)
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = human.construct("gate", {}, emit)
  inst.deliver(single("chat", "mag.ApprovalReply", { approved = true, content = "stray" }))
  assert_true(find_kind(msgs, "human.Approved") == nil,
    "a stray reply with no pending request emits nothing")
end

-- ==================================================================
-- human: drain cancels an outstanding request, then completes
-- ==================================================================

do
  local msgs, emit = capture()
  local inst = human.construct("gate", {}, emit)
  inst.deliver(single("up", "generic-provider.FinalAnswer", { text = "awaiting" }))

  inst.handle_drain()
  local cancel = find_kind(msgs, "mag.ApprovalCancel")
  assert_true(cancel ~= nil, "drain cancels the outstanding approval request")
  assert_eq(cancel.correlation, "gate", "cancel carries the correlation handle")
  assert_true(find_kind(msgs, "mag.complete") ~= nil, "drain ends with a signed completion")

  -- After drain the pending request is gone: a late reply resolves nothing.
  local before = count_kind(msgs, "human.Approved")
  inst.deliver(single("chat", "mag.ApprovalReply", { approved = true, content = "late" }))
  assert_eq(count_kind(msgs, "human.Approved"), before,
    "a reply after drain-cancel produces no output")
end

print("mag-kernel flow_test: all assertions passed")
