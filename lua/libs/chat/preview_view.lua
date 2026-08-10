-- TUI-owned rendering of the TUI's local projection of generic MAG facts.
-- No factory or producer supplies presentation instructions.
local common = require("libs.chat.common")
local preview_state = require("libs.chat.preview_state")
local STYLE = common.STYLE

local M = {}

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
local function value_size(value)
  if value == nil then return 0 end
  if type(value) == "string" then return #value end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.encode) == "function" then
    local ok, encoded = pcall(nefor.json.encode, value)
    if ok and type(encoded) == "string" then return #encoded end
  end
  return #encode(value)
end

local function size_text(bytes)
  if bytes < 1024 then return tostring(bytes) .. " B" end
  return string.format("%.1f KiB", bytes / 1024)
end

local function tool_parts(value)
  value = type(value) == "table" and value or { value = value }
  local fn = type(value["function"]) == "table" and value["function"] or nil
  return value, value.name or (fn and fn.name), value.id or value.tool_call_id,
    value.arguments or value.args or (fn and fn.arguments)
end

local function concise_error(value)
  local text = encode(value):gsub("%s+", " ")
  local limit = 160
  if #text <= limit then return text end
  return text:sub(1, limit) .. "…"
end

local function tool_value(value, result, ctx)
  local expanded = ctx.state.expanded_details == true
  local original, name, id, args = tool_parts(value)
  if result then
    local failed = original.error ~= nil
    local output = failed and original.error
      or original.output or original.result or original.content or original.value or original
    local label = (failed and "✗ " or "✓ ") .. tostring(name or "tool")
    if not expanded then
      if failed then return label .. " · failed · " .. concise_error(output) end
      return label .. " · completed · " .. size_text(value_size(output)) .. " hidden"
    end
    return label .. (id and (" · " .. tostring(id)) or "") .. "\n" .. encode(output)
  end
  local label = "▸ " .. tostring(name or "tool")
  if not expanded then
    local display = require("libs.chat.tool_display")
    local contract = type(ctx.state.tool_displays) == "table"
      and ctx.state.tool_displays[name] or nil
    local projection = display.project(contract, args, nil, false, name)
    if projection then
      label = "▸ " .. tostring(projection.label or name or "tool")
      if projection.primary and projection.primary ~= "" then
        label = label .. " · " .. projection.primary
      end
    end
    local fields = {}
    for _, field in ipairs((projection and projection.arguments) or {}) do
      fields[#fields + 1] = field.label .. ": " .. field.value
    end
    return #fields > 0 and (label .. " · " .. table.concat(fields, " · ")) or label
  end
  return "▼ " .. tostring(name or "tool") .. (id and (" · " .. tostring(id)) or "")
    .. "\n" .. encode(args ~= nil and args or original)
end

local function reasoning_value(value, ctx)
  local text = tostring(value or "")
  local expanded = ctx.state.expanded_details == true
  local working = ctx.node.status == "working" or ctx.node.status == "running"
  local live = working and ctx.is_last
  if not expanded and not live then return "▸ reasoning" end
  return (live and "▼ thinking…" or "▼ reasoning") .. "\n  "
    .. text:gsub("\n", "\n  ")
end

M.format_value = encode

local function section(title, values)
  local keys = {}
  for key in pairs(values or {}) do
    if key ~= "last" then keys[#keys + 1] = key end
  end
  if #keys == 0 and values and values.last ~= nil then keys[1] = "last" end
  table.sort(keys)
  if #keys == 0 then return nil end
  local children = { tui.text { content = title, style = STYLE.popup_user, wrap = "none" } }
  for _, key in ipairs(keys) do
    children[#children + 1] = tui.text {
      content = tostring(key) .. "\n" .. encode(values[key]), style = STYLE.status, wrap = "word",
    }
  end
  return tui.column { gap = 1, children = children }
end

function M.node(state, run_id, actor_id, options)
  local node = preview_state.node(state, run_id, actor_id)
  if not node then return tui.text { content = "Node is no longer available.", style = STYLE.status_dim } end
  local children = {
    tui.text { content = tostring(node.factory or "actor") .. " · " .. tostring(node.status or "pending"),
      style = STYLE.status, wrap = "word" },
  }
  local sections = { section("Params", node.params), section("Input", node.inputs),
    section("Output", node.outputs) }
  for index = 1, 3 do
    local candidate = sections[index]
    if candidate then children[#children + 1] = candidate end
  end
  local items = preview_state.merged(state, run_id, function(id) return id == actor_id end)
  local activity = {}
  for index, item in ipairs(items) do
    local widget = M.activity(item, state, node, index == #items, options)
    if widget then activity[#activity + 1] = widget end
  end
  if #activity > 0 then
    children[#children + 1] = tui.text { content = "Activity", style = STYLE.popup_user, wrap = "none" }
    for _, widget in ipairs(activity) do children[#children + 1] = widget end
  end
  return tui.column { gap = 1, children = children }
end

function M.activity(item, state, node, is_last, options)
  local value = item.item and item.item.value
  local kind = type(value) == "table" and value.kind or nil
  if options and options.hide_tool_streams
      and (kind == "stdout" or kind == "stderr") then return nil end
  if kind == "reasoning" then
    return tui.text { content = reasoning_value(value.text, {
      state = state, node = node or {}, is_last = is_last,
    }), style = STYLE.reasoning, wrap = "word" }
  elseif kind == "tool_call" or kind == "call"
      or kind == "tool_result" or kind == "result" or kind == "error" then
    local result = kind == "tool_result" or kind == "result" or kind == "error"
    return tui.text { content = tool_value(value.value, result, {
      state = state, node = node or {},
    }), style = result and (kind == "error" and STYLE.tool_error or STYLE.status_ok)
      or STYLE.system, wrap = "word" }
  elseif kind == "diagnostic" and type(value.value) == "table" then
    local diagnostic = value.value
    if diagnostic.kind == "validation" then
      local content = "validation attempt " .. tostring(diagnostic.attempt or "?")
      if state.expanded_details == true then
        content = content .. "\nviolations:\n" .. encode(diagnostic.violations)
      else
        content = content .. " · details hidden"
      end
      return tui.text { content = content, style = STYLE.tool_error, wrap = "word" }
    end
    return tui.text { content = encode(diagnostic), style = STYLE.status_dim, wrap = "word" }
  end
  local label = kind or item.binding
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
