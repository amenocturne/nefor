-- JSON Schema `pattern` support for tool argument schemas.
--
-- Lua patterns cannot express alternation or quantified groups, so a schema
-- pattern is compiled from a small ECMA-262 regular-expression subset and
-- matched by backtracking. Supported syntax:
--
--   literals, `.`, `^`, `$`, `|`, `(...)`, `(?:...)`,
--   `*`, `+`, `?`, `{n}`, `{n,}`, `{n,m}` (greedy),
--   `[...]` / `[^...]` with ranges and the escapes below,
--   `\d \D \w \W \s \S \n \r \t` and any escaped punctuation.
--
-- Anything else (lazy quantifiers, lookaround, backreferences, word
-- boundaries, Unicode escapes, flags) is rejected at compile time so a schema
-- can never advertise a pattern this matcher would silently misread. Matching
-- is byte-oriented and unanchored, as JSON Schema specifies; `.` and negated
-- classes consume single bytes.

local M = {}

local function class_test(kind)
  if kind == "d" then return function(b) return b >= 48 and b <= 57 end end
  if kind == "w" then
    return function(b)
      return (b >= 48 and b <= 57) or (b >= 65 and b <= 90)
          or (b >= 97 and b <= 122) or b == 95
    end
  end
  if kind == "s" then
    return function(b) return b == 32 or (b >= 9 and b <= 13) end
  end
  return nil
end

local CONTROL_ESCAPES = { n = 10, r = 13, t = 9 }

local function is_punctuation(char)
  return char:match("^[%p]$") ~= nil
end

local Parser = {}
Parser.__index = Parser

function Parser:fail(message)
  error({ pattern_error = message .. " at offset " .. tostring(self.pos - 1) }, 0)
end

function Parser:peek() return self.src:sub(self.pos, self.pos) end

function Parser:take()
  local char = self:peek()
  self.pos = self.pos + 1
  return char
end

-- Returns a byte predicate for an escape, or a literal byte.
function Parser:escape()
  local char = self:take()
  if char == "" then self:fail("pattern ends with an incomplete escape") end
  local lower = char:lower()
  local test = class_test(lower)
  if test then
    if char ~= lower then return { negate = true, test = test } end
    return { test = test }
  end
  if CONTROL_ESCAPES[char] then return { byte = CONTROL_ESCAPES[char] } end
  if is_punctuation(char) then return { byte = char:byte() } end
  self:fail("unsupported escape `\\" .. char .. "`")
end

