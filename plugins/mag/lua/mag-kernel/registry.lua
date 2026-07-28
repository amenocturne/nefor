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
local kinds = require("kinds")
local type_node = require("type-node")

local registry = {}
registry.__index = registry

local function compatible_output_type(expected, actual)
  if type_node.equal(expected, actual) then return true end
  -- A library boundary may give a foreign actor's nominal success output a
  -- more specific domain name while retaining the actor's runtime wire.
  return expected.kind == "named" and actual.kind == "named"
end

local function accepts_semantic(target, source)
  if type_node.equal(target, source) then return true end
  local host = nefor and nefor.semantic_type
  if type(host) ~= "table" or type(host.accepts) ~= "function" then
    error("registry requires compiler semantic_type.accepts")
  end
  return host.accepts(target, source)
end

local function product_input_covered(target, sources)
  if #sources == 1 and type_node.equal(target, sources[1]) then return true end
  local host = nefor and nefor.semantic_type
  if type(host) ~= "table" or type(host.input_covered_by) ~= "function" then
    error("registry requires compiler semantic_type.input_covered_by")
  end
  return host.input_covered_by(target, sources)
end

-- Kernel-synthesized status types (docs/ir.md, Firing: "Reserved status types
-- are kernel-emitted"). Lowering encodes ordering/failure edges as route keys
-- carrying these tags, yet a factory never declares them as outputs — the
-- kernel emits them when applying a completion (routing.lua, apply_completion).
-- So they are implicitly-permitted route keys on any actor:
--   mag.Unit   — successful completion (a pure dependency edge, `C -> A`)
--   mag.Failed — a suffered failure the kernel synthesizes (provider error,
--                kill mid-flight, budget). A factory's OWN computed failure
--                output is a declared tag and needs no exception.
-- Both names are the canonical constants (kinds.lua), shared with routing and
-- the factories so no route key is re-spelled inline.
local RESERVED_ROUTE_KEYS = {
  [kinds.Unit] = true,
  [kinds.Failed] = true,
}

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

  return decl
end

-- ---- construction -----------------------------------------------------------

function registry.new()
  return setmetatable({ factories = {}, identities = {} }, registry)
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
    return nil, string.format("foreign identity %q already registered", identity)
  end
  decl.identity = identity
  self.factories[decl.name] = { declaration = decl, construct = entry.construct }
  self.identities[identity] = decl.name
  return decl
end

-- Look up a factory by name; nil if unknown.
function registry:lookup(name)
  return self.factories[name] or self.factories[self.identities[name]]
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
  local f = self:lookup(name)
  return f and f.declaration or nil
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
  local f = self:lookup(name)
  if not f then
    return nil, string.format("unknown factory %q", tostring(name))
  end
  return f.construct(id, params, emit, deps)
end

-- ---- modification validation ------------------------------------------------

-- Validate the actor specs of a graph modification against declared contracts
-- (docs/ir.md). Two checks per actor spec { id, factory, params, routes }:
--
--   1. `factory` names a registered factory        (unknown -> rejection).
--   2. every `routes` key is a declared output tag of that factory OR a
--      reserved kernel-synthesized status tag (RESERVED_ROUTE_KEYS — mag.Unit
--      dependency edges and the suffered-failure tag, which lowering emits but
--      no factory declares), and each destination actor's declared input shape
--      accepts that tag (wiring compatibility — the v1 "sniff inputs for
--      output.tool_calls" mode is gone; compatibility is a type fact over
--      declared shapes). The check stays strict for every other key.
--
-- Destination resolution: an id spawned in the same modification resolves
-- through `actors`; anything else resolves through the optional `resolve`
-- callback — `fn(id) -> factory, state` for an id the caller (the kernel
-- fold) already knows, nil for a never-existed id. With a resolver:
--
--   * a live destination's declared inputs must accept the tag (mismatch ->
--     rejection — this is the apply-time seat belt that makes the shipped
--     "warn-dropped exhaust route" bug class unrepresentable);
--   * a dead destination is skipped — routes computed while the target lived
--     are race artifacts, and the delivery layer drops those sends as logged
--     no-ops (settled race semantics, docs/ir.md);
--   * a never-existed destination rejects — mirroring message-target
--     validation ("only never-existed targets reject — that is a typo").
--
-- Without a resolver (direct registry use in tests), destinations outside the
-- modification are skipped, as before.
--
-- Returns { ok = true } or { ok = false, errors = { <msg>, ... } }.
function registry:validate_modification(modification, resolve, existing_specs)
  local errors = {}
  local actors = (modification and modification.actors) or {}
  local declarations = modification and modification.types or nil
  local semantic_host = nefor and nefor.semantic_type

  local function validate_type_reference(reference, label)
    if type(reference) ~= "table" or type(reference.type) ~= "table" or
        type(reference.type_id) ~= "string" then
      table.insert(errors, label .. ": missing semantic descriptor identity")
      return
    end
    local declared = declarations and declarations[reference.type_id] or nil
    if not declared or not type_node.equal(declared, reference.type) then
      table.insert(errors, label .. ": semantic descriptor identity is absent or mismatched")
    end
  end

  if declarations ~= nil then
    if type(declarations) ~= "table" or type(semantic_host) ~= "table" or
        type(semantic_host.validate_declarations) ~= "function" then
      table.insert(errors, "modification semantic declarations cannot be verified")
    else
      local ok, valid_or_error = pcall(semantic_host.validate_declarations, declarations)
      if not ok or valid_or_error ~= true then
        table.insert(errors, "modification semantic declarations are invalid: " ..
          tostring(valid_or_error))
      end
      for _, spec in ipairs(actors) do
        validate_type_reference(spec.input, string.format("actor %q input", tostring(spec.id)))
        for index, output in ipairs(spec.outputs or {}) do
          validate_type_reference(output, string.format(
            "actor %q output %d", tostring(spec.id), index))
        end
      end
      for index, message in ipairs(modification.messages or {}) do
        validate_type_reference(
          { type = message.semantic_type, type_id = message.semantic_type_id },
          string.format("message %d", index))
      end
      for index, rule in ipairs(modification.rules or {}) do
        validate_type_reference(rule.on, string.format("rule %d", index))
      end
      if modification.result ~= nil then
        validate_type_reference(modification.result.from, "result boundary")
      end
    end
  end

  -- id -> factory name, for destination wiring checks.
  local factory_of = {}
  local spec_of = {}
  for _, spec in ipairs(actors) do
    if type(spec.id) == "string" then
      factory_of[spec.id] = spec.factory
      spec_of[spec.id] = spec
    end
  end

  -- Resolve a route destination to a factory name (or nil + skip/error).
  -- Same-modification spawns win; then the caller's inventory via `resolve`.
  local function dest_factory_of(spec, tag, dest_id)
    local dest_factory = factory_of[dest_id]
    if dest_factory then
      return dest_factory, spec_of[dest_id]
    end
    if not resolve then
      return nil -- no resolver: outside-the-modification dests are skipped
    end
    local factory, state, resolved_spec = resolve(dest_id)
    if factory == nil then
      table.insert(errors, string.format(
        "actor %q: route %q destination %q does not exist "
        .. "(not in the inventory, not spawned in this modification)",
        tostring(spec.id), tag, tostring(dest_id)))
      return nil
    end
    if state == "dead" then
      return nil -- race artifact; delivery drops these as logged no-ops
    end
    return factory, resolved_spec
  end

  for _, spec in ipairs(actors) do
    local decl = self:declaration(spec.factory)
    if not decl then
      table.insert(errors, string.format(
        "actor %q: unknown factory %q", tostring(spec.id), tostring(spec.factory)))
    else
      local variables = decl.type_variables or {}
      local evidence = spec.evidence
      if decl.semantic and type(evidence) ~= "table" then
        table.insert(errors, string.format(
          "actor %q: semantic factory requires compiler foreign evidence", tostring(spec.id)))
      elseif type(evidence) == "table" and
          (evidence.version ~= 2 or evidence.identity ~= decl.identity or
           type(evidence.arguments) ~= "table" or type(evidence.input) ~= "table" or
           type(evidence.output) ~= "table") then
        table.insert(errors, string.format(
          "actor %q: invalid or mismatched foreign evidence", tostring(spec.id)))
      elseif type(evidence) == "table" and #evidence.arguments ~= #variables then
        table.insert(errors, string.format(
          "actor %q: factory %q expects %d type argument(s), got %d",
          tostring(spec.id), spec.factory, #variables, #evidence.arguments))
      elseif type(evidence) == "table" then
        local arguments={}; local evidence_ok=true
        for index, argument in ipairs(evidence.arguments) do
          local ok,err=type_node.validate(argument); arguments[index]=argument
          if not ok then evidence_ok=false; table.insert(errors,string.format(
            "actor %q: foreign evidence argument %d: %s",tostring(spec.id),index,err)) end
        end
        local input_ok,input_err=type_node.validate(evidence.input)
        local output_ok,output_err=type_node.validate(evidence.output)
        if not input_ok then evidence_ok=false; table.insert(errors,string.format("actor %q: evidence input: %s",tostring(spec.id),input_err)) end
        if not output_ok then evidence_ok=false; table.insert(errors,string.format("actor %q: evidence output: %s",tostring(spec.id),output_err)) end
        local input = spec.input
        local input_type=type(input)=="table" and input.type or nil
        local spec_input_ok=type_node.validate(input_type)
        if not spec_input_ok then
          table.insert(errors, string.format("actor %q: semantic input is not a valid structural type", tostring(spec.id)))
        end
        if decl.semantic and evidence_ok then
          local bindings={}; for index,variable in ipairs(variables) do bindings[variable]=arguments[index] end
          local evidence_input=type_node.substitute(decl.semantic.input,bindings)
          local evidence_output=type_node.substitute(decl.semantic.output,bindings)
          local expected_input=nil
          for _,endpoint in ipairs(decl.semantic.inputs) do
            if endpoint.wire==input.wire then expected_input=type_node.substitute(endpoint.type,bindings) end
          end
          local expected_outputs={}
          for _,endpoint in ipairs(decl.semantic.outputs) do
            expected_outputs[endpoint.wire]=type_node.substitute(endpoint.type,bindings)
          end
          if not type_node.equal(evidence.input,evidence_input) or
              not type_node.equal(evidence.output,evidence_output) then
            table.insert(errors, string.format("actor %q: foreign evidence does not instantiate the registry scheme", tostring(spec.id)))
          end
          if not expected_input or not type_node.equal(input.type,expected_input) then
            table.insert(errors,string.format("actor %q: semantic input wire has the wrong type",tostring(spec.id)))
          end
          local actual={}
          for _,output in ipairs(spec.outputs or {}) do
            local ok,err=type_node.validate(output.type)
            if not ok then table.insert(errors,string.format("actor %q output type: %s",tostring(spec.id),err))
            elseif actual[output.wire] then table.insert(errors,string.format("actor %q: duplicate semantic output wire %q",tostring(spec.id),output.wire))
            else actual[output.wire]=output.type end
          end
          for wire,expected in pairs(expected_outputs) do
            local required=type_node.equal(expected,evidence_output)
            if evidence_output.kind=="union" then
              for _,arm in ipairs(evidence_output.items) do if type_node.equal(expected,arm) then required=true end end
            end
            if (required and not actual[wire]) or (actual[wire] and not compatible_output_type(expected,actual[wire])) then
              table.insert(errors,string.format("actor %q: semantic output for wire %q is missing or has the wrong type",tostring(spec.id),wire))
            end
            actual[wire]=nil
          end
          if next(actual) then table.insert(errors,string.format("actor %q: undeclared semantic output wire",tostring(spec.id))) end
          end
        end
      local declared_output = {}
      for _, tag in ipairs(decl.outputs) do
        declared_output[tag] = true
      end

      for tag, dests in pairs(spec.routes or {}) do
        if not declared_output[tag] and not RESERVED_ROUTE_KEYS[tag] then
          table.insert(errors, string.format(
            "actor %q: route key %q is not a declared output of factory %q",
            tostring(spec.id), tostring(tag), spec.factory))
        else
          local source_endpoint = nil
          for _, output in ipairs(spec.outputs or {}) do
            if output.wire == tag then
              if source_endpoint then source_endpoint = false else source_endpoint = output end
            end
          end
          for _, destination in ipairs(dests) do
            local dest_id = destination.actor
            local dest_wire = destination.wire
            local dest_factory,dest_spec = dest_factory_of(spec, tag, dest_id)
            if dest_factory then
              local dest_decl = self:declaration(dest_factory)
              if not dest_decl then
                table.insert(errors, string.format(
                  "actor %q: destination %q uses unknown factory %q",
                  tostring(spec.id), tostring(dest_id), tostring(dest_factory)))
              else
                local accepted = false
                for _, in_shape in pairs(dest_decl.inputs) do
                  if shape.accepts(in_shape, dest_wire) then
                    accepted = true
                    break
                  end
                end
                if not accepted then
                  table.insert(errors, string.format(
                    "wiring %q -%s-> %q/%s: no input of factory %q accepts the destination wire",
                    tostring(spec.id), tag, tostring(dest_id), tostring(dest_wire), dest_factory))
                elseif dest_spec and (spec.evidence or dest_spec.evidence) then
                  local source_semantic=nil
                  for _,output in ipairs(spec.outputs or {}) do
                    if output.wire==tag then
                      if source_semantic then source_semantic=false else source_semantic=output.type end
                    end
                  end
                  local dest_semantic=type(dest_spec.input)=="table" and dest_spec.input.type or nil
                  if not source_semantic or not dest_semantic or
                      not accepts_semantic(dest_semantic, source_semantic) then
                    table.insert(errors,string.format(
                      "wiring %q -%s-> %q: semantic endpoint types differ",
                      tostring(spec.id),tag,tostring(dest_id)))
                  end
                end
                if declarations ~= nil then
                  if not source_endpoint or
                      destination.source_type_id ~= source_endpoint.type_id or
                      type(dest_spec.input) ~= "table" or
                      destination.destination_type_id ~= dest_spec.input.type_id then
                    table.insert(errors, string.format(
                      "wiring %q -%s-> %q: route semantic identities differ from endpoints",
                      tostring(spec.id), tag, tostring(dest_id)))
                  end
                end
              end
            end
          end
        end
      end
    end
  end

  -- Product firing is an all-of contract over the complete post-apply route
  -- topology. Per-edge compatibility is insufficient: repeated components
  -- need repeated sender-bound edges, and both underfill and overfill must be
  -- rejected before actors are registered.
  local post_specs = {}
  for _, spec in ipairs(existing_specs or {}) do post_specs[spec.id] = spec end
  for _, spec in ipairs(actors) do
    local _, prior_state = nil, nil
    if resolve then _, prior_state = resolve(spec.id) end
    -- Inventory lifecycles are monotone: a spawn at a tombstoned id is a
    -- no-op, so its proposed routes cannot participate in post-apply product
    -- coverage. A live id is already represented by existing_specs and also
    -- wins over a duplicate spawn.
    if prior_state ~= "dead" and not post_specs[spec.id] then
      post_specs[spec.id] = spec
    end
  end
  for _, id in ipairs((modification and modification.kills) or {}) do
    post_specs[id] = nil
  end
  local incoming = {}
  for _, source in pairs(post_specs) do
    for wire, destinations in pairs(source.routes or {}) do
      local source_type = nil
      for _, output in ipairs(source.outputs or {}) do
        if output.wire == wire then
          if source_type then source_type = false else source_type = output.type end
        end
      end
      for _, destination in ipairs(destinations) do
        if source_type then
          incoming[destination.actor] = incoming[destination.actor] or {}
          table.insert(incoming[destination.actor], source_type)
        end
      end
    end
  end
  for id, target in pairs(post_specs) do
    local input_type = type(target.input) == "table" and target.input.type or nil
    if type(input_type) == "table" and input_type.kind == "product" and
        not product_input_covered(input_type, incoming[id] or {}) then
      table.insert(errors, string.format(
        "actor %q: product input incoming route types must exactly cover its component multiset",
        tostring(id)))
    end
  end

  if #errors == 0 then
    return { ok = true }
  end
  return { ok = false, errors = errors }
end

return registry
