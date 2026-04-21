-- starter/init.lua
--
-- Reference config for the nefor MVP. Copy to ~/.config/nefor/init.lua (or
-- wherever NEFOR_APPNAME points) and adapt to taste. This file has no
-- architectural privilege — it is a regular user config that composes the
-- primitives the binary exposes (`nefor.events`, `nefor.ui`, `nefor.log`,
-- `nefor.process`, `nefor.concurrency.sleep`) with the `mock-plugin` plugin.
--
-- What it wires up:
--   * Loads the mock-plugin plugin from the nefor monorepo's plugins/ dir.
--   * Creates a single Claude Code session (general-purpose; no personas).
--   * Renders a three-region chat TUI:
--       top    — title bar (1 row)
--       center — scrolling transcript
--       bottom — "> <draft>" input line + hint (2 rows)
--   * Handles keys via the raw `key` event bus so we get every printable
--     char, Enter, Backspace, etc. `nefor.ui.subscribe_key` uses a small
--     pattern grammar that's great for named keys / chords but awkward for
--     a free-form text input — the raw event fits better here.

-------------------------------------------------------------------------
-- 1. Plugin load path
-------------------------------------------------------------------------
-- We point Lua's require() at the monorepo's plugins/ directory. Adjust
-- this if you cloned nefor elsewhere, or set NEFOR_PLUGINS in your shell
-- env before launching nefor.
--
-- With PLUGINS_ROOT = ".../plugins":
--   require("mock-plugin")            -> plugins/mock-plugin/init.lua
--     via the "<root>/?/init.lua" entry below
--   require("mock-plugin.lua.session") -> plugins/mock-plugin/lua/session.lua
--     via the "<root>/?.lua" entry (Lua converts dots to path separators)
local PLUGINS_ROOT = os.getenv("NEFOR_PLUGINS")
  or (os.getenv("HOME") .. "/Vault/Projects/personal/nefor/plugins")

package.path = table.concat({
  PLUGINS_ROOT .. "/?.lua",
  PLUGINS_ROOT .. "/?/init.lua",
  package.path,
}, ";")

local cc = require("mock-plugin")

-------------------------------------------------------------------------
-- 2. UI state
-------------------------------------------------------------------------
-- All UI state is just plain Lua locals. The widget renderers close over
-- them and return fresh line arrays each frame. ratatui (via the LuaWidget
-- wrapper in Rust) calls the renderer once per draw and treats the result
-- as a Paragraph.
local transcript = {}          -- list of strings, oldest first
local draft = ""               -- current input buffer
local session_id_display = "(new session)"

-- Cap the transcript so long sessions don't balloon memory. Dropping from
-- the front is O(n) but n is tiny by construction (500 lines max).
local MAX_TRANSCRIPT_LINES = 500

