local sessions = require("libs.chat.sessions")
local popups = require("libs.chat.popups")
local function contents(tree, out)
  out = out or {}
  if type(tree) ~= "table" then return out end
  if type(tree.content) == "string" then
    assert(utf8.len(tree.content), "preview tree must remain UTF-8")
    out[#out + 1] = tree.content
  end
  for _, child in pairs(tree) do
    if type(child) == "table" then contents(child, out) end
  end
  return out
end
for _, sample in ipairs({string.rep("Ж", 40), string.rep("😀", 22), string.rep("a", 48) .. "€😀Ж", string.rep("a", 70) .. "😀Ж"}) do
  for _, budget in ipairs({1, 2, 3, 50, 72}) do
    local clipped = sessions.clip_preview(sample, budget)
    assert(utf8.len(clipped))
    if #sample > budget then
      local prefix = clipped:sub(1, -4)
      assert(#prefix <= budget - 1)
      assert(sample:sub(1, #prefix) == prefix)
      assert(clipped:sub(-3) == "…")
    else assert(clipped == sample) end
  end
  local session = {id="saved", preview=sample, started_at="2026-07-10T12:30"}
  local prompt = {id="prompt", text=sample}
  local session_tree = popups.session_picker({popup={variant="session_picker", cursor=1, sessions={session}}})
  local rewind_tree = popups.rewind_picker({popup={variant="rewind_picker", cursor=1, prompts={prompt}}})
  local session_text = table.concat(contents(session_tree), "\n")
  local rewind_text = table.concat(contents(rewind_tree), "\n")
  assert(session_text:find(sessions.clip_preview(sample, 50), 1, true))
  assert(rewind_text:find(sessions.clip_preview(sample, 72), 1, true))
  assert(session.id == "saved" and session.preview == sample)
  assert(prompt.id == "prompt" and prompt.text == sample)
end
assert(sessions.clip_preview("a\nb\rc", 50) == "a b c")
