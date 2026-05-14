-- starter/lib/session_config.lua — derive a session's active
-- (provider, model) from its on-disk jsonl.
--
-- ## Why
--
-- The active provider/model lives in agentic-loop's `state.config`,
-- which is mutated by `/model` slash-commands at runtime. This config
-- is NOT persisted as a session-header field — it gets reconstructed
-- across `/resume` by walking the recorded envelopes:
--
--   * `chat.model.set { provider, model }` — explicit user-side switch
--     (the canonical envelope chat.lua emits when the user runs
--     `/model X`). Latest one wins.
--   * `<prefix>.chat.create { chat_id, model }` — emitted by the
--     reasoners when a new chat starts. The prefix names the provider;
--     the body's `model` field captures whatever model the create rode
--     with. Default-provider sessions (no /model switch) rely on this
--     fallback because there's no `chat.model.set` in their log.
--
-- Without the restore, /resume after a /model switch in the LIVE
-- session leaves `state.config.provider` pointing at the live-side
-- selection while `state.current_state.chat_id` is restored to the
-- resumed session's chat — so the next live submit dispatches the
-- chat against a provider that doesn't own it ("chat 'X' not found").
--
-- ## Wire shape
--
-- A `<prefix>.chat.create` is recognised by the suffix `.chat.create`
-- on the body's `kind`. The provider name is the substring before that
-- suffix. (We can't enumerate "known providers" up-front — the wrapper
-- list is dynamic and the resumed session may name a provider not
-- currently loaded; the model-name restore still helps debug those
-- cases by surfacing the original provider clearly.)
--
-- ## Output
--
-- `read_active_model(path)` returns `{ provider, model }`. Either
-- field may be nil if the log doesn't carry that signal. Callers
-- decide whether to apply a partial result or fall through to defaults.

local json = nefor.json

local M = {}

local CREATE_SUFFIX = ".chat.create"
local CREATE_SUFFIX_LEN = #CREATE_SUFFIX

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

---Strip the `.chat.create` suffix to recover a provider prefix.
---Returns nil for kinds that don't end in `.chat.create` or have an
---empty prefix.
---@param kind string
---@return string|nil
local function provider_from_chat_create_kind(kind)
  if type(kind) ~= "string" then return nil end
  local n = #kind
  if n <= CREATE_SUFFIX_LEN then return nil end
  if kind:sub(n - CREATE_SUFFIX_LEN + 1) ~= CREATE_SUFFIX then return nil end
  local prefix = kind:sub(1, n - CREATE_SUFFIX_LEN)
  if #prefix == 0 then return nil end
  return prefix
end

---Walk `path` (a sessions jsonl) and return the latest active
---(provider, model). Walks the whole file (small N — sessions don't
---hold millions of envelopes) and tracks two latest-seen sources:
---explicit `chat.model.set` and inferred `<prefix>.chat.create`.
---
---Order of preference (matches the live mutation order in
---agentic-loop): explicit `chat.model.set` overrides the chat.create
---inference for both fields. If `chat.model.set` is absent, fall back
---to chat.create.
---
---@param path string
---@return { provider: string|nil, model: string|nil }
function M.read_active_model(path)
  local result = { provider = nil, model = nil }
  if type(path) ~= "string" or path == "" then return result end

  local fh = io.open(path, "r")
  if not fh then return result end

  local create_provider, create_model
  local explicit_provider, explicit_model

  for line in fh:lines() do
    -- Cheap header skip; full parse only on entries.
    if line:sub(1, 12) ~= [[{"_session":]] then
      local ok, row = pcall(json.decode, line)
      if ok and type(row) == "table" and row.origin == "step" then
        local body = payload_body(row)
        if body ~= nil then
          local k = body.kind
          if k == "chat.model.set" then
            -- Latest wins — overwrite both fields when present.
            if type(body.provider) == "string" and #body.provider > 0 then
              explicit_provider = body.provider
            end
            if type(body.model) == "string" and #body.model > 0 then
              explicit_model = body.model
            end
          else
            local prov = provider_from_chat_create_kind(k)
            if prov ~= nil then
              create_provider = prov
              -- chat.create may omit `model` (e.g., responder branch);
              -- only update when present so a newer create without a
              -- model doesn't blank the prior model.
              if type(body.model) == "string" and #body.model > 0 then
                create_model = body.model
              end
            end
          end
        end
      end
    end
  end
  fh:close()

  result.provider = explicit_provider or create_provider
  result.model    = explicit_model    or create_model
  return result
end

return M
