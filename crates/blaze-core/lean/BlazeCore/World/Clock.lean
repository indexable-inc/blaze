import Blaze.Auto
import BlazeModel.blaze_core.world.clock.a_timer_never_fires_early

/-!
`blaze_core::world::clock` (crates/blaze-core/src/world/clock.rs): the clock a sans-IO core
reads, as a reference model.

- `a_timer_never_fires_early`: for every clock, wait and reading, a timer set now that the
  clock allows fired at the reading waited at least its time (or the deadline did not fit a
  `u64` and the reading is the last there is), the clock after the reading allows none before
  it, and nothing in the clock's functions overflows or panics.
-/

open Blaze Std.WP Lean.Order

theorem blaze_core.world.clock.a_timer_never_fires_early.spec.proof :
    blaze_core.world.clock.a_timer_never_fires_early.spec.statement := by
  intro now after reading later
  blaze_auto [blaze_core.world.clock.a_timer_never_fires_early,
    blaze_core.world.clock.Clock.deadline, blaze_core.world.clock.Clock.fires,
    blaze_core.world.clock.Clock.allows, blaze_core.world.clock.Clock.read,
    Scalar.saturating_add, Scalar.clamp_val]
