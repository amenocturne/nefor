-- starter/mag-kernel/registry.lua — factory declarations + the kernel-side
-- registry that composition validates against.
--
-- A factory is the trait layer (plugins/mag/docs/actor-model.md, Factories):
-- an abstract shape that becomes a concrete actor when MAG instantiates it.
-- It has two parts:
--
--   declaration — plain, readable data: name, params schema, the input shapes
--                 it accepts (firing-bearing), the output tags it produces,
--                 and the signals it handles. Reading the declaration is the
--                 whole contract; no handler is generated from it.
--   constructor — `fn(id, params, emit) -> instance`. The instance signs all
--                 output with its id and confirms creation with a ready
--                 message for that id (actor-model.md, Lifecycle).
--
-- No injected behavior: the declaration lists which signals a factory handles,
-- but nothing here wraps or synthesizes a handler. The stub factory's source
-- is the whole truth about what it does.
--
-- The registry is a single map from factory name to { declaration, construct }.
-- A modification naming a factory not in the registry is a validation
-- rejection (docs/ir.md, Application semantics: "every modification is
-- validated before applying").

local shape = require("shape")

local registry = {}
registry.__index = registry

-- ---- declaration validation -------------------------------------------------

-- Validate a factory declaration is well-formed plain data. Returns the
-- (normalized) declaration on success, or nil + message.
--
-- Declaration fields:
--   name    string                  factory name (registry key)
--   params  table (schema)          readable params schema; opaque to kernel
--   inputs  { <name> = <shape> }    named input ports, each a shape.* value
--   outputs { <tag>, ... }          fully-qualified output tags produced
--   signals { <signal>, ... }       signals the factory handles (declared only)
local function validate_declaration(decl)
  if type(decl) ~= "table" then
    return nil, "declaration must be a table"
  end
  if type(decl.name) ~= "string" or decl.name == "" then
    return nil, "declaration.name must be a non-empty string"
  end
  if decl.params ~= nil and type(decl.params) ~= "table" then
    return nil, "declaration.params must be a table (params schema)"
  end

  if type(decl.inputs) ~= "table" then
    return nil, "declaration.inputs must be a table of named input shapes"
  end
  for port, s in pairs(decl.inputs) do
    local kind, err = shape.classify(s)
    if not kind then
      return nil, string.format("input port %q: %s", tostring(port), err)
    end
  end

  if type(decl.outputs) ~= "table" then
    return nil, "declaration.outputs must be a list of output tags"
  end
  for _, tag in ipairs(decl.outputs) do
    if type(tag) ~= "string" or tag == "" then
      return nil, "declaration.outputs entries must be non-empty tag strings"
    end
  end

  if decl.signals ~= nil then
    if type(decl.signals) ~= "table" then
      return nil, "declaration.signals must be a list of signal names"
    end
    for _, sig in ipairs(decl.signals) do
      if type(sig) ~= "string" or sig == "" then
        return nil, "declaration.signals entries must be non-empty strings"
      end
    end
  end

  return decl
end

-- ---- construction -----------------------------------------------------------

function registry.new()
  return setmetatable({ factories = {} }, registry)
end

-- Register a factory: a declaration plus its constructor. Rejects a
-- malformed declaration or a duplicate name (monotone: a name is claimed once).
function registry:register(entry)
  if type(entry) ~= "table" then
    return nil, "register expects { declaration = {...}, construct = fn }"
  end
  local decl, err = validate_declaration(entry.declaration)
  if not decl then
    return nil, err
  end
  if type(entry.construct) ~= "function" then
    return nil, string.format("factory %q: construct must be a function", decl.name)
  end
  if self.factories[decl.name] then
    return nil, string.format("factory %q already registered", decl.name)
  end
  self.factories[decl.name] = { declaration = decl, construct = entry.construct }
  return decl
end

-- Look up a factory by name; nil if unknown.
function registry:lookup(name)
  return self.factories[name]
end

registry.declaration = function(self, name)
  local f = self.factories[name]
  return f and f.declaration or nil
end

-- The declared input shape of a factory's named port (nil if unknown),
-- exposed for wiring-compatibility checks against upstream output tags.
function registry:declared_input(name, port)
  local decl = self:declaration(name)
  if not decl then
    return nil
  end
  return decl.inputs[port]
end

-- Construct an instance via the named factory. `emit` is the kernel's outbound
-- sink — the actor's entire world (actor-model.md): ready and every signed
-- output leave through it. Rejects an unknown factory.
function registry:construct(name, id, params, emit)
  local f = self.factories[name]
  if not f then
    return nil, string.format("unknown factory %q", tostring(name))
  end
  return f.construct(id, params, emit)
end

-- ---- modification validation ------------------------------------------------

-- Validate the actor specs of a graph modification against declared contracts
-- (docs/ir.md). Two checks per actor spec { id, factory, params, routes }:
--
--   1. `factory` names a registered factory        (unknown -> rejection).
--   2. every `routes` key is a declared output tag  of that factory, and each
--      destination actor's declared input shape accepts that output tag
--      (wiring compatibility — the v1 "sniff inputs for output.tool_calls"
--      mode is gone; compatibility is a type fact over declared shapes).
--
-- Destination input compatibility only checks destinations that are actors in
-- the same modification (an id -> factory map built from `actors`); a
-- destination already live in the inventory is the kernel fold's concern.
--
-- Returns { ok = true } or { ok = false, errors = { <msg>, ... } }.
function registry:validate_modification(modification)
  local errors = {}
  local actors = (modification and modification.actors) or {}

  -- id -> factory name, for destination wiring checks.
  local factory_of = {}
  for _, spec in ipairs(actors) do
    if type(spec.id) == "string" then
      factory_of[spec.id] = spec.factory
    end
  end

  for _, spec in ipairs(actors) do
    local decl = self:declaration(spec.factory)
    if not decl then
      table.insert(errors, string.format(
        "actor %q: unknown factory %q", tostring(spec.id), tostring(spec.factory)))
    else
      local declared_output = {}
      for _, tag in ipairs(decl.outputs) do
        declared_output[tag] = true
      end

      for tag, dests in pairs(spec.routes or {}) do
        if not declared_output[tag] then
          table.insert(errors, string.format(
            "actor %q: route key %q is not a declared output of factory %q",
            tostring(spec.id), tostring(tag), spec.factory))
        else
          for _, dest_id in ipairs(dests) do
            local dest_factory = factory_of[dest_id]
            if dest_factory then
              local dest_decl = self:declaration(dest_factory)
              if not dest_decl then
                table.insert(errors, string.format(
                  "actor %q: destination %q uses unknown factory %q",
                  tostring(spec.id), tostring(dest_id), tostring(dest_factory)))
              else
                local accepted = false
                for _, in_shape in pairs(dest_decl.inputs) do
                  if shape.accepts(in_shape, tag) then
                    accepted = true
                    break
                  end
                end
                if not accepted then
                  table.insert(errors, string.format(
                    "wiring %q -%s-> %q: no input of factory %q accepts that tag",
                    tostring(spec.id), tag, tostring(dest_id), dest_factory))
                end
              end
            end
          end
        end
      end
    end
  end

  if #errors == 0 then
    return { ok = true }
  end
  return { ok = false, errors = errors }
end

return registry
