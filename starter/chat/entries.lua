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

local function truncate_salient(s, max)
  if not s or #s <= max then return s end
  local is_path = s:find("/", 1, true)
  if is_path then
    local tail = s:sub(-(max - 1))
    local slash = tail:find("/", 1, true)
    if slash then tail = tail:sub(slash) end
    return "…" .. tail
  end
  return s:sub(1, max - 3) .. "..."
end

-- User entry: full-width bordered block in user blue. Body stays in
-- default fg.
local function render_user_entry(entry, queued)
  local chrome = queued and STYLE.user_chrome_queued or STYLE.user_chrome
  return bordered_box(
    tui.text { content = entry.text or "", wrap = "word" },
    chrome
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

-- Stable three-slot layout: each slot is a keyed column so the
-- reconciler matches by key across frames. When a slot is empty its
-- column has zero children (zero height, no gap consumed). This
-- prevents child-count changes from cascading position-based key
-- mismatches during streaming transitions.
local function render_assistant_entry(entry, expanded)
  local body_empty = (entry.text or "") == ""
  local reason = reasoning_rows(entry.reasoning, body_empty, expanded)
  local body   = (not body_empty) and md(entry.text) or nil
  local foot   = (not entry.streaming) and turn_footer(entry) or nil
  return tui.column { gap = 0, children = {
    tui.column { key = "reason", gap = 0, children = reason and { reason } or {} },
    tui.column { key = "body",   gap = 0, children = body   and { body }   or {} },
    tui.column { key = "foot",   gap = 0, children = foot   and { foot }   or {} },
  } }
end

-- Salient input summary for the tool collapsed-line.
local function tool_salient(entry)
  local name = entry.name or ""
  local input = entry.input_table or {}
  if name == "Bash" or name == "bash" then return input.command end
  if name == "Read" or name == "Edit" or name == "Write" or name == "MultiEdit" then
    return input.file_path
  end
  if name == "read_file" or name == "edit_file" or name == "write_file" then return input.path end
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
    header = header .. "(" .. truncate_salient(salient, 80) .. ")"
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
    header = header .. "(" .. truncate_salient(salient, 80) .. ")"
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

-- Two-column node list (id, role). The id column pads to the widest
-- id so the role column lines up for scan-ability. Empty when the
-- envelope carried no nodes (e.g. malformed graph) — caller decides
-- whether to render the section at all.
local function graph_result_nodes_block(nodes)
  if type(nodes) ~= "table" or #nodes == 0 then return nil end
  local widest = 0
  for _, n in ipairs(nodes) do
    local id = tostring(n.id or "")
    if #id > widest then widest = #id end
  end
  local lines = { "nodes:" }
  for _, n in ipairs(nodes) do
    local id   = tostring(n.id or "")
    local role = tostring(n.role or "")
    local pad  = widest - #id
    lines[#lines + 1] = "  " .. id .. string.rep(" ", pad) .. "  " .. role
  end
  return "  " .. table.concat(lines, "\n  ")
end

local function graph_result_header(entry, glyph)
  local failed = (entry.status == "failed")
  local style = failed and STYLE.graph_result_error or STYLE.graph_result_name
  local run_id = tostring(entry.run_id or "?")
  local node_count = (type(entry.nodes) == "table") and #entry.nodes or 0
  local header
  if failed then
    header = glyph .. "graph(run_id=" .. run_id .. ") FAILED"
  else
    local nlabel
    if node_count == 1 then nlabel = "1 node"
    else nlabel = tostring(node_count) .. " nodes" end
    header = glyph .. "graph(run_id=" .. run_id .. ", " .. nlabel .. ")"
  end
  return tui.text { content = header, style = style, wrap = "none" }
end

local function graph_result_collapsed(entry)
  return tui.column { gap = 0, children = { graph_result_header(entry, "◆ ") } }
end

local function graph_result_expanded(entry)
  local rows = { graph_result_header(entry, "◇ ") }
  local nodes_block = graph_result_nodes_block(entry.nodes)
  if nodes_block then
    rows[#rows + 1] = tui.text {
      content = pad_block(nodes_block),
      style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
      wrap    = "none",
    }
  end
  local failed = (entry.status == "failed")
  if failed then
    rows[#rows + 1] = tui.text { content = "  error:", style = STYLE.status_danger, wrap = "none" }
    local err = entry.error
    if type(err) == "string" and #err > 0 then
      local indented = "  " .. err:gsub("\n", "\n  ")
      rows[#rows + 1] = tui.text {
        content = pad_block(indented),
        style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
        wrap    = "none",
      }
    end
  else
    rows[#rows + 1] = tui.text { content = "  output:", style = STYLE.footer, wrap = "none" }
    local out = entry.output
    if type(out) == "string" and #out > 0 then
      local indented = "  " .. out:gsub("\n", "\n  ")
      rows[#rows + 1] = tui.text {
        content = pad_block(indented),
        style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
        wrap    = "none",
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

-- AGENTS.md auto-load: tool-gate emits a system message when a path-
-- touching tool call lands in a directory with an AGENTS.md. The text
-- carries project guidance for the model but is noisy in the chat
-- surface — render it as a foldable block keyed on the path (parallels
-- the tool_call collapsed/expanded shape).
local function agents_md_collapsed(entry)
  local path = entry.path or "AGENTS.md"
  return tui.column { gap = 0, children = {
    tui.text {
      content = "▸ AGENTS.md(" .. path .. ")",
      style   = STYLE.footer,
      wrap    = "none",
    },
  } }
end

local function agents_md_expanded(entry)
  local path = entry.path or "AGENTS.md"
  local rows = { tui.text {
    content = "▼ AGENTS.md(" .. path .. ")",
    style   = STYLE.footer,
    wrap    = "none",
  } }
  local body = entry.text or ""
  if #body > 0 then
    local indented = "  " .. body:gsub("\n", "\n  ")
    rows[#rows + 1] = tui.text {
      content = pad_block(indented),
      style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
      wrap    = "none",
    }
  end
  return tui.column { gap = 0, children = rows }
end

local function compaction_label(entry, glyph)
  local parts = { glyph .. "context compacted" }
  if type(entry.trigger) == "string" and #entry.trigger > 0 then
    parts[#parts + 1] = entry.trigger
  end
  local provider_model = nil
  if type(entry.provider) == "string" and #entry.provider > 0
      and type(entry.model) == "string" and #entry.model > 0 then
    provider_model = entry.provider .. "/" .. entry.model
  elseif type(entry.provider) == "string" and #entry.provider > 0 then
    provider_model = entry.provider
  elseif type(entry.model) == "string" and #entry.model > 0 then
    provider_model = entry.model
  end
  if provider_model ~= nil then parts[#parts + 1] = provider_model end
  return table.concat(parts, " · ")
end

local function compaction_collapsed(entry)
  local rows = {
    tui.text {
      content = compaction_label(entry, "▸ "),
      style   = STYLE.system,
      wrap    = "none",
    },
  }
  if type(entry.display_summary) == "string" and #entry.display_summary > 0 then
    rows[#rows + 1] = tui.text {
      content = "  " .. entry.display_summary,
      style   = STYLE.footer,
      wrap    = "word",
    }
  end
  return tui.column { gap = 0, children = rows }
end

local function compaction_expanded(entry)
  local rows = {
    tui.text {
      content = compaction_label(entry, "▼ "),
      style   = STYLE.system,
      wrap    = "none",
    },
  }
  if type(entry.display_summary) == "string" and #entry.display_summary > 0 then
    rows[#rows + 1] = tui.text {
      content = "  " .. entry.display_summary,
      style   = STYLE.footer,
      wrap    = "word",
    }
  end
  local artifact = entry.model_context_artifact
  if type(artifact) == "table" then
    local slim = {}
    for k, v in pairs(artifact) do
      if k ~= "items" then slim[k] = v end
    end
    artifact = slim
  end
  local details = {
    strategy = entry.strategy,
    model_context_artifact = artifact,
    metadata = entry.metadata,
  }
  rows[#rows + 1] = tui.text {
    content = pad_block("  " .. pretty_json(details):gsub("\n", "\n  ")),
    style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
    wrap    = "none",
  }
  return tui.column { gap = 0, children = rows }
end

function M.render(entry, _i, expanded, queued)
  if entry.kind == "tool_call" then
    if expanded then return tool_expanded(entry) end
    return tool_collapsed(entry)
  end
  if entry.kind == "graph_result" then
    if expanded then return graph_result_expanded(entry) end
    return graph_result_collapsed(entry)
  end
  if entry.kind == "agents_md" then
    if expanded then return agents_md_expanded(entry) end
    return agents_md_collapsed(entry)
  end
  if entry.kind == "compaction" then
    if expanded then return compaction_expanded(entry) end
    return compaction_collapsed(entry)
  end
  if entry.kind == "plan" then
    return render_plan_entry(entry)
  end
  if entry.role == "assistant" or entry.kind == "stream" then
    return render_assistant_entry(entry, expanded)
  end
  if entry.role == "user" then
    return render_user_entry(entry, queued)
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
