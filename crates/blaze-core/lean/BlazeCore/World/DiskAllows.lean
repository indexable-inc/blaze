import Blaze.Auto
import BlazeModel.blaze_core.world.disk.Disk.allows

/-!
`blaze_core::world::disk` (crates/blaze-core/src/world/disk.rs): what the disk world's
`Disk::allows` means, proved over its generated model as a small library of loop
specifications.

- `reach_spec`: `File::reach` is `reachOf`, how many changes a crash keeps whole or in part.
- `keeps_ordered_spec`: `File::keeps_ordered` says whether every barrier completed before
  `newest` covers no more than the changes kept whole.
- `newest_spec`: `File::newest` folds the newest epoch of the changes a crash keeps into
  `(none, newest)`.
- `disk_newest_spec`: `Disk::newest` is an upper bound of the epoch of every change the crash
  keeps, and `none` only when it keeps no change.
- `allows_orders`: a crash the disk allows that keeps, whole or in part, a change made after a
  barrier completed keeps whole everything that barrier covers.
- `allows_total`: with enough fuel `Disk::allows` neither panics nor runs out of fuel.

Fuel: every slice is at most `usize::MAX = 2^64 - 1` long, so `2^66` fuel is enough for
every loop `allows` runs, nested ones included; the theorems about `allows` take that bound.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.World.DiskAllows

open blaze_core.world.disk

/-- `i + 1` does not overflow below `usize::MAX`, and is `i`'s value plus one. -/
theorem add_one (i : Usize) (h : i.val < 18446744073709551615) :
    ∃ j : Usize, Scalar.add i (1 : Usize) = Result.ok j ∧ j.val = i.val + 1 := by
  have h1 : (1 : Usize).val = 1 :=
    Scalar.ofNat_val .Usize 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  have h0 := i.hmin
  simp only [ScalarTy.min] at h0
  refine ⟨⟨i.val + 1, by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩,
    ?_, rfl⟩
  unfold Scalar.add
  rw [h1]
  exact Scalar.checked_ok .Usize _
    ⟨by simp only [ScalarTy.min]; omega, by simp only [ScalarTy.max]; omega⟩

/-- The literal `0 : usize` is zero. -/
theorem usize_zero : (0 : Usize).val = 0 :=
  Scalar.ofNat_val .Usize 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- The literal `0 : u64` is zero. -/
theorem u64_zero : (0 : U64).val = 0 :=
  Scalar.ofNat_val .U64 0 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-! ## 1. `File::reach` -/

/-- How many of a file's changes a crash that keeps `k` leaves, whole or in part: the changes
kept whole (no more than the file has), and one more when a part of the next one landed and
there is a next one. -/
def reachOf (f : File) (k : Kept) : Nat :=
  match k.torn with
  | Torn.Nothing => min k.whole.val.toNat f.changes.val.length
  | _ =>
    if min k.whole.val.toNat f.changes.val.length < f.changes.val.length then
      min k.whole.val.toNat f.changes.val.length + 1
    else min k.whole.val.toNat f.changes.val.length

/-- A crash keeps no more changes than the file has. -/
theorem reachOf_le (f : File) (k : Kept) : reachOf f k ≤ f.changes.val.length := by
  unfold reachOf
  split <;> (try split) <;> omega

/-- `File::reach` never panics and is `reachOf`. -/
theorem reach_spec (f : File) (k : Kept) :
    ∃ r : Usize, File.reach f k = Result.ok r ∧ r.val.toNat = reachOf f k := by
  rcases k with ⟨w, torn, u⟩
  have hl := f.changes.hlen
  have hw := w.hmin
  simp only [ScalarTy.min] at hw
  by_cases h : w.val < (f.changes.val.length : Int)
  · cases torn with
    | Nothing => exact ⟨w, by simp [File.reach, h], by simp only [reachOf]; (try simp only [Slice.len_val]); (try split) <;> omega⟩
    | Prefix n =>
      obtain ⟨j, hj, hjv⟩ := add_one w (by omega)
      exact ⟨j, by simp [File.reach, h, hj], by simp only [reachOf]; (try simp only [Slice.len_val]); (try split) <;> omega⟩
    | Sectors v =>
      obtain ⟨j, hj, hjv⟩ := add_one w (by omega)
      exact ⟨j, by simp [File.reach, h, hj], by simp only [reachOf]; (try simp only [Slice.len_val]); (try split) <;> omega⟩
  · cases torn with
    | Nothing =>
      exact ⟨Slice.len f.changes, by simp [File.reach, h], by simp only [reachOf]; (try simp only [Slice.len_val]); (try split) <;> omega⟩
    | Prefix n =>
      exact ⟨Slice.len f.changes, by simp [File.reach, h], by simp only [reachOf]; (try simp only [Slice.len_val]); (try split) <;> omega⟩
    | Sectors v =>
      exact ⟨Slice.len f.changes, by simp [File.reach, h], by simp only [reachOf]; (try simp only [Slice.len_val]); (try split) <;> omega⟩