function Parser:class()
  local negate = false
  if self:peek() == "^" then negate = true; self:take() end
  local items = {}
  local first = true
  while true do
    local char = self:take()
    if char == "" then self:fail("unterminated character class") end
    if char == "]" and not first then break end
    if char == "]" then self:fail("empty character class") end
    first = false
    local low
    if char == "\\" then
      local escaped = self:escape()
      if escaped.test then
        items[#items + 1] = escaped
        goto continue
      end
      low = escaped.byte
    elseif char == "[" then
      self:fail("nested `[` in character class must be escaped")
    else
      low = char:byte()
    end
    if self:peek() == "-" and self.src:sub(self.pos + 1, self.pos + 1) ~= "]"
        and self.src:sub(self.pos + 1, self.pos + 1) ~= "" then
      self:take()
      local high_char = self:take()
      local high
      if high_char == "\\" then
        local escaped = self:escape()
        if escaped.test then self:fail("character class range cannot end in a class escape") end
        high = escaped.byte
      else
        high = high_char:byte()
      end
      if high < low then self:fail("character class range is out of order") end
      items[#items + 1] = { low = low, high = high }
    else
      items[#items + 1] = { low = low, high = low }
    end
    ::continue::
  end
  local function test(b)
    for _, item in ipairs(items) do
      if item.test then
        local hit = item.test(b)
        if item.negate then hit = not hit end
        if hit then return true end
      elseif b >= item.low and b <= item.high then
        return true
      end
    end
    return false
  end
  return { kind = "set", test = test, negate = negate }
end

function Parser:number()
  local digits = self.src:match("^%d+", self.pos)
  if not digits then return nil end
  self.pos = self.pos + #digits
  return tonumber(digits)
end

function Parser:quantifier(atom)
  local char = self:peek()
  local min, max
  if char == "*" then self:take(); min, max = 0, math.huge
  elseif char == "+" then self:take(); min, max = 1, math.huge
  elseif char == "?" then self:take(); min, max = 0, 1
  elseif char == "{" then
    self:take()
    min = self:number()
    if min == nil then self:fail("`{` must start a `{n}`, `{n,}` or `{n,m}` quantifier") end
    if self:peek() == "," then
      self:take()
      max = self:number() or math.huge
    else
      max = min
    end
    if self:take() ~= "}" then self:fail("unterminated `{` quantifier") end
    if max < min then self:fail("quantifier maximum is below its minimum") end
  else
    return atom
  end
  if atom.kind == "bol" or atom.kind == "eol" then self:fail("anchors cannot be quantified") end
  local follow = self:peek()
  if follow == "?" then self:fail("lazy quantifiers are not supported") end
  if follow == "*" or follow == "+" or follow == "{" then self:fail("nested quantifier") end
  return { kind = "repeat", node = atom, min = min, max = max }
end

function Parser:atom()
  local char = self:take()
  if char == "(" then
    if self:peek() == "?" then
      if self.src:sub(self.pos, self.pos + 1) ~= "?:" then
        self:fail("only `(?:...)` groups are supported")
      end
      self.pos = self.pos + 2
    end
    local node = self:alternation()
    if self:take() ~= ")" then self:fail("unterminated group") end
    return node
  end
  if char == "[" then return self:class() end
  if char == "." then
    return { kind = "set", test = function(b) return b ~= 10 end }
  end
  if char == "^" then return { kind = "bol" } end
  if char == "$" then return { kind = "eol" } end
  if char == "\\" then
    local escaped = self:escape()
    if escaped.test then
      return { kind = "set", test = escaped.test, negate = escaped.negate }
    end
    local byte = escaped.byte
    return { kind = "set", test = function(b) return b == byte end }
  end
  if char == "*" or char == "+" or char == "?" or char == "{" then
    self:fail("quantifier `" .. char .. "` has nothing to repeat")
  end
  if char == "]" or char == "}" then self:fail("unescaped `" .. char .. "`") end
  local byte = char:byte()
  return { kind = "set", test = function(b) return b == byte end }
end

function Parser:sequence()
  local items = {}
  while true do
    local char = self:peek()
    if char == "" or char == "|" or char == ")" then break end
    items[#items + 1] = self:quantifier(self:atom())
  end
  return { kind = "seq", items = items }
end

function Parser:alternation()
  local branches = { self:sequence() }
  while self:peek() == "|" do
    self:take()
    branches[#branches + 1] = self:sequence()
  end
  if #branches == 1 then return branches[1] end
  return { kind = "alt", branches = branches }
end

local match_node

local function match_seq(items, index, subject, position, continue)
  if index > #items then return continue(position) end
  return match_node(items[index], subject, position, function(next_position)
    return match_seq(items, index + 1, subject, next_position, continue)
  end)
end

match_node = function(node, subject, position, continue)
  local kind = node.kind
  if kind == "set" then
    if position > #subject then return false end
    local hit = node.test(subject:byte(position))
    if node.negate then hit = not hit end
    if not hit then return false end
    return continue(position + 1)
  elseif kind == "seq" then
    return match_seq(node.items, 1, subject, position, continue)
  elseif kind == "alt" then
    for _, branch in ipairs(node.branches) do
      if match_node(branch, subject, position, continue) then return true end
    end
    return false
  elseif kind == "bol" then
    return position == 1 and continue(position)
  elseif kind == "eol" then
    return position == #subject + 1 and continue(position)
  elseif kind == "repeat" then
    local function attempt(count, at)
      if count < node.max and match_node(node.node, subject, at, function(after)
        -- A zero-width iteration cannot make progress; stop repeating.
        if after == at then return false end
        return attempt(count + 1, after)
      end) then
        return true
      end
      return count >= node.min and continue(at)
    end
    return attempt(0, position)
  end
  error("pattern: unknown node kind " .. tostring(kind))
end

-- compile(source) -> matcher | nil, error
function M.compile(source)
  if type(source) ~= "string" then return nil, "pattern must be a string" end
  local parser = setmetatable({ src = source, pos = 1 }, Parser)
  local ok, result = pcall(function()
    local node = parser:alternation()
    if parser.pos <= #source then parser:fail("unbalanced `)`") end
    return node
  end)
  if not ok then
    if type(result) == "table" and result.pattern_error then return nil, result.pattern_error end
    error(result, 0)
  end
  local root = result
  return function(subject)
    for start = 1, #subject + 1 do
      if match_node(root, subject, start, function() return true end) then return true end
    end
    return false
  end
end

return M
