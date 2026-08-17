-- Canonical composition policy for model-visible continuation content.
-- Mechanisms consume this table; they do not restate its limits.

local M = {
  item_limit = 32 * 1024,
  continuation_limit = 96 * 1024,
}

function M.instruction()
  return string.format([[## Bounded model context

Model-visible MAG and tool-result content is bounded to %d bytes per result and %d bytes combined in one continuation. Full outputs remain persisted. Truncation markers name the canonical output path and the exact omitted zero-based half-open byte range `[start, end)`; retrieve it with `read_file(path=<canonical path>, offset=start, max_bytes=end-start)`.
]], M.item_limit, M.continuation_limit)
end

function M.inject_before_tools(prompt)
  local heading = "\n## Tools\n"
  local start = prompt:find(heading, 1, true)
  if not start then
    return prompt .. "\n\n" .. M.instruction()
  end
  return prompt:sub(1, start - 1) .. "\n\n" .. M.instruction() .. prompt:sub(start)
end

return M
