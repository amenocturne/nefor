-- examples/nefor-agent/tool-validator — config composition for command classification.
--
-- This configuration's explicit classifier flags (`da` 0.3) for each
-- approval policy.
--
-- ReadOnly is the strict read-only rule stack and never consults the model:
-- a command no rule recognises is deferred, which a ReadOnly agent's call
-- does not survive.
-- Default is the wide stack: it mirrors da's README example, minus
-- --mkdir-cwd (which is `--path`-bound; the agent's cwd is the engine's cwd,
-- not per-call, so the path scope is ambiguous and we'd false-defer on
-- legitimate mkdirs), plus `--ml`, the classifier's embedded model, for
-- commands no rule recognises: it approves what it is confident is safe and
-- denies what it classifies as dangerous.
--
-- Neither stack passes a mode flag (--interactive / --autonomous), so the
-- classifier answers approve / defer / deny and the gate's table decides what
-- each means in the current permission mode.

local CLASSIFIER_ARGS = {
  ReadOnly = {
    "--read-only",
    "--macos-only",
    "--help-bypass",
    "--git", "read",
  },
  Default = {
    "--read-only",
    "--macos-only",
    "--help-bypass",
    "--git",   "read,add,commit,restore-staged,tag,fetch,pull,push",
    "--cargo", "local",
    "--ml",
  },
}

return {
  CLASSIFIER_ARGS = CLASSIFIER_ARGS,
  build = function(opts)
    return require("libs.tool-validator").build {
      classifier = opts.classifier,
      classifier_args = CLASSIFIER_ARGS,
      -- This configuration approves no command ahead of the classifier.
      command_fastpaths = {},
    }
  end,
}
