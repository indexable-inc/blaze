import Blaze.Auto
import BlazeCore.World.DiskAllows
import BlazeCore.World.DiskMap
import BlazeCore.World.DiskBytes
import BlazeModel.blaze_core.world.disk.a_crash_cuts_an_appended_file_to_a_prefix
import BlazeModel.blaze_core.world.disk.a_crash_keeps_a_synced_file

/-!
`blaze_core::world::disk` (crates/blaze-core/src/world/disk.rs): the two crash theorems left,
proved over the generated models.

- `appended_proof`: on a well-formed, small disk, a crash the disk allows cuts a file whose
  changes are all appends and that has nothing to undo to a prefix: its durable bytes before are
  a prefix of what it holds after, and that a prefix of what it read before.
- `synced_proof`: on a well-formed, small disk, once a file is synced, a crash the disk allows
  keeps exactly what it read before the sync.

Fuel: `2^67` (`147573952589676412928`) for both.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.World.DiskCrash

open blaze_core.world.disk
open BlazeCore.World.DiskAllows (add_one usize_zero fits_total allows_spec allows_total)
open BlazeCore.World.DiskMap (Sorted find_spec slot_spec open_spec wf_spec small_spec)
open BlazeCore.World.DiskBytes

/-! ## 1. What `Disk::small` and `Disk::allows` say of each file -/

/-- The loop of `Disk::small` from `i`, when it gives `true` at any fuel: every file from `i`
on is `Small`. -/
theorem small_loop_files (d : Disk) : ∀ (fuel : Nat) (i : Usize),
    Disk.small.loop_0 fuel d i = Result.ok true →
    ∀ k (hk : k < d.files.val.length), i.val.toNat ≤ k → Small d.files.val[k] := by
  intro fuel
  induction fuel with
  | zero => intro i h; simp [Disk.small.loop_0] at h
  | succ n ih =>
    intro i h k hk hik
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := d.files.hlen
    have hlt : i.val.toNat < d.files.val.length := by omega
    have hltv : i.val < (d.files.val.length : Int) := by omega
    obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
    rcases hs : File.small n d.files.val[i.val.toNat] with b | e | _
    · cases b with
      | false => simp [Disk.small.loop_0, Slice.index, hltv, hlt, hs] at h
      | true =>
        have hrest : Disk.small.loop_0 n d j = Result.ok true := by
          simpa [Disk.small.loop_0, Slice.index, hltv, hlt, hs, hj] using h
        by_cases e : k = i.val.toNat
        · subst e
          exact small_sound n _ hs
        · exact ih j hrest k hk (by omega)
    · simp [Disk.small.loop_0, Slice.index, hltv, hlt, hs] at h
    · simp [Disk.small.loop_0, Slice.index, hltv, hlt, hs] at h

/-- `Disk::small` giving `true`, at any fuel: every file is `Small`. -/
theorem small_files (fuel : Nat) (d : Disk) (h : Disk.small fuel d = Result.ok true) :
    ∀ k (hk : k < d.files.val.length), Small d.files.val[k] := by
  have h1 : (65536 : Usize).val = 65536 :=
    Scalar.ofNat_val .Usize 65536 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
  by_cases hl : (65536 : Int) < (d.files.val.length : Int)
  · simp [Disk.small, h1, hl] at h
  · have h' : Disk.small.loop_0 fuel d 0 = Result.ok true := by
      simpa [Disk.small, h1, hl] using h
    intro k hk
    exact small_loop_files d fuel 0 h' k hk (by rw [usize_zero]; omega)

/-- The first loop of `Disk::allows` from `i`, when it gives `true` at any fuel: the `Kept` of
every file from `i` on fits it. -/
theorem allows_loop_fits (d : Disk) (cut : Slice Kept) : ∀ (fuel : Nat) (i : Usize),
    Disk.allows.loop_0 fuel d cut i = Result.ok true →
    ∀ k (hk : k < d.files.val.length) (hkc : k < cut.val.length), i.val.toNat ≤ k →
      File.fits d.files.val[k] cut.val[k] = Result.ok true := by
  intro fuel
  induction fuel with
  | zero => intro i h; simp [Disk.allows.loop_0] at h
  | succ n ih =>
    intro i h k hk hkc hik
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hlc := cut.hlen
    have hlt : i.val.toNat < cut.val.length := by omega
    have hif : i.val.toNat < d.files.val.length := by omega
    have hicv : i.val < (cut.val.length : Int) := by omega
    obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
    obtain ⟨bf, hbf⟩ := fits_total d.files.val[i.val.toNat] cut.val[i.val.toNat]
    cases bf with
    | false => simp [Disk.allows.loop_0, Slice.index, hif, hlt, hicv, hbf] at h
    | true =>
      have hrest : Disk.allows.loop_0 n d cut j = Result.ok true := by
        simpa [Disk.allows.loop_0, Slice.index, hif, hlt, hicv, hbf, hj] using h
      by_cases e : k = i.val.toNat
      · subst e
        exact hbf
      · exact ih j hrest k hk hkc (by omega)

/-- `Disk::allows` giving `true`, at any fuel: the cut has one `Kept` per file, and each fits
its file. -/
theorem allows_fits (fuel : Nat) (d : Disk) (cut : Slice Kept)
    (h : Disk.allows fuel d cut = Result.ok true) :
    cut.val.length = d.files.val.length ∧
      ∀ k (hk : k < d.files.val.length) (hkc : k < cut.val.length),
        File.fits d.files.val[k] cut.val[k] = Result.ok true := by
  by_cases hlen : (cut.val.length : Int) = (d.files.val.length : Int)
  · have h' : Disk.allows.loop_0 fuel d cut 0 = Result.ok true := by
      simpa [Disk.allows, hlen] using h
    exact ⟨by omega, fun k hk hkc =>
      allows_loop_fits d cut fuel 0 h' k hk hkc (by rw [usize_zero]; omega)⟩
  · simp [Disk.allows, hlen] at h

/-! ## 2. Files within the bounds a sync or a crash keeps -/

/-- What a file of a small disk stays within while syncs make its changes durable: its durable
bytes and `2^41` per change not synced together at most `2^59`, at most `2^16` changes, each
small. A `Small` file is `Bounded`, and so is any sync of a `Bounded` one. -/
def Bounded (f : File) : Prop :=
  f.durable.val.length + 2199023255552 * f.changes.val.length ≤ 576460752303423488 ∧
    f.changes.val.length ≤ 65536 ∧ ∀ m ∈ f.changes.val, ChangeSmall m.change

/-- A `Small` file is `Bounded`. -/
theorem small_bounded (f : File) (h : Small f) : Bounded f := by
  obtain ⟨hd, hc, _, hs⟩ := h
  exact ⟨by omega, hc, hs⟩

/-- Small changes made in order add at most `2^41` bytes each. -/
theorem foldl_length : ∀ (l : List Made) (b : List U8), (∀ m ∈ l, ChangeSmall m.change) →
    (l.foldl stepL b).length ≤ b.length + 2199023255552 * l.length
  | [], b, _ => by simp
  | m :: l, b, h => by
    have h1 := applyL_length m.change b (h m (by simp))
    have ih := foldl_length l (stepL b m) (fun x hx => h x (by simp [hx]))
    have e : (stepL b m).length = (applyL m.change b).length := rfl
    simp only [List.foldl_cons, List.length_cons]
    omega

/-- `File::sync` of a `Bounded` file, with `2^66` fuel: it never panics nor runs out of fuel,
keeps the name, and leaves a `Bounded` file; a file with no change keeps its durable bytes and
is left with no change and nothing to undo. -/
theorem sync_bounded (fuel : Nat) (f : File) (covers : Usize) (hb : Bounded f)
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ r, File.sync fuel f covers = Result.ok r ∧ r.name = f.name ∧ Bounded r ∧
      (f.changes.val = [] →
        r.durable.val = f.durable.val ∧ r.changes.val = [] ∧ r.undo = none) := by
  obtain ⟨hd, hcl, hsm⟩ := hb
  have hok : ∀ m ∈ f.changes.val, ApplyOk m.change := fun m hm => apply_ok _ (hsm m hm)
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
  obtain ⟨r1, hr1, b1, b2, b3, b4, -⟩ := sync_loop1 cv r0.barriers.val fuel r0 0
    (by rw [usize_zero]; omega) (by have := r0.barriers.hlen; rw [usize_zero]; omega)
    (fun k _ hk => by rw [usize_zero] at hk; omega)
    (fun k hk _ => List.getElem_mem hk)
  have hc : (cv.val - (0 : Usize).val).toNat = min covers.val.toNat f.changes.val.length := by
    rw [usize_zero]; omega
  rw [hc] at a4 a5
  have hchg : r1.changes.val = f.changes.val.drop (min covers.val.toNat f.changes.val.length) := by
    rw [b3]; exact a4
  have hdur : r1.durable.val =
      (f.changes.val.take (min covers.val.toNat f.changes.val.length)).foldl stepL
        f.durable.val := by
    rw [b2]; exact a5
  have hB : Bounded r1 := by
    have hfl := foldl_length (f.changes.val.take (min covers.val.toNat f.changes.val.length))
      f.durable.val (fun m hm => hsm m (List.mem_of_mem_take hm))
    rw [← hdur, List.length_take] at hfl
    refine ⟨?_, ?_, fun m hm => ?_⟩
    · rw [hchg, List.length_drop]
      have : 2199023255552 * min (min covers.val.toNat f.changes.val.length) f.changes.val.length
          + 2199023255552 * (f.changes.val.length - min covers.val.toNat f.changes.val.length) =
          2199023255552 * f.changes.val.length := by
        rw [← Nat.mul_add]; congr 1; omega
      omega
    · rw [hchg, List.length_drop]; omega
    · rw [hchg] at hm; exact hsm m (List.mem_of_mem_drop hm)
  by_cases hemp : r1.changes.val = []
  · refine ⟨{ r1 with undo := none }, ?_, by simp only; rw [b1, a1], hB, ?_⟩
    · simp only [File.sync, hcv_eq, hr0, hr1, Result.bind_ok]
      simp [hemp]
    · intro hn
      refine ⟨?_, hemp, rfl⟩
      simp only
      rw [hdur, hn]
      simp
  · refine ⟨r1, ?_, by rw [b1, a1], hB, ?_⟩
    · simp only [File.sync, hcv_eq, hr0, hr1, Result.bind_ok]
      simp [hemp]
    · intro hn
      rw [hchg, hn] at hemp
      simp at hemp

