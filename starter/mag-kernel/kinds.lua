-- starter/mag-kernel/kinds.lua — the canonical reserved-kind constants.
--
-- The kernel and its factories agree on a small set of reserved message /
-- event kinds (plugins/mag/docs/actor-model.md, Signals; docs/ir.md, Firing).
-- Before this module they were re-spelled as string literals in every file,
-- which let naming drift creep in (mag.Completed vs mag.complete; mag.RunComplete
-- vs mag.run_complete). This is the single source of truth; kernel modules and
-- factories require it and never spell a reserved name inline.
--
-- Two channels, two casing conventions (kept deliberately distinct):
--
--   * reserved MESSAGE kinds — an actor emits these through its emitter; the
--     kernel intercepts them (routing.lua on_emit). PascalCase for the
--     kernel-visible status/terminal messages (Unit, Failed, RunComplete),
--     lowercase for the async-completion acks (ready, complete, failed) that
--     mirror the pending-activation vocabulary.
--   * lifecycle EVENT kinds — the kernel emits these to the control plane
--     (observer.lua M.EVENTS). snake_case throughout. Only the run-complete
--     event lives here (it is the one that drifted against the message kind);
--     the rest stay in observer.lua's canonical set.
--
-- The message kind `mag.RunComplete` (the sink's terminal emit) and the event
-- kind `mag.run_complete` (the lifecycle surfacing of it) are intentionally
-- two names: one is the actor→kernel signal, the other the kernel→control-plane
-- event. Unifying them would conflate the two channels.

local M = {
  -- Kernel-synthesized completion status (docs/ir.md, Firing). A factory never
  -- returns or declares these; the kernel emits them when applying a completion.
  Unit = "mag.Unit", -- successful completion (a pure dependency edge, C -> A)
  Failed = "mag.Failed", -- a suffered failure the kernel synthesizes

  -- Reserved emit acks an actor sends through its emitter (routing intercepts).
  ready = "mag.ready", -- the ready barrier confirmation
  complete = "mag.complete", -- deferred-activation success (async completion / drain flush)
  failed = "mag.failed", -- deferred-activation failure (async completion)

  -- The sink's terminal completion MESSAGE (factories/sink.lua) and the
  -- lifecycle EVENT the delivery layer surfaces from it (observer.lua).
  RunComplete = "mag.RunComplete", -- actor -> kernel emit (message)
  run_complete = "mag.run_complete", -- kernel -> control plane (event)
}

return M
