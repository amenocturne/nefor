-- Bounded textual projections for values entering a provider continuation.
-- Canonical typed values and persisted node outputs stay untouched; only the
-- provider-facing content produced by adapter factories passes through here.

local policy = require("libs.model-context-policy")
local M = {}

M.ITEM_LIMIT = policy.item_limit
M.TURN_LIMIT = policy.continuation_limit
M.HEAD_TARGET = math.floor(M.ITEM_LIMIT * 3 / 4)
M.TAIL_TARGET = M.ITEM_LIMIT - M.HEAD_TARGET

local function utf8_head(value, limit)
  if #value <= limit then return value end
  local finish = limit
  while finish > 0 do
    local byte = value:byte(finish)
    if byte < 128 or byte >= 192 then
      if byte >= 192 then
        local width = byte < 224 and 2 or (byte < 240 and 3 or 4)
        if finish + width - 1 > limit then finish = finish - 1 end
      end
      break
    end
    finish = finish - 1
  end
  return value:sub(1, finish)
end

local function utf8_tail(value, limit)
  if #value <= limit then return value end
  local start = #value - limit + 1
  while start <= #value do
    local byte = value:byte(start)
    if byte < 128 or byte >= 192 then break end
    start = start + 1
  end
  return value:sub(start)
end

local function location(path)
  if type(path) == "string" and path ~= "" then
    return "Full output: " .. path
  end
  return "Full output is unavailable (no persisted output path)."
end

local function marker(original, omitted_start, omitted_end, path)
  return string.format(
    "\n\n[output truncated: original %d bytes; omitted %d bytes at zero-based half-open range [%d, %d). %s]\n\n",
    original, omitted_end - omitted_start, omitted_start, omitted_end, location(path))
end

local function render(value, budget, path)
  local original = #value
  if original <= budget then return value end
  if budget <= 0 then return "" end

  local retained = math.min(original, budget)
  local text
  -- Marker digits depend on the actual UTF-8-safe boundaries. Recompute until
  -- the rendered byte count and stated omitted range agree.
  for _ = 1, 8 do
    local nominal_start = math.min(original, retained)
    local nominal_end = original
    local note = marker(original, nominal_start, nominal_end, path)
    local room = math.max(0, budget - #note)
    local head_room = math.min(M.HEAD_TARGET, math.floor(room * 3 / 4))
    local tail_room = math.min(M.TAIL_TARGET, room - head_room)
    local spare = room - head_room - tail_room
    head_room = head_room + spare
    local head = utf8_head(value, head_room)
    local tail = utf8_tail(value, tail_room)
    local omitted_start = #head
    local omitted_end = original - #tail
    retained = #head + #tail
    text = head .. marker(original, omitted_start, omitted_end, path) .. tail
    if #text <= budget then return text end
  end
  -- Defensive convergence fallback for unusually long paths: preserve a
  -- UTF-8-safe marker prefix rather than exceeding the hard byte ceiling.
  return utf8_head(text or marker(original, 0, original, path), budget)
end

local function textual(value)
  if type(value) == "string" then return value end
  if type(nefor) == "table" and type(nefor.json) == "table"
      and type(nefor.json.encode) == "function" then
    local ok, encoded = pcall(nefor.json.encode, value)
    if ok and type(encoded) == "string" then return encoded end
  end
  return tostring(value or "")
end

-- Deterministic max-min allocation. Every entry first receives the same share;
-- entries smaller than that share are satisfied and their unused bytes are
-- redistributed among the remaining entries. Thus small siblings survive a
-- huge result intact and no result is silently omitted (subject only to the
-- pathological case of more entries than bytes in the aggregate ceiling).
local function allocations(sizes, total)
  local result, pending, remaining = {}, {}, total
  for index = 1, #sizes do pending[index] = true end
  local count = #sizes
  while count > 0 do
    local share = math.floor(remaining / count)
    local satisfied = false
    for index = 1, #sizes do
      if pending[index] and sizes[index] <= share then
        result[index] = sizes[index]
        remaining = remaining - sizes[index]
        pending[index] = nil
        count = count - 1
        satisfied = true
      end
    end
    if not satisfied then
      local extra = remaining - share * count
      for index = 1, #sizes do
        if pending[index] then
          result[index] = share + (extra > 0 and 1 or 0)
          if extra > 0 then extra = extra - 1 end
        end
      end
      break
    end
  end
  return result
end

function M.project(entries, preserve_under_limit)
  local texts, sizes, total = {}, {}, 0
  for index, entry in ipairs(entries or {}) do
    local text = textual(entry.value)
    texts[index] = text
    sizes[index] = math.min(#text, M.ITEM_LIMIT)
    total = total + sizes[index]
  end
  local budgets = total <= M.TURN_LIMIT and sizes or allocations(sizes, M.TURN_LIMIT)
  local projected = {}
  for index, entry in ipairs(entries or {}) do
    if preserve_under_limit and #texts[index] <= budgets[index] then
      projected[index] = entry.value
    else
      projected[index] = render(texts[index], budgets[index], entry.output_path)
    end
  end
  return projected
end

M._utf8_head = utf8_head
M._utf8_tail = utf8_tail
M._render = render
M._allocations = allocations

return M
