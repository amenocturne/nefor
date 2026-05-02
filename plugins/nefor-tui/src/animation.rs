//! `tui.animation` per-instance state + frame sampling.
//!
//! Time-as-source-of-truth: each instance records its `mount_time_ms`
//! (the engine's monotonic-ish clock value when first observed) and
//! samples the current frame on every render. No per-component clock,
//! no per-frame reschedule. Survives across re-renders via the
//! reconciler so a re-rendered animation does not jump back to frame
//! zero each frame.
//!
//! Sampling math (per spec § Animation):
//! - `total_cycles = elapsed_ms / duration_ms`
//! - `forward_index = floor((total_cycles mod 1) * #frames)` clamped to
//!   `0..#frames`
//! - `forward`   → `forward_index`
//! - `reverse`   → `#frames - 1 - forward_index`
//! - `alternate` → period-2 cycle. On `[0, 1)` use `forward_index`; on
//!   `[1, 2)` use `#frames - 1 - forward_index`.
//! - `iterations = Some(n)`: when `elapsed_ms >= n * duration_ms`, hold
//!   the end frame for the active direction (last for `forward`, first
//!   for `reverse`, leg-appropriate for `alternate`).
//!
//! A render-time "absurd speed" case — `duration_ms / #frames` smaller
//! than the render frame period — is handled gracefully: sampling drops
//! whatever frames the renderer didn't get to, since we always derive
//! the index from elapsed time, never from a step counter.

use crate::desc::AnimationDirection;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnimationState {
    /// Engine-clock millisecond value at first observation. `None` until
    /// the first sample call seeds it.
    pub mount_time_ms: Option<u64>,
}

/// Result of sampling one animation instance at the current `now_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnimationSample {
    /// Frame index into the description's `frames` array.
    pub frame_index: usize,
    /// `true` when iterations are bounded and have run out — the final
    /// frame is held. The engine uses this to decide whether the
    /// instance should still keep the render loop alive at frame rate.
    pub completed: bool,
}

