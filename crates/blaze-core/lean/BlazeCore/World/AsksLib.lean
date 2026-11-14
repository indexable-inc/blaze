import Blaze.Auto
import BlazeCore.World.DiskCrash
import BlazeModel.blaze_core.world.asks.a_synced_append_survives_a_crash

/-!
The disk-level lemmas `BlazeCore.World.Asks` needs beyond `DiskMap`, `DiskBytes` and
`DiskCrash` (written by claude-opus-5-5, lane asksproof):

- `sync_spec_b`, `grown_sync`, `flush_loop_grown`, `disk_sync_grown`: a sync of an
  only-appended file with nothing to undo, on a disk whose files are only `Bounded` (not
  `Small`): it makes some first changes durable, at least as many as it covers, and the flush
  after it may make more.
- `cut_bounded`: a crash's cut of a `Bounded`, only-appended file keeps its durable bytes.
- `open_sorted`, `append_sorted`, `append_there`: `Disk::append` keeps the names sorted; to a
  file that is there it adds one change at the end and nothing else.
- `copy_bytes_id`, `copy_change_id`, `copy_file_id`, `copy_disk_id`: the copies are the
  identity.
-/

open Blaze Std.WP Lean.Order

namespace BlazeCore.World.AsksLib

open blaze_core.world.disk blaze_core.world.asks
open BlazeCore.World.DiskAllows (add_one usize_zero u64_zero)
open BlazeCore.World.DiskMap (Sorted find_spec slot_spec open_spec change_spec)
open BlazeCore.World.DiskBytes
open BlazeCore.World.DiskCrash (Bounded sync_bounded small_files sorted_insertIdx)

/-! ## 1. `File::sync` of a `Bounded` file -/

/-- `File::sync` of a `Bounded` file, with `2^66` fuel: `sync_spec` without its barriers, for a
file that need not be `Small`. -/
theorem sync_spec_b (fuel : Nat) (f : File) (covers : Usize) (hb : Bounded f)
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ r, File.sync fuel f covers = Result.ok r ∧ r.name = f.name ∧
      r.durable.val = (f.changes.val.take (min covers.val.toNat f.changes.val.length)).foldl
        stepL f.durable.val ∧
      r.changes.val = f.changes.val.drop (min covers.val.toNat f.changes.val.length) ∧
      r.undo = (if f.changes.val.drop (min covers.val.toNat f.changes.val.length) = [] then none
        else f.undo) := by
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
  by_cases hemp : r1.changes.val = []
  · refine ⟨{ r1 with undo := none }, ?_, by simp only; rw [b1, a1], by simp only; rw [b2, a5],
      by simp only; rw [hchg], ?_⟩
    · simp only [File.sync, hcv_eq, hr0, hr1, Result.bind_ok]
      simp [hemp]
    · rw [← hchg]; simp [hemp]
  · refine ⟨r1, ?_, by rw [b1, a1], by rw [b2, a5], hchg, ?_⟩
    · simp only [File.sync, hcv_eq, hr0, hr1, Result.bind_ok]
      simp [hemp]
    · rw [← hchg]; simp [hemp, b4, a3]

/-- `f` is what syncs left of the only-appended file `f0`: its name, nothing to undo, and of
`f0`'s changes the first `k` durable, `k` at least `m`, and the rest left. -/
def Grown (f0 : File) (m : Nat) (f : File) : Prop :=
  f.name = f0.name ∧ f.undo = none ∧ ∃ k, m ≤ k ∧ k ≤ f0.changes.val.length ∧
    f.durable.val = f0.durable.val ++ appendedOf (f0.changes.val.take k) ∧
    f.changes.val = f0.changes.val.drop k

theorem grown_self (f0 : File) (hu : f0.undo = none) : Grown f0 0 f0 :=
  ⟨rfl, hu, 0, Nat.le_refl _, Nat.zero_le _, by simp [appendedOf], by simp⟩

theorem grown_mono {f0 f : File} {m m' : Nat} (hm : m' ≤ m) (h : Grown f0 m f) : Grown f0 m' f := by
  obtain ⟨h1, h2, k, hk, h3⟩ := h
  exact ⟨h1, h2, k, by omega, h3⟩

