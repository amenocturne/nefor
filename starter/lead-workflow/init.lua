-- starter/lead-workflow/init.lua — team shim over shared 0.4 lead workflow.
--
-- The 0.4 mechanism lives in lua/libs/lead-workflow: write-review approval,
-- MAG compile/execute, graph-status/terminate-graph, mag-eval, run tracking,
-- and session cleanup. Team-specific behavior now lives in prompts, tool policy,
-- and MAG role/tool libraries; do not carry the old dispatch-graph actor here.
return require('libs.lead-workflow')
