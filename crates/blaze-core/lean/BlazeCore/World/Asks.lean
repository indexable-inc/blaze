import BlazeCore.World.AsksLib

/-!
A synced append survives a crash (written by claude-opus-5-5, lane asksproof).

The run: `wf` and `small` hold, the first append is asked and told done, the disk is still
small, the file is there, only appended to and with nothing to undo, and its contents are
`before`; the sync is asked (tag 1, owing every unsynced change), then each `between` step is
asked and told (tag 2), then the sync is told it covered `covers`, and then a crash.

The invariant between steps (`Inv`): the asks are exactly the sync, owing `o`; the file is at a
place `p` of a disk whose names strictly increase, with nothing to undo, every change an append,
`o` at most its changes, and `before` a prefix of its durable bytes and the bytes of its first
`o` changes. Each turn of the loop checks `small` again, so between turns nothing more is kept.

Fuel: `2^68`. The loop over `between` takes up to `2^64` turns, and the sync in the last turn
and the crash need `2^67` fuel left.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.World.Asks

open blaze_core.world.disk blaze_core.world.asks
open BlazeCore.World.DiskAllows (add_one usize_zero u64_zero allows_total)
open BlazeCore.World.DiskMap (Sorted find_spec wf_spec small_spec)
open BlazeCore.World.DiskBytes
open BlazeCore.World.DiskCrash (Bounded small_bounded small_files sorted_set uadd usub crash_spec
  contents_of_mem with_undo_none allows_fits)
open BlazeCore.World.AsksLib

/-! ## 1. Literals and the sync's ask -/

theorem u64_one : (1 : U64).val = 1 :=
  Scalar.ofNat_val .U64 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

theorem u64_two : (2 : U64).val = 2 :=
  Scalar.ofNat_val .U64 2 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

theorem usize_one : (1 : Usize).val = 1 :=
  Scalar.ofNat_val .Usize 1 (by simp only [ScalarTy.min, ScalarTy.max]; omega)

/-- The sync asked first: tag 1, owing `o`. -/
def S1 (file : U64) (o : Usize) : Asked := { tag := 1, ask := Ask.Sync file, owed := o }

/-! ## 2. `World::find` on the shapes the run has -/

theorem find_nil (n : Nat) (w : World) (t : U64) (hw : w.asked.val = []) :
    World.find (n + 1) w t = Result.ok none := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw; subst hw
  simp [World.find, World.find.loop_0, Scalar.lt, Slice.len_val, usize_zero]

theorem find_one_ne (n : Nat) (w : World) (t : U64) (x : Asked) (hw : w.asked.val = [x])
    (hx : x.tag.val ≠ t.val) : World.find (n + 2) w t = Result.ok none := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw; subst hw
  obtain ⟨j, hj, hjv⟩ := add_one (0 : Usize) (by rw [usize_zero]; omega)
  rw [usize_zero] at hjv
  simp [World.find, World.find.loop_0, Scalar.lt, Slice.len_val, usize_zero, Slice.index,
    Scalar.eq, hx, hj, hjv]

theorem find_one_eq (n : Nat) (w : World) (t : U64) (x : Asked) (hw : w.asked.val = [x])
    (hx : x.tag.val = t.val) : World.find (n + 1) w t = Result.ok (some 0) := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw; subst hw
  simp [World.find, World.find.loop_0, Scalar.lt, Slice.len_val, usize_zero, Slice.index,
    Scalar.eq, hx]

theorem find_two (n : Nat) (w : World) (t : U64) (x y : Asked) (hw : w.asked.val = [x, y])
    (hx : x.tag.val ≠ t.val) (hy : y.tag.val = t.val) :
    ∃ j : Usize, World.find (n + 2) w t = Result.ok (some j) ∧ j.val = 1 := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw; subst hw
  obtain ⟨j, hj, hjv⟩ := add_one (0 : Usize) (by rw [usize_zero]; omega)
  rw [usize_zero] at hjv
  refine ⟨j, ?_, by omega⟩
  have hj1 : j.val.toNat = 1 := by omega
  simp [World.find, World.find.loop_0, Scalar.lt, Slice.len_val, usize_zero, Slice.index,
    Scalar.eq, hx, hj, hjv, hj1, hy]

/-! ## 3. `World::ask` of a tag not asked -/

theorem ask_append (fuel : Nat) (w : World) (tag : U64) (file : U64) (bytes : Vec U8)
    (hf : World.find fuel w tag = Result.ok none)
    (hl : w.asked.val.length < 18446744073709551615) :
    ∃ w', World.ask fuel w tag (Ask.Append file bytes) = Result.ok (true, w') ∧
      w'.disk = w.disk ∧ w'.asked.val = w.asked.val ++ [⟨tag, Ask.Append file bytes, 0⟩] := by
  refine ⟨⟨w.disk, ⟨w.asked.val ++ [⟨tag, Ask.Append file bytes, 0⟩], by
    simp only [List.length_append, List.length_singleton]; omega⟩⟩, ?_, rfl, rfl⟩
  simp [World.ask, hf, synced_name, hl]

theorem ask_sync (fuel : Nat) (w : World) (tag : U64) (file : U64) (u : Usize)
    (hf : World.find fuel w tag = Result.ok none) (hu : unsynced fuel w.disk file = Result.ok u)
    (hl : w.asked.val.length < 18446744073709551615) :
    ∃ w', World.ask fuel w tag (Ask.Sync file) = Result.ok (true, w') ∧
      w'.disk = w.disk ∧ w'.asked.val = w.asked.val ++ [⟨tag, Ask.Sync file, u⟩] := by
  refine ⟨⟨w.disk, ⟨w.asked.val ++ [⟨tag, Ask.Sync file, u⟩], by
    simp only [List.length_append, List.length_singleton]; omega⟩⟩, ?_, rfl, rfl⟩
  simp [World.ask, hf, synced_name, hl, hu]

/-! ## 4. `World::tell`'s loops with the sync left -/

