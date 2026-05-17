-- Reducer for the chat surface. Receives a message + current state,
-- returns (next_state, effects). Effects are NCP envelopes the engine
-- routes onto the bus. Pure update except for `tui.now_ms` reads and
-- `tui.scroll_*` / `tui.copy_to_clipboard` side-effect bindings.

local tui_lib = require("nefor-tui")
local W       = tui_lib.widget

local common    = require("chat.common")
local slash     = require("chat.slash")
local sessions  = require("chat.sessions")
local at_path   = require("chat.at_path")
local history   = require("chat.history")
local dag       = require("chat.dag")
local transcript = require("chat.transcript")
local popups    = require("chat.popups")

local shallow_merge = common.shallow_merge
local NIL_SENTINEL  = common.NIL_SENTINEL
local format_args   = common.format_args

local M = {}

local DOUBLE_ESC_MS = 600

-- Per-model context window. opus/sonnet/haiku → 200k. Other models
-- have unknown context windows; the ctx bar hides until we know.
-- Per-model context window sizes reported by the provider's model list.
-- Populated by chat.models.listed events; keyed by model id.
local model_context_windows = {}

-- The prompt widget's `handle()` consumes a fixed set of kinds when a
-- completion is active (key.up/down/tab/escape) and on every value
-- change. Wrap it so the reducer can fold the state patch back into
-- chat.lua's flat state shape.
local function prompt_widget_opts(state)
  return {
    state       = {
      value          = state.input_value,
      completion     = state.completion,
      history_cursor = state.history_cursor,
    },
    on_change   = "input.changed",
    on_submit   = "input.submit",
    completions = slash.completions(),
    history     = function() return state.prompt_history or {} end,
  }
end

local function fold_prompt_patch(state, patch)
  -- The widget's patch keys are { value, completion, history_cursor }
  -- inside `state.<patch>`; translate them into chat.lua's flat fields.
  local out = {}
  if patch.value          ~= nil then out.input_value    = patch.value          end
  if patch.completion     ~= nil then out.completion     = patch.completion     end
  if patch.history_cursor ~= nil then out.history_cursor = patch.history_cursor end
  return shallow_merge(state, out)
end

