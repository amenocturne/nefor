-- starter/lib/history_replay.lua — chat-history rebuild primitive.
--
-- Walks a sessions jsonl log file and re-emits the chat-state envelopes
-- that rebuild a single chat_id's conversation history into a target
-- provider, translated from the source provider's wire prefix.
--
-- ## Why
--
-- Provider binaries hold per-chat_id history in process memory. When
-- the active conversation has to land on a *different* provider than
-- the one that built it (cross-provider /model switch), the new
-- provider has zero history for that chat — the model replies with
-- no memory of prior turns. This module's `replay_chat_history`
-- writes the rebuild sequence onto the bus so the new provider's
-- wrapper delivers it to its binary the same way the live path
-- would have.
--
-- The cross-process /resume case (fresh nefor process re-feeds the
-- recorded history into the same provider) is handled separately by
-- `starter/openai-provider/init.lua`'s `handle_replay`, which gates on
-- the framework's replay-window flag and re-feeds verbatim. This
-- module is the *cross-provider* analogue: same chat_id semantics,
-- but the source and target providers differ so we translate the
-- prefix as we walk.
--
-- ## Wire shape
--
-- Source envelopes recognised in the log (each is a step entry the
-- live agentic stack already emitted):
--
--   `<src_prefix>.chat.create { chat_id, model? }`           — verbatim re-feed
--   `<src_prefix>.chat.append { chat_id, message }`          — verbatim re-feed
--   `tool.result { result.next_state.chat_id == src_chat_id }` — synthesise an
--                                                                assistant `<tgt>.chat.append`
--                                                                from `result.text` + `result.tool_calls`
--
-- The synthesis path mirrors `openai-provider/handle_replay`'s
-- assistant-turn synthesis: the live `chat.complete` flow appends the
-- assistant message inside the binary's streaming handler, so it never
-- shows up on the wire as a `chat.append`. The wrapper-emitted
-- `tool.result` IS persisted (with `result.text` + optional
-- `result.tool_calls` + `result.next_state.chat_id`), so it's the
-- canonical signal for the assistant turn.
--
-- ## Output
--
-- Re-emitted envelopes (each via `nefor.engine.send` so they:
-- (a) persist into the active session log, (b) reach the target
-- wrapper's `to_plugin` for delivery, (c) surface to any bus observer):
--
--   `<target_provider>.chat.create { chat_id = target_chat_id, model? }`
--   `<target_provider>.chat.append { chat_id = target_chat_id, message }`
--
-- The first emit is always `chat.create` so the new provider's binary
-- has a chat to attach to. Subsequent appends use the same target
-- chat_id.
--
-- ## Usage
--
--   local history_replay = require("core.history_replay")
--   local count = history_replay.replay_chat_history {
--     path             = sessions.current_path(),
--     src_prefix       = "mock-plugin",
--     src_chat_id      = "chat-7",
--     target_provider  = "ollama",
--     target_chat_id   = "chat-12",
--     model            = "qwen3",
--   }

local json     = nefor.json
local envelope = require("core.envelope")

local M = {}

---@param decoded any
---@return table|nil
local function payload_body(decoded)
  if type(decoded) ~= "table" then return nil end
  if type(decoded.payload) ~= "string" then return nil end
  local ok, env = pcall(json.decode, decoded.payload)
  if not ok or type(env) ~= "table" or type(env.body) ~= "table" then
    return nil
  end
  return env.body
end

---Walk `path` (a sessions jsonl) and re-emit the source provider's
---chat history for `src_chat_id` translated to `target_provider` /
---`target_chat_id`. Returns (n_emitted, err) — `err` is nil on success.
---
---@param opts { path: string, src_prefix: string, src_chat_id: string, target_provider: string, target_chat_id: string, model: string|nil }
---@return integer n_emitted, string|nil err
function M.replay_chat_history(opts)
  if type(opts) ~= "table" then return 0, "opts table required" end
  local path             = opts.path
  local src_prefix       = opts.src_prefix
  local src_chat_id      = opts.src_chat_id
  local target_provider  = opts.target_provider
  local target_chat_id   = opts.target_chat_id
  local model            = opts.model
  if type(path) ~= "string" or path == "" then return 0, "path required" end
  if type(src_prefix) ~= "string" or src_prefix == "" then return 0, "src_prefix required" end
  if type(src_chat_id) ~= "string" or src_chat_id == "" then return 0, "src_chat_id required" end
  if type(target_provider) ~= "string" or target_provider == "" then return 0, "target_provider required" end
  if type(target_chat_id) ~= "string" or target_chat_id == "" then return 0, "target_chat_id required" end

  local fh, oerr = io.open(path, "r")
  if not fh then return 0, "open failed: " .. tostring(oerr) end

  local src_create_kind   = src_prefix .. ".chat.create"
  local src_append_kind   = src_prefix .. ".chat.append"
  local target_create_kind = target_provider .. ".chat.create"
  local target_append_kind = target_provider .. ".chat.append"

  local emitted   = 0
  local saw_create = false

  -- The first emit on the bus must be chat.create so the new provider's
  -- binary has a chat to attach subsequent appends to. The model on that
  -- chat.create is whatever the caller passed — that name lives in the
  -- TARGET provider's namespace. We deliberately do NOT carry the model
  -- recorded on the source log's chat.create across the boundary: the
  -- source-log model name is in the OLD provider's namespace and a
  -- cross-provider switch implies the new provider doesn't share it
  -- (e.g., "mock-model" → ollama, "qwen3" → groq). Asking the new
  -- provider to spin up an old-namespace model name surfaces as an
  -- API error on the next chat.complete.
  local create_model = model
  local pending_appends = {}

  for line in fh:lines() do
    if line:sub(1, 12) ~= [[{"_session":]] then
      local ok, row = pcall(json.decode, line)
      if ok and type(row) == "table" and row.origin == "step" then
        local body = payload_body(row)
        if body ~= nil then
          local k = body.kind
          if k == src_create_kind and body.chat_id == src_chat_id then
            saw_create = true
          elseif k == src_append_kind and body.chat_id == src_chat_id then
            pending_appends[#pending_appends + 1] = body.message
          elseif k == "tool.result" then
            local result = body.result
            if type(result) == "table" then
              local ns = result.next_state
              if type(ns) == "table" and ns.chat_id == src_chat_id then
                local text = type(result.text) == "string" and result.text or ""
                local tcs  = result.tool_calls
                local has_text = #text > 0
                local has_tcs  = type(tcs) == "table" and #tcs > 0
                if has_text or has_tcs then
                  local message = { role = "assistant", content = text }
                  if has_tcs then message.tool_calls = tcs end
                  pending_appends[#pending_appends + 1] = message
                end
              end
            end
          end
        end
      end
    end
  end
  fh:close()

  if not saw_create then
    return 0, "no chat.create for src_chat_id in log"
  end

  -- Emit chat.create first.
  local create_body = { kind = target_create_kind, chat_id = target_chat_id }
  if type(create_model) == "string" and #create_model > 0 then
    create_body.model = create_model
  end
  envelope.emit_to(target_provider, create_body)
  emitted = emitted + 1

  for _, message in ipairs(pending_appends) do
    envelope.emit_to(target_provider, {
      kind    = target_append_kind,
      chat_id = target_chat_id,
      message = message,
    })
    emitted = emitted + 1
  end

  return emitted, nil
end

return M
