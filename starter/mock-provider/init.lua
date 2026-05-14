-- starter/mock_provider.lua — script for mock-plugin to impersonate an
-- openai-provider for deterministic smoke testing of the spawn_graph
-- pipeline AND a self-documenting interactive test machine for
-- developers who launch `nefor --config ./starter` (NEFOR_CONFIG=test
-- is the default — see config.lua).
--
-- Speaks the same wire shape as openai-provider:
--   <name>.chat.create  { chat_id, model? }
--   <name>.chat.append  { chat_id, message: { role, content, ... } }
--   <name>.chat.complete { chat_id }
-- responds with:
--   <name>.stream.delta { id, chat_id, text }   (one or more)
--   <name>.stream.end   { id, chat_id, text, model, duration_ms, finish_reason? }
--   <name>.chat.complete.result { chat_id, output: ProviderOut }
--   <name>.chat.error   { chat_id, message }    (error-shaped close)
--
-- ProviderOut shape (matching openai-provider's chat_complete_result_body):
--   { text, tool_calls?: [{id, name, arguments: object}], finish_reason?, usage }
--
-- ### Response selection (state machine, applied in order)
--
-- For each chat.complete, look at the chat's history and pattern-match
-- the latest user message against this priority list:
--
--   1. Deferred result / failure / submitted-ack from a prior
--      spawn_graph (relay path — must run first to keep the original
--      octopus+lighthouse workflow intact).
--   2. Orchestrator-turn octopus + lighthouse + parallel/combine →
--      emit spawn_graph tool call (must come before sub-graph canned
--      text since the canonical prompt matches both).
--   3. Sub-graph canned text (responder nodes inside the spawn_graph
--      run — Summarise octopuses / lighthouses / Combine paragraph).
--   4. SLOW_STREAM_REGRESSION_ marker — Bug 1 watchdog regression hook.
--   5. Interactive triggers (read readme, cwd/pwd, secret key memory,
--      list files, count to N, think out loud, fail).
--   6. Help fallback — the banner-prefixed help block is also what
--      shows up on the very first turn (no other trigger matched yet).
--
-- ### Why a Lua script and not a new Rust plugin
--
-- mock-plugin is already a scriptable NCP peer; reusing it costs a Lua
-- file instead of a fresh crate.

local NAME = nefor.name -- "mock-plugin"

-- per-chat history: chat_id -> array of {role, content, tool_call_id?, tool_calls?}
local chats = {}

-- The graph the orchestrator-turn responds with via spawn_graph.
-- Encoded as a Lua table; mock-plugin serialises nested tables to JSON.
-- IMPORTANT: rg_adapter expects `arguments` to be a JSON object on the
-- chat.complete.result wire (openai-provider de-nests it before emit;
-- we emit the de-nested shape directly).
local CANNED_GRAPH = {
  nodes = {
    { id = "sx",       reasoner = "responder", args = { prompt = "Summarise octopuses in one sentence." } },
    { id = "sy",       reasoner = "responder", args = { prompt = "Summarise lighthouses in one sentence." } },
    { id = "combine",  reasoner = "responder", args = { prompt = "Combine the two summaries above into one paragraph." } },
    { id = "terminal", reasoner = "terminal",  args = {} },
  },
  edges = {
    { from = "sx",      to = "combine"  },
    { from = "sy",      to = "combine"  },
    { from = "combine", to = "terminal" },
  },
}

-- Canned text responses keyed by pattern in the last user message.
-- Order matters: more specific patterns must come before general ones.
local CANNED_TEXT = {
  -- Combiner sees three user messages: octopus summary, lighthouse summary,
  -- and the explicit "Combine..." instruction. Match the instruction first.
  { pattern = "[Cc]ombine.*paragraph",
    text = "Octopuses, with their remarkable intelligence and adaptive camouflage, share an unlikely kinship with the steadfast lighthouse — both serve as vigilant sentinels of their respective worlds, the cephalopod beneath the waves and the beacon above them, each watchful in its solitary post." },
  { pattern = "[Ss]ummarise octopuses",
    text = "Octopuses are highly intelligent invertebrate cephalopods known for problem-solving, dynamic camouflage, and eight prehensile arms lined with chemosensitive suckers." },
  { pattern = "[Ss]ummarise lighthouses",
    text = "Lighthouses are tall coastal towers crowned with bright rotating beams that guide ships safely past hazards and into harbours, dating back to the Pharos of Alexandria." },
}

-- After spawn_graph returns its serialised result, the orchestrator's
-- wrap node fires again with a "tool" message carrying that text. We
-- relay it as the assistant's final answer.
local FINAL_RELAY_PREFIX = ""

-- Async spawn_graph: the immediate `tool.result` is just an ack
-- ("Submitted sub-graph run_id=..."). The real result arrives later
-- as a USER-role message starting with "[spawn_graph(run_id=...)
-- result]". Pattern-match on that prefix in the latest user message
-- to drive the relay turn.
--
-- The marker shape comes from `agentic-loop.results.format_deferred`.
local DEFERRED_RESULT_MARKER = "%[spawn_graph%(run_id="
local DEFERRED_LEGACY_MARKER = "%[Deferred result for spawn_graph"
local DEFERRED_FAILURE_MARKER = "%[spawn_graph%(run_id=[^)]*%) FAILED%]"
local DEFERRED_FAILURE_LEGACY = "%[Deferred FAILURE for spawn_graph"
local SUBMITTED_ACK_MARKER = "Submitted sub%-graph run_id="

