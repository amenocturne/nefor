-- Inline <think>...</think> -> reasoning split for models that emit
-- chain-of-thought as text deltas instead of via OpenAI's
-- delta.reasoning field (qwen-family on some Nestor-served stacks).
-- Without intervention those tags reach the chat surface as plain text
-- and look like the model is thinking out loud at the user.
--
-- A second wrinkle: some servers strip the opening <think> tag but
-- keep the closing </think>, so the stream starts in thinking content
-- and only reveals the mode via the close tag. Both cases are handled
-- by buffering until the mode is decided.
--
-- Per-stream state machine (one instance per chat_id):
--   buffering -- accumulate text; haven't yet seen <think> or </think>
--   thinking  -- inside a <think> block; emit thinking records
--   text      -- outside think; emit content records
--   disabled  -- native delta.reasoning was detected upstream; pass-through
--
-- Each phase tail-guards the last 8 chars (</think>) or 6 chars (<think)
-- of the buffer so a partial tag spanning a chunk boundary isn't
-- mis-emitted; the guard flushes on stream end.

local M = {}

local OPEN_TAG  = "<think>"
local CLOSE_TAG = "</think>"
local OPEN_LEN  = #OPEN_TAG    -- 7
local CLOSE_LEN = #CLOSE_TAG   -- 8

-- plain=true: literal match, no regex magic, no escaping.
local function find(s, needle)
  return string.find(s, needle, 1, true)
end

-- Adjust a byte position n so that s:sub(1, n) does NOT end in the
-- middle of a multi-byte UTF-8 sequence. Lua strings are byte arrays;
-- buffer:sub uses byte indices and will happily split a 👋 (4 bytes)
-- across two emits. The downstream JSON encoder (mlua-serde) then sees
-- a Lua string that isn't valid UTF-8 and surfaces it as a byte_array
-- type, which serde_json::Value can't represent — encode crashes with
-- "deserialize error: invalid type: byte array".
--
-- UTF-8 byte classes:
--   0x00..0x7F  ASCII (single-byte char, complete by itself)
--   0x80..0xBF  continuation byte (mid-char, requires earlier start)
--   0xC0..0xFF  start of a 2/3/4-byte multi-byte char
--
-- If the byte at position n+1 is a continuation byte, our cut would
-- leave the char it belongs to half-emitted. Walk back to the byte
-- before that char's start and cut there instead.
local function utf8_safe_end(s, n)
  if n <= 0 then return 0 end
  if n >= #s then return n end
  local nb = s:byte(n + 1)
  if nb == nil or nb < 0x80 or nb >= 0xC0 then
    -- Byte after the cut is ASCII or a fresh start byte. Cut is clean.
    return n
  end
  -- Walk back through continuation bytes until we find the char's start.
  local i = n
  while i > 0 do
    local b = s:byte(i)
    if b == nil then return 0 end
    if b < 0x80 or b >= 0xC0 then
      -- s[i] is the start byte of the char that extends past n. Cut
      -- BEFORE this start byte.
      return i - 1
    end
    i = i - 1
  end
  return 0
end

function M.make()
  local self = {}
  local phase  = "buffering"
  local buffer = ""

  local function emit(out, kind, text)
    if text == nil or text == "" then return end
    out[#out + 1] = { kind = kind, text = text }
  end

  -- Process one upstream stream.delta chunk. Returns a (possibly empty)
  -- list of { kind = "content"|"thinking", text } records.
  function self:process(delta)
    if delta == nil or delta == "" then return {} end
    if phase == "disabled" then
      return { { kind = "content", text = delta } }
    end

    buffer = buffer .. delta
    local out = {}

    -- Buffering: decide the mode by scanning the accumulated buffer for
    -- the first tag we recognise.
    if phase == "buffering" then
      local open_idx = find(buffer, OPEN_TAG)
      if open_idx ~= nil then
        if open_idx > 1 then
          emit(out, "content", buffer:sub(1, open_idx - 1))
        end
        buffer = buffer:sub(open_idx + OPEN_LEN)
        phase = "thinking"
        -- fall through into the post-buffering loop
      else
        local close_idx = find(buffer, CLOSE_TAG)
        if close_idx ~= nil then
          -- Missing-open-tag case: everything before </think> was
          -- thinking content; flip to text after.
          if close_idx > 1 then
            emit(out, "thinking", buffer:sub(1, close_idx - 1))
          end
          buffer = buffer:sub(close_idx + CLOSE_LEN)
          phase = "text"
          -- fall through
        else
          -- Still ambiguous. Hold the buffer; emit nothing.
          return out
        end
      end
    end

    -- Post-buffering loop: alternate between thinking and text phases
    -- as we encounter tags. The tail-guard on each branch keeps a
    -- partial tag at the very end of the buffer until the next chunk.
    while #buffer > 0 do
      if phase == "thinking" then
        local close_idx = find(buffer, CLOSE_TAG)
        if close_idx == nil then
          -- No close yet; emit everything except the last 8 chars
          -- (potentially a partial "</think>") and bail until more
          -- text arrives. Snap the cut to a UTF-8 boundary so a
          -- multi-byte char isn't split between two emits.
          local safe = utf8_safe_end(buffer, #buffer - CLOSE_LEN)
          if safe > 0 then
            emit(out, "thinking", buffer:sub(1, safe))
            buffer = buffer:sub(safe + 1)
          end
          break
        end
        if close_idx > 1 then
          emit(out, "thinking", buffer:sub(1, close_idx - 1))
        end
        buffer = buffer:sub(close_idx + CLOSE_LEN)
        phase = "text"

      elseif phase == "text" then
        local open_idx = find(buffer, OPEN_TAG)
        if open_idx == nil then
          -- No open tag; emit all but the last 6 chars (potentially
          -- the start of "<think"). Snap to a UTF-8 boundary.
          local safe = utf8_safe_end(buffer, #buffer - (OPEN_LEN - 1))
          if safe > 0 then
            emit(out, "content", buffer:sub(1, safe))
            buffer = buffer:sub(safe + 1)
          end
          break
        end
        if open_idx > 1 then
          emit(out, "content", buffer:sub(1, open_idx - 1))
        end
        buffer = buffer:sub(open_idx + OPEN_LEN)
        phase = "thinking"

      else
        -- "buffering" handled above; "disabled" returned at top.
        break
      end
    end

    return out
  end

  -- End-of-stream flush. Buffered content emits as content in buffering
  -- and text phases, as thinking (with implicit close) in thinking.
  function self:flush()
    if phase == "disabled" then return {} end
    local out = {}
    if #buffer > 0 then
      if phase == "thinking" then
        emit(out, "thinking", buffer)
      else
        emit(out, "content", buffer)
      end
      buffer = ""
    end
    return out
  end

  -- Suppress all further interception. Called when upstream already
  -- emitted a native reasoning event (model reports thinking via
  -- delta.reasoning instead of inline tags). Returns the buffered
  -- content so the caller can re-emit it as plain content.
  function self:disable()
    phase = "disabled"
    local b = buffer
    buffer = ""
    return b
  end

  function self:phase() return phase end
  function self:buffer() return buffer end

  return self
end

return M
