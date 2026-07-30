-- Generic interpreter for serialized factory preview descriptions.
-- Concrete factory names never participate in rendering.
local common = require("libs.chat.common")
local preview_state = require("libs.chat.preview_state")
local STYLE = common.STYLE
local MARKDOWN_THEME = common.MARKDOWN_THEME

local M = {}

local STYLE_NAMES = {
  section = STYLE.popup_user, status = STYLE.status, dim = STYLE.status_dim,
  reasoning = STYLE.reasoning, assistant = STYLE.status,
  command = STYLE.popup_user, stdin = STYLE.system, stdout = STYLE.status,
  stderr = STYLE.tool_error, tool_call = STYLE.system,
  tool_result = STYLE.status_ok, error = STYLE.tool_error,
}

local function encode(value, depth, seen)
  if type(value) == "string" then return value end
  if type(value) ~= "table" then return tostring(value == nil and "—" or value) end
  depth, seen = depth or 0, seen or {}
  if seen[value] then return "<cycle>" end
  seen[value] = true
  local keys, array = {}, true
  for k in pairs(value) do keys[#keys + 1] = k; if type(k) ~= "number" then array = false end end
  table.sort(keys, function(a, b) return tostring(a) < tostring(b) end)
  local parts = {}
  for _, k in ipairs(keys) do
    local rendered = encode(value[k], depth + 1, seen)
    parts[#parts + 1] = array and rendered or (tostring(k) .. ": " .. rendered)
  end
  seen[value] = nil
  if array then return "[" .. table.concat(parts, ", ") .. "]" end
  if #parts == 0 then return "{}" end
  local indent = string.rep("  ", depth + 1)
  return "{\n" .. indent .. table.concat(parts, ",\n" .. indent)
    .. "\n" .. string.rep("  ", depth) .. "}"
end
local function tool_value(value, result)
  value = type(value) == "table" and value or { value = value }
  local name = value.name or ((type(value["function"]) == "table") and value["function"].name)
  local id = value.id or value.tool_call_id
  local rows = {}
  if result then
    rows[#rows + 1] = "tool result" .. (id and (" · " .. tostring(id)) or "")
    rows[#rows + 1] = encode(value.output or value.result or value.content or value.value or value)
  else
    rows[#rows + 1] = "tool call" .. (name and (" · " .. tostring(name)) or "")
      .. (id and (" · " .. tostring(id)) or "")
    local args = value.arguments or value.args
      or ((type(value["function"]) == "table") and value["function"].arguments)
    rows[#rows + 1] = encode(args ~= nil and args or value)
  end
  return table.concat(rows, "\n")
end

local function format_value(value, format)
  if format == "tool_call" then return tool_value(value, false) end
  if format == "tool_result" then return tool_value(value, true) end
  if format == "validation" and type(value) == "table" then
    return "validation attempt " .. tostring(value.attempt or "?") .. "\noutput:\n"
      .. encode(value.output) .. "\nviolations:\n" .. encode(value.violations)
  end
  return encode(value)
end
M.format_value = encode

local function resolve(ref, ctx)
  if type(ref) ~= "table" or not ref.binding then return ref end
  if ref.binding == "param" then return (ctx.node.params or {})[ref.name] end
  if ref.binding == "input" then return (ctx.node.inputs or {})[ref.name] end
  if ref.binding == "output" then return (ctx.node.outputs or {})[ref.name] end
  if ref.binding == "state" then return (ctx.node.states or {})[ref.name] end
  if ref.binding == "stream" then return (ctx.node.streams or {})[ref.name] or {} end
  if ref.binding == "item" then return type(ctx.item) == "table" and ctx.item[ref.name] or nil end
  if ref.binding == "lifecycle" then
    if ref.name == "run_id" then return ctx.run_id end
    if ref.name == "run_name" then return ctx.run and ctx.run.run_name end
    if ref.name == "actor_id" then return ctx.actor_id end
    if ref.name == "factory" then return ctx.node.factory end
    return ctx.node[ref.name]
  end
end

local render

local function render_collection(desc, ctx)
  local values = resolve(desc.source, ctx) or {}
  local children = {}
  for _, wrapped in ipairs(values) do
    local item = wrapped.value ~= nil and wrapped.value or wrapped
    local template = desc.item
    if template and template.kind == "cases" then
      template = template.values and template.values[type(item) == "table" and item.kind or nil]
    end
    if template then children[#children + 1] = render(template, {
      state = ctx.state, node = ctx.node, run = ctx.run, run_id = ctx.run_id,
      actor_id = ctx.actor_id, item = item,
    }) end
  end
  if #children == 0 and desc.empty then children[1] = render(desc.empty, ctx) end
  return tui.column { gap = desc.gap or 0, key = desc.key, children = children }
end

render = function(desc, ctx)
  if type(desc) ~= "table" then
    return tui.text { content = "preview error: malformed description", style = STYLE.tool_error, wrap = "word" }
  end
  local kind = desc.kind
  if kind == "column" or kind == "row" then
    local children = {}
    for _, child in ipairs(desc.children or {}) do children[#children + 1] = render(child, ctx) end
    return tui[kind] { gap = desc.gap or 0, key = desc.key, children = children }
  elseif kind == "padding" then
    return tui.padding { value = desc.value or { top = desc.top or 0, right = desc.right or 0,
      bottom = desc.bottom or 0, left = desc.left or 0 }, key = desc.key, child = render(desc.child, ctx) }
  elseif kind == "block" then
    return common.bordered_box(render(desc.child, ctx), STYLE_NAMES[desc.style] or STYLE.footer, desc.key)
  elseif kind == "spacer" then
    return tui.spacer { flex = desc.flex, key = desc.key }
  elseif kind == "text" then
    return tui.text { content = tostring(resolve(desc.value, ctx) or ""),
      style = STYLE_NAMES[desc.style], wrap = desc.wrap or "word", key = desc.key }
  elseif kind == "spans" then
    local value = resolve(desc.value, ctx)
    local spans = {}
    for _, span in ipairs(type(value) == "table" and value or {}) do
      spans[#spans + 1] = type(span) == "table" and span or { text = tostring(span) }
    end
    return tui.spans { spans = spans, wrap = desc.wrap or "word", key = desc.key }
  elseif kind == "markdown" then
    return tui.markdown { source = tostring(resolve(desc.value, ctx) or ""),
      theme = MARKDOWN_THEME, wrap = desc.wrap or "word", key = desc.key }
  elseif kind == "value" then
    return tui.text { content = format_value(resolve(desc.value, ctx), desc.format),
      style = STYLE_NAMES[desc.style], wrap = desc.wrap or "word", key = desc.key }
  elseif kind == "stream" or kind == "list" then
    return render_collection(desc, ctx)
  end
  return tui.text { content = "preview error: unsupported primitive " .. tostring(kind),
    style = STYLE.tool_error, wrap = "word" }
end

function M.node(state, run_id, actor_id)
  local node = preview_state.node(state, run_id, actor_id)
  local contract = preview_state.contract(state, node)
  if not node then return tui.text { content = "Node is no longer available.", style = STYLE.status_dim } end
  if not contract or not contract.preview then
    return tui.text { content = "Preview declaration is unavailable.", style = STYLE.tool_error }
  end
  return render(contract.preview, { state = state, node = node,
    run = (state.runs or {})[run_id], run_id = run_id, actor_id = actor_id })
end

function M.activity(item)
  local value = item.item and item.item.value
  local label = type(value) == "table" and value.kind or item.binding
  local content
  if type(value) == "table" and type(value.text) == "string" then content = value.text
  elseif type(value) == "table" and value.value ~= nil then content = encode(value.value)
  else content = encode(value) end
  return tui.column { gap = 0, children = {
    tui.text { content = tostring(label or "activity"), style = STYLE.footer, wrap = "none" },
    tui.text { content = content, wrap = "word" },
  } }
end

return M