/-- `File::contents` of a file with no change is its durable bytes. -/
theorem contents_nil (fuel : Nat) (f : File) (hc : f.changes.val = []) (hf : 0 < fuel) :
    ∃ v, File.contents fuel f = Result.ok v ∧ v.val = f.durable.val := by
  have hcond : (Vec.new : Vec U8).val.length + f.durable.val.length ≤ 18446744073709551615 := by
    simp only [Vec.new_val, List.length_nil, Nat.zero_add]; exact f.durable.hlen
  obtain ⟨n, rfl⟩ : ∃ n, fuel = n + 1 := ⟨fuel - 1, by omega⟩
  refine ⟨⟨(Vec.new : Vec U8).val ++ f.durable.val, by rw [List.length_append]; exact hcond⟩,
    ?_, by simp⟩
  simp only [File.contents]
  rw [Vec.extend_from_slice_ok _ _ hcond, Result.bind_ok]
  simp [File.contents.loop_0, Scalar.lt, hc, usize_zero]

/-! ## 3. A crash's cut of any file within the bounds never panics -/

/-- `a - b` of `usize`s does not overflow when `b ≤ a`. -/
theorem usub (a b : Usize) (h : b.val ≤ a.val) :
    ∃ t : Usize, Scalar.sub a b = Result.ok t ∧ t.val = a.val - b.val := by
  have h0 := b.hmin
  have h1 := a.hmax
  simp only [ScalarTy.min, ScalarTy.max] at h0 h1
  exact ⟨_, Scalar.sub_ok a b ⟨by simp only [ScalarTy.min]; omega,
    by simp only [ScalarTy.max]; omega⟩, rfl⟩

/-- `a + b` of `usize`s does not overflow up to `usize::MAX`. -/
theorem uadd (a b : Usize) (h : a.val + b.val ≤ 18446744073709551615) :
    ∃ t : Usize, Scalar.add a b = Result.ok t ∧ t.val = a.val + b.val := by
  have h0 := b.hmin
  have h1 := a.hmin
  simp only [ScalarTy.min] at h0 h1
  exact ⟨_, Scalar.add_ok a b ⟨by simp only [ScalarTy.min]; omega,
    by simp only [ScalarTy.max]; omega⟩, rfl⟩

/-- `write`'s second loop from `i`, between `from` and `to`, on bytes long enough to hold
`more[from..to]` at `at`: it never panics and keeps the length. -/
theorem write_loop1_total (a : Usize) (more : Slice U8) (fr to : Usize)
    (hto : to.val ≤ (more.val.length : Int)) :
    ∀ (fuel : Nat) (bytes : Vec U8) (i : Usize), fr.val ≤ i.val → i.val ≤ to.val →
      (to.val - i.val).toNat < fuel →
      a.val + (to.val - fr.val) ≤ (bytes.val.length : Int) →
      ∃ v, write.loop_1 fuel bytes a more fr to i = Result.ok v ∧
        v.val.length = bytes.val.length := by
  intro fuel
  induction fuel with
  | zero => intro _ i _ _ h; omega
  | succ n ih =>
    intro bytes i hfi hit hfuel hlen
    have ha0 := a.hmin
    have hi0 := fr.hmin
    simp only [ScalarTy.min] at ha0 hi0
    have hbl := bytes.hlen
    have hml := more.hlen
    by_cases hlt : i.val < to.val
    · have hil : i.val.toNat < more.val.length := by omega
      obtain ⟨t31, ht31, ht31v⟩ := usub i fr hfi
      obtain ⟨t29, ht29, ht29v⟩ := uadd a t31 (by omega)
      have hidx : t29.val.toNat < bytes.val.length := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hs1 : (bytes.val.set t29.val.toNat more.val[i.val.toNat]).length ≤
          18446744073709551615 := by simp only [List.length_set]; exact bytes.hlen
      obtain ⟨v, hv, hvv⟩ := ih ⟨bytes.val.set t29.val.toNat more.val[i.val.toNat], hs1⟩ j
        (by omega) (by omega) (by omega) (by simp only [List.length_set]; omega)
      refine ⟨v, ?_, by rw [hvv]; simp⟩
      simp [write.loop_1, Scalar.lt, Slice.index, Slice.update, hlt, hil, ht31, ht29, hidx, hj, hv]
    · exact ⟨bytes, by simp [write.loop_1, Scalar.lt, hlt], rfl⟩

/-- `write` of `more[from..to]` at `at`, with `2^64` fuel, on bytes at most `2^62` long and
`at + (to - from)` at most `2^62`: it never panics, and the bytes are as long as they were or
as `at + (to - from)`. -/
theorem write_total (fuel : Nat) (bytes : Vec U8) (a : Usize) (more : Slice U8) (fr to : Usize)
    (hfuel : 18446744073709551616 ≤ fuel) (hb : bytes.val.length ≤ 4611686018427387904)
    (hft : fr.val ≤ to.val) (hto : to.val ≤ (more.val.length : Int))
    (ha : a.val + (to.val - fr.val) ≤ 4611686018427387904) :
    ∃ v, write fuel bytes a more fr to = Result.ok v ∧
      (v.val.length = bytes.val.length ∨ (v.val.length : Int) = a.val + (to.val - fr.val)) := by
  have ha0 := a.hmin
  simp only [ScalarTy.min] at ha0
  obtain ⟨t8, ht8, ht8v⟩ := usub to fr hft
  obtain ⟨e, he, hev⟩ := uadd a t8 (by omega)
  obtain ⟨g, hg, hgv⟩ := write_zeros e fuel bytes (by omega)
  have hgl : g.val.length = bytes.val.length + (e.val.toNat - bytes.val.length) := by
    rw [hgv]; simp only [List.length_append, List.length_replicate]
  obtain ⟨v, hv, hvv⟩ := write_loop1_total a more fr to hto fuel g fr (Int.le_refl _) hft
    (by omega) (by omega)
  refine ⟨v, ?_, by omega⟩
  simp only [write]
  rw [ht8, Result.bind_ok, he, Result.bind_ok, hg, Result.bind_ok]
  exact hv

