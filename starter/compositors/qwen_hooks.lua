-- starter/compositors/qwen_hooks.lua — qwen-family hooks for upstream's
-- provider compositor.
--
-- Wires three pieces of team-specific behaviour into upstream's
-- `compositors/provider.lua` via its opts.hooks seam, so the team
-- doesn't have to fork the compositor:
--
-- 1. Stream-delta `<think>...</think>` filtering — each prefixed
--    stream.delta is run through a per-chat-id filter BEFORE upstream's
--    translator.outbound. The filter may split one chunk into N records
--    (content + thinking interleaved); non-last records emit as
--    synthetic bus envelopes via helpers.emit_synthetic, the last
--    record mutates env.body in place.
--
-- 2. Native-reasoning detection — when the provider emits
--    reasoning_delta / reasoning_end (OpenAI native chain-of-thought),
--    flip a per-chat flag so subsequent inline-extracted THINKING
--    records drop (the chat already has the reasoning via the native
--    channel). The filter still strips inline tags from content —
--    qwen-family models double-emit (native + inline), and the content
--    stream still carries `<think>` spans we don't want in chat.
--
-- 3. Final-text strip — at stream.end and chat.complete.result the
--    binary emits the FULL accumulated assistant text (delta concat),
--    which still contains inline `<think>` spans the streaming-side
--    filter stripped. Re-strip here so chat.lua's finalize_assistant
--    (which overwrites streamed-and-filtered text with body.text from
--    the end event) and the orchestrator's node-result chain don't see
--    raw spans.
--
-- 4. `chat.model.list_requested` drop — Nestor has no /v1/models, so
--    delivering this to the binary would just produce a noisy
--    turn.error. The cached boot-fetch list is served from a bus
--    subscriber in init.lua; intercept_to_plugin drops the outbound
--    request before the binary sees it.

local think_tag_filter = require("openai-provider.think_tag_filter")

local M = {}

-- Strip inline `<think>...</think>` spans from a fully-assembled text.
-- Used at stream.end and chat.complete.result where the binary emits
-- the unfiltered concatenation of content deltas.
--
-- The `-` quantifier is Lua's lazy match: `<think>.-</think>` finds the
-- shortest paired span. gsub iterates so multiple paired blocks all
-- get removed. The follow-up handles the missing-open-tag quirk Nestor
-- exhibits for some prompts: after paired-block removal, an orphan
-- `</think>` means the response started in thinking mode without an
-- open tag — drop everything through that close.
local function strip_thinking_blocks(s)
  if type(s) ~= "string" or #s == 0 then return s end
  local stripped = (s:gsub("<think>.-</think>", ""))
  local close_pos = stripped:find("</think>", 1, true)
  if close_pos then
    stripped = stripped:sub(close_pos + 8)
  end
  return stripped
end

