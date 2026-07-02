-- starter/mag-kernel/init.lua — MAG actor-kernel entry (stub).
--
-- Loaded by the `mag` plugin's embedded Lua VM at startup. In the full
-- runtime this module owns the kernel: the actor inventory, the ready
-- barrier, pending mailboxes, routing, and the fold over graph
-- modifications produced by the resident nefor-mag evaluator (see
-- plugins/mag/docs/actor-model.md and docs/ir.md).
--
-- For now it is a skeleton: it confirms the host wired the VM correctly
-- and returns the kernel table the plugin holds onto. No kernel behavior
-- is implemented yet.
--
-- `nefor.log` is a host binding that writes to the plugin's tracing
-- subscriber (stderr). The kernel must never write to stdout — that is
-- the NCP wire.

nefor.log("mag-kernel stub loaded")

return {
  name = "mag-kernel",
  stub = true,
}
