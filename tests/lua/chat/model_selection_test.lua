-- Selection laws for the cross-provider model switch. One primitive serves the
-- picker and every `/model` argument form; acknowledgements are correlated
-- against the pending pair so a stale ack can never move the surface.
package.preload["nefor-tui"] = function()
  local nil_sentinel = {}
  return {
    util = {
      NIL = nil_sentinel,
      shallow_merge = function(base, patch)
        local merged = {}
        for k, v in pairs(base) do merged[k] = v end
        for k, v in pairs(patch) do
          if v == nil_sentinel then merged[k] = nil else merged[k] = v end
        end
        return merged
      end,
    },
  }
end

local selection = require("libs.chat.model_selection")

local function eq(actual, expected, message)
  if actual ~= expected then
    error((message or "values differ") .. ": expected " .. tostring(expected)
      .. ", got " .. tostring(actual), 2)
  end
end

local function assert_true(value, message)
  if not value then error("assertion failed: " .. (message or "expected truthy"), 2) end
end

local providers = { chatgpt = "connected", openrouter = "connected", ollama = "login_required" }
local catalog = {}
catalog = selection.absorb_catalog(catalog, "chatgpt", { "gpt-5", "shared-model" })
catalog = selection.absorb_catalog(catalog, "openrouter",
  { "anthropic/claude-sonnet-4.5", "shared-model" })

local lookups = {
  reasoning_default = function(provider, model)
    if provider == "openrouter" and model == "anthropic/claude-sonnet-4.5" then
      return "medium"
    end
    return nil
  end,
  context_window = function(_, model)
    if model == "anthropic/claude-sonnet-4.5" then return 200000 end
    return nil
  end,
}

-- ── argument resolution ───────────────────────────────────────────────

eq(selection.resolve("", providers, catalog).kind, "browse",
  "/model with no arguments browses catalogs")
eq(selection.resolve("   ", providers, catalog).kind, "browse",
  "whitespace-only arguments still browse")

local qualified = selection.resolve("openrouter anthropic/claude-sonnet-4.5", providers, catalog)
eq(qualified.kind, "select", "a qualified pair selects directly")
eq(qualified.provider, "openrouter", "qualified provider is the leading token")
eq(qualified.model, "anthropic/claude-sonnet-4.5", "a model id keeps its slashes")

local unlisted = selection.resolve("chatgpt gpt-6-preview", providers, catalog)
eq(unlisted.kind, "select", "an explicit pair does not require catalog membership")
eq(unlisted.model, "gpt-6-preview", "explicit qualification is the user's own authority")

local bare = selection.resolve("anthropic/claude-sonnet-4.5", providers, catalog)
eq(bare.kind, "select", "a bare model unique across catalogs resolves")
eq(bare.provider, "openrouter", "bare resolution names the only catalog offering it")

