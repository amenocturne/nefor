-- Per-entry rendering for the transcript. Entries are tagged by `kind`
-- and `role`; this module owns the visual shape of each variant and
-- the small helpers (tool_salient, format_submitted_at,
-- humanize_duration_ms) the renderer pulls in.

local common = require("libs.chat.common")

local C        = common.C
local STYLE    = common.STYLE
local md       = common.md
local compact  = common.compact
local pad_block = common.pad_block
local pretty_json = common.pretty_json
local humanize_duration_ms = common.humanize_duration_ms
local bordered_box = common.bordered_box

local M = {}

-- User entry: full-width bordered block in user blue. Body stays in
-- default fg.
local function render_user_entry(entry, queued)
  local chrome = queued and STYLE.user_chrome_queued or STYLE.user_chrome
  return bordered_box(
    tui.text { content = entry.text or "", wrap = "word" },
    chrome
  )
end

local function render_error_entry(entry)
  local rows = {
    tui.text {
      content = "✗ " .. (entry.title or "Something went wrong"),
      style = STYLE.status_danger,
      wrap = "word",
    },
    tui.text {
      content = "  " .. (entry.message or "The operation failed."),
      style = STYLE.status,
      wrap = "word",
    },
  }
  if entry.retryable then
    rows[#rows + 1] = tui.text {
      content = "  You can retry the request.",
      style = STYLE.footer,
      wrap = "word",
    }
  end
  return tui.column { gap = 0, children = rows }
end

-- Reasoning rows above the assistant body. Three visual states:
--   live  (streaming + body empty)  → "▼ thinking…"  + body
--   expanded (Ctrl+O)               → "▼ reasoning"  + body
--   collapsed                       → "▸ reasoning (Ns)"
local function indent_block(text, prefix)
  return prefix .. (text or ""):gsub("\n", "\n" .. prefix)
end

local function reasoning_rows(reasoning, body_empty, expanded)
  if reasoning == nil or (reasoning.text or "") == "" then return nil end
  local live = reasoning.streaming and body_empty
  if live or expanded then
    local header_text = live and "▼ thinking…" or "▼ reasoning"
    return tui.column {
      gap = 0,
      children = {
        tui.text { content = header_text, style = STYLE.footer, wrap = "none" },
        tui.text { content = indent_block(reasoning.text, "  "), style = STYLE.reasoning, wrap = "word" },
      },
    }
  end
  local dur = humanize_duration_ms(reasoning.duration_ms)
  local label = dur and ("▸ reasoning (" .. dur .. ")") or "▸ reasoning"
  return tui.text { content = label, style = STYLE.footer, wrap = "none" }
end

