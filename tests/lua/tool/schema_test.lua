-- Tool schema checking and argument validation. Driven by
-- engine/tests/tool_constructor_test.rs.

local schema = require("libs.tool.schema")
local json = nefor.json

local function eq(actual, expected, label)
  if actual ~= expected then
    error(string.format("%s:\n  expected %s\n  got      %s", label,
      tostring(expected), tostring(actual)), 2)
  end
end

local function checked(parameters)
  local ok, err = schema.check(parameters)
  assert(ok, "schema should be accepted: " .. tostring(err))
  return parameters
end

local function rejects(parameters, fragment)
  local ok, err = schema.check(parameters)
  assert(not ok, "schema should be rejected: " .. fragment)
  assert(err:find(fragment, 1, true), "rejection mentions `" .. fragment .. "`: " .. tostring(err))
end

-- Definition time -----------------------------------------------------------

rejects({ type = "string" }, "parameters.type must be \"object\"")
rejects({ type = "object", properties = { a = { type = "string", format = "uri" } } },
  "parameters.properties.a uses unsupported keyword `format`")
rejects({ type = "object", title = "x" }, "unsupported keyword `title`")
rejects({ type = "object", additionalProperties = true }, "additionalProperties may only be false")
rejects({ type = "object", additionalProperties = { type = "string" } }, "additionalProperties may only be false")
rejects({ type = "object", properties = { a = { description = "untyped" } } }, "must declare type, enum, or oneOf")
rejects({ type = "object", properties = { a = { type = "text" } } }, ".type must be one of")
rejects({ type = "object", properties = { a = { type = { "string", "null" } } } }, ".type must be one of")
rejects({ type = "object", required = { "missing" }, properties = {} }, "names undeclared argument `missing`")
rejects({ type = "object", required = { "a", "a" }, properties = { a = { type = "string" } } }, "lists `a` twice")
rejects({ type = "object", properties = { a = { type = "string", minimum = 1 } } }, "minimum requires type integer or number")
rejects({ type = "object", properties = { a = { type = "integer", minLength = 1 } } }, "minLength requires type string")
rejects({ type = "object", properties = { a = { type = "string", items = { type = "string" } } } }, "items requires type array")
rejects({ type = "object", properties = { a = { type = "array", minItems = -1 } } }, "minItems must be a non-negative integer")
rejects({ type = "object", properties = { a = { type = "integer", maximum = json.decode("null") } } }, "maximum must be a finite number")
rejects({ type = "object", properties = { a = { type = "integer", minimum = 5, maximum = 1 } } }, "minimum exceeds maximum")
rejects({ type = "object", properties = { a = { type = "string", enum = {} } } }, "enum must be a non-empty array")
rejects({ type = "object", properties = { a = { oneOf = {} } } }, "oneOf must be a non-empty array")
rejects({ type = "object", properties = { a = { oneOf = { { type = "string", const = "x" } } } } }, "oneOf[0] uses unsupported keyword `const`")
rejects({ type = "object", properties = { a = { type = "array", items = { { type = "string" } } } } }, ".items must be a schema object")
rejects({ type = "object", properties = { a = { type = "string", pattern = "(?=x)" } } }, ".pattern is not supported")
rejects({ type = "object", properties = json.decode("[]") }, "properties must be an object")

checked({ type = "object" })
checked({ type = "object", properties = {} })
checked(json.decode('{"type":"object","properties":{},"required":[]}'))
checked({ type = "object", properties = { a = { type = "string", description = "d", default = "x" } } })

-- Call time -----------------------------------------------------------------

local tool = checked({
  type = "object",
  properties = {
    file = { type = "string", pattern = "\\.(mag|magl)$", minLength = 1 },
    count = { type = "integer", minimum = 1, maximum = 10 },
    ratio = { type = "number", minimum = 0, maximum = 1 },
    flag = { type = "boolean" },
    scope = { type = "string", enum = { "auto", "subfolders" } },
    argv = { type = "array", items = { type = "string", minLength = 1 }, minItems = 1 },
    name = { oneOf = { { type = "string" }, { type = "array", items = { type = "string" } } } },
    timeout = {
      type = "object",
      properties = { present = { type = "boolean" }, milliseconds = { type = "integer", minimum = 0 } },
      required = { "present", "milliseconds" },
    },
  },
  required = { "file" },
})

local function problem(args) return schema.validate(tool, args) end

eq(problem({ file = "a.mag" }), nil, "minimal valid arguments")
eq(problem(json.decode('{"file":"a.magl","count":3,"ratio":0.5,"flag":true,"scope":"auto",'
  .. '"argv":["ls"],"name":["x","y"],"timeout":{"present":false,"milliseconds":0}}')), nil,
  "complete decoded arguments")
