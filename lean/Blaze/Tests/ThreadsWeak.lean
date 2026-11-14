import Blaze.ThreadsWeak

/-!
`Blaze.Threads.Weak` by evaluation: litmus tests, each on listed schedules and picks.
-/

namespace Blaze.Tests.ThreadsWeak

open Blaze Blaze.Threads Blaze.core.sync.atomic

/-- Thread 1 goes, with the world's pick. -/
def a (pick : Nat := 0) : Step := ⟨false, Scalar.lit .U8 pick⟩
/-- Thread 2 goes, with the world's pick. -/
def b (pick : Nat := 0) : Step := ⟨true, Scalar.lit .U8 pick⟩

/-- Both threads' values where both returned; `(-1, -1)` otherwise. -/
def both {α β : Type} (f : α → Int) (g : β → Int) : Ended α → Ended β → Int × Int
  | .Done x, .Done y => (f x, g y)
  | _, _ => (-1, -1)

/-! 1. Store buffering. -/

/-- T1: x := 1; r1 := y.  T2: y := 1; r2 := x. -/
def sb (store load : Order) (schedule : List Step) : Prog (Int × Int) := do
  let x ← Atomic.new (0 : U32)
  let y ← Atomic.new (0 : U32)
  let (r1, r2) ← two schedule
    (do Atomic.store x 1 store; let r ← Atomic.load y load; pure r.val)
    (do Atomic.store y 1 store; let r ← Atomic.load x load; pure r.val)
  pure (both id id r1 r2)

-- Release stores and acquire loads: each load picks the stale message, both read 0.
example : (sb .Release .Acquire [a, b, a 1, b 1]).valueWeak = .ok (0, 0) := by rfl
-- Every ordering SeqCst, in the sequentially consistent world, on the same schedule: not (0, 0).
example : (sb .SeqCst .SeqCst [a, b, a 1, b 1]).value = .ok (1, 1) := by rfl

/-! 2. Message passing. -/

/-- T1: data := 42 (Relaxed); flag := 1 (`flag`).  T2: f := flag (Acquire); d := data
(Relaxed). -/
def mp (flag : Order) (schedule : List Step) : Prog (Int × Int) := do
  let data ← Atomic.new (0 : U32)
  let fl ← Atomic.new (0 : U32)
  let (_, r) ← two schedule
    (do Atomic.store data 42 .Relaxed; Atomic.store fl 1 flag)
    (do let f ← Atomic.load fl .Acquire; let d ← Atomic.load data .Relaxed; pure (f.val, d.val))
  pure (match r with | .Done fd => fd | _ => (-1, -1))

-- With a Release flag, f = 1 brings the data message into T2's view: d = 42 whatever the data
-- load picks.
example : (mp .Release [a, a, b, b 0]).valueWeak = .ok (1, 42) := by rfl
example : (mp .Release [a, a, b, b 1]).valueWeak = .ok (1, 42) := by rfl
example : (mp .Release [a, a, b, b 2]).valueWeak = .ok (1, 42) := by rfl
example : (mp .Release [a, a, b, b 3]).valueWeak = .ok (1, 42) := by rfl
-- T2 reads the flag between T1's stores, or picks the stale flag: f = 0, and d may be 0.
example : (mp .Release [a, b, a, b 1]).valueWeak = .ok (0, 0) := by rfl
example : (mp .Release [a, a, b 1, b 1]).valueWeak = .ok (0, 0) := by rfl
-- With a Relaxed flag nothing is brought: f = 1 and d = 0.
example : (mp .Relaxed [a, a, b, b 1]).valueWeak = .ok (1, 0) := by rfl

/-! 3. Coherence on one cell. -/

/-- T1: x := 1 (Relaxed).  T2: a := x; b := x (Relaxed). -/
def coh (schedule : List Step) : Prog (Int × Int) := do
  let x ← Atomic.new (0 : U32)
  let (_, r) ← two schedule (Atomic.store x 1 .Relaxed)
    (do let p ← Atomic.load x .Relaxed; let q ← Atomic.load x .Relaxed; pure (p.val, q.val))
  pure (match r with | .Done pq => pq | _ => (-1, -1))

-- Once a = 1, the second load cannot go back to 0, whatever it picks.
example : (coh [a, b 0, b 0]).valueWeak = .ok (1, 1) := by rfl
example : (coh [a, b 0, b 1]).valueWeak = .ok (1, 1) := by rfl
example : (coh [a, b 0, b 5]).valueWeak = .ok (1, 1) := by rfl
-- Reading stale first and new second, or stale twice, is allowed.
example : (coh [a, b 1, b 0]).valueWeak = .ok (0, 1) := by rfl
example : (coh [a, b 1, b 1]).valueWeak = .ok (0, 0) := by rfl

/-! 4. Two `fetch_add`s on one cell. -/

