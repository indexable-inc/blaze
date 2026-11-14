import BlazeCore.World.DiskAllows
import BlazeModel.blaze_core.world.disk.copy
import BlazeModel.blaze_core.world.disk.starts_with
import BlazeModel.blaze_core.world.disk.kept
import BlazeModel.blaze_core.world.disk.cut_to_a_prefix
import BlazeModel.blaze_core.world.disk.extend
import BlazeModel.blaze_core.world.disk.Change.apply
import BlazeModel.blaze_core.world.disk.File.small
import BlazeModel.blaze_core.world.disk.File.only_appended
import BlazeModel.blaze_core.world.disk.File.contents
import BlazeModel.blaze_core.world.disk.File.sync
import BlazeModel.blaze_core.world.disk.File.cut

/-!
`blaze_core::world::disk` (crates/blaze-core/src/world/disk.rs): what the disk world's per-file
functions do to a file's bytes, proved over their generated models.

- Bytes helpers: `starts_with_spec`, `copy_spec`, `extend_spec`, `kept_spec`,
  `cut_to_a_prefix_spec`, each a plain list statement.
- `Change::apply` as the list function `applyL`: `apply_append`, and `apply_ok` for every
  small change (`write_spec`, `resize_spec`).
- `small_sound`: `File::small` returning `true`, at any fuel, gives `Small` (durable bytes at
  most `2^40`, at most `2^16` changes and barriers, each change's bytes and offset at most
  `2^40`); `only_appended_spec`: `File::only_appended` is `AllAppend`.
- `contents_spec`, `contents_appended`: `File::contents` folds `applyL` over the changes; for
  appends, the durable bytes then `appended f`.
- `sync_spec`, `sync_appended`: `File::sync`.
- `cut_appended` (A): a crash cuts an only-appended file to a prefix of what it read as, never
  below its durable bytes. `cut_synced` (B): a crash keeps a file with no change exactly.
  `sync_all`: a sync covering every change leaves what `File::contents` read, durable.

Fuel: one bound for everything, `2^66 ≤ fuel` (as `DiskAllows`); each nested call gets its
caller's remaining fuel, and no loop here turns more than `2^64` times.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.World.DiskBytes

open blaze_core.world.disk
open BlazeCore.World.DiskAllows (add_one usize_zero)

/-! ## 1. Bytes helpers -/

