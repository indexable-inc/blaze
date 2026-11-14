import Blaze.Result
import Blaze.Scalar

/-!
Threads that share atomic words (docs/v2/plan/design/proofs-for-sans-io.md, section 8).

A function that touches an atomic is not a `Result`: what it does next depends on what the
other threads did to the word meanwhile. Its model is a `Prog`: a tree that ends with a
`Result`, or does one operation on what is shared (`Op`) and goes on with the answer (`Got`).
Everything between two operations is a thread's own and is not a step.

The world (`World`) is what is shared: the cells, and who sleeps in a futex wait of which.
`World.step` is one operation by one thread, with the world's free choice at that step (`pick`:
1 fails a weak compare-and-swap for nothing and wakes a sleeper for nothing, 2 tells a sleeper
with a time limit that it ran out, and a wake takes the sleeper of that number, the first
where there is none). `Prog.exec` runs a program alone.

`two` runs two programs as two threads, one operation at a time in the order a schedule gives,
and is itself a program: so a theorem's function is an ordinary one that takes the schedule as
an argument, and its statement, for every schedule, is every interleaving.

The memory is sequentially consistent: an operation is one step and sees the last value
written, whatever its `Ordering` says. That is the meaning of a run only where the orderings
cannot be told apart from `SeqCst`, and the world checks it at every step of a thread
(`World.fits`): either every operation of the threads is `SeqCst`, or the threads touch one
cell only, where every hardware and language memory model is sequentially consistent (each
gives one total order of the writes to a cell that every thread's reads and writes of it
respect). Any other run is refused (`Error.unmodelled`), never read as if it were `SeqCst`: a
store buffer or a stale read across two cells is not in this world yet.

A futex wait compares the cell and goes to sleep in one step, and a wake finds every thread
that sleeps: what the kernel promises of `FUTEX_WAIT` and `FUTEX_WAKE`, and of
`os_sync_wait_on_address`. A sleeper returns when woken, and may return for nothing or, with a
time limit, because the kernel says its time ran out, at any step the world picks.

The world has a clock, in nanoseconds, and a script of readings still to come (`Op.clock`
installs it before a run). Time moves only where someone looks: `Op.now` moves the clock to the
later of itself and the script's next reading. `Op.wait_until` is the wait with a deadline on
that clock: a sleeper whose deadline the clock has reached returns timed out at its next look,
whatever the pick, and is not asleep when a schedule ends; the kernel may say the time ran out
only where there is a deadline, and saying so moves the clock to it.
-/

namespace Blaze

namespace core.sync.atomic

/-- `core::sync::atomic::Ordering` (sync/atomic.rs, `pub enum Ordering`). -/
inductive Ordering where
  | Relaxed
  | Release
  | Acquire
  | AcqRel
  | SeqCst
  deriving Repr, DecidableEq

end core.sync.atomic

namespace Threads

/-- An operation's ordering, as Rust names it. -/
abbrev Order := core.sync.atomic.Ordering

/-- What an operation answers: a value, and a flag whose meaning is the operation's. -/
structure Got where
  value : Int
  flag : Bool
  deriving Repr, DecidableEq

/-- One operation on what threads share: one step of a run. -/
inductive Op where
  /-- A new cell holding `value`. Answers its number. -/
  | new (value : Int)
  /-- Answers the cell's value. -/
  | load (cell : Nat) (order : Order)
  /-- Writes `value`. -/
  | store (cell : Nat) (value : Int) (order : Order)
  /-- Reads the cell and, where `change` of what it read is `some new`, writes `new` in the
  same step. Answers what it read, flagged with whether it wrote. `weak`: it may write nothing
  for no reason (`compare_exchange_weak`), by the world's pick. A swap, a compare-and-swap and
  a `fetch_add` are each one of these. -/
  | update (cell : Nat) (change : Int → Option Int) (weak : Bool) (success failure : Order)
  /-- A futex wait: where the cell holds `expected` the thread goes to sleep, in the same step
  as the comparison. Answers, flagged, why the wait returned (`Woke.code`); unflagged, that the
  thread sleeps still. `timed`: the kernel may say its time ran out. -/
  | wait (cell : Nat) (expected : Int) (timed : Bool)
  /-- A futex wake of one sleeper of the cell, or of all. Answers how many it woke, flagged
  where that is any. -/
  | wake (cell : Nat) (all : Bool)
  /-- Not an operation of a program: `two` asks it of a thread whose next operation is a wait
  when the schedule ends. Flagged where that wait would not return unless the kernel wakes it
  for nothing: the thread sleeps and no wake came, or it would go to sleep. -/
  | asleep (cell : Nat) (expected : Int)
  /-- Installs the clock's script: the readings still to come, in nanoseconds. Answers 0,
  flagged; the clock does not move. -/
  | clock (readings : List Int)
  /-- Reads the clock, in nanoseconds, flagged. `move`: the clock first becomes the later of
  itself and the script's next reading, which is taken off the script (with the script empty it
  stays). Not `move`: nothing changes. -/
  | now (move : Bool)
  /-- A futex wait with a deadline on the world's clock (`none`: none). As `wait`, but where
  the clock has reached the deadline it returns timed out whatever the pick, and where the
  kernel says the time ran out (pick 2, with a deadline) the clock moves to the deadline. -/
  | wait_until (cell : Nat) (expected : Int) (deadline : Option Int)
  /-- Not an operation of a program: `ended` asks it of a thread whose next operation is a
  `wait_until`. Flagged where that wait would not return if asked now with pick 0. -/
  | asleep_until (cell : Nat) (expected : Int) (deadline : Option Int)

