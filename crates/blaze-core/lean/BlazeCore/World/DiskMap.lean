import Blaze.Auto
import BlazeCore.World.DiskAllows
import BlazeModel.blaze_core.world.disk.Disk.wf
import BlazeModel.blaze_core.world.disk.Disk.small
import BlazeModel.blaze_core.world.disk.Disk.find
import BlazeModel.blaze_core.world.disk.Disk.append
import BlazeModel.blaze_core.world.disk.Disk.barrier

/-!
`blaze_core::world::disk` (crates/blaze-core/src/world/disk.rs): the disk's functions as a
sorted map of files, proved over their generated models as loop specifications (by induction on
fuel, as equations `f fuel .. = Result.ok ..`).

- `wf_spec`, `small_spec`: `Disk::wf` and `Disk::small` are total; `wf` gives `true` only when
  the names strictly increase (`Sorted`), `small` only within `2^16` files, and `2^16` changes
  and barriers per file.
- `find_spec`, `slot_spec`: `Disk::find` is the first file of the name, or `none` when no file
  has it; `Disk::slot` the first whose name is not below it.
- `open_spec`, `open_files`: on sorted names, `Disk::open` finds the file of the name, or puts an
  empty one in its place.
- `change_spec`, `barrier_spec`, `append_spec`: what each does to the files and the epoch.

Fuel: each specification states the bound it needs; every one is below `2^67`.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.World.DiskMap

open blaze_core.world.disk
open BlazeCore.World.DiskAllows (add_one usize_zero)

