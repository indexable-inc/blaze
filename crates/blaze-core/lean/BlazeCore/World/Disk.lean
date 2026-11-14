import Blaze.Auto
import BlazeCore.World.DiskAllows
import BlazeCore.World.DiskMap
import BlazeModel.blaze_core.world.disk.a_crash_keeps_what_a_barrier_ordered

/-!
`blaze_core::world::disk` (crates/blaze-core/src/world/disk.rs): the disk world's crash
theorems, proved over the generated models with the specifications of `DiskMap` (the disk's
functions as a sorted map of files) and `DiskAllows` (what a cut the disk allows keeps).

- `a_crash_keeps_what_a_barrier_ordered`: from every well-formed disk within the bounds, below
  the last epoch, once a barrier of `file` that covers all its changes has completed and bytes
  were then appended to `later`, a cut the disk allows that keeps any part of that append keeps
  whole every change the barrier covered; and nothing on the way panics or runs out of fuel.

Fuel: `2^67`, more than every loop needs (`allows` takes `2^66`, `wf` `2^66`, `small` `2^65`,
and `find`, `slot` and `open` more than the files, which are at most `usize::MAX`).
-/

open Blaze Std.WP Lean.Order

open blaze_core.world.disk BlazeCore.World.DiskAllows BlazeCore.World.DiskMap in
theorem blaze_core.world.disk.a_crash_keeps_what_a_barrier_ordered.spec.proof :
    blaze_core.world.disk.a_crash_keeps_what_a_barrier_ordered.spec.statement := by
  intro disk file later bytes cut
  refine ⟨147573952589676412928, ?_⟩
  have hmax : (18446744073709551615 : U64).val = 18446744073709551615 :=
    Scalar.ofNat_val .U64 18446744073709551615 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  have hl0 := disk.files.hlen
  obtain ⟨bw, hwf, hwfs⟩ := wf_spec 147573952589676412928 disk (by omega)
  obtain ⟨bs, hsm, hsms⟩ := small_spec 147573952589676412928 disk (by omega)
  by_cases hgo : bw = true ∧ bs = true ∧ disk.epoch.val ≠ 18446744073709551615
  case neg =>
    by_cases he : disk.epoch.val = 18446744073709551615
    · cases bw <;> cases bs <;>
        simp [a_crash_keeps_what_a_barrier_ordered, hwf, hsm, hmax, he]
    · cases bw <;> cases bs <;> simp_all [a_crash_keeps_what_a_barrier_ordered]
  obtain ⟨hbw, hbs, he⟩ := hgo
  subst hbw hbs
  have hsort := hwfs rfl
  obtain ⟨hlen16, hfile16⟩ := hsms rfl
  have he' : disk.epoch.val < 18446744073709551615 := by
    have := disk.epoch.hmax
    simp only [ScalarTy.max] at this
    omega
  obtain ⟨r0, hfind, hnone, hsome⟩ := find_spec 147573952589676412928 disk file (by omega)
  cases r0 with
  | none =>
    simp [a_crash_keeps_what_a_barrier_ordered, hwf, hsm, hmax, he, hfind]
  | some at0 =>
    obtain ⟨hat, hatn, -⟩ := hsome at0 rfl
    -- The barrier.
    obtain ⟨d1, hbar, hep1, hlen1, hprop1⟩ :=
      barrier_spec 147573952589676412928 disk file
        (Slice.len (disk.files.val[at0.val.toNat]).changes) at0.val.toNat hsort (by omega)
        (by omega) hat hatn (by simp only [Slice.len_val])
        (by have := (hfile16 _ hat).2; omega) he'
    have hsort1 : Sorted d1.files.val := by
      intro i j hi hj hij
      have := hsort i j (by omega) (by omega) hij
      rw [(hprop1 i (by omega)).1, (hprop1 j (by omega)).1]
      exact this
    have hc1 : ∀ k (hk : k < d1.files.val.length),
        (d1.files.val[k]).changes.val.length < 18446744073709551615 := by
      intro k hk
      rw [(hprop1 k (by omega)).2.1]
      have := (hfile16 k (by omega)).1
      omega
    -- The append.
    obtain ⟨D, happ, hepD, ⟨q, hq, hqn⟩, hlast, hbarD, hotherD, hbackD⟩ :=
      append_spec 147573952589676412928 d1 later bytes hsort1 (by omega) (by omega) hc1
    have hlD := D.files.hlen
    have hlc := cut.hlen
    obtain ⟨ba, hal⟩ := allows_total 147573952589676412928 D cut (by omega)
    cases ba with
    | false =>
      simp [a_crash_keeps_what_a_barrier_ordered, hwf, hsm, hmax, he, hfind,
        Slice.index_ok disk.files at0 hat, hbar, happ, hal]
    | true =>
      obtain ⟨hcl, horders⟩ := allows_orders 147573952589676412928 D cut (by omega) hal
      -- `file` is still on the disk.
      have hfileD : ∃ k, ∃ hk : k < D.files.val.length, (D.files.val[k]).name.val = file.val := by
        by_cases hfl : file.val = later.val
        · exact ⟨q, hq, hqn.trans hfl.symm⟩
        · have hn1 : (d1.files.val[at0.val.toNat]'(by omega)).name.val ≠ later.val := by
            rw [(hprop1 _ hat).1, hatn]
            exact hfl
          obtain ⟨k, hk, hkp⟩ := hbackD at0.val.toNat (by omega) hn1
          exact ⟨k, hk, by rw [hkp, (hprop1 _ hat).1]; exact hatn⟩
      obtain ⟨r1, hfind1, hnone1, hsome1⟩ := find_spec 147573952589676412928 D file (by omega)
      cases r1 with
      | none =>
        obtain ⟨k, hk, hkn⟩ := hfileD
        exact absurd hkn (hnone1 rfl k hk)
      | some ordered =>
        obtain ⟨ho, hon, -⟩ := hsome1 ordered rfl
        obtain ⟨r2, hfind2, hnone2, hsome2⟩ :=
          find_spec 147573952589676412928 D later (by omega)
        cases r2 with
        | none => exact absurd hqn (hnone2 rfl q hq)
        | some appended =>
          obtain ⟨hap, hapn, -⟩ := hsome2 appended rfl
          obtain ⟨xs, hxs⟩ := hlast appended.val.toNat hap hapn
          have hlenA : (D.files.val[appended.val.toNat]).changes.val.length = xs.length + 1 := by
            rw [hxs, List.length_append, List.length_singleton]
          obtain ⟨last, hsub, hlastv⟩ :=
            sub_one (Slice.len (D.files.val[appended.val.toNat]).changes)
              (by simp only [Slice.len_val]; omega)
          have hcap : appended.val.toNat < cut.val.length := by omega
          have hcor : ordered.val.toNat < cut.val.length := by omega
          obtain ⟨r, hr, hrv⟩ := reach_spec D.files.val[appended.val.toNat] cut.val[appended.val.toNat]
          -- The barrier is on the file `file` names.
          have hbo : 0 < (Slice.len (disk.files.val[at0.val.toNat]).changes).val →
              ∃ b ∈ (D.files.val[ordered.val.toNat]).barriers.val,
                b.covers = Slice.len (disk.files.val[at0.val.toNat]).changes ∧
                  b.epoch = disk.epoch := by
            intro hc0
            obtain ⟨b, hb, hbc, hbe⟩ := (hprop1 at0.val.toNat hat).2.2 rfl hc0
            by_cases hfl : file.val = later.val
            · rw [hbarD ordered.val.toNat ho (hon.trans hfl) at0.val.toNat (by omega)
                (by rw [(hprop1 _ hat).1, hatn, hfl])]
              exact ⟨b, hb, hbc, hbe⟩
            · obtain ⟨p, hp, hpe⟩ := hotherD ordered.val.toNat ho (by rw [hon]; exact hfl)
              have hpa : p = at0.val.toNat := hsort1.unique p at0.val.toNat hp (by omega)
                (by rw [← hpe, hon, (hprop1 _ hat).1, hatn])
              subst hpa
              rw [hpe]
              exact ⟨b, hb, hbc, hbe⟩
          have key : ordered_first r last (cut.val[ordered.val.toNat]).whole
              (Slice.len (disk.files.val[at0.val.toNat]).changes) = true := by
            by_cases hrl : r.val ≤ last.val
            · simp [ordered_first, Scalar.le, hrl]
            · have hw0 := (cut.val[ordered.val.toNat]).whole.hmin
              simp only [ScalarTy.min] at hw0
              have hcw : (Slice.len (disk.files.val[at0.val.toNat]).changes).val ≤
                  (cut.val[ordered.val.toNat]).whole.val := by
                by_cases hc0 : 0 < (Slice.len (disk.files.val[at0.val.toNat]).changes).val
                · obtain ⟨b, hb, hbc, hbe⟩ := hbo hc0
                  have hcl' : (D.files.val[appended.val.toNat]).changes.val.length - 1 <
                      (D.files.val[appended.val.toNat]).changes.val.length := by omega
                  have hepc : ((D.files.val[appended.val.toNat]).changes.val[
                      (D.files.val[appended.val.toNat]).changes.val.length - 1]).epoch = d1.epoch := by
                    simp [hxs]
                  have := horders ordered.val.toNat ho hcor appended.val.toNat hap hcap b hb
                    ((D.files.val[appended.val.toNat]).changes.val.length - 1) hcl'
                    (by simp only [Slice.len_val] at hlastv; omega)
                    (by rw [hepc, hbe]; omega)
                  rw [← hbc]
                  exact this
                · omega
              simp only [Slice.len_val] at hcw
              simp [ordered_first, Scalar.le, Scalar.ge, hrl, hcw]
          simp [a_crash_keeps_what_a_barrier_ordered, hwf, hsm, hmax, he, hfind,
            Slice.index_ok disk.files at0 hat, hbar, happ, hal, hfind1, hfind2,
            Slice.index_ok D.files appended hap, hsub, Slice.index_ok cut appended hcap, hr,
            Slice.index_ok cut ordered hcor, key]
