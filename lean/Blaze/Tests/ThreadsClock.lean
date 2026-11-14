import Blaze.ThreadsWeak

/-!
The clock of `Blaze.Threads` by evaluation: `clock`, `now`, `time` and `wait_until`, each run
in the sequentially consistent world and, where it has two threads, in the weak world too.
-/

namespace Blaze.Tests.ThreadsClock

open Blaze Blaze.Threads Blaze.core.sync.atomic

/-- Thread 1 goes. -/
def a (pick : Nat := 0) : Step := ⟨false, Scalar.lit .U8 pick⟩
/-- Thread 2 goes. -/
def b (pick : Nat := 0) : Step := ⟨true, Scalar.lit .U8 pick⟩

/-- `n` nanoseconds. -/
def ns (n : Nat) : core.time.Duration := core.time.Duration.from_nanos (Scalar.lit .U64 n)

/-- A script of readings, in nanoseconds. -/
def script (l : List Nat) (h : (l.map ns).length ≤ 18446744073709551615 := by decide) :
    Slice core.time.Duration :=
  ⟨l.map ns, h⟩

/-! `now` and `time`. -/

/-- Four `now`s on the script [5, 3, 9], then a `time`. -/
def reads : Prog (List Int) := do
  clock (script [5, 3, 9])
  let r1 ← now
  let r2 ← now
  let r3 ← now
  let r4 ← now
  let t ← time
  pure [r1.toNanos, r2.toNanos, r3.toNanos, r4.toNanos, t.toNanos]

-- The clock never goes back: 3 after 5 reads 5; past the script's end it stays at 9; `time`
-- reads the clock.
example : reads.value = .ok [5, 5, 9, 9, 9] := by rfl

/-- `time` before and between `now`s on the script [5, 9]. -/
def timeTakesNothing : Prog (List Int) := do
  clock (script [5, 9])
  let t1 ← time
  let r1 ← now
  let t2 ← time
  let r2 ← now
  pure [t1.toNanos, r1.toNanos, t2.toNanos, r2.toNanos]

-- `time` takes no reading off the script: each `now` still gets the next one.
example : timeTakesNothing.value = .ok [0, 5, 5, 9] := by rfl

/-! A sleeper with a deadline of 10 ns on a word nobody wakes; thread 2 reads the clock once. -/

def sleeper (readings : Slice core.time.Duration) (schedule : List Step) : Prog (Ended Woke) := do
  clock readings
  let word ← Atomic.new (0 : U32)
  let (x, _) ← two schedule (wait_until word 0 (some (ns 10))) now
  pure x

-- The clock read to 7: the sleeper is still asleep when the schedule ends.
example : (sleeper (script [7]) [a, b]).value = .ok .Asleep := by rfl
-- The clock read to 12: the sleeper's next look returns timed out though the pick is 0.
example : (sleeper (script [12]) [a, b, a]).value = .ok (.Done .TimedOut) := by rfl
-- The clock read to 12 and the schedule ends before that look: running, not asleep.
example : (sleeper (script [12]) [a, b]).value = .ok .Running := by rfl
-- Before the clock moves, pick 0 leaves it asleep.
example : (sleeper (script [12]) [a, a]).value = .ok .Asleep := by rfl

-- The same runs in the weak world.
example : (sleeper (script [7]) [a, b]).valueWeak = .ok .Asleep := by rfl
example : (sleeper (script [12]) [a, b, a]).valueWeak = .ok (.Done .TimedOut) := by rfl
example : (sleeper (script [12]) [a, b]).valueWeak = .ok .Running := by rfl
example : (sleeper (script [12]) [a, a]).valueWeak = .ok .Asleep := by rfl

/-! The kernel says the time ran out. -/

/-- The sleeper with a deadline of 10 ns, the clock read to 4 by thread 2, then the clock. -/
def told (schedule : List Step) : Prog (Ended Woke × Int) := do
  clock (script [4])
  let word ← Atomic.new (0 : U32)
  let (x, _) ← two schedule (wait_until word 0 (some (ns 10))) now
  let t ← time
  pure (x, t.toNanos)

-- Pick 2 at clock 4: timed out, and the clock is at the deadline after.
example : (told [a, b, a 2]).value = .ok (.Done .TimedOut, 10) := by rfl
-- Pick 1: for nothing, and the clock stays at 4.
example : (told [a, b, a 1]).value = .ok (.Done .Spurious, 4) := by rfl
example : (told [a, b, a 2]).valueWeak = .ok (.Done .TimedOut, 10) := by rfl
example : (told [a, b, a 1]).valueWeak = .ok (.Done .Spurious, 4) := by rfl

/-! A wait whose deadline has passed when it begins. -/

/-- Alone: the clock read to 12, then a wait with a deadline of 10 on a word holding `v`. -/
def late (v : U32) : Prog Woke := do
  clock (script [12])
  let word ← Atomic.new v
  let _ ← now
  wait_until word 0 (some (ns 10))

-- The word holds `expected`: timed out at once, never asleep.
example : (late 0).value = .ok .TimedOut := by rfl
-- It does not: changed.
example : (late 1).value = .ok .Changed := by rfl
example : (late 0).valueWeak = .ok .TimedOut := by rfl
example : (late 1).valueWeak = .ok .Changed := by rfl

/-! A wake before the deadline. -/

def woken (schedule : List Step) : Prog (Ended Woke × Int) := do
  let word ← Atomic.new (0 : U32)
  let (x, _) ← two schedule (wait_until word 0 (some (ns 10)))
    (do Atomic.store word 1 .SeqCst; wake word)
  let t ← time
  pure (x, t.toNanos)

-- Asleep, stored and woken: woken, and the clock untouched.
example : (woken [a, b, b, a]).value = .ok (.Done .Woken, 0) := by rfl
example : (woken [a, b, b, a]).valueWeak = .ok (.Done .Woken, 0) := by rfl

/-! No deadline. -/

def untimed (schedule : List Step) : Prog (Ended Woke × Ended Woke) := do
  let word ← Atomic.new (0 : U32)
  let (x, _) ← two schedule (wait_until word 0 none) (pure ())
  let other ← Atomic.new (0 : U32)
  let (y, _) ← two schedule (wait other 0 false) (pure ())
  pure (x, y)

-- Pick 2 does nothing to a sleeper with no deadline: still asleep, as an untimed `wait`.
example : (untimed [a, a 2]).value = .ok (.Asleep, .Asleep) := by rfl
-- Pick 1 returns it for nothing, as it does a `wait`.
example : (untimed [a, a 1]).value = .ok (.Done .Spurious, .Done .Spurious) := by rfl
example : (untimed [a, a 2]).valueWeak = .ok (.Asleep, .Asleep) := by rfl

end Blaze.Tests.ThreadsClock