-- make(name, opts) — builds the { intercept_inbound, intercept_to_plugin }
-- table to hand to provider.spawn_spec via opts.hooks.
--
-- opts.enable_think_tag_filter (boolean) — when truthy, intercept_inbound
--   runs the per-chat-id filter on stream.delta, flips the native flag
--   on reasoning_*, and re-strips at stream.end / chat.complete.result.
--   When falsy, intercept_inbound is a pass-through.
--
-- opts.intercept_model_list_request (boolean) — when truthy,
--   intercept_to_plugin drops chat.model.list_requested envelopes
--   targeted at this provider. When falsy, intercept_to_plugin is a
--   pass-through.
function M.make(name, opts)
  assert(type(name) == "string" and #name > 0,
    "qwen_hooks.make: name required")
  opts = opts or {}

  local enable_filter        = opts.enable_think_tag_filter == true
  local intercept_models_list = opts.intercept_model_list_request == true

  -- Per-chat-id filter state. Created lazily on first delta from a
  -- chat we haven't seen; dropped on stream.end. Chat ids are a few
  -- bytes each and sessions cap at a few thousand turns, so no GC.
  local filters = {}

  -- Per-chat-id "the upstream provider already emitted a structured
  -- reasoning_delta event for this chat" flag. When set, the filter
  -- stays active (so inline `<think>` tags still get stripped from
  -- content), but extracted THINKING records get dropped — the chat
  -- plugin already has the reasoning via the native channel.
  local native_reasoning_seen = {}

  local function get_filter(chat_id)
    local f = filters[chat_id]
    if f == nil then
      f = think_tag_filter.make()
      filters[chat_id] = f
    end
    return f
  end

  -- intercept_inbound runs between maybe_inject_static_token and
  -- translator.outbound. It receives the raw prefixed env (env.body.kind
  -- is e.g. `<prefix>.stream.delta`, not the canonical kind), so we
  -- match against helpers.kinds.
  --
  -- Returns false to drop the env; any other return continues.
  local function intercept_inbound_real(env, helpers)
    -- Guard non-event / non-table envelopes — they have no body.kind
    -- to dispatch on. Upstream's translator.outbound will handle them.
    if env.type ~= "event" or type(env.body) ~= "table" then
      return true
    end

    local kinds = helpers.kinds
    local k = env.body.kind

    if k == kinds.stream_reasoning_delta or k == kinds.stream_reasoning_end then
      local chat_id = env.body.chat_id
      if type(chat_id) == "string" then
        native_reasoning_seen[chat_id] = true
      end
      return true
    end

    if k == kinds.stream_delta then
      local chat_id = env.body.chat_id
      local text    = env.body.text or env.body.delta
      if type(chat_id) ~= "string" or type(text) ~= "string" or #text == 0 then
        return true
      end

      local f = get_filter(chat_id)
      local records = f:process(text)
      if #records == 0 then
        return false
      end

      local native = native_reasoning_seen[chat_id] == true
      local kept = {}
      for _, r in ipairs(records) do
        if not (r.kind == "thinking" and native) then
          kept[#kept + 1] = r
        end
      end
      if #kept == 0 then
        return false
      end

      local from = env.from or helpers.name
      local last = kept[#kept]
      for i = 1, #kept - 1 do
        local r = kept[i]
        helpers.emit_synthetic(from, {
          kind    = (r.kind == "thinking") and kinds.stream_reasoning_delta or kinds.stream_delta,
          chat_id = chat_id,
          text    = r.text,
        })
      end

      env.body.text = last.text
      if last.kind == "thinking" then
        env.body.kind = kinds.stream_reasoning_delta
      end
      return true
    end

    if k == kinds.chat_complete_result then
      if type(env.body.output) == "table"
          and type(env.body.output.text) == "string" then
        env.body.output.text = strip_thinking_blocks(env.body.output.text)
      end
      return true
    end

    if k == kinds.stream_end then
      if type(env.body.text) == "string" then
        env.body.text = strip_thinking_blocks(env.body.text)
      end
      local chat_id = env.body.chat_id
      if type(chat_id) == "string" then
        local f = filters[chat_id]
        if f ~= nil then
          local native = native_reasoning_seen[chat_id] == true
          local from = env.from or helpers.name
          for _, r in ipairs(f:flush()) do
            if not (r.kind == "thinking" and native) then
              helpers.emit_synthetic(from, {
                kind    = (r.kind == "thinking") and kinds.stream_reasoning_delta or kinds.stream_delta,
                chat_id = chat_id,
                text    = r.text,
              })
            end
          end
          filters[chat_id] = nil
          native_reasoning_seen[chat_id] = nil
        end
      end
      return true
    end

    return true
  end

  -- intercept_to_plugin runs inside to_plugin's non-replay branch
  -- before translator.inbound. Drop chat.model.list_requested envelopes
  -- targeted at this provider — Nestor has no /v1/models so the
  -- binary's standard handler 404s and surfaces a noisy turn.error.
  -- init.lua's nefor.bus.on_event subscriber serves the cached list
  -- from boot fetch.
  local function intercept_to_plugin_real(env)
    if env.type == "event"
        and type(env.body) == "table"
        and env.body.kind == "chat.model.list_requested"
        and env.body.provider == name then
      return false
    end
    return true
  end

  local pass_through = function() return true end

  return {
    intercept_inbound   = enable_filter and intercept_inbound_real or pass_through,
    intercept_to_plugin = intercept_models_list and intercept_to_plugin_real or pass_through,
  }
end

return M
