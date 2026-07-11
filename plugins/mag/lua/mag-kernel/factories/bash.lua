-- plugins/mag/lua/mag-kernel/factories/bash.lua — the shell capability node.
--
-- `(bash "command")` in MAG lowers to one actor of this factory (crates/
-- nefor-mag eval_bash). Semantics mirror a real shell pipe: the input message
-- is the command's stdin, its stdout is the output message — `->` is the pipe.
--
-- Contract:
--   params  { command, timeout_ms? }       the shell line (authored in MAG,
--                                          `(bash "cmd" {:timeout_ms N}?)`);
--                                          absent timeout = run until exit
--   input   ( mag.Unit | mag.Text )        union — fires on either:
--             mag.Unit   dependency firing — run the command, no stdin
--                        (the initial activation of a source bash node, or an
--                        upstream ordering edge)
--             mag.Text   an upstream node's stdout, delivered as stdin
--   output  mag.Text                       the command's stdout
--           mag.CommandFailed              declared so failure edges are
--                                          routable; unrouted it escalates
--                                          (routing.lua apply_completion)
--
-- ── async flow (routing.lua, the kernel⇄factory contract) ───────────────────
--   An activation emits one `capability.invoke` (name "bash", args carrying
--   { command, stdin, timeout_ms? }) and returns { status = "pending" }. The gate-forwarded
--   answer arrives as a reply activation whose ref names the call; the reply
--   delivery returns the completion directly — no batching, one call per
--   activation (cf. run-tool.lua, which aggregates batches).
--
-- ── output parsing (the basic-tools bash API, flagged) ───────────────────────
--   basic-tools' `bash` answers with ONE combined string:
--     <stdout>\n [stderr]\n<stderr>\n [exit N]
--   The factory knows the capability's shape the way llm.lua knows the
--   provider's (actor-model.md, Cancellation: "low-level details live where
--   low-level knowledge already is"): it splits the footer and the stderr
--   section. Exit 0 → the stdout part is the mag.Text output. Non-zero exit
--   (or a transport error) → a failed completion carrying the stderr detail;
--   unrouted, the kernel escalates it as mag.run_failed and the host fails
--   the run loudly. A footer-less answer (a hand-scripted responder) is
--   treated as clean stdout.
--
-- ── kill with an in-flight command (signal choice) ───────────────────────────
--   Same shape as run-tool.lua: the tool surface exposes no abort primitive —
--   a running command finishes server-side regardless. On kill the kernel has
--   already dropped this id's correlations, so the late answer voids; the
--   handler only drops local pending state.

local kinds = require("kinds")

local M = {}

-- The factory's own computed-failure tag (NOT the kernel-synthesized
-- mag.Failed — routing.lua emits that; a factory never returns it).
local COMMAND_FAILED = "mag.CommandFailed"

M.declaration = {
  name = "bash",
  semantic = {
    input={kind="union",items={{kind="primitive",name="Unit"},{kind="named",name="nefor.contracts.Text",arguments={}}}},
    output={kind="named",name="nefor.contracts.Text",arguments={}},
    inputs={
      {wire="mag.Unit",type={kind="primitive",name="Unit"}},
      {wire="mag.Text",type={kind="named",name="nefor.contracts.Text",arguments={}}},
    },
    outputs={
      {wire="mag.Text",type={kind="named",name="nefor.contracts.Text",arguments={}}},
      {wire=COMMAND_FAILED,type={kind="primitive",name="Data"}},
    },
  },

  params = {
    command = "string", -- required; construction fails without it
    timeout_ms = "number?", -- optional wall-clock bound; absent = unbounded
  },

  inputs = {
    -- Union (shape.lua): fires on whichever arrives. Unit runs the command
    -- with no stdin; Text is delivered as stdin.
    input = { "mag.Unit", "mag.Text" },
  },

  outputs = {
    "mag.Text",
    COMMAND_FAILED,
  },

  signals = {
    "kill",
  },
}