/-- `land`'s loop from `sector` and `from`, with an offset and bytes to land at most `2^40`, on
bytes at most `C` long (`2^41 ≤ C ≤ 2^62`), with `2^65` fuel and more: it never panics, and
leaves bytes at most `C` long. -/
theorem land_loop_total (a : Usize) (more : Slice U8) (landed : Slice Bool) (C : Nat)
    (ha : a.val ≤ 1099511627776) (hm : more.val.length ≤ 1099511627776)
    (hC1 : 2199023255552 ≤ C) (hC2 : C ≤ 4611686018427387904) :
    ∀ (fuel : Nat) (bytes : Vec U8) (sector fr : Usize), fr.val ≤ (more.val.length : Int) →
      (landed.val.length - sector.val.toNat) + 18446744073709551616 < fuel →
      bytes.val.length ≤ C →
      ∃ v, land.loop_0 fuel bytes a more landed sector fr = Result.ok v ∧ v.val.length ≤ C := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ h; omega
  | succ n ih =>
    intro bytes sector fr hfr hfuel hb
    have hs0 := sector.hmin
    have hf0 := fr.hmin
    have ha0 := a.hmin
    simp only [ScalarTy.min] at hs0 hf0 ha0
    have hll := landed.hlen
    have h512 : (512 : Usize).val = 512 :=
      Scalar.ofNat_val .Usize 512 (by simp only [ScalarTy.min, ScalarTy.max]; omega)
    by_cases hgo : sector.val < (landed.val.length : Int) ∧ fr.val < (more.val.length : Int)
    · obtain ⟨t18, ht18, ht18v⟩ := usub (Slice.len more) fr (by simp only [Slice.len_val]; omega)
      simp only [Slice.len_val] at ht18v
      have hsl : sector.val.toNat < landed.val.length := by omega
      obtain ⟨sj, hsj, hsjv⟩ := add_one sector (by omega)
      have key : ∀ to : Usize, fr.val ≤ to.val → to.val ≤ (more.val.length : Int) →
          ∃ v, (if landed.val[sector.val.toNat] = true then do
                let t31 ← Scalar.add a fr
                let bytes ← write n bytes t31 more fr to
                let sector ← Scalar.add sector (1 : Usize)
                land.loop_0 n bytes a more landed sector to
              else do
                let sector ← Scalar.add sector (1 : Usize)
                land.loop_0 n bytes a more landed sector to) = Result.ok v ∧
            v.val.length ≤ C := by
        intro to h1 h2
        by_cases hl : landed.val[sector.val.toNat] = true
        · obtain ⟨t31, ht31, ht31v⟩ := uadd a fr (by omega)
          obtain ⟨w, hw, hwl⟩ := write_total n bytes t31 more fr to (by omega) (by omega)
            h1 h2 (by omega)
          obtain ⟨v, hv, hvl⟩ := ih w sj to h2 (by omega) (by omega)
          exact ⟨v, by simp only [hl, ite_true, ht31, Result.bind_ok, hw, hsj, hv], hvl⟩
        · obtain ⟨v, hv, hvl⟩ := ih bytes sj to h2 (by omega) hb
          exact ⟨v, by simp only [hl, Bool.false_eq_true, ite_false, Result.bind_ok, hsj, hv],
            hvl⟩
      have hgo1 : Scalar.lt sector (Slice.len landed) = true := by
        simp [Scalar.lt, Slice.len_val, hgo.1]
      have hgo2 : Scalar.lt fr (Slice.len more) = true := by
        simp [Scalar.lt, Slice.len_val, hgo.2]
      by_cases hlt : t18.val < 512
      · have hlt' : Scalar.lt t18 (512 : Usize) = true := by simp [Scalar.lt, h512, hlt]
        obtain ⟨v, hv, hvl⟩ := key (Slice.len more) (by simp only [Slice.len_val]; omega)
          (by simp only [Slice.len_val]; omega)
        refine ⟨v, ?_, hvl⟩
        simp only [land.loop_0, hgo1, hgo2, ite_true, ht18, Result.bind_ok, Slice.index, hsl,
          dite_true, List.get_eq_getElem, hlt']
        exact hv
      · have hlt' : Scalar.lt t18 (512 : Usize) = false := by simp [Scalar.lt, h512, hlt]
        obtain ⟨t, ht, htv⟩ := uadd fr 512 (by rw [h512]; omega)
        obtain ⟨v, hv, hvl⟩ := key t (by rw [htv, h512]; omega) (by rw [htv, h512]; omega)
        refine ⟨v, ?_, hvl⟩
        simp only [land.loop_0, hgo1, hgo2, ite_true, ht18, Result.bind_ok, Slice.index, hsl,
          dite_true, List.get_eq_getElem, hlt', Bool.false_eq_true, ite_false, ht]
        exact hv
    · refine ⟨bytes, ?_, hb⟩
      by_cases h1 : sector.val < (landed.val.length : Int)
      · have h2 : ¬ fr.val < (more.val.length : Int) := fun h2 => hgo ⟨h1, h2⟩
        simp [land.loop_0, Scalar.lt, Slice.len_val, h1, h2]
      · simp [land.loop_0, Scalar.lt, Slice.len_val, h1]

/-- `File::cut` of a `Bounded` file, with `2^66` fuel and any `Kept`: it never panics nor runs
out of fuel, and keeps the file's name. -/
theorem cut_total (fuel : Nat) (f : File) (k : Kept) (hb : Bounded f)
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ f', File.cut fuel f k = Result.ok f' ∧ f'.name = f.name := by
  obtain ⟨r, hr, hrn, ⟨hrd, hrc, hrs⟩, -⟩ := sync_bounded fuel f k.whole hb hfuel
  rcases r with ⟨rn, rd, ⟨rcl, rch⟩, rb, ru⟩
  simp only at hrn hrd hrc hrs
  cases rcl with
  | nil => simp [File.cut, hr, hrn]
  | cons m rest =>
    have hms := hrs m (by simp)
    simp only [List.length_cons] at hrd
    cases ht : k.torn with
    | Nothing => simp [File.cut, hr, ht, Vec.remove, usize_zero, hrn]
    | Prefix count =>
      cases hc : m.change with
      | Append more =>
        rw [hc] at hms
        have hml : more.val.length ≤ 1099511627776 := hms
        obtain ⟨x3, hx3, -⟩ := extend_spec fuel rd more count (by omega) (by omega)
        simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, hx3, hrn]
      | At a more =>
        rw [hc] at hms
        have hml : a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776 := hms
        have hc0 := count.hmin
        simp only [ScalarTy.min] at hc0
        by_cases h : count.val < (more.val.length : Int)
        · obtain ⟨x6, hx6, -⟩ := write_total fuel rd a more 0 count (Nat.le_trans (by decide) hfuel) (by omega)
            (by rw [usize_zero]; exact hc0) (by omega) (by rw [usize_zero]; omega)
          simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, Scalar.lt, h, hx6, hrn]
        · obtain ⟨x6, hx6, -⟩ := write_total fuel rd a more 0 (Slice.len more) (Nat.le_trans (by decide) hfuel)
            (by omega) (by rw [usize_zero]; simp only [Slice.len_val]; omega)
            (by simp only [Slice.len_val]; omega)
            (by rw [usize_zero]; simp only [Slice.len_val]; omega)
          simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, Scalar.lt, h, hx6, hrn]
      | Sectors a more => simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, hrn]
      | Cut len => simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, hrn]
    | Sectors landed =>
      cases hc : m.change with
      | Sectors a more =>
        rw [hc] at hms
        have hml : a.val ≤ 1099511627776 ∧ more.val.length ≤ 1099511627776 := hms
        obtain ⟨x10, hx10, -⟩ := land_loop_total a more landed 4611686018427387904 hml.1 hml.2
          (by decide) (Nat.le_refl _) fuel rd 0 0 (by rw [usize_zero]; omega)
          (by have := landed.hlen; omega) (by omega)
        simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, land, hx10, hrn]
      | Append more => simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, hrn]
      | At a more => simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, hrn]
      | Cut len => simp [File.cut, hr, ht, hc, Vec.remove, usize_zero, hrn]

/-! ## 4. `Disk::take` and `Disk::put` on a disk whose names strictly increase -/

/-- Taking one file out keeps the names increasing. -/
theorem sorted_eraseIdx {L : List File} (hs : Sorted L) (p : Nat) (hp : p < L.length) :
    Sorted (L.eraseIdx p) := by
  intro i j hi hj hij
  rw [List.length_eraseIdx_of_lt hp] at hi hj
  simp only [List.getElem_eraseIdx]
  split <;> split
  all_goals exact hs _ _ (by omega) (by omega) (by omega)

