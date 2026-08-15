-- tests/lua/mag-kernel/factory_test.lua — unit tests for the MAG kernel's
-- factory declaration format, registry, and contract validation.
--
-- Driven from `engine/tests/starter_mag_kernel_test.rs`, which installs a
-- minimal `nefor.log` surface (the mag host binding) and points package.path
-- at `plugins/mag/lua/mag-kernel/` so bare requires resolve (matching the real host).

local shape    = require("shape")
local Registry = require("registry")
local stub     = require("factories.stub")

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

-- ==================================================================
-- shape representation — classification and firing
-- ==================================================================

do
  assert_eq(shape.classify("stub.In"), "single", "string is a single shape")
  assert_eq(shape.classify({ "A", "B" }), "union", "list of tags is a union shape")
  assert_eq(shape.classify({ product = { "A", "B" } }), "product",
    "a `product` struct is a product shape")

  assert_eq(shape.firing("stub.In"), "per-message", "single fires per message")
  assert_eq(shape.firing({ "A", "B" }), "any", "union fires on any")
  assert_eq(shape.firing({ product = { "A", "B" } }), "all", "product fires on all")

  local kind, err = shape.classify({})
  assert_true(kind == nil and type(err) == "string",
    "empty table is not a well-formed shape")
end

-- ==================================================================
-- stub factory registers and constructs — id-signed output + ready
-- ==================================================================

do
  local reg = Registry.new()
  local decl, err = reg:register({ declaration = stub.declaration, construct = stub.construct })
  assert_true(decl ~= nil and err == nil, "stub factory registers cleanly")
  assert_true(reg:lookup("stub") ~= nil, "registered stub is looked up by name")
  assert_eq(reg:declaration("stub").outputs[1], "stub.Out",
    "declaration exposes its output tags as plain data")
  assert_eq(reg:declared_input("stub", "input"), "stub.In",
    "declared input shape is exposed for wiring checks")

  local msgs, emit = capture()
  local instance, cerr = reg:construct("stub", "docs-explorer.stub", { greeting = "hi" }, emit)
  assert_true(instance ~= nil and cerr == nil, "construct returns an instance")
  assert_eq(instance.id, "docs-explorer.stub", "instance carries its id")

  -- Ready confirmation, signed with the id.
  local ready = find_kind(msgs, "mag.ready")
  assert_true(ready ~= nil, "construction emits a ready confirmation")
  assert_eq(ready.from, "docs-explorer.stub", "ready is signed with the actor id")

  -- A delivered activation drives the declared output; all output is
  -- id-signed and the return value is the completion (routing.lua contract).
  local completion = instance.deliver({
    shape = "single",
    messages = { { from = "upstream", tag = "stub.In", message = "payload-1" } },
  })
  assert_eq(completion.status, "ok", "synchronous stub returns a successful completion")
  local out = find_kind(msgs, "stub.Out")
  assert_true(out ~= nil, "instance emits its declared output tag")
  assert_eq(out.from, "docs-explorer.stub", "output is signed with the actor id")
  assert_eq(out.payload, "payload-1", "output carries the delivered message")
  assert_eq(out.greeting, "hi", "output reflects the factory's own params")
end

-- ==================================================================
-- registry rejects a malformed declaration and a duplicate name
-- ==================================================================

do
  local reg = Registry.new()
  local ok, err = reg:register({ declaration = { name = "", inputs = {}, outputs = {} }, construct = function() end })
  assert_true(ok == nil and type(err) == "string", "empty factory name rejected")

  reg:register({ declaration = stub.declaration, construct = stub.construct })
  local dup, derr = reg:register({ declaration = stub.declaration, construct = stub.construct })
  assert_true(dup == nil and derr:match("already registered"),
    "a factory name is claimed once")
end

-- ==================================================================
-- unknown factory in a modification is a validation rejection
-- ==================================================================

do
  local reg = Registry.new()
  reg:register({ declaration = stub.declaration, construct = stub.construct })

  -- construct-side rejection
  local inst, cerr = reg:construct("no-such", "x", {}, function() end)
  assert_true(inst == nil and cerr:match("unknown factory"),
    "constructing an unknown factory is rejected")

  -- modification-side rejection
  local result = reg:validate_modification({
    actors = {
      { id = "a", factory = "ghost", params = {}, routes = {} },
    },
  })
  assert_eq(result.ok, false, "modification with unknown factory is rejected")
  assert_true(result.errors[1]:match("unknown factory"),
    "rejection names the unknown factory")
end

-- ==================================================================
-- routes key not among declared outputs is rejected
-- ==================================================================