/// Compute `frame_index` and the completion flag for an animation
/// described by `(frames_len, duration_ms, iterations, direction)`,
/// observed at `mount_time_ms` and sampled at `now_ms`.
///
/// Implements the spec formula directly:
///   `frame_index_forward = floor((elapsed/duration % 1) * frames)` clamped
/// then per direction:
///   forward   → frame_index_forward
///   reverse   → frames-1 - frame_index_forward
///   alternate → forward on the even legs, reversed on the odd legs
///
/// `iterations = Some(n)`: when `elapsed >= n * duration`, hold the last
/// frame for `forward`, the first frame for `reverse`, and the leg-
/// appropriate end frame for `alternate`.
pub fn sample(
    frames_len: usize,
    duration_ms: u64,
    iterations: Option<u32>,
    direction: AnimationDirection,
    mount_time_ms: u64,
    now_ms: u64,
) -> AnimationSample {
    debug_assert!(frames_len > 0, "animation must have ≥ 1 frame");
    debug_assert!(duration_ms > 0, "animation must have duration > 0");

    let elapsed = now_ms.saturating_sub(mount_time_ms);

    // Completion check: elapsed_ms >= iterations * duration_ms.
    if let Some(n) = iterations {
        let cap = (n as u128) * (duration_ms as u128);
        if (elapsed as u128) >= cap {
            let frame_index = match direction {
                AnimationDirection::Forward => frames_len - 1,
                AnimationDirection::Reverse => 0,
                // For alternate, the n-th leg ends pointing wherever
                // playback was about to flip. Even-iteration end → leg
                // (n-1) was reverse, ended at frame 0; odd-iteration
                // end → leg (n-1) was forward, ended at last frame.
                AnimationDirection::Alternate => {
                    if n % 2 == 1 {
                        frames_len - 1
                    } else {
                        0
                    }
                }
            };
            return AnimationSample {
                frame_index,
                completed: true,
            };
        }
    }

    // f64 is plenty here — we use ms resolution and animations are
    // typically a few seconds long; f64 exact-int range comfortably
    // covers > 285 million years in ms.
    let total_cycles = elapsed as f64 / duration_ms as f64;
    let frames_f = frames_len as f64;

    // Forward sampling: `floor((t mod 1) * #frames)` clamped.
    let forward_index = {
        let t = total_cycles.rem_euclid(1.0);
        ((t * frames_f).floor() as isize).clamp(0, frames_len as isize - 1) as usize
    };

    let frame_index = match direction {
        AnimationDirection::Forward => forward_index,
        AnimationDirection::Reverse => frames_len - 1 - forward_index,
        AnimationDirection::Alternate => {
            // Period-2 cycle: [0,1) forward, [1,2) reverse.
            let cycle_pos = total_cycles.rem_euclid(2.0);
            if cycle_pos < 1.0 {
                forward_index
            } else {
                frames_len - 1 - forward_index
            }
        }
    };

    AnimationSample {
        frame_index,
        completed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_advances_through_frames() {
        // 4 frames over 1000ms — 250ms per frame.
        for (now, expected) in [(0, 0), (250, 1), (500, 2), (999, 3)] {
            let s = sample(4, 1000, None, AnimationDirection::Forward, 0, now);
            assert_eq!(s.frame_index, expected, "@ {now}ms");
            assert!(!s.completed);
        }
    }

    #[test]
    fn forward_wraps_indefinitely() {
        // Past one cycle, frame should wrap.
        let s = sample(4, 1000, None, AnimationDirection::Forward, 0, 1250);
        assert_eq!(s.frame_index, 1);
        assert!(!s.completed);
    }

    #[test]
    fn finite_iterations_hold_last_frame() {
        let s = sample(4, 1000, Some(2), AnimationDirection::Forward, 0, 5000);
        assert_eq!(s.frame_index, 3, "should hold final frame");
        assert!(s.completed);
    }

    #[test]
    fn reverse_counts_down() {
        // Frames 0..4. At t=0, reverse should be at the end (or near it).
        let s = sample(4, 1000, None, AnimationDirection::Reverse, 0, 0);
        assert_eq!(s.frame_index, 3, "reverse @ t=0 should pick last frame");
        // Just past 0 — just inside the first leg, dropping toward index 2.
        let s = sample(4, 1000, None, AnimationDirection::Reverse, 0, 250);
        assert_eq!(s.frame_index, 2);
        let s = sample(4, 1000, None, AnimationDirection::Reverse, 0, 750);
        assert_eq!(s.frame_index, 0);
    }

    #[test]
    fn reverse_finite_holds_first_frame() {
        let s = sample(4, 1000, Some(2), AnimationDirection::Reverse, 0, 5000);
        assert_eq!(s.frame_index, 0);
        assert!(s.completed);
    }

    #[test]
    fn alternate_goes_forward_then_back() {
        // 2 frames over 1000ms duration. Alternate:
        // t in [0, 1)  → forward leg → frame 0 then 1
        // t in [1, 2)  → reverse leg → frame 1 then 0
        let s = sample(2, 1000, None, AnimationDirection::Alternate, 0, 0);
        assert_eq!(s.frame_index, 0);
        let s = sample(2, 1000, None, AnimationDirection::Alternate, 0, 600);
        assert_eq!(s.frame_index, 1);
        let s = sample(2, 1000, None, AnimationDirection::Alternate, 0, 1100);
        assert_eq!(s.frame_index, 1);
        let s = sample(2, 1000, None, AnimationDirection::Alternate, 0, 1700);
        assert_eq!(s.frame_index, 0);
    }

    #[test]
    fn high_speed_sampling_drops_intermediate_frames_gracefully() {
        // 1000 frames over 10ms — frame period 0.01ms. Sampling every
        // 1ms must still yield a well-formed in-range index, never
        // panic, and stay strictly inside [0, frames_len).
        for now in 0..1000 {
            let s = sample(1000, 10, None, AnimationDirection::Forward, 0, now);
            assert!(s.frame_index < 1000);
            assert!(!s.completed);
        }
    }

    #[test]
    fn mount_offset_shifts_origin() {
        // Animation that mounted at 500ms should treat now=750 as 250ms
        // elapsed (frame 1 of 4 at 1000ms duration).
        let s = sample(4, 1000, None, AnimationDirection::Forward, 500, 750);
        assert_eq!(s.frame_index, 1);
    }

    #[test]
    fn single_frame_animation_always_zero() {
        let s = sample(1, 1000, None, AnimationDirection::Forward, 0, 9999);
        assert_eq!(s.frame_index, 0);
    }
}
