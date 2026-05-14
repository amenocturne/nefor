-- Per-entry rendering for the transcript. Entries are tagged by `kind`
-- and `role`; this module owns the visual shape of each variant and
-- the small helpers (tool_salient, format_submitted_at,
-- humanize_duration_ms) the renderer pulls in.

local common = require("chat.common")

local C        = common.C
local STYLE    = common.STYLE
local md       = common.md
local compact  = common.compact
local pad_block = common.pad_block
local pretty_json = common.pretty_json
local format_graph = common.format_graph
local format_spawn_graph_output = common.format_spawn_graph_output
local humanize_duration_ms = common.humanize_duration_ms
local bordered_box = common.bordered_box

local M = {}

-- User entry: full-width bordered block in user blue. Body stays in
-- default fg.
local function render_user_entry(entry)
  return bordered_box(
    tui.text { content = entry.text or "", wrap = "word" },
    STYLE.user_chrome
  )
end

-- Reasoning rows above the assistant body. Three visual states:
--   live  (streaming + body empty)  → "▼ thinking…"  + body
--   expanded (Ctrl+O)               → "▼ reasoning"  + body
--   collapsed                       → "▸ reasoning (Ns)"
local function reasoning_rows(reasoning, body_empty, expanded)
  if reasoning == nil or (reasoning.text or "") == "" then return nil end
  local live = reasoning.streaming and body_empty
  if live or expanded then
    local header_text = live and "▼ thinking…" or "▼ reasoning"
    return tui.column {
      gap = 0,
      children = {
        tui.text { content = header_text, style = STYLE.footer, wrap = "none" },
        tui.text { content = "  " .. (reasoning.text or ""), style = STYLE.reasoning, wrap = "word" },
      },
    }
  end
  local dur = humanize_duration_ms(reasoning.duration_ms)
  local label = dur and ("▸ reasoning (" .. dur .. ")") or "▸ reasoning"
  return tui.text { content = label, style = STYLE.footer, wrap = "none" }
end

-- Per-turn footer: "▣ <model> · <duration>".
local function turn_footer(entry)
  local model = entry.model
  local dur = humanize_duration_ms(entry.duration_ms)
  if model and dur then
    return tui.text { content = "▣ " .. model .. " · " .. dur, style = STYLE.footer, wrap = "none" }
  elseif model then
    return tui.text { content = "▣ " .. model, style = STYLE.footer, wrap = "none" }
  elseif dur then
    return tui.text { content = "▣ " .. dur, style = STYLE.footer, wrap = "none" }
  end
  return nil
end

local function render_assistant_entry(entry, expanded)
  local body_empty = (entry.text or "") == ""
  local rows = compact {
    reasoning_rows(entry.reasoning, body_empty, expanded),
    body_empty and nil or md(entry.text),
    (not entry.streaming) and turn_footer(entry) or nil,
  }
  return tui.column { gap = 0, children = rows }
end

-- Salient input summary for the tool collapsed-line.
local function tool_salient(entry)
  local name = entry.name or ""
  local input = entry.input_table or {}
  if name == "Bash" or name == "bash" then return input.command end
  if name == "Read" or name == "Edit" or name == "Write" or name == "MultiEdit" then
    return input.file_path
  end
  if name == "read_file" or name == "write_file" then return input.path end
  if name == "Grep" or name == "Glob" then return input.pattern end
  if name == "spawn_graph" then
    local nodes = input.graph and input.graph.nodes or nil
    if type(nodes) == "table" then
      local n = #nodes
      if n == 1 then return "1 node" end
      return tostring(n) .. " nodes"
    end
  end
  -- Fall back: first short string field, skipping policy-ish names.
  for k, v in pairs(input) do
    local skip = (k == "on_node_failure" or k == "mode" or k == "policy" or k == "strategy")
    if (not skip) and type(v) == "string" then return v end
  end
  return nil
end