/-- The model of a function that touches what threads share. -/
inductive Prog (α : Type) : Type where
  /-- It ended: with a value, a panic, or out of fuel. -/
  | done (result : Result α) : Prog α
  /-- It does `op` and goes on with the answer. `who` and `pick` are 0 as a function's model
  gives it; `two` writes the thread and the world's pick of the step there. -/
  | op (who pick : Nat) (op : Op) (next : Got → Prog α) : Prog α

namespace Prog

def bind {α β : Type} : Prog α → (α → Prog β) → Prog β
  | done (.ok v), f => f v
  | done (.fail e), _ => done (.fail e)
  | done .div, _ => done .div
  | op who pick o next, f => op who pick o (fun got => bind (next got) f)

end Prog

instance : Monad Prog where
  pure v := .done (.ok v)
  bind := Prog.bind

/-- A model that touches nothing shared, inside one that does. -/
instance : MonadLift Result Prog := ⟨Prog.done⟩

instance {α : Type} : Coe (Result α) (Prog α) := ⟨Prog.done⟩

/-- One operation, by a function's model. -/
def Prog.ask (o : Op) : Prog Got := .op 0 0 o (fun got => .done (.ok got))

@[simp] theorem Prog.bind_done_ok {α β : Type} (v : α) (f : α → Prog β) :
    ((Prog.done (.ok v) : Prog α) >>= f) = f v := rfl

@[simp] theorem Prog.bind_done_fail {α β : Type} (e : Error) (f : α → Prog β) :
    ((Prog.done (.fail e) : Prog α) >>= f) = Prog.done (.fail e) := rfl

@[simp] theorem Prog.bind_done_div {α β : Type} (f : α → Prog β) :
    ((Prog.done .div : Prog α) >>= f) = Prog.done .div := rfl

@[simp] theorem Prog.bind_op {α β : Type} (who pick : Nat) (o : Op) (next : Got → Prog α)
    (f : α → Prog β) :
    (Prog.op who pick o next >>= f) = Prog.op who pick o (fun got => next got >>= f) := rfl

@[simp] theorem Prog.pure_eq {α : Type} (v : α) : (pure v : Prog α) = Prog.done (.ok v) := rfl

/-! The world. -/

/-- What threads share: the cells, who sleeps in a wait of which, and what the threads of a
run have touched (for `World.fits`). -/
structure World where
  /-- The cells' values; a cell's number is its place. -/
  cells : List Int
  /-- (thread, cell): asleep in a wait of the cell. -/
  asleep : List (Nat × Nat)
  /-- (thread, cell): woken by a wake, its wait not yet returned. -/
  woken : List (Nat × Nat)
  /-- The cells a thread of a run has touched, each once. -/
  used : List Nat
  /-- Whether a thread of a run did an operation with an ordering that is not `SeqCst`. -/
  weak : Bool
  /-- The clock's last reading, in nanoseconds. -/
  clock : Int := 0
  /-- The clock's script: the readings still to come (`Op.clock`, `Op.now`). -/
  readings : List Int := []
  deriving Repr, DecidableEq

/-- Whether a clock reading `clock` has reached `deadline` (`none`: no deadline, never). -/
def passed (clock : Int) : Option Int → Bool
  | none => false
  | some d => decide (d ≤ clock)

namespace World

/-- Nothing shared yet. -/
def new : World := ⟨[], [], [], [], false, 0, []⟩

