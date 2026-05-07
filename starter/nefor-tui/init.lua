-- starter/nefor-tui/init.lua — wrapper actor for the nefor-tui Rust
-- binary.
--
-- ## from_plugin (binary → bus)
--
-- Republish every TUI emission verbatim onto the bus. The agentic-loop
-- subscribes via `nefor.bus.on_event` and reacts; other wrappers
-- (provider, tool-gate) see the same envelopes and decide whether to
-- forward them to their peer. The Phase-3 architecture means the
-- provider wrapper's `to_plugin` no longer translates
-- `chat.input.submit` → `<prefix>.prompt` (the orchestration goes
-- through `tool.invoke` instead), so the previous "drop these kinds at
-- ingress" guard isn't needed.
--
-- ## to_plugin (bus → binary)
--
-- Deliver verbatim, skipping self-emissions only. No translation —
-- chat.message.append / chat.stream.delta / etc. flow through to the
-- TUI as-is.
--
-- Unlike the other wrappers, this one DOES NOT short-circuit on
-- `replay_window.active()`. The TUI surface needs every replayed
-- envelope so chat.lua can rebuild its transcript on resume — that is
-- the entire point of the resume UX. Replay-side suppression of fresh
-- side effects (popups for previously-approved tools, fresh DAG-panel
-- mutations from observation envelopes, etc.) lives INSIDE the chat.
-- lua reducer, gated by `state.replay_mode` which the reducer flips
-- on `sessions.replay.start` / `sessions.replay.end`. The other
-- wrappers (tool-gate, openai-provider, …) do skip during replay
-- because their peer plugins would treat the replayed envelopes as
-- fresh invocations and produce duplicate side effects (Bug 5).

local json = nefor.json

local M = {}

function M.spawn_spec(command)
  assert(type(command) == "table", "nefor-tui.spawn_spec: command required")

  local function from_plugin(envs)
    for _, env in ipairs(envs) do
      if type(env.body) == "table" then
        nefor.engine.send(json.encode({
          type = "event",
          from = env.from or "nefor-tui",
          ts   = nefor.engine.now(),
          body = env.body,
        }))
      end
    end
  end

  -- Mechanical loop — one stdin line per envelope, in order. The
  -- resume-rendering optimization from the batch protocol design doc
  -- doesn't live here: the wire shape stays one envelope per line
  -- (each line is a self-contained NCP envelope that the binary
  -- parses + dispatches independently). Coalescing happens INSIDE
  -- the binary's main loop — after pulling the first line in a tick,
  -- it drains every other line currently in its stdin channel before
  -- calling render_if_dirty, so a burst of N envelopes produces one
  -- render pass instead of N. See `plugins/nefor-tui/src/main.rs`'s
  -- `process_envelope` + drain loop for the binary side; the wrapper
  -- just delivers in order and lets the binary batch.
  local function to_plugin(envs)
    for _, env in ipairs(envs) do
      if env.from ~= "nefor-tui" then
        -- Strip framework-only fields (`replay`, …) when encoding for
        -- the wire; the protocol parser rejects unknown envelope
        -- fields.
        nefor.engine.deliver("nefor-tui", json.encode({
          type = env.type,
          from = env.from,
          ts   = env.ts,
          body = env.body,
        }))
      end
    end
  end

  return {
    name        = "nefor-tui",
    command     = command,
    from_plugin = from_plugin,
    to_plugin   = to_plugin,
    receive_msg = function(_) end,
  }
end

return M