eq(problem({ file = "a.mag", count = 2.0 }), nil, "integral float is an integer")
eq(problem({ file = "a.mag", name = "skill" }), nil, "oneOf string alternative")

-- Closed objects, with or without additionalProperties.
eq(problem({ file = "a.mag", run_id = "r" }),
  "unknown argument `run_id`; accepted arguments: file, argv, count, flag, name, ratio, scope, timeout",
  "unknown root argument")
eq(problem({ file = "a.mag", timeout = { present = true, milliseconds = 1, unit = "s" } }),
  "unknown argument `timeout.unit`; accepted in `timeout`: present, milliseconds",
  "unknown nested argument")
local empty = checked({ type = "object", properties = {} })
eq(schema.validate(empty, { x = 1 }), "unknown argument `x`; this tool accepts no arguments",
  "a schema without properties accepts nothing")
eq(schema.validate(checked({ type = "object", additionalProperties = false,
  properties = { a = { type = "string" } } }), { b = "x" }),
  "unknown argument `b`; accepted arguments: a", "explicit additionalProperties false")

-- required
eq(problem({}), "missing required argument `file`", "missing required root argument")
eq(problem({ file = "a.mag", timeout = { present = true } }),
  "missing required argument `timeout.milliseconds`", "missing required nested argument")

-- type
eq(problem(json.decode("[]")), "arguments must be an object, got array", "array arguments")
eq(problem("x"), "arguments must be an object, got string", "string arguments")
eq(problem({ file = 7 }), "argument `file` must be a string, got integer", "string type")
eq(problem({ file = "a.mag", count = "3" }), "argument `count` must be an integer, got string", "integer type")
eq(problem({ file = "a.mag", count = 1.5 }), "argument `count` must be an integer, got number", "fractional integer")
eq(problem({ file = "a.mag", ratio = "x" }), "argument `ratio` must be a number, got string", "number type")
eq(problem({ file = "a.mag", flag = "yes" }), "argument `flag` must be a boolean, got string", "boolean type")
eq(problem({ file = "a.mag", argv = "ls" }), "argument `argv` must be an array, got string", "array type")
eq(problem({ file = "a.mag", timeout = json.decode("[]") }),
  "argument `timeout` must be an object, got array", "object type")
eq(problem(json.decode('{"file":"a.mag","count":null}')),
  "argument `count` must be an integer, got null", "JSON null is not an absent argument")

-- enum
eq(problem({ file = "a.mag", scope = "all" }),
  'argument `scope` must be one of "auto", "subfolders", got "all"', "enum mismatch")

-- oneOf
eq(problem({ file = "a.mag", name = 3 }),
  "argument `name` must be exactly one of: string or array of string, got integer", "oneOf mismatch")
eq(problem({ file = "a.mag", name = { 1 } }),
  "argument `name` must be exactly one of: string or array of string, got array", "oneOf item mismatch")

-- items / minItems
eq(problem({ file = "a.mag", argv = json.decode("[]") }),
  "argument `argv` must contain at least 1 item, got 0", "minItems")
eq(problem({ file = "a.mag", argv = { "ls", 3 } }),
  "argument `argv[1]` must be a string, got integer", "items type")
eq(problem({ file = "a.mag", argv = { "ls", "" } }),
  "argument `argv[1]` must be at least 1 character long", "items minLength")

-- minLength / pattern
eq(problem({ file = "" }), "argument `file` must be at least 1 character long", "minLength")
eq(problem({ file = "notes.txt" }), "argument `file` must match pattern `\\.(mag|magl)$`", "pattern")

-- minimum / maximum
eq(problem({ file = "a.mag", count = 0 }), "argument `count` must be at least 1, got 0", "minimum")
eq(problem({ file = "a.mag", count = 11 }), "argument `count` must be at most 10, got 11", "maximum")
eq(problem({ file = "a.mag", ratio = 1.5 }), "argument `ratio` must be at most 1, got 1.5", "number maximum")
eq(problem({ file = "a.mag", timeout = { present = true, milliseconds = -1 } }),
  "argument `timeout.milliseconds` must be at least 0, got -1", "nested minimum")

-- An empty Lua table satisfies an object or an array; JSON-marked arrays do not.
eq(problem({ file = "a.mag", argv = {} }),
  "argument `argv` must contain at least 1 item, got 0", "empty Lua table as array")
eq(schema.validate(empty, {}), nil, "empty Lua table as object")

return true
