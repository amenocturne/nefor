-- starter/auth/init.lua — Nestor (corp Tinkoff LLM gateway) auth.
--
-- Two-step flow:
--   1. Locate the `dp` CLI binary and run `dp auth print-token` to get
--      a DP (DevPlatform) bearer token.
--   2. Exchange the DP token for a short-lived Nestor JWT by POSTing
--      to Nestor's /api/v2/token endpoint. The JWT is what
--      openai-provider sends as `Nestor-Token: <jwt>` on every chat
--      completion request.
--
-- The JWT is held in module-local state. No disk persistence — DP's
-- own session lives wherever DP stores it; we don't touch that file.
--
-- io.popen + curl rather than nefor.process.spawn because the engine's
-- async-spawn :wait() yields a coroutine and init.lua executes
-- synchronously (lua.load(...).exec()). DP auth must finish before
-- openai-provider spawns (the JWT goes into --api-key on the command
-- line; the engine's plugin spawn API rejects per-instance env vars),
-- so we need a synchronous subprocess path. mlua's safe stdlib ships
-- io.popen and os.execute, both of which block until the child exits.
-- HTTP shells out to curl for the same reason — the engine doesn't
-- expose a synchronous HTTP binding.

local M = {}

local json = nefor.json

local NESTOR_BASE      = "https://code-completion-nestor.tcsbank.ru"
local TOKEN_ENDPOINT   = NESTOR_BASE .. "/api/v2/token"
local MODELS_ENDPOINT  = NESTOR_BASE .. "/api/v1/cli/models"
local DP_WORKDIR_NAME  = "dp_v13.4.2"

-- openai-provider appends `/v1/chat/completions` itself. Nestor's full
-- chat URL is `${NESTOR_BASE}/api/v1/cli/openai-like/v1/chat/completions`,
-- so the base we hand openai-provider must omit the trailing `/v1`.
M.OPENAI_PROVIDER_BASE_URL = NESTOR_BASE .. "/api/v1/cli/openai-like"

local cached = nil  -- { jwt = "...", expires_at_ms = number } or nil

-- io.popen captures stdout but not stderr. To diagnose DP failure
-- modes (not logged in vs. binary missing vs. timeout) redirect stderr
-- to a temp file and read it back.
--
-- os.tmpname() returns an OS-allocated unique path (race-free vs.
-- os.time()+math.random() concatenation, which is predictable on a
-- shared host and would let an attacker pre-create the file with a
-- symlink to something the calling user can write).
local function shell_capture(cmd)
  local stderr_path = os.tmpname()
  local full = cmd .. " 2>" .. stderr_path
  local pipe = io.popen(full, "r")
  if pipe == nil then
    return nil, "io.popen failed", -1
  end
  local stdout = pipe:read("*a") or ""
  -- pipe:close() returns ok, kind, exit. On success exit is 0.
  local ok, _, exit = pipe:close()
  local stderr_fh = io.open(stderr_path, "r")
  local stderr = ""
  if stderr_fh ~= nil then
    stderr = stderr_fh:read("*a") or ""
    stderr_fh:close()
  end
  pcall(os.remove, stderr_path)
  if ok then exit = exit or 0 end
  return stdout, stderr, exit or -1
end

-- Wrap a string for safe inclusion in a /bin/sh command. Single-quote
-- the value and escape any embedded single quotes via the standard
-- '"'"' trick. Used for tokens that may contain shell metacharacters.
local function sh_quote(s)
  return "'" .. tostring(s):gsub("'", [['"'"']]) .. "'"
end

local function file_exists(path)
  local fh = io.open(path, "r")
  if fh == nil then return false end
  fh:close()
  return true
end

-- Checks the system install path first, then the nessy-managed
-- fallback, then a PATH lookup. The order matters: a stale nessy
-- install would otherwise shadow a freshly-upgraded /usr/local/bin/dp.
local function find_dp_binary()
  local candidates = {
    "/usr/local/bin/dp",
    (os.getenv("HOME") or "") .. "/.nessy/" .. DP_WORKDIR_NAME .. "/dp",
  }

  for _, p in ipairs(candidates) do
    if file_exists(p) then return p end
  end

  -- Fall back to PATH via `command -v`. Don't use `which` — it may not
  -- exist on minimal containers; `command -v` is a POSIX shell builtin.
  local stdout, _stderr, exit = shell_capture("command -v dp")
  if exit == 0 then
    local trimmed = (stdout or ""):gsub("%s+$", "")
    if #trimmed > 0 and file_exists(trimmed) then
      return trimmed
    end
  end

  return nil
end

-- Returns the raw DP token string, or raises one of:
--   "not-logged-in"     — DP CLI installed but no active session.
--   "dp-binary-missing" — DP CLI not found on this host.
--   "dp-timeout"        — `dp auth print-token` hung past 5 seconds.
--   "dp-failed: <stderr>" — anything else (network, malformed output).
--
-- Without the timeout, a wedged DP daemon (rare but seen in practice)
-- would hang startup forever — fail fast and tell the user to re-login.
local function get_dp_token(dp_path)
  -- Set DP_WORKDIR only if the nessy-managed dir actually exists —
  -- system-installed dp uses its own default and forcing a
  -- non-existent path breaks auth lookup.
  local nessy_workdir = (os.getenv("HOME") or "") .. "/.nessy/" .. DP_WORKDIR_NAME
  local env_prefix = ""
  if file_exists(nessy_workdir) then
    env_prefix = "DP_WORKDIR=" .. sh_quote(nessy_workdir) .. " "
  end

  -- gtimeout/timeout: use `timeout 5` if present, else fall back to no
  -- timeout. The macOS coreutils path is `gtimeout`; Linux is `timeout`.
  -- Both accept identical args.
  local timeout_prefix = ""
  local _, _, has_timeout = shell_capture("command -v timeout >/dev/null && echo y")
  if has_timeout == 0 then
    timeout_prefix = "timeout 5 "
  else
    local _, _, has_gtimeout = shell_capture("command -v gtimeout >/dev/null && echo y")
    if has_gtimeout == 0 then
      timeout_prefix = "gtimeout 5 "
    end
  end

  local cmd = env_prefix .. timeout_prefix .. sh_quote(dp_path) .. " auth print-token"
  local stdout, stderr, exit = shell_capture(cmd)
  -- timeout/gtimeout exit 124 on timeout fired.
  if exit == 124 then
    error("dp-timeout", 0)
  end

  local raw = (stdout or ""):gsub("^%s+", ""):gsub("%s+$", "")

  -- Empty / "no access token" / "authorize" / suspiciously short
  -- output — treat all as not-logged-in. Real DP tokens are >100 chars.
  local err_text = (raw .. "\n" .. (stderr or "")):lower()
  if #raw == 0
      or raw:lower():find("no access token", 1, true)
      or raw:lower():find("authorize", 1, true)
      or err_text:find("no access token", 1, true)
      or #raw < 10 then
    error("not-logged-in", 0)
  end

  if exit ~= 0 then
    error("dp-failed: " .. (stderr or "(no stderr)"), 0)
  end

  return raw
end

-- Best-effort UUID v4-ish for X-Request-Id. The Nestor backend logs
-- this to correlate requests; uniqueness matters more than RFC4122
-- conformance.
local function uuid()
  math.randomseed((os.time() * 1000) + math.floor((os.clock() or 0) * 1e6))
  local function hex(n)
    local s = ""
    for _ = 1, n do s = s .. string.format("%x", math.random(0, 15)) end
    return s
  end
  return hex(8) .. "-" .. hex(4) .. "-4" .. hex(3) .. "-" ..
         string.format("%x", math.random(8, 11)) .. hex(3) .. "-" .. hex(12)
end

-- Returns { jwt = "...", expires_at_ms = number }, or raises. Shells
-- out to curl rather than introducing a pure-Lua HTTP library — curl
-- ships everywhere, supports HTTP/2, handles TLS via the system store,
-- and the engine has no built-in HTTP client. --max-time caps the
-- call at 10s so a wedged endpoint can't hang startup.
local function exchange_for_jwt(dp_token)
  local req_id = uuid()
  -- os.tmpname() — see shell_capture for the rationale.
  local body_path = os.tmpname()
  local cmd = table.concat({
    "curl",
    "--silent",                       -- no progress meter
    "--show-error",                   -- but DO show errors on stderr
    "--max-time 10",                  -- hard cap
    "--write-out '%{http_code}'",     -- last line of stdout
    "--output " .. sh_quote(body_path),
    "-H 'Content-Type: application/json'",
    "-H " .. sh_quote("Authorization: Bearer " .. dp_token),
    "-H " .. sh_quote("X-Request-Id: " .. req_id),
    "-X POST",
    "-d '{}'",
    sh_quote(TOKEN_ENDPOINT),
  }, " ")

  local stdout, stderr, exit = shell_capture(cmd)
  if exit ~= 0 then
    pcall(os.remove, body_path)
    error("token-exchange-curl-failed: exit=" .. tostring(exit) ..
          " stderr=" .. (stderr or "(empty)"), 0)
  end

  local status = tonumber((stdout or ""):match("(%d+)%s*$") or "")
  local body_fh = io.open(body_path, "r")
  local body = body_fh and body_fh:read("*a") or ""
  if body_fh then body_fh:close() end
  pcall(os.remove, body_path)

  if status == nil or status >= 300 then
    -- Deliberately omit the response body: the /api/v2/token endpoint
    -- can echo back submitted credentials or correlation IDs in error
    -- responses, and this error surfaces to user-visible logs.
    error(string.format("token-exchange-failed: status=%s endpoint=%s",
          tostring(status), TOKEN_ENDPOINT), 0)
  end

  local ok, data = pcall(json.decode, body)
  if not ok or type(data) ~= "table" or type(data.jwt) ~= "string" then
    error("token-exchange-malformed: " .. body:sub(1, 500), 0)
  end

  -- expires_at is ISO8601. We don't strictly need the exact ms; we
  -- just want a "should we refresh?" check. Best-effort parse: if the
  -- string parses as a date, convert; otherwise treat as 1h-from-now.
  local expires_at_ms
  if type(data.token) == "table" and type(data.token.expires_at) == "string" then
    -- `date -d` is GNU; `date -j -f` is BSD/macOS. Try BSD form first
    -- since the team is on macOS per current env; fall back to GNU.
    local iso = data.token.expires_at
    -- Strip subsecond + Z suffix variability for a portable parse:
    iso = iso:gsub("%.%d+", ""):gsub("Z$", "+0000")
    local cmd1 = "date -j -f '%Y-%m-%dT%H:%M:%S%z' " .. sh_quote(iso) .. " +%s"
    local out1, _, ex1 = shell_capture(cmd1)
    local epoch
    if ex1 == 0 then
      epoch = tonumber((out1 or ""):match("%d+"))
    else
      local cmd2 = "date -d " .. sh_quote(data.token.expires_at) .. " +%s"
      local out2, _, ex2 = shell_capture(cmd2)
      if ex2 == 0 then
        epoch = tonumber((out2 or ""):match("%d+"))
      end
    end
    if epoch ~= nil then
      expires_at_ms = epoch * 1000
    end
  end
  if expires_at_ms == nil then
    expires_at_ms = (os.time() + 3600) * 1000  -- conservative 1h default
  end

  return { jwt = data.jwt, expires_at_ms = expires_at_ms }
end

-- Returns an array of { name, desc?, is_default? }. On any failure
-- returns an empty array — the caller decides whether to abort startup
-- or run with a hardcoded fallback model.
local function fetch_models(jwt)
  -- os.tmpname() — see shell_capture for the rationale.
  local body_path = os.tmpname()
  local cmd = table.concat({
    "curl",
    "--silent", "--show-error",
    "--max-time 10",
    "--write-out '%{http_code}'",
    "--output " .. sh_quote(body_path),
    "-H 'Content-Type: application/json'",
    "-H " .. sh_quote("Nestor-Token: " .. jwt),
    sh_quote(MODELS_ENDPOINT),
  }, " ")

  local stdout, _stderr, exit = shell_capture(cmd)
  if exit ~= 0 then
    pcall(os.remove, body_path)
    return {}
  end

  local status = tonumber((stdout or ""):match("(%d+)%s*$") or "")
  local body_fh = io.open(body_path, "r")
  local body = body_fh and body_fh:read("*a") or ""
  if body_fh then body_fh:close() end
  pcall(os.remove, body_path)

  if status == nil or status >= 300 then return {} end

  local ok, data = pcall(json.decode, body)
  if not ok then return {} end

  -- Server may return either a bare array or `{ models = [...] }`.
  local list
  if type(data) == "table" then
    if data[1] ~= nil then
      list = data
    elseif type(data.models) == "table" then
      list = data.models
    end
  end
  return list or {}
end

-- Authenticate against Nestor and return the JWT. The JWT is cached
-- in module-local state; subsequent calls within the cache window
-- return the cached value, and tokens within 60s of expiry trigger a
-- re-exchange. Raises a user-actionable error on failure.
function M.get_jwt()
  if cached ~= nil then
    local now_ms = os.time() * 1000
    if cached.expires_at_ms - now_ms > 60 * 1000 then
      return cached.jwt
    end
  end

  local dp = find_dp_binary()
  if dp == nil then
    error("dp CLI not found. Install it (see <INTERNAL_DP_DOCS_URL> in README) " ..
          "and run `dp auth login`, then restart nefor.", 0)
  end

  local ok, raw_or_err = pcall(get_dp_token, dp)
  if not ok then
    local msg = tostring(raw_or_err)
    if msg:find("not%-logged%-in") then
      error("DP authentication required. Run `dp auth login` in another " ..
            "terminal, then restart nefor.", 0)
    elseif msg:find("dp%-timeout") then
      error("`dp auth print-token` timed out (>5s). The DP daemon may be " ..
            "wedged — try `dp auth logout && dp auth login`, then restart nefor.", 0)
    else
      error("DP authentication failed: " .. msg, 0)
    end
  end

  local pair = exchange_for_jwt(raw_or_err)
  cached = pair
  return pair.jwt
end

-- Fetch the model list. Caller must hand in a JWT (call get_jwt
-- first); split apart so auth and discovery failure modes stay
-- separable in the startup banner.
function M.list_models(jwt)
  return fetch_models(jwt)
end

M.NESTOR_BASE      = NESTOR_BASE
M.TOKEN_ENDPOINT   = TOKEN_ENDPOINT
M.MODELS_ENDPOINT  = MODELS_ENDPOINT

return M