local function push(line)
  transcript[#transcript + 1] = line
  while #transcript > MAX_TRANSCRIPT_LINES do
    table.remove(transcript, 1)
  end
end

-------------------------------------------------------------------------
-- 3. Widgets
-------------------------------------------------------------------------
-- Each renderer returns a list of strings that ratatui joins with \n and
-- draws as a Paragraph. We don't know the frame dimensions here; for the
-- MVP we let ratatui truncate. Scrollback controls are a post-MVP plugin.

-- Top: one-row title bar showing the session id once it's known.
nefor.ui.register_widget({ kind = "top", size = 1 }, function()
  return { "nefor - claude code chat - " .. session_id_display }
end)

-- Center: the whole transcript. The Lua widget renderer in nefor tail-aligns
-- the list when it overflows the area — the newest line stays pinned to the
-- bottom of the rect and older lines scroll off the top. Manual scrollback
-- (PgUp/PgDn) is a future plugin; for MVP auto-scroll is all we need.
nefor.ui.register_widget({ kind = "center" }, function()
  return transcript
end)

-- Bottom: two stacked widgets.
--   * Hint — pinned to the very last row (Bottom, size 1). Registered first
--     so the `bottom` layout carves the outermost slot for it.
--   * Input — the live `> <draft>|` line. Registered with `kind="bottom"`
--     and no `size`, which nefor treats as auto-height: the widget's
--     `measure(width)` reports `ceil_wrap(draft)` rows each frame, so the
--     input grows from 1 row to 2/3/… as the draft wraps. The `|` fakes a
--     terminal cursor because the current binding can't position one.
nefor.ui.register_widget({ kind = "bottom", size = 1 }, function()
  return { "Ctrl-C to quit - Enter to send" }
end)

nefor.ui.register_widget({ kind = "bottom" }, function()
  return { "> " .. draft .. "|" }
end)

push("[info] starter/init.lua loaded. type a prompt and press Enter.")

-------------------------------------------------------------------------
-- 4. Claude Code session
-------------------------------------------------------------------------
-- One general-purpose session for the whole TUI lifetime. Each callback
-- mutates transcript state and relies on the once-per-frame redraw to
-- surface changes. No explicit invalidation needed.
local session              -- forward decl; referenced from callbacks below

session = cc.session.new({
  -- `bypassPermissions` lets Claude run tools without per-call approval
  -- prompts — nefor has no permission-gate UI yet (that's post-MVP).
  -- Tighten to "default" or "acceptEdits" once you trust your setup
  -- less; "plan" is Claude's read-only planning mode.
  permission_mode = "bypassPermissions",

  -- Stream deltas arrive a few tokens at a time. We coalesce them into a
  -- single "[assistant] ..." line so the transcript doesn't explode with
  -- one line per token. The prefix check is cheap and unambiguous because
  -- nothing else the UI emits starts with "[assistant] ".
  on_message_delta = function(text)
    local last = transcript[#transcript]
    if last and last:sub(1, 12) == "[assistant] " then
      transcript[#transcript] = last .. text
    else
      push("[assistant] " .. text)
    end
  end,

  -- Claude is about to run a tool. We show the tool name + a short hint
  -- drawn from the tool's input (Bash has `command`, most file tools have
  -- `file_path`, etc.). Best-effort — unknown tools just show the name.
  on_tool_start = function(tool_name, tool_input)
    local hint = ""
    if type(tool_input) == "table" then
      hint = tool_input.command
        or tool_input.file_path
        or tool_input.description
        or tool_input.pattern
        or ""
    end
    push("[tool " .. tool_name .. "] " .. tostring(hint))
  end,

  -- Fires once per turn when Claude signals result:success. `info` carries
  -- the timing summary from the CC stream. We skip the cost field — on a
  -- Claude Max subscription CC frequently reports $0 and the number is
  -- meaningless; `/cost` in a CC session is the source of truth when you do
  -- want the billing picture.
  on_turn_done = function(_final_text, info)
    push(string.format(
      "[done] %dms - %d turn(s)",
      info and info.duration_ms or 0,
      info and info.num_turns or 0
    ))
    session_id_display = (session and session.id()) or session_id_display
  end,

  on_turn_error = function(message)
    push("[error] " .. tostring(message))
  end,
})

-------------------------------------------------------------------------
-- 5. Key handling
-------------------------------------------------------------------------
-- The `key` event payload shape (see crates/nefor/src/lua/bindings.rs,
-- payload_to_lua + describe_key_code) is:
--   {
--     code = "Char" | "Enter" | "Backspace" | "Esc" | "Up" | ...,
--     char = "a",            -- present iff code == "Char"
--     f = 1..12,             -- present iff code == "F" (function keys)
--     modifiers = { ctrl = bool, shift = bool, alt = bool },
--   }
-- We only care about Char / Enter / Backspace for the MVP chat loop.
nefor.events.on("key", function(ev)
  if type(ev) ~= "table" then return end
  local mods = ev.modifiers or {}

  if ev.code == "Enter" then
    -- Submit. Empty drafts are a silent no-op so a stray Enter doesn't
    -- emit a blank turn to Claude (CC rejects empty prompts anyway).
    if draft == "" then return end
    local prompt = draft
    draft = ""
    push("> " .. prompt)
    session.run(prompt)

  elseif ev.code == "Backspace" then
    -- UTF-8-safe erase would need a codepoint walk; for ASCII input the
    -- byte-level strip is correct, and multibyte input in a plain Lua
    -- string will just leave a truncated sequence that ratatui renders
    -- as a replacement char. Good enough for MVP.
    draft = draft:sub(1, -2)

  elseif ev.code == "Char" and ev.char then
    -- Filter out control/alt chords so Ctrl-L or Alt-x don't get typed
    -- literally. Shift is allowed so capital letters and shifted
    -- punctuation work normally.
    if not mods.ctrl and not mods.alt then
      draft = draft .. ev.char
    end
  end
  -- All other keys (Esc, arrows, F-keys, ...) are currently ignored.
  -- Easy extension point: history recall on Up/Down, clear-line on Esc.
end)