local ambiguous = selection.resolve("shared-model", providers, catalog)
eq(ambiguous.kind, "ambiguous", "a bare model in two catalogs is rejected as ambiguous")
eq(#ambiguous.providers, 2, "ambiguity names every candidate provider")
local guidance = selection.ambiguous_message(ambiguous)
assert_true(guidance:find("/model chatgpt shared-model", 1, true) ~= nil,
  "ambiguity guidance offers a runnable qualified command")
assert_true(guidance:find("/model openrouter shared-model", 1, true) ~= nil,
  "ambiguity guidance offers every runnable qualified command")

local unresolved = selection.resolve("no-such-model", providers, catalog)
eq(unresolved.kind, "unresolved", "an unknown bare model is not guessed onto a provider")
assert_true(selection.unresolved_message(unresolved):find("/model <provider> no-such-model", 1, true) ~= nil,
  "unresolved guidance names the qualified form")

-- ── selection lifecycle ───────────────────────────────────────────────

local base = {
  provider = "chatgpt", model = "gpt-5",
  reasoning_effort = "high", max_tokens = 400000,
  current_context_tokens = 1234,
}

local requested, effects = selection.request(base, "openrouter", "anthropic/claude-sonnet-4.5")
eq(#effects, 1, "a selection emits exactly one request")
eq(effects[1].body.kind, "chat.model.set", "the request is the public model-set event")
eq(effects[1].body.provider, "openrouter", "the request names the owning provider")
eq(requested.provider, "chatgpt", "the surface does not adopt an unacknowledged pair")
eq(requested.model, "gpt-5", "the surface does not adopt an unacknowledged model")
eq(requested.pending_model_selection.model, "anthropic/claude-sonnet-4.5",
  "the pending pair is recorded for correlation")

-- A stale ack: right provider, superseded model.
local stale = selection.acknowledge(requested,
  { provider = "openrouter", model = "some-older-model" }, lookups)
eq(stale.model, "gpt-5", "an ack for another model does not move the surface")
eq(stale.pending_model_selection.model, "anthropic/claude-sonnet-4.5",
  "a stale ack leaves the pending selection outstanding")

-- An ack from an unrelated provider is equally stale.
local foreign = selection.acknowledge(requested,
  { provider = "ollama", model = "anthropic/claude-sonnet-4.5" }, lookups)
eq(foreign.provider, "chatgpt", "an ack from a provider that owns no pending pair is ignored")

local adopted = selection.acknowledge(requested,
  { provider = "openrouter", model = "anthropic/claude-sonnet-4.5" }, lookups)
eq(adopted.provider, "openrouter", "the acknowledged provider becomes active")
eq(adopted.model, "anthropic/claude-sonnet-4.5", "the acknowledged model becomes active")
eq(adopted.reasoning_effort, "medium", "the new pair's reasoning default replaces the old effort")
eq(adopted.max_tokens, 200000, "the new pair's context window replaces the old one")
eq(adopted.current_context_tokens, nil, "measured usage of the previous pair is cleared")
eq(adopted.pending_model_selection, nil, "an adopted selection is no longer pending")

-- Adoption happens on the ack, before any turn completes: nothing about the
-- surface's turn state is consulted or required.
eq(adopted.pending, nil, "adoption does not wait on turn completion")

-- Unknown reasoning default and context window clear rather than inherit.
local plain = selection.acknowledge(
  select(1, selection.request(base, "openrouter", "some-model")),
  { provider = "openrouter", model = "some-model" }, lookups)
eq(plain.reasoning_effort, nil, "an unknown pair does not inherit the previous effort")
eq(plain.max_tokens, nil, "an unknown pair does not inherit the previous context window")

-- ── failure handling ──────────────────────────────────────────────────

local rolled_back, did_roll = selection.reject(requested,
  { provider = "openrouter", model = "anthropic/claude-sonnet-4.5", error = "no" })
eq(did_roll, true, "a correlated rejection rolls the selection back")
eq(rolled_back.provider, "chatgpt", "rollback restores the previous provider")
eq(rolled_back.model, "gpt-5", "rollback restores the previous model")
eq(rolled_back.reasoning_effort, "high", "rollback restores the previous reasoning effort")
eq(rolled_back.max_tokens, 400000, "rollback restores the previous context window")
eq(rolled_back.current_context_tokens, 1234, "rollback restores measured context usage")
eq(rolled_back.pending_model_selection, nil, "rollback clears the pending selection")

local uncorrelated, no_roll = selection.reject(requested,
  { provider = "chatgpt", model = "gpt-5" })
eq(no_roll, false, "a rejection for another pair changes nothing")
eq(uncorrelated.pending_model_selection.provider, "openrouter",
  "an uncorrelated rejection leaves the pending selection intact")

local dropped = select(1, selection.provider_unavailable(requested, "openrouter", "login_required"))
eq(dropped.provider, "chatgpt", "losing the pending provider rolls the selection back")
eq(dropped.pending_model_selection, nil, "losing the pending provider clears the pending record")
eq(select(1, selection.provider_unavailable(requested, "openrouter", "connected"))
  .pending_model_selection.provider, "openrouter",
  "a still-connected provider keeps the selection pending")
eq(select(1, selection.provider_unavailable(requested, "chatgpt", "error"))
  .pending_model_selection.provider, "openrouter",
  "an unrelated provider's status does not touch the pending selection")

-- ── unsolicited acknowledgements ──────────────────────────────────────

local hello = selection.acknowledge(base, { provider = "chatgpt", model = "gpt-5-mini" }, lookups)
eq(hello.model, "gpt-5-mini", "an unsolicited ack from the active provider still applies")
local intruder = selection.acknowledge(base, { provider = "openrouter", model = "x" }, lookups)
eq(intruder.model, "gpt-5",
  "an unsolicited ack from a non-active provider cannot take the surface over")

-- ── overlay parity ────────────────────────────────────────────────────

local active = selection.active(requested)
eq(active.provider, "chatgpt", "overlays read the same active provider as the statusline")
eq(active.pending_model, "anthropic/claude-sonnet-4.5",
  "overlays can mark the requested-but-unacknowledged pair")
eq(selection.active(adopted).pending_model, nil,
  "no pair is pending once the selection is adopted")

print("model_selection_test: all assertions passed")
