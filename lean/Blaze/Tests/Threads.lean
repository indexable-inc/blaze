import Blaze.Atomic

/-!
`Blaze.Threads` by evaluation: small runs whose every schedule is listed or decided.
-/

namespace Blaze.Tests.Threads

open Blaze Blaze.Threads Blaze.core.sync.atomic

/-- Thread 1 goes. -/
def a (pick : Nat := 0) : Step := ⟨false, Scalar.lit .U8 pick⟩
/-- Thread 2 goes. -/
def b (pick : Nat := 0) : Step := ⟨true, Scalar.lit .U8 pick⟩

/-- Two threads each add one to a counter, then it is read: 2 on every schedule that lets both
finish, since a `fetch_add` is one step. -/
def counted (schedule : List Step) : Prog (Bool × Int) := do
  let n ← Atomic.new (0 : U32)
  let (x, y) ← two schedule (Atomic.fetch_add n 1 .SeqCst) (Atomic.fetch_add n 1 .SeqCst)
  let total ← Atomic.load n .SeqCst
  pure (match x, y with | .Done _, .Done _ => true | _, _ => false, total.val)

example : (counted [a, b]).value = .ok (true, 2) := by rfl
example : (counted [b, a]).value = .ok (true, 2) := by rfl
example : (counted [a]).value = .ok (false, 1) := by rfl

/-- The same with a load and then a store in place of the `fetch_add`: an update is lost on
the schedule that puts both loads first. -/
def lost (schedule : List Step) : Prog Int := do
  let n ← Atomic.new (0 : U32)
  let bump : Prog Unit := do
    let old ← Atomic.load n .SeqCst
    Atomic.store n (Scalar.wrapping_add old 1) .SeqCst
  let _ ← two schedule bump bump
  let total ← Atomic.load n .SeqCst
  pure total.val

example : (lost [a, a, b, b]).value = .ok 2 := by rfl
example : (lost [a, b, a, b]).value = .ok 1 := by rfl

/-- A weaker ordering on one cell is this memory's; on two cells it is refused. -/
def weak_one (schedule : List Step) : Prog Int := do
  let n ← Atomic.new (0 : U32)
  let _ ← two schedule (Atomic.store n 1 .Relaxed) (Atomic.store n 2 .Release)
  let last ← Atomic.load n .Acquire
  pure last.val

example : (weak_one [a, b]).value = .ok 2 := by rfl
example : (weak_one [b, a]).value = .ok 1 := by rfl

def weak_two (schedule : List Step) : Prog Unit := do
  let n ← Atomic.new (0 : U32)
  let m ← Atomic.new (0 : U32)
  let _ ← two schedule (Atomic.store n 1 .Relaxed) (Atomic.store m 2 .SeqCst)
  pure ()

example : (weak_two [a, b]).value = .fail .unmodelled := by rfl
example : (weak_two [b]).value = .ok () := by rfl

/-- A sleeper and its waker: woken on every order of the two, and asleep with no wake coming
only while the waker has not gone. -/
def slept (schedule : List Step) : Prog (Bool × Bool) := do
  let word ← Atomic.new (0 : U32)
  let (x, y) ← two schedule (wait word 0 false)
    (do Atomic.store word 1 .SeqCst; wake word)
  pure (match x with | .Done _ => true | _ => false, match x, y with | .Asleep, .Done _ => true | _, _ => false)

-- The sleeper sleeps, the waker stores and wakes, the sleeper returns.
example : (slept [a, b, b, a]).value = .ok (true, false) := by rfl
-- The waker goes first: the word changed, the sleeper never sleeps.
example : (slept [b, b, a]).value = .ok (true, false) := by rfl
-- The waker has stored but not yet woken: the sleeper is asleep, and the waker is not done.
example : (slept [a, b]).value = .ok (false, false) := by rfl

/-- The lost wakeup: a waker that wakes before it stores. The sleeper that looks between the
two sleeps on a word nobody will wake again. -/
def lost_wake (schedule : List Step) : Prog Bool := do
  let word ← Atomic.new (0 : U32)
  let (x, y) ← two schedule (wait word 0 false)
    (do let _ ← wake word; Atomic.store word 1 .SeqCst)
  pure (match x, y with | .Asleep, .Done _ => true | _, _ => false)

example : (lost_wake [b, a, b]).value = .ok true := by rfl

/-- Alone, a wait that sleeps never returns, and one whose word changed returns at once. -/
example : (do let word ← Atomic.new (0 : U32); wait word 0 false : Prog Woke).value = .div := by rfl
example : (do let word ← Atomic.new (1 : U32); wait word 0 false : Prog Woke).value = .ok .Changed := by rfl

end Blaze.Tests.Threads