/-- Each thread adds 1 (Relaxed); then thread 0 reads the cell. What each read, and the total. -/
def adds (schedule : List Step) : Prog (Int × Int × Int) := do
  let n ← Atomic.new (0 : U32)
  let (x, y) ← two schedule (Atomic.fetch_add n 1 .Relaxed) (Atomic.fetch_add n 1 .Relaxed)
  let total ← Atomic.load n .Relaxed
  let (p, q) := both (fun v : U32 => v.val) (fun v : U32 => v.val) x y
  pure (p, q, total.val)

-- 2 on both orders, whatever the second update picks: the message it would read back is
-- glued, so it reads the newest.
example : (adds [a, b 0]).valueWeak = .ok (0, 1, 2) := by rfl
example : (adds [a, b 1]).valueWeak = .ok (0, 1, 2) := by rfl
example : (adds [a, b 2]).valueWeak = .ok (0, 1, 2) := by rfl
example : (adds [b, a 0]).valueWeak = .ok (1, 0, 2) := by rfl
example : (adds [b, a 1]).valueWeak = .ok (1, 0, 2) := by rfl
example : (adds [b 3, a 7]).valueWeak = .ok (1, 0, 2) := by rfl

/-! 5. A store placed back in time. -/

/-- T1: x := 1 (Release).  T2: x := 2 (Release). Then thread 0 reads x. -/
def placed (schedule : List Step) : Prog Int := do
  let x ← Atomic.new (0 : U32)
  let _ ← two schedule (Atomic.store x 1 .Release) (Atomic.store x 2 .Release)
  let v ← Atomic.load x .Acquire
  pure v.val

-- T2 stores last in the schedule but its pick puts its message before T1's: the newest by
-- timestamp is 1.
example : (placed [a, b 1]).valueWeak = .ok 1 := by rfl
example : (placed [a, b 0]).valueWeak = .ok 2 := by rfl

/-! 6. Rule R1. -/

/-- T1: r := x (Relaxed); y := 1 (`order`). -/
def r1 (order : Order) : Prog Unit := do
  let x ← Atomic.new (0 : U32)
  let y ← Atomic.new (0 : U32)
  let _ ← two [a, a] (do let _ ← Atomic.load x .Relaxed; Atomic.store y 1 order) (pure () : Prog Unit)
  pure ()

-- A relaxed store after a relaxed load of another cell could show load buffering: refused.
example : (r1 .Relaxed).valueWeak = .fail .unmodelled := by rfl
-- A release store is ordered after the load (trusted): run.
example : (r1 .Release).valueWeak = .ok () := by rfl

/-! 7. Spawn and join. -/

/-- Thread 0 makes x and stores 5; T1 reads x (Relaxed); T2 writes 9 to y. Then thread 0
reads y. -/
def spawn (schedule : List Step) : Prog (Int × Int) := do
  let x ← Atomic.new (0 : U32)
  let y ← Atomic.new (0 : U32)
  Atomic.store x 5 .Relaxed
  let (r, _) ← two schedule (do let v ← Atomic.load x .Relaxed; pure v.val)
    (Atomic.store y 9 .Relaxed)
  let after ← Atomic.load y .Relaxed
  pure (match r with | .Done v => v | _ => -1, after.val)

-- The spawn shows T1 the 5 on every pick; the join shows thread 0 the 9.
example : (spawn [a 0, b]).valueWeak = .ok (5, 9) := by rfl
example : (spawn [a 1, b]).valueWeak = .ok (5, 9) := by rfl
example : (spawn [b, a 2]).valueWeak = .ok (5, 9) := by rfl

/-! 8. `valueAny`. -/

-- Two cells with weaker orderings: the sequentially consistent world refuses, the weak one runs.
example : (sb .Release .Acquire [a, b, a 1, b 1]).value = .fail .unmodelled := by rfl
example : (sb .Release .Acquire [a, b, a 1, b 1]).valueAny = .ok (0, 0) := by rfl

/-- One cell, weaker orderings: the sequentially consistent world's run. -/
def weak_one (schedule : List Step) : Prog Int := do
  let n ← Atomic.new (0 : U32)
  let _ ← two schedule (Atomic.store n 1 .Relaxed) (Atomic.store n 2 .Release)
  let last ← Atomic.load n .Acquire
  pure last.val

example : (weak_one [a, b]).value = .ok 2 := by rfl
example : (weak_one [a, b]).valueAny = .ok 2 := by rfl
example : (weak_one [b, a]).valueAny = .ok 1 := by rfl

/-! 9. A futex. -/

/-- A sleeper and its waker (`Tests.Threads.slept`). -/
def slept (schedule : List Step) : Prog (Bool × Bool) := do
  let word ← Atomic.new (0 : U32)
  let (x, y) ← two schedule (wait word 0 false)
    (do Atomic.store word 1 .SeqCst; wake word)
  pure (match x with | .Done _ => true | _ => false, match x, y with | .Asleep, .Done _ => true | _, _ => false)

example : (slept [a, b, b, a]).valueWeak = .ok (true, false) := by rfl
example : (slept [b, b, a]).valueWeak = .ok (true, false) := by rfl
example : (slept [a, b]).valueWeak = .ok (false, false) := by rfl

