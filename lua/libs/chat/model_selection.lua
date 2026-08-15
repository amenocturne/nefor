-- Model selection mechanism: one primitive for every way the surface picks a
-- provider/model pair (the `/model` picker, a qualified `/model <provider>
-- <model>`, or a bare `/model <model>` resolved against the loaded catalogs).
--
-- A selection is a request, not a fact. The surface records the requested pair
-- and the state it would roll back to, emits `chat.model.set`, and only adopts
-- the pair when the owning provider acknowledges it. Acknowledgements are
-- correlated against that pending pair, so an ack for some other pair — a
-- provider hello, a replayed session, a slow answer to a superseded request —
-- cannot move the surface onto a model the user did not select.
--
-- Provider/model/reasoning-default/context-window move together in a single
-- patch: no intermediate state shows the new model against the previous
-- model's reasoning effort or context window.

local common = require("libs.chat.common")
local shallow_merge = common.shallow_merge
local NIL_SENTINEL = common.NIL_SENTINEL

local M = {}

local function nonempty(value)
  return type(value) == "string" and value ~= ""
end

local function trim(value)
  return (tostring(value or ""):gsub("^%s+", ""):gsub("%s+$", ""))
end

-- ── catalogs ──────────────────────────────────────────────────────────

-- Providers report their model lists through `chat.models.listed`. The catalog
-- is kept whether or not a picker is open so a bare `/model <model>` can decide
-- uniqueness without a round trip.
function M.absorb_catalog(catalog, provider, models)
  if not nonempty(provider) then return catalog or {} end
  local next_catalog = {}
  for name, list in pairs(catalog or {}) do next_catalog[name] = list end
  local names = {}
  if type(models) == "table" then
    for _, model in ipairs(models) do
      if nonempty(tostring(model)) then names[#names + 1] = tostring(model) end
    end
  end
  table.sort(names)
  next_catalog[provider] = names
  return next_catalog
end

local function providers_offering(catalog, model)
  local owners = {}
  for provider, models in pairs(catalog or {}) do
    for _, candidate in ipairs(models or {}) do
      if candidate == model then
        owners[#owners + 1] = provider
        break
      end
    end
  end
  table.sort(owners)
  return owners
end

-- ── argument resolution ───────────────────────────────────────────────

-- `providers` is the set of registered provider names (the surface's auth map);
-- `catalog` is provider → model list. Returns one of:
--   { kind = "browse" }                            — open the picker
--   { kind = "select", provider, model }           — an unambiguous pair
--   { kind = "ambiguous", model, providers }       — several catalogs offer it
--   { kind = "unresolved", model, searched }       — no catalog offers it
function M.resolve(args, providers, catalog)
  local text = trim(args)
  if text == "" then return { kind = "browse" } end

  local head, rest = text:match("^(%S+)%s+(.+)$")
  if head ~= nil and (providers or {})[head] ~= nil then
    -- Qualified selection is the user's own authority: a model id may contain
    -- slashes or any other punctuation, and need not appear in a loaded
    -- catalog (a catalog can be stale, partial, or never requested).
    return { kind = "select", provider = head, model = trim(rest) }
  end

  local owners = providers_offering(catalog, text)
  if #owners == 1 then
    return { kind = "select", provider = owners[1], model = text }
  end
  if #owners > 1 then
    return { kind = "ambiguous", model = text, providers = owners }
  end

  local searched = {}
  for provider in pairs(catalog or {}) do searched[#searched + 1] = provider end
  table.sort(searched)
  return { kind = "unresolved", model = text, searched = searched }
end

function M.ambiguous_message(resolution)
  local lines = {
    "`" .. resolution.model .. "` is offered by more than one provider.",
    "Qualify the provider:",
  }
  for _, provider in ipairs(resolution.providers or {}) do
    lines[#lines + 1] = "  /model " .. provider .. " " .. resolution.model
  end
  return table.concat(lines, "\n")
end

function M.unresolved_message(resolution)
  local lines = { "No loaded catalog offers `" .. resolution.model .. "`." }
  if #(resolution.searched or {}) == 0 then
    lines[#lines + 1] = "No model catalog is loaded yet."
  else
    lines[#lines + 1] = "Searched: " .. table.concat(resolution.searched, ", ")
  end
  lines[#lines + 1] = "Run /model to browse catalogs, or qualify the provider:"
  lines[#lines + 1] = "  /model <provider> " .. resolution.model
  return table.concat(lines, "\n")
end

-- ── selection lifecycle ───────────────────────────────────────────────

-- Record the pending pair and the state a failure rolls back to, and emit the
-- request. Both the picker and `/model <args>` go through here, so the two
-- entry points cannot drift apart.
function M.request(state, provider, model)
  if not nonempty(provider) or not nonempty(model) then return state, {} end
  local pending = {
    provider = provider,
    model = model,
    previous = {
      provider = state.provider,
      model = state.model,
      reasoning_effort = state.reasoning_effort,
      max_tokens = state.max_tokens,
      current_context_tokens = state.current_context_tokens,
    },
  }
  return shallow_merge(state, { pending_model_selection = pending }), {
    { kind = "send_to", target = "engine",
      body = { kind = "chat.model.set", provider = provider, model = model } },
  }
end

local function optional(value)
  if value == nil then return NIL_SENTINEL end
  return value
end

-- One atomic adoption: provider, model, the pair's reasoning default and the
-- pair's context window all move together, and a pair change clears the
-- previous pair's measured context usage.
local function adopt(state, provider, model, effort, lookups)
  local pair_changed = provider ~= state.provider or model ~= state.model
  if effort == nil then
    effort = lookups.reasoning_default(provider, model)
  end
  if effort == nil and not pair_changed then effort = state.reasoning_effort end
  local max_tokens = lookups.context_window(provider, model)
  if max_tokens == nil and not pair_changed then max_tokens = state.max_tokens end
  local patch = {
    provider = provider,
    model = model,
    reasoning_effort = optional(effort),
    max_tokens = optional(max_tokens),
    pending_model_selection = NIL_SENTINEL,
  }
  if pair_changed then patch.current_context_tokens = NIL_SENTINEL end
  return shallow_merge(state, patch)
end

-- Correlate an acknowledgement.
--   pending selection  → adopt only the acknowledged pending pair; anything
--                        else is a stale ack and changes nothing.
--   no pending         → an unsolicited ack (provider hello, restored session)
--                        is adopted only for the already-active provider.
function M.acknowledge(state, msg, lookups)
  local pending = state.pending_model_selection
  local ack_provider = nonempty(msg.provider) and msg.provider or nil
  local ack_model = nonempty(msg.model) and msg.model or nil
  local effort = nonempty(msg.reasoning_effort) and msg.reasoning_effort or nil

  if type(pending) == "table" then
    if ack_provider ~= nil and ack_provider ~= pending.provider then return state end
    if ack_model ~= nil and ack_model ~= pending.model then return state end
    if ack_provider == nil and ack_model == nil then return state end
    return adopt(state, pending.provider, pending.model, effort, lookups)
  end

  if ack_provider ~= nil and nonempty(state.provider) and ack_provider ~= state.provider then
    return state
  end
  return adopt(state, ack_provider or state.provider, ack_model or state.model,
    effort, lookups)
end

-- A provider rejected the pending pair. Restore exactly what the request
-- captured and clear the pending record; an ack that arrives afterwards no
-- longer correlates and is ignored.
function M.reject(state, msg)
  local pending = state.pending_model_selection
  if type(pending) ~= "table" then return state, false end
  if nonempty(msg.provider) and msg.provider ~= pending.provider then return state, false end
  if nonempty(msg.model) and msg.model ~= pending.model then return state, false end
  local previous = pending.previous or {}
  return shallow_merge(state, {
    provider = optional(previous.provider),
    model = optional(previous.model),
    reasoning_effort = optional(previous.reasoning_effort),
    max_tokens = optional(previous.max_tokens),
    current_context_tokens = optional(previous.current_context_tokens),
    pending_model_selection = NIL_SENTINEL,
  }), true
end

-- The pending provider left the connected state; the selection can never be
-- acknowledged, so roll it back rather than leaving it pending forever.
function M.provider_unavailable(state, provider, status)
  local pending = state.pending_model_selection
  if type(pending) ~= "table" or pending.provider ~= provider then return state, false end
  if status == "connected" or status == "login_in_progress" then return state, false end
  return M.reject(state, { provider = provider, model = pending.model })
end

function M.active(state)
  local pending = state.pending_model_selection
  return {
    provider = state.provider,
    model = state.model,
    pending_provider = type(pending) == "table" and pending.provider or nil,
    pending_model = type(pending) == "table" and pending.model or nil,
  }
end

return M