-- Split basic-tools' combined answer into stdout, stderr, and the exit code.
-- Shape: `<stdout>\n` then optionally `[stderr]\n<stderr>\n` then `[exit N]`.
-- No footer → treat the whole string as clean stdout (exit "0").
local function parse_output(raw)
  local body, exit = raw:match("^(.*)%[exit ([^%[%]]*)%]$")
  if body == nil then
    return raw, nil, "0"
  end
  local stdout, stderr
  if body:sub(1, 9) == "[stderr]\n" then
    stdout, stderr = "", body:sub(10)
  else
    local s, e = body:find("\n[stderr]\n", 1, true)
    if s then
      -- The newline before the marker is stdout's own trailing newline.
      stdout, stderr = body:sub(1, s), body:sub(e + 1)
    else
      stdout, stderr = body, nil
    end
  end
  return stdout, stderr, exit
end

-- construct(id, params, emit, deps) -> instance
function M.construct(id, params, emit, deps)
  params = params or {}

  local command = params.command
  if type(command) ~= "string" or command == "" then
    return nil, "bash actor requires a non-empty string params.command"
  end
  local timeout_ms = params.timeout_ms
  if timeout_ms ~= nil and (type(timeout_ms) ~= "number" or timeout_ms < 1) then
    return nil, "bash actor params.timeout_ms must be a positive number of milliseconds"
  end

  local function sign(message)
    message.from = id
    return message
  end

  -- In-flight calls keyed by a per-instance sequence (ref echoed on the
  -- reply). A union input fires per message, so overlapping activations each
  -- hold their own slot; a slot dropped by kill voids its late answer.
  local pending = {}
  local seq = 0

  local instance = { id = id }

  -- A correlated capability answer: parse the combined output, emit stdout as
  -- the mag.Text output on success, and return the completion directly (one
  -- call per activation — nothing to aggregate).
  local function handle_reply(activation)
    local ref = activation.ref or {}
    if not pending[ref.call] then
      -- Killed or duplicate: the answer is voided.
      return nil
    end
    pending[ref.call] = nil

    if activation.error ~= nil then
      return {
        status = "failed",
        failure = COMMAND_FAILED,
        value = { error = "bash capability error: " .. tostring(activation.error), command = command },
      }
    end
    if type(activation.result) ~= "string" then
      return {
        status = "failed",
        failure = COMMAND_FAILED,
        value = { error = "bash capability returned a non-text result", command = command },
      }
    end

    local stdout, stderr, exit = parse_output(activation.result)
    if exit == "0" then
      emit(sign({ kind = "mag.Text", text = stdout }))
      return { status = "ok" }
    end
    local detail = stderr
    if detail == nil or detail == "" then
      detail = stdout
    end
    return {
      status = "failed",
      failure = COMMAND_FAILED,
      value = {
        error = string.format("bash exited %s: %s", tostring(exit), tostring(detail)),
        command = command,
      },
    }
  end

  -- A graph activation: Unit fires the command bare, Text feeds it as stdin.
  -- Dispatch on the declared tag — a type fact, never a shape sniff.
  local function handle_input(one)
    local stdin = nil
    if one.tag == "mag.Text" then
      stdin = (one.message or {}).text or ""
    end
    seq = seq + 1
    pending[seq] = true
    emit(sign({
      kind = "capability.invoke",
      capability = "bash",
      -- Wrapped like run-tool's request: the bridge lifts the inner args out
      -- as the gate's tool args (plugins/mag/src/bridge.rs gate_invoke).
      request = {
        name = "bash",
        args = { command = command, stdin = stdin, timeout_ms = timeout_ms },
      },
      ref = { call = seq },
    }))
    return { status = "pending" }
  end

  -- deliver(activation) -> completion (routing.lua, the kernel⇄factory
  -- contract). A reply activation is the correlated command answer; any other
  -- is a graph delivery on the union input port.
  function instance.deliver(activation)
    activation = activation or {}
    if activation.kind == "reply" then
      return handle_reply(activation)
    end
    local one = (activation.messages or {})[1] or {}
    return handle_input(one)
  end

  -- Kill handler (SIGKILL analog; see the header): nothing to abort — the
  -- command finishes server-side and the kernel's correlation drop voids the
  -- answer — so only local pending state is dropped.
  function instance.handle_kill()
    pending = {}
  end

  -- Readiness confirmation (actor-model.md, Lifecycle): construction happens
  -- at the first activation, so this emit coincides with beginning work.
  emit(sign({ kind = kinds.ready }))

  return instance
end

return M