/-- Replacing a file by one of the same name keeps the names increasing. -/
theorem sorted_set {L : List File} (hs : Sorted L) (p : Nat) (x : File) (hp : p < L.length)
    (hx : x.name.val = (L[p]).name.val) : Sorted (L.set p x) := by
  have hn : ∀ k (hk : k < (L.set p x).length),
      ((L.set p x)[k]).name.val = (L[k]'(by simpa using hk)).name.val := by
    intro k hk
    rw [List.getElem_set]
    split
    · subst_vars; exact hx
    · rfl
  intro i j hi hj hij
  rw [hn, hn]
  simp only [List.length_set] at hi hj
  exact hs i j hi hj hij

/-- Putting a file in at the first position whose name is not below its own, where no file has
its name, keeps the names increasing. -/
theorem sorted_insertIdx {L : List File} (hs : Sorted L) (s : Nat) (x : File)
    (hsl : s ≤ L.length)
    (hbefore : ∀ k (hk : k < L.length), k < s → (L[k]).name.val < x.name.val)
    (hafter : ∀ hs' : s < L.length, x.name.val < (L[s]).name.val) :
    Sorted (L.insertIdx s x) := by
  have hlen := List.length_insertIdx_of_le_length hsl x
  intro i j hi hj hij
  rw [hlen] at hi hj
  by_cases hi1 : i < s
  · rw [List.getElem_insertIdx_of_lt hi1]
    by_cases hj1 : j < s
    · rw [List.getElem_insertIdx_of_lt hj1]
      exact hs _ _ _ _ hij
    · by_cases hj2 : j = s
      · subst hj2
        rw [List.getElem_insertIdx_self]
        exact hbefore _ _ hi1
      · rw [List.getElem_insertIdx_of_gt (by omega)]
        exact hs _ _ _ _ (by omega)
  · by_cases hi2 : i = s
    · subst hi2
      rw [List.getElem_insertIdx_self, List.getElem_insertIdx_of_gt (by omega)]
      have h1 := hafter (by omega)
      by_cases hj3 : j - 1 = i
      · simp only [hj3]; exact h1
      · have := hs i (j - 1) (by omega) (by omega) (by omega)
        omega
    · rw [List.getElem_insertIdx_of_gt (by omega), List.getElem_insertIdx_of_gt (by omega)]
      exact hs _ _ _ _ (by omega)

/-- `Disk::take` on a disk whose names strictly increase, with more fuel than files: it never
panics nor runs out of fuel; the names still strictly increase, there are no more files than
before, and every file of another name is still there. -/
theorem take_spec (fuel : Nat) (d : Disk) (name : U64) (hs : Sorted d.files.val)
    (hf : d.files.val.length < fuel) :
    ∃ r d', Disk.take fuel d name = Result.ok (r, d') ∧ Sorted d'.files.val ∧
      d'.files.val.length ≤ d.files.val.length ∧
      (∀ F ∈ d.files.val, F.name.val ≠ name.val → F ∈ d'.files.val) := by
  obtain ⟨r0, hfind, hnone, hsome⟩ := find_spec fuel d name hf
  cases r0 with
  | none => exact ⟨none, d, by simp [Disk.take, hfind], hs, Nat.le_refl _, fun F hF _ => hF⟩
  | some at0 =>
    obtain ⟨hat, hatn, -⟩ := hsome at0 rfl
    have hl := d.files.hlen
    refine ⟨some d.files.val[at0.val.toNat],
      ⟨⟨d.files.val.eraseIdx at0.val.toNat, by rw [List.length_eraseIdx_of_lt hat]; omega⟩,
        d.epoch⟩, ?_, sorted_eraseIdx hs _ hat, ?_, ?_⟩
    · simp [Disk.take, hfind, Vec.remove, hat]
    · simp only [List.length_eraseIdx_of_lt hat]; omega
    · intro F hF hFn
      obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hF
      have hka : k ≠ at0.val.toNat := fun e => hFn (by subst e; exact hatn)
      have hl' := List.length_eraseIdx_of_lt hat
      rcases Nat.lt_or_gt_of_ne hka with hlt | hgt
      · refine List.mem_iff_getElem.mpr ⟨k, by simp only; omega, ?_⟩
        simp only [List.getElem_eraseIdx, hlt, dite_true]
      · refine List.mem_iff_getElem.mpr ⟨k - 1, by simp only; omega, ?_⟩
        have hn : ¬ k - 1 < at0.val.toNat := by omega
        simp only [List.getElem_eraseIdx, hn, dite_false]
        congr 1
        omega

/-- `Disk::put` on a disk whose names strictly increase, below `usize::MAX` files, with more
fuel than files: it never panics nor runs out of fuel; the names still strictly increase, there
is at most one file more, `file` is there, and every file of another name is still there. -/
theorem put_spec (fuel : Nat) (d : Disk) (file : File) (hs : Sorted d.files.val)
    (hl : d.files.val.length < 18446744073709551615) (hf : d.files.val.length < fuel) :
    ∃ d', Disk.put fuel d file = Result.ok d' ∧ Sorted d'.files.val ∧
      d'.files.val.length ≤ d.files.val.length + 1 ∧ file ∈ d'.files.val ∧
      (∀ F ∈ d.files.val, F.name.val ≠ file.name.val → F ∈ d'.files.val) := by
  obtain ⟨s, hsq, hs1, h2, h3⟩ := slot_spec fuel d file.name hf
  have hs0 := s.hmin
  simp only [ScalarTy.min] at hs0
  by_cases hrep : ∃ hsl : s.val.toNat < d.files.val.length,
      (d.files.val[s.val.toNat]).name.val = file.name.val
  · obtain ⟨hsl, he⟩ := hrep
    refine ⟨⟨⟨d.files.val.set s.val.toNat file, by simp only [List.length_set]; exact d.files.hlen⟩,
      d.epoch⟩, ?_, sorted_set hs _ file hsl he.symm, by simp,
      List.mem_iff_getElem.mpr ⟨s.val.toNat, by simp [hsl], by simp⟩, ?_⟩
    · have hsv : s.val < (d.files.val.length : Int) := by omega
      simp [Disk.put, hsq, Scalar.lt, hsv, Slice.index, hsl, Scalar.eq, he, Slice.update]
    · intro F hF hFn
      obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hF
      have hks : s.val.toNat ≠ k := fun e => hFn (by subst e; exact he)
      exact List.mem_iff_getElem.mpr ⟨k, by simp [hk], by simp [hks]⟩
  · have hne : ∀ hsl : s.val.toNat < d.files.val.length,
        (d.files.val[s.val.toNat]).name.val ≠ file.name.val := fun hsl e => hrep ⟨hsl, e⟩
    have hins : s.val.toNat ≤ d.files.val.length ∧ d.files.val.length < 18446744073709551615 :=
      ⟨hs1, hl⟩
    have hlen := List.length_insertIdx_of_le_length hs1 file
    refine ⟨⟨⟨d.files.val.insertIdx s.val.toNat file, by rw [hlen]; omega⟩, d.epoch⟩, ?_,
      sorted_insertIdx hs _ file hs1 h2 (fun hsl => by
        have := h3 hsl; have := hne hsl; omega), by simp only [hlen]; omega,
      List.mem_iff_getElem.mpr ⟨s.val.toNat, by simp only [hlen]; omega,
        List.getElem_insertIdx_self ..⟩, ?_⟩
    · by_cases hsl : s.val.toNat < d.files.val.length
      · have hsv : s.val < (d.files.val.length : Int) := by omega
        have hn' : ¬ (d.files.val[s.val.toNat]).name.val = file.name.val := hne hsl
        have hsle : s.val ≤ (d.files.val.length : Int) := by omega
        simp [Disk.put, hsq, Scalar.lt, hsv, Slice.index, hsl, Scalar.eq, hn', Vec.insert, hins,
          hsle]
      · have hsv : ¬ s.val < (d.files.val.length : Int) := by omega
        have hsle : s.val ≤ (d.files.val.length : Int) := by omega
        simp [Disk.put, hsq, Scalar.lt, hsv, Vec.insert, hins, hsle]
    · intro F hF _
      obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hF
      by_cases hlt : k < s.val.toNat
      · refine List.mem_iff_getElem.mpr ⟨k, by simp only [hlen]; omega, ?_⟩
        simp only
        rw [List.getElem_insertIdx_of_lt hlt]
      · refine List.mem_iff_getElem.mpr ⟨k + 1, by simp only [hlen]; omega, ?_⟩
        rw [List.getElem_insertIdx_of_gt (by omega)]
        rfl

/-! ## 5. `Disk::crash`'s second loop: what it puts back never takes another file's place -/

/-- THE KEY INVARIANT. The second loop of `Disk::crash`, on a disk whose names strictly
increase, with a file `F` none of the undone writes and renames names, at most `2^20` files
and twice the entries left together, and more fuel than `2^20` and the entries left: it never
panics nor runs out of fuel, the names still strictly increase, and `F` is still there,
unchanged. Each turn takes the file of an entry's name, which is not `F`'s; puts a file back
under the name it was renamed from only where no file has that name, which `F` has; and puts
what the name held before under the entry's name. -/
theorem crash_loop1 (F : File) :
    ∀ (fuel : Nat) (self : Disk) (undone : Vec Undone), Sorted self.files.val →
      F ∈ self.files.val → (∀ u ∈ undone.val, u.name.val ≠ F.name.val) →
      self.files.val.length + 2 * undone.val.length ≤ 1048576 →
      undone.val.length + 1048576 < fuel →
      ∃ d', Disk.crash.loop_1 fuel self undone = Result.ok d' ∧ Sorted d'.files.val ∧
        F ∈ d'.files.val := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ _ _ h; omega
  | succ n ih =>
    intro self undone hs hF hu hlen hfuel
    rcases undone with ⟨ul, uh⟩
    cases ul with
    | nil => exact ⟨self, by simp [Disk.crash.loop_1], hs, hF⟩
    | cons u rest =>
      simp only [List.mem_cons, forall_eq_or_imp] at hu
      simp only [List.length_cons] at hlen hfuel
      obtain ⟨r, d1, htake, hs1, hl1, hk1⟩ := take_spec n self u.name hs (by omega)
      have hF1 := hk1 F hF (Ne.symm hu.1)
      simp only [Disk.crash.loop_1, Slice.is_empty, List.isEmpty_cons, Bool.not_false,
        ↓reduceIte, Vec.remove_ok (⟨u :: rest, uh⟩ : Vec Undone) 0 (by rw [usize_zero]; simp),
        Result.bind_ok, htake,
        usize_zero, Int.toNat_zero, List.eraseIdx_cons_zero, List.getElem_cons_zero]
      have cont : ∀ d3 : Disk, Sorted d3.files.val → F ∈ d3.files.val →
          d3.files.val.length ≤ self.files.val.length + 2 →
          ∃ d', Disk.crash.loop_1 n d3 ⟨rest, by omega⟩ = Result.ok d' ∧ Sorted d'.files.val ∧
            F ∈ d'.files.val :=
        fun d3 h1 h2 h3 => ih d3 ⟨rest, by omega⟩ h1 h2 hu.2 (by simp only; omega)
          (by simp only; omega)
      have putw : ∀ (d2 : Disk) (w : Vec U8), Sorted d2.files.val → F ∈ d2.files.val →
          d2.files.val.length ≤ self.files.val.length + 1 →
          ∃ d3, Disk.put n d2 (File.whole u.name w) = Result.ok d3 ∧ Sorted d3.files.val ∧
            F ∈ d3.files.val ∧ d3.files.val.length ≤ self.files.val.length + 2 := by
        intro d2 w h1 h2 h3
        obtain ⟨d3, hput, hs3, hl3, -, hk3⟩ :=
          put_spec n d2 (File.whole u.name w) h1 (by omega) (by omega)
        exact ⟨d3, hput, hs3, hk3 F h2 (Ne.symm hu.1), by omega⟩
      have was : ∀ d2 : Disk, Sorted d2.files.val → F ∈ d2.files.val →
          d2.files.val.length ≤ self.files.val.length + 1 →
          (∃ d', Disk.crash.loop_1 n d2 ⟨rest, by omega⟩ = Result.ok d' ∧
            Sorted d'.files.val ∧ F ∈ d'.files.val) ∧
          ∀ w, ∃ d', (do
              let s ← Disk.put n d2 (File.whole u.name w)
              Disk.crash.loop_1 n s ⟨rest, by omega⟩) = Result.ok d' ∧ Sorted d'.files.val ∧
            F ∈ d'.files.val := by
        intro d2 h1 h2 h3
        refine ⟨cont d2 h1 h2 (by omega), fun w => ?_⟩
        obtain ⟨d3, hput, hs3, hF3, hl3⟩ := putw d2 w h1 h2 h3
        obtain ⟨d', h', hs', hF'⟩ := cont d3 hs3 hF3 hl3
        exact ⟨d', by rw [hput, Result.bind_ok]; exact h', hs', hF'⟩
      have hw1 := was d1 hs1 hF1 (by omega)
      cases hfr : u.undo.«from» with
      | none =>
        cases hw : u.undo.was with
        | none => exact hw1.1
        | some w => exact hw1.2 w
      | some fr =>
        cases r with
        | none =>
          cases hw : u.undo.was with
          | none => exact hw1.1
          | some w => exact hw1.2 w
        | some file =>
          obtain ⟨r2, hfind2, hnone2, -⟩ := find_spec n d1 fr (by omega)
          cases r2 with
          | some q =>
            cases hw : u.undo.was with
            | none =>
              obtain ⟨d', h', hs', hF'⟩ := hw1.1
              exact ⟨d', by simp [hfind2, h'], hs', hF'⟩
            | some w =>
              obtain ⟨d', h', hs', hF'⟩ := hw1.2 w
              exact ⟨d', by simp [hfind2]; exact h', hs', hF'⟩
          | none =>
            have hFn : F.name.val ≠ fr.val := by
              obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hF1
              exact hnone2 rfl k hk
            obtain ⟨d2, hput, hs2, hl2, -, hk2⟩ :=
              put_spec n d1 { file with name := fr } hs1 (by omega) (by omega)
            have hw2 := was d2 hs2 (hk2 F hF1 hFn) (by omega)
            cases hw : u.undo.was with
            | none =>
              obtain ⟨d', h', hs', hF'⟩ := hw2.1
              exact ⟨d', by simp [hfind2, hput, h'], hs', hF'⟩
            | some w =>
              obtain ⟨d', h', hs', hF'⟩ := hw2.2 w
              exact ⟨d', by simp [hfind2, hput]; exact h', hs', hF'⟩