theorem grown_append {f0 f : File} {m : Nat} (ha : AllAppend f0) (h : Grown f0 m f) :
    AllAppend f := by
  obtain ⟨-, -, k, -, -, -, hc⟩ := h
  intro x hx
  rw [hc] at hx
  exact ha x (List.mem_of_mem_drop hx)

theorem appendedOf_take_add (l : List Made) (k j : Nat) :
    appendedOf (l.take (k + j)) = appendedOf (l.take k) ++ appendedOf ((l.drop k).take j) := by
  rw [List.take_add, appendedOf_append]

/-- A sync of a `Bounded` file grown from the only-appended `f0`, with `2^66` fuel: a `Bounded`
file grown from `f0` by as many more as it covers. -/
theorem grown_sync (fuel : Nat) (f0 f : File) (m : Nat) (covers : Usize) (ha : AllAppend f0)
    (hb : Bounded f) (hg : Grown f0 m f) (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ r, File.sync fuel f covers = Result.ok r ∧ Bounded r ∧
      Grown f0 (m + min covers.val.toNat f.changes.val.length) r := by
  have hfa := grown_append ha hg
  obtain ⟨r, hr, h1, h2, h3, h4⟩ := sync_spec_b fuel f covers hb hfuel
  obtain ⟨r', hr', -, hB, -⟩ := sync_bounded fuel f covers hb hfuel
  rw [hr] at hr'
  cases hr'
  obtain ⟨hn, hu, k, hmk, hkl, hd, hc⟩ := hg
  have hfl : f.changes.val.length = f0.changes.val.length - k := by rw [hc, List.length_drop]
  refine ⟨r, hr, hB, h1.trans hn, ?_, k + min covers.val.toNat f.changes.val.length,
    by omega, by omega, ?_, ?_⟩
  · rw [h4]; split <;> simp [hu]
  · rw [h2, foldl_appends _ _ (fun x hx => hfa x (List.mem_of_mem_take hx)), hd,
      appendedOf_take_add, hc, List.append_assoc]
  · rw [h3, hc, List.drop_drop]

/-! ## 2. `Disk::flush` and `Disk::sync` keep a file grown -/

/-- The loop of `Disk::flush` from `i` keeps every file's name and place and every file
`Bounded`, and a file at `p` grown from the only-appended `f0` stays so. -/
theorem flush_loop_grown (d : Disk) (f0 : File) (m : Nat) (ha : AllAppend f0) (p : Nat)
    (hp : p < d.files.val.length) :
    ∀ (fuel : Nat) (self : Disk) (i : Usize)
      (hl : self.files.val.length = d.files.val.length),
      i.val.toNat ≤ d.files.val.length →
      (d.files.val.length - i.val.toNat) + 73786976294838206464 < fuel →
      (∀ k (hk : k < d.files.val.length), Bounded (self.files.val[k]'(by omega))) →
      (∀ k (hk : k < d.files.val.length),
        (self.files.val[k]'(by omega)).name = (d.files.val[k]).name) →
      Grown f0 m (self.files.val[p]'(by omega)) →
      ∃ s', Disk.flush.loop_0 fuel self i = Result.ok s' ∧
        ∃ hl' : s'.files.val.length = d.files.val.length,
        (∀ k (hk : k < d.files.val.length), Bounded (s'.files.val[k]'(by omega))) ∧
        (∀ k (hk : k < d.files.val.length),
          (s'.files.val[k]'(by omega)).name = (d.files.val[k]).name) ∧
        Grown f0 m (s'.files.val[p]'(by omega)) := by
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
      · obtain ⟨c, hc⟩ := BlazeCore.World.DiskCrash.ordered_loop_total
          self.files.val[i.val.toNat] n 0 0
          (by rw [usize_zero]; omega)
          (by have := (self.files.val[i.val.toNat]).barriers.hlen; rw [usize_zero]; omega)
        obtain ⟨r, hr, hrn, hrB, -⟩ := sync_bounded n self.files.val[i.val.toNat] c
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
            obtain ⟨r', hr', -, hg'⟩ := grown_sync n f0 _ m c ha (hB _ hlt) hq (by omega)
            rw [hr] at hr'
            cases hr'
            exact grown_mono (by omega) hg'
          · simp only [e, ite_false]; exact hq
    · have hstop : ¬ i.val < (self.files.val.length : Int) := by omega
      exact ⟨self, by simp [Disk.flush.loop_0, Scalar.lt, Slice.len_val, hstop], hl, hB, hname,
        hq⟩

/-- `Disk::sync` of the name of the file at `p`, only appended to and with nothing to undo, on a
disk whose names strictly increase, with at most `2^16` files, all `Bounded`, and `2^67` fuel:
every file keeps its name and place and stays `Bounded`, and the file at `p` is grown from what
it was by at least as many changes as the sync covers. -/
theorem disk_sync_grown (fuel : Nat) (d : Disk) (N : U64) (p : Nat) (covers : Usize)
    (hs : Sorted d.files.val) (hL : d.files.val.length ≤ 65536)
    (hB : ∀ k (hk : k < d.files.val.length), Bounded d.files.val[k])
    (hp : p < d.files.val.length) (hpn : (d.files.val[p]).name.val = N.val)
    (ha : AllAppend d.files.val[p]) (hu : (d.files.val[p]).undo = none)
    (hfuel : 147573952589676412928 ≤ fuel) :
    ∃ d', Disk.sync fuel d N covers = Result.ok d' ∧
      ∃ hl' : d'.files.val.length = d.files.val.length,
      (∀ k (hk : k < d.files.val.length), Bounded (d'.files.val[k]'(by omega))) ∧
      (∀ k (hk : k < d.files.val.length),
        (d'.files.val[k]'(by omega)).name = (d.files.val[k]).name) ∧
      Grown d.files.val[p] (min covers.val.toNat (d.files.val[p]).changes.val.length)
        (d'.files.val[p]'(by omega)) := by
  obtain ⟨a, d1, hopen, -, -, ⟨ha', han⟩, hsame, -⟩ :=
    open_spec fuel d N hs (by omega) (by omega)
  have hd1 := (hsame ⟨p, hp, hpn⟩).symm
  subst hd1
  have e : a.val.toNat = p := hs.unique _ _ ha' hp (han.trans hpn.symm)
  subst e
  obtain ⟨r, hsync, hrB, hrg⟩ := grown_sync fuel d.files.val[a.val.toNat] _ 0 covers ha
    (hB _ ha') (grown_self _ hu) (by omega)
  have hlen2 : (d.files.val.set a.val.toNat r).length ≤ 18446744073709551615 := by
    simp only [List.length_set]; exact d.files.hlen
  obtain ⟨s', hfl, hl', hB', hn', hq'⟩ := flush_loop_grown d d.files.val[a.val.toNat]
    (0 + min covers.val.toNat (d.files.val[a.val.toNat]).changes.val.length) ha a.val.toNat ha'
    fuel { d with files := ⟨d.files.val.set a.val.toNat r, hlen2⟩ } 0
    (by simp only [List.length_set]) (by rw [usize_zero]; omega) (by rw [usize_zero]; omega)
    (fun k hk => by
      simp only [List.getElem_set]
      by_cases e : a.val.toNat = k
      · subst e; simp only [ite_true]; exact hrB
      · simp only [e, ite_false]; exact hB k hk)
    (fun k hk => by
      simp only [List.getElem_set]
      by_cases e : a.val.toNat = k
      · subst e; simp only [ite_true]; exact hrg.1
      · simp only [e, ite_false])
    (by simp only [List.getElem_set, ite_true]; exact hrg)
  refine ⟨s', by simp [Disk.sync, hopen, Slice.index_ok d.files a ha', hsync, Slice.update, ha',
    Disk.flush, hfl], hl', hB', hn', grown_mono (by omega) hq'⟩

/-! ## 3. A crash's cut of a `Bounded`, only-appended file -/

/-- `File::cut` of a `Bounded` file whose changes are all appends, with `2^66` fuel and any
`Kept`: no change left, and its durable bytes still first. -/
theorem cut_bounded (fuel : Nat) (f : File) (k : Kept) (hb : Bounded f) (ha : AllAppend f)
    (hfuel : 73786976294838206464 ≤ fuel) :
    ∃ f', File.cut fuel f k = Result.ok f' ∧ f'.changes.val = [] ∧
      f.durable.val <+: f'.durable.val := by
  have hb' := hb
  obtain ⟨hd, hcl, hsm⟩ := hb'
  obtain ⟨r, hr, -, h2, h3, -⟩ := sync_spec_b fuel f k.whole hb hfuel
  rw [foldl_appends _ _ (fun x hx => ha x (List.mem_of_mem_take hx))] at h2
  have hA := appendedOf_length (f.changes.val.take (min k.whole.val.toNat f.changes.val.length))
    (fun m hm => hsm m (List.mem_of_mem_take hm))
  have htl : (f.changes.val.take (min k.whole.val.toNat f.changes.val.length)).length ≤ 65536 := by
    simp only [List.length_take]; omega
  rcases r with ⟨rn, rd, ⟨rcl, rch⟩, rb, ru⟩
  simp only at h2 h3
  cases rcl with
  | nil =>
    refine ⟨⟨rn, rd, Vec.new, Vec.new, none⟩, ?_, rfl, ?_⟩
    · simp [File.cut, hr, Vec.clear]
    · show f.durable.val <+: rd.val
      rw [h2]; exact List.prefix_append _ _
  | cons m rest =>
    have hmd : m ∈ f.changes.val.drop (min k.whole.val.toNat f.changes.val.length) := by
      rw [← h3]; simp
    have hap := ha m (List.mem_of_mem_drop hmd)
    cases hc : m.change with
    | At a more => rw [hc] at hap; simp [isAppend] at hap
    | Sectors a more => rw [hc] at hap; simp [isAppend] at hap
    | Cut len => rw [hc] at hap; simp [isAppend] at hap
    | Append more =>
      have hml : more.val.length ≤ 1099511627776 := by
        have := hsm m (List.mem_of_mem_drop hmd); rw [hc] at this; exact this
      cases ht : k.torn with
      | Nothing =>
        refine ⟨⟨rn, rd, Vec.new, Vec.new, none⟩, ?_, rfl, ?_⟩
        · simp [File.cut, hr, ht, Vec.clear, Vec.remove, usize_zero]
        · show f.durable.val <+: rd.val
          rw [h2]; exact List.prefix_append _ _
      | Sectors l =>
        refine ⟨⟨rn, rd, Vec.new, Vec.new, none⟩, ?_, rfl, ?_⟩
        · simp [File.cut, hr, ht, hc, Vec.clear, Vec.remove, usize_zero]
        · show f.durable.val <+: rd.val
          rw [h2]; exact List.prefix_append _ _
      | Prefix count =>
        have hrd : rd.val.length ≤ 576460752303423488 + 1099511627776 * 65536 := by
          rw [h2, List.length_append]
          have := Nat.mul_le_mul_left 1099511627776 htl
          omega
        obtain ⟨x3, hx3, hx3v⟩ := extend_spec fuel rd more count (by omega) (by omega)
        refine ⟨⟨rn, x3, Vec.new, Vec.new, none⟩, ?_, rfl, ?_⟩
        · simp [File.cut, hr, ht, hc, Vec.clear, Vec.remove, usize_zero, hx3]
        · show f.durable.val <+: x3.val
          rw [hx3v, h2, List.append_assoc]; exact List.prefix_append _ _

/-! ## 4. `Disk::append` -/

/-- `Disk::open` on a disk whose names strictly increase: the names still strictly increase, the
epoch stays, and the file at the place it gives has the name and, where every file had fewer
than `usize::MAX` changes, fewer than that. -/
theorem open_sorted (fuel : Nat) (d : Disk) (name : U64) (hs : Sorted d.files.val)
    (hl : d.files.val.length < 18446744073709551615) (h : d.files.val.length < fuel)
    (hc : ∀ k (hk : k < d.files.val.length),
      (d.files.val[k]).changes.val.length < 18446744073709551615) :
    ∃ (a : Usize) (d' : Disk), Disk.«open» fuel d name = Result.ok (a, d') ∧
      Sorted d'.files.val ∧ d'.epoch = d.epoch ∧ ∃ ha : a.val.toNat < d'.files.val.length,
      (d'.files.val[a.val.toNat]).changes.val.length < 18446744073709551615 := by
  obtain ⟨s, hsq, hs1, h2, h3⟩ := slot_spec fuel d name h
  have hs0 := s.hmin
  simp only [ScalarTy.min] at hs0
  by_cases hp : ∃ hsl : s.val.toNat < d.files.val.length,
      (d.files.val[s.val.toNat]).name.val = name.val
  · obtain ⟨hsl, he⟩ := hp
    have hsv : ¬ s.val = (d.files.val.length : Int) := by omega
    refine ⟨s, d, ?_, hs, rfl, hsl, hc _ hsl⟩
    simp [Disk.«open», hsq, hsv, Slice.index, hsl, he]
  · have hins : s.val.toNat ≤ d.files.val.length ∧ d.files.val.length < 18446744073709551615 :=
      ⟨hs1, hl⟩
    have hlen := List.length_insertIdx_of_le_length hs1 (File.whole name Vec.new)
    refine ⟨s, ⟨⟨d.files.val.insertIdx s.val.toNat (File.whole name Vec.new), by omega⟩, d.epoch⟩,
      ?_, ?_, rfl, by simp only [hlen]; omega, ?_⟩
    · by_cases hsl : s.val.toNat < d.files.val.length
      · have hsv : ¬ s.val = (d.files.val.length : Int) := by omega
        have hne : ¬ (d.files.val[s.val.toNat]).name.val = name.val := fun e => hp ⟨hsl, e⟩
        have hsle : s.val ≤ (d.files.val.length : Int) := by omega
        simp [Disk.«open», hsq, hsv, Slice.index, hsl, hne, Vec.insert, hins, hsle]
      · have hsv : s.val = (d.files.val.length : Int) := by omega
        simp [Disk.«open», hsq, hsv, Vec.insert, hins]
    · refine sorted_insertIdx hs _ _ hs1 (fun k hk hks => h2 k hk hks) (fun hsl => ?_)
      have hle := h3 hsl
      have hne : (d.files.val[s.val.toNat]).name.val ≠ name.val := fun e => hp ⟨hsl, e⟩
      show name.val < _
      omega
    · simp [List.getElem_insertIdx_self, File.whole, Vec.new]

/-- `Disk::append` on a disk whose names strictly increase, with fewer than `usize::MAX` files
and changes of each: it never panics nor runs out of fuel, and the names still strictly
increase. -/
theorem append_sorted (fuel : Nat) (d : Disk) (name : U64) (bytes : Vec U8)
    (hs : Sorted d.files.val) (hl : d.files.val.length < 18446744073709551615)
    (hf : d.files.val.length < fuel)
    (hc : ∀ k (hk : k < d.files.val.length),
      (d.files.val[k]).changes.val.length < 18446744073709551615) :
    ∃ d', Disk.append fuel d name bytes = Result.ok d' ∧ Sorted d'.files.val := by
  obtain ⟨a, d1, hop, hs1, -, ha, hca⟩ := open_sorted fuel d name hs hl hf hc
  obtain ⟨d', hch, -, hlen', hprop⟩ := change_spec d1 a (Change.Append bytes) ha hca
  refine ⟨d', by simp [Disk.append, hop, hch], ?_⟩
  have hn : ∀ k (hk : k < d'.files.val.length),
      (d'.files.val[k]).name = (d1.files.val[k]'(by omega)).name := by
    intro k hk
    by_cases e : k = a.val.toNat
    · exact ((hprop k (by omega)).2 e).1
    · rw [(hprop k (by omega)).1 e]
  intro i j hi hj hij
  rw [hn i hi, hn j hj]
  exact hs1 i j (by omega) (by omega) hij

/-- `Disk::change` at `a`, exactly: the file there with the change after its changes. -/
theorem change_eq (d : Disk) (a : Usize) (c : Change) (ha : a.val.toNat < d.files.val.length)
    (hc : (d.files.val[a.val.toNat]).changes.val.length < 18446744073709551615) :
    ∃ d', Disk.change d a c = Result.ok d' ∧ d'.epoch = d.epoch ∧ ∃ cs : Vec Made,
      cs.val = (d.files.val[a.val.toNat]).changes.val ++ [⟨c, d.epoch⟩] ∧
      d'.files.val = d.files.val.set a.val.toNat { d.files.val[a.val.toNat] with changes := cs } := by
  have hl := d.files.hlen
  refine ⟨⟨⟨d.files.val.set a.val.toNat { d.files.val[a.val.toNat] with
      changes := ⟨(d.files.val[a.val.toNat]).changes.val ++ [⟨c, d.epoch⟩],
        by simp only [List.length_append, List.length_singleton]; omega⟩ },
      by simp only [List.length_set]; omega⟩, d.epoch⟩, ?_, rfl, _, rfl, rfl⟩
  simp [Disk.change, Slice.index, ha, Vec.push, hc, Slice.update]

/-- `Disk::append` to the file at `p`, on a disk whose names strictly increase: that file with
the append after its changes, every other file as it was, the epoch kept. -/
theorem append_there (fuel : Nat) (d : Disk) (name : U64) (bytes : Vec U8) (p : Nat)
    (hs : Sorted d.files.val) (hp : p < d.files.val.length)
    (hpn : (d.files.val[p]).name.val = name.val)
    (hl : d.files.val.length < 18446744073709551615) (hf : d.files.val.length < fuel)
    (hc : (d.files.val[p]).changes.val.length < 18446744073709551615) :
    ∃ d', Disk.append fuel d name bytes = Result.ok d' ∧ d'.epoch = d.epoch ∧ ∃ cs : Vec Made,
      cs.val = (d.files.val[p]).changes.val ++ [⟨Change.Append bytes, d.epoch⟩] ∧
      d'.files.val = d.files.val.set p { d.files.val[p] with changes := cs } := by
  obtain ⟨a, d1, hopen, -, -, ⟨ha', han⟩, hsame, -⟩ := open_spec fuel d name hs hl hf
  have hd1 := (hsame ⟨p, hp, hpn⟩).symm
  subst hd1
  have e : a.val.toNat = p := hs.unique _ _ ha' hp (han.trans hpn.symm)
  subst e
  obtain ⟨d', hch, hep, cs, hcs, hfs⟩ := change_eq d a (Change.Append bytes) ha' hc
  exact ⟨d', by simp [Disk.append, hopen, hch], hep, cs, hcs, hfs⟩

/-! ## 5. The copies are the identity -/

theorem slice_ext {α : Type} {a b : Slice α} (h : a.val = b.val) : a = b := by
  cases a; cases b; simp only at h; subst h; rfl

/-- The loop of `prefix` from `i`: the first `count` bytes. -/
theorem prefix_loop (b : Slice U8) (c : Usize) :
    ∀ (fuel : Nat) (out : Vec U8) (i : Usize),
      i.val.toNat ≤ min c.val.toNat b.val.length → out.val = b.val.take i.val.toNat →
      min c.val.toNat b.val.length - i.val.toNat < fuel →
      ∃ v, «prefix».loop_0 fuel b c out i = Result.ok v ∧ v.val = b.val.take c.val.toNat := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ h; omega
  | succ n ih =>
    intro out i hi ho hfuel
    have hi0 := i.hmin
    have hc0 := c.hmin
    have hbl := b.hlen
    simp only [ScalarTy.min] at hi0 hc0
    by_cases hlt : i.val.toNat < min c.val.toNat b.val.length
    · have h1 : i.val < c.val := by omega
      have h2 : i.val < (b.val.length : Int) := by omega
      have hib : i.val.toNat < b.val.length := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hol : out.val.length < 18446744073709551615 := by
        rw [ho, List.length_take]; omega
      obtain ⟨v, hv, hvv⟩ := ih ⟨out.val ++ [b.val[i.val.toNat]], by
          simp only [List.length_append, List.length_singleton]; omega⟩ j (by omega)
        (by simp only [ho]; rw [show j.val.toNat = i.val.toNat + 1 by omega,
          List.take_succ_eq_append_getElem hib]) (by omega)
      refine ⟨v, ?_, hvv⟩
      simp [«prefix».loop_0, Scalar.lt, h1, h2, Slice.len_val, Slice.index, hib, Vec.push, hol, hj,
        hv]
    · have hstop : ¬ (i.val < c.val ∧ i.val < (b.val.length : Int)) := by omega
      refine ⟨out, ?_, ?_⟩
      · by_cases h1 : i.val < c.val
        · have h2 : ¬ i.val < (b.val.length : Int) := by omega
          simp [«prefix».loop_0, Scalar.lt, h1, h2, Slice.len_val]
        · simp [«prefix».loop_0, Scalar.lt, h1]
      · rw [ho]
        by_cases hcb : c.val.toNat ≤ b.val.length
        · congr 1; omega
        · rw [List.take_of_length_le (by omega), List.take_of_length_le (by omega)]

/-- `copy_bytes`, with more than `2^64` fuel, is the identity. -/
theorem copy_bytes_id (fuel : Nat) (b : Slice U8) (h : 18446744073709551616 < fuel) :
    copy_bytes fuel b = Result.ok b := by
  have hbl := b.hlen
  obtain ⟨v, hv, hvv⟩ := prefix_loop b (Slice.len b) fuel Vec.new 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; simp) (by rw [usize_zero]; omega)
  have : v = b := slice_ext (by rw [hvv]; simp [Slice.len_val])
  subst this
  simpa [copy_bytes, «prefix»] using hv

/-- `copy_change`, with more than `2^64` fuel, is the identity. -/
theorem copy_change_id (fuel : Nat) (c : Change) (h : 18446744073709551616 < fuel) :
    copy_change fuel c = Result.ok c := by
  cases c <;> simp [copy_change, copy_bytes_id fuel _ h]

/-- The first loop of `copy_file` from `i`: the file's changes. -/
theorem copy_file_loop0 (f : File) :
    ∀ (fuel : Nat) (changes : Vec Made) (i : Usize), i.val.toNat ≤ f.changes.val.length →
      changes.val = f.changes.val.take i.val.toNat →
      f.changes.val.length - i.val.toNat + 18446744073709551617 < fuel →
      copy_file.loop_0 fuel f changes i = Result.ok f.changes := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ h; omega
  | succ n ih =>
    intro changes i hi hc hfuel
    have hi0 := i.hmin
    have hl := f.changes.hlen
    simp only [ScalarTy.min] at hi0
    by_cases hlt : i.val.toNat < f.changes.val.length
    · have hiv : i.val < (f.changes.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hcl : changes.val.length < 18446744073709551615 := by
        rw [hc, List.length_take]; omega
      have hrec := ih ⟨changes.val ++ [f.changes.val[i.val.toNat]], by
          simp only [List.length_append, List.length_singleton]; omega⟩ j (by omega)
        (by simp only [hc]; rw [show j.val.toNat = i.val.toNat + 1 by omega,
          List.take_succ_eq_append_getElem hlt]) (by omega)
      simp [copy_file.loop_0, Scalar.lt, hiv, Slice.len_val, Slice.index, hlt,
        copy_change_id n _ (by omega), Vec.push, hcl, hj, hrec]
    · have hiv : ¬ i.val < (f.changes.val.length : Int) := by omega
      have : changes = f.changes := slice_ext (by
        rw [hc, List.take_of_length_le (by omega)])
      subst this
      simp [copy_file.loop_0, Scalar.lt, hiv, Slice.len_val]

/-- The second loop of `copy_file` from `i`: the file's barriers. -/
theorem copy_file_loop1 (f : File) :
    ∀ (fuel : Nat) (barriers : Vec Barrier) (i : Usize), i.val.toNat ≤ f.barriers.val.length →
      barriers.val = f.barriers.val.take i.val.toNat →
      f.barriers.val.length - i.val.toNat < fuel →
      copy_file.loop_1 fuel f i barriers = Result.ok f.barriers := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ h; omega
  | succ n ih =>
    intro barriers i hi hc hfuel
    have hi0 := i.hmin
    have hl := f.barriers.hlen
    simp only [ScalarTy.min] at hi0
    by_cases hlt : i.val.toNat < f.barriers.val.length
    · have hiv : i.val < (f.barriers.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hcl : barriers.val.length < 18446744073709551615 := by
        rw [hc, List.length_take]; omega
      have hrec := ih ⟨barriers.val ++ [f.barriers.val[i.val.toNat]], by
          simp only [List.length_append, List.length_singleton]; omega⟩ j (by omega)
        (by simp only [hc]; rw [show j.val.toNat = i.val.toNat + 1 by omega,
          List.take_succ_eq_append_getElem hlt]) (by omega)
      simp [copy_file.loop_1, Scalar.lt, hiv, Slice.len_val, Slice.index, hlt, Vec.push, hcl, hj,
        hrec]
    · have hiv : ¬ i.val < (f.barriers.val.length : Int) := by omega
      have : barriers = f.barriers := slice_ext (by
        rw [hc, List.take_of_length_le (by omega)])
      subst this
      simp [copy_file.loop_1, Scalar.lt, hiv, Slice.len_val]

/-- `copy_file`, with `2^66` fuel, is the identity. -/
theorem copy_file_id (fuel : Nat) (f : File) (h : 73786976294838206464 ≤ fuel) :
    copy_file fuel f = Result.ok f := by
  have h0 := copy_file_loop0 f fuel Vec.new 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; simp) (by have := f.changes.hlen; rw [usize_zero]; omega)
  have h1 := copy_file_loop1 f fuel Vec.new 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; simp) (by have := f.barriers.hlen; rw [usize_zero]; omega)
  rcases f with ⟨name, durable, changes, barriers, undo⟩
  rcases undo with _ | ⟨was, fr⟩
  · simp only at h0 h1
    simp [copy_file, h0, h1, copy_bytes_id fuel _ (by omega)]
  · rcases was with _ | w
    · simp only at h0 h1
      simp [copy_file, h0, h1, copy_bytes_id fuel _ (by omega)]
    · simp only at h0 h1
      simp [copy_file, h0, h1, copy_bytes_id fuel _ (by omega)]

/-- The loop of `copy_disk` from `i`: the disk's files. -/
theorem copy_disk_loop (d : Disk) :
    ∀ (fuel : Nat) (files : Vec File) (i : Usize), i.val.toNat ≤ d.files.val.length →
      files.val = d.files.val.take i.val.toNat →
      d.files.val.length - i.val.toNat + 73786976294838206464 < fuel →
      copy_disk.loop_0 fuel d files i = Result.ok d.files := by
  intro fuel
  induction fuel with
  | zero => intro _ _ _ _ h; omega
  | succ n ih =>
    intro files i hi hc hfuel
    have hi0 := i.hmin
    have hl := d.files.hlen
    simp only [ScalarTy.min] at hi0
    by_cases hlt : i.val.toNat < d.files.val.length
    · have hiv : i.val < (d.files.val.length : Int) := by omega
      obtain ⟨j, hj, hjv⟩ := add_one i (by omega)
      have hcl : files.val.length < 18446744073709551615 := by
        rw [hc, List.length_take]; omega
      have hrec := ih ⟨files.val ++ [d.files.val[i.val.toNat]], by
          simp only [List.length_append, List.length_singleton]; omega⟩ j (by omega)
        (by simp only [hc]; rw [show j.val.toNat = i.val.toNat + 1 by omega,
          List.take_succ_eq_append_getElem hlt]) (by omega)
      simp [copy_disk.loop_0, Scalar.lt, hiv, Slice.len_val, Slice.index, hlt,
        copy_file_id n _ (by omega), Vec.push, hcl, hj, hrec]
    · have hiv : ¬ i.val < (d.files.val.length : Int) := by omega
      have : files = d.files := slice_ext (by
        rw [hc, List.take_of_length_le (by omega)])
      subst this
      simp [copy_disk.loop_0, Scalar.lt, hiv, Slice.len_val]

/-- `copy_disk`, with `2^67` fuel, is the identity. -/
theorem copy_disk_id (fuel : Nat) (d : Disk) (h : 147573952589676412928 ≤ fuel) :
    copy_disk fuel d = Result.ok d := by
  have h0 := copy_disk_loop d fuel Vec.new 0 (by rw [usize_zero]; omega)
    (by rw [usize_zero]; simp) (by have := d.files.hlen; rw [usize_zero]; omega)
  simp [copy_disk, h0]

end BlazeCore.World.AsksLib
