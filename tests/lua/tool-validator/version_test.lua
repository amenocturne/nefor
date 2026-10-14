-- Synthetic process results exercise the da protocol and cached validator check.
local da = require("libs.tool-validator.da")
local tv = require("libs.tool-validator")
local original_run = nefor.process.run
local path = "/managed/da/bin/da"
local calls = {}
local response = { code = 0, stdout = " \tda 0.3.0\n" }
nefor.process.run = function(opts)
  calls[#calls + 1] = opts
  assert(opts.cmd == path and #opts.args == 1 and opts.args[1] == "--version")
  return response
end
local verified = da.verify_version(path, "0.3.0")
assert(verified.version == "0.3.0" and verified.executable == path)
for _, case in ipairs({
  { code = 0, stdout = "da 0.2.0\n" },
  { code = 0, stdout = "other 0.3.0\n" },
  { code = 0, stdout = "da 0.3.0 extra\n" },
  { code = 0, stdout = "da 0.3.0\nwarning" },
  { code = 0, stdout = "da 0.3.0-beta\n" },
  { code = 0, stdout = "" },
  { code = -1, stderr = "missing executable" },
  { code = 1, stdout = "da 0.3.0\n" },
  false,
}) do
  response = case
  local ok, err = pcall(da.verify_version, path, "0.3.0")
  assert(not ok)
  assert(tostring(err):find(path, 1, true) and tostring(err):find("expected da 0.3.0", 1, true))
  assert(tostring(err):find("actual", 1, true))
end
local count = #calls
for _, version in ipairs({ "", "v0.3.0", "0.3", "0.3.0-beta", false }) do
  assert(not pcall(da.verify_version, path, version))
end
assert(not pcall(da.verify_version, "", "0.3.0"))
assert(#calls == count, "invalid expectations fail without invoking processes")

-- Release verification uses the supplied version and the materialized package,
-- even if the caller later mutates its options. No package manager is called.
nefor.process.run = function(opts)
  assert(opts.cmd == "uname")
  return { code = 0, stdout = opts.args[1] == "-s" and "Linux\n" or "x86_64\n" }
end
local opts = { version = "0.3.0", checksums = { ["x86_64-unknown-linux-gnu"] = string.rep("a", 64) } }
local spec = da.release_package(opts)
opts.version = "0.4.0"
assert(spec.archive.url:find("/v0.3.0/", 1, true))
nefor.process.run = function(args)
  assert(args.cmd == path and args.args[1] == "--version")
  return { code = 0, stdout = "da 0.3.0\n" }
end
local evidence = spec.verify { name = "da", dir = "/managed/da", identity = { kind = "archive" } }
assert(evidence.version == "0.3.0" and evidence.executable == path)
assert(da.package { commit = string.rep("a", 40) }.verify == nil,
  "source builds must not invent a release version")
local source = da.package { commit = string.rep("a", 40), version = "0.3.0" }
assert(source.verify { dir = "/managed/da" }.version == "0.3.0")

local function build(check)
  return tv.build {
    classifier = path, classifier_check = check,
    classifier_args = { ReadOnly = { "--read-only" }, Default = { "--ml" } },
    command_fastpaths = {},
  }
end
assert(not pcall(build, true), "callback type is checked at construction")
assert(not pcall(build, false), "false is not an absent callback")
local probes, classifications = 0, 0
local current_version = "0.2.0"
nefor.process.run = function(args)
  assert(args.cmd == path, "only the configured executable is run; no installs or downloads")
  if args.args[1] == "--version" then
    probes = probes + 1
    return { code = 0, stdout = "da " .. current_version }
  end
  classifications = classifications + 1
  assert(args.args[1] == "--ml" and args.stdin == "git status")
  return { code = 0 }
end
-- Pass through the starter wrapper, proving it retains the check.
local validator = require("tool-validator").build {
  classifier = path, classifier_check = function(p) return da.verify_version(p, "0.3.0") end,
}
local ok, err = pcall(validator.classify_command, "Default", "git status")
assert(not ok and tostring(err):find("expected da 0.3.0", 1, true))
assert(probes == 1 and classifications == 0, "mismatch fails before classification")
-- A failed check is not cached. Use the generic validator to check its flags.
validator = build(function(p) return da.verify_version(p, "0.3.0") end)
assert(not pcall(validator.classify_command, "Default", "git status"))
current_version = "0.3.0"
assert(validator.classify_command("Default", "git status") == "safe")
assert(validator.classify_command("Default", "git status") == "safe")
assert(probes == 3 and classifications == 2, "only successful checks are cached")
nefor.process.run = original_run
return true