/-! ## 6. `Disk::crash`'s first loop: each file cut, the undone ones remembered -/

/-- The first loop of `Disk::crash`, from `i`, over the files of `d` (all `Bounded`, at most
`2^16`) and a cut with one `Kept` per file, with more fuel than `2^66` and the turns left: it
never panics nor runs out of fuel; every file keeps its name and place; the file at `p` is what
`File::cut` of it (its undo taken) gives, so has whatever `Q` every such cut has; and every
entry remembered as undone names a file with an undo, so never `N` when the file named `N` has
none, and there are at most as many as files. -/
theorem crash_loop0 (d : Disk) (cut : Slice Kept) (N : U64) (Q : File → Prop) (p : Nat)
    (hlc : cut.val.length = d.files.val.length) (hL : d.files.val.length ≤ 65536)
    (hB : ∀ k (hk : k < d.files.val.length), Bounded d.files.val[k])
    (hN : ∀ k (hk : k < d.files.val.length), (d.files.val[k]).name.val = N.val →
      (d.files.val[k]).undo = none)
    (hp : p < d.files.val.length)
    (hQ : ∀ n, 73786976294838206464 ≤ n → ∃ f', File.cut n
      { d.files.val[p] with undo := none } (cut.val[p]'(by omega)) = Result.ok f' ∧ Q f') :
    ∀ (fuel : Nat) (self : Disk) (undone : Vec Undone) (i : Usize)
      (hl : self.files.val.length = d.files.val.length),
      i.val.toNat ≤ d.files.val.length →
      (d.files.val.length - i.val.toNat) + 73786976294838206464 < fuel →
      (∀ k (hk : k < d.files.val.length), i.val.toNat ≤ k →
        self.files.val[k]'(by omega) = d.files.val[k]) →
      (∀ k (hk : k < d.files.val.length),
        (self.files.val[k]'(by omega)).name = (d.files.val[k]).name) →
      (p < i.val.toNat → Q (self.files.val[p]'(by omega))) →
      (∀ x ∈ undone.val, x.name.val ≠ N.val) → undone.val.length ≤ i.val.toNat →
      ∃ s' u', Disk.crash.loop_0 fuel self cut undone i = Result.ok (s', u') ∧
        ∃ hl' : s'.files.val.length = d.files.val.length,
        (∀ k (hk : k < d.files.val.length),
          (s'.files.val[k]'(by omega)).name = (d.files.val[k]).name) ∧
        Q (s'.files.val[p]'(by omega)) ∧
        (∀ x ∈ u'.val, x.name.val ≠ N.val) ∧ u'.val.length ≤ d.files.val.length := by
  intro fuel
  induction fuel with
  | zero => intro _ _ i _ _ h; omega
  | succ n ih =>
    intro self undone i hl hi hfuel hsame hname hq hu hul
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hif : i.val.toNat < self.files.val.length := by omega
      have hic : i.val.toNat < cut.val.length := by omega
      have hiv : i.val < (self.files.val.length : Int) := by omega
      have hicv : i.val < (cut.val.length : Int) := by omega
      have hx0 : self.files.val[i.val.toNat] = d.files.val[i.val.toNat] :=
        hsame _ hlt (Nat.le_refl _)
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      obtain ⟨x8, hx8, hx8n⟩ := cut_total n { d.files.val[i.val.toNat] with undo := none }
        cut.val[i.val.toNat] (hB _ hlt) (by omega)
      have hx8q : i.val.toNat = p → Q x8 := by
        intro e
        subst e
        obtain ⟨f', hf', hq'⟩ := hQ n (by omega)
        rw [hx8] at hf'
        cases hf'
        exact hq'
      have hlen2 : ((self.files.val.set i.val.toNat
          { d.files.val[i.val.toNat] with undo := none }).set i.val.toNat x8).length ≤
          18446744073709551615 := by
        simp only [List.length_set]; exact self.files.hlen
      have cont : ∀ U2 : Vec Undone, (∀ x ∈ U2.val, x.name.val ≠ N.val) →
          U2.val.length ≤ j.val.toNat →
          ∃ s' u', Disk.crash.loop_0 n ⟨⟨(self.files.val.set i.val.toNat
              { d.files.val[i.val.toNat] with undo := none }).set i.val.toNat x8, hlen2⟩,
              self.epoch⟩ cut U2 j = Result.ok (s', u') ∧
            ∃ hl' : s'.files.val.length = d.files.val.length,
            (∀ k (hk : k < d.files.val.length),
              (s'.files.val[k]'(by omega)).name = (d.files.val[k]).name) ∧
            Q (s'.files.val[p]'(by omega)) ∧
            (∀ x ∈ u'.val, x.name.val ≠ N.val) ∧ u'.val.length ≤ d.files.val.length := by
        intro U2 hU2 hU2l
        refine ih _ U2 j (by simp only [List.length_set]; exact hl) (by omega) (by omega)
          ?_ ?_ ?_ hU2 hU2l
        · intro k hk hjk
          have hne : i.val.toNat ≠ k := by omega
          simp only [List.getElem_set, hne, ite_false]
          exact hsame k hk (by omega)
        · intro k hk
          simp only [List.getElem_set]
          by_cases e : i.val.toNat = k
          · subst e; simp only [ite_true]; exact hx8n
          · simp only [e, ite_false]; exact hname k hk
        · intro hpj
          simp only [List.getElem_set]
          by_cases e : i.val.toNat = p
          · subst e; simp only [ite_true]; exact hx8q rfl
          · simp only [e, ite_false]; exact hq (by omega)
      cases hund : (d.files.val[i.val.toNat]).undo with
      | none =>
        have hstep : Disk.crash.loop_0 (n + 1) self cut undone i =
            Disk.crash.loop_0 n ⟨⟨(self.files.val.set i.val.toNat
              { d.files.val[i.val.toNat] with undo := none }).set i.val.toNat x8, hlen2⟩,
              self.epoch⟩ cut undone j := by
          simp [Disk.crash.loop_0, Scalar.lt, Slice.len_val, hiv, hicv, Slice.index, hif, hic,
            Slice.update, hx0, hund, hx8, hj]
        rw [hstep]
        exact cont undone hu (by omega)
      | some v =>
        by_cases hcu : (cut.val[i.val.toNat]).undone = true
        · have hnn : (d.files.val[i.val.toNat]).name.val ≠ N.val := by
            intro e
            have := hN _ hlt e
            rw [hund] at this
            cases this
          have hpush : undone.val.length < 18446744073709551615 := by omega
          have hstep : Disk.crash.loop_0 (n + 1) self cut undone i =
              Disk.crash.loop_0 n ⟨⟨(self.files.val.set i.val.toNat
                { d.files.val[i.val.toNat] with undo := none }).set i.val.toNat x8, hlen2⟩,
                self.epoch⟩ cut
                ⟨undone.val ++ [{ name := (d.files.val[i.val.toNat]).name, undo := v }],
                  by simp only [List.length_append, List.length_singleton]; omega⟩ j := by
            simp [Disk.crash.loop_0, Scalar.lt, Slice.len_val, hiv, hicv, Slice.index, hif, hic,
              Slice.update, hx0, hund, hx8, hj, hcu, Vec.push, hpush]
          rw [hstep]
          refine cont _ ?_ (by simp only [List.length_append, List.length_singleton]; omega)
          intro x hx
          simp only [List.mem_append, List.mem_singleton] at hx
          rcases hx with hx | hx
          · exact hu x hx
          · subst hx; exact hnn
        · have hstep : Disk.crash.loop_0 (n + 1) self cut undone i =
              Disk.crash.loop_0 n ⟨⟨(self.files.val.set i.val.toNat
                { d.files.val[i.val.toNat] with undo := none }).set i.val.toNat x8, hlen2⟩,
                self.epoch⟩ cut undone j := by
            simp [Disk.crash.loop_0, Scalar.lt, Slice.len_val, hiv, hicv, Slice.index, hif, hic,
              Slice.update, hx0, hx8, hj, hcu]
          rw [hstep]
          exact cont undone hu (by omega)
    · have hstop : ¬ i.val < (self.files.val.length : Int) := by omega
      refine ⟨self, undone, by simp [Disk.crash.loop_0, Scalar.lt, Slice.len_val, hstop], hl,
        hname, hq (by omega), hu, by omega⟩

/-! ## 7. `Disk::crash` and `Disk::contents` after it -/

/-- `Disk::crash` of a disk whose names strictly increase, with at most `2^16` files, all
`Bounded`, a cut with one `Kept` per file and `2^67` fuel, when the file at `p` has nothing to
undo: it never panics nor runs out of fuel, the names still strictly increase, and a file with
the name of the one at `p` is there with whatever `Q` every cut of that file has. -/
theorem crash_spec (fuel : Nat) (d : Disk) (cut : Slice Kept) (Q : File → Prop) (p : Nat)
    (hs : Sorted d.files.val)
    (hlc : cut.val.length = d.files.val.length) (hL : d.files.val.length ≤ 65536)
    (hB : ∀ k (hk : k < d.files.val.length), Bounded d.files.val[k])
    (hp : p < d.files.val.length) (hpu : (d.files.val[p]).undo = none)
    (hQ : ∀ n, 73786976294838206464 ≤ n → ∃ f', File.cut n
      { d.files.val[p] with undo := none } (cut.val[p]'(by omega)) = Result.ok f' ∧ Q f')
    (hfuel : 147573952589676412928 ≤ fuel) :
    ∃ d', Disk.crash fuel d cut = Result.ok d' ∧ Sorted d'.files.val ∧
      ∃ F ∈ d'.files.val, F.name.val = (d.files.val[p]).name.val ∧ Q F := by
  have hN : ∀ k (hk : k < d.files.val.length),
      (d.files.val[k]).name.val = (d.files.val[p]).name.val → (d.files.val[k]).undo = none := by
    intro k hk e
    have := hs.unique k p hk hp e
    subst this
    exact hpu
  obtain ⟨s', u', h0, hl', hnm, hq, hu, hul⟩ :=
    crash_loop0 d cut (d.files.val[p]).name Q p hlc hL hB hN hp hQ fuel d Vec.new 0 rfl
      (by rw [usize_zero]; omega) (by rw [usize_zero]; omega) (fun _ _ _ => rfl)
      (fun _ _ => rfl) (fun h => by rw [usize_zero] at h; omega) (by simp [Vec.new])
      (by simp [Vec.new])
  have hs' : Sorted s'.files.val := by
    intro i j hi hj hij
    rw [hnm i (by omega), hnm j (by omega)]
    exact hs i j (by omega) (by omega) hij
  obtain ⟨d', h1, hs1, hF1⟩ := crash_loop1 (s'.files.val[p]'(by omega)) fuel s' u' hs'
    (List.getElem_mem _) (fun u hu' => by rw [hnm p hp]; exact hu u hu') (by omega) (by omega)
  refine ⟨d', by simp [Disk.crash, h0, h1], hs1, _, hF1, by rw [hnm p hp], hq⟩

/-- `Disk::contents` of a name, on a disk whose names strictly increase, where the file of that
name has no change, with more fuel than files: its durable bytes. -/
theorem contents_of_mem (fuel : Nat) (d : Disk) (F : File) (N : U64) (hs : Sorted d.files.val)
    (hF : F ∈ d.files.val) (hn : F.name.val = N.val) (hc : F.changes.val = [])
    (hf : d.files.val.length < fuel) :
    ∃ v, Disk.contents fuel d N = Result.ok (some v) ∧ v.val = F.durable.val := by
  obtain ⟨k, hk, rfl⟩ := List.mem_iff_getElem.mp hF
  obtain ⟨r, hfind, hnone, hsome⟩ := find_spec fuel d N hf
  cases r with
  | none => exact absurd hn (hnone rfl k hk)
  | some j =>
    obtain ⟨hj, hjn, -⟩ := hsome j rfl
    have e : j.val.toNat = k := hs.unique _ _ hj hk (hjn.trans hn.symm)
    subst e
    obtain ⟨v, hv, hvv⟩ := contents_nil fuel d.files.val[j.val.toNat] hc (by omega)
    exact ⟨v, by simp [Disk.contents, hfind, Slice.index_ok d.files j hj, hv], hvv⟩

/-- A file with nothing to undo is itself with its undo taken. -/
theorem with_undo_none (f : File) (h : f.undo = none) : { f with undo := none } = f := by
  cases f
  simp_all

/-! ## 8. A crash cuts an appended file to a prefix

Fuel: `2^67` (`147573952589676412928`), more than every loop needs: `wf`, `allows` and the
contents, syncs and cuts of files take `2^66`, `small` `2^65`, and the crash's loops, `find`
and `only_appended` more than the files, changes and undone entries, which are at most `2^20`.
-/

theorem appended_proof :
    blaze_core.world.disk.a_crash_cuts_an_appended_file_to_a_prefix.spec.statement := by
  intro disk file cut
  refine ⟨147573952589676412928, ?_⟩
  have hl0 := disk.files.hlen
  obtain ⟨bw, hwf, hwfs⟩ := wf_spec 147573952589676412928 disk (by omega)
  obtain ⟨bs, hsm, hsms⟩ := small_spec 147573952589676412928 disk (by omega)
  by_cases hgo : bw = true ∧ bs = true
  case neg =>
    cases bw <;> cases bs <;> simp_all [a_crash_cuts_an_appended_file_to_a_prefix]
  obtain ⟨hbw, hbs⟩ := hgo
  subst hbw hbs
  have hsort := hwfs rfl
  obtain ⟨hlen16, -⟩ := hsms rfl
  have hsmall := small_files _ disk hsm
  obtain ⟨r0, hfind, hnone, hsome⟩ := find_spec 147573952589676412928 disk file (by omega)
  cases r0 with
  | none => simp [a_crash_cuts_an_appended_file_to_a_prefix, hwf, hsm, hfind]
  | some at0 =>
    obtain ⟨hat, hatn, -⟩ := hsome at0 rfl
    have hS := hsmall _ hat
    have hix := Slice.index_ok disk.files at0 hat
    obtain ⟨ba, hoa, hoaiff⟩ := only_appended_spec 147573952589676412928
      disk.files.val[at0.val.toNat] (by have := hS.2.1; omega)
    cases ba with
    | false => simp [a_crash_cuts_an_appended_file_to_a_prefix, hwf, hsm, hfind, hix, hoa]
    | true =>
      have hA := hoaiff.mp rfl
      cases hu : (disk.files.val[at0.val.toNat]).undo with
      | some u =>
        simp [a_crash_cuts_an_appended_file_to_a_prefix, hwf, hsm, hfind, hix, hoa, hu,
          core.option.Option.is_some]
      | none =>
        obtain ⟨read, hread, hreadv⟩ := contents_appended 147573952589676412928
          disk.files.val[at0.val.toNat] hS hA (by omega)
        obtain ⟨dur, hdur, hdurv⟩ := copy_spec (disk.files.val[at0.val.toNat]).durable
        obtain ⟨bal, hal⟩ := allows_total 147573952589676412928 disk cut (by omega)
        cases bal with
        | false =>
          simp [a_crash_cuts_an_appended_file_to_a_prefix, hwf, hsm, hfind, hix, hoa,
            core.option.Option.is_some, hread, hdur, hal]
        | true =>
          obtain ⟨hlc, hfits⟩ := allows_fits _ disk cut hal
          obtain ⟨d', hcr, hs', F, hF, hFn, hFc, hFp1, hFp2⟩ :=
            crash_spec 147573952589676412928 disk cut
              (fun F => F.changes.val = [] ∧
                (disk.files.val[at0.val.toNat]).durable.val <+: F.durable.val ∧
                F.durable.val <+: (disk.files.val[at0.val.toNat]).durable.val ++
                  appended disk.files.val[at0.val.toNat])
              at0.val.toNat hsort hlc hlen16 (fun k hk => small_bounded _ (hsmall k hk)) hat hu
              (fun n hn => by
                rw [with_undo_none _ hu]
                obtain ⟨f', h1, -, h2, -, h3, h4⟩ := cut_appended n _ _ hS hA
                  (hfits _ hat (by omega)) hn
                exact ⟨f', h1, h2, h3, h4⟩)
              (Nat.le_refl _)
          obtain ⟨v, hcont, hv⟩ := contents_of_mem 147573952589676412928 d' F file hs' hF
            (hFn.trans hatn) hFc (by have := d'.files.hlen; omega)
          obtain ⟨b, hb, hbiff⟩ := cut_to_a_prefix_spec 147573952589676412928 dur read (some v)
            (by omega)
          have hbt : b = true := hbiff.mpr ⟨v, rfl, by rw [hv, hdurv]; exact hFp1,
            by rw [hv, hreadv]; exact hFp2⟩
          subst hbt
          simp [a_crash_cuts_an_appended_file_to_a_prefix, hwf, hsm, hfind, hix, hoa,
            core.option.Option.is_some, hread, hdur, hal, hcr, hcont, hb]

/-! ## 9. `Disk::sync`: the synced file keeps what it read, and the flush keeps it so -/

/-- A file fully synced to the bytes `D`: no change left, nothing to undo, and `D` durable. -/
def Synced (D : List U8) (f : File) : Prop :=
  f.changes.val = [] ∧ f.undo = none ∧ f.durable.val = D

/-- The loop of `File::ordered` from `i`, with more fuel than barriers left: it never panics nor
runs out of fuel. -/
theorem ordered_loop_total (f : File) : ∀ (fuel : Nat) (most i : Usize),
    i.val.toNat ≤ f.barriers.val.length → f.barriers.val.length - i.val.toNat < fuel →
    ∃ c, File.ordered.loop_0 fuel f most i = Result.ok c := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ h; omega
  | succ n ih =>
    intro most i hi hfuel
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    have hl := f.barriers.hlen
    by_cases hlt : i.val.toNat < f.barriers.val.length
    · have hiv : i.val < (f.barriers.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      by_cases hg : most.val < (f.barriers.val[i.val.toNat]).covers.val
      · obtain ⟨c, hc⟩ := ih (f.barriers.val[i.val.toNat]).covers j (by omega) (by omega)
        exact ⟨c, by simp [File.ordered.loop_0, Scalar.lt, Scalar.gt, Slice.len_val, hiv,
          Slice.index, hlt, hg, hj, hc]⟩
      · obtain ⟨c, hc⟩ := ih most j (by omega) (by omega)
        exact ⟨c, by simp [File.ordered.loop_0, Scalar.lt, Scalar.gt, Slice.len_val, hiv,
          Slice.index, hlt, hg, hj, hc]⟩
    · have hstop : ¬ i.val < (f.barriers.val.length : Int) := by omega
      exact ⟨most, by simp [File.ordered.loop_0, Scalar.lt, Slice.len_val, hstop]⟩

/-- The loop of `Disk::flush` from `i`, over a disk with as many files as `d` and their names,
all `Bounded`, with more fuel than `2^66` and the turns left: it never panics nor runs out of
fuel, keeps every file's name and place and every file `Bounded`, and a file at `p` synced to
`D` stays so. -/
theorem flush_loop (d : Disk) (D : List U8) (p : Nat) (hp : p < d.files.val.length) :
    ∀ (fuel : Nat) (self : Disk) (i : Usize)
      (hl : self.files.val.length = d.files.val.length),
      i.val.toNat ≤ d.files.val.length →
      (d.files.val.length - i.val.toNat) + 73786976294838206464 < fuel →
      (∀ k (hk : k < d.files.val.length), Bounded (self.files.val[k]'(by omega))) →
      (∀ k (hk : k < d.files.val.length),
        (self.files.val[k]'(by omega)).name = (d.files.val[k]).name) →
      Synced D (self.files.val[p]'(by omega)) →
      ∃ s', Disk.flush.loop_0 fuel self i = Result.ok s' ∧
        ∃ hl' : s'.files.val.length = d.files.val.length,
        (∀ k (hk : k < d.files.val.length), Bounded (s'.files.val[k]'(by omega))) ∧
        (∀ k (hk : k < d.files.val.length),
          (s'.files.val[k]'(by omega)).name = (d.files.val[k]).name) ∧
        Synced D (s'.files.val[p]'(by omega)) := by
  intro fuel
  induction fuel with
  | zero => intro _ i _ _ h; omega
  | succ n ih =>
    intro self i hl hi hfuel hB hname hq
    have hi0 := i.hmin
    simp only [ScalarTy.min] at hi0
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hsl := self.files.hlen
      have hif : i.val.toNat < self.files.val.length := by omega
      have hiv : i.val < (self.files.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      by_cases hbe : (self.files.val[i.val.toNat]).barriers.val = []
      · have hstep : Disk.flush.loop_0 (n + 1) self i = Disk.flush.loop_0 n self j := by
          simp [Disk.flush.loop_0, Scalar.lt, Slice.len_val, hiv, Slice.index, hif,
            Slice.is_empty, hbe, hj]
        rw [hstep]
        exact ih self j hl (by omega) (by omega) hB hname hq
      · obtain ⟨c, hc⟩ := ordered_loop_total self.files.val[i.val.toNat] n 0 0
          (by rw [usize_zero]; omega)
          (by have := (self.files.val[i.val.toNat]).barriers.hlen; rw [usize_zero]; omega)
        obtain ⟨r, hr, hrn, hrB, hrs⟩ := sync_bounded n self.files.val[i.val.toNat] c
          (hB _ hlt) (by omega)
        have hlen2 : (self.files.val.set i.val.toNat r).length ≤ 18446744073709551615 := by
          simp only [List.length_set]; exact self.files.hlen
        have hstep : Disk.flush.loop_0 (n + 1) self i =
            Disk.flush.loop_0 n { self with files := ⟨self.files.val.set i.val.toNat r, hlen2⟩ }
              j := by
          simp [Disk.flush.loop_0, Scalar.lt, Slice.len_val, hiv, Slice.index, hif,
            Slice.is_empty, hbe, File.ordered, hc, hr, Slice.update, hj]
        rw [hstep]
        refine ih _ j (by simp only [List.length_set]; exact hl) (by omega) (by omega) ?_ ?_ ?_
        · intro k hk
          simp only [List.getElem_set]
          by_cases e : i.val.toNat = k
          · subst e; simp only [ite_true]; exact hrB
          · simp only [e, ite_false]; exact hB k hk
        · intro k hk
          simp only [List.getElem_set]
          by_cases e : i.val.toNat = k
          · subst e; simp only [ite_true]; rw [hrn]; exact hname _ hk
          · simp only [e, ite_false]; exact hname k hk
        · simp only [List.getElem_set]
          by_cases e : i.val.toNat = p
          · subst e
            simp only [ite_true]
            obtain ⟨h1, h2, h3⟩ := hq
            obtain ⟨g1, g2, g3⟩ := hrs h1
            exact ⟨g2, g3, g1.trans h3⟩
          · simp only [e, ite_false]; exact hq
    · have hstop : ¬ i.val < (self.files.val.length : Int) := by omega
      exact ⟨self, by simp [Disk.flush.loop_0, Scalar.lt, Slice.len_val, hstop], hl, hB, hname,
        hq⟩

/-- `Disk::sync` of the name of the file at `p`, on a disk whose names strictly increase, with
at most `2^16` files, all `Bounded`, the one at `p` `Small`, covering all its changes, with
`2^67` fuel: it never panics nor runs out of fuel; every file keeps its name and place and stays
`Bounded`; and the file at `p` is synced to its contents before. -/
theorem disk_sync (fuel : Nat) (d : Disk) (N : U64) (p : Nat) (covers : Usize)
    (hs : Sorted d.files.val) (hL : d.files.val.length ≤ 65536)
    (hB : ∀ k (hk : k < d.files.val.length), Bounded d.files.val[k])
    (hp : p < d.files.val.length) (hpn : (d.files.val[p]).name.val = N.val)
    (hS : Small d.files.val[p])
    (hall : ((d.files.val[p]).changes.val.length : Int) ≤ covers.val)
    (hfuel : 147573952589676412928 ≤ fuel) (read : Vec U8)
    (hread : File.contents fuel d.files.val[p] = Result.ok read) :
    ∃ d', Disk.sync fuel d N covers = Result.ok d' ∧
      ∃ hl' : d'.files.val.length = d.files.val.length,
      (∀ k (hk : k < d.files.val.length), Bounded (d'.files.val[k]'(by omega))) ∧
      (∀ k (hk : k < d.files.val.length),
        (d'.files.val[k]'(by omega)).name = (d.files.val[k]).name) ∧
      Synced read.val (d'.files.val[p]'(by omega)) := by
  obtain ⟨a, d1, hopen, -, -, ⟨ha, han⟩, hsame, -⟩ :=
    open_spec fuel d N hs (by omega) (by omega)
  have hd1 := (hsame ⟨p, hp, hpn⟩).symm
  subst hd1
  have e : a.val.toNat = p := hs.unique _ _ ha hp (han.trans hpn.symm)
  subst e
  obtain ⟨r, v, hsync, hv, hrc, hru, hrn, hrd⟩ := sync_all fuel _ covers hS hall (by omega)
  rw [hread] at hv
  cases hv
  obtain ⟨r', hr', -, hrB, -⟩ := sync_bounded fuel _ covers (hB _ ha) (by omega)
  rw [hsync] at hr'
  cases hr'
  have hlen2 : (d.files.val.set a.val.toNat r).length ≤ 18446744073709551615 := by
    simp only [List.length_set]; exact d.files.hlen
  obtain ⟨s', hfl, hl', hB', hn', hq'⟩ := flush_loop d read.val a.val.toNat ha fuel
    { d with files := ⟨d.files.val.set a.val.toNat r, hlen2⟩ } 0
    (by simp only [List.length_set]) (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
    (fun k hk => by
      simp only [List.getElem_set]
      by_cases e : a.val.toNat = k
      · subst e; simp only [ite_true]; exact hrB
      · simp only [e, ite_false]; exact hB k hk)
    (fun k hk => by
      simp only [List.getElem_set]
      by_cases e : a.val.toNat = k
      · subst e; simp only [ite_true]; exact hrn
      · simp only [e, ite_false])
    (by simp only [List.getElem_set, ite_true]; exact ⟨hrc, hru, hrd⟩)
  exact ⟨s', by simp [Disk.sync, hopen, Slice.index_ok d.files a ha, hsync, Slice.update, ha,
    Disk.flush, hfl], hl', hB', hn', hq'⟩

/-- `File::cut` of a `Bounded` file with no change, with `2^66` fuel and any `Kept`: its name and
durable bytes, and no change. -/
theorem cut_nil (fuel : Nat) (f : File) (k : Kept) (hb : Bounded f) (hc : f.changes.val = [])
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ f', File.cut fuel f k = Result.ok f' ∧ f'.name = f.name ∧ f'.changes.val = [] ∧
      f'.durable.val = f.durable.val := by
  obtain ⟨r, hr, hrn, -, h3⟩ := sync_bounded fuel f k.whole hb hfuel
  obtain ⟨hd, hrc, -⟩ := h3 hc
  rcases r with ⟨rn, rd, ⟨rcl, rch⟩, rb, ru⟩
  simp only at hrn hd hrc
  subst hrc
  simp [File.cut, hr, hrn, hd, Vec.clear]

/-! ## 10. A crash keeps a synced file

Fuel: `2^67` (`147573952589676412928`), as for `appended_proof`; the sync and its flush take
`2^66` and more than the files and barriers. -/

theorem synced_proof :
    blaze_core.world.disk.a_crash_keeps_a_synced_file.spec.statement := by
  intro disk file cut
  refine ⟨147573952589676412928, ?_⟩
  have hl0 := disk.files.hlen
  obtain ⟨bw, hwf, hwfs⟩ := wf_spec 147573952589676412928 disk (by omega)
  obtain ⟨bs, hsm, hsms⟩ := small_spec 147573952589676412928 disk (by omega)
  by_cases hgo : bw = true ∧ bs = true
  case neg =>
    cases bw <;> cases bs <;> simp_all [a_crash_keeps_a_synced_file]
  obtain ⟨hbw, hbs⟩ := hgo
  subst hbw hbs
  have hsort := hwfs rfl
  obtain ⟨hlen16, -⟩ := hsms rfl
  have hsmall := small_files _ disk hsm
  obtain ⟨r0, hfind, hnone, hsome⟩ := find_spec 147573952589676412928 disk file (by omega)
  cases r0 with
  | none => simp [a_crash_keeps_a_synced_file, hwf, hsm, hfind]
  | some at0 =>
    obtain ⟨hat, hatn, -⟩ := hsome at0 rfl
    have hS := hsmall _ hat
    have hix := Slice.index_ok disk.files at0 hat
    have hall : ((disk.files.val[at0.val.toNat]).changes.val.length : Int) ≤
        (Slice.len (disk.files.val[at0.val.toNat]).changes).val := by
      simp only [Slice.len_val]; omega
    obtain ⟨-, read, -, hread, -⟩ := sync_all 147573952589676412928 _ _ hS hall (by omega)
    obtain ⟨d1, hds, hl1, hB1, hn1, hq1⟩ := disk_sync 147573952589676412928 disk file
      at0.val.toNat _ hsort hlen16 (fun k hk => small_bounded _ (hsmall k hk)) hat hatn hS hall
      (Nat.le_refl _) read hread
    have hsort1 : Sorted d1.files.val := by
      intro i j hi hj hij
      rw [hn1 i (by omega), hn1 j (by omega)]
      exact hsort i j (by omega) (by omega) hij
    obtain ⟨bal, hal⟩ := allows_total 147573952589676412928 d1 cut (by omega)
    cases bal with
    | false =>
      simp [a_crash_keeps_a_synced_file, hwf, hsm, hfind, hix, hread, hds, hal]
    | true =>
      obtain ⟨hlc, -⟩ := allows_fits _ d1 cut hal
      have hat1 : at0.val.toNat < d1.files.val.length := by omega
      obtain ⟨hq1c, hq1u, hq1d⟩ := hq1
      obtain ⟨d', hcr, hs', F, hF, hFn, hFc, hFd⟩ :=
        crash_spec 147573952589676412928 d1 cut
          (fun F => F.changes.val = [] ∧ F.durable.val = read.val)
          at0.val.toNat hsort1 hlc (by omega) (fun k hk => hB1 k (by omega)) hat1 hq1u
          (fun n hn => by
            rw [with_undo_none _ hq1u]
            obtain ⟨f', h1, -, h2, h3⟩ := cut_nil n _ (cut.val[at0.val.toNat]'(by omega))
              (hB1 _ hat) hq1c hn
            exact ⟨f', h1, h2, h3.trans hq1d⟩)
          (Nat.le_refl _)
      have hFn' : F.name.val = file.val := by
        rw [hFn, hn1 _ hat]; exact hatn
      obtain ⟨v, hcont, hv⟩ := contents_of_mem 147573952589676412928 d' F file hs' hF hFn' hFc
        (by have := d'.files.hlen; omega)
      obtain ⟨b, hb, hbiff⟩ := kept_spec 147573952589676412928 read (some v) (by omega)
      have hbt : b = true := hbiff.mpr ⟨v, rfl, by rw [hv, hFd]⟩
      subst hbt
      simp [a_crash_keeps_a_synced_file, hwf, hsm, hfind, hix, hread, hds, hal, hcr, hcont, hb]

end BlazeCore.World.DiskCrash
