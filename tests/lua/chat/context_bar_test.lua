-- Load the starter opinion explicitly, not from config search precedence.
local common_path = assert(package.searchpath("libs.chat.common", package.path))
local root = assert(common_path:match("^(.*)/lua/libs/chat/common.lua$"))
package.loaded["nefor-tui"] = { util = {} }
local bar = assert(loadfile(root .. "/examples/nefor-agent/chat/context_bar.lua"))()
package.loaded["chat.context_bar"] = bar
package.loaded.config = { active = { usage = {
  statusline_ids = { "fixture/quota" },
} } }
tui = {
  spans = function(value) return value end,
  expanded = function(value) return value.child end,
  constrained = function(value) return value.child end,
  row = function(value)
    value.spans = {}
    for _, child in ipairs(value.children) do
      for _, span in ipairs(child.spans) do value.spans[#value.spans + 1] = span end
    end
    return value
  end,
}
local footer = assert(loadfile(root .. "/examples/nefor-agent/chat/statusline.lua"))()

local function eq(actual, expected, reason)
  assert(actual == expected, reason .. ": expected " .. tostring(expected)
    .. ", got " .. tostring(actual))
end

local function text(segment)
  local out = {}
  for _, span in ipairs(segment.spans) do out[#out + 1] = span.text end
  return table.concat(out)
end

local function cells(segment)
  local out = {}
  for _, span in ipairs(segment.spans) do
    for char in span.text:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
      out[#out + 1] = { char = char, fg = span.fg, bg = span.bg }
    end
  end
  return out
end

local function luminance(color)
  local components = {}
  for component in color:gmatch("%x%x") do
    local n = tonumber(component, 16) / 255
    components[#components + 1] = n <= 0.04045 and n / 12.92 or ((n + 0.055) / 1.055) ^ 2.4
  end
  return 0.2126 * components[1] + 0.7152 * components[2] + 0.0722 * components[3]
end

local filled_colors = { ["#5F87D7"] = true, ["#D7AF5F"] = true, ["#D75F5F"] = true }
local function colored(segment)
  local out, filled = {}, 0
  for _, cell in ipairs(cells(segment)) do
    if cell.bg and cell.char ~= "│" then
      out[#out + 1] = cell
      local fg, bg = luminance(cell.fg), luminance(cell.bg)
      assert((math.max(fg, bg) + 0.05) / (math.min(fg, bg) + 0.05) >= 4.5,
        "region text has at least 4.5:1 contrast on filled and unfilled cells")
      if filled_colors[cell.bg] then
        filled = filled + 1
        eq(cell.fg, "#101010", "dark text contrasts with filled backgrounds")
      else
        assert(cell.fg ~= cell.bg, "unfilled labels contrast with their background")
      end
    end
  end
  return out, filled
end

local example = bar.segment(108000, 272000, 872000)
eq(text(example), "108k · 12%   272k   │     740k      │872k", "stationary centered three-region fixture")
eq(#cells(example), 41, "compact example width, including dividers")
for _, cell in ipairs(cells(example)) do
  if cell.char == "·" or cell.char == "%" then
    eq(cell.bg, nil, "count/percentage prefix is not part of the colored bar")
    eq(cell.fg, example.used.fg, "dot and percentage use the ordinary text style")
  end
end
for _, segment in ipairs({ bar.segment(0, 872000), bar.segment(0, 800000, 872000),
  bar.segment(0, 500000, 872000), bar.segment(nil, nil, nil) }) do
  eq(#cells({ spans = segment.bar }), 30, "one/two/three-region and neutral bars keep the same footprint")
end
local baseline = colored(example)
eq(#baseline, 28, "28 proportional colored cells")
local previous = -1
for used = 0, 900000, 1000 do
  local segment = bar.segment(used, 272000, 872000)
  local current, filled = colored(segment)
  eq(#current, 28, "usage cannot resize the bar")
  assert(filled >= previous, "visual fill is monotonic through maximum")
  previous = filled
  for i, cell in ipairs(current) do
    eq(cell.char, baseline[i].char, "label position stays fixed while fill crosses it")
    local region = i <= 9 and "blue" or (i <= 24 and "yellow" or "red")
    local expected = ({ blue = "#5F87D7", yellow = "#D7AF5F", red = "#D75F5F" })[region]
    local empty = ({ blue = "#20304D", yellow = "#463920", red = "#462424" })[region]
    assert(cell.bg == expected or cell.bg == empty, "fill respects region colors")
  end
end
local _, full = colored(bar.segment(872000, 272000, 872000))
eq(full, 28, "maximum fills the whole bar")
local over = bar.segment(1000000, 272000, 872000)
assert(text(over):match("^1.0M ") and text(over):match(" · 115%% "), "over-capacity usage and percent are not capped")
local _, over_fill = colored(over)
eq(over_fill, 28, "only visual fill caps at maximum")
local unknown = bar.segment(nil, 272000, 872000)
assert(text(unknown):match("^%? · %?%% "), "unknown is not a measured zero")
for _, segment in ipairs({ unknown, bar.segment(nil, nil, nil), bar.segment(47000, nil, nil) }) do
  for _, span in ipairs(segment.bar) do
    if span.text ~= "│" then
      eq(span.bg, "#30303C", "unknown measurements or capacities use neutral backgrounds")
    end
    assert(not filled_colors[span.bg], "unknown is not a measured fill")
  end
  eq(segment.percent, "?", "percentage needs both measurement and denominator")
end
assert(text(bar.segment(0, nil, nil)):match("^0 · %?%% "), "known zero without capacity")
assert(text(bar.segment(nil, nil, nil)):match("^%? · %?%% "), "unknown without capacity")
assert(text(bar.segment(108000, nil, nil)):match("^108k · %?%% "), "no invented capacity or percent")
eq(text(bar.segment(108000, 272000)), "108k · 40%              272k             ", "one default-window fixture")
eq(text(bar.segment(108000, 272000, 272000)), text(bar.segment(108000, 272000)), "equal maximum collapses")
eq(text(bar.segment(108000, 272000, 100000)), text(bar.segment(108000, 272000)), "inconsistent maximum retains default")
eq(text(bar.segment(1, 1, 2, 1)), "1 · 50% 1│2", "tiny windows omit overlapping warning boundary")
eq(text(bar.segment(950, 900, 1000, 1)), "950 · 95% 900│1k", "default beyond warning collapses yellow")
eq(text(bar.segment(1001, 1001, 1002, 1)), "1k · 100% 1001│1002", "rounded labels do not duplicate thresholds")
local narrow = bar.segment(108000, 272000, 872000, 1)
eq(text(narrow), "108k · 12% 272k│740k│872k", "minimum width keeps labels in their regions")
local invalid = bar.segment(-1, 0, 0)
assert(text(invalid):match("^%? · %?%% "), "invalid inputs do not become zero")
eq(text(bar.segment(0 / 0, 272000)), text(bar.segment(nil, 272000)), "nonfinite measurement stays unknown")

local row = footer.view({ model = "gpt-5.4", reasoning_effort = "high", gate_mode = "safe",
  current_context_tokens = 108000, max_tokens = 272000, max_context_tokens = 872000,
  usage = { ["fixture/quota"] = { kind = "free" } },
})
eq(text(row), "SAFE │ gpt-5.4 · high │ free │ 108k · 12%    272k   │     740k      │872k", "footer preserves model/reasoning/quota")
for _, child in ipairs(row.children) do
  if child.wrap then eq(child.wrap, "none", "footer remains a single row") end
end
assert(#cells(row) <= 80, "representative sidebar chat width fits within 80 columns")
assert(text(footer.view({ model = "gpt-5.4", max_tokens = 272000 })):find("?", 1, true), "footer shows unknown with known capacity")
print("context_bar_test: all assertions passed")