-- Banner prepended to first-turn output and to every help-fallback
-- response. The marker (`MOCK_PROVIDER_BANNER`) is the substring tests
-- can pin on to distinguish the help/banner path from canned-text
-- replies without locking to the full banner string.
local MOCK_PROVIDER_BANNER =
  "**MOCK PROVIDER** -- this is a deterministic test machine, not a real model."

-- Markdown body deliberately exercises every block kind the chat
-- surface's renderer should handle — headings, emphasis variants,
-- inline code, fenced code, lists, blockquote, table, link, hr,
-- emojis, and a CJK example. Broken rendering surfaces visually here
-- so a developer launching `nefor --config ./starter` sees what
-- works and what doesn't on the first turn.
local HELP_BODY = table.concat({
  "",
  "# 🤖 Mock test machine",
  "",
  "## What is this?",
  "",
  "You're talking to a **deterministic mock provider** 🧪 — not a real LLM.",
  "Every reply is pattern-matched against the user message, so the same input",
  "always produces the same output. *Useful* for shaking out the chat surface,",
  "tool-gate, sub-graph orchestration, and ~~caching bugs~~ without waiting on",
  "a cloud round-trip.",
  "",
  "Switch to a real model with `/model` 🚀 — the picker lists every connected",
  "provider, including the local **ollama**/qwen instance spawned alongside.",
  "Once you pick one, you're talking to it ⚡ at real-LLM latency.",
  "",
  "---",
  "",
  "## Triggers",
  "",
  "| Type | Example | What it exercises |",
  "|------|---------|-------------------|",
  "| spawn_graph | `summarize octopuses and lighthouses` | 4-node sub-graph |",
  "| read_file | `read readme` | 📄 tool-gate allowlist |",
  "| bash (pwd) | `what is my cwd` | tool-gate prompt path |",
  "| bash (ls -la) | `list files` | 📁 tool-gate prompt path |",
  "| memory | `the secret key is <v>` | 🔑 history scan |",
  "| streaming | `count to <N>` | 🔢 paced delta emission |",
  "| reasoning | `think out loud about <topic>` | 🤔 reasoning channel |",
  "| error | `fail` | 💥 run-error rendering |",
  "",
  "### 1. spawn_graph",
  "",
  "Submits a 4-node sub-graph: parallel summaries → combine → terminal.",
  "The tool-call payload looks like:",
  "",
  "```json",
  "{",
  "  \"name\": \"spawn_graph\",",
  "  \"arguments\": {",
  "    \"graph\": { \"nodes\": [...], \"edges\": [...] }",
  "  }",
  "}",
  "```",
  "",
  "### 2. Tool calls",
  "",
  "- 📄 `read readme` — uses the `read_file` tool to fetch `README.md`",
  "  (requires `read_file` on the tool-gate allowlist, or auto)",
  "- `what is my cwd` — uses `bash` to run `pwd`",
  "- 📁 `list files` — uses `bash` to run `ls -la`",
  "",
  "### 3. Memory",
  "",
  "1. Set: `the secret key is <value>` 🔑 — value enters chat history",
  "2. Recall: `what is the secret key?` — scans history for a prior set",
  "",
  "> Bilingual example: try `translate hello to japanese` — the help",
  "> intentionally includes CJK like 「こんにちは」 / 你好 to exercise",
  "> wide-character rendering in the markdown renderer.",
  "",
  "### 4. Streaming + reasoning",
  "",
  "- 🔢 `count to <N>` — streams `1, 2, 3, …, N` (capped at 50)",
  "- 🤔 `think out loud about <topic>` — emits `reasoning_delta` chunks",
  "  before the final text",
  "",
  "### 5. Error path",
  "",
  "- 💥 `fail` — returns `finish_reason=error` to exercise the run-error",
  "  rendering path",
  "",
  "---",
  "",
  "*Any other input prints this help.* See the [nefor README](https://github.com/anthropics/nefor)",
  "for the full architecture; this provider is the dev surface for ***fast***,",
  "***deterministic*** smoke testing.",
}, "\n")