do
  local reg = Registry.new()
  reg:register({ declaration = stub.declaration, construct = stub.construct })

  local bad = reg:validate_modification({
    actors = {
      { id = "a", factory = "stub", params = {}, routes = { ["stub.Nope"] = { { actor = "b", wire = "stub.Nope" } } } },
    },
  })
  assert_eq(bad.ok, false, "route key not in declared outputs is rejected")
  assert_true(bad.errors[1]:match("not a declared output"),
    "rejection explains the undeclared route key")

  local good = reg:validate_modification({
    actors = {
      -- routes to an id not present in this modification: destination compat
      -- is the fold's concern, so key-in-outputs alone must pass.
      { id = "a", factory = "stub", params = {}, routes = { ["stub.Out"] = { { actor = "downstream", wire = "stub.Out" } } } },
    },
  })
  assert_eq(good.ok, true, "declared output tag as a route key is accepted")
end

-- ==================================================================
-- declared-output-to-declared-input compatibility: single/union/product
-- ==================================================================

do
  -- shape.accepts is the primitive: one output tag on one edge vs an input shape.
  assert_true(shape.accepts("generic-provider.ProviderOut", "generic-provider.ProviderOut"),
    "single input accepts its exact tag")
  assert_true(not shape.accepts("generic-provider.ProviderOut", "other.Tag"),
    "single input rejects a different tag")

  local union_in = { "generic-tool.ToolCalls", "generic-provider.TextAnswer" }
  assert_true(shape.accepts(union_in, "generic-tool.ToolCalls"),
    "union input accepts either variant (a)")
  assert_true(shape.accepts(union_in, "generic-provider.TextAnswer"),
    "union input accepts either variant (b)")
  assert_true(not shape.accepts(union_in, "generic-provider.ProviderOut"),
    "union input rejects a non-variant tag")

  local product_in = { product = { "explore.Findings", "review.Notes" } }
  assert_true(shape.accepts(product_in, "explore.Findings"),
    "product input accepts a component tag (slot a)")
  assert_true(shape.accepts(product_in, "review.Notes"),
    "product input accepts a component tag (slot b)")
  assert_true(not shape.accepts(product_in, "mag.Unit"),
    "product input rejects a non-component tag")
end

-- ==================================================================
-- end-to-end wiring compatibility through validate_modification,
-- exercising single, union, and product destination inputs
-- ==================================================================

do
  local reg = Registry.new()

  -- A producer whose single output feeds three differently-shaped consumers.
  reg:register({
    declaration = {
      name = "producer",
      inputs = { input = "src.Start" },
      outputs = { "src.Out" },
    },
    construct = function(id) return { id = id } end,
  })
  reg:register({
    declaration = {
      name = "single-consumer",
      inputs = { input = "src.Out" },
      outputs = {},
    },
    construct = function(id) return { id = id } end,
  })
  reg:register({
    declaration = {
      name = "union-consumer",
      inputs = { input = { "src.Out", "other.Tag" } },
      outputs = {},
    },
    construct = function(id) return { id = id } end,
  })
  reg:register({
    declaration = {
      name = "product-consumer",
      inputs = { joined = { product = { "src.Out", "review.Notes" } } },
      outputs = {},
    },
    construct = function(id) return { id = id } end,
  })
  reg:register({
    declaration = {
      name = "wrong-consumer",
      inputs = { input = "unrelated.Tag" },
      outputs = {},
    },
    construct = function(id) return { id = id } end,
  })

  local compatible = reg:validate_modification({
    actors = {
      { id = "p", factory = "producer", params = {},
        routes = { ["src.Out"] = { { actor = "c-single", wire = "src.Out" }, { actor = "c-union", wire = "src.Out" }, { actor = "c-product", wire = "src.Out" } } } },
      { id = "c-single",  factory = "single-consumer",  params = {}, routes = {} },
      { id = "c-union",   factory = "union-consumer",   params = {}, routes = {} },
      { id = "c-product", factory = "product-consumer", params = {}, routes = {} },
    },
  })
  assert_eq(compatible.ok, true,
    "src.Out wires into single, union, and product inputs that all accept it")

  local incompatible = reg:validate_modification({
    actors = {
      { id = "p", factory = "producer", params = {},
        routes = { ["src.Out"] = { { actor = "c-wrong", wire = "src.Out" } } } },
      { id = "c-wrong", factory = "wrong-consumer", params = {}, routes = {} },
    },
  })
  assert_eq(incompatible.ok, false,
    "wiring a declared output into an input that declares no matching tag is rejected")
  assert_true(incompatible.errors[1]:match("no input of factory"),
    "rejection explains the wiring incompatibility")
end

print("mag-kernel factory_test: all assertions passed")
