-- starter/nefor-tui/init.lua — wrapper actor for the nefor-tui Rust
-- binary.
--
-- Constructor returns the actor spec for the declarative TUI plugin
-- (`bin/nefor-tui --script chat.lua`). The wrapper's only job is
-- envelope filtering at ingress: certain `chat.*` envelopes the TUI
-- emits are consumed entirely by the agentic-loop and shouldn't fan
-- out to other plugins. Without filtering, the openai-provider
-- wrapper's `to_plugin` translates `chat.input.submit` → `<prefix>.
-- prompt` and ships it to the provider — duplicating the user prompt
-- on every turn.
--
-- The agentic-loop subscribes to the same envelopes via its actor
-- `receive_msg` (broadcast-bus dispatch), so dropping at ingress is
-- safe: the loop already saw the envelope by the time we return nil.
--
-- This wrapper has no `to_plugin` — outbound envelopes targeted at
-- nefor-tui (chat.message.append, chat.stream.delta, …) flow through
-- as-is.

local M = {}

-- Envelopes the TUI emits that the agentic-loop fully owns. Returning
-- nil from from_plugin drops them from the broadcast fan-out, so
-- sibling plugins (provider, tool-gate, reasoner-graph) don't see
-- them. The agentic-loop already received them via its own bus
-- subscription before this filter runs.
--
-- Why each one:
--   * chat.input.submit  — the loop emits the right downstream traffic
--                          (reasoner-graph.run + user echo). Letting
--                          it broadcast also triggers openai-provider's
--                          outer_to, which would send a stray
--                          <prefix>.prompt to the provider.
--   * chat.interrupt_all — the loop owns cancel_all() fan-out. The
--                          wrapper used to call it directly via
--                          for_chat; same coupling lifted into the
--                          loop's receive_msg.
local TUI_DROP_KINDS = {
  ["chat.input.submit"]  = true,
  ["chat.interrupt_all"] = true,
}

function M.spawn_spec(command)
  assert(type(command) == "table", "nefor-tui.spawn_spec: command required")

  local function from_plugin(env)
    if env.type ~= "event" or type(env.body) ~= "table" then return env end
    local kind = env.body.kind
    if type(kind) == "string" and TUI_DROP_KINDS[kind] then
      return nil
    end
    return env
  end

  return {
    name        = "nefor-tui",
    command     = command,
    from_plugin = from_plugin,
    receive_msg = function(_) end,
  }
end

return M