local HELP_TEXT = MOCK_PROVIDER_BANNER .. "\n" .. HELP_BODY

-- ------------------------------------------------------------------
-- helpers
-- ------------------------------------------------------------------

-- UTF-8-safe truncate: returns `s` truncated to at most `n` codepoints.
-- `string.sub` is byte-indexed; slicing inside a multibyte codepoint
-- yields invalid UTF-8 that downstream `json.encode` chokes on. Use
-- `utf8.offset` (Lua 5.3+) to find the byte offset of codepoint n+1.
local function utf8_truncate(s, n)
  if type(s) ~= "string" then return s end
  local end_byte = utf8.offset(s, n + 1)
  if end_byte == nil then return s end
  return string.sub(s, 1, end_byte - 1)
end

-- Lowercased copy used by the interactive trigger router.
local function lc(s)
  if type(s) ~= "string" then return "" end
  return string.lower(s)
end

-- Per-chat tool-call id minter. The chat-side reducer correlates
-- tool.result back to its tool block by exact id match (chat.lua's
-- attach_tool_end matches the FIRST entry with the given id and stops),
-- so a static id like "call_mock_pwd" repeated across turns means the
-- second turn's deny / approve clobbers the first turn's already-done
-- block and the second block never receives its own result. Mint a
-- fresh id every call.
local tool_id_counter = 0
local function mint_tool_id(label)
  tool_id_counter = tool_id_counter + 1
  return "call_mock_" .. label .. "_" .. tostring(tool_id_counter)
end

