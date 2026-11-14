-- Hand-written, not generated: the proof of the `blaze_shapes` fixture's statement.
import BlazeModel.blaze_shapes.hunts_find_the_hit

/-!
A loop whose `match` returns (`hunt`) or breaks with (`hunt_past`) the field of the arm it
matched (crates/hir-ty-lean/src/tests.rs, `WORLD_SHAPES`): from 0 the first hit is 3, so
`hunt` gives 6 and gives 0 when it may only try twice; from 4 the first hit is 10, so
`hunt_past` gives 21. It is closed, so the kernel decides it by evaluation, with fuel enough
for the longest of the loops (seven rounds).
-/

open Blaze

theorem blaze_shapes.hunts_find_the_hit.spec.proof : blaze_shapes.hunts_find_the_hit.spec.statement :=
  ⟨8, by rfl⟩
