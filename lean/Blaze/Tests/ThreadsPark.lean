import Blaze.Atomic

/-!
A park and an unpark on one word, as `crates/blaze-core/src/wait/parking.rs` orders their
atomics, without its clock: the shape of the proof that no unpark is lost, on a model written
by hand. The proof of the real functions is over the models `blaze prove` emits from them.

The word is a token: `unpark` leaves `NOTIFIED`, which a park takes; a park waits while the
word is `PARKED`.
-/

namespace Blaze.Tests.ThreadsPark

open Blaze Blaze.Threads Blaze.core.sync.atomic

def EMPTY : U32 := 0
def NOTIFIED : U32 := 1
def PARKED : U32 := 4294967295

/-- The park's loop: wait while the word is `PARKED`, take a token that came, and at a timeout
leave, with the token if one came since. `true`: it took a token. -/
def park.loop (fuel : Nat) (word : Atomic .U32) (timed : Bool) : Prog Bool :=
  match fuel with
  | 0 => Prog.done .div
  | fuel + 1 => do
    let woke ← wait word PARKED timed
    let took ← Atomic.compare_exchange word NOTIFIED EMPTY .Acquire .Relaxed
    match took with
    | .Ok _ => pure true
    | .Err _ =>
      match woke with
      | .TimedOut => do
        let old ← Atomic.swap word EMPTY .Acquire
        pure (decide (old.val = NOTIFIED.val))
      | _ => park.loop fuel word timed

/-- Parks until an unpark's token (`true`) or, where `timed`, the kernel's word that its time
ran out (`false`). A token left before the park is taken at once. -/
def park (fuel : Nat) (word : Atomic .U32) (timed : Bool) : Prog Bool := do
  let first ← Atomic.compare_exchange word NOTIFIED EMPTY .Acquire .Relaxed
  match first with
  | .Ok _ => pure true
  | .Err _ => do
    let parked ← Atomic.compare_exchange word EMPTY PARKED .AcqRel .Acquire
    match parked with
    | .Err _ => do
      -- The only other state is a token that came since the look above: taken.
      Atomic.store word EMPTY .Relaxed
      pure true
    | .Ok _ => park.loop fuel word timed

/-- Leaves a token and wakes the parker if it is parked. -/
def unpark (word : Atomic .U32) : Prog Unit := do
  let old ← Atomic.swap word NOTIFIED .Release
  if old.val = PARKED.val then do
    let _ ← wake word
    pure ()
  else pure ()

/-- The planted bug: the park looks for a token and then stores `PARKED` over whatever came
since, in place of one compare-and-swap. -/
def park_planted (fuel : Nat) (word : Atomic .U32) (timed : Bool) : Prog Bool := do
  let first ← Atomic.compare_exchange word NOTIFIED EMPTY .Acquire .Relaxed
  match first with
  | .Ok _ => pure true
  | .Err _ => do
    let seen ← Atomic.load word .Acquire
    if seen.val = NOTIFIED.val then do
      Atomic.store word EMPTY .Relaxed
      pure true
    else do
      Atomic.store word PARKED .Release
      park.loop fuel word timed

/-- One park and one unpark of an empty word, by `schedule`: how each stood at its end, and the
word after. -/
def run (park : Nat → Atomic .U32 → Bool → Prog Bool) (fuel : Nat) (timed : Bool)
    (schedule : List Step) : Prog (Ended Bool × Ended Unit × U32) := do
  let word ← Atomic.new EMPTY
  let (parked, unparked) ← two schedule (park fuel word timed) (unpark word)
  let last ← Atomic.load word .SeqCst
  pure (parked, unparked, last)

/-- Thread 1, the parker, goes. -/
def p (pick : Nat := 0) : Step := ⟨false, Scalar.lit .U8 pick⟩
/-- Thread 2, the unparker, goes. -/
def u (pick : Nat := 0) : Step := ⟨true, Scalar.lit .U8 pick⟩

/-- Whether a run ended with the unpark returned and the park asleep with no wake coming. -/
def lost : Ended Bool × Ended Unit × U32 → Bool
  | (.Asleep, .Done _, _) => true
  | _ => false

-- The control: the planted park loses the unpark that comes between its look and its store.
-- The parker fails its first compare, loads `EMPTY`; the unparker swaps (it saw `EMPTY`, so
-- it wakes no one) and returns; the parker stores `PARKED` over the token and waits.
example : lost <$> (run park_planted 8 false [p, p, u, p, p]).value = .ok true := by rfl
-- The park as it is, with the unpark between its two compares: its compare-and-swap of `EMPTY`
-- fails on the token, which it takes, and it returns.
example : (run park 8 false [p, u, p, p]).value = .ok (.Done true, .Done (), EMPTY) := by rfl

end Blaze.Tests.ThreadsPark