local function pick_response_for(chat_id)
  local history = chats[chat_id] or {}

  -- Find the most recent user message and detect whether a tool message
  -- has landed (chat-orchestrator wrap node, second turn).
  local last_user
  local last_user_idx
  local last_tool
  local last_tool_idx
  for i = #history, 1, -1 do
    local m = history[i]
    if m.role == "tool" and not last_tool then
      last_tool = m.content
      last_tool_idx = i
    end
    if m.role == "user" and not last_user then
      last_user = m.content
      last_user_idx = i
    end
  end

  -- A tool message is "pending relay" only when it's MORE RECENT than
  -- the latest user message — i.e. a tool result just landed and we
  -- should respond to it. If the user has spoken since (next-turn
  -- after a prior spawn_graph completed), last_tool is stale chat
  -- history; pattern-match the new user input instead. Without this
  -- gate, every turn in a chat that ever ran spawn_graph falls into
  -- the SUBMITTED_ACK_MARKER branch because chat_id (and therefore
  -- mock-side history) persists across orchestrator runs for
  -- conversation continuity.
  local tool_is_pending_relay
  if last_tool_idx == nil then
    tool_is_pending_relay = false
  elseif last_user_idx == nil then
    tool_is_pending_relay = true
  else
    tool_is_pending_relay = last_tool_idx > last_user_idx
  end
  if not tool_is_pending_relay then last_tool = nil end

  -- ----------------------------------------------------------------
  -- 1. Deferred-result branch (async spawn_graph). MUST stay first —
  --    the orchestrator's wrap-node turn looks like an ordinary user
  --    message but starts with the deferred marker, and routing it
  --    through the help fallback would break the existing workflow.
  -- ----------------------------------------------------------------
  if type(last_user) == "string"
      and (string.find(last_user, DEFERRED_RESULT_MARKER)
        or string.find(last_user, DEFERRED_LEGACY_MARKER)) then
    -- Strip the leading marker line; what remains is the actual
    -- combined paragraph the model should relay.
    local body = string.match(last_user, "%-%-%- output %-%-%-\n(.*)$")
    if body == nil then
      body = string.match(last_user, "^%[Deferred result for spawn_graph%([^)]*%)%]\n(.*)$")
    end
    return {
      text = FINAL_RELAY_PREFIX .. tostring(body or last_user),
      finish_reason = "stop",
      with_reasoning = true,
    }
  end
  if type(last_user) == "string"
      and (string.find(last_user, DEFERRED_FAILURE_MARKER)
        or string.find(last_user, DEFERRED_FAILURE_LEGACY)) then
    return {
      text = "The spawned sub-graph failed: " .. tostring(last_user),
      finish_reason = "stop",
    }
  end

  -- Async ack branch: the only "tool" message in history is the
  -- spawn_graph immediate ack. We can't relay that to the user as a
  -- final answer — emit a short transitional ack so the orchestrator
  -- terminates and the chat unblocks.
  if last_tool ~= nil and string.find(tostring(last_tool), SUBMITTED_ACK_MARKER) then
    return {
      text = "Started the sub-graph; I'll relay the results when they arrive.",
      finish_reason = "stop",
    }
  end

  -- ----------------------------------------------------------------
  -- Tool-result relay for the new interactive triggers. When the wrap
  -- node fires after a read_file / bash tool result, render a friendly
  -- response that quotes the tool output. Keyed off the most recent
  -- user-role trigger string already in history.
  -- ----------------------------------------------------------------
  if last_tool ~= nil and type(last_user) == "string" then
    local low_user = lc(last_user)
    if string.find(low_user, "read readme", 1, true) then
      local content = tostring(last_tool)
      local snippet = utf8_truncate(content, 200)
      return {
        text = string.format(
          "Tool returned README.md (length: %d chars). Snippet: %s...",
          #content, snippet),
        finish_reason = "stop",
      }
    end
    if string.find(low_user, "pwd", 1, true)
        or string.find(low_user, "cwd")
        or string.find(low_user, "where am i", 1, true) then
      -- bash tool output may include a trailing newline / exit-code
      -- footer; keep it terse for the common pwd case.
      local trimmed = string.match(tostring(last_tool), "^%s*(.-)%s*$") or tostring(last_tool)
      return {
        text = "Working directory: " .. trimmed,
        finish_reason = "stop",
      }
    end
    if string.find(low_user, "list files", 1, true) then
      return {
        text = "Files in cwd:\n" .. tostring(last_tool),
        finish_reason = "stop",
      }
    end
    -- Legacy / synchronous-style fallback: relay the tool result text
    -- as the final answer. Kept for safety if anything reverts
    -- spawn_graph to synchronous semantics, or for unrecognised tool
    -- calls in tests.
    return {
      text = FINAL_RELAY_PREFIX .. tostring(last_tool),
      finish_reason = "stop",
    }
  end
  if last_tool ~= nil then
    return {
      text = FINAL_RELAY_PREFIX .. tostring(last_tool),
      finish_reason = "stop",
    }
  end

  if type(last_user) ~= "string" then
    return { text = "[mock provider: no user message]", finish_reason = "stop" }
  end

  -- ----------------------------------------------------------------
  -- 2. Orchestrator-turn spawn_graph: octopus + lighthouse anywhere in
  --    the latest user message. MUST come before CANNED_TEXT (which
  --    matches "Combine ... paragraph" for the inner combine node).
  --    Inner sub-graph nodes don't have both words in their latest
  --    user message — `sx` sees "Summarise octopuses…", `sy` sees
  --    "Summarise lighthouses…", `combine` sees "Combine the two
  --    summaries…" — so the broad two-word match is safe. The
  --    relay turn's last_user matches the deferred-result branch
  --    first and returns before reaching here.
  -- ----------------------------------------------------------------
  if string.find(last_user, "octopus") and string.find(last_user, "lighthouse") then
    return {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = {
        {
          id        = mint_tool_id("spawn_graph"),
          name      = "spawn_graph",
          -- arguments is a JSON OBJECT in the openai-provider's
          -- de-nested wire shape; rg_adapter forwards verbatim and
          -- tool-executor reads `arguments` as the call's parameter
          -- map.
          arguments = { graph = CANNED_GRAPH },
        },
      },
    }
  end

  -- ----------------------------------------------------------------
  -- 3. Sub-graph canned text. Used by the responder nodes inside the
  --    spawn_graph workflow — each fires its own chat with a prompt
  --    like "Summarise octopuses..." or "Combine the two summaries
  --    above into one paragraph."
  -- ----------------------------------------------------------------
  for _, entry in ipairs(CANNED_TEXT) do
    if string.find(last_user, entry.pattern) then
      return { text = entry.text, finish_reason = "stop" }
    end
  end

  -- ----------------------------------------------------------------
  -- 4. SLOW_STREAM_REGRESSION_ — Bug 1 watchdog regression hook
  --    (commit 0941531). Triggered by the literal substring; mock
  --    blocks for ~1.2s before emitting.
  -- ----------------------------------------------------------------
  if string.find(last_user, "SLOW_STREAM_REGRESSION_") then
    -- Coarse sleep — `os.execute` is fine because the mock already
    -- runs in its own subprocess.
    os.execute("sleep 1.2")
    return {
      text = "slow regression payload acknowledged",
      finish_reason = "stop",
    }
  end

  -- ----------------------------------------------------------------
  -- 5. Interactive triggers — the developer-facing test machine.
  -- ----------------------------------------------------------------
  local low = lc(last_user)

  -- 5a. read readme -> read_file tool call
  if string.find(low, "read readme", 1, true) then
    return {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = {
        {
          id        = mint_tool_id("read_readme"),
          name      = "read_file",
          arguments = { path = "README.md" },
        },
      },
    }
  end

  -- 5b. cwd / pwd / where am i -> bash tool call (pwd)
  if string.find(low, "pwd", 1, true)
      or string.find(low, "current cwd", 1, true)
      or string.find(low, "where am i", 1, true)
      or low == "cwd"
      or string.find(low, "what is my cwd", 1, true)
      or string.find(low, "my cwd", 1, true) then
    return {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = {
        {
          id        = mint_tool_id("pwd"),
          name      = "bash",
          arguments = { command = "pwd" },
        },
      },
    }
  end

  -- 5c. list files -> bash tool call (ls -la)
  if string.find(low, "list files", 1, true) then
    return {
      text = "",
      finish_reason = "tool_calls",
      tool_calls = {
        {
          id        = mint_tool_id("ls"),
          name      = "bash",
          arguments = { command = "ls -la" },
        },
      },
    }
  end

  -- 5d. fail / trigger error -> error-shaped close
  if low == "fail" or string.find(low, "trigger error", 1, true) then
    return {
      text          = "",
      finish_reason = "error",
      error_message = "Mock provider triggered error on user request.",
    }
  end

  -- 5e. secret-key memory (history-aware lookup)
  if string.find(low, "what is the secret key") then
    -- Look for any prior user message of the form "secret key is X".
    -- Use a non-anchored case-insensitive scan.
    local value
    for i = #history - 1, 1, -1 do
      local m = history[i]
      if m.role == "user" and type(m.content) == "string" then
        local lc_content = lc(m.content)
        local capture = string.match(lc_content, "secret key is (.+)")
        if capture then
          -- Re-extract from the original (case-preserving) content
          -- using the same offset so we keep the user's casing.
          local start_idx = string.find(lc_content, "secret key is ", 1, true)
          if start_idx then
            value = string.sub(m.content, start_idx + #"secret key is ")
            -- Strip a trailing `?` or `.` if user phrased as a sentence.
            value = string.match(value, "^(.-)%s*[%?%.]*%s*$") or value
          end
          break
        end
      end
    end
    if value and value ~= "" then
      return {
        text = "The secret key is: " .. value,
        finish_reason = "stop",
      }
    end
    return {
      text = "I don't know yet -- tell me with: \"the secret key is <value>\"",
      finish_reason = "stop",
    }
  end

  -- 5f. set the secret key (no special branch — value is now in
  -- history and will be retrieved on the next "what is the secret
  -- key?" turn). Acknowledge cleanly so the chat doesn't fall through
  -- to help.
  if string.find(low, "secret key is ") then
    return {
      text = "Got it. The secret key is now in this chat's history.",
      finish_reason = "stop",
    }
  end

  -- 5f-bilingual. translate hello to japanese -> canned reply, used by
  -- the help text's "Bilingual example" and renders CJK in the
  -- assistant text channel so the wide-char rendering is visible
  -- outside the help block.
  if string.find(low, "translate hello to japanese", 1, true)
      or string.find(low, "translate hello to chinese", 1, true)
      or string.find(low, "translate hello", 1, true) then
    local greeting = "こんにちは"
    if string.find(low, "chinese", 1, true) then greeting = "你好" end
    return {
      text = "**Hello** in that language is `" .. greeting .. "`.",
      finish_reason = "stop",
    }
  end

  -- 5g. count to N (cap at 50)
  do
    local n_str = string.match(low, "count to (%d+)")
    if n_str then
      local n = tonumber(n_str) or 0
      if n > 50 then n = 50 end
      if n < 1 then n = 1 end
      local parts = {}
      for i = 1, n do parts[i] = tostring(i) end
      return {
        text = table.concat(parts, ", "),
        finish_reason = "stop",
      }
    end
  end

  -- 5h. think out loud about / reason about <topic>
  do
    local topic = string.match(low, "think out loud about (.+)")
                or string.match(low, "reason about (.+)")
    if topic then
      -- Strip a trailing punctuation block if user typed a sentence.
      topic = string.match(topic, "^(.-)%s*[%?%.!]*%s*$") or topic
      return {
        text = "Reasoned about " .. topic
            .. "; conclusion: deterministic mock has no opinion, but the reasoning channel works.",
        finish_reason = "stop",
        with_reasoning = true,
      }
    end
  end

  -- ----------------------------------------------------------------
  -- 6. Help fallback. Same body whether this is the first turn (chat
  --    history has only the just-arrived user message) or a later
  --    unrecognised input — the banner-prefixed help block is the
  --    deterministic "I don't know what you mean, here's what I do
  --    know" answer.
  -- ----------------------------------------------------------------
  return { text = HELP_TEXT, finish_reason = "stop" }
end

-- Canned reasoning chunks emitted ahead of content for the orchestrator's
-- relay turn. Five chunks, deterministic — this is what the user sees as
-- the live "thinking" preview before it collapses on first content delta.
-- For sub-graph chats, rg_adapter's gate drops these silently.
local CANNED_REASONING_CHUNKS = {
  "Reading the deferred sub-graph result.\n",
  "It carries a single combined paragraph already, so",
  " I don't need to recompose anything — relaying it",
  " verbatim is the right call.\n",
  "Producing the final answer now.",
}

-- Stream pacing — mimic ~200 tok/s, fast cloud LLM territory. ~4
-- chars/token English × 200 tok/s = ~800 chars/sec → 16-char chunks
-- at 20ms intervals. Slower than instant so streaming reads as live
-- output instead of paste; faster than 80 tok/s so it doesn't become
-- the dominant cost of an interactive turn.
-- `os.execute("sleep …")` spawns a sleep subprocess per chunk; cheap
-- enough at this rate (~20/sec) and the mock has no other concurrent
-- work to do while a stream is in flight.
local STREAM_CHUNK_CODEPOINTS = 16
local STREAM_PACE_SECONDS     = 0.02

-- Skip pacing under tests — agentic_cli_mock_e2e fires several
-- scenarios with a 10s wall-clock cap and the long-stream regression
-- already exercises a deliberate slow path. Activated by the same
-- NEFOR_CONFIG=test env the cli-config harness uses; interactive
-- launches with NEFOR_CONFIG=test still get pacing because the mock
-- runs as its own subprocess and inherits the parent's env (the cli
-- harness sets NEFOR_TEST_FAST_MOCK=1 explicitly to opt into instant
-- streaming).
local function pacing_enabled()
  local v = os.getenv("NEFOR_TEST_FAST_MOCK")
  return not (v == "1" or v == "true")
end

local function pace()
  if pacing_enabled() then
    os.execute("sleep " .. tostring(STREAM_PACE_SECONDS))
  end
end

local function emit_reasoning(chat_id, id)
  -- Emit reasoning chunks ahead of the content stream, then a
  -- reasoning_end carrying the full accumulated text. Mirrors what
  -- openai-provider does on a real Qwen 3 turn.
  local full = ""
  for i, chunk in ipairs(CANNED_REASONING_CHUNKS) do
    full = full .. chunk
    nefor.emit("stream.reasoning_delta", {
      id      = id,
      chat_id = chat_id,
      text    = chunk,
    })
    if i < #CANNED_REASONING_CHUNKS then pace() end
  end
  if interrupted[chat_id] then return false end
  nefor.emit("stream.reasoning_end", {
    id          = id,
    chat_id     = chat_id,
    text        = full,
    duration_ms = 250,
  })
  return true
end

-- Emit the full response stream. Returns `(completed, partial)` where
-- `completed` is true if the stream ran to completion, false if
-- `interrupted[chat_id]` flipped mid-stream; `partial` is the substring
-- actually emitted as `stream.delta` chunks so far (the full text on
-- completion, the prefix-up-to-the-cancel-boundary on interrupt). The
-- caller persists `partial` into the chat history so the next turn's
-- context shows what the model said before being cut off — mirrors
-- openai-provider's `outcome.full_text` push on `outcome.interrupted`
-- (plugins/openai-provider/src/main.rs:751-755). Without this, the
-- model on the next turn has no record of its own interrupted attempt,
-- so the user's "you started thinking wrongly" follow-up has nothing
-- to anchor against.
local function emit_stream(chat_id, text, opts)
  if type(text) ~= "string" or #text == 0 then return true, "" end
  opts = opts or {}
  local id = "resp-" .. chat_id

  if opts.with_reasoning then
    if not emit_reasoning(chat_id, id) then return false, "" end
  end

  -- Stream the response in small fixed-size chunks paced to ~200 tok/s
  -- to mimic a real cloud LLM. Chunk boundaries snap to UTF-8
  -- codepoint edges (string.sub is byte-indexed; slicing inside a
  -- multibyte codepoint produces invalid UTF-8 that downstream
  -- serde_json can't deserialise).
  local cp_count = utf8.len(text) or #text
  local cp_per_chunk = STREAM_CHUNK_CODEPOINTS
  local cp_i = 1
  local emitted = ""
  while cp_i <= cp_count do
    if interrupted[chat_id] then return false, emitted end
    local cp_stop = math.min(cp_i + cp_per_chunk - 1, cp_count)
    local byte_start = utf8.offset(text, cp_i)
    local byte_after_stop = utf8.offset(text, cp_stop + 1)
    local byte_stop = byte_after_stop and (byte_after_stop - 1) or #text
    local chunk = string.sub(text, byte_start, byte_stop)
    nefor.emit("stream.delta", {
      id      = id,
      chat_id = chat_id,
      text    = chunk,
    })
    emitted = emitted .. chunk
    cp_i = cp_stop + 1
    if cp_i <= cp_count then pace() end
  end
  if interrupted[chat_id] then return false, emitted end
  nefor.emit("stream.end", {
    id            = id,
    chat_id       = chat_id,
    text          = text,
    model         = "mock-model",
    duration_ms   = 0,
  })
  return true, text
end

nefor.on_ready_ok(function()
  -- Synthetic `<name>.hello { model = ... }` so chat_orchestrator's
  -- adapter learns the model name. Mirrors openai-provider's hello.
  nefor.emit("hello", { model = "mock-model" })
end)

nefor.on(NAME .. ".chat.create", function(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end
  chats[chat_id] = {}
  nefor.log("chat.create chat_id=" .. chat_id)
end)

nefor.on(NAME .. ".chat.append", function(body)
  local chat_id = body.chat_id
  local message = body.message
  if type(chat_id) ~= "string" or type(message) ~= "table" then return end
  if not chats[chat_id] then chats[chat_id] = {} end
  table.insert(chats[chat_id], {
    role            = message.role,
    content         = message.content,
    tool_call_id    = message.tool_call_id,
    tool_calls      = message.tool_calls,
  })
  nefor.log(string.format(
    "chat.append chat_id=%s role=%s content_len=%d",
    chat_id,
    tostring(message.role),
    type(message.content) == "string" and #message.content or 0))
end)

nefor.on(NAME .. ".chat.complete", function(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end

  -- Clear any leftover interrupt flag from a prior turn on this
  -- chat_id; the flag is set by the `<NAME>.interrupt` handler when
  -- /cancel fires mid-stream and is consumed by emit_stream's per-
  -- chunk check below.
  interrupted[chat_id] = nil

  local resp = pick_response_for(chat_id)
  nefor.log(string.format(
    "chat.complete chat_id=%s finish=%s text_len=%d tool_calls=%s",
    chat_id,
    tostring(resp.finish_reason),
    type(resp.text) == "string" and #resp.text or 0,
    resp.tool_calls and #resp.tool_calls or 0))

  -- Error branch: emit `<name>.chat.error` and skip the result wire.
  -- The wrapper actor (starter/openai-provider/init.lua) translates
  -- chat.error into `tool.result { error }` for the agentic-loop's
  -- run-error path, which is the rendering target the brief asks for.
  if resp.finish_reason == "error" then
    nefor.emit("chat.error", {
      chat_id = chat_id,
      message = resp.error_message or "mock provider error",
    })
    -- Echo the assistant turn into history so subsequent turns see
    -- the failed attempt; content stays empty.
    if not chats[chat_id] then chats[chat_id] = {} end
    table.insert(chats[chat_id], {
      role    = "assistant",
      content = "",
    })
    return
  end

  -- Stream phase (only when there's text — tool-call turns skip
  -- streaming, matching openai-provider's behaviour). The
  -- `with_reasoning` flag is set on the deferred-result relay turn so
  -- the orchestrator's wrap node demonstrates the live thinking →
  -- collapse rendering path. `partial` is the prefix actually emitted
  -- before the loop ran out (full text on completion, cancel-boundary
  -- prefix on interrupt — used to seed history below).
  local completed = true
  local partial = ""
  if type(resp.text) == "string" and #resp.text > 0 then
    completed, partial = emit_stream(chat_id, resp.text, { with_reasoning = resp.with_reasoning })
  end

  -- Cancelled mid-stream: emit `<name>.chat.error` with msg "interrupted"
  -- so the wrapper translates it into a `[interrupted]` system message
  -- (mirrors openai-provider's turn.error("interrupted") path) and skip
  -- the result wire — the agentic-loop's pending entry closes via
  -- chat.error → tool.result{error="interrupted"}.
  --
  -- Persist `partial` (the prefix actually streamed before the cancel
  -- flag flipped) into chat history so the NEXT turn's `pick_response_for`
  -- — and any real-LLM equivalent — sees what the model said before
  -- being cut off. Without this, a "/cancel + 'you were thinking
  -- wrong'" follow-up has no anchor in context. Mirrors openai-
  -- provider's push_assistant on outcome.interrupted
  -- (plugins/openai-provider/src/main.rs around line 751). Skip the
  -- push entirely when the cancel landed before any chunks (partial
  -- empty) — an empty assistant message confuses both the OpenAI wire
  -- shape (content: null vs "") and any history-walking heuristic.
  if not completed then
    interrupted[chat_id] = nil
    nefor.emit("chat.error", { chat_id = chat_id, message = "interrupted" })
    if not chats[chat_id] then chats[chat_id] = {} end
    if type(partial) == "string" and #partial > 0 then
      table.insert(chats[chat_id], { role = "assistant", content = partial })
    end
    return
  end

  -- chat.complete.result with ProviderOut shape.
  local output = {
    text          = resp.text or "",
    finish_reason = resp.finish_reason,
    usage         = {
      prompt_tokens     = 0,
      completion_tokens = type(resp.text) == "string" and #resp.text or 0,
      model             = "mock-model",
    },
  }
  if resp.tool_calls and #resp.tool_calls > 0 then
    output.tool_calls = resp.tool_calls
  end
  if resp.with_reasoning then
    -- Mirrors openai-provider's chat.complete.result.output.reasoning
    -- field — non-streaming consumers (sub-graph node outputs, audit
    -- listeners) get the full trace alongside the content.
    local full = ""
    for _, chunk in ipairs(CANNED_REASONING_CHUNKS) do full = full .. chunk end
    output.reasoning = full
  end
  nefor.emit("chat.complete.result", {
    chat_id = chat_id,
    output  = output,
  })

  -- Echo the assistant turn into our local history so subsequent
  -- chat.complete calls (cycle re-fires) see it.
  if not chats[chat_id] then chats[chat_id] = {} end
  table.insert(chats[chat_id], {
    role       = "assistant",
    content    = resp.text or "",
    tool_calls = resp.tool_calls,
  })
end)

-- Cancellation hook. The chat-side `chat.interrupt` envelope is
-- translated by the openai-provider wrapper to `<NAME>.interrupt`
-- (carrying the chat_id, so concurrent chats don't clobber each other);
-- when it lands during a stream we flip the per-chat flag and the
-- emit_stream / emit_reasoning loops break at the next chunk boundary.
-- Without this hook /cancel had to wait for the canned text to finish
-- before taking effect.
nefor.on(NAME .. ".interrupt", function(body)
  local chat_id = body and body.chat_id
  if type(chat_id) == "string" then
    interrupted[chat_id] = true
    nefor.log("interrupt chat_id=" .. chat_id)
  else
    -- Bare interrupt without chat_id: cancel every active chat. Rare
    -- (the wrapper always carries the chat_id) but defensive.
    for k, _ in pairs(chats) do interrupted[k] = true end
    nefor.log("interrupt fanned out (no chat_id)")
  end
end)

nefor.on(NAME .. ".chat.delete", function(body)
  local chat_id = body.chat_id
  if type(chat_id) ~= "string" then return end
  chats[chat_id] = nil
  interrupted[chat_id] = nil
end)

nefor.on(NAME .. ".reset", function()
  chats = {}
  interrupted = {}
end)

-- Tool-result accumulation is handled by rg_adapter's `adapter`
-- reasoner: it translates the tool-executor's ToolResults into
-- `{role="tool", content, tool_call_id}` messages that the wrap node
-- then appends via chat.append on its next firing. So mock receives
-- the tool message through the normal chat.append path and doesn't
-- need to subscribe to broadcast `tool.result` directly.

-- The auth dance — chat_orchestrator's openai_provider_adapter expects
-- to inject a static_token via `<name>.auth.set` after seeing
-- `<name>.ready`. Mock has no auth, but we acknowledge the set so the
-- adapter doesn't think auth failed.
nefor.on(NAME .. ".auth.set", function(_body)
  nefor.emit("auth.status", { state = "connected" })
end)

-- /model picker discovery. The TUI fans out one
-- `chat.model.list_requested { provider }` per connected provider when
-- the user opens the picker; the wrapper translates that to
-- `<NAME>.models.list_requested`. We answer with a single-model list.
nefor.on(NAME .. ".models.list_requested", function(_body)
  nefor.emit("models.listed", { models = { "mock-model" } })
end)

-- Debug-only history snapshot. Used by integration tests (and ad-hoc
-- diagnostics) to peek at the per-chat messages table without
-- re-driving a full chat.complete cycle. The production chat path
-- doesn't depend on it; nothing on the bus emits or subscribes to
-- `<NAME>.debug.history.*` outside of test harnesses.
nefor.on(NAME .. ".debug.history.dump", function(body)
  local chat_id = body and body.chat_id
  if type(chat_id) ~= "string" then return end
  local history = chats[chat_id] or {}
  local snapshot = {}
  for i, m in ipairs(history) do
    snapshot[i] = {
      role         = m.role,
      content      = m.content,
      tool_call_id = m.tool_call_id,
      tool_calls   = m.tool_calls,
    }
  end
  nefor.emit("debug.history.result", {
    chat_id  = chat_id,
    messages = snapshot,
  })
end)

-- /model <name> selection. Mock has only one model; whatever model the
-- user picks we ack back unchanged so chat.lua's reducer records the
-- selection. The wrapper translates `<NAME>.model.set_ack` →
-- `chat.model.set_ack { provider, model }`.
nefor.on(NAME .. ".model.set", function(body)
  local model = body and body.model
  if type(model) ~= "string" or model == "" then return end
  nefor.emit("model.set_ack", { model = model })
end)