-- Per-turn footer: "▣ <model> · <duration> · <speed>".
local function turn_footer(entry)
  local parts = {}
  if entry.model then parts[#parts + 1] = entry.model end
  local dur = humanize_duration_ms(entry.duration_ms)
  if dur then parts[#parts + 1] = dur end
  if entry.output_tokens and entry.duration_ms and entry.duration_ms > 0 then
    local tps = math.floor((entry.output_tokens * 1000) / entry.duration_ms + 0.5)
    parts[#parts + 1] = tostring(tps) .. " tok/s"
  end
  if #parts == 0 then return nil end
  return tui.text {
    content = "▣ " .. table.concat(parts, " · "),
    style = STYLE.footer,
    wrap = "none",
  }
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
local function raw_tool_expanded(entry)
  local rows = { tui.text { content = "▼ " .. (entry.name or "?"), style = entry.error and STYLE.tool_error or STYLE.tool_name, wrap = "none" } }
  rows[#rows + 1] = tui.text { content = "  input:", style = STYLE.footer, wrap = "none" }
  local raw_input = entry.raw_input
  local input_text
  if type(raw_input) == "table" then input_text = pretty_json(raw_input)
  elseif raw_input ~= nil then input_text = tostring(raw_input)
  else input_text = entry.input_table and pretty_json(entry.input_table) or entry.input end
  if input_text and input_text ~= "(object)" and #input_text > 0 then rows[#rows + 1] = tui.text { content = pad_block("  " .. input_text:gsub("\n", "\n  ")), style = { fg = C.md_code_fg, bg = C.md_code_block_bg }, wrap = "word" } end
  local label = entry.error and "  error:" or "  output:"
  if entry.output == nil and not entry.error then label = "  running..." end
  rows[#rows + 1] = tui.text { content = label, style = entry.error and STYLE.status_danger or STYLE.footer, wrap = "none" }
  local output_text
  if type(entry.output) == "table" then output_text = pretty_json(entry.output)
  elseif entry.output ~= nil then output_text = tostring(entry.output) end
  if output_text and #output_text > 0 then
    rows[#rows + 1] = tui.text { content = pad_block("  " .. output_text:gsub("\n", "\n  ")), style = { fg = C.md_code_fg, bg = C.md_code_block_bg }, wrap = "word" }
  end
  return tui.column { gap = 0, children = rows }
end
local function semantic_projection(entry)
  local display = require("libs.chat.tool_display")
  local args = entry.raw_input
  if args == nil then args = entry.input_table or entry.input end
  local projected = display.project(entry.display, args, entry.output, entry.error, entry.name)
  return projected
end
local function tool_header(entry, glyph)
  local p = semantic_projection(entry)
  local label = entry.name or (p and p.label) or "?"
  local header = glyph .. label
  if p and p.primary and p.primary ~= "" then header = header .. " · " .. p.primary end
  if entry.output == nil and not entry.error then header = header .. " …" end
  return header
end
local function collapsed_tool_header(entry)
  if entry.name ~= "mag" then return tool_header(entry, "▸ ") end
  local args = entry.raw_input
  if args == nil then args = entry.input_table or entry.input end
  local action = type(args) == "table" and args.action or nil
  if action == nil then action = "compile" end
  local p = semantic_projection(entry)
  local header = "▸ mag " .. tostring(action)
  if p and p.primary and p.primary ~= "" then header = header .. " · " .. p.primary end
  if entry.output == nil and not entry.error then header = header .. " …" end
  return header
end
local function collapsed_path_tool_header(entry, projection)
  local style = entry.error and STYLE.tool_error or STYLE.tool_name
  local label = entry.name or projection.label or "?"
  local children = {
    tui.text { content = "▸ " .. label .. " · ", style = style, wrap = "none" },
    tui.expanded { child = tui.text {
      content = projection.primary,
      style = style,
      wrap = "tail",
    } },
  }
  if entry.output == nil and not entry.error then
    children[#children + 1] = tui.text { content = " …", style = style, wrap = "none" }
  end
  return tui.row { gap = 0, children = children }
end
local function tool_collapsed(entry)
  local projection = semantic_projection(entry)
  local header
  if entry.name ~= "mag" and projection and projection.primary_is_path
      and projection.primary and projection.primary ~= "" then
    header = collapsed_path_tool_header(entry, projection)
  else
    local wrap = projection and projection.primary_is_path and "tail" or "none"
    header = tui.text {
      content = collapsed_tool_header(entry),
      style = entry.error and STYLE.tool_error or STYLE.tool_name,
      wrap = wrap,
    }
  end
  local rows = { header }
  if entry.error then
    rows[#rows + 1] = tui.text { content = "  error:", style = STYLE.status_danger, wrap = "none" }
    local output_text
    if type(entry.output) == "table" then output_text = pretty_json(entry.output)
    elseif entry.output ~= nil then output_text = tostring(entry.output) end
    if output_text and output_text ~= "" then
      rows[#rows + 1] = tui.text { content = "  " .. output_text:gsub("\n", "\n  "), style = STYLE.status_danger, wrap = "word" }
    end
  end
  return tui.column { gap = 0, children = rows }
end
local function tool_expanded(entry, raw)
  local p = semantic_projection(entry)
  if not p then
    p = {
      label = entry.name or "?",
      arguments = {},
      result = entry.error
        and { kind = "content", text = entry.output or "", error = true }
        or { kind = entry.output == nil and "running" or "receipt", text = "completed" },
    }
  end
  local rows = { tui.text { content = tool_header(entry, "▼ "), style = entry.error and STYLE.tool_error or STYLE.tool_name, wrap = p.primary_is_path and "char" or "word" } }
  for _, field in ipairs(p.arguments) do
    rows[#rows + 1] = tui.text { content = "  " .. field.label .. ": " .. field.value, style = STYLE.footer, wrap = "word" }
  end
  if p.result.kind == "running" then
    rows[#rows + 1] = tui.text { content = "  running...", style = STYLE.footer, wrap = "none" }
  elseif p.result.kind == "receipt" then
    rows[#rows + 1] = tui.text { content = "  ✓ " .. p.result.text, style = STYLE.footer, wrap = "word" }
  else
    rows[#rows + 1] = tui.text { content = p.result.error and "  error:" or "  result:", style = p.result.error and STYLE.status_danger or STYLE.footer, wrap = "none" }
    if p.result.text ~= "" then
      rows[#rows + 1] = tui.text { content = pad_block("  " .. p.result.text:gsub("\n", "\n  ")), style = { fg = C.md_code_fg, bg = C.md_code_block_bg }, wrap = "word" }
    end
  end
  rows[#rows + 1] = tui.text {
    content = raw
      and "  raw: visible (/raw " .. tostring(entry.id or "?") .. " to hide)"
      or "  raw: hidden (/raw " .. tostring(entry.id or "?") .. " to reveal)",
    style = STYLE.footer,
    wrap = "none",
  }
  if raw then
    local raw_rows = raw_tool_expanded(entry)
    rows[#rows + 1] = raw_rows
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

-- Human-readable identity for the run: the readable run_name the kernel
-- stamped on mag.run_started, falling back to the raw run_id only when a
-- name never arrived.
local function graph_result_ident(entry)
  local name = entry.run_name
  if type(name) == "string" and #name > 0 then return name end
  return tostring(entry.run_id or "?")
end

-- Collapsed header: `◆ mag workflow · <run_name> · <duration>`. Machine
-- detail (exact run_id, node count, status/output) lives in the unfolded
-- state — this line is the at-a-glance summary only. A failed run appends
-- a FAILED tail so a failure is unmistakable in the collapsed view.
local function graph_result_header(entry, glyph)
  local failed = (entry.status == "failed")
  local style = failed and STYLE.graph_result_error or STYLE.graph_result_name
  local parts = { glyph .. "mag workflow", graph_result_ident(entry) }
  local dur = humanize_duration_ms(entry.duration_ms)
  if dur then parts[#parts + 1] = dur end
  if failed then parts[#parts + 1] = "FAILED" end
  return tui.text { content = table.concat(parts, " · "), style = style, wrap = "none" }
end

-- The machine detail moved out of the collapsed header: exact run_id and
-- node count. Rendered as a small code block at the top of the unfolded
-- body, above the per-node list.
local function graph_result_meta_block(entry)
  local run_id = tostring(entry.run_id or "?")
  local node_count = (type(entry.nodes) == "table") and #entry.nodes or 0
  local nlabel
  if node_count == 1 then nlabel = "1 node"
  else nlabel = tostring(node_count) .. " nodes" end
  return "  run_id: " .. run_id .. "\n  nodes:  " .. nlabel
end

local function graph_result_collapsed(entry)
  return tui.column { gap = 0, children = { graph_result_header(entry, "◆ ") } }
end

local function graph_result_expanded(entry)
  local rows = { graph_result_header(entry, "◇ ") }
  rows[#rows + 1] = tui.text {
    content = pad_block(graph_result_meta_block(entry)),
    style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
    wrap    = "word",
  }
  local nodes_block = graph_result_nodes_block(entry.nodes)
  if nodes_block then
    rows[#rows + 1] = tui.text {
      content = pad_block(nodes_block),
      style   = { fg = C.md_code_fg, bg = C.md_code_block_bg },
      wrap    = "word",
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
        wrap    = "word",
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
        wrap    = "word",
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

-- Instruction-file reminders are useful model context but noisy in the
-- chat surface; render them as foldable blocks keyed on the path.
local function agents_md_collapsed(entry)
  local path = entry.path or "instructions"
  return tui.column { gap = 0, children = {
    tui.text {
      content = "▸ instructions(" .. path .. ")",
      style   = STYLE.footer,
      wrap    = "none",
    },
  } }
end

local function agents_md_expanded(entry)
  local path = entry.path or "instructions"
  local rows = { tui.text {
    content = "▼ instructions(" .. path .. ")",
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
  local title = "context compacted"
  if entry.status == "pending" then
    title = "context compacting..."
  elseif entry.status == "failed" then
    title = "context compaction failed"
  end
  local parts = { glyph .. title }
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

local function compaction_separator(entry, glyph)
  local label = compaction_label(entry, glyph)
  return tui.row { gap = 1, children = {
    tui.text {
      content = label,
      style   = STYLE.compaction_separator,
      wrap    = "none",
    },
    tui.expanded { child = tui.fill {
      char = "─",
      style = STYLE.compaction_separator,
    } },
  } }
end

local function compaction_header(entry, glyph)
  if entry.status == "complete" then
    return compaction_separator(entry, glyph)
  end
  return tui.text {
    content = compaction_label(entry, glyph),
    style   = STYLE.system,
    wrap    = "none",
  }
end

local function compaction_collapsed(entry)
  local rows = { compaction_header(entry, "▸ ") }
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
  local artifact = entry.model_context_artifact
  if type(artifact) == "table" then
    local slim = {}
    for k, v in pairs(artifact) do
      if k ~= "items" then slim[k] = v end
    end
    artifact = slim
  end
  local rows = { compaction_header(entry, "▼ ") }
  if type(entry.display_summary) == "string" and #entry.display_summary > 0 then
    rows[#rows + 1] = tui.text {
      content = "  " .. entry.display_summary,
      style   = STYLE.footer,
      wrap    = "word",
    }
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

function M.render(entry, _i, expanded, queued, raw_tool_id)
  if entry.kind == "error" then
    return render_error_entry(entry)
  end
  if entry.kind == "tool_call" then
    if expanded then return tool_expanded(entry, raw_tool_id == entry.id) end
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
