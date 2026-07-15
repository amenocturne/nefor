-- Unit tests for `lua/libs/instruction-files` — the generic instruction-file
-- discovery / reminder mechanism consumed by read-only-tools (the
-- discover_instruction_files tool) and the tool-gate compositor bridge
-- (per-tool-call reminders).
--
-- The stateful primitives are exercised against a throwaway fixture tree
-- built under `mktemp -d` (outside any git repo, so discovery is
-- deterministic and never sees the checkout's own AGENTS.md/CLAUDE.md).

local IF = require("libs.instruction-files")

local function assert_eq(actual, expected, msg)
  if actual ~= expected then
    error(string.format("assertion failed: %s\n  expected: %s\n  actual:   %s",
      msg or "values differ", tostring(expected), tostring(actual)), 2)
  end
end

local function assert_true(cond, msg)
  if not cond then error("assertion failed: " .. (msg or "(no message)"), 2) end
end

local function contains(haystack, needle)
  return type(haystack) == "string" and haystack:find(needle, 1, true) ~= nil
end

-- ── pure path helpers ─────────────────────────────────────────────────
do
  assert_eq(IF._normalise("a//b/./c/"), "a/b/c", "normalise collapses //, /./ and trailing /")
  assert_eq(IF._normalise("./x/y"), "x/y", "normalise strips leading ./")
  assert_eq(IF._parent_dir("/a/b/c"), "/a/b", "parent_dir drops last segment")
  assert_eq(IF._parent_dir("/a"), "/", "parent_dir of top-level is /")
  assert_eq(IF._to_absolute("/already/abs"), "/already/abs", "to_absolute keeps absolute paths")
  assert_eq(IF._to_absolute("rel/path", "/base"), "/base/rel/path", "to_absolute joins base for relatives")
end

-- ── fixture tree ──────────────────────────────────────────────────────
local function sh(cmd)
  local p = io.popen(cmd)
  local out = p and p:read("*l") or nil
  if p then p:close() end
  return out
end

local ROOT = sh("mktemp -d 2>/dev/null")
assert_true(type(ROOT) == "string" and #ROOT > 0, "mktemp produced a fixture root")
ROOT = IF._normalise(ROOT)

local function write_file(path, content)
  local f = assert(io.open(path, "w"))
  f:write(content or "x\n")
  f:close()
end

os.execute("mkdir -p '" .. ROOT .. "/sub'")
os.execute("mkdir -p '" .. ROOT .. "/node_modules/pkg'")
write_file(ROOT .. "/AGENTS.md", "# root agents\n")
write_file(ROOT .. "/sub/CLAUDE.md", "# sub claude\n")
write_file(ROOT .. "/other.txt", "not an instruction file\n")
-- Pruned dir: an instruction file here must NOT be discovered.
write_file(ROOT .. "/node_modules/pkg/AGENTS.md", "# should be pruned\n")

-- ── discover: subfolders scope ────────────────────────────────────────
do
  local S = IF.new()
  local result = S.discover(ROOT, { scope = "subfolders" })
  assert_eq(#result.files, 2, "discovers both instruction files, prunes node_modules")
  assert_eq(result.total, 2, "total counts the two non-pruned files")
  assert_eq(result.status, "all files shown", "status reports all shown")

  local rels = {}
  for _, f in ipairs(result.files) do rels[f.relative_path] = f.status end
  assert_eq(rels["AGENTS.md"], "unread", "root AGENTS.md present and unread")
  assert_eq(rels["sub/CLAUDE.md"], "unread", "nested CLAUDE.md present with relative path")
end

-- ── mark_read + unread_only filtering ─────────────────────────────────
do
  local S = IF.new()
  S.mark_read("lead:session-1", ROOT .. "/AGENTS.md")

  local all = S.discover(ROOT, { scope = "subfolders", principal = "lead:session-1" })
  local status_by_rel = {}
  for _, f in ipairs(all.files) do status_by_rel[f.relative_path] = f.status end
  assert_eq(status_by_rel["AGENTS.md"], "read", "marked file reports read")
  assert_eq(status_by_rel["sub/CLAUDE.md"], "unread", "unmarked file stays unread")

  local unread = S.discover(ROOT, {
    scope = "subfolders", principal = "lead:session-1", unread_only = true,
  })
  assert_eq(#unread.files, 1, "unread_only hides the read file")
  assert_eq(unread.files[1].relative_path, "sub/CLAUDE.md", "only the unread file remains")

  -- Non-instruction paths are ignored by mark_read.
  S.mark_read(nil, ROOT .. "/other.txt")
  local st = S._state("lead:session-1")
  assert_true(st.read_files[IF._normalise(ROOT .. "/other.txt")] == nil,
    "mark_read ignores non-instruction filenames")
end

-- ── format_discovery / format_reminder wording ────────────────────────
do
  local S = IF.new()
  local result = S.discover(ROOT, { scope = "subfolders" })

  local discovery = IF.format_discovery(result)
  assert_true(contains(discovery, "Instruction files for " .. ROOT),
    "format_discovery names the root")
  assert_true(contains(discovery, "unread"), "format_discovery groups unread files")
  assert_true(contains(discovery, "AGENTS.md"), "format_discovery lists a file")

  local reminder = IF.format_reminder(result)
  assert_true(reminder:match("^Local instruction files available"),
    "format_reminder uses the reminder header the chat reducer matches on")
  assert_true(contains(reminder, "Contents are not loaded automatically"),
    "format_reminder tells the agent contents are not auto-loaded")

  assert_eq(IF.format_reminder({ files = {} }), nil,
    "format_reminder returns nil when there is nothing to remind about")
end

-- ── tool-context registry → folder derivation ─────────────────────────
do
  local S = IF.new()
  local n = S.record_tool_contexts_from_advertise({
    tools = {
      { name = "list_dir", context = { folders = { { from = "directory", arg = "path" } } } },
      { name = "no_context" },
    },
  })
  assert_eq(n, 1, "records only tools that carry a context table")

  local folders = S.folders_for_tool_call("list_dir", { path = ROOT })
  assert_eq(#folders, 1, "derives one folder from a directory-arg tool call")
  assert_eq(folders[1], ROOT, "derived folder is the normalised call path")

  assert_eq(#S.folders_for_tool_call("no_context", {}), 0,
    "a tool without context yields no folders")
end

-- ── emit_reminders_for_tool_call: emit once, dedup thereafter ─────────
do
  local S = IF.new()
  S.record_tool_contexts_from_advertise({
    tools = { { name = "list_dir", context = { folders = { { from = "directory", arg = "path" } } } } },
  })

  local emitted = {}
  local emitter = {
    valid = function() return true end,
    principal_key = function() return "lead:session-1" end,
    notice = function(text, opts)
      emitted[#emitted + 1] = { text = text, opts = opts }
      return true
    end,
  }

  local first = S.emit_reminders_for_tool_call("list_dir", { path = ROOT }, emitter)
  assert_eq(first, 1, "first folder-touching call emits one reminder")
  assert_eq(#emitted, 1, "emitter.notice called exactly once")
  assert_true(emitted[1].text:match("^Local instruction files available"),
    "emitted reminder carries the reminder header")
  assert_eq(emitted[1].opts.path, ROOT, "reminder is tagged with the touched root path")

  local second = S.emit_reminders_for_tool_call("list_dir", { path = ROOT }, emitter)
  assert_eq(second, 0, "the same lead session is not reminded twice")
  assert_eq(#emitted, 1, "no additional emission on the deduped call")

  local sibling = {
    valid = function() return true end,
    principal_key = function() return "subagent:session-1:run-2:scout.run-tool" end,
    notice = function(text, opts)
      emitted[#emitted + 1] = { text = text, opts = opts }
      return true
    end,
  }
  assert_eq(S.emit_reminders_for_tool_call("list_dir", { path = ROOT }, sibling), 1,
    "a sibling principal touching the same root has independent reminder state")
  assert_eq(#emitted, 2, "sibling notice is emitted exactly once")

  local same_actor_other_run = {
    valid = function() return true end,
    principal_key = function() return "subagent:session-1:run-3:scout.run-tool" end,
    notice = function(text, opts)
      emitted[#emitted + 1] = { text = text, opts = opts }
      return true
    end,
  }
  assert_eq(S.emit_reminders_for_tool_call(
      "list_dir", { path = ROOT }, same_actor_other_run), 1,
    "identical actor names in different runs remain distinct principals")
  assert_eq(#emitted, 3, "same-named actor in another run gets one notice")

  os.execute("mkdir -p '" .. ROOT .. "/other'")
  write_file(ROOT .. "/other/AGENTS.md", "# other agents\n")
  local concurrent = {
    valid = function() return true end,
    principal_key = function() return "subagent:session-1:run-4:builder.run-tool" end,
    notice = function(text, opts)
      emitted[#emitted + 1] = { text = text, opts = opts }
      return true
    end,
  }
  assert_eq(S.emit_reminders_for_tool_call(
      "list_dir", { path = ROOT .. "/other" }, concurrent), 1,
    "a concurrent principal in a different folder gets its own notice")
  assert_eq(#emitted, 4, "concurrent folder notice is emitted exactly once")

  local invalid = {
    valid = function() return false end,
    principal_key = function() return nil end,
  }
  assert_eq(S.emit_reminders_for_tool_call("list_dir", { path = ROOT }, invalid), 0,
    "missing or malformed provenance fails closed")
  local internal = S._state(nil)
  assert_true(internal.all_reminded_scopes._global == nil,
    "invalid scope never creates a global reminder bucket")
  assert_true(internal.all_read_files._global == nil,
    "invalid scope never creates a global read bucket")
end

os.execute("rm -rf '" .. ROOT .. "'")

print("instruction_files discover_test: all assertions passed")
