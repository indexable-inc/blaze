import BlazeCore.Handoff

/-!
No hand-back is missed, on every schedule and every pick of the weak-memory world: the proof
over a hand-written model shaped like `blaze_core::handoff::Wake` (crates/blaze-core/src/
handoff.rs), whose orderings are a parameter: with `AcqRel` it is the theorem's shape, and with
`Relaxed` it is the control, a run by evaluation in which a hand-back is missed. It makes its
cells in the real `hands`'s order (the flag, cell 0, then the slot, cell 1) and has its
oracle, so the invariant and step lemmas are the ones the emitted model's proof uses
(`BlazeCore.Handoff`): this file gives the points and their shapes.

The sender hands back (a relaxed store of its slot) and then calls `Wake::wake`
(`!woken.swap(true, AcqRel)`): whether it must wake the driver. The driver calls `Wake::drain`
(`woken.swap(false, AcqRel)`) and then reads the slot (relaxed). "None is missed": where both
returned, the driver saw the slot or the sender was told to wake it.

Why: the flag's two swaps are updates of one cell, so they read different messages, the second
the first's. If the sender's is first, the driver's reads the sender's message and acquires its
view, which names the sender's store of the slot: the driver's load cannot read the slot's first
message. If the driver's is first, the sender's reads the driver's message, `false`: the sender
is told to wake. With `Relaxed` swaps the first case fails (the control below).
-/

namespace BlazeCore.HandoffModel

open Blaze Blaze.Threads Blaze.core.sync.atomic

/-- The sender: hands back (a relaxed store of its slot), then `Wake::wake`: whether it must
wake the driver. -/
def sender (o : core.sync.atomic.Ordering) (slot : Atomic .U32) (woken : AtomicBool) : Prog Bool := do
  Atomic.store slot 1 .Relaxed
  let was ← AtomicBool.swap woken true o
  pure (!was)

/-- The driver: `Wake::drain`, then reads what was handed back. -/
def driver (o : core.sync.atomic.Ordering) (slot : Atomic .U32) (woken : AtomicBool) : Prog U32 := do
  let _ ← AtomicBool.swap woken false o
  Atomic.load slot .Relaxed

/-- The oracle, as the real `hands` has it: where both returned, whether the driver saw the slot
or the sender was told to wake it; every other end is `true`. -/
def oracle : Ended Bool → Ended U32 → Bool
  | .Done must, .Done seen => if Scalar.eq seen 1 then true else must
  | _, _ => true

/-- Whether a run by `schedule`, from a flag that holds `flagged`, misses no hand-back: where
both returned, the driver saw the slot or the sender was told to wake it. -/
def hands (o : core.sync.atomic.Ordering) (schedule : List Step) (flagged : Bool) : Prog Bool := do
  let woken ← AtomicBool.new flagged
  let slot ← Atomic.new (0 : U32)
  let (sent, drained) ← two schedule (sender o slot woken) (driver o slot woken)
  pure (oracle sent drained)

/-- The sender goes, with the world's pick. -/
def tx (pick : Nat := 0) : Step := ⟨false, Scalar.lit .U8 pick⟩
/-- The driver goes, with the world's pick. -/
def rx (pick : Nat := 0) : Step := ⟨true, Scalar.lit .U8 pick⟩

/-! Evaluation first. A load's pick 1 tries the slot's first message; a swap's pick 1 tries the
flag's older message (glued: it reads the newest). -/

-- The sender's swap first, the flag clear: told to wake; the driver's load, trying the stale
-- message, reads the hand-back anyway (its swap acquired the sender's view).
example : (hands .AcqRel [tx, tx, rx, rx 1] false).valueWeak = .ok true := by rfl
-- The flag already set: not told to wake, and the driver reads the hand-back.
example : (hands .AcqRel [tx, tx, rx, rx 1] true).valueWeak = .ok true := by rfl
example : (hands .AcqRel [tx, tx, rx 1, rx 1] true).valueWeak = .ok true := by rfl
-- The driver's swap first and its load reads the slot's first message: the sender's swap,
-- even trying the flag's older message, reads the driver's `false` and is told to wake.
example : (hands .AcqRel [tx, rx, rx 1, tx 1] true).valueWeak = .ok true := by rfl
example : (hands .AcqRel [rx, rx 1, tx, tx 1] true).valueWeak = .ok true := by rfl
example : (hands .AcqRel [rx, tx, tx, rx 1] false).valueWeak = .ok true := by rfl

/-! The control: with relaxed swaps, the sender's swap first with the flag already set (not
told to wake), the driver's swap acquires nothing and its load reads the slot's first message. -/

example : (hands .Relaxed [tx, tx, rx, rx 1] true).valueWeak = .ok false := by rfl
-- The same run with the load's pick 0 reads the hand-back.
example : (hands .Relaxed [tx, tx, rx, rx 0] true).valueWeak = .ok true := by rfl

namespace Proof

open BlazeCore.HandoffProof

/-- The slot is cell 1, the flag cell 0. -/
def senderAt : SPt → Prog Bool
  | .start => sender .AcqRel ⟨1⟩ ⟨0⟩
  | .swap => do
    let was ← AtomicBool.swap ⟨0⟩ true .AcqRel
    pure (!was)
  | .ret must => pure must

def driverAt : DPt → Prog U32
  | .start => driver .AcqRel ⟨1⟩ ⟨0⟩
  | .load => Atomic.load ⟨1⟩ .Relaxed
  | .ret seen => pure seen

theorem shapes : Shapes senderAt driverAt where
  sstart := Prog.op_ext fun _ => rfl
  sswap := Prog.op_ext fun _ => rfl
  sret := fun _ => rfl
  dstart := Prog.op_ext fun _ => rfl
  dload := Prog.op_ext fun _ => rfl
  dret := fun _ => rfl

/-- The oracle is `true` on every end `Post` holds of. -/
theorem oracle_true {ea : Ended Bool} {eb : Ended U32} {w : Weak.World} (h : Post ea eb w) :
    oracle ea eb = true := by
  cases ea with
  | Done must =>
    cases eb with
    | Done seen =>
      have h : seen.val = 1 ∨ must = true := h
      have he : Scalar.eq seen (1 : U32) = decide (seen.val = 1) := by
        simp only [Scalar.eq, one_val]
      simp only [oracle, he]
      by_cases hs : seen.val = 1
      · simp [hs]
      · have hm : must = true := h.resolve_left hs
        simp [hs, hm]
    | Running => rfl
    | Asleep => rfl
  | Running => cases eb <;> rfl
  | Asleep => cases eb <;> rfl

end Proof

open BlazeCore.HandoffProof Proof in
theorem no_hand_back_is_missed (schedule : List Step) (flagged : Bool) :
    (hands .AcqRel schedule flagged).valueWeak = .ok true := by
  obtain ⟨ea, eb, w', he, hp⟩ := wake_ends Proof.shapes schedule flagged
  have he' : (two schedule (sender .AcqRel ⟨1⟩ ⟨0⟩) (driver .AcqRel ⟨1⟩ ⟨0⟩)).execWeak
      (start flagged) = .ok ((ea, eb), w') := he
  apply Prog.valueWeak_of_execWeak (w := w')
  simp only [hands, Prog.execWeak_bind, new_flag, new_slot, he', Prog.pure_eq,
    Prog.execWeak_done_ok, oracle_true hp]


end BlazeCore.HandoffModel
