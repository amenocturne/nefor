-- examples/nefor-agent/tool-validator — config composition for command classification.
--
-- The starter uses the mechanism's default classifier flags for each approval
-- policy. A configuration that needs different flags, a model flag, or
-- command fast paths passes them here.

return {
  build = function(opts)
    opts = opts or {}
    return require("libs.tool-validator").build {
      classifier = opts.classifier,
      classifier_args = opts.classifier_args,
      model_flag = opts.model_flag,
      command_fastpaths = opts.command_fastpaths,
    }
  end,
}