theorem loop1_one (n : Nat) (w : World) (file : U64) (o : Usize) (kept : Vec Usize)
    (t after : Usize) (hw : w.asked.val = [S1 file o]) (hk : kept.val = [t])
    (hu : unsynced (n + 1) w.disk file = Result.ok after) :
    ∃ w' : World, ∃ o' : Usize, World.tell.loop_1 (n + 2) w kept 0 = Result.ok w' ∧
      w'.disk = w.disk ∧ w'.asked.val = [S1 file o'] ∧
      o'.val.toNat = o.val.toNat - (t.val.toNat - after.val.toNat) := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  obtain ⟨K, hK⟩ := kept
  simp only at hw hk hu; subst hw hk
  obtain ⟨j, hj, hjv⟩ := add_one (0 : Usize) (by rw [usize_zero]; omega)
  rw [usize_zero] at hjv
  have ht0 := t.hmin; have ha0 := after.hmin; have ho0 := o.hmin
  simp only [ScalarTy.min] at ht0 ha0 ho0
  by_cases hg : after.val < t.val
  · obtain ⟨dv, hdv, hdvv⟩ := usub t after (by omega)
    have hd0 := dv.hmin
    simp only [ScalarTy.min] at hd0
    by_cases hg2 : dv.val < o.val
    · obtain ⟨o', ho', ho'v⟩ := usub o dv (by omega)
      refine ⟨⟨D, ⟨[S1 file o'], by simp⟩⟩, o', ?_, rfl, rfl, by omega⟩
      simp [World.tell.loop_1, Scalar.lt, Slice.len_val, usize_zero, Slice.index, S1, synced_name,
        hu, Scalar.gt, hg, hdv, hg2, ho', Slice.update, hj, hjv]
    · refine ⟨⟨D, ⟨[S1 file 0], by simp⟩⟩, 0, ?_, rfl, rfl, by rw [usize_zero]; omega⟩
      simp [World.tell.loop_1, Scalar.lt, Slice.len_val, usize_zero, Slice.index, S1, synced_name,
        hu, Scalar.gt, hg, hdv, hg2, Slice.update, hj, hjv]
  · by_cases hg2 : 0 < o.val
    · obtain ⟨o', ho', ho'v⟩ := usub o 0 (by rw [usize_zero]; omega)
      rw [usize_zero] at ho'v
      refine ⟨⟨D, ⟨[S1 file o'], by simp⟩⟩, o', ?_, rfl, rfl, by omega⟩
      simp [World.tell.loop_1, Scalar.lt, Slice.len_val, usize_zero, Slice.index, S1, synced_name,
        hu, Scalar.gt, hg, hg2, ho', Slice.update, hj, hjv]
    · refine ⟨⟨D, ⟨[S1 file 0], by simp⟩⟩, 0, ?_, rfl, rfl, by rw [usize_zero]; omega⟩
      simp [World.tell.loop_1, Scalar.lt, Slice.len_val, usize_zero, Slice.index, S1, synced_name,
        hu, Scalar.gt, hg, hg2, Slice.update, hj, hjv]

theorem loop0_one (n : Nat) (w : World) (file : U64) (o : Usize) (told : Told) (x : Asked)
    (u a t : Usize) (w2 w3 : World) (hw : w.asked.val = [S1 file o])
    (hd : done told = false ∨ renames x.ask file = Result.ok false)
    (hthere : there (n + 1) w.disk file = Result.ok true)
    (hu : unsynced (n + 1) w.disk file = Result.ok u)
    (hadd : added (n + 1) x.ask told file true = Result.ok a)
    (hsum : Scalar.add u a = Result.ok t)
    (hc : World.complete n w x.ask told = Result.ok w2)
    (h1 : ∀ kept : Vec Usize, kept.val = [t] → World.tell.loop_1 n w2 kept 0 = Result.ok w3) :
    World.tell.loop_0 (n + 2) w told x Vec.new 0 = Result.ok (true, w3) := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw hthere hu hc; subst hw
  obtain ⟨j, hj, hjv⟩ := add_one (0 : Usize) (by rw [usize_zero]; omega)
  rw [usize_zero] at hjv
  have hk := h1 ⟨[t], by simp⟩ rfl
  simp only [S1] at hc
  have hp : Vec.push (Vec.new : Vec Usize) t = Result.ok ⟨[t], by simp⟩ := by
    rw [Vec.push_ok _ _ (by simp)]; rfl
  cases hdt : done told
  · simp [World.tell.loop_0, Scalar.lt, Slice.len_val, usize_zero, Slice.index, S1, synced_name,
      hdt, hthere, hu, hadd, hsum, hj, hjv, hc, hp, hk]
  · have hr : renames x.ask file = Result.ok false := hd.resolve_left (by simp [hdt])
    simp [World.tell.loop_0, Scalar.lt, Slice.len_val, usize_zero, Slice.index, S1, synced_name,
      hdt, hr, hthere, hu, hadd, hsum, hj, hjv, hc, hp, hk]

theorem loop0_nil (n : Nat) (w : World) (told : Told) (x : Asked) (w2 : World)
    (hw : w.asked.val = []) (hc : World.complete (n + 1) w x.ask told = Result.ok w2)
    (hw2 : w2.asked.val = []) :
    World.tell.loop_0 (n + 2) w told x Vec.new 0 = Result.ok (true, w2) := by
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  obtain ⟨D2, ⟨L2, hl2⟩⟩ := w2
  simp only at hw hw2 hc; subst hw hw2
  simp [World.tell.loop_0, World.tell.loop_1, Scalar.lt, Slice.len_val, usize_zero, hc]

theorem tell_no (fuel : Nat) (w : World) (tag : U64) (told : Told)
    (ha : World.allows fuel w tag told = Result.ok false) :
    World.tell fuel w tag told = Result.ok (false, w) := by
  simp [World.tell, ha]

theorem tell_go (fuel : Nat) (w : World) (tag : U64) (told : Told) (at' : Usize)
    (hat : at'.val.toNat < w.asked.val.length)
    (ha : World.allows fuel w tag told = Result.ok true)
    (hf : World.find fuel w tag = Result.ok (some at')) (r : Bool × World)
    (h : ∀ w1 : World, w1.disk = w.disk → w1.asked.val = w.asked.val.eraseIdx at'.val.toNat →
      World.tell.loop_0 fuel w1 told (w.asked.val[at'.val.toNat]) Vec.new 0 = Result.ok r) :
    World.tell fuel w tag told = Result.ok r := by
  have hk := h ⟨w.disk, ⟨w.asked.val.eraseIdx at'.val.toNat, by
    have := w.asked.hlen; rw [List.length_eraseIdx_of_lt hat]; omega⟩⟩ rfl rfl
  simp [World.tell, ha, hf, hat, hk]

/-! ## 5. The disk facts the asks read -/

theorem find_at (m : Nat) (D : Disk) (file : U64) (p : Nat) (hp : p < D.files.val.length)
    (hs : Sorted D.files.val) (hn : (D.files.val[p]).name.val = file.val)
    (hm : D.files.val.length < m) :
    ∃ q : Usize, Disk.find m D file = Result.ok (some q) ∧ q.val.toNat = p := by
  obtain ⟨r, hr, hnone, hsome⟩ := find_spec m D file hm
  cases r with
  | none => exact absurd hn (hnone rfl p hp)
  | some q =>
    obtain ⟨hq, hqn, -⟩ := hsome q rfl
    exact ⟨q, hr, hs.unique _ _ hq hp (hqn.trans hn.symm)⟩

theorem unsynced_at (m : Nat) (D : Disk) (file : U64) (p : Nat) (hp : p < D.files.val.length)
    (hs : Sorted D.files.val) (hn : (D.files.val[p]).name.val = file.val)
    (hm : D.files.val.length < m) :
    ∃ u : Usize, unsynced m D file = Result.ok u ∧
      u.val = ((D.files.val[p]).changes.val.length : Int) := by
  obtain ⟨q, hq, hqp⟩ := find_at m D file p hp hs hn hm
  subst hqp
  exact ⟨_, by simp [unsynced, hq, Slice.index_ok D.files q hp], Slice.len_val _⟩

theorem there_at (m : Nat) (D : Disk) (file : U64) (p : Nat) (hp : p < D.files.val.length)
    (hs : Sorted D.files.val) (hn : (D.files.val[p]).name.val = file.val)
    (hm : D.files.val.length < m) : there m D file = Result.ok true := by
  obtain ⟨q, hq, -⟩ := find_at m D file p hp hs hn hm
  simp [there, hq]

/-! ## 6. The invariant -/

/-- What `Disk::small` checking `true` gives. -/
def SmallD (D : Disk) : Prop :=
  D.files.val.length ≤ 65536 ∧ ∀ k (hk : k < D.files.val.length), Small D.files.val[k]

theorem smallD_of (n : Nat) (D : Disk) (hn : 36893488147419103232 ≤ n)
    (h : Disk.small n D = Result.ok true) : SmallD D := by
  obtain ⟨b, hb, hbs⟩ := small_spec n D hn
  rw [h] at hb
  cases hb
  exact ⟨(hbs rfl).1, small_files n D h⟩

/-- The disk between steps: the file at `p`, names strictly increasing, nothing to undo, only
appends, `o` at most its changes, and `before` a prefix of its durable bytes and the bytes of
its first `o` changes. -/
def DInv (file : U64) (before : List U8) (o : Nat) (D : Disk) : Prop :=
  Sorted D.files.val ∧ ∃ p, ∃ hp : p < D.files.val.length,
    (D.files.val[p]).name.val = file.val ∧ (D.files.val[p]).undo = none ∧
    AllAppend D.files.val[p] ∧ o ≤ (D.files.val[p]).changes.val.length ∧
    before <+: (D.files.val[p]).durable.val ++ appendedOf ((D.files.val[p]).changes.val.take o)

/-- The world between steps: the sync is the only ask. -/
def Inv (file : U64) (before : List U8) (w : World) : Prop :=
  ∃ o : Usize, w.asked.val = [S1 file o] ∧ DInv file before o.val.toNat w.disk

/-- The disk after the last sync: `before` is a prefix of the file's durable bytes. -/
def Final (file : U64) (before : List U8) (D : Disk) : Prop :=
  Sorted D.files.val ∧ D.files.val.length ≤ 65536 ∧
    (∀ k (hk : k < D.files.val.length), Bounded D.files.val[k]) ∧
    ∃ p, ∃ hp : p < D.files.val.length, (D.files.val[p]).name.val = file.val ∧
      (D.files.val[p]).undo = none ∧ AllAppend D.files.val[p] ∧
      before <+: (D.files.val[p]).durable.val

theorem pre_mono (before D : List U8) (l : List Made) (o k : Nat) (h : o ≤ k)
    (hp : before <+: D ++ appendedOf (l.take o)) : before <+: D ++ appendedOf (l.take k) := by
  have e : k = o + (k - o) := by omega
  rw [e, appendedOf_take_add, ← List.append_assoc]
  exact hp.trans (List.prefix_append _ _)

/-! ## 7. `World::allows` on the shapes the run has -/

theorem allows_append (n : Nat) (w : World) (file : U64) (o : Usize) (b : Vec U8) (told : Told)
    (hw : w.asked.val = [S1 file o, ⟨2, Ask.Append file b, 0⟩]) :
    ∃ c, World.allows (n + 2) w 2 told = Result.ok c ∧ (told = Told.Done → c = true) := by
  obtain ⟨j, hf, hj⟩ := find_two n w 2 _ _ hw (by simp [S1, u64_one, u64_two]) rfl
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw hf; subst hw
  have hj1 : j.val.toNat = 1 := by omega
  cases told <;> simp [World.allows, hf, Slice.index, hj1, hj]

theorem allows_sync2 (n : Nat) (w : World) (file : U64) (o u c u' : Usize)
    (hw : w.asked.val = [S1 file o, ⟨2, Ask.Sync file, u⟩])
    (hu : unsynced (n + 2) w.disk file = Result.ok u') :
    ∃ b, World.allows (n + 2) w 2 (Told.Covered c) = Result.ok b ∧
      (b = true → u.val ≤ c.val ∧ c.val ≤ u'.val) := by
  obtain ⟨j, hf, hj⟩ := find_two n w 2 _ _ hw (by simp [S1, u64_one, u64_two]) rfl
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw hf hu; subst hw
  have hj1 : j.val.toNat = 1 := by omega
  by_cases h1 : u.val ≤ c.val
  · by_cases h2 : c.val ≤ u'.val
    · exact ⟨true, by simp [World.allows, hf, Slice.index, hj1, hj, Scalar.le, h1, h2, hu],
        fun _ => ⟨h1, h2⟩⟩
    · exact ⟨false, by simp [World.allows, hf, Slice.index, hj1, hj, Scalar.le, h1, h2, hu],
        by simp⟩
  · exact ⟨false, by simp [World.allows, hf, Slice.index, hj1, hj, Scalar.le, h1], by simp⟩

theorem allows_final (n : Nat) (w : World) (file : U64) (o c u' : Usize)
    (hw : w.asked.val = [S1 file o]) (hu : unsynced (n + 1) w.disk file = Result.ok u') :
    ∃ b, World.allows (n + 1) w 1 (Told.Covered c) = Result.ok b ∧
      (b = true → o.val ≤ c.val ∧ c.val ≤ u'.val) := by
  have hf := find_one_eq n w 1 _ hw rfl
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw hf hu; subst hw
  simp only [S1] at hf
  by_cases h1 : o.val ≤ c.val
  · by_cases h2 : c.val ≤ u'.val
    · exact ⟨true, by simp [World.allows, hf, Slice.index, usize_zero, S1, Scalar.le, h1, h2, hu],
        fun _ => ⟨h1, h2⟩⟩
    · exact ⟨false, by
        simp [World.allows, hf, Slice.index, usize_zero, S1, Scalar.le, h1, h2, hu], by simp⟩
  · exact ⟨false, by simp [World.allows, hf, Slice.index, usize_zero, S1, Scalar.le, h1], by simp⟩

theorem allows0 (n : Nat) (w : World) (file : U64) (bytes : Vec U8)
    (hw : w.asked.val = [⟨0, Ask.Append file bytes, 0⟩]) :
    World.allows (n + 1) w 0 Told.Done = Result.ok true := by
  have hf := find_one_eq n w 0 _ hw rfl
  obtain ⟨D, ⟨L, hl⟩⟩ := w
  simp only at hw hf; subst hw
  simp [World.allows, hf, Slice.index, usize_zero]

/-! ## 8. Telling each step -/

/-- The first append told done: the asks are left empty and the names still increase. -/
theorem tell0 (n : Nat) (w : World) (file : U64) (bytes : Vec U8)
    (hw : w.asked.val = [⟨0, Ask.Append file bytes, 0⟩]) (hs : Sorted w.disk.files.val)
    (hL : w.disk.files.val.length ≤ 65536)
    (hc : ∀ k (hk : k < w.disk.files.val.length), (w.disk.files.val[k]).changes.val.length ≤ 65536)
    (hn : 65536 < n) :
    ∃ w', World.tell (n + 4) w 0 Told.Done = Result.ok (true, w') ∧ w'.asked.val = [] ∧
      Sorted w'.disk.files.val := by
  obtain ⟨D1, happ, hs1⟩ := append_sorted (n + 3) w.disk file bytes hs (by omega) (by omega)
    (fun k hk => by have := hc k hk; omega)
  have hf := find_one_eq (n + 3) w 0 _ hw rfl
  have ha := allows0 (n + 3) w file bytes hw
  refine ⟨⟨D1, Vec.new⟩, tell_go (n + 4) w 0 Told.Done 0 (by rw [hw, usize_zero]; simp) ha hf _
    (fun w1 hw1d hw1a => ?_), rfl, hs1⟩
  have hx : w.asked.val[(0 : Usize).val.toNat]'(by rw [hw, usize_zero]; simp) =
      ⟨0, Ask.Append file bytes, 0⟩ := by simp [hw, usize_zero]
  rw [hx]
  have hw1 : w1.asked.val = [] := by rw [hw1a, hw, usize_zero]; simp
  refine loop0_nil (n + 2) w1 Told.Done _ _ hw1 ?_ rfl
  simp [World.complete, hw1d, happ]
  obtain ⟨D, ⟨L, hl⟩⟩ := w1
  simp only at hw1
  subst hw1
  rfl

/-- An append between told done or failed after some bytes: the invariant stays, with the
same `o`. -/
theorem tell_append_core (n : Nat) (w : World) (file : U64) (o : Usize) (b b' : Vec U8)
    (told : Told) (before : List U8)
    (hw : w.asked.val = [S1 file o, ⟨2, Ask.Append file b, 0⟩])
    (hI : DInv file before o.val.toNat w.disk) (hS : SmallD w.disk) (hfuel : 65536 < n)
    (hallow : World.allows (n + 8) w 2 told = Result.ok true)
    (hdone : done told = false ∨ renames (Ask.Append file b) file = Result.ok false)
    (hadd : added (n + 7) (Ask.Append file b) told file true = Result.ok 1)
    (hcomp : ∀ W1 : World, W1.disk = w.disk →
      World.complete (n + 6) W1 (Ask.Append file b) told =
        (do let x ← Disk.append (n + 6) w.disk file b'; Result.ok { W1 with disk := x })) :
    ∃ w', World.tell (n + 8) w 2 told = Result.ok (true, w') ∧ Inv file before w' := by
  obtain ⟨hs, p, hp, hpn, hpu, hpa, hop, hpre⟩ := hI
  obtain ⟨hL, hSm⟩ := hS
  obtain ⟨-, hcl, -, -⟩ := hSm p hp
  obtain ⟨D', happ, -, cs, hcs, hfs⟩ := append_there (n + 6) w.disk file b' p hs hp hpn
    (by omega) (by omega) (by omega)
  have hlen' : D'.files.val.length = w.disk.files.val.length := by rw [hfs, List.length_set]
  have hp' : p < D'.files.val.length := by omega
  have hD'p : D'.files.val[p] = { w.disk.files.val[p] with changes := cs } := by
    simp [hfs]
  have hs' : Sorted D'.files.val := by
    rw [hfs]; exact sorted_set hs p _ hp rfl
  have hpn' : (D'.files.val[p]).name.val = file.val := by rw [hD'p]; exact hpn
  obtain ⟨u, hu, huv⟩ := unsynced_at (n + 7) w.disk file p hp hs hpn (by omega)
  obtain ⟨t, ht, htv⟩ := uadd u 1 (by rw [huv, usize_one]; omega)
  obtain ⟨u', hu', hu'v⟩ := unsynced_at (n + 5) D' file p hp' hs' hpn' (by omega)
  have hcsl : cs.val.length = (w.disk.files.val[p]).changes.val.length + 1 := by
    rw [hcs]; simp
  have hu'v2 : u'.val = u.val + 1 := by
    rw [hu'v, huv]; simp only [hD'p, hcsl]; omega
  obtain ⟨j, hf, hj⟩ := find_two (n + 6) w 2 _ _ hw (by simp [S1, u64_one, u64_two]) rfl
  have hj1 : j.val.toNat = 1 := by omega
  obtain ⟨w3, o', hl1, hd3, ha3, ho'⟩ := loop1_one (n + 4) ⟨D', ⟨[S1 file o], by simp⟩⟩
    file o ⟨[t], by simp⟩ t u' rfl rfl hu'
  have hjl : j.val.toNat < w.asked.val.length := by rw [hw]; simp; omega
  refine ⟨w3, tell_go (n + 8) w 2 told j hjl hallow hf _ (fun w1 hw1d hw1a => ?_), o', ha3, ?_⟩
  · have hx : w.asked.val[j.val.toNat]'hjl = ⟨2, Ask.Append file b, 0⟩ := by simp [hw, hj1]
    rw [hx]
    have hw1 : w1.asked.val = [S1 file o] := by rw [hw1a, hw, hj1]; simp
    refine loop0_one (n + 6) w1 file o told _ u 1 t { w1 with disk := D' } w3 hw1 hdone
      (by rw [hw1d]; exact there_at _ _ _ p hp hs hpn (by omega)) (by rw [hw1d]; exact hu) hadd ht
      (by show World.complete (n + 6) w1 (Ask.Append file b) told = _
          rw [hcomp w1 hw1d, happ]; rfl) (fun kept hk => ?_)
    obtain ⟨D1, ⟨L1, hl1'⟩⟩ := w1
    simp only at hw1; subst hw1
    obtain ⟨K, hK⟩ := kept
    simp only at hk; subst hk
    exact hl1
  · have hoo : o'.val.toNat = o.val.toNat := by
      rw [ho']; rw [usize_one] at htv; omega
    simp only at hd3
    rw [hd3, hoo]
    refine ⟨hs', p, hp', hpn', by rw [hD'p]; exact hpu, ?_, by rw [hD'p]; show _ ≤ cs.val.length; omega,
      ?_⟩
    · intro m hm
      rw [hD'p] at hm
      change m ∈ cs.val at hm
      rw [hcs, List.mem_append, List.mem_singleton] at hm
      rcases hm with hm | hm
      · exact hpa m hm
      · rw [hm]; rfl
    · rw [hD'p]
      show before <+: (w.disk.files.val[p]).durable.val ++ appendedOf (cs.val.take o.val.toNat)
      rw [hcs, List.take_append_of_le_length hop]
      exact hpre

theorem tell_append_done (n : Nat) (w : World) (file : U64) (o : Usize) (b : Vec U8)
    (before : List U8) (hw : w.asked.val = [S1 file o, ⟨2, Ask.Append file b, 0⟩])
    (hI : DInv file before o.val.toNat w.disk) (hS : SmallD w.disk) (hfuel : 65536 < n) :
    ∃ w', World.tell (n + 8) w 2 Told.Done = Result.ok (true, w') ∧ Inv file before w' := by
  obtain ⟨c, hc, hct⟩ := allows_append (n + 6) w file o b Told.Done hw
  rw [hct rfl] at hc
  exact tell_append_core n w file o b b Told.Done before hw hI hS hfuel hc
    (Or.inr (by simp [renames])) (by simp [added, Scalar.eq])
    (fun W1 h => by rw [← h]; rfl)

theorem tell_append_failed (n : Nat) (w : World) (file : U64) (o : Usize) (b : Vec U8)
    (m : Usize) (before : List U8) (hw : w.asked.val = [S1 file o, ⟨2, Ask.Append file b, 0⟩])
    (hI : DInv file before o.val.toNat w.disk) (hS : SmallD w.disk)
    (hfuel : 18446744073709551616 < n) :
    ∃ r w', World.tell (n + 8) w 2 (Told.FailedAfter m) = Result.ok (r, w') ∧
      (r = true → Inv file before w') := by
  obtain ⟨c, hc, -⟩ := allows_append (n + 6) w file o b (Told.FailedAfter m) hw
  cases c with
  | false => exact ⟨false, w, tell_no _ _ _ _ hc, by simp⟩
  | true =>
    have hbl := b.hlen
    obtain ⟨v, hv, -⟩ := prefix_loop b m (n + 6) Vec.new 0 (by rw [usize_zero]; omega)
      (by rw [usize_zero]; simp) (by rw [usize_zero]; omega)
    obtain ⟨w', h1, h2⟩ := tell_append_core n w file o b v (Told.FailedAfter m) before hw hI hS
      (by omega) hc (Or.inl rfl) (by simp [added, Scalar.eq])
      (fun W1 h => by
        rw [← h]
        show (do let t1 ← «prefix» (n + 6) b m; let x1 ← Disk.append (n + 6) W1.disk file t1
                 Result.ok { W1 with disk := x1 }) = _
        simp only [«prefix»] at hv ⊢
        rw [hv]; rfl)
    exact ⟨true, w', h1, fun _ => h2⟩

/-- A sync between, asked owing every unsynced change and told it covered `c`: allowed only when
`c` is all of them, and then the file has no change left and the sync owes nothing. -/
theorem tell_sync (n : Nat) (w : World) (file : U64) (o u c : Usize) (before : List U8)
    (hw : w.asked.val = [S1 file o, ⟨2, Ask.Sync file, u⟩])
    (hu : ∀ p (hp : p < w.disk.files.val.length), (w.disk.files.val[p]).name.val = file.val →
      u.val = ((w.disk.files.val[p]).changes.val.length : Int))
    (hI : DInv file before o.val.toNat w.disk) (hS : SmallD w.disk)
    (hfuel : 147573952589676412928 ≤ n) :
    ∃ r w', World.tell (n + 8) w 2 (Told.Covered c) = Result.ok (r, w') ∧
      (r = true → Inv file before w') := by
  obtain ⟨hs, p, hp, hpn, hpu, hpa, hop, hpre⟩ := hI
  obtain ⟨hL, hSm⟩ := hS
  have huv := hu p hp hpn
  obtain ⟨u8, hu8, hu8v⟩ := unsynced_at (n + 8) w.disk file p hp hs hpn (by omega)
  obtain ⟨b, hb, hbt⟩ := allows_sync2 (n + 6) w file o u c u8 hw hu8
  cases b with
  | false => exact ⟨false, w, tell_no _ _ _ _ hb, by simp⟩
  | true =>
  obtain ⟨hc1, hc2⟩ := hbt rfl
  obtain ⟨D', hsync, hl', hB', hn', hG⟩ := disk_sync_grown (n + 6) w.disk file p c hs hL
    (fun k hk => small_bounded _ (hSm k hk)) hp hpn hpa hpu (by omega)
  have hA' := grown_append hpa hG
  have hp' : p < D'.files.val.length := by omega
  have hs' : Sorted D'.files.val := by
    intro i j hi hj hij
    rw [hn' i (by omega), hn' j (by omega)]
    exact hs i j (by omega) (by omega) hij
  obtain ⟨hGn, hGu, k, hk1, hk2, hGd, hGc⟩ := hG
  have hk : k = (w.disk.files.val[p]).changes.val.length := by omega
  subst hk
  have hpn' : (D'.files.val[p]).name.val = file.val := by rw [hGn]; exact hpn
  obtain ⟨u', hu', hu'v⟩ := unsynced_at (n + 5) D' file p hp' hs' hpn' (by omega)
  have hu'0 : u'.val = 0 := by rw [hu'v, hGc]; simp
  obtain ⟨u7, hu7, hu7v⟩ := unsynced_at (n + 7) w.disk file p hp hs hpn (by omega)
  obtain ⟨-, hcl, -, -⟩ := hSm p hp
  obtain ⟨t, ht, htv⟩ := uadd u7 0 (by rw [hu7v, usize_zero]; omega)
  rw [usize_zero] at htv
  obtain ⟨j, hf, hj⟩ := find_two (n + 6) w 2 _ _ hw (by simp [S1, u64_one, u64_two]) rfl
  have hj1 : j.val.toNat = 1 := by omega
  obtain ⟨w3, o', hl1, hd3, ha3, ho'⟩ := loop1_one (n + 4) ⟨D', ⟨[S1 file o], by simp⟩⟩
    file o ⟨[t], by simp⟩ t u' rfl rfl hu'
  have hjl : j.val.toNat < w.asked.val.length := by rw [hw]; simp; omega
  refine ⟨true, w3, tell_go (n + 8) w 2 _ j hjl hb hf _ (fun w1 hw1d hw1a => ?_),
    fun _ => ⟨o', ha3, ?_⟩⟩
  · have hx : w.asked.val[j.val.toNat]'hjl = ⟨2, Ask.Sync file, u⟩ := by simp [hw, hj1]
    rw [hx]
    have hw1 : w1.asked.val = [S1 file o] := by rw [hw1a, hw, hj1]; simp
    refine loop0_one (n + 6) w1 file o _ _ u7 0 t { w1 with disk := D' } w3 hw1 (Or.inl rfl)
      (by rw [hw1d]; exact there_at _ _ _ p hp hs hpn (by omega)) (by rw [hw1d]; exact hu7)
      (by simp [added]) ht
      (by show World.complete (n + 6) w1 (Ask.Sync file) (Told.Covered c) = _
          simp [World.complete, hw1d, hsync]) (fun kept hk => ?_)
    obtain ⟨D1, ⟨L1, hl1'⟩⟩ := w1
    simp only at hw1; subst hw1
    obtain ⟨K, hK⟩ := kept
    simp only at hk; subst hk
    exact hl1
  · have hoo : o'.val.toNat = 0 := by rw [ho']; omega
    simp only at hd3
    rw [hd3, hoo]
    refine ⟨hs', p, hp', hpn', hGu, hA', Nat.zero_le _, ?_⟩
    rw [List.take_zero, hGd]
    simp only [appendedOf, List.flatMap_nil, List.append_nil]
    exact pre_mono _ _ _ _ _ hop hpre

/-- The sync told it covered `c`: allowed only when it covers what it owes and no more than the
file's changes, and then `before` is a prefix of the file's durable bytes. -/
theorem tell_final (n : Nat) (w : World) (file : U64) (o c : Usize) (before : List U8)
    (hw : w.asked.val = [S1 file o]) (hI : DInv file before o.val.toNat w.disk)
    (hS : SmallD w.disk) (hfuel : 147573952589676412928 ≤ n) :
    ∃ r w', World.tell (n + 8) w 1 (Told.Covered c) = Result.ok (r, w') ∧
      (r = true → w'.asked.val = [] ∧ Final file before w'.disk) := by
  obtain ⟨hs, p, hp, hpn, hpu, hpa, hop, hpre⟩ := hI
  obtain ⟨hL, hSm⟩ := hS
  obtain ⟨u8, hu8, hu8v⟩ := unsynced_at (n + 8) w.disk file p hp hs hpn (by omega)
  obtain ⟨b, hb, hbt⟩ := allows_final (n + 7) w file o c u8 hw hu8
  cases b with
  | false => exact ⟨false, w, tell_no _ _ _ _ hb, by simp⟩
  | true =>
  obtain ⟨hc1, hc2⟩ := hbt rfl
  obtain ⟨D', hsync, hl', hB', hn', hG⟩ := disk_sync_grown (n + 7) w.disk file p c hs hL
    (fun k hk => small_bounded _ (hSm k hk)) hp hpn hpa hpu (by omega)
  have hA' := grown_append hpa hG
  have hp' : p < D'.files.val.length := by omega
  have hs' : Sorted D'.files.val := by
    intro i j hi hj hij
    rw [hn' i (by omega), hn' j (by omega)]
    exact hs i j (by omega) (by omega) hij
  obtain ⟨hGn, hGu, k, hk1, hk2, hGd, hGc⟩ := hG
  have hf := find_one_eq (n + 7) w 1 _ hw rfl
  have hjl : (0 : Usize).val.toNat < w.asked.val.length := by rw [hw, usize_zero]; simp
  refine ⟨true, ⟨D', Vec.new⟩, tell_go (n + 8) w 1 _ 0 hjl hb hf _ (fun w1 hw1d hw1a => ?_),
    fun _ => ⟨rfl, ?_⟩⟩
  · have hx : w.asked.val[(0 : Usize).val.toNat]'hjl = S1 file o := by simp [hw, usize_zero]
    rw [hx]
    have hw1 : w1.asked.val = [] := by rw [hw1a, hw, usize_zero]; simp
    refine loop0_nil (n + 6) w1 _ _ _ hw1 ?_ rfl
    show World.complete (n + 7) w1 (Ask.Sync file) (Told.Covered c) = _
    simp [World.complete, hw1d, hsync]
    obtain ⟨D1, ⟨L1, hl1⟩⟩ := w1
    simp only at hw1
    subst hw1
    rfl
  · show Final file before D'
    refine ⟨hs', by omega, fun k hk => hB' k (by omega), p, hp', by rw [hGn]; exact hpn, hGu,
      hA', ?_⟩
    rw [hGd]
    exact pre_mono _ _ _ _ _ (by omega) hpre

/-! ## 9. The crash -/

theorem lands_nil (n : Nat) (w : World) (landings : Slice Landing) (hw : w.asked.val = [])
    (hl : landings.val = []) : World.lands (n + 1) w landings = Result.ok true := by
  obtain ⟨L, hL⟩ := landings
  simp only at hl; subst hl
  simp [World.lands, World.lands.loop_0, Scalar.lt, Slice.len_val, usize_zero]

theorem lands_cons (n : Nat) (w : World) (landings : Slice Landing) (hw : w.asked.val = [])
    (hl : landings.val ≠ []) : World.lands (n + 1) w landings = Result.ok false := by
  obtain ⟨L, hL⟩ := landings
  obtain ⟨D, ⟨A, hA⟩⟩ := w
  simp only at hl hw; subst hw
  cases L with
  | nil => exact absurd rfl hl
  | cons x xs =>
    have := x.ask.hmin
    simp only [ScalarTy.min] at this
    simp [World.lands, World.lands.loop_0, Scalar.lt, Slice.len_val, usize_zero, Slice.index,
      Scalar.ge, this]

theorem landed_nil (n : Nat) (w : World) (landings : Slice Landing) (hw : w.asked.val = [])
    (hl : landings.val = []) (hn : 147573952589676412928 ≤ n) :
    World.landed (n + 1) w landings = Result.ok (some w.disk) := by
  have h1 := lands_nil n w landings hw hl
  have h2 := copy_disk_id (n + 1) w.disk (by omega)
  obtain ⟨L, hL⟩ := landings
  simp only at hl h1; subst hl
  simp [World.landed, h1, h2, World.landed.loop_0, Scalar.lt, Slice.len_val, usize_zero]

theorem landed_cons (n : Nat) (w : World) (landings : Slice Landing) (hw : w.asked.val = [])
    (hl : landings.val ≠ []) : World.landed (n + 1) w landings = Result.ok none := by
  simp [World.landed, lands_cons n w landings hw hl]

/-- After the last sync, the crash: not allowed, or allowed and `before` survives it. -/
theorem tail (n : Nat) (w : World) (file : U64) (before : Vec U8) (landings : Slice Landing)
    (cut : Slice Kept) (hw : w.asked.val = []) (hF : Final file before.val w.disk)
    (hfuel : 147573952589676412928 ≤ n) :
    World.allows_crash (n + 1) w landings cut = Result.ok false ∨
      (World.allows_crash (n + 1) w landings cut = Result.ok true ∧
        ∃ w'', World.crash (n + 1) w landings cut = Result.ok w'' ∧
          ∃ v, Disk.contents (n + 1) w''.disk file = Result.ok v ∧
            survives (n + 1) before v = Result.ok true) := by
  by_cases hl : landings.val = []
  case neg =>
    left; simp [World.allows_crash, landed_cons n w landings hw hl]
  have hld := landed_nil n w landings hw hl hfuel
  obtain ⟨b, hb⟩ := allows_total (n + 1) w.disk cut (by omega)
  cases b with
  | false => left; simp [World.allows_crash, hld, hb]
  | true =>
  right
  refine ⟨by simp [World.allows_crash, hld, hb], ?_⟩
  obtain ⟨hs, hL, hB, p, hp, hpn, hpu, hpa, hpre⟩ := hF
  obtain ⟨hlc, -⟩ := allows_fits _ w.disk cut hb
  obtain ⟨d', hcr, hs', F, hF, hFn, hFc, hFd⟩ :=
    crash_spec (n + 1) w.disk cut
      (fun F => F.changes.val = [] ∧ before.val <+: F.durable.val)
      p hs hlc hL hB hp hpu
      (fun m hm => by
        rw [with_undo_none _ hpu]
        obtain ⟨f', h1, h2, h3⟩ := cut_bounded m _ (cut.val[p]'(by omega)) (hB p hp) hpa hm
        exact ⟨f', h1, h2, hpre.trans h3⟩)
      (by omega)
  have hFn' : F.name.val = file.val := by rw [hFn]; exact hpn
  obtain ⟨v, hcont, hv⟩ := contents_of_mem (n + 1) d' F file hs' hF hFn' hFc
    (by have := d'.files.hlen; omega)
  obtain ⟨b', hb', hbiff⟩ := starts_with_spec (n + 1) v before
    (by have := before.hlen; omega)
  have hbt : b' = true := hbiff.mpr (by rw [hv]; exact hFd)
  subst hbt
  refine ⟨{ w with disk := d', asked := Vec.clear w.asked }, ?_, some v, hcont, ?_⟩
  · simp [World.crash, hld, hb, hcr]
  · simp [survives, hb']

/-! ## 10. The loop over `between` -/

/-- One turn of the loop, on a step there: it ends with `none`, or leaves the invariant and
goes on at the next step. -/
theorem turn (n : Nat) (file : U64) (between : Slice Between) (covers : Option Usize)
    (landings : Slice Landing) (cut : Slice Kept) (w : World) (before : Vec U8) (i : Usize)
    (hI : Inv file before.val w) (hlt : i.val.toNat < between.val.length)
    (hfuel : 147573952589676412928 + 16 ≤ n) :
    synced_append.loop_0 (n + 1) file between covers landings cut w before i = Result.ok none ∨
      ∃ w' j, j.val = i.val + 1 ∧ Inv file before.val w' ∧
        synced_append.loop_0 (n + 1) file between covers landings cut w before i =
          synced_append.loop_0 n file between covers landings cut w' before j := by
  obtain ⟨o, hw, hI⟩ := hI
  have hi0 := i.hmin
  simp only [ScalarTy.min] at hi0
  have hiv : i.val < (between.val.length : Int) := by omega
  obtain ⟨j, hj, hjv⟩ := add_one i (by have := between.hlen; omega)
  obtain ⟨sb, hsb, -⟩ := small_spec n w.disk (by omega)
  cases sb with
  | false => left; simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb]
  | true =>
  have hS := smallD_of n w.disk (by omega) hsb
  obtain ⟨m, rfl⟩ : ∃ m, n = m + 8 := ⟨n - 8, by omega⟩
  have hl1 : w.asked.val.length < 18446744073709551615 := by rw [hw]; simp
  have hf2 := find_one_ne (m + 6) w 2 _ hw (by simp [S1, u64_one, u64_two])
  cases hx : between.val[i.val.toNat] with
  | Append b fa =>
    obtain ⟨w2, hask, hd2, ha2⟩ := ask_append (m + 8) w 2 file b hf2 hl1
    have hw2 : w2.asked.val = [S1 file o, ⟨2, Ask.Append file b, 0⟩] := by rw [ha2, hw]; rfl
    have hI2 : DInv file before.val o.val.toNat w2.disk := by rw [hd2]; exact hI
    have hS2 : SmallD w2.disk := by rw [hd2]; exact hS
    have hcp := copy_bytes_id (m + 8) b (by omega)
    cases fa with
    | none =>
      obtain ⟨w3, ht, hI3⟩ := tell_append_done m w2 file o b before.val hw2 hI2 hS2 (by omega)
      right
      refine ⟨w3, j, hjv, hI3, ?_⟩
      simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, Slice.index, hlt, hx, hcp,
        hask, ht, hj]
    | some k =>
      obtain ⟨r, w3, ht, hI3⟩ :=
        tell_append_failed m w2 file o b k before.val hw2 hI2 hS2 (by omega)
      cases r with
      | false =>
        left
        simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, Slice.index, hlt, hx, hcp,
          hask, ht]
      | true =>
        right
        refine ⟨w3, j, hjv, hI3 rfl, ?_⟩
        simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, Slice.index, hlt, hx, hcp,
          hask, ht, hj]
  | Sync c =>
    obtain ⟨hs, p, hp, hpn, -⟩ := id hI
    obtain ⟨u, hu, huv⟩ := unsynced_at (m + 8) w.disk file p hp hs hpn (by have := hS.1; omega)
    obtain ⟨w2, hask, hd2, ha2⟩ := ask_sync (m + 8) w 2 file u hf2 hu hl1
    have hw2 : w2.asked.val = [S1 file o, ⟨2, Ask.Sync file, u⟩] := by rw [ha2, hw]; rfl
    have hI2 : DInv file before.val o.val.toNat w2.disk := by rw [hd2]; exact hI
    have hS2 : SmallD w2.disk := by rw [hd2]; exact hS
    have hu2 : ∀ q (hq : q < w2.disk.files.val.length),
        (w2.disk.files.val[q]).name.val = file.val →
        u.val = ((w2.disk.files.val[q]).changes.val.length : Int) := by
      intro q hq hqn
      have hq' : q < w.disk.files.val.length := by rw [← hd2]; exact hq
      have e : q = p := hs.unique q p hq' hp (by
        have : (w2.disk.files.val[q]).name.val = (w.disk.files.val[q]'hq').name.val := by
          simp only [hd2]
        rw [← this, hqn, hpn])
      subst e
      rw [huv]
      simp only [hd2]
    obtain ⟨r, w3, ht, hI3⟩ := tell_sync m w2 file o u c before.val hw2 hu2 hI2 hS2 (by omega)
    cases r with
    | false =>
      left
      simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, Slice.index, hlt, hx,
        hask, ht]
    | true =>
      right
      refine ⟨w3, j, hjv, hI3 rfl, ?_⟩
      simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, Slice.index, hlt, hx,
        hask, ht, hj]

/-- The loop's last turn, past the steps: `none`, or `before` survives the crash. -/
theorem turn_end (n : Nat) (file : U64) (between : Slice Between) (covers : Usize)
    (landings : Slice Landing) (cut : Slice Kept) (w : World) (before : Vec U8) (i : Usize)
    (hI : Inv file before.val w) (hge : ¬ i.val.toNat < between.val.length)
    (hfuel : 147573952589676412928 + 16 ≤ n) :
    synced_append.loop_0 (n + 1) file between (some covers) landings cut w before i =
        Result.ok none ∨
      synced_append.loop_0 (n + 1) file between (some covers) landings cut w before i =
        Result.ok (some true) := by
  obtain ⟨o, hw, hI⟩ := hI
  have hiv : ¬ i.val < (between.val.length : Int) := by
    have := i.hmin; simp only [ScalarTy.min] at this; omega
  obtain ⟨sb, hsb, -⟩ := small_spec n w.disk (by omega)
  cases sb with
  | false => left; simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb]
  | true =>
  have hS := smallD_of n w.disk (by omega) hsb
  obtain ⟨m, rfl⟩ : ∃ m, n = m + 8 := ⟨n - 8, by omega⟩
  obtain ⟨r, w', ht, hF⟩ := tell_final m w file o covers before.val hw hI hS (by omega)
  cases r with
  | false =>
    left
    simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, ht]
  | true =>
  obtain ⟨hw', hFin⟩ := hF rfl
  rcases tail (m + 7) w' file before landings cut hw' hFin (by omega) with
    hac | ⟨hac, w'', hcr, v, hcont, hsv⟩
  · left
    simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, ht, hac]
  · right
    simp [synced_append.loop_0, Scalar.lt, Slice.len_val, hiv, hsb, ht, hac, hcr, hcont, hsv]

theorem loop_spec (file : U64) (between : Slice Between) (covers : Usize)
    (landings : Slice Landing) (cut : Slice Kept) (before : Vec U8) :
    ∀ (fuel : Nat) (w : World) (i : Usize), Inv file before.val w →
      i.val.toNat ≤ between.val.length →
      between.val.length - i.val.toNat + 147573952589676412928 + 32 < fuel →
      synced_append.loop_0 fuel file between (some covers) landings cut w before i =
          Result.ok none ∨
        synced_append.loop_0 fuel file between (some covers) landings cut w before i =
          Result.ok (some true) := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ h; omega
  | succ n ih =>
    intro w i hI hi hf
    by_cases hlt : i.val.toNat < between.val.length
    · rcases turn n file between (some covers) landings cut w before i hI hlt (by omega) with
        h | ⟨w', j, hjv, hI', heq⟩
      · exact Or.inl h
      · rw [heq]
        have := i.hmin
        simp only [ScalarTy.min] at this
        exact ih w' j hI' (by omega) (by omega)
    · exact turn_end n file between covers landings cut w before i hI hlt (by omega)

/-! ## 11. The whole run -/

theorem run (F : Nat) (hF : 295147905179352825856 ≤ F) (disk : Disk) (file : U64)
    (bytes : Vec U8) (between : Slice Between) (covers : Usize) (landings : Slice Landing)
    (cut : Slice Kept) :
    ∃ r, synced_append F disk file bytes between (some covers) landings cut = Result.ok r ∧
      (r = none ∨ r = some true) := by
  obtain ⟨n, rfl⟩ : ∃ n, F = n + 8 := ⟨F - 8, by omega⟩
  have hl0 := disk.files.hlen
  obtain ⟨bw, hwf, hwfs⟩ := wf_spec (n + 8) disk (by omega)
  obtain ⟨bs, hsm, hsms⟩ := small_spec (n + 8) disk (by omega)
  by_cases hgo : bw = true ∧ bs = true
  case neg =>
    refine ⟨none, ?_, Or.inl rfl⟩
    cases bw with
    | false => simp [synced_append, hwf]
    | true =>
      cases bs with
      | false => simp [synced_append, hwf, hsm]
      | true => exact absurd ⟨rfl, rfl⟩ hgo
  obtain ⟨hbw, hbs⟩ := hgo
  subst hbw hbs
  have hs := hwfs rfl
  obtain ⟨hL, hc⟩ := hsms rfl
  obtain ⟨W1, hask0, hd1, ha1⟩ := ask_append (n + 8) { disk := disk, asked := Vec.new } 0 file
    bytes (find_nil (n + 7) _ 0 rfl) (by simp)
  have hw1 : W1.asked.val = [⟨0, Ask.Append file bytes, 0⟩] := by rw [ha1]; rfl
  obtain ⟨W2, htell0, hw2, hs2⟩ := tell0 (n + 4) W1 file bytes hw1 (by rw [hd1]; exact hs)
    (by rw [hd1]; exact hL) (by rw [hd1]; exact fun k hk => (hc k hk).1) (by omega)
  obtain ⟨sb, hsb, -⟩ := small_spec (n + 8) W2.disk (by omega)
  cases sb with
  | false =>
    exact ⟨none, by simp [synced_append, hwf, hsm, hask0, htell0, hsb], Or.inl rfl⟩
  | true =>
  have hS := smallD_of _ _ (by omega) hsb
  obtain ⟨r0, hfind, -, hsome⟩ := find_spec (n + 8) W2.disk file (by have := hS.1; omega)
  cases r0 with
  | none =>
    exact ⟨none, by simp [synced_append, hwf, hsm, hask0, htell0, hsb, hfind], Or.inl rfl⟩
  | some q =>
  obtain ⟨hq, hqn, -⟩ := hsome q rfl
  have hix := Slice.index_ok W2.disk.files q hq
  have hSq := hS.2 _ hq
  obtain ⟨ba, hba, hbaiff⟩ := only_appended_spec (n + 8) W2.disk.files.val[q.val.toNat]
    (by have := hSq.2.1; omega)
  cases ba with
  | false =>
    exact ⟨none, by simp [synced_append, hwf, hsm, hask0, htell0, hsb, hfind, hix, hba],
      Or.inl rfl⟩
  | true =>
  cases hu : (W2.disk.files.val[q.val.toNat]).undo with
  | some x =>
    exact ⟨none, by simp [synced_append, hwf, hsm, hask0, htell0, hsb, hfind, hix, hba, hu],
      Or.inl rfl⟩
  | none =>
  have hA := hbaiff.mp rfl
  obtain ⟨v, hcont, hv⟩ := contents_appended (n + 8) _ hSq hA (by omega)
  obtain ⟨u, hu1, huv⟩ := unsynced_at (n + 8) W2.disk file q.val.toNat hq hs2 hqn
    (by have := hS.1; omega)
  obtain ⟨W3, hask1, hd3, ha3⟩ := ask_sync (n + 8) W2 1 file u (find_nil (n + 7) W2 1 hw2) hu1
    (by rw [hw2]; simp)
  have hI3 : Inv file v.val W3 := by
    refine ⟨u, by rw [ha3, hw2]; rfl, ?_⟩
    rw [hd3]
    have hut : u.val.toNat = (W2.disk.files.val[q.val.toNat]).changes.val.length := by omega
    refine ⟨hs2, q.val.toNat, hq, hqn, hu, hA, by omega, ?_⟩
    rw [hut, List.take_length, hv]
    exact List.prefix_refl _
  rcases loop_spec file between covers landings cut v (n + 8) W3 0 hI3
      (by rw [usize_zero]; omega) (by rw [usize_zero]; have := between.hlen; omega) with h | h
  · exact ⟨none, by simp [synced_append, hwf, hsm, hask0, htell0, hsb, hfind, hix, hba, hu,
      hcont, hask1, h], Or.inl rfl⟩
  · exact ⟨some true, by simp [synced_append, hwf, hsm, hask0, htell0, hsb, hfind, hix, hba, hu,
      hcont, hask1, h], Or.inr rfl⟩

end BlazeCore.World.Asks

open BlazeCore.World.Asks in
/-- A synced append survives a crash: with `2^68` fuel the run gives `true`. -/
theorem blaze_core.world.asks.a_synced_append_survives_a_crash.spec.proof :
    blaze_core.world.asks.a_synced_append_survives_a_crash.spec.statement := by
  intro disk file bytes between covers landings cut
  refine ⟨295147905179352825856, ?_⟩
  obtain ⟨r, hr, h⟩ := run 295147905179352825856 (Nat.le_refl _) disk file bytes between covers
    landings cut
  rcases h with rfl | rfl <;>
    simp [blaze_core.world.asks.a_synced_append_survives_a_crash, hr]
