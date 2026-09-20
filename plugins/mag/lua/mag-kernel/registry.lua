-- plugins/mag/lua/mag-kernel/registry.lua — factory declarations + the kernel-side
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
--   constructor — `fn(id, params, emit, deps) -> instance`. The instance signs
--                 all output with its id and confirms creation with a ready
--                 message for that id (actor-model.md, Lifecycle). `params` is
--                 authored plain data; `deps` carries kernel-injected
--                 capabilities (e.g. the sink's output writer), kept distinct
--                 so MAG programs never author a runtime closure.
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
local type_node = require("type-node")
local plain_data = require("plain-data")

local registry = {}
registry.__index = registry

local function is_dense_list(value)
  if type(value) ~= "table" then return false end
  local count = 0
  for key in pairs(value) do
    if type(key) ~= "number" or key < 1 or key % 1 ~= 0 then return false end
    count = count + 1
  end
  for index = 1, count do
    if value[index] == nil then return false end
  end
  return true
end

local function compatible_output_type(expected, actual)
  return type_node.equal(expected, actual)
end

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
  if decl.template ~= nil then
    if type(decl.template) ~= "table" or not is_dense_list(decl.template.relocations) then
      return nil, "declaration.template must contain a dense relocations list"
    end
    for index, relocation in ipairs(decl.template.relocations) do
      if type(relocation) ~= "table" or not is_dense_list(relocation.path)
          or #relocation.path == 0
          or (relocation.shape ~= "actor_id" and relocation.shape ~= "actor_id_list") then
        return nil, string.format("declaration.template.relocations[%d] is malformed", index)
      end
      for _, part in ipairs(relocation.path) do
        if type(part) ~= "string" or part == "" then
          return nil, string.format("declaration.template.relocations[%d] path is malformed", index)
        end
      end
      for key in pairs(relocation) do
        if key ~= "path" and key ~= "shape" then
          return nil, string.format("declaration.template.relocations[%d] has unknown field %s", index, tostring(key))
        end
      end
    end
    if decl.template.parameter_equals ~= nil then
      if type(decl.template.parameter_equals) ~= "table" then
        return nil, "declaration.template.parameter_equals must be an object"
      end
      for key, value in pairs(decl.template.parameter_equals) do
        if type(key) ~= "string" or key == "" or not (decl.params or {})[key]
            or (type(value) ~= "string" and type(value) ~= "number"
              and type(value) ~= "boolean") then
          return nil, "declaration.template.parameter_equals must name declared scalar parameters"
        end
      end
    end
    for key in pairs(decl.template) do
      if key ~= "relocations" and key ~= "parameter_equals" then
        return nil, "declaration.template has unknown field " .. tostring(key)
      end
    end
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

  if decl.type_variables ~= nil then
    if type(decl.type_variables) ~= "table" then
      return nil, "declaration.type_variables must be a list of names"
    end
    local seen = {}
    for _, variable in ipairs(decl.type_variables) do
      if type(variable) ~= "string" or variable == "" then
        return nil, "declaration.type_variables entries must be non-empty strings"
      end
      if seen[variable] then
        return nil, string.format("duplicate type variable %q", variable)
      end
      seen[variable] = true
    end
  end

  if decl.semantic ~= nil then
    local variables=decl.type_variables or {}
    if type(decl.semantic)~="table" or type(decl.semantic.input)~="table" or
        type(decl.semantic.output)~="table" or type(decl.semantic.inputs)~="table" or
        type(decl.semantic.outputs)~="table" then
      return nil,"declaration.semantic must contain input/output schemes and endpoint pairs"
    end
    local ok,err=type_node.validate(decl.semantic.input,variables)
    if not ok then return nil,"semantic input scheme: "..err end
    ok,err=type_node.validate(decl.semantic.output,variables)
    if not ok then return nil,"semantic output scheme: "..err end
    local seen={}
    for index,endpoint in ipairs(decl.semantic.inputs) do
      if type(endpoint)~="table" or type(endpoint.wire)~="string" or seen[endpoint.wire] then
        return nil,"semantic inputs need unique {wire,type} entries"
      end
      seen[endpoint.wire]=true
      ok,err=type_node.validate(endpoint.type,variables)
      if not ok then return nil,string.format("semantic input %d: %s",index,err) end
    end
    seen={}
    for index,endpoint in ipairs(decl.semantic.outputs) do
      if type(endpoint)~="table" or type(endpoint.wire)~="string" or seen[endpoint.wire] then
        return nil,"semantic outputs need unique {wire,type} entries"
      end
      seen[endpoint.wire]=true
      ok,err=type_node.validate(endpoint.type,variables)
      if not ok then return nil,string.format("semantic output %d: %s",index,err) end
    end
    if decl.semantic.params ~= nil then
      if type(decl.semantic.params) ~= "table" then
        return nil, "semantic params must map parameter names to type schemes"
      end
      for name,scheme in pairs(decl.semantic.params) do
        if type(name) ~= "string" or not (decl.params or {})[name] then
          return nil, "semantic param names must identify declared factory params"
        end
        ok,err=type_node.validate(scheme,variables)
        if not ok then return nil,string.format("semantic param %s: %s",name,err) end
      end
    end

    local function wire_set(values)
      local set={}
      for _,wire in ipairs(values) do
        if set[wire] then return nil,"duplicate runtime wire "..tostring(wire) end
        set[wire]=true
      end
      return set
    end
    local runtime_inputs={}
    for _,input_shape in pairs(decl.inputs) do
      for _,wire in ipairs(shape.tags(input_shape)) do runtime_inputs[wire]=true end
    end
    local semantic_inputs={}; for _,endpoint in ipairs(decl.semantic.inputs) do semantic_inputs[endpoint.wire]=true end
    local runtime_outputs,wire_err=wire_set(decl.outputs)
    if not runtime_outputs then return nil,wire_err end
    local semantic_outputs={}; for _,endpoint in ipairs(decl.semantic.outputs) do semantic_outputs[endpoint.wire]=true end
    local function same_set(left,right)
      for wire in pairs(left) do if not right[wire] then return false end end
      for wire in pairs(right) do if not left[wire] then return false end end
      return true
    end
    if not same_set(runtime_inputs,semantic_inputs) then
      return nil,"semantic input wires must exactly match runtime input tags"
    end
    if not same_set(runtime_outputs,semantic_outputs) then
      return nil,"semantic output wires must exactly match runtime output tags"
    end
  elseif #(decl.type_variables or {})>0 then
    return nil,"generic declaration requires a semantic endpoint scheme"
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

  local owned, ownership_error = plain_data.owned(decl, "declaration")
  if not owned then return nil, ownership_error end
  return owned
end

-- ---- construction -----------------------------------------------------------

function registry.new()
  return setmetatable({
    factories = {}, identities = {},
  }, registry)
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
  local identity = decl.identity or ("nefor.factory." .. decl.name)
  if type(identity) ~= "string" or identity == "" or not identity:find("%.") then
    return nil, string.format("factory %q: identity must be a qualified symbol", decl.name)
  end
  if self.identities[identity] then
    return nil, string.format("factory identity %q already registered", identity)
  end
  decl.identity = identity
  self.factories[decl.name] = { declaration = decl, construct = entry.construct }
  self.identities[identity] = decl.name
  return decl
end

-- Look up a factory by name; nil if unknown.
local function factory_entry(self, name)
  return self.factories[name] or self.factories[self.identities[name]]
end

function registry:lookup(name)
  local found = factory_entry(self, name)
  if not found then return nil end
  return { declaration = plain_data.copy(found.declaration), construct = found.construct }
end

-- The registered factory names, sorted for a stable surface. This is the
-- control plane's validation source of truth (the lead validates reasoner /
-- factory types against it instead of a hand-synced allowlist).
function registry:names()
  local names = {}
  for name in pairs(self.factories) do
    names[#names + 1] = name
  end
  table.sort(names)
  return names
end

registry.declaration = function(self, name)
  local f = factory_entry(self, name)
  return f and plain_data.copy(f.declaration) or nil
end

-- Serializable immutable input for MAG libraries and their generic checker.
-- Constructors and every other runtime closure are deliberately absent. The
-- type scheme is concrete today, but its data shape admits explicit variables
-- once a factory needs specialization.
function registry:contracts(array_mt)
  local function array_copy(values)
    local copy = {}
    for i, value in ipairs(values or {}) do copy[i] = value end
    if array_mt ~= nil then setmetatable(copy, array_mt) end
    return copy
  end

  local out = array_copy()
  for _, name in ipairs(self:names()) do
    local decl = self.factories[name].declaration
    local input_tags = array_copy()
    local seen_input = {}
    for _, input_shape in pairs(decl.inputs) do
      for _, tag in ipairs(shape.tags(input_shape)) do
        if not seen_input[tag] then
          input_tags[#input_tags + 1] = tag
          seen_input[tag] = true
        end
      end
    end
    table.sort(input_tags)
    out[#out + 1] = {
      identity = decl.identity,
      implementation = decl.name,
      params = decl.params or {},
      template = decl.template,
      type_scheme = {
        variables = array_copy(decl.type_variables),
        inputs = decl.inputs,
        input_tags = input_tags,
        outputs = array_copy(decl.outputs),
        semantic = decl.semantic,
      },
      signals = array_copy(decl.signals),
    }
  end
  return out
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
-- output leave through it. `deps` (optional) carries kernel-injected
-- capabilities — plain data authored in `params`, runtime closures in `deps` —
-- threaded through untouched to the factory. Rejects an unknown factory.
function registry:construct(name, id, params, emit, deps)
  local f = factory_entry(self, name)
  if not f then
    return nil, string.format("unknown factory %q", tostring(name))
  end
  return f.construct(id, params, emit, deps)
end

-- ---- modification validation ------------------------------------------------

-- Validate capability actor specs against their registered factory contracts.
-- The compiler has already lowered fixed combinators into routes and ordered
-- transforms, and topology owns validation of those runtime relationships. The
-- registry therefore checks only factory identity, explicit specialization,
-- and each actor's concrete semantic input/output/parameter endpoints.
--
-- Returns { ok = true } or { ok = false, errors = { <msg>, ... } }.
function registry:validate_modification(modification)
  local errors = {}
  local actors = (modification and modification.actors) or {}
  local declarations = modification and modification.types or nil
  if declarations ~= nil then
    local host = nefor and nefor.semantic_type
    local ok, valid = pcall(host and host.validate_declarations, declarations)
    if not ok or valid ~= true then
      errors[#errors + 1] = "modification semantic declarations are invalid: " .. tostring(valid)
    end
    local function reference(value, label)
      local declared = type(value)=="table" and declarations[value.type_id]
      if type(value)~="table" or type(value.type)~="table" or type(value.type_id)~="string"
          or not declared or not type_node.equal(declared,value.type) then
        errors[#errors+1]=label..": semantic descriptor identity is absent or mismatched"
      end
    end
    for _,actor in ipairs(actors) do
      reference(actor.input,string.format("actor %q input",tostring(actor.id)))
      for index,output in ipairs(actor.outputs or {}) do reference(output,string.format("actor %q output %d",tostring(actor.id),index)) end
    end
    for index,message in ipairs(modification.messages or {}) do
      reference({type=message.semantic_type,type_id=message.semantic_type_id},string.format("message %d",index))
    end
  end
  for _,spec in ipairs(actors) do
    local decl=self:declaration(spec.factory)
    if not decl then
      errors[#errors+1]=string.format("actor %q: unknown factory %q",tostring(spec.id),tostring(spec.factory))
    else
      local variables=decl.type_variables or {}
      if not is_dense_list(spec.type_arguments) then
        errors[#errors+1]=string.format("actor %q: type_arguments must be a dense list",tostring(spec.id))
      elseif #spec.type_arguments~=#variables then
        errors[#errors+1]=string.format("actor %q: factory %q expects %d type argument(s), got %d",tostring(spec.id),spec.factory,#variables,#spec.type_arguments)
      else
        local bindings={}
        for index,variable in ipairs(variables) do
          bindings[variable]=spec.type_arguments[index]
          local ok,err=type_node.validate(spec.type_arguments[index])
          if not ok then errors[#errors+1]=string.format("actor %q: type argument %d: %s",tostring(spec.id),index,err) end
        end
        if decl.semantic then
          local expected_input
          for _,input in ipairs(decl.semantic.inputs or {}) do
            if type(spec.input)=="table" and input.wire==spec.input.wire then expected_input=type_node.substitute(input.type,bindings) end
          end
          if not expected_input or not type_node.equal(spec.input.type,expected_input) then
            errors[#errors+1]=string.format("actor %q: semantic input wire has the wrong type",tostring(spec.id))
          end
          local expected={}
          for _,output in ipairs(decl.semantic.outputs or {}) do expected[output.wire]={type=type_node.substitute(output.type,bindings),required=output.required~=false} end
          for _,output in ipairs(spec.outputs or {}) do
            local item=expected[output.wire]
            if not item or not compatible_output_type(item.type,output.type) then errors[#errors+1]=string.format("actor %q: undeclared semantic output wire %q",tostring(spec.id),tostring(output.wire)) else expected[output.wire]=nil end
          end
          for wire,item in pairs(expected) do if item.required then errors[#errors+1]=string.format("actor %q: semantic output for wire %q is missing",tostring(spec.id),wire) end end
          for name,scheme in pairs(decl.semantic.params or {}) do
            local expected_type=type_node.substitute(scheme,bindings)
            if type(spec.params)~="table" or not type_node.equal(spec.params[name],expected_type) then errors[#errors+1]=string.format("actor %q: semantic param %q has the wrong type descriptor",tostring(spec.id),name) end
          end
        end
      end
    end
  end
  return #errors==0 and {ok=true} or {ok=false,errors=errors}
end

return registry