/-! ## 2. `File::keeps_ordered` -/

/-- The loop of `keeps_ordered` from index `i`, with more fuel than barriers left: it gives
`true` exactly when every barrier at a position `≥ i` that completed before `newest` covers no
more than `whole` changes. -/
theorem keeps_loop (f : File) (whole : Usize) (newest : U64) :
    ∀ (fuel : Nat) (i : Usize), f.barriers.val.length - i.val.toNat < fuel →
      ∃ b, File.keeps_ordered.loop_0 fuel f whole newest i = Result.ok b ∧
        (b = true ↔ ∀ k (hk : k < f.barriers.val.length), i.val.toNat ≤ k →
          (f.barriers.val[k]).epoch.val < newest.val →
            (f.barriers.val[k]).covers.val ≤ whole.val) := by
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
      by_cases hc : (f.barriers.val[i.val.toNat]).epoch.val < newest.val ∧
          whole.val < (f.barriers.val[i.val.toNat]).covers.val
      · refine ⟨false, ?_, ?_⟩
        · simp [File.keeps_ordered.loop_0, Slice.index, hi, hiv, hc.1, hc.2]
        · simp only [Bool.false_eq_true, false_iff]
          intro H
          exact absurd (H _ hi (Nat.le_refl _) hc.1) (Int.not_le.mpr hc.2)
      · have hgood : (f.barriers.val[i.val.toNat]).epoch.val < newest.val →
            (f.barriers.val[i.val.toNat]).covers.val ≤ whole.val :=
          fun he => Int.not_lt.mp (fun h2 => hc ⟨he, h2⟩)
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb, hbiff⟩ := ih j (by omega)
        refine ⟨b, ?_, ?_⟩
        · by_cases he : (f.barriers.val[i.val.toNat]).epoch.val < newest.val
          · have hn : ¬ whole.val < (f.barriers.val[i.val.toNat]).covers.val :=
              Int.not_lt.mpr (hgood he)
            simp [File.keeps_ordered.loop_0, Slice.index, hi, hiv, he, hn, hj, hb]
          · simp [File.keeps_ordered.loop_0, Slice.index, hi, hiv, he, hj, hb]
        · rw [hbiff]
          constructor
          · intro H k hk hik hek
            by_cases e : k = i.val.toNat
            · subst e
              exact hgood hek
            · exact H k hk (by omega) hek
          · intro H k hk hik
            exact H k hk (by omega)
    · have hiv : ¬ i.val < (f.barriers.val.length : Int) := by omega
      refine ⟨true, by simp [File.keeps_ordered.loop_0, hiv], ?_⟩
      simp only [true_iff]
      intro k hk hik
      omega

/-- `File::keeps_ordered`, with more fuel than the file has barriers: it gives `true` exactly
when every barrier that completed before the epoch `newest` covers no more than the `whole`
changes kept whole. -/
theorem keeps_ordered_spec (fuel : Nat) (f : File) (whole : Usize) (newest : U64)
    (h : f.barriers.val.length < fuel) :
    ∃ b, File.keeps_ordered fuel f whole newest = Result.ok b ∧
      (b = true ↔ ∀ x ∈ f.barriers.val, x.epoch.val < newest.val → x.covers.val ≤ whole.val) := by
  obtain ⟨b, hb, hiff⟩ := keeps_loop f whole newest fuel 0 (by rw [usize_zero]; omega)
  refine ⟨b, by simp only [File.keeps_ordered]; exact hb, ?_⟩
  rw [hiff, usize_zero]
  constructor
  · intro H x hx
    obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hx
    exact H k hk (by omega)
  · intro H k hk _
    exact H _ (List.getElem_mem hk)