local function tool_collapsed(entry)
  local glyph = "▸ "
  local header_style = entry.error and STYLE.tool_error or STYLE.tool_name
  local salient = tool_salient(entry)
  local header = glyph .. (entry.name or "?")
  if salient then
    local trimmed = salient
    if #trimmed > 80 then trimmed = trimmed:sub(1, 77) .. "..." end
    header = header .. "(" .. trimmed .. ")"
  end
  if entry.output == nil and not entry.error then
    header = header .. " …"  -- running indicator
  end
  local rows = { tui.text { content = header, style = header_style, wrap = "none" } }
  if entry.error then
    rows[#rows + 1] = tui.text { content = "  error", style = STYLE.status_danger, wrap = "none" }
  end
  return tui.column { gap = 0, children = rows }
end

local function tool_expanded(entry)
  local glyph = "▼ "
  local header_style = entry.error and STYLE.tool_error or STYLE.tool_name
  local salient = tool_salient(entry)
  local header = glyph .. (entry.name or "?")
  if salient then
    local trimmed = salient
    if #trimmed > 80 then trimmed = trimmed:sub(1, 77) .. "..." end
    header = header .. "(" .. trimmed .. ")"
  end
  local rows = { tui.text { content = header, style = header_style, wrap = "none" } }
  rows[#rows + 1] = tui.text { content = "  input:",  style = STYLE.footer, wrap = "none" }
  -- spawn_graph: render as compact node-list + edge-list. Args of each
  -- node are intentionally omitted (would clutter); future "focus a
  -- node" UI surfaces them on demand. For everything else, prefer JSON
  -- pretty-print of the structured input_table; fall back to the raw
  -- string when only a string was sent.
  local input_text
  if entry.name == "spawn_graph" and entry.input_table
    and type(entry.input_table.graph) == "table" then
    input_text = format_graph(entry.input_table.graph)
  elseif entry.input_table ~= nil then
    input_text = pretty_json(entry.input_table)
  elseif entry.input and #entry.input > 0 and entry.input ~= "(object)" then
    input_text = entry.input
  end
  if input_text and #input_text > 0 then
    -- 2-space indent each line so the body sits inset from the bullet
    -- column and the dark background reads as a single block. Pad each
    -- line to max width post-indent so the bg renders as a rectangle.
    local indented = "  " .. input_text:gsub("\n", "\n  ")
    rows[#rows + 1] = tui.text {
      content = pad_block(indented),
      style = { fg = C.md_code_fg, bg = C.md_code_block_bg },
      wrap = "none",
    }
  end
  if entry.output == nil and not entry.error then
    rows[#rows + 1] = tui.text { content = "  running...", style = STYLE.footer, wrap = "none" }
  else
    -- Label the trailing block by terminal status. `error:` (red) for
    -- the deny / policy / unknown-tool / timeout paths so the block
    -- reads as a denial rather than an empty `output:`. The tool-gate
    -- wrapper puts the error message into the `output` field so it
    -- lands here instead of being dropped on the floor.
    local label, label_style
    if entry.error then
      label, label_style = "  error:", STYLE.status_danger
    else
      label, label_style = "  output:", STYLE.footer
    end
    rows[#rows + 1] = tui.text { content = label, style = label_style, wrap = "none" }
    if entry.output and #entry.output > 0 then
      local out_text = entry.output
      if entry.name == "spawn_graph" then
        out_text = format_spawn_graph_output(out_text)
      end
      local indented_out = "  " .. out_text:gsub("\n", "\n  ")
      rows[#rows + 1] = tui.text {
        content = pad_block(indented_out),
        style = { fg = C.md_code_fg, bg = C.md_code_block_bg },
        wrap = "none",
      }
    end
  end
  return tui.column { gap = 0, children = rows }
end

-- Plan entries carry a `submitted_at` timestamp the lead-workflow
-- actor stamps when the write-review tool fires. Render as "HH:MM"
-- for the plan-box subtitle. Accepts ISO 8601 strings and epoch-ms
-- numbers; anything else stringifies as-is so a malformed value
-- doesn't crash the surface.
local function format_submitted_at(s)
  if type(s) == "number" then
    return os.date("!%H:%M", math.floor(s / 1000))
  end
  if type(s) ~= "string" then return tostring(s) end
  local hh, mm = s:match("T(%d%d):(%d%d)")
  if hh ~= nil then return hh .. ":" .. mm end
  return s
end

-- Plan entry: full-width yellow-bordered block carrying a write-review
-- plan the lead-workflow actor submitted. Render-only — the model
-- already saw the plan via the tool call's args, so chat.lua does NOT
-- forward the body into model context. Status drives the border style:
-- pending = yellow active, approved = yellow italic with green check
-- subtitle, rejected = red strikethrough with red status subtitle.
-- The hint row only renders for `pending`.
local function render_plan_entry(entry)
  local status = entry.status or "pending"
  local border_style
  if status == "approved" then
    border_style = STYLE.plan_chrome_approved
  elseif status == "rejected" then
    border_style = STYLE.plan_chrome_rejected
  else
    border_style = STYLE.plan_chrome
  end

  local subtitle_text = "plan"
  local stamped = format_submitted_at(entry.submitted_at)
  if stamped and stamped ~= "" then
    subtitle_text = subtitle_text .. " · submitted at " .. stamped
  end

  local rows = {
    tui.text { content = subtitle_text, style = STYLE.plan_subtitle, wrap = "none" },
    md(entry.text or ""),
  }

  if status == "pending" then
    rows[#rows + 1] = tui.text {
      content = "[/approve to proceed | /reject <reason> to send back]",
      style   = STYLE.plan_hint,
      wrap    = "word",
    }
  elseif status == "approved" then
    rows[#rows + 1] = tui.text {
      content = "✓ approved",
      style   = STYLE.plan_status_approved,
      wrap    = "none",
    }
  elseif status == "rejected" then
    rows[#rows + 1] = tui.text {
      content = "✗ rejected",
      style   = STYLE.plan_status_rejected,
      wrap    = "none",
    }
  end

  return bordered_box(
    tui.column { gap = 0, children = rows },
    border_style
  )
end

function M.render(entry, _i, expanded)
  if entry.kind == "tool_call" then
    if expanded then return tool_expanded(entry) end
    return tool_collapsed(entry)
  end
  if entry.kind == "plan" then
    return render_plan_entry(entry)
  end
  if entry.role == "assistant" or entry.kind == "stream" then
    return render_assistant_entry(entry, expanded)
  end
  if entry.role == "user" then
    return render_user_entry(entry)
  end
  if entry.role == "system" then
    return tui.text {
      content = "[" .. (entry.text or "") .. "]",
      style   = STYLE.system,
      wrap    = "word",
    }
  end
  return tui.text { content = entry.text or "", wrap = "word" }
end

return M
