-- Unit tests for the nefor-tui widget library. Loaded by
-- crates/nefor/tests/plugin_libs_test.rs against a mock nefor.* surface
-- and a stub `tui.*` global; widgets must not reach into bus / NCP /
-- orchestrator state — anything that tries to call nefor.engine.send
-- here crashes because the stub deliberately omits those bindings.

local function tagged(kind)
  return function(args)
    args = args or {}
    args.__kind = kind
    return args
  end
end

_G.tui = {
  text         = tagged("text"),
  spans        = tagged("spans"),
  markdown     = tagged("markdown"),
  animation    = tagged("animation"),
  column       = tagged("column"),
  row          = tagged("row"),
  padding      = tagged("padding"),
  stack        = tagged("stack"),
  expanded     = tagged("expanded"),
  spacer       = tagged("spacer"),
  fill         = tagged("fill"),
  constrained  = tagged("constrained"),
  align        = tagged("align"),
  anchored     = tagged("anchored"),
  text_input   = tagged("text_input"),
  scrollable   = tagged("scrollable"),
}

local scroll_calls = {}
function _G.tui.scroll_by(key, delta)
  scroll_calls[#scroll_calls + 1] = { fn = "scroll_by", key = key, delta = delta }
end
function _G.tui.scroll_to(key, off)
  scroll_calls[#scroll_calls + 1] = { fn = "scroll_to", key = key, offset = off }
end
function _G.tui.scroll_into_view(key)
  scroll_calls[#scroll_calls + 1] = { fn = "scroll_into_view", key = key }
end
function _G.tui.scroll_position(key)
  return { offset = 0, max = 100, key = key }
end
function _G.tui.now_ms() return 0 end
function _G.tui.dimensions() return { width = 120, height = 40 } end
function _G.tui.virtual_scroll_prepare(key, n, heights, gap) return nil end
function _G.tui.virtual_scroll_invalidate(key) end
function _G.tui.copy_to_clipboard(text) end
function _G.tui.measure(node, width) return 1 end

local function reset_scroll_calls() scroll_calls = {} end

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format(
      "assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then
    error("assertion failed: " .. (msg or "(no message)"), 2)
  end
end

local function find_child(node, kind)
  if node == nil then return nil end
  if node.__kind == kind then return node end
  if node.children ~= nil then
    for _, c in ipairs(node.children) do
      local hit = find_child(c, kind)
      if hit ~= nil then return hit end
    end
  end
  if node.child ~= nil then
    return find_child(node.child, kind)
  end
  return nil
end

-- popup
local popup = require("nefor-tui.widget.popup")

do
  local tree = popup.view({
    open = true,
    title = "hello",
    child = "world",
    border_style = { fg = "#fff" },
  })
  assert_true(tree ~= nil, "popup renders when open")
  assert_eq(tree.__kind, "anchored", "popup outer = anchored")
  local title = find_child(tree, "text")
  assert_true(title ~= nil, "popup contains a text leaf")
end

do
  local tree = popup.view({ open = false, child = "x" })
  assert_eq(tree, nil, "closed popup returns nil")
end

do
  local closed = false
  local handler = function(api)
    return api.close()
  end
  local result = popup.handle({
    open = true,
    keys = { ["key.escape"] = handler },
  }, { kind = "key.escape" })
  assert_true(result ~= nil, "handler ran")
  assert_eq(result.open, false, "close() returned { open = false }")
  closed = result.open == false
  assert_true(closed, "popup closed")
end

do
  local result = popup.handle({
    open = true,
    keys = { ["key.escape"] = function() return {} end },
  }, { kind = "key.q" })
  assert_eq(result, nil, "no matching handler → nil")
end

-- picker
local picker = require("nefor-tui.widget.picker")

do
  local calls = 0
  local opts = {
    state = { cursor = 1 },
    entries = function() calls = calls + 1; return { "a", "b", "c" } end,
  }
  local tree = picker.view(opts)
  assert_true(tree ~= nil, "picker view renders")
  assert_eq(calls, 1, "source called once on view()")
  picker.handle(opts, { kind = "key.down" })
  assert_eq(calls, 2, "source called once on handle()")
end

do
  local opts = {
    entries = function() return { "alpha", "beta", "gamma" } end,
  }
  local matches = picker.filter(opts, opts.entries(), "be")
  assert_eq(#matches, 1, "default filter substring match")
  assert_eq(matches[1], "beta", "filter matched beta")
end

do
  local opts = {
    entries = function() return { 1, 2, 3, 4, 5 } end,
    filter = function(entries, q)
      local out = {}
      for _, e in ipairs(entries) do
        if e > tonumber(q or "0") then out[#out + 1] = e end
      end
      return out
    end,
  }
  local matches = picker.filter(opts, opts.entries(), "2")
  assert_eq(#matches, 3, "custom filter returned 3, 4, 5")
end

do
  local opts = {
    state = { cursor = 1 },
    entries = function() return { "a", "b", "c" } end,
  }
  local r = picker.handle(opts, { kind = "key.down" })
  assert_true(r ~= nil, "down consumed")
  assert_eq(r.state.cursor, 2, "cursor advanced to 2")
  opts.state = { cursor = 2 }
  r = picker.handle(opts, { kind = "key.up" })
  assert_eq(r.state.cursor, 1, "cursor back to 1")
end

do
  local selected
  local opts = {
    state = { cursor = 2 },
    entries = function() return { "a", "b", "c" } end,
    on_select = function(e) selected = e; return { kind = "picked", entry = e } end,
  }
  local r = picker.handle(opts, { kind = "key.enter" })
  assert_eq(selected, "b", "on_select got the cursor entry")
  assert_eq(r.result.kind, "picked", "result from on_select returned")
  assert_eq(r.selected, "b", "selected field set")
end

do
  local cancelled = false
  local opts = {
    state = { cursor = 1 },
    entries = function() return { "a" } end,
    on_cancel = function() cancelled = true; return { kind = "cancel" } end,
  }
  local r = picker.handle(opts, { kind = "key.escape" })
  assert_true(cancelled, "on_cancel ran")
  assert_true(r.cancelled, "cancelled flag set")
end

do
  local opts = {
    state = { cursor = 1, query = "" },
    entries = function() return { "alpha", "beta" } end,
  }
  local r = picker.handle(opts, { kind = "key.a" })
  assert_eq(r.state.query, "a", "query accumulated 'a'")
  assert_eq(r.state.cursor, 1, "cursor reset on filter change")
end

do
  local tree = picker.mount_in_popup({
    state = { cursor = 1 },
    entries = function() return { "x", "y" } end,
    border_style = { fg = "#fff" },
    width = "60%",
    height = "60%",
  })
  assert_eq(tree.__kind, "anchored", "mount_in_popup wraps in anchored")
end

-- prompt
local prompt = require("nefor-tui.widget.prompt")

do
  local tree = prompt.view({
    state = { value = "hello" },
    focused = true,
    border_style = { fg = "#fff" },
  })
  assert_true(tree ~= nil, "prompt renders")
  local input = find_child(tree, "text_input")
  assert_true(input ~= nil, "prompt contains text_input")
  assert_eq(input.value, "hello", "value forwarded to text_input")
end

do
  local source_called_with = nil
  local opts = {
    state = { value = "/new", completion = nil },
    on_change = "prompt.changed",
    completions = {
      { trigger = "/", anchor = "start",
        source = function(body) source_called_with = body; return { { name = "new" }, { name = "next" } } end,
      },
    },
  }
  local r = prompt.handle(opts, { kind = "prompt.changed", value = "/ne" })
  assert_true(r ~= nil, "input.changed consumed")
  assert_eq(source_called_with, "ne", "completion source receives trigger body")
  assert_true(r.state.completion ~= nil, "completion opened")
end

do
  local opts = {
    state = { value = "" },
    completions = {
      { trigger = "/", anchor = "start",
        source = function() return { { name = "new" }, { name = "model" }, { name = "help" } } end,
      },
    },
  }
  local r = prompt.handle(opts, { kind = "prompt.changed", value = "/m" })
  local c = r.state.completion
  assert_eq(#c.matches, 1, "filter narrowed to one match")
  assert_eq(c.matches[1].name, "model", "model matched /m prefix")
end

do
  local opts = {
    state = { value = "" },
    completions = {
      { trigger = "/", anchor = "start",
        source = function() return { "alpha", "beta", "gamma" } end,
        filter = function(entries, query)
          local out = {}
          for _, e in ipairs(entries) do
            if e:find(query, 1, true) ~= nil then out[#out + 1] = e end
          end
          return out
        end,
      },
    },
  }
  local r = prompt.handle(opts, { kind = "prompt.changed", value = "/et" })
  local c = r.state.completion
  assert_eq(#c.matches, 1, "custom filter ran")
  assert_eq(c.matches[1], "beta", "matched beta")
end

do
  local opts = {
    state = {
      value = "/",
      completion = {
        trigger = "/", anchor = "start",
        matches = { { name = "a" }, { name = "b" }, { name = "c" } },
        cursor = 1, body = "",
      },
    },
  }
  local r = prompt.handle(opts, { kind = "key.down" })
  assert_eq(r.state.completion.cursor, 2, "down advanced cursor")
end

do
  local util = require("nefor-tui.util")
  local opts = {
    state = {
      value = "/",
      completion = {
        trigger = "/", anchor = "start",
        matches = { { name = "a" } }, cursor = 1,
      },
    },
  }
  local r = prompt.handle(opts, { kind = "key.escape" })
  assert_eq(r.state.completion, util.NIL, "completion cleared by Esc")
end

do
  local opts = {
    state = { value = "", history_cursor = nil },
    history = { "newest", "older", "oldest" },
  }
  local r = prompt.handle(opts, { kind = "key.up" })
  assert_eq(r.state.value, "newest", "up loaded newest")
  assert_eq(r.state.history_cursor, 1, "history_cursor at 1")
end

do
  local util = require("nefor-tui.util")
  local opts = {
    state = { value = "newest", history_cursor = 1 },
    history = { "newest", "older" },
  }
  local r = prompt.handle(opts, { kind = "key.down" })
  assert_eq(r.state.value, "", "down past newest cleared")
  assert_eq(r.state.history_cursor, util.NIL, "history_cursor cleared")
end

do
  local opts = {
    state = { value = "", history_cursor = nil },
    history = function() return { "from-fn" } end,
  }
  local r = prompt.handle(opts, { kind = "key.up" })
  assert_eq(r.state.value, "from-fn", "history function consulted")
end

do
  local opts = {}
  local state = {
    value = "/mo",
    completion = {
      trigger = "/", anchor = "start",
      matches = { { name = "model" } }, cursor = 1, body = "mo",
    },
  }
  local nv = prompt.apply_completion(opts, state)
  assert_eq(nv, "/model", "default apply replaces whole input")
end

do
  local opts = {}
  local state = {
    value = "@src/m",
    completion = {
      trigger = "@", anchor = "word",
      matches = { { name = "main.lua", is_dir = false } }, cursor = 1,
      body = "src/m", token = "@src/m",
      apply = function(entry, body, value, token)
        local idx = (value:find(token, 1, true) or 1) - 1
        return value:sub(1, idx) .. "@src/" .. entry.name
      end,
    },
  }
  local nv = prompt.apply_completion(opts, state)
  assert_eq(nv, "@src/main.lua", "custom apply ran")
end

-- chat
local chat = require("nefor-tui.widget.chat")

do
  local calls = 0
  local opts = {
    entries = function() calls = calls + 1; return { { text = "a", v = 1 } } end,
    render_entry = function(e) return tui.text { content = e.text } end,
  }
  chat.view(opts)
  chat.view(opts)
  assert_eq(calls, 2, "entries() called on each view()")
end

do
  local received
  local opts = {
    entries = function() return { { text = "x", v = 1 }, { text = "y", v = 2 } } end,
    context = { expanded = true },
    render_entry = function(e, i, ctx)
      if i == 2 then received = { e = e, i = i, ctx = ctx } end
      return tui.text { content = e.text }
    end,
  }
  chat.view(opts)
  assert_eq(received.i, 2, "render_entry got index")
  assert_eq(received.ctx.expanded, true, "render_entry got context")
end

do
  reset_scroll_calls()
  local opts = {
    key = "transcript",
    entries = function() return {} end,
    render_entry = function() return tui.text {} end,
  }
  chat.handle(opts, { kind = "key.pageup" })
  chat.handle(opts, { kind = "key.end" })
  assert_eq(#scroll_calls, 2, "two scroll calls captured")
  assert_eq(scroll_calls[1].fn, "scroll_by", "pageup → scroll_by")
  assert_eq(scroll_calls[1].key, "transcript", "scroll_by used widget's key")
  assert_eq(scroll_calls[1].delta, -10, "pageup delta = -10")
  assert_eq(scroll_calls[2].fn, "scroll_into_view", "end → scroll_into_view")
end

do
  local opts = {
    entries = function() return {} end,
    render_entry = function() return tui.text {} end,
    empty_view = function() return tui.text { content = "empty!" } end,
  }
  local tree = chat.view(opts)
  assert_eq(tree.__kind, "stack", "empty view wraps in stack")
end

do
  reset_scroll_calls()
  chat.scroll_to_end({ key = "transcript" })
  assert_eq(scroll_calls[1].fn, "scroll_into_view", "scroll_to_end → scroll_into_view")
  assert_eq(scroll_calls[1].key, "transcript", "uses widget key")
end

-- text_pane
local text_pane = require("nefor-tui.widget.text_pane")

do
  local tree = text_pane.view({ content = "hello" })
  assert_eq(tree.__kind, "scrollable", "text_pane scrollable by default")
  local text = find_child(tree, "text")
  assert_eq(text.content, "hello", "string content wrapped in tui.text")
end

do
  local tree = text_pane.view({
    content = tui.column { children = { tui.text { content = "x" } } },
  })
  local col = find_child(tree, "column")
  assert_true(col ~= nil, "column passed through")
end

do
  local calls = 0
  local tree = text_pane.view({
    content = function() calls = calls + 1; return "computed" end,
  })
  assert_eq(calls, 1, "content fn called on view()")
  local text = find_child(tree, "text")
  assert_eq(text.content, "computed", "fn result wrapped")
end

do
  local tree = text_pane.view({ content = "static", scrollable = false })
  assert_eq(tree.__kind, "text", "non-scrollable returns leaf")
end

do
  reset_scroll_calls()
  text_pane.handle({ key = "info-pane" }, { kind = "key.pagedown" })
  assert_eq(scroll_calls[1].key, "info-pane", "routed to widget's key")
  assert_eq(scroll_calls[1].delta, 10, "pagedown delta = 10")
end

-- util input validation
local util = require("nefor-tui.util")

do
  local ok, err = pcall(util.resolve_content, 42)
  assert_true(not ok, "resolve_content rejects numbers")
  assert_true(err:find("unsupported content type") ~= nil,
    "resolve_content error names the issue")
end

do
  local ok = pcall(util.resolve_content, true)
  assert_true(not ok, "resolve_content rejects booleans")
end

do
  -- shallow_merge is an internal helper between widget modules; it
  -- trusts the contract and does not assert on bad input. It returns
  -- an empty table when either side is nil/non-table-iterable.
  local out = util.shallow_merge({ a = 1 }, { b = 2 })
  assert_eq(out.a, 1, "shallow_merge a preserved")
  assert_eq(out.b, 2, "shallow_merge b added")
end

do
  local out = util.shallow_merge({ a = 1, b = 2 }, { b = util.NIL })
  assert_eq(out.a, 1, "shallow_merge unrelated key preserved")
  assert_eq(out.b, nil, "shallow_merge NIL sentinel clears")
end