/-- The lost wakeup (`Tests.Threads.lost_wake`). -/
def lost_wake (schedule : List Step) : Prog Bool := do
  let word ← Atomic.new (0 : U32)
  let (x, y) ← two schedule (wait word 0 false)
    (do let _ ← wake word; Atomic.store word 1 .SeqCst)
  pure (match x, y with | .Asleep, .Done _ => true | _, _ => false)

example : (lost_wake [b, a, b]).valueWeak = .ok true := by rfl

/-- The sleeper/waker pair, answering why the sleeper's wait returned. -/
def slept_why (schedule : List Step) : Prog (Option Woke) := do
  let word ← Atomic.new (0 : U32)
  let (x, _) ← two schedule (wait word 0 false)
    (do Atomic.store word 1 .SeqCst; wake word)
  pure (match x with | .Done why => some why | _ => none)

-- The store is done before the wait looks: with pick 0 the look sees it, as in the sequentially
-- consistent world.
example : (slept_why [b, a, b]).value = .ok (some .Changed) := by rfl
example : (slept_why [b, a, b]).valueWeak = .ok (some .Changed) := by rfl
-- The look picks the stale 0 and sleeps although the store is done; the wake then wakes it.
example : (slept_why [b, a 1, b, a]).valueWeak = .ok (some .Woken) := by rfl

/-! A weak compare-and-swap that fails for nothing on a stale value. -/

/-- x goes 0, 1, 0 by T2. T1: a weak compare-and-swap of 0 for 5, then a relaxed load. -/
def weak_cas (schedule : List Step) : Prog (Int × Int × Int) := do
  let x ← Atomic.new (0 : U32)
  let (r, _) ← two schedule
    (do
      let got ← Atomic.compare_exchange_weak x 0 5 .Relaxed .Relaxed
      let v ← Atomic.load x .Relaxed
      pure (match got with | .Ok o => (1, o.val, v.val) | .Err o => (0, o.val, v.val)))
    (do Atomic.store x 1 .Relaxed; Atomic.store x 0 .Relaxed)
  pure (match r with | .Done t => t | _ => (-1, -1, -1))

-- Pick 5 = 2*2+1: fails for nothing having read 2 back (the first 0); the load then reads 1,
-- as an LL/SC target can give.
example : (weak_cas [b, b, a 5, a 1]).valueWeak = .ok (0, 0, 1) := by rfl
-- Pick 4 = 2*2: reads the first 0 and writes 5 right after it.
example : (weak_cas [b, b, a 4, a 0]).valueWeak = .ok (1, 0, 0) := by rfl
example : (weak_cas [b, b, a 4, a 2]).valueWeak = .ok (1, 0, 5) := by rfl

/-! Release sequences. -/

/-- T1: data := 42 (Relaxed); flag := 1 (Release).  T2: f := flag.fetch_add 1 (`order`); d :=
data (Relaxed). -/
def rs (order : Order) (schedule : List Step) : Prog (Int × Int) := do
  let data ← Atomic.new (0 : U32)
  let fl ← Atomic.new (0 : U32)
  let (_, r) ← two schedule
    (do Atomic.store data 42 .Relaxed; Atomic.store fl 1 .Release)
    (do let f ← Atomic.fetch_add fl 1 order; let d ← Atomic.load data .Relaxed; pure (f.val, d.val))
  pure (match r with | .Done fd => fd | _ => (-1, -1))

-- A relaxed update does not acquire: f = 1 and d = 0.
example : (rs .Relaxed [a, a, b, b 1]).valueWeak = .ok (1, 0) := by rfl
-- An acquiring one: d = 42 on every pick once f = 1.
example : (rs .Acquire [a, a, b, b 0]).valueWeak = .ok (1, 42) := by rfl
example : (rs .Acquire [a, a, b, b 1]).valueWeak = .ok (1, 42) := by rfl
example : (rs .Acquire [a, a, b, b 2]).valueWeak = .ok (1, 42) := by rfl

/-- The release sequence itself, with two threads: the update's thread reads its own update's
message with an acquire, which synchronises with the release it continued. T1: data := 42
(Relaxed); flag := 1 (Release).  T2: f := flag.fetch_add 1 (Relaxed); g := flag (Acquire);
d := data (Relaxed). -/
def rs_own (schedule : List Step) : Prog (Int × Int × Int) := do
  let data ← Atomic.new (0 : U32)
  let fl ← Atomic.new (0 : U32)
  let (_, r) ← two schedule
    (do Atomic.store data 42 .Relaxed; Atomic.store fl 1 .Release)
    (do
      let f ← Atomic.fetch_add fl 1 .Relaxed
      let g ← Atomic.load fl .Acquire
      let d ← Atomic.load data .Relaxed
      pure (f.val, g.val, d.val))
  pure (match r with | .Done t => t | _ => (-1, -1, -1))

-- f = 1 and g = 2: d = 42 whatever the data load picks (without the release sequence, 0).
example : (rs_own [a, a, b, b, b 0]).valueWeak = .ok (1, 2, 42) := by rfl
example : (rs_own [a, a, b, b, b 1]).valueWeak = .ok (1, 2, 42) := by rfl

end Blaze.Tests.ThreadsWeak
