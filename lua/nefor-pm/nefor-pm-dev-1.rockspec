rockspec_format = "3.0"
package = "nefor-pm"
version = "dev-1"
source = { url = "git+https://github.com/amenocturne/nefor.git" }
description = {
  summary = "Replaceable Lua package/source manager for Nefor compositions",
  license = "MIT",
}
dependencies = { "lua >= 5.4, < 5.5", "http == 0.4" }
build = {
  type = "builtin",
  modules = {
    ["nefor-pm"] = "lua/nefor-pm/init.lua",
    ["nefor-pm.archive"] = "lua/nefor-pm/archive.lua",
    ["nefor-pm.download"] = "lua/nefor-pm/download.lua",
  },
}
