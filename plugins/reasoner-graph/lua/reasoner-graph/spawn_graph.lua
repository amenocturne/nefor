-- plugins/reasoner-graph/lua/reasoner-graph/spawn_graph.lua
--
-- Thin re-export of the spawn-graph protocol contract, which now lives
-- under `lua/libs/spawn-graph/` so the tool-gate plugin lib can consume
-- it without reaching into another plugin's namespace. Existing
-- consumers that already require this module (e.g. `agentic-loop`) keep
-- working unchanged.

return require("libs.spawn-graph")