/-! ## 3. `File::newest` -/

/-- The loop of `File::newest` from index `i` up to `reach`, with more fuel than turns left.
Given that `none` only before the first turn, and that otherwise `newest` bounds the epochs
of the changes before `i`: it gives `none'` only when nothing was seen and no turn is left;
otherwise a `newest'` that bounds the epochs of every change before `reach`; and it never
forgets a change already seen, nor lowers `newest`. -/
theorem newest_loop (f : File) (reach : Usize)
    (hr : reach.val ≤ (f.changes.val.length : Int)) :
    ∀ (fuel : Nat) (none : Bool) (newest : U64) (i : Usize),
      i.val ≤ reach.val → (reach.val - i.val).toNat < fuel →
      (none = true → i.val = 0) →
      (none = false → ∀ c (hc : c < f.changes.val.length), c < i.val.toNat →
        (f.changes.val[c]).epoch.val ≤ newest.val) →
      ∃ none' newest', File.newest.loop_0 fuel f reach none newest i = Result.ok (none', newest') ∧
        (none' = true ↔ none = true ∧ reach.val ≤ i.val) ∧
        (none' = false → ∀ c (hc : c < f.changes.val.length), c < reach.val.toNat →
          (f.changes.val[c]).epoch.val ≤ newest'.val) ∧
        (none = false → none' = false ∧ newest.val ≤ newest'.val) := by
  intro fuel
  induction fuel with
  | zero => intro none newest i _ h; omega
  | succ n ih =>
    intro none newest i hir hfuel hnone hseen
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    by_cases hi : i.val < reach.val
    · have hil : i.val.toNat < f.changes.val.length := by omega
      have hl := f.changes.hlen
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      -- One turn: the state after it, which has seen a change and bounds every one up to `i`.
      obtain ⟨w1, hstep, hmono, hcover⟩ : ∃ w1 : U64,
          File.newest.loop_0 (n + 1) f reach none newest i =
            File.newest.loop_0 n f reach false w1 j ∧
          (none = false → newest.val ≤ w1.val) ∧
          (∀ c (hc : c < f.changes.val.length), c < j.val.toNat →
            (f.changes.val[c]).epoch.val ≤ w1.val) := by
        cases none with
        | true =>
          refine ⟨(f.changes.val[i.val.toNat]).epoch, ?_, by simp, ?_⟩
          · simp [File.newest.loop_0, Slice.index, hi, hil, hj]
          · intro c hc hcj
            have e : c = i.val.toNat := by have := hnone rfl; omega
            subst e
            exact Int.le_refl _
        | false =>
          by_cases hgt : newest.val < (f.changes.val[i.val.toNat]).epoch.val
          · refine ⟨(f.changes.val[i.val.toNat]).epoch, ?_, fun _ => Int.le_of_lt hgt, ?_⟩
            · simp [File.newest.loop_0, Slice.index, hi, hil, hj, hgt]
            · intro c hc hcj
              by_cases e : c = i.val.toNat
              · subst e
                exact Int.le_refl _
              · have := hseen rfl c hc (by omega)
                omega
          · refine ⟨newest, ?_, fun _ => Int.le_refl _, ?_⟩
            · simp [File.newest.loop_0, Slice.index, hi, hil, hj, hgt]
            · intro c hc hcj
              by_cases e : c = i.val.toNat
              · subst e
                exact Int.not_lt.mp hgt
              · exact hseen rfl c hc (by omega)
      obtain ⟨n', w', h1, h2, h3, h4⟩ :=
        ih false w1 j (by omega) (by omega) (by simp) (fun _ => hcover)
      have hn' : n' = false := (h4 rfl).1
      refine ⟨n', w', hstep ▸ h1, ?_, h3, ?_⟩
      · subst hn'
        simp only [Bool.false_eq_true, false_iff, not_and]
        intro _
        omega
      · intro hn
        exact ⟨hn', Int.le_trans (hmono hn) (h4 rfl).2⟩
    · have hiv : i.val = reach.val := by omega
      refine ⟨none, newest, by simp [File.newest.loop_0, hi], ?_, ?_, ?_⟩
      · exact ⟨fun hn => ⟨hn, by omega⟩, fun hn => hn.1⟩
      · intro hn c hc hcr
        exact hseen hn c hc (by omega)
      · intro hn
        exact ⟨hn, Int.le_refl _⟩

/-- `File::newest`, with more fuel than the changes a crash that keeps `k` leaves: it gives
`none'` exactly when `none` and the crash keeps no change of the file; when not `none'`, a
`newest'` at least the epoch of every change kept whole or in part; and it never turns a seen
change back to `none` nor lowers `newest`. -/
theorem newest_spec (fuel : Nat) (f : File) (k : Kept) (none : Bool) (newest : U64)
    (h : reachOf f k < fuel) :
    ∃ none' newest', File.newest fuel f k none newest = Result.ok (none', newest') ∧
      (none' = true ↔ none = true ∧ reachOf f k = 0) ∧
      (none' = false → ∀ c (hc : c < f.changes.val.length), c < reachOf f k →
        (f.changes.val[c]).epoch.val ≤ newest'.val) ∧
      (none = false → none' = false ∧ newest.val ≤ newest'.val) := by
  obtain ⟨r, hr, hrv⟩ := reach_spec f k
  have hle := reachOf_le f k
  have hr0 := r.hmin
  simp only [ScalarTy.min] at hr0
  obtain ⟨n', w', h1, h2, h3, h4⟩ :=
    newest_loop f r (by omega) fuel none newest 0 (by rw [usize_zero]; omega)
      (by rw [usize_zero]; omega) (fun _ => usize_zero)
      (fun _ c _ hc => by rw [usize_zero] at hc; omega)
  refine ⟨n', w', ?_, ?_, ?_, h4⟩
  · simp only [File.newest, hr, Result.bind_ok]
    exact h1
  · rw [h2, usize_zero]
    constructor
    · intro ⟨a, b⟩; exact ⟨a, by omega⟩
    · intro ⟨a, b⟩; exact ⟨a, by omega⟩
  · intro hn c hc hcr
    exact h3 hn c hc (by omega)

/-! ## 4. `Disk::newest` -/

/-- After the files before position `i`: when `none` is not set, `newest` bounds the epoch of
every change a crash that keeps `cut` leaves of them; when it is, the crash leaves no change of
them. -/
def Covered (d : Disk) (cut : Slice Kept) (i : Nat) (none : Bool) (newest : U64) : Prop :=
  (none = false → ∀ j (hj : j < d.files.val.length) (hjc : j < cut.val.length), j < i →
    ∀ c (hc : c < (d.files.val[j]).changes.val.length), c < reachOf d.files.val[j] cut.val[j] →
      ((d.files.val[j]).changes.val[c]).epoch.val ≤ newest.val) ∧
  (none = true → ∀ j (hj : j < d.files.val.length) (hjc : j < cut.val.length), j < i →
    reachOf d.files.val[j] cut.val[j] = 0)

/-- The loop of `Disk::newest` from index `i`, with more fuel than `2^64` plus the turns left
(each turn runs `File::newest`, whose loop takes up to `2^64 - 1` turns): it keeps `Covered`,
up to the shorter of the files and the cut. -/
theorem disk_newest_loop (d : Disk) (cut : Slice Kept) :
    ∀ (fuel : Nat) (i : Usize) (none : Bool) (newest : U64),
      i.val.toNat ≤ min d.files.val.length cut.val.length →
      (min d.files.val.length cut.val.length - i.val.toNat) + 18446744073709551616 < fuel →
      Covered d cut i.val.toNat none newest →
      ∃ none' newest', Disk.newest.loop_0 fuel d cut none newest i = Result.ok (none', newest') ∧
        Covered d cut (min d.files.val.length cut.val.length) none' newest' := by
  intro fuel
  induction fuel with
  | zero => intro i none newest _ h; omega
  | succ n ih =>
    intro i none newest hi hfuel hcov
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hlf := d.files.hlen
    have hlc := cut.hlen
    by_cases hlt : i.val.toNat < min d.files.val.length cut.val.length
    · have hif : i.val.toNat < d.files.val.length := by omega
      have hic : i.val.toNat < cut.val.length := by omega
      have hifv : i.val < (d.files.val.length : Int) := by omega
      have hicv : i.val < (cut.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hreach := reachOf_le d.files.val[i.val.toNat] cut.val[i.val.toNat]
      have hlch := (d.files.val[i.val.toNat]).changes.hlen
      obtain ⟨n1, w1, hns, hn1, hcov1, hmono⟩ :=
        newest_spec n d.files.val[i.val.toNat] cut.val[i.val.toNat] none newest (by omega)
      have hcov' : Covered d cut j.val.toNat n1 w1 := by
        constructor
        · intro hn1f k hk hkc hkj c hc hcr
          by_cases e : k = i.val.toNat
          · subst e
            exact hcov1 hn1f c hc hcr
          · cases none with
            | true =>
              have := hcov.2 rfl k hk hkc (by omega)
              omega
            | false =>
              have := hcov.1 rfl k hk hkc (by omega) c hc hcr
              have := (hmono rfl).2
              omega
        · intro hn1t k hk hkc hkj
          have ⟨hnt, hr0⟩ := hn1.mp hn1t
          by_cases e : k = i.val.toNat
          · subst e
            exact hr0
          · exact hcov.2 hnt k hk hkc (by omega)
      obtain ⟨n', w', h1, h2⟩ := ih j n1 w1 (by omega) (by omega) hcov'
      refine ⟨n', w', ?_, h2⟩
      simp [Disk.newest.loop_0, Slice.index, hif, hic, hifv, hicv, hns, hj, h1]
    · have heq : i.val.toNat = min d.files.val.length cut.val.length := by omega
      have hstop : ¬ (i.val < (d.files.val.length : Int) ∧ i.val < (cut.val.length : Int)) := by
        omega
      refine ⟨none, newest, ?_, heq ▸ hcov⟩
      by_cases ha : i.val < (d.files.val.length : Int)
      · have hb : ¬ i.val < (cut.val.length : Int) := fun hb => hstop ⟨ha, hb⟩
        simp [Disk.newest.loop_0, ha, hb]
      · simp [Disk.newest.loop_0, ha]

/-- `Disk::newest`, with at least `2^65` fuel: `some n` bounds the epoch of every change a
crash that keeps `cut` leaves, whole or in part, of each file it has a `Kept` for; `none` says
it leaves none. -/
theorem disk_newest_spec (fuel : Nat) (d : Disk) (cut : Slice Kept)
    (h : 36893488147419103232 ≤ fuel) :
    ∃ r, Disk.newest fuel d cut = Result.ok r ∧
      (∀ n, r = some n → ∀ j (hj : j < d.files.val.length) (hjc : j < cut.val.length),
        ∀ c (hc : c < (d.files.val[j]).changes.val.length), c < reachOf d.files.val[j] cut.val[j] →
          ((d.files.val[j]).changes.val[c]).epoch.val ≤ n.val) ∧
      (r = none → ∀ j (hj : j < d.files.val.length) (hjc : j < cut.val.length),
        reachOf d.files.val[j] cut.val[j] = 0) := by
  have hlf := d.files.hlen
  obtain ⟨n', w', h1, h2⟩ := disk_newest_loop d cut fuel 0 true 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; omega)
    ⟨fun h => by simp at h, fun _ j _ _ hj => by rw [usize_zero] at hj; omega⟩
  cases n' with
  | true =>
    refine ⟨Option.none, by simp [Disk.newest, h1], by simp, fun _ j hj hjc => ?_⟩
    exact h2.2 rfl j hj hjc (by omega)
  | false =>
    refine ⟨some w', by simp [Disk.newest, h1], ?_, by simp⟩
    intro n hn j hj hjc c hc hcr
    simp only [Option.some.injEq] at hn
    subst hn
    exact h2.1 rfl j hj hjc (by omega) c hc hcr

/-! ## 5. `Disk::allows` -/

/-- `sectors` never panics. -/
theorem sectors_total (len : Usize) : ∃ s, sectors len = Result.ok s := by
  blaze_auto [blaze_core.world.disk.sectors]

/-- `sectors` never panics, as a triple. -/
theorem sectors_triple (len : Usize) {epost : Result.EPost} :
    Triple (sectors len) True (fun _ => True) epost := by
  obtain ⟨s, hs⟩ := sectors_total len
  rw [hs]
  exact Result.ok_spec (post := fun _ => True) s

/-- `File::fits` never panics. -/
theorem fits_total (f : File) (k : Kept) : ∃ b, File.fits f k = Result.ok b := by
  blaze_auto [blaze_core.world.disk.File.fits, sectors_triple]

/-- What `allows` promises: of every pair of files `j1`, `j2` the cut has a `Kept` for, each
barrier of `j1` that completed before a change `j2` keeps whole or in part has everything it
covers kept whole. -/
def Orders (d : Disk) (cut : Slice Kept) : Prop :=
  ∀ j1 (h1 : j1 < d.files.val.length) (h1c : j1 < cut.val.length)
    j2 (h2 : j2 < d.files.val.length) (h2c : j2 < cut.val.length),
    ∀ b ∈ (d.files.val[j1]).barriers.val,
    ∀ c (hc : c < (d.files.val[j2]).changes.val.length), c < reachOf d.files.val[j2] cut.val[j2] →
      b.epoch.val < ((d.files.val[j2]).changes.val[c]).epoch.val →
      b.covers.val ≤ (cut.val[j1]).whole.val

/-- The second loop of `allows`, from index `i`, with more fuel than `2^64` plus the turns left:
it gives `true` only when every file from `i` on keeps whole what each of its barriers that
completed before `newest` covers. -/
theorem allows_loop1 (d : Disk) (cut : Slice Kept) (newest : U64)
    (hlen : cut.val.length = d.files.val.length) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ cut.val.length →
      (cut.val.length - i.val.toNat) + 18446744073709551616 < fuel →
      ∃ b, Disk.allows.loop_1 fuel d cut i newest = Result.ok b ∧
        (b = true → ∀ j (hj : j < d.files.val.length) (hjc : j < cut.val.length),
          i.val.toNat ≤ j → ∀ x ∈ (d.files.val[j]).barriers.val,
            x.epoch.val < newest.val → x.covers.val ≤ (cut.val[j]).whole.val) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hlc := cut.hlen
    by_cases hlt : i.val.toNat < cut.val.length
    · have hif : i.val.toNat < d.files.val.length := by omega
      have hicv : i.val < (cut.val.length : Int) := by omega
      have hlb := (d.files.val[i.val.toNat]).barriers.hlen
      obtain ⟨b0, hb0, hiff0⟩ := keeps_ordered_spec n d.files.val[i.val.toNat]
        (cut.val[i.val.toNat]).whole newest (by omega)
      cases b0 with
      | false =>
        refine ⟨false, ?_, by simp⟩
        simp [Disk.allows.loop_1, Slice.index, hif, hlt, hicv, hb0]
      | true =>
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb, himp⟩ := ih j (by omega) (by omega)
        refine ⟨b, ?_, ?_⟩
        · simp [Disk.allows.loop_1, Slice.index, hif, hlt, hicv, hb0, hj, hb]
        · intro hbt k hk hkc hik x hx hxe
          by_cases e : k = i.val.toNat
          · subst e
            exact hiff0.mp rfl x hx hxe
          · exact himp hbt k hk hkc (by omega) x hx hxe
    · have hicv : ¬ i.val < (cut.val.length : Int) := by omega
      refine ⟨true, by simp [Disk.allows.loop_1, hicv], ?_⟩
      intro _ k _ hkc hik
      omega

/-- The first loop of `allows`, from index `i`, with more fuel than `2^65` plus the turns left:
it never panics, and gives `true` only when the cut `Orders` the disk. -/
theorem allows_loop0 (d : Disk) (cut : Slice Kept)
    (hlen : cut.val.length = d.files.val.length) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ cut.val.length →
      (cut.val.length - i.val.toNat) + 36893488147419103232 < fuel →
      ∃ b, Disk.allows.loop_0 fuel d cut i = Result.ok b ∧ (b = true → Orders d cut) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hlc := cut.hlen
    by_cases hlt : i.val.toNat < cut.val.length
    · have hif : i.val.toNat < d.files.val.length := by omega
      have hicv : i.val < (cut.val.length : Int) := by omega
      obtain ⟨bf, hbf⟩ := fits_total d.files.val[i.val.toNat] cut.val[i.val.toNat]
      cases bf with
      | false =>
        refine ⟨false, ?_, by simp⟩
        simp [Disk.allows.loop_0, Slice.index, hif, hlt, hicv, hbf]
      | true =>
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb, himp⟩ := ih j (by omega) (by omega)
        exact ⟨b, by simp [Disk.allows.loop_0, Slice.index, hif, hlt, hicv, hbf, hj, hb], himp⟩
    · have hicv : ¬ i.val < (cut.val.length : Int) := by omega
      obtain ⟨r, hr, hsome, hnone⟩ := disk_newest_spec n d cut (by omega)
      cases r with
      | none =>
        refine ⟨true, by simp [Disk.allows.loop_0, hicv, hr], fun _ => ?_⟩
        intro j1 h1 h1c j2 h2 h2c x hx c hc hcr
        have := hnone rfl j2 h2 h2c
        omega
      | some w =>
        obtain ⟨b, hb, himp⟩ := allows_loop1 d cut w hlen n 0 (by rw [usize_zero]; omega)
          (by rw [usize_zero]; omega)
        refine ⟨b, by simp [Disk.allows.loop_0, hicv, hr, hb], fun hbt => ?_⟩
        intro j1 h1 h1c j2 h2 h2c x hx c hc hcr hxe
        have hw := hsome w rfl j2 h2 h2c c hc hcr
        exact himp hbt j1 h1 h1c (by rw [usize_zero]; omega) x hx (by omega)

/-- `Disk::allows`, with at least `2^66` fuel: it never panics nor runs out of fuel, and gives
`true` only for a cut with one `Kept` per file that `Orders` the disk. -/
theorem allows_spec (fuel : Nat) (d : Disk) (cut : Slice Kept)
    (h : 73786976294838206464 ≤ fuel) :
    ∃ b, Disk.allows fuel d cut = Result.ok b ∧
      (b = true → cut.val.length = d.files.val.length ∧ Orders d cut) := by
  have hlc := cut.hlen
  by_cases hlen : cut.val.length = d.files.val.length
  · have hlenv : (cut.val.length : Int) = (d.files.val.length : Int) := by omega
    obtain ⟨b, hb, himp⟩ := allows_loop0 d cut hlen fuel 0 (by rw [usize_zero]; omega)
      (by rw [usize_zero]; omega)
    exact ⟨b, by simp [Disk.allows, hlenv, hb], fun hbt => ⟨hlen, himp hbt⟩⟩
  · have hlenv : ¬ (cut.val.length : Int) = (d.files.val.length : Int) := by omega
    exact ⟨false, by simp [Disk.allows, hlenv], by simp⟩

/-- What `Disk::allows` is for: with at least `2^66` fuel, a cut it allows has one `Kept` per
file, and a crash that keeps it, which keeps whole or in part a change made after a barrier
completed, keeps whole everything that barrier covers. -/
theorem allows_orders (fuel : Nat) (d : Disk) (cut : Slice Kept)
    (h : 73786976294838206464 ≤ fuel) (H : Disk.allows fuel d cut = Result.ok true) :
    cut.val.length = d.files.val.length ∧
      ∀ j1 (h1 : j1 < d.files.val.length) (h1c : j1 < cut.val.length)
        j2 (h2 : j2 < d.files.val.length) (h2c : j2 < cut.val.length),
        ∀ b ∈ (d.files.val[j1]).barriers.val,
        ∀ c (hc : c < (d.files.val[j2]).changes.val.length),
          c < reachOf d.files.val[j2] cut.val[j2] →
          b.epoch.val < ((d.files.val[j2]).changes.val[c]).epoch.val →
          b.covers.val ≤ (cut.val[j1]).whole.val := by
  obtain ⟨b, hb, himp⟩ := allows_spec fuel d cut h
  rw [H] at hb
  cases hb
  exact himp rfl

/-! ## 6. Totality -/

/-- `Disk::allows`, with at least `2^66` fuel, neither panics nor runs out of fuel. -/
theorem allows_total (fuel : Nat) (d : Disk) (cut : Slice Kept)
    (h : 73786976294838206464 ≤ fuel) :
    ∃ b, Disk.allows fuel d cut = Result.ok b :=
  (allows_spec fuel d cut h).imp fun _ hb => hb.1

end BlazeCore.World.DiskAllows
