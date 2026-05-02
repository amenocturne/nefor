-- Phase-5b animation integration scenario.
--
-- Renders a 4-frame spinner over 100ms (25ms per frame) so the
-- integration test can step the engine clock by ~30ms between renders
-- and observe frame_index advancement.

tui.start {
  initial_state = {},
  view = function(_)
    return tui.animation {
      frames      = { "A", "B", "C", "D" },
      duration_ms = 100,
    }
  end,
  update = function(_, s) return s, {} end,
}