function M.update(msg, state)
  local kind = msg.kind or ""

  -- Pure-update prune for stale dag runs + expired toasts.
  do
    local now = tui.now_ms()
    local pruned = dag.prune(state.dag_runs or {}, now)
    if pruned ~= state.dag_runs then
      state = shallow_merge(state, { dag_runs = pruned })
    end
    local toasts = state.toasts
    if toasts ~= nil and #toasts > 0 then
      local kept = {}
      for _, t in ipairs(toasts) do
        if not W.toast.is_expired(t, now) then kept[#kept + 1] = t end
      end
      if #kept ~= #toasts then
        state = shallow_merge(state, { toasts = kept })
      end
    end
  end

  -- ── text_input callbacks ────────────────────────────────────────────
  if kind == "input.changed" then
    local result = W.prompt.handle(prompt_widget_opts(state), msg)
    if result and result.state then
      return fold_prompt_patch(state, result.state), {}
    end
    return state, {}
  end

  if kind == "input.submit" then
    local text = msg.value or ""
    -- Note on @-path autocomplete + Enter: slash submits the highlighted
    -- match because the slash command IS the action; @-paths are file
    -- references embedded in a wider message, so Enter is overwhelmingly
    -- "send my message" — Tab is the right key to insert a completion.
    -- The popup is informational, not modal: closes on submit.
    -- Slash autocomplete open + Enter → run the highlighted match,
    -- regardless of what fragment the user actually typed. Browser-style
    -- combobox semantics: pressing Enter while the dropdown is open
    -- selects the focused option, it doesn't submit the partial query.
    -- This lets `/mo` + Enter execute `/model` when the dropdown shows
    -- `/model` highlighted.
    if state.completion and state.completion.trigger == "/" then
      local c = state.completion
      local m = c.matches and c.matches[c.cursor or 1]
      if m then
        text = "/" .. m.name
      end
    end
    if #text == 0 then return state, {} end
    -- Slash dispatch.
    local cmd, args, _has_ws = slash.parse(text)
    if cmd == "quit" or cmd == "exit" then
      return state, { { kind = "exit" } }
    end
    if cmd == "new" or cmd == "clear" then
      -- `/new` mints a brand-new session on disk in addition to
      -- clearing the visual transcript. Without sessions.new_request,
      -- the on-disk session id stays put and every subsequent submit
      -- lands in the file the picker previewed before — so the picker
      -- only ever showed one growing entry no matter how many `/new`s
      -- the user typed. The starter's sessions module subscribes to
      -- this kind and runs the in-process mint + swap (session_end →
      -- close+prune → open fresh → session_start → resume_done with
      -- replay=0). The chat.interrupt_all envelope is still emitted so
      -- any in-flight streaming aborts immediately rather than waiting
      -- for the session_end teardown to fan out via the broker.
      local cleared = shallow_merge(state, {
        entries = {}, in_flight = NIL_SENTINEL, input_value = "",
        pending = false, completion = NIL_SENTINEL,
        dag_runs = {}, firing_to_node = {},
        turn_started_at = NIL_SENTINEL,
        last_turn_duration_ms = NIL_SENTINEL,
        last_esc_ms = NIL_SENTINEL,
        history_cursor = NIL_SENTINEL,
        popup = NIL_SENTINEL,
      })
      return cleared, {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt_all" } },
        { kind = "send_to", target = "engine",
          body = { kind = "sessions.new_request" } },
      }
    end
    if cmd == "help" then
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = { variant = "help" },
      }), {}
    end
    if cmd == "yolo" then
      local s = shallow_merge(state, { input_value = "", completion = NIL_SENTINEL })
      return s, {
        { kind = "send_to", target = "engine",
          body = { kind = "tool-gate.set_mode", mode = "yolo" } },
      }
    end
    if cmd == "safe" then
      local s = shallow_merge(state, { input_value = "", completion = NIL_SENTINEL })
      return s, {
        { kind = "send_to", target = "engine",
          body = { kind = "tool-gate.set_mode", mode = "normal" } },
      }
    end
    if cmd == "login" or cmd == "logout" then
      if args and #args > 0 then
        -- Direct path: refuse if the named provider doesn't advertise
        -- a login flow. Avoids `/logout ollama` succeeding-silently the
        -- same way the picker would have prevented.
        local supports = (state.supports_login or {})[args]
        if not supports then
          return shallow_merge(state, {
            input_value = "", completion = NIL_SENTINEL,
            popup = {
              variant = "warning",
              title   = "/" .. cmd,
              body    = "Provider `" .. args .. "` doesn't support " .. cmd .. ".",
            },
          }), {}
        end
        local body = { kind = "chat." .. cmd .. "_requested", provider = args }
        return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine", body = body },
        }
      end
      -- No args — open the provider picker. Filter to providers that
      -- advertise `supports_login` via chat.auth.status (so mock + ollama
      -- with a static token aren't offered for a no-op login/logout).
      -- For /login include any state; for /logout require state ==
      -- connected (you can't log out from one you're not logged into).
      local supports = state.supports_login or {}
      local providers = {}
      for n, st in pairs(state.auth or {}) do
        if supports[n] then
          if cmd == "logout" then
            if st == "connected" then
              providers[#providers + 1] = { name = n, state = st }
            end
          else
            providers[#providers + 1] = { name = n, state = st }
          end
        end
      end
      table.sort(providers, function(a, b) return a.name < b.name end)
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = {
          variant   = "login_picker",
          mode      = cmd,                -- "login" | "logout"
          providers = providers,
          cursor    = 1,
        },
      }), {}
    end
    if cmd == "model" then
      if args and #args > 0 then
        -- `/model <name>` — direct switch on the active provider.
        -- Active provider = first connected (alphabetical) when no
        -- explicit selection has been made yet.
        local provider = nil
        local connected = {}
        for n, st in pairs(state.auth or {}) do
          if st == "connected" then connected[#connected + 1] = n end
        end
        table.sort(connected)
        provider = connected[1]
        local body = { kind = "chat.model.set", model = args }
        if provider then body.provider = provider end
        return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine", body = body },
        }
      end
      -- `/model` (no args) — open the picker and fan out one
      -- chat.model.list_requested per *known* provider, connected or
      -- not. Disconnected providers will respond with an empty list,
      -- but their section still renders so the user sees what's
      -- available behind a login.
      local providers = {}
      for n, st in pairs(state.auth or {}) do
        providers[#providers + 1] = { name = n, state = st, models = {} }
      end
      table.sort(providers, function(a, b) return a.name < b.name end)
      local awaiting = {}
      for _, prov in ipairs(providers) do awaiting[prov.name] = true end
      local effects = {}
      for _, prov in ipairs(providers) do
        effects[#effects + 1] = {
          kind = "send_to", target = "engine",
          body = { kind = "chat.model.list_requested", provider = prov.name },
        }
      end
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = {
          variant   = "model_picker",
          providers = providers,
          query     = "",
          cursor    = 1,
          awaiting  = awaiting,
        },
      }), effects
    end
    if cmd == "resume" then
      -- `/resume <session-id>` — direct: emit sessions.resume_request
      -- onto the bus and clear the input. The starter's sessions module
      -- runs the in-process swap (no exit, no sidechannel).
      --
      -- Locally clear the transcript here rather than waiting for the
      -- session_end bus envelope to do it — the session_end handler
      -- deliberately doesn't touch `entries` so that user keystrokes
      -- between /new (or /resume) and the broker's lifecycle round-
      -- trip aren't silently wiped. Replay arrives via push_entry on
      -- each chat.message.append and rebuilds the view from empty.
      if args and #args > 0 then
        local id = args:match("^([%w%-]+)") or args
        return shallow_merge(state, {
          input_value = "", completion = NIL_SENTINEL,
          entries = {}, in_flight = NIL_SENTINEL,
          pending = false, dag_runs = {}, firing_to_node = {},
          turn_started_at = NIL_SENTINEL,
          last_turn_duration_ms = NIL_SENTINEL,
        }), {
          sessions.emit_resume_request(id),
        }
      end
      -- `/resume` (no args) — open the picker.
      local rows = sessions.list_recent(10)
      return shallow_merge(state, {
        input_value = "", completion = NIL_SENTINEL,
        popup = {
          variant  = "session_picker",
          sessions = rows,
          cursor   = 1,
        },
      }), {}
    end
    if cmd ~= nil then
      -- Unknown slash → generic chat.command for user-defined Lua handlers.
      return shallow_merge(state, { input_value = "", completion = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.command", name = cmd, args = args or "" } },
      }
    end
    -- Plain text submit.
    --
    -- We push the user message LOCALLY for instant feedback, then emit
    -- chat.input.submit. The orchestrator's `for_chat` handler echoes
    -- the message back via chat.message.append { role=user } (so the
    -- message persists in the session log and replays on resume); the
    -- corresponding handler below dedupes that round-trip against
    -- pending_user_echo so we render once locally + once on replay,
    -- never twice live.
    --
    -- `@path` preprocessor: the wire envelope, the local user bubble,
    -- and the dedup marker all carry the EXPANDED text. The user sees
    -- exactly what the model receives (transparency); the
    -- orchestrator's echo round-trips through the same dedup gate.
    -- Prompt-history (recall via arrow-up) keeps the ORIGINAL @-form
    -- so a recalled prompt edits like a fresh one and re-expands at
    -- next submit (file contents may have changed in the meantime —
    -- the user re-submitting expects the current state, not a
    -- snapshot from the original turn).
    local wire_text = at_path.expand(text)
    local with_user = transcript.push_entry(state, { role = "user", text = wire_text, kind = "text" })
    -- Prepend to prompt_history (newest at index 1) and cap. History
    -- recall reads from index 1, so prepending keeps the cursor model
    -- simple — Up = older = larger index, Down = newer = smaller.
    -- Mirror to disk so the entry survives a nefor restart.
    local hist = { text }
    for i, v in ipairs(state.prompt_history or {}) do
      if i >= history.INPUT_HISTORY_MAX then break end
      hist[#hist + 1] = v
    end
    history.persist(hist)
    local cleared = shallow_merge(with_user, {
      input_value = "", pending = true,
      turn_started_at = tui.now_ms(), completion = NIL_SENTINEL,
      prompt_history = hist,
      history_cursor = NIL_SENTINEL,
      -- Mark the next bus-delivered chat.message.append with this
      -- exact text + role as the orchestrator's persist-echo and
      -- swallow it. Cleared after one match — sequential identical
      -- submits each set their own marker on submit, so the second
      -- echo doesn't get eaten by the first marker.
      pending_user_echo = wire_text,
    })
    -- Re-pin the transcript to the bottom: stick_to = "end" only
    -- auto-follows new content while was_at_end is true, so a user
    -- who scrolled up to read older context and submits a new prompt
    -- would otherwise stay parked mid-transcript watching their fresh
    -- message + the incoming response render off-screen below the
    -- viewport. scroll_into_view flips the flag on the next paint so
    -- the auto-follow re-engages for the streaming response too.
    tui.scroll_into_view("transcript")
    return cleared, {
      { kind = "send_to", target = "engine",
        body = { kind = "chat.input.submit", text = wire_text } },
    }
  end

  -- ── keyboard shortcuts ──────────────────────────────────────────────
  -- Ctrl+C and Ctrl+D both exit. Raw-mode terminals deliver these as
  -- key events (not signals), so the app must terminate explicitly.
  if kind == "key.ctrl_c" or kind == "key.ctrl_d" then
    return state, { { kind = "exit" } }
  end

  if kind == "key.ctrl_b" then
    return shallow_merge(state, { show_sidebar = not state.show_sidebar }), {}
  end

  if kind == "key.ctrl_o" then
    -- Global toggle for tool I/O + reasoning expansion.
    return shallow_merge(state, { expanded_details = not state.expanded_details }), {}
  end

  if kind == "key.?" or kind == "key.shift_?" then
    -- ? opens help only when the input is empty (so users can type ?
    -- in regular messages). Otherwise it bubbles into the input field.
    if state.input_value == "" then
      return shallow_merge(state, { popup = { variant = "help" } }), {}
    end
    return state, {}
  end

  -- Info / warning / error popups are dismiss-only and accept Esc,
  -- Enter, or Q. Earlier the popup said "Esc / Q to close" but only
  -- Esc was wired; Enter is the natural confirm key when the popup is
  -- a notification.
  if state.popup
     and (state.popup.variant == "info"
       or state.popup.variant == "warning"
       or state.popup.variant == "error")
     and (kind == "key.escape" or kind == "key.enter"
       or kind == "key.q" or kind == "key.Q") then
    return shallow_merge(state, { popup = NIL_SENTINEL }), {}
  end

  if kind == "key.escape" then
    -- 1) close popup
    local has_toast = state.toasts and #state.toasts > 0
    if state.popup or has_toast then
      -- Tool permission ESC = deny.
      if state.popup and state.popup.variant == "tool_permission" then
        local id = state.popup.id
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = { kind = "tool.permission_response", id = id, decision = "deny" } },
        }
      end
      return shallow_merge(state, { popup = NIL_SENTINEL, toasts = {} }), {}
    end
    -- 2) close completion dropdown (slash or @-path)
    if state.completion ~= nil then
      return shallow_merge(state, { completion = NIL_SENTINEL }), {}
    end
    -- 3) cancel prompt-history navigation (clear recalled value)
    if state.history_cursor ~= nil then
      return shallow_merge(state, {
        input_value    = "",
        history_cursor = NIL_SENTINEL,
      }), {}
    end
    -- 4) double-ESC escalation
    local now = tui.now_ms()
    if state.last_esc_ms and (now - state.last_esc_ms) <= DOUBLE_ESC_MS then
      local interrupted = dag.interrupt_all(state, now)
      return shallow_merge(interrupted, { last_esc_ms = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt_all" } },
      }
    end
    -- 4) single ESC interrupts the current turn
    if state.pending or state.in_flight ~= nil then
      local interrupted = dag.interrupt_all(state, now)
      return shallow_merge(interrupted, { last_esc_ms = now }), {
        { kind = "send_to", target = "engine",
          body = { kind = "chat.interrupt" } },
      }
    end
    -- Stamp anyway so a follow-up ESC within the window can escalate.
    return shallow_merge(state, { last_esc_ms = now }), {}
  end

  -- Tool permission popup keys. Routes to tool-gate via broadcast event
  -- (target hint is documentation-only; tool-gate matches by `id`).
  -- A / Enter → approve, D → deny. Esc handled in the popup-close
  -- branch above — also denies. The footer chrome advertises the same.
  if state.popup and state.popup.variant == "tool_permission" then
    if kind == "key.a" or kind == "key.A" or kind == "key.enter" then
      local id = state.popup.id
      return shallow_merge(state, { popup = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "approve" } },
      }
    end
    if kind == "key.d" or kind == "key.D" then
      local id = state.popup.id
      return shallow_merge(state, { popup = NIL_SENTINEL }), {
        { kind = "send_to", target = "engine",
          body = { kind = "tool.permission_response", id = id, decision = "deny" } },
      }
    end
  end

  -- Model + session picker popups: delegate cursor/filter handling to
  -- W.picker.handle. Each popup's state lives under state.popup; when
  -- a handler returns, we fold its state patch into the popup slot
  -- and emit effects through the caller-side on_select callback. We
  -- gate the picker delegation on key.* events so non-key messages
  -- (chat.models.listed, chat.model.set_ack, etc.) keep flowing to
  -- their dedicated handlers below.
  if state.popup and state.popup.variant == "model_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    -- Flatten provider sections into a list of selectable rows for
    -- the picker widget. Filter each provider's models by the typed
    -- query so cursor navigation only lands on visible models.
    local q_lc = (p.query or ""):lower()
    local flat_rows = {}
    for _, prov in ipairs(p.providers or {}) do
      for _, m in ipairs(prov.models or {}) do
        local s = tostring(m):lower()
        if q_lc == "" or s:find(q_lc, 1, true) ~= nil then
          flat_rows[#flat_rows + 1] = { provider = prov.name, model = m }
        end
      end
    end
    local result = W.picker.handle({
      state   = { cursor = p.cursor or 1, query = p.query or "" },
      entries = function() return flat_rows end,
      -- flat_rows is already query-filtered (matches the section view).
      -- The widget's default_filter would re-filter via tostring(entry)
      -- on each {provider, model} table → "table: 0x…" → zero matches
      -- → arrow keys silently do nothing. Pass identity instead.
      filter  = function(entries, _q) return entries end,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil then
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = {
              kind     = "chat.model.set",
              provider = result.selected.provider,
              model    = result.selected.model,
            } },
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
  end

  -- Login/logout picker: pick a provider to authenticate or revoke.
  -- Emission kind switches on popup.mode ("login" → chat.login_requested,
  -- "logout" → chat.logout_requested). No filter input — provider lists
  -- stay short.
  if state.popup and state.popup.variant == "login_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local rows = p.providers or {}
    local result = W.picker.handle({
      state       = { cursor = p.cursor or 1 },
      entries     = function() return rows end,
      show_search = false,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil and result.selected.name then
        local mode = p.mode or "login"
        return shallow_merge(state, { popup = NIL_SENTINEL }), {
          { kind = "send_to", target = "engine",
            body = {
              kind     = "chat." .. mode .. "_requested",
              provider = result.selected.name,
            } },
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
    return state, {}
  end

  -- Session picker: same shape as model picker, no filter input. Esc
  -- handled in the popup-close branch above (closes without emitting).
  -- All other keys swallow so they don't bubble.
  if state.popup and state.popup.variant == "session_picker"
     and kind:sub(1, 4) == "key." then
    local p = state.popup
    local rows = p.sessions or {}
    local result = W.picker.handle({
      state       = { cursor = p.cursor or 1 },
      entries     = function() return rows end,
      show_search = false,
    }, msg)
    if result ~= nil then
      if result.selected ~= nil and result.selected.id then
        return shallow_merge(state, {
          popup = NIL_SENTINEL,
          entries = {}, in_flight = NIL_SENTINEL,
          pending = false, dag_runs = {}, firing_to_node = {},
          turn_started_at = NIL_SENTINEL,
          last_turn_duration_ms = NIL_SENTINEL,
        }), {
          sessions.emit_resume_request(result.selected.id),
        }
      end
      return shallow_merge(state, {
        popup = shallow_merge(p, result.state),
      }), {}
    end
    return state, {}
  end

  -- Slash and @-path autocomplete keys (when completion popup open).
  -- The prompt widget consumes key.up/down/tab/escape against the
  -- active completion; chat.lua folds its state patch back into the
  -- flat reducer shape.
  if state.completion ~= nil then
    local result = W.prompt.handle(prompt_widget_opts(state), msg)
    if result ~= nil then
      return fold_prompt_patch(state, result.state or {}), {}
    end
  end

  -- Scroll keys: route to the active popup's scrollable when a popup
  -- is open; otherwise to the transcript. Popups are wrapped by
  -- W.popup.view with `scroll_key = "popup_<variant>"`, so the same
  -- tui.scroll_* API drives both. Up/Down on the transcript surface
  -- ALSO drive prompt-history recall when no popup is active and the
  -- input is empty / already navigating — handled separately below.
  local function active_scroll_key()
    if state.popup then return popups.scroll_key(state.popup.variant) end
    return nil
  end

  local function route_scroll(delta_or_fn)
    local target = active_scroll_key() or "transcript"
    delta_or_fn(target)
  end

  if kind == "key.pageup" then
    route_scroll(function(k) tui.scroll_by(k, -10) end)
    return state, {}
  end
  if kind == "key.pagedown" then
    route_scroll(function(k) tui.scroll_by(k, 10) end)
    return state, {}
  end
  if kind == "key.up" or kind == "key.down" then
    -- Up/Down on the chat surface: when no popup owns scroll routing,
    -- the prompt widget gets first dibs (history nav when the input
    -- is empty or already navigating). If the widget didn't consume,
    -- the key routes to scroll.
    if active_scroll_key() == nil then
      local result = W.prompt.handle(prompt_widget_opts(state), msg)
      if result ~= nil then
        return fold_prompt_patch(state, result.state or {}), {}
      end
    end
    local delta = (kind == "key.up") and -1 or 1
    route_scroll(function(k) tui.scroll_by(k, delta) end)
    return state, {}
  end
  if kind == "key.home" then
    route_scroll(function(k) tui.scroll_to(k, 0) end)
    return state, {}
  end
  if kind == "key.end" then
    route_scroll(function(k) tui.scroll_into_view(k) end)
    return state, {}
  end

  -- ── session lifecycle ───────────────────────────────────────────────
  -- The starter's `sessions` module emits four control events on the
  -- bus. `session_end` and `session_start` bracket a resume swap;
  -- `resume_done` is the "we're back, finalise rendering" signal.
  -- During replay (between session_start and resume_done) we paint
  -- envelopes normally — chat.message.append, chat.stream.* land in
  -- transcript exactly the way they would on a live turn. The resume
  -- envelopes ARE the past, so rendering them rebuilds the prior view.
  if kind == "sessions.session_end" then
    -- Tear down ephemeral turn state — but DO NOT touch `entries`.
    --
    -- Rationale: session_end arrives via the bus, on a different
    -- broker tick than the user's keystroke that triggered it (a
    -- /new or /resume submit). If the user typed their first prompt
    -- in the new session before this envelope landed, `entries`
    -- already holds their locally-pushed message — and wiping it
    -- here is exactly the race that made the user's prompt
    -- invisible while the orchestrator's tool_call still painted.
    -- The transcript clear is owned by the trigger paths instead:
    -- /new clears locally in its slash-command handler; /resume
    -- clears locally too. For a /resume that lands replay envelopes,
    -- push_entry on each replayed chat.message.append rebuilds the
    -- prior view directly — no wipe needed here.
    --
    -- pending_user_echo is preserved for a similar reason: if the
    -- user submitted text moments ago, the echo's defensive dedup
    -- (chat.message.append handler) is what protects against
    -- double-rendering. Clearing the marker here would mean the
    -- echo arrives, finds no marker, and unconditionally pushes —
    -- producing the OPPOSITE bug (user line rendered twice). The
    -- marker self-clears the moment the echo lands or the next
    -- /new fires.
    return shallow_merge(state, {
      in_flight        = NIL_SENTINEL,
      pending          = false,
      turn_started_at  = NIL_SENTINEL,
      last_turn_duration_ms = NIL_SENTINEL,
      popup            = NIL_SENTINEL,
      toasts           = {},
      completion       = NIL_SENTINEL,
      dag_runs         = {},
    }), {}
  end

  if kind == "sessions.session_start" then
    -- dag_runs always cleared: a fresh session is a fresh DAG-context
    -- boundary regardless of which path we took to get here.
    -- session_end's clear isn't enough on its own — if a session swap
    -- doesn't go cleanly through session_end, or if a run was
    -- mid-flight when the swap fired, stale runs would otherwise
    -- stack on top of new ones in the panel.
    --
    -- Boot path: state is already empty; the entries-wipe that USED
    -- to live here turned out to break ncp.lua's replay-on-attach
    -- (boot session_start delivered AFTER the user's first prompt
    -- nuked the local-push), so we deliberately do nothing for
    -- entries here. dag_runs is independent of that race (no
    -- pre-boot dispatch path) so we still clear it.
    --
    -- Replay-mode flip is driven by sessions.replay.start /
    -- sessions.replay.end markers below — the framing-marker
    -- contract is the canonical replay window.
    return shallow_merge(state, { dag_runs = {}, firing_to_node = {} }), {}
  end

  if kind == "sessions.replay.start" then
    -- Replay started — suppress UI side effects that would re-trigger
    -- against envelopes the user already saw the first time round.
    -- Notable example: the tool.permission_request popup. The user
    -- already approved in the original session (its decision is
    -- recorded in the jsonl); a fresh popup would be a re-prompt.
    return shallow_merge(state, { replay_mode = true }), {}
  end

  if kind == "sessions.replay.end" then
    -- Replay finished — flip back to live so future envelopes drive
    -- popups and other side effects normally again.
    return shallow_merge(state, { replay_mode = NIL_SENTINEL }), {}
  end

  if kind == "chat.reset" then
    -- agentic_workflow's `teardown_for_session_end` broadcasts
    -- chat.reset so the provider's chat-history map clears. The TUI
    -- receives it too (broadcast doesn't filter peers), but the
    -- transcript clear that USED to live here is redundant —
    -- sessions.session_end fires alongside chat.reset and already
    -- wipes entries. Pinning a no-op handler instead of letting the
    -- envelope fall through is intentional: it documents that the
    -- TUI deliberately ignores chat.reset, so a future contributor
    -- who's tempted to "do something on reset" lands here first and
    -- sees the comment explaining why session_end owns the clear.
    return state, {}
  end

  -- ── inbound chat-contract events ────────────────────────────────────
  if kind == "chat.message.append" then
    local text = msg.text or ""
    if #text == 0 then return state, {} end
    -- Round-trip echo from the orchestrator's `for_chat` handler:
    -- when the user submits, we push locally for instant feedback AND
    -- emit chat.input.submit to the bus. The orchestrator replies with
    -- chat.message.append { role=user, text=<same> } so the user
    -- message lands in the session log (and replays). Live, that
    -- round-trip would render the same line twice; eat it once when
    -- the marker matches.
    --
    -- BUT: only swallow the echo if the local push actually landed in
    -- state.entries. The marker by itself isn't enough — if a
    -- session-lifecycle event wiped entries between the local push
    -- and this echo, eating the echo would leave the transcript with
    -- NO user line. Only the orchestrator's echo can re-paint the user
    -- line in that case, so let it through. The check is "the tail of
    -- entries is a user-role entry with matching text" — that's
    -- exactly the shape the local push leaves.
    local role = msg.role or "system"
    if role == "user"
       and state.pending_user_echo ~= nil
       and state.pending_user_echo == text then
      local entries = state.entries or {}
      local tail = entries[#entries]
      local local_push_landed = tail
        and tail.role == "user"
        and tail.text == text
      if local_push_landed then
        return shallow_merge(state, { pending_user_echo = NIL_SENTINEL }), {}
      end
      -- Marker stranded by an intervening clear — fall through and
      -- push the echo so the user line is still visible. Clear the
      -- marker so a future genuine duplicate can't ride this branch.
      return transcript.push_entry(
        shallow_merge(state, { pending_user_echo = NIL_SENTINEL }),
        { role = role, text = text, kind = "text" }
      ), {}
    end
    -- System messages always indicate the turn ended (interrupted,
    -- error from the provider, etc.) — clear the thinking spinner
    -- and turn-elapsed counter so the UI doesn't sit on
    -- "[thinking... Ns]" forever after the orchestrator gives up.
    local turn_state = role == "system"
      and { pending = false, turn_started_at = NIL_SENTINEL }
      or  {}

    -- AGENTS.md auto-load: tool-gate's loader emits a system message
    -- shaped as `[Loaded <path> because tool call touched a file in
    -- <dir>. This is project guidance for that directory, not a user
    -- request.]\n\n<contents>`. The text needs to ride into model
    -- context (the wire envelope is unchanged), but in the UI it's
    -- noisy guidance — render it as a foldable block instead of a
    -- plain inline system message. Pattern is unique enough to match
    -- safely (the bracketed prelude is generated by one code path).
    if role == "system" then
      local path, dir = text:match(
        "^%[Loaded (.-) because tool call touched a file in (.-)%. This is project guidance for that directory, not a user request%.%]")
      if path and dir then
        -- Sub-graph routing: tool-gate stamps `chat_id` on agents_md
        -- emissions. If that chat_id maps to a known sub-graph node
        -- (registered via `graph.node.chat.bound` when the agent
        -- reasoner spun up), route to the DAG sidebar's "last tool"
        -- slot for that node and DO NOT add to the main transcript.
        -- The lead's chat history is independent — only the visible
        -- transcript needs to suppress it. If chat_id is set but we
        -- have no binding for it, treat as a sub-graph emission for a
        -- node we haven't seen yet (still drop from main chat —
        -- rendering it would be the bug we're fixing).
        local mc = msg.chat_id
        if type(mc) == "string" and #mc > 0 then
          local binding = state.chat_id_to_node and state.chat_id_to_node[mc]
          if binding then
            local now = tui.now_ms()
            local synth = "AGENTS.md(" .. path .. ")"
            return shallow_merge(
              dag.node_tool_invoked(state, binding.run_id, binding.node_id, synth, nil, now),
              turn_state
            ), {}
          end
          -- Sub-chat we don't have a binding for — drop silently
          -- rather than leak into the main transcript.
          return shallow_merge(state, turn_state), {}
        end
        local body = text:match("\n\n(.*)$") or ""
        return transcript.push_entry(shallow_merge(state, turn_state), {
          kind = "agents_md",
          role = "system",
          path = path,
          dir  = dir,
          text = body,
        }), {}
      end
    end

    return transcript.push_entry(shallow_merge(state, turn_state), {
      role = role, text = text, kind = "text",
    }), {}
  end

  if kind == "chat.stream.delta" then
    local t = msg.text or msg.delta or ""
    if #t == 0 then return state, {} end
    return transcript.append_assistant_delta(state, t), {}
  end

  if kind == "chat.stream.end" then
    return transcript.finalize_assistant(state, msg.text, msg.model, msg.duration_ms), {}
  end

  if kind == "chat.stream.reasoning_delta" then
    local t = msg.text or msg.delta or ""
    if #t == 0 then return state, {} end
    return transcript.append_reasoning_delta(state, t), {}
  end

  if kind == "chat.stream.reasoning_end" then
    return transcript.finalize_reasoning(state, msg.duration_ms), {}
  end

  if kind == "chat.session.stats" then
    local stats = shallow_merge(state.stats or {}, {})
    for k, v in pairs(msg) do
      if k ~= "kind" then stats[k] = v end
    end
    local s = shallow_merge(state, { stats = stats })
    if msg.model then
      local mt = msg.max_context_tokens
        or model_context_windows[msg.model]
        or state.max_tokens
      s = shallow_merge(s, { model = msg.model, max_tokens = mt })
    end
    return s, {}
  end

  if kind == "chat.tool.start" then
    -- Preserve the raw input table for `tool_salient`.
    local input_str
    if type(msg.input) == "string" then input_str = msg.input
    elseif type(msg.input) == "table" then input_str = "(object)"
    else input_str = "" end
    return transcript.push_entry(state, {
      kind   = "tool_call",
      role   = "tool",
      id     = msg.id or "",
      name   = msg.name or "?",
      input  = input_str,
      input_table = type(msg.input) == "table" and msg.input or nil,
    }), {}
  end

  if kind == "chat.tool.end" then
    return transcript.attach_tool_end(state, msg.id or "", msg.output or "", msg.error == true), {}
  end

  -- Sub-graph result block. agentic-loop emits this from its run-close
  -- handler so the user can see what the dispatched sub-graph actually
  -- returned (the model-bound user-role echo separately feeds the
  -- lead's chat history). Distinct entry kind so entries.lua renders
  -- with its own glyph + style; survives /resume because the chat-bridge
  -- forwards live envelopes into the session log and the reducer
  -- re-runs on replay.
  if kind == "chat.graph_result.append" then
    return transcript.push_entry(state, {
      kind        = "graph_result",
      role        = "graph",
      run_id      = msg.run_id or "",
      status      = msg.status or "success",
      nodes       = msg.nodes or {},
      output      = msg.output,
      error       = msg.error,
      duration_ms = msg.duration_ms,
    }), {}
  end

  -- ── plan-message contract (lead-workflow `write-review` tool) ──────
  -- The lead-workflow actor's write-review tool fires a plan envelope
  -- the chat surface renders as a yellow-bordered "plan" entry. This
  -- block is render-only on chat.lua's side — the plan body is NOT
  -- added to model context by anything in this file (the submit
  -- reducer's chat.input.submit emit carries only the user's typed
  -- text). The model already saw the plan as the tool call's args, so
  -- re-forwarding it via chat.message.append would be a duplication.
  if kind == "chat.plan.append" then
    local text = msg.text or ""
    if #text == 0 then return state, {} end
    -- Dedup by submitted_at: the lead-workflow actor's plan.submitted
    -- reducer fires chat.plan.append on every handling — live AND
    -- replay. Sessions persists the live emission, then /resume
    -- replays both the persisted envelope and the re-emit. Without
    -- this guard the same plan would produce two yellow boxes after
    -- every /resume.
    local submitted_at = msg.submitted_at
    if submitted_at ~= nil then
      for _, v in ipairs(state.entries) do
        if v.kind == "plan" and v.submitted_at == submitted_at then
          return state, {}
        end
      end
    end
    return transcript.push_entry(state, {
      kind         = "plan",
      text         = text,
      submitted_at = submitted_at,
      status       = "pending",
    }), {}
  end

  -- Approval/rejection arrives from the lead-workflow actor after the
  -- user types `/approve` or `/reject`. There is one active plan at a
  -- time and verdicts target the most recent pending plan entry — we
  -- update its status in place so the visual state changes (border
  -- colour, check/cross subtitle) but the plan stays in the transcript
  -- so the user can scroll back to see what was decided.
  if kind == "lead-workflow.plan.approved" then
    local approved = (msg.approved == true)
    local entries = {}
    local target_idx
    for i, v in ipairs(state.entries) do
      if v.kind == "plan" and v.status == "pending" then
        target_idx = i
      end
      entries[i] = v
    end
    if target_idx == nil then return state, {} end
    entries[target_idx] = shallow_merge(entries[target_idx], {
      status = approved and "approved" or "rejected",
    })
    return shallow_merge(state, { entries = entries }), {}
  end

  if kind == "chat.popup" then
    local v = msg.level or "info"
    return shallow_merge(state, {
      popup = {
        variant = v,
        title   = msg.title or v,
        body    = msg.message or msg.text or "",
        source  = msg.source,
      },
    }), {}
  end

  if kind == "chat.toast" then
    local now = tui.now_ms()
    local ttl = msg.ttl_ms or 2000
    local toasts = {}
    for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
    toasts[#toasts + 1] = {
      id            = msg.id or tostring(now) .. "-" .. tostring(#toasts + 1),
      text          = msg.text or "",
      level         = msg.level or "info",
      started_at_ms = now,
      ttl_ms        = ttl,
    }
    return shallow_merge(state, { toasts = toasts }), {}
  end

  if kind == "chat.model.set_ack" then
    -- Replayed set_ack envelopes carry the model the OLD session was
    -- bound to — replaying them onto the live state would clobber
    -- whatever the user set via /model in the LIVE session before the
    -- /resume (chat.model.set_ack is persisted, so the original
    -- session's mock-provider hello → set_ack lives in the jsonl).
    -- The agentic-loop owns the live provider/model and doesn't
    -- replay chat.model.set on its own input gate, so its state stays
    -- correct; chat.lua mirrors that posture by ignoring replayed
    -- set_ack envelopes — only LIVE ones drive the badge.
    if state.replay_mode then return state, {} end
    return shallow_merge(state, {
      model = msg.model or state.model,
      max_tokens = model_context_windows[msg.model] or state.max_tokens,
    }), {}
  end

  if kind == "chat.models.listed" then
    -- Absorb per-model context windows if the provider reported them.
    if type(msg.context_windows) == "table" then
      for model_id, ctx_size in pairs(msg.context_windows) do
        if type(ctx_size) == "number" and ctx_size > 0 then
          model_context_windows[model_id] = ctx_size
        end
      end
    end
    -- Update the open model_picker popup if one is up; otherwise drop.
    if not (state.popup and state.popup.variant == "model_picker") then
      return state, {}
    end
    local provider = msg.provider or ""
    local list = msg.models or {}
    local models = {}
    if type(list) == "table" then
      for _, m in ipairs(list) do models[#models + 1] = tostring(m) end
    end
    table.sort(models)
    local new_providers = {}
    local found = false
    for _, prov in ipairs(state.popup.providers or {}) do
      if prov.name == provider then
        new_providers[#new_providers + 1] = shallow_merge(prov, { models = models })
        found = true
      else
        new_providers[#new_providers + 1] = prov
      end
    end
    -- If the picker opened before this provider was known, append it.
    -- State from chat.auth.status will fill in on the next status.
    if not found then
      new_providers[#new_providers + 1] = {
        name = provider, state = state.auth and state.auth[provider] or "unknown",
        models = models,
      }
      table.sort(new_providers, function(a, b) return a.name < b.name end)
    end
    local prev_awaiting = state.popup.awaiting or {}
    local new_awaiting = {}
    for k, v in pairs(prev_awaiting) do new_awaiting[k] = v end
    new_awaiting[provider] = nil
    return shallow_merge(state, {
      popup = shallow_merge(state.popup, {
        providers = new_providers,
        awaiting  = new_awaiting,
      }),
    }), {}
  end

  if kind == "chat.auth.status" then
    local provider = msg.provider or ""
    local status = msg.status or msg.state or "unknown"
    if provider == "" then return state, {} end
    local auth = {}
    for k, v in pairs(state.auth or {}) do auth[k] = v end
    auth[provider] = status
    -- Carry forward the provider's `supports_login` capability flag.
    -- Default false: providers that don't set it on their auth.status
    -- are hidden from the /login and /logout pickers.
    local supports = {}
    for k, v in pairs(state.supports_login or {}) do supports[k] = v end
    if msg.supports_login ~= nil then
      supports[provider] = msg.supports_login and true or false
    end
    -- If an open model picker has a section for this provider, update
    -- its state so [connected]/[disconnected] tag re-colours live.
    local new_popup = state.popup
    if state.popup and state.popup.variant == "model_picker"
       and state.popup.providers then
      local found_section = false
      local new_providers = {}
      for _, prov in ipairs(state.popup.providers) do
        if prov.name == provider then
          found_section = true
          new_providers[#new_providers + 1] = shallow_merge(prov, { state = status })
        else
          new_providers[#new_providers + 1] = prov
        end
      end
      -- New provider not in the picker yet: insert (alphabetical).
      if not found_section then
        new_providers[#new_providers + 1] = {
          name = provider, state = status, models = {},
        }
        table.sort(new_providers, function(a, b) return a.name < b.name end)
      end
      new_popup = shallow_merge(state.popup, { providers = new_providers })
    end
    return shallow_merge(state, {
      auth = auth, supports_login = supports, popup = new_popup,
    }), {}
  end

  if kind == "chat.tool.popup_request" then
    -- Replay path: the user already approved in the original session
    -- and the decision is in the jsonl — popping a fresh approval
    -- popup would be a re-prompt for the same call. Drop the request
    -- silently; the matching tool.permission_response is also in the
    -- jsonl and will replay through tool-gate's normal handler.
    if state.replay_mode then return state, {} end
    -- Wire shape from the tool-validator actor:
    --   { kind = "chat.tool.popup_request",
    --     id   = "<provider outer id>",
    --     tool = "<tool name>",
    --     args = <JSON object> }
    -- The validator is the only emitter — it owns the decision split
    -- between auto-approve, auto-deny, and "ask the human". tool-gate
    -- still emits `chat.tool.permission_request`; the validator
    -- forwards as popup_request when it defers. We render `args` into
    -- a small key/value summary; the response goes back as
    -- `tool.permission_response { id, decision }` (handled in the
    -- popup keymap below). `msg.input_pretty` and `msg.name` are kept
    -- as forward-compatible fallbacks in case future emitters pre-format.
    local args = msg.args
    local body
    if msg.input_pretty ~= nil then
      body = tostring(msg.input_pretty)
    elseif type(args) == "table" then
      body = format_args(args)
    elseif args ~= nil then
      body = tostring(args)
    else
      body = ""
    end
    return shallow_merge(state, {
      popup = {
        variant = "tool_permission",
        tool    = msg.tool or msg.name or "?",
        id      = msg.id,
        body    = body,
        source  = msg.source,
      },
    }), {}
  end

  if kind == "tool-gate.mode_changed" then
    return shallow_merge(state, { gate_yolo = msg.mode == "yolo" }), {}
  end

  -- DAG observation. Each handler short-circuits during replay: the
  -- graph.* envelopes seeded into the resumed session's jsonl are
  -- snapshots from the prior live run, not fresh dispatches, and
  -- mutating dag_runs from them would re-light a panel that should
  -- start clean (sessions.session_start clears it).
  --
  -- Wire shape from reasoner-graph:
  --   * graph.run_started  { run_id, total_nodes }
  --   * graph.node.fired   { run_id, node_id, firing_id, reasoner }
  --     — paired observer for each tool.invoke dispatch.
  --   * tool.result        { id, result | error }
  --     — id == firing_id closes one node; id == run_id closes the run.
  -- We also keep a firing_id → (run_id, node_id) map per state so
  -- tool.result events can be routed back to the right node without
  -- parsing dispatch traffic.
  if kind == "graph.run_started" then
    if state.replay_mode then return state, {} end
    local now = tui.now_ms()
    return dag.run_started(state, msg.run_id or "", msg.total_nodes or 0, now), {}
  end
  if kind == "graph.node.fired" then
    if state.replay_mode then return state, {} end
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    if (msg.firing_id or "") == "" then return state, {} end
    local now = tui.now_ms()
    local with_dispatch = dag.node_dispatched(state, msg.run_id, msg.node_id, msg.reasoner or "", now)
    local prev_map = with_dispatch.firing_to_node or {}
    local next_map = {}
    for k, v in pairs(prev_map) do next_map[k] = v end
    next_map[msg.firing_id] = { run_id = msg.run_id, node_id = msg.node_id }
    return shallow_merge(with_dispatch, { firing_to_node = next_map }), {}
  end
  -- Agent-reasoner progress: the agent inside (run_id, node_id) just
  -- dispatched a sub-tool to tool-gate. Surface the tool name as a
  -- second indented line on the node row so the user can tell what
  -- each parallel agent is actually doing.
  if kind == "graph.node.tool.invoke" then
    if state.replay_mode then return state, {} end
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    if type(msg.tool_name) ~= "string" or #msg.tool_name == 0 then return state, {} end
    local now = tui.now_ms()
    return dag.node_tool_invoked(state, msg.run_id, msg.node_id, msg.tool_name, msg.tool_args, now), {}
  end
  -- Sub-agent chat-id binding. The agent reasoner emits this once per
  -- firing right after minting its chat_id; the chat surface stores
  -- chat_id → (run_id, node_id) so emissions tagged with that chat_id
  -- (notably tool-gate's AGENTS.md auto-load system message) can be
  -- routed to the DAG sidebar instead of leaking into the main
  -- transcript. See the agents_md branch in the chat.message.append
  -- handler for the consumer side.
  if kind == "graph.node.chat.bound" then
    if (msg.run_id or "") == "" or (msg.node_id or "") == "" then return state, {} end
    if type(msg.chat_id) ~= "string" or #msg.chat_id == 0 then return state, {} end
    local prev = state.chat_id_to_node or {}
    local next_map = {}
    for k, v in pairs(prev) do next_map[k] = v end
    next_map[msg.chat_id] = { run_id = msg.run_id, node_id = msg.node_id }
    return shallow_merge(state, { chat_id_to_node = next_map }), {}
  end
  if kind == "tool.result" then
    if state.replay_mode then return state, {} end
    local id = msg.id
    if type(id) ~= "string" or id == "" then return state, {} end
    local now = tui.now_ms()
    -- Run-close: id matches a tracked run.
    if state.dag_runs and state.dag_runs[id] then
      local result = msg.result
      local status, results
      if type(result) == "table" then
        status  = result.status
        results = result.results
      end
      return dag.run_complete(state, id, status, results, now), {}
    end
    -- Per-firing close: look up firing_id → (run_id, node_id) map.
    local map_entry = (state.firing_to_node or {})[id]
    if map_entry then
      local run_id  = map_entry.run_id
      local node_id = map_entry.node_id
      local has_output = msg.result ~= nil
      local has_error  = msg.error  ~= nil
      local next_state = dag.node_result(state, run_id, node_id, has_output, has_error, now)
      local next_map = {}
      for k, v in pairs(state.firing_to_node or {}) do next_map[k] = v end
      next_map[id] = nil
      return shallow_merge(next_state, { firing_to_node = next_map }), {}
    end
    return state, {}
  end

  -- Mouse drag-to-select: the engine extracts the highlighted text
  -- from the framebuffer and dispatches `mouse.selection`. Policy
  -- lives here, not the engine — we copy non-empty selections to the
  -- clipboard and surface a short toast acknowledging the action.
  if kind == "mouse.selection" then
    local text = msg.text or ""
    if #text > 0 then
      -- Read `now` BEFORE calling tui.copy_to_clipboard. The clipboard
      -- binding hits a system-wide pasteboard (NSPasteboard on macOS,
      -- X/Wayland selections on Linux), which can block tens to
      -- hundreds of ms under contention. tui.now_ms is the cached
      -- frame clock the engine installs at the start of each dispatch
      -- — same value regardless of read order — but reading it up-
      -- front signals intent and survives a future refactor where the
      -- binding refreshes the clock mid-dispatch.
      local now = tui.now_ms()
      tui.copy_to_clipboard(text)
      local toasts = {}
      for _, t in ipairs(state.toasts or {}) do toasts[#toasts + 1] = t end
      toasts[#toasts + 1] = {
        id            = "clipboard-" .. tostring(now),
        text          = string.format("copied %d chars", #text),
        level         = "info",
        started_at_ms = now,
        -- 4 s lifetime: covers the 2 s default plus headroom for the
        -- clipboard call's wall-clock cost. Without this padding the
        -- toast can wink out before the user's eye registers it on
        -- slow / contended systems (and tests racing the same path
        -- flake intermittently).
        ttl_ms        = 4000,
      }
      return shallow_merge(state, { toasts = toasts }), {}
    end
    return state, {}
  end

  return state, {}
end

return M
