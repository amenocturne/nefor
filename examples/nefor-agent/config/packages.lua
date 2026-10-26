local da_package = require("libs.tool-validator.da")
local M = { classifier_version = "0.3.0" }

function M.external()
  return {
    da_package.release_package {
      version = M.classifier_version,
      checksums = {
        ["aarch64-apple-darwin"] = "04fd66aba05497c954e729b8e48c5eaf87d6319292bbf75cc3b4f7967078e053",
        ["aarch64-unknown-linux-gnu"] = "0cdac8e2a4bc7488a7c4bb37ac1e475a7adabbb6b1b49156f76b2d11751b4503",
        ["x86_64-unknown-linux-gnu"] = "06c71d7e18e9e7a9cc6027a641b400ff97e99f5900f4972eef214b77a07b2a1b",
      },
    },
  }
end

return M