/-- Why a wait returned, as `Got.value`: 0 woken, 1 the cell did not hold what was expected,
2 for nothing, 3 its time ran out. -/
def woken_code : Int := 0
def changed_code : Int := 1
def spurious_code : Int := 2
def timed_out_code : Int := 3

/-- The memory is the run's meaning: every operation of its threads is `SeqCst`, or they touch
one cell. -/
def fits (w : World) : Bool := !w.weak || decide (w.used.length ≤ 1)

/-- The world after thread `who` touched `cell` with `orders`. The thread that runs alone
(0: before a run's threads start and after they end) is not one of a run's. -/
def touched (w : World) (who cell : Nat) (orders : List Order) : World :=
  if who = 0 then w
  else
    { w with
      used := if w.used.contains cell then w.used else cell :: w.used
      weak := w.weak || orders.any (fun order => order != core.sync.atomic.Ordering.SeqCst) }

/-- One operation by thread `who`, with the world's pick. `none`: a cell that is not there, or
a run this memory is not the meaning of (`fits`). -/
def step (who pick : Nat) (o : Op) (w : World) : Option (Got × World) :=
  match o with
  | .new value => some (⟨w.cells.length, true⟩, { w with cells := w.cells ++ [value] })
  | .load cell order =>
    match w.cells[cell]? with
    | none => none
    | some value =>
      let w := w.touched who cell [order]
      if w.fits then some (⟨value, true⟩, w) else none
  | .store cell value order =>
    if cell < w.cells.length then
      let w := { w.touched who cell [order] with cells := w.cells.set cell value }
      if w.fits then some (⟨0, true⟩, w) else none
    else none
  | .update cell change weak success failure =>
    match w.cells[cell]? with
    | none => none
    | some old =>
      let w := w.touched who cell [success, failure]
      if !w.fits then none
      else if weak && pick == 1 then some (⟨old, false⟩, w)
      else
        match change old with
        | some new => some (⟨old, true⟩, { w with cells := w.cells.set cell new })
        | none => some (⟨old, false⟩, w)
  | .wait cell expected timed =>
    match w.cells[cell]? with
    | none => none
    | some value =>
      let w := w.touched who cell []
      if !w.fits then none
      else if w.woken.contains (who, cell) then
        some (⟨woken_code, true⟩, { w with woken := w.woken.erase (who, cell) })
      else if w.asleep.contains (who, cell) then
        if pick == 1 then
          some (⟨spurious_code, true⟩, { w with asleep := w.asleep.erase (who, cell) })
        else if pick == 2 && timed then
          some (⟨timed_out_code, true⟩, { w with asleep := w.asleep.erase (who, cell) })
        else some (⟨0, false⟩, w)
      else if value != expected then some (⟨changed_code, true⟩, w)
      else some (⟨0, false⟩, { w with asleep := (who, cell) :: w.asleep })
  | .wake cell all =>
    if cell < w.cells.length then
      let w := w.touched who cell []
      let sleepers := w.asleep.filter (fun it => it.2 == cell)
      if !w.fits then none
      else
        match sleepers[pick]? <|> sleepers.head? with
        | none => some (⟨0, false⟩, w)
        | some one =>
          if all then
            some (⟨sleepers.length, true⟩,
              { w with
                asleep := w.asleep.filter (fun it => it.2 != cell)
                woken := sleepers ++ w.woken })
          else some (⟨1, true⟩, { w with asleep := w.asleep.erase one, woken := one :: w.woken })
    else none
  | .asleep cell expected =>
    match w.cells[cell]? with
    | none => none
    | some value =>
      some (⟨0, !w.woken.contains (who, cell)
        && (w.asleep.contains (who, cell) || value == expected)⟩, w)
  | .clock readings => some (⟨0, true⟩, { w with readings := readings })
  | .now move =>
    if move then
      match w.readings with
      | [] => some (⟨w.clock, true⟩, w)
      | r :: rest =>
        let c := if w.clock < r then r else w.clock
        some (⟨c, true⟩, { w with clock := c, readings := rest })
    else some (⟨w.clock, true⟩, w)
  | .wait_until cell expected deadline =>
    match w.cells[cell]? with
    | none => none
    | some value =>
      let w := w.touched who cell []
      if !w.fits then none
      else if w.woken.contains (who, cell) then
        some (⟨woken_code, true⟩, { w with woken := w.woken.erase (who, cell) })
      else if w.asleep.contains (who, cell) then
        if passed w.clock deadline then
          some (⟨timed_out_code, true⟩, { w with asleep := w.asleep.erase (who, cell) })
        else if pick == 1 then
          some (⟨spurious_code, true⟩, { w with asleep := w.asleep.erase (who, cell) })
        else
          match deadline with
          | some d =>
            if pick == 2 then
              some (⟨timed_out_code, true⟩,
                { w with asleep := w.asleep.erase (who, cell), clock := d })
            else some (⟨0, false⟩, w)
          | none => some (⟨0, false⟩, w)
      else if value != expected then some (⟨changed_code, true⟩, w)
      else if passed w.clock deadline then some (⟨timed_out_code, true⟩, w)
      else some (⟨0, false⟩, { w with asleep := (who, cell) :: w.asleep })
  | .asleep_until cell expected deadline =>
    match w.cells[cell]? with
    | none => none
    | some value =>
      some (⟨0, !w.woken.contains (who, cell)
        && (w.asleep.contains (who, cell) || value == expected)
        && !passed w.clock deadline⟩, w)

end World

/-- Whether `got` says the thread that asked `o` sleeps: a wait that did not return. -/
def Op.sleeps (o : Op) (got : Got) : Bool :=
  match o with
  | .wait _ _ _ => !got.flag
  | .wait_until _ _ _ => !got.flag
  | _ => false

namespace Prog

/-- A program run alone, from `w`: its value and the world after. A wait of its own (thread
0, not one `two` gave a thread) that sleeps has no other thread to wake it and never returns
(`div`); a step the world refuses is `Error.unmodelled`. -/
def exec {α : Type} : Prog α → World → Result (α × World)
  | done (.ok v), w => .ok (v, w)
  | done (.fail e), _ => .fail e
  | done .div, _ => .div
  | op who pick o next, w =>
    match w.step who pick o with
    | none => .fail .unmodelled
    | some (got, w') => if who == 0 && o.sleeps got then .div else exec (next got) w'

/-- What a program run alone in a world with nothing shared returns: the left side of a
theorem's statement. -/
def value {α : Type} (p : Prog α) : Result α :=
  match p.exec World.new with
  | .ok (v, _) => .ok v
  | .fail e => .fail e
  | .div => .div

end Prog

/-! Two threads. -/

/-- One step of a run of two threads: which goes, and the world's pick at the step. The model
of `blaze_core::world::threads::Step`. -/
structure Step where
  second : Bool
  pick : U8

/-- How a thread stood when the schedule ended. The model of
`blaze_core::world::threads::Ended`. -/
inductive Ended (α : Type) where
  /-- It returned. -/
  | Done (value : α)
  /-- It had more to do and could go on. -/
  | Running
  /-- Its next operation is a wait that would not return unless the kernel wakes it for
  nothing. -/
  | Asleep

/-- How thread `who`, at `p`, stands when the schedule ends. -/
def ended {α : Type} (who : Nat) : Prog α → Prog (Ended α)
  | .done (.ok v) => .done (.ok (.Done v))
  | .done (.fail e) => .done (.fail e)
  | .done .div => .done .div
  | .op _ _ (.wait cell expected _) _ =>
    .op who 0 (.asleep cell expected)
      (fun got => .done (.ok (if got.flag then .Asleep else .Running)))
  | .op _ _ (.wait_until cell expected deadline) _ =>
    .op who 0 (.asleep_until cell expected deadline)
      (fun got => .done (.ok (if got.flag then .Asleep else .Running)))
  | .op _ _ _ _ => .done (.ok .Running)

/-- Thread `who` at `p` does its next operation with the world's `pick`, and the run goes on
by `rest` from where the thread then stands. A thread that has returned does nothing; one
that sleeps stays at its wait; an operation already given a thread (a run inside a run) is
not modelled. -/
def turn {α β : Type} (who pick : Nat) (p : Prog α) (rest : Prog α → Prog β) : Prog β :=
  match p with
  | .done _ => rest p
  | .op tagged _ o next =>
    if tagged != 0 then .done (.fail .unmodelled)
    else .op who pick o (fun got => rest (if o.sleeps got then p else next got))

/-- `first` and `second` as threads 1 and 2, one operation at a time in the order `schedule`
gives, and how each stood at its end. A thread that panicked, or ran out of fuel, is the
run's panic, or its running out. The model of `blaze_core::world::threads::two`. -/
def two {α β : Type} : List Step → Prog α → Prog β → Prog (Ended α × Ended β)
  | [], first, second => do
    let a ← ended 1 first
    let b ← ended 2 second
    pure (a, b)
  | step :: rest, first, second =>
    if step.second then turn 2 step.pick.val.toNat second (fun second => two rest first second)
    else turn 1 step.pick.val.toNat first (fun first => two rest first second)

end Threads

end Blaze
