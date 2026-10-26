-- Use the starter's real package declarations and lifecycle, without starting actors.
local starter = assert(os.getenv("NEFOR_TEST_STARTER_CONFIG"))
package.path = starter .. "/?.lua;" .. starter .. "/?/init.lua;" .. package.path
local distribution = require("config.distribution")
distribution.load_lua_dependencies()
local runtime = distribution.runtime_root()
package.path = runtime .. "/lua/?.lua;" .. runtime .. "/lua/?/init.lua;" .. package.path
local pm = require("nefor-pm")
pm.install(require("config.packages").external(), { on_progress = pm.stderr_progress })
function dispatch() end
nefor.engine.shutdown { code = 0, reason = "TUI dependencies prepared", grace_ms = 1000 }