/-- The barrier loop of `File::wf` from index `i`, with more fuel than barriers left, neither
panics nor runs out of fuel. -/
theorem file_wf_loop1 (f : File) (e : U64) :
    ∀ (fuel : Nat) (i : Usize), f.barriers.val.length - i.val.toNat < fuel →
      ∃ b, File.wf.loop_1 fuel f e i = Result.ok b := by
  intro fuel
  induction fuel with
  | zero => intro i h; omega
  | succ n ih =>
    intro i hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.barriers.hlen
    by_cases hi : i.val.toNat < f.barriers.val.length
    · have hiv : i.val < (f.barriers.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      obtain ⟨b, hb⟩ := ih j (by omega)
      simp only [File.wf.loop_1, Slice.index, Scalar.lt_eq, Slice.len_val, hiv, hi,
        ↓reduceIte, ↓reduceDIte, Result.bind_ok, hj, hb]
      (repeat' split) <;> exact ⟨_, rfl⟩
    · have hiv : ¬ i.val < (f.barriers.val.length : Int) := by omega
      exact ⟨true, by simp [File.wf.loop_1, hiv]⟩

/-- The change loop of `File::wf` from index `i`, with more fuel than the changes left and the
barriers together, neither panics nor runs out of fuel. -/
theorem file_wf_loop0 (f : File) (e : U64) :
    ∀ (fuel : Nat) (i : Usize),
      (f.changes.val.length - i.val.toNat) + f.barriers.val.length + 1 < fuel →
      ∃ b, File.wf.loop_0 fuel f e i = Result.ok b := by
  intro fuel
  induction fuel with
  | zero => intro i h; omega
  | succ n ih =>
    intro i hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.changes.hlen
    by_cases hi : i.val.toNat < f.changes.val.length
    · have hiv : i.val < (f.changes.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      obtain ⟨b, hb⟩ := ih j (by omega)
      simp only [File.wf.loop_0, Slice.index, Scalar.lt_eq, Slice.len_val, hiv, hi,
        ↓reduceIte, ↓reduceDIte, Result.bind_ok, hj, hb]
      (repeat' split) <;> exact ⟨_, rfl⟩
    · have hiv : ¬ i.val < (f.changes.val.length : Int) := by omega
      obtain ⟨b, hb⟩ := file_wf_loop1 f e n 0 (by rw [usize_zero]; omega)
      exact ⟨b, by simp only [File.wf.loop_0, Scalar.lt_eq, Slice.len_val, hiv, ↓reduceIte]; exact hb⟩

/-- `File::wf` neither panics nor runs out of fuel when the fuel is more than the file's
changes and barriers together. -/
theorem file_wf_total (fuel : Nat) (f : File) (e : U64)
    (h : f.changes.val.length + f.barriers.val.length + 1 < fuel) :
    ∃ b, File.wf fuel f e = Result.ok b := by
  obtain ⟨b, hb⟩ := file_wf_loop0 f e fuel 0 (by rw [usize_zero]; omega)
  exact ⟨b, by simp only [File.wf]; exact hb⟩

/-- The loop of `File::small` from index `i`, with more fuel than changes left, neither panics
nor runs out of fuel. -/
theorem file_small_loop (f : File) :
    ∀ (fuel : Nat) (i : Usize), f.changes.val.length - i.val.toNat < fuel →
      ∃ b, File.small.loop_0 fuel f i = Result.ok b := by
  intro fuel
  induction fuel with
  | zero => intro i h; omega
  | succ n ih =>
    intro i hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.changes.hlen
    by_cases hi : i.val.toNat < f.changes.val.length
    · have hiv : i.val < (f.changes.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      obtain ⟨b, hb⟩ := ih j (by omega)
      simp only [File.small.loop_0, Slice.index, Scalar.lt_eq, Slice.len_val, hiv, hi,
        ↓reduceIte, ↓reduceDIte, Result.bind_ok, hj, hb]
      (repeat' split) <;> exact ⟨_, rfl⟩
    · have hiv : ¬ i.val < (f.changes.val.length : Int) := by omega
      exact ⟨true, by simp [File.small.loop_0, hiv]⟩

/-- `File::small` neither panics nor runs out of fuel when the fuel is more than its changes. -/
theorem file_small_total (fuel : Nat) (f : File) (h : f.changes.val.length < fuel) :
    ∃ b, File.small fuel f = Result.ok b := by
  obtain ⟨b, hb⟩ := file_small_loop f fuel 0 (by rw [usize_zero]; omega)
  simp only [File.small]
  (repeat' split) <;> first | exact ⟨_, rfl⟩ | exact ⟨b, hb⟩

/-- A file `File::small` accepts has at most `2^16` changes and at most `2^16` barriers. -/
theorem file_small_bounds (fuel : Nat) (f : File) (H : File.small fuel f = Result.ok true) :
    f.changes.val.length ≤ 65536 ∧ f.barriers.val.length ≤ 65536 := by
  have h1 : (65536 : Usize).val = 65536 :=
    Scalar.ofNat_val .Usize 65536 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  have h40 : (1099511627776 : Usize).val = 1099511627776 :=
    Scalar.ofNat_val .Usize 1099511627776 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  refine Classical.byContradiction fun hc => ?_
  have hf : File.small fuel f = Result.ok false := by
    simp only [File.small, Scalar.gt, h1, h40, Slice.len_val]
    repeat' split
    all_goals first | rfl | (exfalso; simp_all; omega)
  rw [hf] at H
  cases H

/-- `i - 1` does not overflow above zero, and is `i`'s value less one. -/
theorem sub_one (i : Usize) (h : 0 < i.val) :
    ∃ j : Usize, Scalar.sub i (1 : Usize) = Result.ok j ∧ j.val = i.val - 1 := by
  have h1 : (1 : Usize).val = 1 :=
    Scalar.ofNat_val .Usize 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  have h0 := i.hmax
  simp only [ScalarTy.max] at h0
  refine ⟨⟨i.val - 1, by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩,
    ?_, rfl⟩
  unfold Scalar.sub
  rw [h1]
  exact Scalar.checked_ok .Usize _
    ⟨by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩

/-- The files' names strictly increase along the list: each name is on one file. -/
def Sorted (L : List File) : Prop :=
  ∀ i j (hi : i < L.length) (hj : j < L.length), i < j → (L[i]).name.val < (L[j]).name.val

/-- Names that increase from each file to the next increase along the whole list. -/
theorem sorted_of_adj (L : List File)
    (h : ∀ k (hk : k + 1 < L.length), (L[k]).name.val < (L[k + 1]).name.val) : Sorted L := by
  intro i j hi hj hij
  induction j with
  | zero => omega
  | succ m ihm =>
    have h1 := h m hj
    by_cases e : i = m
    · subst e
      exact h1
    · have := ihm (by omega) (by omega)
      omega

/-- On sorted names, a file that has a name is the only one that has it. -/
theorem Sorted.unique {L : List File} (hs : Sorted L) (i j : Nat) (hi : i < L.length)
    (hj : j < L.length) (h : (L[i]).name.val = (L[j]).name.val) : i = j := by
  refine Classical.byContradiction fun hne => ?_
  rcases Nat.lt_or_gt_of_ne hne with hlt | hlt
  · have := hs i j hi hj hlt; omega
  · have := hs j i hj hi hlt; omega

/-- The loop of `Disk::wf` from index `i`, with more fuel than `2^65` plus the turns left: it
never panics, and gives `true` only when each name from file `i` on is above the one before. -/
theorem wf_loop (d : Disk) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ d.files.val.length →
      (d.files.val.length - i.val.toNat) + 36893488147419103232 < fuel →
      ∃ b, Disk.wf.loop_0 fuel d i = Result.ok b ∧
        (b = true → ∀ k (hk : k + 1 < d.files.val.length), i.val.toNat ≤ k + 1 →
          (d.files.val[k]).name.val < (d.files.val[k + 1]).name.val) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := d.files.hlen
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hiv : i.val < (d.files.val.length : Int) := by omega
      have hc := (d.files.val[i.val.toNat]).changes.hlen
      have hb := (d.files.val[i.val.toNat]).barriers.hlen
      obtain ⟨bw, hbw⟩ := file_wf_total n d.files.val[i.val.toNat] d.epoch (by omega)
      cases bw with
      | false =>
        refine ⟨false, ?_, by simp⟩
        simp [Disk.wf.loop_0, Slice.index, hiv, hlt, hbw]
      | true =>
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb', himp⟩ := ih j (by omega) (by omega)
        by_cases i0 : i.val = 0
        · refine ⟨b, ?_, ?_⟩
          · have hgt : ¬ (0 : Int) < i.val := by omega
            simp [Disk.wf.loop_0, Slice.index, hiv, hlt, hbw, hgt, usize_zero, hj, hb']
          · intro hbt k hk hik
            exact himp hbt k hk (by omega)
        · obtain ⟨m, hm, hmv⟩ := sub_one i (by omega)
          have hml : m.val.toNat < d.files.val.length := by omega
          by_cases hge : (d.files.val[i.val.toNat]).name.val ≤ (d.files.val[m.val.toNat]).name.val
          · refine ⟨false, ?_, by simp⟩
            have i0' : 0 < i.val := by omega
            simp [Disk.wf.loop_0, Slice.index, hiv, hlt, hbw, i0', usize_zero, hm, hml, hge]
          · refine ⟨b, ?_, ?_⟩
            · have i0' : 0 < i.val := by omega
              simp [Disk.wf.loop_0, Slice.index, hiv, hlt, hbw, i0', usize_zero, hm, hml, hge,
                hj, hb']
            · intro hbt k hk hik
              by_cases e : k + 1 = i.val.toNat
              · have e1 : m.val.toNat = k := by omega
                have e2 : i.val.toNat = k + 1 := by omega
                simp only [e1, e2] at hge
                omega
              · exact himp hbt k hk (by omega)
    · have hiv : ¬ i.val < (d.files.val.length : Int) := by omega
      refine ⟨true, by simp [Disk.wf.loop_0, hiv], fun _ k hk hik => ?_⟩
      omega

/-- `Disk::wf`, with at least `2^66` fuel: it never panics nor runs out of fuel, and gives
`true` only for a disk whose files' names strictly increase. -/
theorem wf_spec (fuel : Nat) (d : Disk) (h : 73786976294838206464 ≤ fuel) :
    ∃ b, Disk.wf fuel d = Result.ok b ∧ (b = true → Sorted d.files.val) := by
  have hl := d.files.hlen
  obtain ⟨b, hb, himp⟩ := wf_loop d fuel 0 (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
  exact ⟨b, by simp only [Disk.wf]; exact hb,
    fun hbt => sorted_of_adj _ fun k hk => himp hbt k hk (by rw [usize_zero]; omega)⟩

/-- The loop of `Disk::small` from index `i`, with more fuel than `2^64` plus the turns left: it
never panics, and gives `true` only when every file from `i` on has at most `2^16` changes and
at most `2^16` barriers. -/
theorem small_loop (d : Disk) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ d.files.val.length →
      (d.files.val.length - i.val.toNat) + 18446744073709551616 < fuel →
      ∃ b, Disk.small.loop_0 fuel d i = Result.ok b ∧
        (b = true → ∀ k (hk : k < d.files.val.length), i.val.toNat ≤ k →
          (d.files.val[k]).changes.val.length ≤ 65536 ∧
            (d.files.val[k]).barriers.val.length ≤ 65536) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := d.files.hlen
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hiv : i.val < (d.files.val.length : Int) := by omega
      have hc := (d.files.val[i.val.toNat]).changes.hlen
      obtain ⟨bs, hbs⟩ := file_small_total n d.files.val[i.val.toNat] (by omega)
      cases bs with
      | false =>
        refine ⟨false, ?_, by simp⟩
        simp [Disk.small.loop_0, Slice.index, hiv, hlt, hbs]
      | true =>
        have hbd := file_small_bounds n _ hbs
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb', himp⟩ := ih j (by omega) (by omega)
        refine ⟨b, by simp [Disk.small.loop_0, Slice.index, hiv, hlt, hbs, hj, hb'], ?_⟩
        intro hbt k hk hik
        by_cases e : k = i.val.toNat
        · subst e
          exact hbd
        · exact himp hbt k hk (by omega)
    · have hiv : ¬ i.val < (d.files.val.length : Int) := by omega
      refine ⟨true, by simp [Disk.small.loop_0, hiv], fun _ k hk hik => ?_⟩
      omega

/-- `Disk::small`, with at least `2^65` fuel: it never panics nor runs out of fuel, and gives
`true` only for a disk of at most `2^16` files, each with at most `2^16` changes and at most
`2^16` barriers. -/
theorem small_spec (fuel : Nat) (d : Disk) (h : 36893488147419103232 ≤ fuel) :
    ∃ b, Disk.small fuel d = Result.ok b ∧
      (b = true → d.files.val.length ≤ 65536 ∧ ∀ k (hk : k < d.files.val.length),
        (d.files.val[k]).changes.val.length ≤ 65536 ∧
          (d.files.val[k]).barriers.val.length ≤ 65536) := by
  have h1 : (65536 : Usize).val = 65536 :=
    Scalar.ofNat_val .Usize 65536 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  by_cases hl : d.files.val.length ≤ 65536
  · have hlv : ¬ (65536 : Int) < (d.files.val.length : Int) := by omega
    obtain ⟨b, hb, himp⟩ := small_loop d fuel 0 (by rw [usize_zero]; omega)
      (by rw [usize_zero]; omega)
    refine ⟨b, by simp [Disk.small, h1, hlv, hb], fun hbt => ⟨hl, fun k hk => ?_⟩⟩
    exact himp hbt k hk (by rw [usize_zero]; omega)
  · have hlv : (65536 : Int) < (d.files.val.length : Int) := by omega
    exact ⟨false, by simp [Disk.small, h1, hlv], by simp⟩

/-- The loop of `Disk::find` from index `i`, past files none of which has the name, with more
fuel than turns left: `none` when no file has the name, and `some j` only for the first file
that has it. -/
theorem find_loop (d : Disk) (name : U64) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ d.files.val.length →
      d.files.val.length - i.val.toNat < fuel →
      (∀ k (hk : k < d.files.val.length), k < i.val.toNat → (d.files.val[k]).name.val ≠ name.val) →
      ∃ r, Disk.find.loop_0 fuel d name i = Result.ok r ∧
        (r = none → ∀ k (hk : k < d.files.val.length), (d.files.val[k]).name.val ≠ name.val) ∧
        (∀ j, r = some j → ∃ hj : j.val.toNat < d.files.val.length,
          (d.files.val[j.val.toNat]).name.val = name.val ∧
          ∀ k (hk : k < d.files.val.length), k < j.val.toNat →
            (d.files.val[k]).name.val ≠ name.val) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel hbefore
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := d.files.hlen
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hiv : i.val < (d.files.val.length : Int) := by omega
      by_cases he : (d.files.val[i.val.toNat]).name.val = name.val
      · refine ⟨some i, by simp [Disk.find.loop_0, Slice.index, hiv, hlt, he], by simp, ?_⟩
        intro j hj
        simp only [Option.some.injEq] at hj
        subst hj
        exact ⟨hlt, he, hbefore⟩
      · obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨r, hr, h1, h2⟩ := ih j (by omega) (by omega) (fun k hk hkj => by
          by_cases e : k = i.val.toNat
          · subst e; exact he
          · exact hbefore k hk (by omega))
        exact ⟨r, by simp [Disk.find.loop_0, Slice.index, hiv, hlt, he, hj, hr], h1, h2⟩
    · have hiv : ¬ i.val < (d.files.val.length : Int) := by omega
      refine ⟨none, by simp [Disk.find.loop_0, hiv], fun _ k hk => hbefore k hk (by omega), ?_⟩
      intro j hj
      cases hj

/-- `Disk::find`, with more fuel than files: it never panics nor runs out of fuel; `none` when
no file has the name, and `some j` only when file `j` is the first that has it. -/
theorem find_spec (fuel : Nat) (d : Disk) (name : U64) (h : d.files.val.length < fuel) :
    ∃ r, Disk.find fuel d name = Result.ok r ∧
      (r = none → ∀ k (hk : k < d.files.val.length), (d.files.val[k]).name.val ≠ name.val) ∧
      (∀ j, r = some j → ∃ hj : j.val.toNat < d.files.val.length,
        (d.files.val[j.val.toNat]).name.val = name.val ∧
        ∀ k (hk : k < d.files.val.length), k < j.val.toNat →
          (d.files.val[k]).name.val ≠ name.val) := by
  obtain ⟨r, hr, h1, h2⟩ := find_loop d name fuel 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; omega) (fun k _ hk => by rw [usize_zero] at hk; omega)
  exact ⟨r, by simp only [Disk.find]; exact hr, h1, h2⟩

/-- The loop of `Disk::slot` from index `i`, past files whose names are all below `name`,
with more fuel than turns left: the first position from `i` whose file's name is not below
`name`, or the end. -/
theorem slot_loop (d : Disk) (name : U64) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ d.files.val.length →
      d.files.val.length - i.val.toNat < fuel →
      (∀ k (hk : k < d.files.val.length), k < i.val.toNat → (d.files.val[k]).name.val < name.val) →
      ∃ s : Usize, Disk.slot.loop_0 fuel d name i = Result.ok s ∧
        s.val.toNat ≤ d.files.val.length ∧
        (∀ k (hk : k < d.files.val.length), k < s.val.toNat →
          (d.files.val[k]).name.val < name.val) ∧
        (∀ hs : s.val.toNat < d.files.val.length,
          name.val ≤ (d.files.val[s.val.toNat]).name.val) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel hbefore
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := d.files.hlen
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hiv : i.val < (d.files.val.length : Int) := by omega
      by_cases hc : (d.files.val[i.val.toNat]).name.val < name.val
      · obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨s, hs, h1, h2, h3⟩ := ih j (by omega) (by omega) (fun k hk hkj => by
          by_cases e : k = i.val.toNat
          · subst e; exact hc
          · exact hbefore k hk (by omega))
        exact ⟨s, by simp [Disk.slot.loop_0, Slice.index, hiv, hlt, hc, hj, hs], h1, h2, h3⟩
      · exact ⟨i, by simp [Disk.slot.loop_0, Slice.index, hiv, hlt, hc], hi, hbefore,
          fun _ => by omega⟩
    · have hiv : ¬ i.val < (d.files.val.length : Int) := by omega
      exact ⟨i, by simp [Disk.slot.loop_0, hiv], hi, hbefore, fun hs => absurd hs hlt⟩

/-- `Disk::slot`, with more fuel than files: it never panics nor runs out of fuel, and gives
the first position whose file's name is not below `name`, or the end. -/
theorem slot_spec (fuel : Nat) (d : Disk) (name : U64) (h : d.files.val.length < fuel) :
    ∃ s : Usize, Disk.slot fuel d name = Result.ok s ∧
      s.val.toNat ≤ d.files.val.length ∧
      (∀ k (hk : k < d.files.val.length), k < s.val.toNat →
        (d.files.val[k]).name.val < name.val) ∧
      (∀ hs : s.val.toNat < d.files.val.length,
        name.val ≤ (d.files.val[s.val.toNat]).name.val) := by
  obtain ⟨s, hs, h1, h2, h3⟩ := slot_loop d name fuel 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; omega) (fun k _ hk => by rw [usize_zero] at hk; omega)
  exact ⟨s, by simp only [Disk.slot]; exact hs, h1, h2, h3⟩

/-- `Disk::open` on a disk whose names strictly increase, with fewer than `usize::MAX` files
and more fuel than files: it never panics nor runs out of fuel, keeps the epoch, and gives a
position `a` whose file has the name. When a file had the name, the disk is unchanged; when
none had it, the files are the old ones with an empty file of that name put in at `a`. -/
theorem open_spec (fuel : Nat) (d : Disk) (name : U64) (hs : Sorted d.files.val)
    (hl : d.files.val.length < 18446744073709551615) (h : d.files.val.length < fuel) :
    ∃ (a : Usize) (d' : Disk), Disk.«open» fuel d name = Result.ok (a, d') ∧
      a.val.toNat ≤ d.files.val.length ∧ d'.epoch = d.epoch ∧
      (∃ ha : a.val.toNat < d'.files.val.length,
        (d'.files.val[a.val.toNat]).name.val = name.val) ∧
      ((∃ p, ∃ hp : p < d.files.val.length, (d.files.val[p]).name.val = name.val) → d' = d) ∧
      ((∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val ≠ name.val) →
        d'.files.val = d.files.val.insertIdx a.val.toNat (File.whole name Vec.new)) := by
  obtain ⟨s, hsq, hs1, h2, h3⟩ := slot_spec fuel d name h
  have hs0 := s.hmin
  simp only [ScalarTy.min] at hs0
  by_cases hp : ∃ hsl : s.val.toNat < d.files.val.length,
      (d.files.val[s.val.toNat]).name.val = name.val
  · obtain ⟨hsl, he⟩ := hp
    have hsv : ¬ s.val = (d.files.val.length : Int) := by omega
    refine ⟨s, d, ?_, hs1, rfl, ⟨hsl, he⟩, fun _ => rfl, fun H => absurd he (H _ hsl)⟩
    simp [Disk.«open», hsq, hsv, Slice.index, hsl, he]
  · have habs : ∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val ≠ name.val := by
      intro p hpl hpe
      by_cases hps : p < s.val.toNat
      · have := h2 p hpl hps
        omega
      · have hsl : s.val.toNat < d.files.val.length := by omega
        have hle := h3 hsl
        have hne : (d.files.val[s.val.toNat]).name.val ≠ name.val := fun e => hp ⟨hsl, e⟩
        by_cases e : p = s.val.toNat
        · subst e
          exact hne hpe
        · have := hs s.val.toNat p hsl hpl (by omega)
          omega
    have hins : s.val.toNat ≤ d.files.val.length ∧ d.files.val.length < 18446744073709551615 :=
      ⟨hs1, hl⟩
    have hlen := List.length_insertIdx_of_le_length hs1 (File.whole name Vec.new)
    refine ⟨s, ⟨⟨d.files.val.insertIdx s.val.toNat (File.whole name Vec.new), by omega⟩, d.epoch⟩,
      ?_, hs1, rfl, ⟨by simp only [hlen]; omega, ?_⟩,
      fun ⟨p, hpl, hpe⟩ => absurd hpe (habs p hpl), fun _ => rfl⟩
    · by_cases hsl : s.val.toNat < d.files.val.length
      · have hsv : ¬ s.val = (d.files.val.length : Int) := by omega
        have hne : ¬ (d.files.val[s.val.toNat]).name.val = name.val := habs _ hsl
        have hsle : s.val ≤ (d.files.val.length : Int) := by omega
        simp [Disk.«open», hsq, hsv, Slice.index, hsl, hne, Vec.insert, hins, hsle]
      · have hsv : s.val = (d.files.val.length : Int) := by omega
        simp [Disk.«open», hsq, hsv, Vec.insert, hins]
    · simp [List.getElem_insertIdx_self, File.whole]

/-- `Disk::change` at a file in bounds with fewer than `usize::MAX` changes: it never panics,
keeps the epoch and the number of files, leaves every other file as it was, and adds the
change, made in the epoch that is now, after the changes of the file at `a`, whose name and
barriers stay. -/
theorem change_spec (d : Disk) (a : Usize) (c : Change) (ha : a.val.toNat < d.files.val.length)
    (hc : (d.files.val[a.val.toNat]).changes.val.length < 18446744073709551615) :
    ∃ d', Disk.change d a c = Result.ok d' ∧ d'.epoch = d.epoch ∧
      ∃ hlen : d'.files.val.length = d.files.val.length,
      ∀ k (hk : k < d.files.val.length),
        (k ≠ a.val.toNat → d'.files.val[k] = d.files.val[k]) ∧
        (k = a.val.toNat → (d'.files.val[k]).name = (d.files.val[k]).name ∧
          (d'.files.val[k]).barriers = (d.files.val[k]).barriers ∧
          (d'.files.val[k]).changes.val =
            (d.files.val[k]).changes.val ++ [⟨c, d.epoch⟩]) := by
  have hl := d.files.hlen
  refine ⟨⟨⟨d.files.val.set a.val.toNat { d.files.val[a.val.toNat] with
      changes := ⟨(d.files.val[a.val.toNat]).changes.val ++ [⟨c, d.epoch⟩],
        by simp only [List.length_append, List.length_singleton]; omega⟩ },
      by simp only [List.length_set]; omega⟩, d.epoch⟩, ?_, rfl, by simp, ?_⟩
  · simp [Disk.change, Slice.index, ha, Vec.push, hc, Slice.update]
  · intro k hk
    constructor
    · intro hne
      simp [List.getElem_set_ne (Ne.symm hne)]
    · intro e
      subst e
      simp [List.getElem_set_self]

/-- `epoch.saturating_add(1)` below `u64::MAX` is one more. -/
theorem saturating_succ (e : U64) (he : e.val < 18446744073709551615) :
    (Scalar.saturating_add e (1 : U64)).val = e.val + 1 := by
  have h1 : (1 : U64).val = 1 :=
    Scalar.ofNat_val .U64 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  have h0 := e.hmin
  simp only [ScalarTy.min] at h0
  rw [Scalar.saturating_add, Scalar.clamp_val, h1]
  have a1 : ¬ e.val + 1 < ScalarTy.U64.min := by simp only [ScalarTy.min]; omega
  have a2 : ¬ ScalarTy.U64.max < e.val + 1 := by simp only [ScalarTy.max]; omega
  simp only [a1, a2, ↓reduceIte]

/-- `Disk::barrier` of a file the disk has, at position `p`, that covers all its changes, on a
disk whose names strictly increase, below `usize::MAX` files and barriers of that file, below
the last epoch and with more fuel than files: it never panics nor runs out of fuel, the epoch
becomes one more, every file keeps its name and changes, and when the file has a change, a
barrier covering them all, of the epoch before, is among its barriers. -/
theorem barrier_spec (fuel : Nat) (d : Disk) (name : U64) (covers : Usize) (p : Nat)
    (hs : Sorted d.files.val) (hl : d.files.val.length < 18446744073709551615)
    (hf : d.files.val.length < fuel) (hp : p < d.files.val.length)
    (hpn : (d.files.val[p]).name.val = name.val)
    (hcov : covers.val = ((d.files.val[p]).changes.val.length : Int))
    (hb : (d.files.val[p]).barriers.val.length < 18446744073709551615)
    (he : d.epoch.val < 18446744073709551615) :
    ∃ d', Disk.barrier fuel d name covers = Result.ok d' ∧ d'.epoch.val = d.epoch.val + 1 ∧
      ∃ hlen : d'.files.val.length = d.files.val.length,
      ∀ k (hk : k < d.files.val.length),
        (d'.files.val[k]).name = (d.files.val[k]).name ∧
        (d'.files.val[k]).changes = (d.files.val[k]).changes ∧
        (k = p → 0 < covers.val →
          ∃ b ∈ (d'.files.val[k]).barriers.val, b.covers = covers ∧ b.epoch = d.epoch) := by
  obtain ⟨a, d1, hop, hale, hep, ⟨hat, han⟩, hpres, _⟩ := open_spec fuel d name hs hl hf
  have hd1 := hpres ⟨p, hp, hpn⟩
  subst d1
  have hap : a.val.toNat = p := hs.unique _ _ hat hp (han.trans hpn.symm)
  subst hap
  have hsat := saturating_succ d.epoch he
  have hlt : ¬ covers.val < ((d.files.val[a.val.toNat]).changes.val.length : Int) := by omega
  have hc0 := covers.hmin
  simp only [ScalarTy.min] at hc0
  by_cases hb0 : 0 < covers.val
  · have hpos : (0 : Int) < ((d.files.val[a.val.toNat]).changes.val.length : Int) := by omega
    refine ⟨⟨⟨d.files.val.set a.val.toNat { d.files.val[a.val.toNat] with
        barriers := ⟨(d.files.val[a.val.toNat]).barriers.val ++
          [⟨Slice.len (d.files.val[a.val.toNat]).changes, d.epoch⟩],
          by simp only [List.length_append, List.length_singleton]; omega⟩ },
        by simp only [List.length_set]; exact d.files.hlen⟩, Scalar.saturating_add d.epoch 1⟩,
      ?_, hsat, by simp, ?_⟩
    · have hposN : 0 < (d.files.val[a.val.toNat]).changes.val.length := by omega
      simp [Disk.barrier, hop, Slice.index, hat, hlt, hposN, usize_zero, Vec.push, hb,
        Slice.update]
    · intro k hk
      by_cases e : a.val.toNat = k
      · subst e
        refine ⟨by simp, by simp, fun _ _ => ⟨⟨Slice.len (d.files.val[a.val.toNat]).changes,
          d.epoch⟩, by simp, Scalar.ext (by simp only [Slice.len_val]; omega), rfl⟩⟩
      · refine ⟨by simp [List.getElem_set_ne e], by simp [List.getElem_set_ne e],
          fun e' => absurd e'.symm e⟩
  · have hpos : ¬ (0 : Int) < ((d.files.val[a.val.toNat]).changes.val.length : Int) := by omega
    refine ⟨⟨d.files, Scalar.saturating_add d.epoch 1⟩, ?_, hsat, rfl, ?_⟩
    · have hz : (d.files.val[a.val.toNat]).changes.val.length = 0 := by omega
      have hnil : (d.files.val[a.val.toNat]).changes.val = [] := List.eq_nil_of_length_eq_zero hz
      have hcov0 : covers.val = 0 := by omega
      simp [Disk.barrier, hop, Slice.index, hat, hnil, hcov0, usize_zero]
    · intro k hk
      exact ⟨rfl, rfl, fun _ h => absurd h hb0⟩

/-- What `Disk::open` leaves, by name, on a disk whose names strictly increase: the file at `a`
has the name, fewer than `usize::MAX` changes and the barriers of the old file of that name
(if there was one); every other file is an old one, of another name; and every old file of
another name is still there, at a position other than `a`. -/
theorem open_files (fuel : Nat) (d : Disk) (name : U64) (hs : Sorted d.files.val)
    (hl : d.files.val.length < 18446744073709551615) (hf : d.files.val.length < fuel)
    (hc : ∀ k (hk : k < d.files.val.length),
      (d.files.val[k]).changes.val.length < 18446744073709551615) :
    ∃ (a : Usize) (d1 : Disk), Disk.«open» fuel d name = Result.ok (a, d1) ∧
      d1.epoch = d.epoch ∧ ∃ ha : a.val.toNat < d1.files.val.length,
      (d1.files.val[a.val.toNat]).name.val = name.val ∧
      (d1.files.val[a.val.toNat]).changes.val.length < 18446744073709551615 ∧
      (∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val = name.val →
        (d1.files.val[a.val.toNat]).barriers = (d.files.val[p]).barriers) ∧
      (∀ k (hk : k < d1.files.val.length), k ≠ a.val.toNat →
        (d1.files.val[k]).name.val ≠ name.val ∧
          ∃ p, ∃ hp : p < d.files.val.length, d1.files.val[k] = d.files.val[p]) ∧
      (∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val ≠ name.val →
        ∃ k, ∃ hk : k < d1.files.val.length, k ≠ a.val.toNat ∧
          d1.files.val[k] = d.files.val[p]) := by
  obtain ⟨a, d1, hop, hale, hep, ⟨hat, han⟩, hpres, hins⟩ := open_spec fuel d name hs hl hf
  refine ⟨a, d1, hop, hep, hat, han, ?_⟩
  by_cases H : ∃ p, ∃ hp : p < d.files.val.length, (d.files.val[p]).name.val = name.val
  · have hd1 := hpres H
    subst d1
    refine ⟨hc _ hat, ?_, ?_, ?_⟩
    · intro p hp hpn
      have e : a.val.toNat = p := hs.unique _ _ hat hp (han.trans hpn.symm)
      subst e
      rfl
    · intro k hk hka
      refine ⟨fun hkn => hka (hs.unique _ _ hk hat (hkn.trans han.symm)), k, hk, rfl⟩
    · intro p hp hpn
      refine ⟨p, hp, fun e => ?_, rfl⟩
      subst e
      exact hpn han
  · have habs : ∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val ≠ name.val :=
      fun p hp e => H ⟨p, hp, e⟩
    have e := hins habs
    have hlen1 : d1.files.val.length = d.files.val.length + 1 := by
      rw [e, List.length_insertIdx_of_le_length hale]
    have hget : ∀ k (hk : k < d1.files.val.length), d1.files.val[k] =
        (d.files.val.insertIdx a.val.toNat (File.whole name Vec.new))[k]'(by rw [← e]; exact hk) :=
      fun k hk => by simp only [e]
    refine ⟨?_, fun p hp hpn => absurd hpn (habs p hp), ?_, ?_⟩
    · rw [hget _ hat, List.getElem_insertIdx_self]
      simp [File.whole, Vec.new]
    · intro k hk hka
      by_cases hlt : k < a.val.toNat
      · rw [hget _ hk, List.getElem_insertIdx_of_lt hlt]
        exact ⟨habs k (by omega), k, by omega, rfl⟩
      · rw [hget _ hk, List.getElem_insertIdx_of_gt (by omega)]
        exact ⟨habs (k - 1) (by omega), k - 1, by omega, rfl⟩
    · intro p hp _
      by_cases hlt : p < a.val.toNat
      · refine ⟨p, by omega, by omega, ?_⟩
        rw [hget _ (by omega), List.getElem_insertIdx_of_lt hlt]
      · refine ⟨p + 1, by omega, by omega, ?_⟩
        rw [hget _ (by omega), List.getElem_insertIdx_of_gt (by omega)]
        rfl

/-- `Disk::append` on a disk whose names strictly increase, below `usize::MAX` files and
changes of each file, with more fuel than files: it never panics nor runs out of fuel, keeps the
epoch, and leaves a file of the name; each file of the name ends with the append, made in the
epoch that is now, and has the barriers of the old file of the name (if there was one); every
file of another name is an old one, and every old file of another name is still there. -/
theorem append_spec (fuel : Nat) (d : Disk) (name : U64) (bytes : Vec U8)
    (hs : Sorted d.files.val) (hl : d.files.val.length < 18446744073709551615)
    (hf : d.files.val.length < fuel)
    (hc : ∀ k (hk : k < d.files.val.length),
      (d.files.val[k]).changes.val.length < 18446744073709551615) :
    ∃ d', Disk.append fuel d name bytes = Result.ok d' ∧ d'.epoch = d.epoch ∧
      (∃ q, ∃ hq : q < d'.files.val.length, (d'.files.val[q]).name.val = name.val) ∧
      (∀ k (hk : k < d'.files.val.length), (d'.files.val[k]).name.val = name.val →
        ∃ xs, (d'.files.val[k]).changes.val = xs ++ [⟨Change.Append bytes, d.epoch⟩]) ∧
      (∀ k (hk : k < d'.files.val.length), (d'.files.val[k]).name.val = name.val →
        ∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val = name.val →
          (d'.files.val[k]).barriers = (d.files.val[p]).barriers) ∧
      (∀ k (hk : k < d'.files.val.length), (d'.files.val[k]).name.val ≠ name.val →
        ∃ p, ∃ hp : p < d.files.val.length, d'.files.val[k] = d.files.val[p]) ∧
      (∀ p (hp : p < d.files.val.length), (d.files.val[p]).name.val ≠ name.val →
        ∃ k, ∃ hk : k < d'.files.val.length, d'.files.val[k] = d.files.val[p]) := by
  obtain ⟨a, d1, hop, hep, hat, han, hca, hbar, hother, hback⟩ :=
    open_files fuel d name hs hl hf hc
  obtain ⟨d', hch, hep', hlen', hprop⟩ := change_spec d1 a (Change.Append bytes) hat hca
  have hname : ∀ k (hk : k < d'.files.val.length), (d'.files.val[k]).name.val = name.val →
      k = a.val.toNat := by
    intro k hk hkn
    refine Classical.byContradiction fun hka => ?_
    have h1 := (hprop k (by omega)).1 hka
    exact (hother k (by omega) hka).1 (h1 ▸ hkn)
  refine ⟨d', by simp [Disk.append, hop, hch], hep'.trans hep, ⟨a.val.toNat, by omega, ?_⟩,
    ?_, ?_, ?_, ?_⟩
  · rw [((hprop _ hat).2 rfl).1]
    exact han
  · intro k hk hkn
    have e := hname k hk hkn
    subst e
    refine ⟨(d1.files.val[a.val.toNat]).changes.val, ?_⟩
    rw [((hprop _ hat).2 rfl).2.2, hep]
  · intro k hk hkn p hp hpn
    have e := hname k hk hkn
    subst e
    rw [((hprop _ hat).2 rfl).2.1]
    exact hbar p hp hpn
  · intro k hk hkn
    have hka : k ≠ a.val.toNat := fun e => hkn (by subst e; rw [((hprop _ hat).2 rfl).1]; exact han)
    rw [(hprop k (by omega)).1 hka]
    exact (hother k (by omega) hka).2
  · intro p hp hpn
    obtain ⟨k, hk, hka, hkp⟩ := hback p hp hpn
    exact ⟨k, by omega, by rw [(hprop k hk).1 hka]; exact hkp⟩

end BlazeCore.World.DiskMap
