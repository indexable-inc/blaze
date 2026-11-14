import Blaze.Auto
import BlazeModel.blaze_core.wait.reaping.a_reaped_child_is_reported

/-!
`blaze_core::wait::reaping` (crates/blaze-core/src/wait/reaping.rs): the reap of a child, proved
over its generated models.

- `a_reaped_child_is_reported`: for every life the child starts with, every moment its exit
  comes, every choice of the kernel's and every deadline or none, the real `reap` against one
  child's world ends as `reaps` says. No loop and no fuel: a finite case analysis. A child that
  has exited is reaped at the first look; a running one is split over `between`, `during`,
  `told` and the deadline. The plant is `ReapPlant.None`, whose comparison is `true`, so the
  wait keeps the child and the reap after it finds it. The opaque `no_child`
  (`io::Error::other`) sits only in arms no case takes.

The statement says the function returns `Result.ok true`.
-/

open Blaze

namespace BlazeCore.Wait.ReapingProofs

open blaze_core.wait.reaping blaze_core.world.children

/-- A child that has exited is reaped at the first look and reported with its status. -/
theorem reaps_exited (s : I32) (between during : Option I32) (told : Bool)
    (deadline : Option blaze_core.wait.retry.Deadline) :
    reaps .None (.Exited s) between during told deadline = .ok true := by
  simp [reaps, run, reap.Script_Still, Script.Children.reap_now, Child.reap_now,
    core.result.Result.Option_i32_WaitError.Try.branch,
    core.result.Result.Option_i32_Error.map_err.WaitError_closure0_wait_reaping_reap_Script_Still]

/-- A running child: each place its exit comes, each choice of the kernel's, each deadline. -/
theorem reaps_running (between during : Option I32) (told : Bool)
    (deadline : Option blaze_core.wait.retry.Deadline) :
    reaps .None .Running between during told deadline = .ok true := by
  cases between <;> cases during <;> cases told <;> cases deadline <;>
  simp [reaps, run, reap.Script_Still, Script.Children.reap_now, Script.Children.exited.Still,
    Script.looked, Child.reap_now, Child.exit, Child.waited, core.cmp.PartialEq.eq,
    core.result.Result.Option_i32_WaitError.Try.branch,
    core.result.Result.Waited_unit_WaitError.Try.branch,
    core.result.Result.Option_i32_Error.map_err.WaitError_closure0_wait_reaping_reap_Script_Still,
    core.result.Result.Option_i32_Error.map_err.WaitError_closure1_wait_reaping_reap_Script_Still]

end BlazeCore.Wait.ReapingProofs

open BlazeCore.Wait.ReapingProofs in
theorem blaze_core.wait.reaping.a_reaped_child_is_reported.spec.proof :
    blaze_core.wait.reaping.a_reaped_child_is_reported.spec.statement := by
  intro running exited between during told deadline
  cases running <;> simp [blaze_core.wait.reaping.a_reaped_child_is_reported, reaps_exited,
    reaps_running]