/-- The loop of `starts_with` from `i`: `true` exactly when `bytes` agrees with `prefix` at
every position from `i` on. -/
theorem starts_loop (bytes pre : Slice U8) (hle : pre.val.length ≤ bytes.val.length) :
    ∀ (fuel : Nat) (i : Usize), i.val.toNat ≤ pre.val.length →
      pre.val.length - i.val.toNat < fuel →
      ∃ b, starts_with.loop_0 fuel bytes pre i = Result.ok b ∧
        (b = true ↔ ∀ k (hk : k < pre.val.length), i.val.toNat ≤ k →
          bytes.val[k]'(by omega) = pre.val[k]) := by
  intro fuel
  induction fuel with
  | zero => intro i _ h; omega
  | succ n ih =>
    intro i hi hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hlp := pre.hlen
    by_cases hlt : i.val.toNat < pre.val.length
    · have hiv : i.val < (pre.val.length : Int) := by omega
      have hib : i.val.toNat < bytes.val.length := by omega
      by_cases heq : (bytes.val[i.val.toNat]).val = (pre.val[i.val.toNat]).val
      · obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb, hiff⟩ := ih j (by omega) (by omega)
        refine ⟨b, ?_, ?_⟩
        · simp [starts_with.loop_0, Slice.index, hlt, hib, hiv, heq, hj, hb, Scalar.ne]
        · rw [hiff]
          constructor
          · intro H k hk hik
            by_cases e : k = i.val.toNat
            · subst e
              exact Scalar.ext heq
            · exact H k hk (by omega)
          · intro H k hk hik
            exact H k hk (by omega)
      · refine ⟨false, ?_, ?_⟩
        · simp [starts_with.loop_0, Slice.index, hlt, hib, hiv, heq, Scalar.ne]
        · simp only [Bool.false_eq_true, false_iff]
          intro H
          exact heq (congrArg Scalar.val (H _ hlt (Nat.le_refl _)))
    · have hiv : ¬ i.val < (pre.val.length : Int) := by omega
      refine ⟨true, by simp [starts_with.loop_0, hiv], ?_⟩
      simp only [true_iff]
      intro k hk hik
      omega

/-- `starts_with`, with more fuel than `prefix` is long: `true` exactly when `prefix` is a
prefix of `bytes`. -/
theorem starts_with_spec (fuel : Nat) (bytes pre : Slice U8) (h : pre.val.length < fuel) :
    ∃ b, starts_with fuel bytes pre = Result.ok b ∧ (b = true ↔ pre.val <+: bytes.val) := by
  by_cases hgt : bytes.val.length < pre.val.length
  · have hgtv : (bytes.val.length : Int) < (pre.val.length : Int) := by omega
    refine ⟨false, by simp [starts_with, Scalar.gt, hgtv], ?_⟩
    simp only [Bool.false_eq_true, false_iff]
    intro hp
    have := hp.length_le
    omega
  · have hgtv : ¬ (bytes.val.length : Int) < (pre.val.length : Int) := by omega
    obtain ⟨b, hb, hiff⟩ := starts_loop bytes pre (by omega) fuel 0 (by rw [usize_zero]; omega)
      (by rw [usize_zero]; omega)
    refine ⟨b, by simp [starts_with, Scalar.gt, hgtv, hb], ?_⟩
    rw [hiff, List.prefix_iff_getElem]
    constructor
    · intro H
      exact ⟨by omega, fun k hk => (H k hk (by rw [usize_zero]; omega)).symm⟩
    · intro ⟨_, H⟩ k hk _
      exact (H k hk).symm

/-- `copy` never panics and gives the same bytes. -/
theorem copy_spec (bytes : Slice U8) :
    ∃ v, copy bytes = Result.ok v ∧ v.val = bytes.val := by
  have h : (Vec.new : Vec U8).val.length + bytes.val.length ≤ 18446744073709551615 := by
    simp only [Vec.new_val, List.length_nil, Nat.zero_add]
    exact bytes.hlen
  refine ⟨_, by simp only [copy]; exact Vec.extend_from_slice_ok _ _ h, ?_⟩
  simp

/-- The loop of `extend` from `i`: the rest of the first `count` bytes of `more` after
`bytes`. -/
theorem extend_loop (more : Slice U8) (count : Usize) :
    ∀ (fuel : Nat) (bytes : Vec U8) (i : Usize), i.val.toNat ≤ more.val.length →
      i.val ≤ count.val → more.val.length - i.val.toNat < fuel →
      bytes.val.length + (min count.val.toNat more.val.length - i.val.toNat) ≤
        18446744073709551615 →
      ∃ v, extend.loop_0 fuel bytes more count i = Result.ok v ∧
        v.val = bytes.val ++ (more.val.take count.val.toNat).drop i.val.toNat := by
  intro fuel
  induction fuel with
  | zero => intro _ i _ _ h; omega
  | succ n ih =>
    intro bytes i hi hic hfuel hlen
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hlm := more.hlen
    by_cases hgo : i.val < count.val ∧ i.val < (more.val.length : Int)
    · have hlt : i.val.toNat < more.val.length := by omega
      have hpush : bytes.val.length < 18446744073709551615 := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hb1 : (bytes.val ++ [more.val[i.val.toNat]]).length ≤ 18446744073709551615 := by
        simp only [List.length_append, List.length_singleton]; omega
      obtain ⟨v, hv, hvv⟩ := ih ⟨bytes.val ++ [more.val[i.val.toNat]], hb1⟩ j
        (by omega) (by omega) (by omega)
        (by simp only [List.length_append, List.length_singleton]; omega)
      refine ⟨v, ?_, ?_⟩
      · simp [extend.loop_0, Scalar.lt, Slice.index, hgo.1, hgo.2, hlt, hpush, hj, hv]
      · rw [hvv]
        have hlt' : i.val.toNat < (more.val.take count.val.toNat).length := by
          simp only [List.length_take]; omega
        have hj' : j.val.toNat = i.val.toNat + 1 := by omega
        rw [hj', List.drop_eq_getElem_cons hlt', List.getElem_take]
        simp
    · refine ⟨bytes, ?_, ?_⟩
      · by_cases hc : i.val < count.val
        · have hm : ¬ i.val < (more.val.length : Int) := fun hm => hgo ⟨hc, hm⟩
          simp [extend.loop_0, Scalar.lt, hc, hm]
        · simp [extend.loop_0, Scalar.lt, hc]
      · rw [Blaze.List.drop_of_length_le _ _ (by simp only [List.length_take]; omega)]
        simp

/-- `extend`, with more fuel than `more` is long and room for what it adds: `bytes` and then
the first `count` bytes of `more` (all of them when it has fewer). -/
theorem extend_spec (fuel : Nat) (bytes : Vec U8) (more : Slice U8) (count : Usize)
    (h : more.val.length < fuel)
    (hlen : bytes.val.length + min count.val.toNat more.val.length ≤ 18446744073709551615) :
    ∃ v, extend fuel bytes more count = Result.ok v ∧
      v.val = bytes.val ++ more.val.take count.val.toNat := by
  have hc := count.hmin
  simp only [ScalarTy.min] at hc
  obtain ⟨v, hv, hvv⟩ := extend_loop more count fuel bytes 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; omega) (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
  refine ⟨v, by simp only [extend]; exact hv, ?_⟩
  rw [hvv, usize_zero]
  simp

/-- `kept`, with `2^64` fuel: `true` exactly when the file is there after the crash and reads
as `read`. -/
theorem kept_spec (fuel : Nat) (read : Slice U8) (after : Option (Vec U8))
    (h : 18446744073709551616 ≤ fuel) :
    ∃ b, kept fuel read after = Result.ok b ∧
      (b = true ↔ ∃ v, after = some v ∧ v.val = read.val) := by
  cases after with
  | none => exact ⟨false, by simp [kept], by simp⟩
  | some v =>
    by_cases hl : v.val.length = read.val.length
    · have hlv : (v.val.length : Int) = (read.val.length : Int) := by omega
      have hr := read.hlen
      obtain ⟨b, hb, hiff⟩ := starts_with_spec fuel v read (by omega)
      refine ⟨b, by simp [kept, Scalar.eq, hlv, hb], ?_⟩
      rw [hiff]
      simp only [Option.some.injEq, exists_eq_left']
      constructor
      · intro hp
        exact (hp.eq_of_length hl.symm).symm
      · intro he
        rw [he]
        exact List.prefix_refl _
    · have hlv : ¬ (v.val.length : Int) = (read.val.length : Int) := by omega
      refine ⟨false, by simp [kept, Scalar.eq, hlv], ?_⟩
      simp only [Bool.false_eq_true, false_iff, Option.some.injEq, exists_eq_left']
      intro he
      exact hl (by rw [he])

/-- `cut_to_a_prefix`, with `2^64` fuel: `true` exactly when the file is there after the
crash, begins with `durable`, and is a prefix of `read`. -/
theorem cut_to_a_prefix_spec (fuel : Nat) (durable read : Slice U8) (after : Option (Vec U8))
    (h : 18446744073709551616 ≤ fuel) :
    ∃ b, cut_to_a_prefix fuel durable read after = Result.ok b ∧
      (b = true ↔ ∃ v, after = some v ∧ durable.val <+: v.val ∧ v.val <+: read.val) := by
  cases after with
  | none => exact ⟨false, by simp [cut_to_a_prefix], by simp⟩
  | some v =>
    have hd := durable.hlen
    have hv := v.hlen
    obtain ⟨b1, hb1, hiff1⟩ := starts_with_spec fuel v durable (by omega)
    cases b1 with
    | false =>
      refine ⟨false, by simp [cut_to_a_prefix, hb1], ?_⟩
      simp only [Bool.false_eq_true, false_iff, Option.some.injEq, exists_eq_left', not_and]
      intro hp
      exact absurd (hiff1.mpr hp) (by simp)
    | true =>
      obtain ⟨b2, hb2, hiff2⟩ := starts_with_spec fuel read v (by omega)
      refine ⟨b2, by simp [cut_to_a_prefix, hb1, hb2], ?_⟩
      rw [hiff2]
      simp only [Option.some.injEq, exists_eq_left']
      exact ⟨fun hp => ⟨hiff1.mp rfl, hp⟩, fun hp => hp.2⟩

/-! ## 2. `Change::apply` of an append -/

/-- `Change::apply` of `Append more` adds `more` at the end, when the two fit in a `usize`. -/
theorem apply_append (fuel : Nat) (more bytes : Vec U8)
    (h : bytes.val.length + more.val.length ≤ 18446744073709551615) :
    ∃ v, Change.apply fuel (Change.Append more) bytes = Result.ok v ∧
      v.val = bytes.val ++ more.val :=
  ⟨_, by simp only [Change.apply]; exact Vec.extend_from_slice_ok _ _ h, rfl⟩

/-! ## Small files -/

/-- `2^40`, `MOST_BYTES`, as the literal the models write. -/
theorem most_bytes : (1099511627776 : Usize).val = 1099511627776 :=
  Scalar.ofNat_val .Usize _ (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- `2^16`, `MOST_CHANGES`, as the literal the models write. -/
theorem most_changes : (65536 : Usize).val = 65536 :=
  Scalar.ofNat_val .Usize _ (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- A change within the bounds: its bytes, its offset and the length it cuts to are at most
`2^40`. -/
def ChangeSmall : Change → Prop
  | Change.Append more => more.val.length ≤ 1099511627776
  | Change.At a more => a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776
  | Change.Sectors a more => a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776
  | Change.Cut len => len.val ≤ 1099511627776

/-- What `File::small` checks: durable bytes at most `2^40`, at most `2^16` changes and
barriers, and each change small. -/
def Small (f : File) : Prop :=
  f.durable.val.length ≤ 1099511627776 ∧ f.changes.val.length ≤ 65536 ∧
    f.barriers.val.length ≤ 65536 ∧ ∀ m ∈ f.changes.val, ChangeSmall m.change

/-- The loop of `File::small` from `i` gives `true` only when every change from `i` on is
small. -/
theorem small_loop (f : File) :
    ∀ (fuel : Nat) (i : Usize), File.small.loop_0 fuel f i = Result.ok true →
      ∀ k (hk : k < f.changes.val.length), i.val.toNat ≤ k →
        ChangeSmall (f.changes.val[k]).change := by
  intro fuel
  induction fuel with
  | zero => intro i h; simp [File.small.loop_0] at h
  | succ n ih =>
    intro i h k hk hik
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.changes.hlen
    by_cases hlt : i.val.toNat < f.changes.val.length
    · have hltv : i.val < (f.changes.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have key : ChangeSmall (f.changes.val[i.val.toNat]).change ∧
          File.small.loop_0 n f j = Result.ok true := by
        cases hc : (f.changes.val[i.val.toNat]).change with
        | Append more =>
          simp [File.small.loop_0, Slice.index, Scalar.lt, Scalar.le, hlt, hltv, hc, hj,
            most_bytes, ChangeSmall] at h ⊢
          split at h
          · simp at h
          · exact ⟨by omega, h⟩
        | At a more =>
          simp [File.small.loop_0, Slice.index, Scalar.lt, Scalar.le, hlt, hltv, hc, hj,
            most_bytes, ChangeSmall] at h ⊢
          split at h
          · simp at h
          · exact ⟨⟨by omega, by omega⟩, h⟩
        | Sectors a more =>
          simp [File.small.loop_0, Slice.index, Scalar.lt, Scalar.le, hlt, hltv, hc, hj,
            most_bytes, ChangeSmall] at h ⊢
          split at h
          · simp at h
          · exact ⟨⟨by omega, by omega⟩, h⟩
        | Cut len =>
          simp [File.small.loop_0, Slice.index, Scalar.lt, Scalar.le, hlt, hltv, hc, hj,
            most_bytes, ChangeSmall] at h ⊢
          split at h
          · simp at h
          · exact ⟨by omega, h⟩
      by_cases e : k = i.val.toNat
      · subst e
        exact key.1
      · exact ih j key.2 k hk (by omega)
    · omega

/-- A file `File::small` says is small, at any fuel, is `Small`. -/
theorem small_sound (fuel : Nat) (f : File) (h : File.small fuel f = Result.ok true) :
    Small f := by
  have hd := f.durable.hlen
  have hc := f.changes.hlen
  have hb := f.barriers.hlen
  by_cases h1 : (f.durable.val.length : Int) ≤ 1099511627776 ∧
      (f.changes.val.length : Int) ≤ 65536 ∧ (f.barriers.val.length : Int) ≤ 65536
  · have g1 : ¬ (1099511627776 : Int) < f.durable.val.length := by omega
    have g2 : ¬ (65536 : Int) < f.changes.val.length := by omega
    have g3 : ¬ (65536 : Int) < f.barriers.val.length := by omega
    have hloop : File.small.loop_0 fuel f 0 = Result.ok true := by
      simpa [File.small, Scalar.gt, most_bytes, most_changes, g1, g2, g3] using h
    refine ⟨by omega, by omega, by omega, ?_⟩
    intro m hm
    obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hm
    exact small_loop f fuel 0 hloop k hk (by rw [usize_zero]; omega)
  · exfalso
    by_cases h2 : (f.durable.val.length : Int) ≤ 1099511627776
    · by_cases h3 : (f.changes.val.length : Int) ≤ 65536
      · have h4 : ¬ (f.barriers.val.length : Int) ≤ 65536 := fun h4 => h1 ⟨h2, h3, h4⟩
        have g3 : (65536 : Int) < f.barriers.val.length := by omega
        simp [File.small, Scalar.gt, most_bytes, most_changes, h2, h3, g3] at h
      · simp [File.small, Scalar.gt, most_bytes, most_changes, h2, h3] at h
    · simp [File.small, Scalar.gt, most_bytes, most_changes, h2] at h

/-! ## 3. `File::only_appended` -/

/-- Whether a change is an append. -/
def isAppend : Change → Bool
  | Change.Append _ => true
  | _ => false

/-- Every change of the file is an append. -/
def AllAppend (f : File) : Prop := ∀ m ∈ f.changes.val, isAppend m.change = true

/-- The loop of `only_appended` from `i`: `true` exactly when every change from `i` on is an
append. -/
theorem only_appended_loop (f : File) :
    ∀ (fuel : Nat) (i : Usize), f.changes.val.length - i.val.toNat < fuel →
      ∃ b, File.only_appended.loop_0 fuel f i = Result.ok b ∧
        (b = true ↔ ∀ k (hk : k < f.changes.val.length), i.val.toNat ≤ k →
          isAppend (f.changes.val[k]).change = true) := by
  intro fuel
  induction fuel with
  | zero => intro i h; omega
  | succ n ih =>
    intro i hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.changes.hlen
    by_cases hlt : i.val.toNat < f.changes.val.length
    · have hltv : i.val < (f.changes.val.length : Int) := by omega
      cases hc : (f.changes.val[i.val.toNat]).change with
      | Append more =>
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        obtain ⟨b, hb, hiff⟩ := ih j (by omega)
        refine ⟨b, by simp [File.only_appended.loop_0, Slice.index, Scalar.lt, hlt, hltv, hc,
          hj, hb], ?_⟩
        rw [hiff]
        constructor
        · intro H k hk hik
          by_cases e : k = i.val.toNat
          · subst e
            rw [hc]; rfl
          · exact H k hk (by omega)
        · intro H k hk hik
          exact H k hk (by omega)
      | At a more =>
        refine ⟨false, by simp [File.only_appended.loop_0, Slice.index, Scalar.lt, hlt, hltv,
          hc], ?_⟩
        simp only [Bool.false_eq_true, false_iff]
        intro H
        have := H _ hlt (Nat.le_refl _)
        rw [hc] at this
        exact absurd this (by simp [isAppend])
      | Sectors a more =>
        refine ⟨false, by simp [File.only_appended.loop_0, Slice.index, Scalar.lt, hlt, hltv,
          hc], ?_⟩
        simp only [Bool.false_eq_true, false_iff]
        intro H
        have := H _ hlt (Nat.le_refl _)
        rw [hc] at this
        exact absurd this (by simp [isAppend])
      | Cut len =>
        refine ⟨false, by simp [File.only_appended.loop_0, Slice.index, Scalar.lt, hlt, hltv,
          hc], ?_⟩
        simp only [Bool.false_eq_true, false_iff]
        intro H
        have := H _ hlt (Nat.le_refl _)
        rw [hc] at this
        exact absurd this (by simp [isAppend])
    · have hltv : ¬ i.val < (f.changes.val.length : Int) := by omega
      refine ⟨true, by simp [File.only_appended.loop_0, Scalar.lt, hltv], ?_⟩
      simp only [true_iff]
      intro k hk hik
      omega

/-- `File::only_appended`, with more fuel than the file has changes: `true` exactly when every
change of the file is an append. -/
theorem only_appended_spec (fuel : Nat) (f : File) (h : f.changes.val.length < fuel) :
    ∃ b, File.only_appended fuel f = Result.ok b ∧ (b = true ↔ AllAppend f) := by
  obtain ⟨b, hb, hiff⟩ := only_appended_loop f fuel 0 (by rw [usize_zero]; omega)
  refine ⟨b, by simp only [File.only_appended]; exact hb, ?_⟩
  rw [hiff]
  constructor
  · intro H m hm
    obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hm
    exact H k hk (by rw [usize_zero]; omega)
  · intro H k hk _
    exact H _ (List.getElem_mem hk)

/-! ## What a change does to a file's bytes, as lists -/

/-- `more` written over `b` at `a`, `b` grown with zeros to hold it: what `write` does with
`from = 0`, `to = more.len()`. -/
def writeL (b : List U8) (a : Nat) (more : List U8) : List U8 :=
  (b ++ List.replicate (a + more.length - b.length) (0 : U8)).take a ++ more ++
    (b ++ List.replicate (a + more.length - b.length) (0 : U8)).drop (a + more.length)

/-- `b` after the change, as a list. -/
def applyL : Change → List U8 → List U8
  | Change.Append more, b => b ++ more.val
  | Change.At a more, b => writeL b a.val.toNat more.val
  | Change.Sectors a more, b => writeL b a.val.toNat more.val
  | Change.Cut len, b => b.take len.val.toNat ++ List.replicate (len.val.toNat - b.length) (0 : U8)

/-- One change made to `b`: the step of the fold that is a file's contents. -/
def stepL (b : List U8) (m : Made) : List U8 := applyL m.change b

/-- A small change makes the bytes at most `2^41` longer. -/
theorem applyL_length (c : Change) (b : List U8) (h : ChangeSmall c) :
    (applyL c b).length ≤ b.length + 2199023255552 := by
  cases c with
  | Append more =>
    have h' : more.val.length ≤ 1099511627776 := h
    simp only [applyL, List.length_append]; omega
  | At a more =>
    have h' : a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776 := h
    simp only [applyL, writeL, List.length_append, List.length_take, List.length_drop,
      List.length_replicate]; omega
  | Sectors a more =>
    have h' : a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776 := h
    simp only [applyL, writeL, List.length_append, List.length_take, List.length_drop,
      List.length_replicate]; omega
  | Cut len =>
    have h' : len.val ≤ 1099511627776 := h
    simp only [applyL, List.length_append, List.length_take, List.length_replicate]; omega

/-- `Change::apply` of `c`, with `2^65` fuel, on bytes at most `2^62` long, gives `applyL`. -/
def ApplyOk (c : Change) : Prop :=
  ∀ n, 36893488147419103232 ≤ n → ∀ b : Vec U8, b.val.length ≤ 4611686018427387904 →
    ∃ v, Change.apply n c b = Result.ok v ∧ v.val = applyL c b.val

/-- A small append is `ApplyOk`. -/
theorem apply_ok_append (more : Vec U8) (h : ChangeSmall (Change.Append more)) :
    ApplyOk (Change.Append more) := by
  intro n _ b hb
  have h' : more.val.length ≤ 1099511627776 := h
  exact apply_append n more b (by omega)

/-! ## `File::contents` -/

/-- The loop of `File::contents` from `i`: the changes from `i` on made to `bytes`, in order. -/
theorem contents_loop (f : File) (hsm : ∀ m ∈ f.changes.val, ChangeSmall m.change)
    (hok : ∀ m ∈ f.changes.val, ApplyOk m.change) :
    ∀ (fuel : Nat) (bytes : Vec U8) (i : Usize), i.val.toNat ≤ f.changes.val.length →
      36893488147419103232 + (f.changes.val.length - i.val.toNat) < fuel →
      bytes.val.length + 2199023255552 * (f.changes.val.length - i.val.toNat) ≤
        4611686018427387904 →
      ∃ v, File.contents.loop_0 fuel f bytes i = Result.ok v ∧
        v.val = (f.changes.val.drop i.val.toNat).foldl stepL bytes.val := by
  intro fuel
  induction fuel with
  | zero => intro _ i _ h; omega
  | succ n ih =>
    intro bytes i hi hfuel hlen
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.changes.hlen
    by_cases hlt : i.val.toNat < f.changes.val.length
    · have hltv : i.val < (f.changes.val.length : Int) := by omega
      have hmem := List.getElem_mem hlt
      obtain ⟨v1, hv1, hv1v⟩ := hok _ hmem n (by omega) bytes (by omega)
      have hl1 : v1.val.length ≤ bytes.val.length + 2199023255552 := by
        rw [hv1v]; exact applyL_length _ _ (hsm _ hmem)
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      obtain ⟨v, hv, hvv⟩ := ih v1 j (by omega) (by omega) (by omega)
      refine ⟨v, ?_, ?_⟩
      · simp [File.contents.loop_0, Scalar.lt, Slice.index, hlt, hltv, hv1, hj, hv]
      · have hj' : j.val.toNat = i.val.toNat + 1 := by omega
        rw [hvv, List.drop_eq_getElem_cons hlt, List.foldl_cons, hv1v, hj']
        rfl
    · have hltv : ¬ i.val < (f.changes.val.length : Int) := by omega
      refine ⟨bytes, by simp [File.contents.loop_0, Scalar.lt, hltv], ?_⟩
      rw [Blaze.List.drop_of_length_le _ _ (by omega)]
      rfl

/-- `File::contents` of a small file whose changes are all `ApplyOk`, with `2^66` fuel: its
durable bytes with every change made, in order. -/
theorem contents_spec (fuel : Nat) (f : File) (hs : Small f)
    (hok : ∀ m ∈ f.changes.val, ApplyOk m.change) (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ v, File.contents fuel f = Result.ok v ∧
      v.val = f.changes.val.foldl stepL f.durable.val := by
  obtain ⟨hd, hcl, _, hsm⟩ := hs
  have hcond : (Vec.new : Vec U8).val.length + f.durable.val.length ≤ 18446744073709551615 := by
    simp only [Vec.new_val, List.length_nil]; omega
  obtain ⟨v, hv, hvv⟩ := contents_loop f hsm hok fuel
    ⟨(Vec.new : Vec U8).val ++ f.durable.val, by rw [List.length_append]; exact hcond⟩ 0
    (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
    (by rw [usize_zero]; simp only [Vec.new_val, List.nil_append]; omega)
  refine ⟨v, ?_, ?_⟩
  · simp only [File.contents]
    rw [Vec.extend_from_slice_ok _ _ hcond, Result.bind_ok]
    exact hv
  · rw [hvv, usize_zero]
    simp

/-! ## `File::sync` -/

/-- The first loop of `File::sync`, from `done` up to `covered`: the first `covered - done`
changes taken off, oldest first, and made to the durable bytes. -/
theorem sync_loop0 (covered : Usize) :
    ∀ (fuel : Nat) (self : File) (done : Usize), done.val ≤ covered.val →
      (covered.val - done.val).toNat ≤ self.changes.val.length →
      (∀ m ∈ self.changes.val, ChangeSmall m.change) →
      (∀ m ∈ self.changes.val, ApplyOk m.change) →
      36893488147419103232 + (covered.val - done.val).toNat < fuel →
      self.durable.val.length + 2199023255552 * (covered.val - done.val).toNat ≤
        4611686018427387904 →
      ∃ r, File.sync.loop_0 fuel self covered done = Result.ok r ∧ r.name = self.name ∧
        r.barriers = self.barriers ∧ r.undo = self.undo ∧
        r.changes.val = self.changes.val.drop (covered.val - done.val).toNat ∧
        r.durable.val =
          (self.changes.val.take (covered.val - done.val).toNat).foldl stepL self.durable.val := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ _ _ h; omega
  | succ n ih =>
    intro self done hd hc hsm hok hfuel hlen
    rcases self with ⟨name, durable, ⟨cl, chl⟩, barriers, undo⟩
    have hd0 := done.hmin
    simp only [ScalarTy.min] at hd0
    have hcm := covered.hmax
    simp only [ScalarTy.max] at hcm
    by_cases hlt : done.val < covered.val
    · cases cl with
      | nil => simp only [List.length_nil] at hc; omega
      | cons m rest =>
        simp only [List.mem_cons, forall_eq_or_imp] at hsm hok
        simp only at hlen hc
        obtain ⟨v1, hv1, hv1v⟩ := hok.1 n (by omega) durable (by omega)
        have hl1 : v1.val.length ≤ durable.val.length + 2199023255552 := by
          rw [hv1v]; exact applyL_length _ _ hsm.1
        obtain ⟨j, hj, hjv⟩ := add_one done (by omega)
        have hk : (covered.val - done.val).toNat = (covered.val - j.val).toNat + 1 := by omega
        have hrl : rest.length ≤ 18446744073709551615 := by
          simp only [List.length_cons] at chl; omega
        simp only [List.length_cons] at hc
        obtain ⟨r, hr, h1, h2, h3, h4, h5⟩ := ih ⟨name, v1, ⟨rest, hrl⟩, barriers, undo⟩ j
          (by omega) (by simp only; omega) hsm.2 hok.2 (by omega) (by simp only; omega)
        refine ⟨r, ?_, h1, h2, h3, ?_, ?_⟩
        · simp [File.sync.loop_0, Scalar.lt, hlt, Vec.remove, usize_zero, hv1, hj, hr]
        · rw [h4, hk]
          rfl
        · rw [h5, hk]
          simp only [List.take_succ_cons, List.foldl_cons, stepL, hv1v]
    · have hz : (covered.val - done.val).toNat = 0 := by omega
      refine ⟨_, by simp [File.sync.loop_0, Scalar.lt, hlt], rfl, rfl, rfl, ?_, ?_⟩
      · rw [hz]; rfl
      · rw [hz]; rfl

/-- What a barrier left by `sync` is: one of `orig` that covered more than `c` changes, now
covering `c` fewer, of the same epoch. -/
def FromBarrier (orig : List Barrier) (c : Int) (x : Barrier) : Prop :=
  ∃ b ∈ orig, c < b.covers.val ∧ x.covers.val = b.covers.val - c ∧ x.epoch = b.epoch

/-- The second loop of `File::sync`, from `i`: it changes only the barriers, and every barrier
it leaves comes `FromBarrier` the original ones. -/
theorem sync_loop1 (covered : Usize) (orig : List Barrier) :
    ∀ (fuel : Nat) (self : File) (i : Usize), i.val.toNat ≤ self.barriers.val.length →
      self.barriers.val.length - i.val.toNat < fuel →
      (∀ k (hk : k < self.barriers.val.length), k < i.val.toNat →
        FromBarrier orig covered.val self.barriers.val[k]) →
      (∀ k (hk : k < self.barriers.val.length), i.val.toNat ≤ k → self.barriers.val[k] ∈ orig) →
      ∃ r, File.sync.loop_1 fuel self covered i = Result.ok r ∧ r.name = self.name ∧
        r.durable = self.durable ∧ r.changes = self.changes ∧ r.undo = self.undo ∧
        ∀ x ∈ r.barriers.val, FromBarrier orig covered.val x := by
  intro fuel
  induction fuel with
  | zero => intro _ i _ h; omega
  | succ n ih =>
    intro self i hi hfuel hpre hsuf
    rcases self with ⟨name, durable, changes, ⟨bl, bhl⟩, undo⟩
    simp only at hi hfuel hpre hsuf
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hc0 := covered.hmin
    simp only [ScalarTy.min] at hc0
    by_cases hlt : i.val.toNat < bl.length
    · have hltv : i.val < (bl.length : Int) := by omega
      by_cases hgt : covered.val < (bl[i.val.toNat]).covers.val
      · have hbm := (bl[i.val.toNat]).covers.hmax
        simp only [ScalarTy.max] at hbm
        obtain ⟨x3, hx3, hx3v⟩ : ∃ x3 : Usize, Scalar.sub (bl[i.val.toNat]).covers covered =
            Result.ok x3 ∧ x3.val = (bl[i.val.toNat]).covers.val - covered.val :=
          ⟨_, Scalar.sub_ok (bl[i.val.toNat]).covers covered ⟨by simp only [ScalarTy.min]; omega,
            by simp only [ScalarTy.max]; omega⟩, rfl⟩
        obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
        have hnl : (bl.set i.val.toNat { bl[i.val.toNat] with covers := x3 }).length ≤
            18446744073709551615 := by simp only [List.length_set]; omega
        obtain ⟨r, hr, h1, h2, h3, h4, h5⟩ := ih
          ⟨name, durable, changes, ⟨bl.set i.val.toNat { bl[i.val.toNat] with covers := x3 }, hnl⟩,
            undo⟩ j (by simp only [List.length_set]; omega) (by simp only [List.length_set]; omega)
          (by
            intro k hk hkj
            simp only [List.length_set] at hk
            simp only [List.getElem_set]
            by_cases e : i.val.toNat = k
            · subst e
              simp only [ite_true]
              exact ⟨bl[i.val.toNat], hsuf _ hlt (Nat.le_refl _), hgt, hx3v, rfl⟩
            · simp only [e, ite_false]
              exact hpre k hk (by omega))
          (by
            intro k hk hkj
            simp only [List.length_set] at hk
            simp only [List.getElem_set]
            have e : i.val.toNat ≠ k := by omega
            simp only [e, ite_false]
            exact hsuf k hk (by omega))
        refine ⟨r, ?_, h1, h2, h3, h4, h5⟩
        simp [File.sync.loop_1, Scalar.lt, Scalar.gt, Slice.index, Slice.update, hlt, hltv, hgt,
          hx3, hj, hr]
      · have hnl : (bl.eraseIdx i.val.toNat).length ≤ 18446744073709551615 := by
          rw [List.length_eraseIdx_of_lt hlt]; omega
        obtain ⟨r, hr, h1, h2, h3, h4, h5⟩ := ih
          ⟨name, durable, changes, ⟨bl.eraseIdx i.val.toNat, hnl⟩, undo⟩ i
          (by simp only; rw [List.length_eraseIdx_of_lt hlt]; omega)
          (by simp only; rw [List.length_eraseIdx_of_lt hlt]; omega)
          (by
            intro k hk hkj
            simp only [List.getElem_eraseIdx]
            simp only [hkj, dite_true]
            exact hpre k (by omega) hkj)
          (by
            intro k hk hkj
            simp only [List.getElem_eraseIdx]
            have e : ¬ k < i.val.toNat := by omega
            simp only [e, dite_false]
            exact hsuf (k + 1) _ (by omega))
        refine ⟨r, ?_, h1, h2, h3, h4, h5⟩
        simp [File.sync.loop_1, Scalar.lt, Scalar.gt, Slice.index, Vec.remove, hlt, hltv, hgt, hr]
    · have hltv : ¬ i.val < (bl.length : Int) := by omega
      refine ⟨_, by simp [File.sync.loop_1, Scalar.lt, hltv], rfl, rfl, rfl, rfl, ?_⟩
      intro x hx
      obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hx
      have hk' : k < bl.length := hk
      exact hpre k hk (by omega)

/-- `File::sync` of a small file whose changes are all `ApplyOk`, with `2^66` fuel: of
`c = min covers changes.len()`, the first `c` changes are made to the durable bytes, in order,
the rest are left in order, the name is kept, the undo goes exactly when no change is left,
and every barrier left is one of the file's that covered more than `c`, now covering `c`
fewer. -/
theorem sync_spec (fuel : Nat) (f : File) (covers : Usize) (hs : Small f)
    (hok : ∀ m ∈ f.changes.val, ApplyOk m.change) (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ r, File.sync fuel f covers = Result.ok r ∧ r.name = f.name ∧
      r.durable.val = (f.changes.val.take (min covers.val.toNat f.changes.val.length)).foldl
        stepL f.durable.val ∧
      r.changes.val = f.changes.val.drop (min covers.val.toNat f.changes.val.length) ∧
      r.undo = (if f.changes.val.drop (min covers.val.toNat f.changes.val.length) = [] then none
        else f.undo) ∧
      ∀ x ∈ r.barriers.val,
        FromBarrier f.barriers.val ((min covers.val.toNat f.changes.val.length : Nat) : Int) x := by
  obtain ⟨hd, hcl, hbl, hsm⟩ := hs
  have hcv0 := covers.hmin
  simp only [ScalarTy.min] at hcv0
  obtain ⟨cv, hcv_eq, hcvv⟩ : ∃ cv : Usize,
      (if Scalar.lt covers (Slice.len f.changes) then covers else Slice.len f.changes) = cv ∧
        cv.val = ((min covers.val.toNat f.changes.val.length : Nat) : Int) := by
    by_cases h : covers.val < (f.changes.val.length : Int)
    · exact ⟨covers, by simp [Scalar.lt, h], by omega⟩
    · exact ⟨Slice.len f.changes, by simp [Scalar.lt, h], by simp only [Slice.len_val]; omega⟩
  obtain ⟨r0, hr0, a1, a2, a3, a4, a5⟩ := sync_loop0 cv fuel f 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; omega) hsm hok (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
  obtain ⟨r1, hr1, b1, b2, b3, b4, b5⟩ := sync_loop1 cv r0.barriers.val fuel r0 0
    (by rw [usize_zero]; omega) (by have := r0.barriers.hlen; rw [usize_zero]; omega)
    (fun k _ hk => by rw [usize_zero] at hk; omega)
    (fun k hk _ => List.getElem_mem hk)
  have hc : (cv.val - (0 : Usize).val).toNat = min covers.val.toNat f.changes.val.length := by
    rw [usize_zero]; omega
  rw [hc] at a4 a5
  rw [a2, hcvv] at b5
  have hchg : r1.changes.val = f.changes.val.drop (min covers.val.toNat f.changes.val.length) := by
    rw [b3]; exact a4
  by_cases hemp : r1.changes.val = []
  · refine ⟨{ r1 with undo := none }, ?_, by simp only; rw [b1, a1], by simp only; rw [b2, a5],
      by simp only; rw [hchg], ?_, b5⟩
    · simp only [File.sync, hcv_eq, hr0, hr1, Result.bind_ok]
      simp [hemp]
    · rw [← hchg]; simp [hemp]
  · refine ⟨r1, ?_, by rw [b1, a1], by rw [b2, a5], hchg, ?_, b5⟩
    · simp only [File.sync, hcv_eq, hr0, hr1, Result.bind_ok]
      simp [hemp]
    · rw [← hchg]; simp [hemp, b4, a3]

/-! ## 4 and 5. Appends: `File::contents` and `File::sync` of an only-appended file -/

/-- The bytes an append adds; none for any other change. -/
def appendBytes : Change → List U8
  | Change.Append more => more.val
  | _ => []

/-- The bytes of the appends among `l`, in order. -/
def appendedOf (l : List Made) : List U8 := l.flatMap fun m => appendBytes m.change

/-- The bytes of the file's appends not synced, in order. -/
def appended (f : File) : List U8 := appendedOf f.changes.val

/-- Appends made in order add their bytes in order. -/
theorem foldl_appends : ∀ (l : List Made) (b : List U8), (∀ m ∈ l, isAppend m.change = true) →
    l.foldl stepL b = b ++ appendedOf l
  | [], b, _ => by simp [appendedOf]
  | m :: l, b, h => by
    have hm := h m (by simp)
    rw [List.foldl_cons, foldl_appends l _ (fun x hx => h x (by simp [hx]))]
    cases hc : m.change with
    | Append more => simp [stepL, applyL, hc, appendedOf, appendBytes]
    | At a more => rw [hc] at hm; simp [isAppend] at hm
    | Sectors a more => rw [hc] at hm; simp [isAppend] at hm
    | Cut len => rw [hc] at hm; simp [isAppend] at hm

/-- Small appends add at most `2^40` bytes each. -/
theorem appendedOf_length : ∀ (l : List Made), (∀ m ∈ l, ChangeSmall m.change) →
    (appendedOf l).length ≤ 1099511627776 * l.length
  | [], _ => by simp [appendedOf]
  | m :: l, h => by
    have hm := h m (by simp)
    have ih := appendedOf_length l (fun x hx => h x (by simp [hx]))
    have hb : (appendBytes m.change).length ≤ 1099511627776 := by
      cases hc : m.change with
      | Append more => rw [hc] at hm; exact hm
      | At a more => simp [appendBytes]
      | Sectors a more => simp [appendBytes]
      | Cut len => simp [appendBytes]
    simp only [appendedOf, List.flatMap_cons, List.length_append, List.length_cons] at ih ⊢
    omega

/-- The appends' bytes split where the list does. -/
theorem appendedOf_append (l1 l2 : List Made) :
    appendedOf (l1 ++ l2) = appendedOf l1 ++ appendedOf l2 := by
  simp [appendedOf, List.flatMap_append]

/-- Every change of a small, only-appended file is `ApplyOk`. -/
theorem append_ok (f : File) (hs : Small f) (ha : AllAppend f) :
    ∀ m ∈ f.changes.val, ApplyOk m.change := by
  intro m hm
  have hsm := hs.2.2.2 m hm
  have hap := ha m hm
  cases hc : m.change with
  | Append more => rw [hc] at hsm; exact apply_ok_append more hsm
  | At a more => rw [hc] at hap; simp [isAppend] at hap
  | Sectors a more => rw [hc] at hap; simp [isAppend] at hap
  | Cut len => rw [hc] at hap; simp [isAppend] at hap

/-- 4. `File::contents` of a small file whose changes are all appends, with `2^66` fuel: its
durable bytes and then the appended bytes, in order. -/
theorem contents_appended (fuel : Nat) (f : File) (hs : Small f) (ha : AllAppend f)
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ v, File.contents fuel f = Result.ok v ∧ v.val = f.durable.val ++ appended f := by
  obtain ⟨v, hv, hvv⟩ := contents_spec fuel f hs (append_ok f hs ha) hfuel
  exact ⟨v, hv, by rw [hvv, foldl_appends _ _ ha]; rfl⟩

/-- 5. `File::sync` of a small file whose changes are all appends, with `2^66` fuel: of
`c = min covers changes.len()`, the first `c` appends' bytes are added to the durable bytes,
the rest of the changes are left in order, the name is kept, the undo goes exactly when no
change is left, and every barrier left is one of the file's that covered more than `c`, now
covering `c` fewer (so with `covers ≥ changes.len()` none that covered at most
`changes.len()` is left). -/
theorem sync_appended (fuel : Nat) (f : File) (covers : Usize) (hs : Small f) (ha : AllAppend f)
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ r, File.sync fuel f covers = Result.ok r ∧ r.name = f.name ∧
      r.durable.val = f.durable.val ++
        appendedOf (f.changes.val.take (min covers.val.toNat f.changes.val.length)) ∧
      r.changes.val = f.changes.val.drop (min covers.val.toNat f.changes.val.length) ∧
      r.undo = (if f.changes.val.drop (min covers.val.toNat f.changes.val.length) = [] then none
        else f.undo) ∧
      ∀ x ∈ r.barriers.val,
        FromBarrier f.barriers.val ((min covers.val.toNat f.changes.val.length : Nat) : Int) x := by
  obtain ⟨r, hr, h1, h2, h3, h4, h5⟩ := sync_spec fuel f covers hs (append_ok f hs ha) hfuel
  refine ⟨r, hr, h1, ?_, h3, h4, h5⟩
  rw [h2, foldl_appends _ _ (fun m hm => ha m (List.mem_of_mem_take hm))]

/-- A sync that covers every change leaves no barrier that covered at most all of them. -/
theorem sync_appended_barriers (fuel : Nat) (f : File) (covers : Usize) (hs : Small f)
    (ha : AllAppend f) (hfuel : 73786976294838206464 ≤ fuel)
    (hall : (f.changes.val.length : Int) ≤ covers.val) :
    ∃ r, File.sync fuel f covers = Result.ok r ∧
      ∀ x ∈ r.barriers.val, ∃ b ∈ f.barriers.val, (f.changes.val.length : Int) < b.covers.val := by
  obtain ⟨r, hr, -, -, -, -, h5⟩ := sync_appended fuel f covers hs ha hfuel
  refine ⟨r, hr, fun x hx => ?_⟩
  obtain ⟨b, hb, hlt, -, -⟩ := h5 x hx
  exact ⟨b, hb, by omega⟩

/-! ## 6 and 7. A crash's cut -/

/-- What `File::fits` says of `whole` and `torn`: no more changes kept whole than the file has,
and a torn part only of a change there is. -/
theorem fits_facts (f : File) (k : Kept) (h : File.fits f k = Result.ok true) :
    k.whole.val.toNat ≤ f.changes.val.length ∧
      (k.torn ≠ Torn.Nothing → k.whole.val.toNat < f.changes.val.length) := by
  rcases k with ⟨w, torn, u⟩
  have hw0 := w.hmin
  simp only [ScalarTy.min] at hw0
  by_cases hgt : (f.changes.val.length : Int) < w.val
  · simp [File.fits, Scalar.gt, hgt] at h
  · refine ⟨by simp only; omega, ?_⟩
    intro hne
    simp only at hne ⊢
    by_cases heq : w.val = (f.changes.val.length : Int)
    · cases torn with
      | Nothing => exact absurd rfl hne
      | Prefix c => simp [File.fits, Scalar.gt, Scalar.eq, heq] at h
      | Sectors l => simp [File.fits, Scalar.gt, Scalar.eq, heq] at h
    · omega

/-- What a crash keeps of a torn append is a prefix of what the file read as. -/
theorem prefix_torn (D A M R : List U8) (c : Nat) :
    (D ++ A) ++ M.take c <+: D ++ (A ++ (M ++ R)) := by
  rw [List.append_assoc]
  apply (List.prefix_append_right_inj D).mpr
  apply (List.prefix_append_right_inj A).mpr
  exact (List.take_prefix c M).trans (List.prefix_append M R)

/-- What a crash keeps of the appends kept whole is a prefix of what the file read as. -/
theorem prefix_whole (D A X : List U8) : D ++ A <+: D ++ (A ++ X) := by
  rw [← List.append_assoc]
  exact List.prefix_append _ _

/-- 6. THEOREM A. A crash cuts a small file that was only appended to, with `2^66` fuel and a
`Kept` that fits it, to a prefix of what it read as, and never below its durable bytes: the
file keeps its name and has no change and nothing to undo left. -/
theorem cut_appended (fuel : Nat) (f : File) (k : Kept) (hs : Small f) (ha : AllAppend f)
    (hfits : File.fits f k = Result.ok true) (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ f', File.cut fuel f k = Result.ok f' ∧ f'.name = f.name ∧ f'.changes.val = [] ∧
      f'.undo = none ∧ f.durable.val <+: f'.durable.val ∧
      f'.durable.val <+: f.durable.val ++ appended f := by
  obtain ⟨hw, htorn⟩ := fits_facts f k hfits
  have hsm := hs.2.2.2
  have hd := hs.1
  have hcl := hs.2.1
  obtain ⟨r, hr, h1, h2, h3, -, -⟩ := sync_appended fuel f k.whole hs ha hfuel
  have hmin : min k.whole.val.toNat f.changes.val.length = k.whole.val.toNat := by omega
  rw [hmin] at h2 h3
  have hsplit : appended f = appendedOf (f.changes.val.take k.whole.val.toNat) ++
      appendedOf (f.changes.val.drop k.whole.val.toNat) := by
    rw [← appendedOf_append, List.take_append_drop]; rfl
  have hA := appendedOf_length (f.changes.val.take k.whole.val.toNat)
    (fun m hm => hsm m (List.mem_of_mem_take hm))
  have htl : (f.changes.val.take k.whole.val.toNat).length ≤ 65536 := by
    simp only [List.length_take]; omega
  rcases r with ⟨rn, rd, ⟨rcl, rch⟩, rb, ru⟩
  simp only at h1 h2 h3
  rw [hsplit]
  cases rcl with
  | nil =>
    refine ⟨⟨rn, rd, Vec.new, Vec.new, none⟩, ?_, h1, rfl, rfl, ?_, ?_⟩
    · simp [File.cut, hr, Vec.clear]
    · show f.durable.val <+: rd.val
      rw [h2]; exact List.prefix_append _ _
    · show rd.val <+: _
      rw [h2]; exact prefix_whole _ _ _
  | cons m rest =>
    have hmd : m ∈ f.changes.val.drop k.whole.val.toNat := by rw [← h3]; simp
    have hap := ha m (List.mem_of_mem_drop hmd)
    have hrest : appendedOf (f.changes.val.drop k.whole.val.toNat) =
        appendBytes m.change ++ appendedOf rest := by
      rw [← h3]; simp [appendedOf]
    rw [hrest]
    cases hc : m.change with
    | At a more => rw [hc] at hap; simp [isAppend] at hap
    | Sectors a more => rw [hc] at hap; simp [isAppend] at hap
    | Cut len => rw [hc] at hap; simp [isAppend] at hap
    | Append more =>
      have hml : more.val.length ≤ 1099511627776 := by
        have := hsm m (List.mem_of_mem_drop hmd); rw [hc] at this; exact this
      simp only [appendBytes]
      cases ht : k.torn with
      | Nothing =>
        refine ⟨⟨rn, rd, Vec.new, Vec.new, none⟩, ?_, h1, rfl, rfl, ?_, ?_⟩
        · simp [File.cut, hr, ht, Vec.clear, Vec.remove, usize_zero]
        · show f.durable.val <+: rd.val
          rw [h2]; exact List.prefix_append _ _
        · show rd.val <+: _
          rw [h2]; exact prefix_whole _ _ _
      | Sectors l =>
        refine ⟨⟨rn, rd, Vec.new, Vec.new, none⟩, ?_, h1, rfl, rfl, ?_, ?_⟩
        · simp [File.cut, hr, ht, hc, Vec.clear, Vec.remove, usize_zero]
        · show f.durable.val <+: rd.val
          rw [h2]; exact List.prefix_append _ _
        · show rd.val <+: _
          rw [h2]; exact prefix_whole _ _ _
      | Prefix count =>
        have hrd : rd.val.length ≤ 1099511627776 + 1099511627776 * 65536 := by
          rw [h2, List.length_append]
          have := Nat.mul_le_mul_left 1099511627776 htl
          omega
        obtain ⟨x3, hx3, hx3v⟩ := extend_spec fuel rd more count (by omega) (by omega)
        refine ⟨⟨rn, x3, Vec.new, Vec.new, none⟩, ?_, h1, rfl, rfl, ?_, ?_⟩
        · simp [File.cut, hr, ht, hc, Vec.clear, Vec.remove, usize_zero, hx3]
        · show f.durable.val <+: x3.val
          rw [hx3v, h2, List.append_assoc]; exact List.prefix_append _ _
        · show x3.val <+: _
          rw [hx3v, h2]; exact prefix_torn _ _ _ _ _

/-- 7. THEOREM B. A crash keeps a small file with no change not synced, with `2^66` fuel and a
`Kept` that fits it: the `Kept` keeps no change and tears none, and the file after the crash
has exactly its durable bytes, its name, no change and nothing to undo. -/
theorem cut_synced (fuel : Nat) (f : File) (k : Kept) (hs : Small f) (hc : f.changes.val = [])
    (hfits : File.fits f k = Result.ok true) (hfuel : 73786976294838206464 ≤ fuel) :
    k.whole.val = 0 ∧ k.torn = Torn.Nothing ∧
      ∃ f', File.cut fuel f k = Result.ok f' ∧ f'.durable.val = f.durable.val ∧
        f'.name = f.name ∧ f'.changes.val = [] ∧ f'.undo = none := by
  obtain ⟨hw, htorn⟩ := fits_facts f k hfits
  rw [hc] at hw htorn
  have hw0 := k.whole.hmin
  simp only [ScalarTy.min] at hw0
  simp only [List.length_nil] at hw htorn
  have ha : AllAppend f := by intro m hm; rw [hc] at hm; simp at hm
  obtain ⟨f', hf', h1, h2, h3, h4, h5⟩ := cut_appended fuel f k hs ha hfits hfuel
  have happ : appended f = [] := by simp [appended, hc, appendedOf]
  rw [happ, List.append_nil] at h5
  refine ⟨by omega, ?_, f', hf', (h4.eq_of_length ?_).symm, h1, h2, h3⟩
  · cases ht : k.torn with
    | Nothing => rfl
    | Prefix c =>
      have := htorn (by rw [ht]; simp)
      omega
    | Sectors l =>
      have := htorn (by rw [ht]; simp)
      omega
  · have := h4.length_le
    have := h5.length_le
    omega

/-! ## 2, continued: `Change::apply` of the other kinds -/

/-- The zeros `write`'s first loop adds: `bytes` grown with zeros to `e`. -/
theorem write_zeros (e : Usize) :
    ∀ (fuel : Nat) (bytes : Vec U8), e.val.toNat - bytes.val.length < fuel →
      ∃ v, write.loop_0 fuel bytes e = Result.ok v ∧
        v.val = bytes.val ++ List.replicate (e.val.toNat - bytes.val.length) (0 : U8) := by
  intro fuel
  induction fuel with
  | zero => intro _ h; omega
  | succ n ih =>
    intro bytes hfuel
    have he := e.hmax
    simp only [ScalarTy.max] at he
    have he0 := e.hmin
    simp only [ScalarTy.min] at he0
    by_cases hlt : (bytes.val.length : Int) < e.val
    · have hpush : bytes.val.length < 18446744073709551615 := by omega
      have hb1 : (bytes.val ++ [(0 : U8)]).length ≤ 18446744073709551615 := by
        simp only [List.length_append, List.length_singleton]; omega
      obtain ⟨v, hv, hvv⟩ := ih ⟨bytes.val ++ [(0 : U8)], hb1⟩
        (by simp only [List.length_append, List.length_singleton]; omega)
      refine ⟨v, by simp [write.loop_0, Scalar.lt, hlt, hpush, hv], ?_⟩
      rw [hvv]
      have hk : e.val.toNat - bytes.val.length =
          (e.val.toNat - (bytes.val ++ [(0 : U8)]).length) + 1 := by
        simp only [List.length_append, List.length_singleton]; omega
      rw [hk, List.replicate_succ]
      simp
    · refine ⟨bytes, by simp [write.loop_0, Scalar.lt, hlt], ?_⟩
      have hz : e.val.toNat - bytes.val.length = 0 := by omega
      rw [hz]
      simp

/-- The zeros `resize`'s loop adds: `bytes` grown with zeros to `e`. -/
theorem resize_zeros (e : Usize) :
    ∀ (fuel : Nat) (bytes : Vec U8), e.val.toNat - bytes.val.length < fuel →
      ∃ v, resize.loop_0 fuel bytes e = Result.ok v ∧
        v.val = bytes.val ++ List.replicate (e.val.toNat - bytes.val.length) (0 : U8) := by
  intro fuel
  induction fuel with
  | zero => intro _ h; omega
  | succ n ih =>
    intro bytes hfuel
    have he := e.hmax
    simp only [ScalarTy.max] at he
    have he0 := e.hmin
    simp only [ScalarTy.min] at he0
    by_cases hlt : (bytes.val.length : Int) < e.val
    · have hpush : bytes.val.length < 18446744073709551615 := by omega
      have hb1 : (bytes.val ++ [(0 : U8)]).length ≤ 18446744073709551615 := by
        simp only [List.length_append, List.length_singleton]; omega
      obtain ⟨v, hv, hvv⟩ := ih ⟨bytes.val ++ [(0 : U8)], hb1⟩
        (by simp only [List.length_append, List.length_singleton]; omega)
      refine ⟨v, by simp [resize.loop_0, Scalar.lt, hlt, hpush, hv], ?_⟩
      rw [hvv]
      have hk : e.val.toNat - bytes.val.length =
          (e.val.toNat - (bytes.val ++ [(0 : U8)]).length) + 1 := by
        simp only [List.length_append, List.length_singleton]; omega
      rw [hk, List.replicate_succ]
      simp
    · refine ⟨bytes, by simp [resize.loop_0, Scalar.lt, hlt], ?_⟩
      have hz : e.val.toNat - bytes.val.length = 0 := by omega
      rw [hz]
      simp

/-- One byte of `M` written in place: the step of `write`'s second loop, as lists. -/
theorem set_step (G M : List U8) (a i : Nat) (hi : i < M.length) (h : a + i < G.length) :
    (G.take a ++ (M.take i ++ G.drop (a + i))).set (a + i) M[i] =
      G.take a ++ (M.take (i + 1) ++ G.drop (a + i + 1)) := by
  have hta : (G.take a).length = a := by simp only [List.length_take]; omega
  have hti : (M.take i).length = i := by simp only [List.length_take]; omega
  rw [List.drop_eq_getElem_cons h, List.take_succ_eq_append_getElem hi]
  generalize G[a + i] = x
  generalize G.drop (a + i + 1) = R
  rw [List.set_append_right _ _ (by omega), hta, Nat.add_sub_cancel_left,
    List.set_append_right _ _ (by omega), hti, Nat.sub_self]
  simp only [List.set_cons_zero, List.append_assoc, List.singleton_append]

/-- `write`'s second loop from `i`, with `from = 0` and `to = more.len()`, over bytes at least
`at + more.len()` long: `more` written at `at`. -/
theorem write_loop1 (a : Usize) (more : Slice U8) (to : Usize)
    (hto : to.val = (more.val.length : Int)) (G : List U8)
    (hG : a.val.toNat + more.val.length ≤ G.length) :
    ∀ (fuel : Nat) (bytes : Vec U8) (i : Usize), i.val.toNat ≤ more.val.length →
      more.val.length - i.val.toNat < fuel →
      bytes.val = G.take a.val.toNat ++ (more.val.take i.val.toNat ++
        G.drop (a.val.toNat + i.val.toNat)) →
      ∃ v, write.loop_1 fuel bytes a more 0 to i = Result.ok v ∧
        v.val = G.take a.val.toNat ++ (more.val ++ G.drop (a.val.toNat + more.val.length)) := by
  intro fuel
  induction fuel with
  | zero => intro _ i _ h; omega
  | succ n ih =>
    intro bytes i hi hfuel hb
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have ha0 := a.hmin
    simp only [ScalarTy.min] at ha0
    have hGl : G.length ≤ 18446744073709551615 := by
      have := bytes.hlen
      rw [hb] at this
      simp only [List.length_append, List.length_take, List.length_drop] at this
      omega
    have hbl : bytes.val.length = G.length := by
      rw [hb]
      simp only [List.length_append, List.length_take, List.length_drop]
      omega
    by_cases hlt : i.val < to.val
    · have hil : i.val.toNat < more.val.length := by omega
      obtain ⟨t31, ht31, ht31v⟩ : ∃ t : Usize, Scalar.sub i (0 : Usize) = Result.ok t ∧
          t.val = i.val :=
        ⟨_, Scalar.sub_ok i 0 ⟨by rw [usize_zero]; simp only [ScalarTy.min]; omega,
          by rw [usize_zero]; simp only [ScalarTy.max]; omega⟩, by simp only; rw [usize_zero]; omega⟩
      obtain ⟨t29, ht29, ht29v⟩ : ∃ t : Usize, Scalar.add a t31 = Result.ok t ∧
          t.val = a.val + i.val :=
        ⟨_, Scalar.add_ok a t31 ⟨by simp only [ScalarTy.min]; omega,
          by simp only [ScalarTy.max]; omega⟩, by simp only; omega⟩
      have hidx : t29.val.toNat < bytes.val.length := by omega
      have hidx' : t29.val.toNat = a.val.toNat + i.val.toNat := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hs1 : (bytes.val.set t29.val.toNat more.val[i.val.toNat]).length ≤
          18446744073709551615 := by simp only [List.length_set]; exact bytes.hlen
      obtain ⟨v, hv, hvv⟩ := ih ⟨bytes.val.set t29.val.toNat more.val[i.val.toNat], hs1⟩ j
        (by omega) (by omega)
        (by
          simp only
          rw [hb, hidx', set_step G more.val a.val.toNat i.val.toNat hil (by omega)]
          have hj' : j.val.toNat = i.val.toNat + 1 := by omega
          rw [hj', Nat.add_assoc])
      refine ⟨v, ?_, hvv⟩
      simp [write.loop_1, Scalar.lt, Slice.index, Slice.update, hlt, hil, ht31, ht29, hidx, hj, hv]
    · refine ⟨bytes, by simp [write.loop_1, Scalar.lt, hlt], ?_⟩
      have hie : i.val.toNat = more.val.length := by omega
      rw [hb, hie, List.take_of_length_le (Nat.le_refl _)]

/-- `write` of all of `more` at `a`, with `2^65` fuel, on bytes at most `2^62` long and
`a + more.len()` at most `2^41`: `writeL`. -/
theorem write_spec (fuel : Nat) (bytes : Vec U8) (a : Usize) (more : Slice U8)
    (hfuel : 36893488147419103232 ≤ fuel) (hb : bytes.val.length ≤ 4611686018427387904)
    (ha : a.val ≤ 1099511627776) (hm : more.val.length ≤ 1099511627776) :
    ∃ v, write fuel bytes a more 0 (Slice.len more) = Result.ok v ∧
      v.val = writeL bytes.val a.val.toNat more.val := by
  have ha0 := a.hmin
  simp only [ScalarTy.min] at ha0
  obtain ⟨t8, ht8, ht8v⟩ : ∃ t : Usize, Scalar.sub (Slice.len more) (0 : Usize) = Result.ok t ∧
      t.val = (more.val.length : Int) :=
    ⟨_, Scalar.sub_ok _ 0 ⟨by rw [usize_zero]; simp only [ScalarTy.min, Slice.len_val]; omega,
      by rw [usize_zero]; simp only [ScalarTy.max, Slice.len_val]; omega⟩,
      by simp only [Slice.len_val]; rw [usize_zero]; omega⟩
  obtain ⟨e, he, hev⟩ : ∃ t : Usize, Scalar.add a t8 = Result.ok t ∧
      t.val = a.val + (more.val.length : Int) :=
    ⟨_, Scalar.add_ok a t8 ⟨by simp only [ScalarTy.min]; omega,
      by simp only [ScalarTy.max]; omega⟩, by simp only; omega⟩
  obtain ⟨g, hg, hgv⟩ := write_zeros e fuel bytes (by omega)
  have hgl : a.val.toNat + more.val.length ≤ g.val.length := by
    rw [hgv]; simp only [List.length_append, List.length_replicate]; omega
  obtain ⟨v, hv, hvv⟩ := write_loop1 a more (Slice.len more) (Slice.len_val more) g.val hgl fuel
    g 0 (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
    (by rw [usize_zero]; simp)
  refine ⟨v, ?_, ?_⟩
  · simp only [write]
    rw [ht8, Result.bind_ok, he, Result.bind_ok, hg, Result.bind_ok]
    exact hv
  · rw [hvv, hgv]
    have hen : e.val.toNat = a.val.toNat + more.val.length := by omega
    rw [hen]
    simp only [writeL, List.append_assoc]

/-- `resize` to `len`, with `2^65` fuel and `len` at most `2^41`: the first `len` bytes, grown
with zeros to `len`. -/
theorem resize_spec (fuel : Nat) (bytes : Vec U8) (len : Usize)
    (hfuel : 36893488147419103232 ≤ fuel) (hl : len.val ≤ 1099511627776) :
    ∃ v, resize fuel bytes len = Result.ok v ∧
      v.val = bytes.val.take len.val.toNat ++
        List.replicate (len.val.toNat - bytes.val.length) (0 : U8) := by
  obtain ⟨v, hv, hvv⟩ := resize_zeros len fuel (Vec.truncate bytes len)
    (by simp only [Vec.truncate_val, List.length_take]; omega)
  refine ⟨v, by simp only [resize]; exact hv, ?_⟩
  rw [hvv]
  simp only [Vec.truncate_val, List.length_take]
  congr 2
  omega

/-- Every small change is `ApplyOk`. -/
theorem apply_ok (c : Change) (h : ChangeSmall c) : ApplyOk c := by
  cases c with
  | Append more => exact apply_ok_append more h
  | At a more =>
    have h' : a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776 := h
    intro n hn b hb
    obtain ⟨v, hv, hvv⟩ := write_spec n b a more hn hb h'.1 h'.2
    exact ⟨v, by simp only [Change.apply]; exact hv, hvv⟩
  | Sectors a more =>
    have h' : a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776 := h
    intro n hn b hb
    obtain ⟨v, hv, hvv⟩ := write_spec n b a more hn hb h'.1 h'.2
    exact ⟨v, by simp only [Change.apply]; exact hv, hvv⟩
  | Cut len =>
    have h' : len.val ≤ 1099511627776 := h
    intro n hn b _
    obtain ⟨v, hv, hvv⟩ := resize_spec n b len hn h'
    exact ⟨v, by simp only [Change.apply]; exact hv, hvv⟩

/-- 7, companion. `sync_all`: a sync of a small file that covers every change, with `2^66`
fuel, leaves it with no change, nothing to undo and the same name, and its durable bytes are
what a read of it saw before. -/
theorem sync_all (fuel : Nat) (f : File) (covers : Usize) (hs : Small f)
    (hall : (f.changes.val.length : Int) ≤ covers.val) (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ r v, File.sync fuel f covers = Result.ok r ∧ File.contents fuel f = Result.ok v ∧
      r.changes.val = [] ∧ r.undo = none ∧ r.name = f.name ∧ r.durable.val = v.val := by
  have hok : ∀ m ∈ f.changes.val, ApplyOk m.change := fun m hm => apply_ok _ (hs.2.2.2 m hm)
  obtain ⟨r, hr, h1, h2, h3, h4, -⟩ := sync_spec fuel f covers hs hok hfuel
  obtain ⟨v, hv, hvv⟩ := contents_spec fuel f hs hok hfuel
  have hmin : min covers.val.toNat f.changes.val.length = f.changes.val.length := by omega
  rw [hmin, List.drop_length] at h3 h4
  rw [hmin, List.take_length] at h2
  refine ⟨r, v, hr, hv, h3, by rw [h4]; rfl, h1, by rw [h2, hvv]⟩

end BlazeCore.World.DiskBytes
