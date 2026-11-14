import Blaze.Threads
import Blaze.Std
import Blaze.Slice
import Blaze.Duration

/-!
`core::sync::atomic`: the atomic integers and `AtomicBool`, each a cell of the world threads
share (`Blaze.Threads`), and their operations, each one step of a run (`Threads.Op`).

`Atomic ty` is the model of `core::sync::atomic::Atomic<T>` for an integer `T`
(sync/atomic.rs:384, `pub struct Atomic<T: AtomicPrimitive>`; `AtomicU32` is `Atomic<u32>`),
and of a reference to one: the cell's number is all a thread holds of it. A cell holds an
`Int`; an operation of `Atomic ty` reads it wrapped to `ty` (it only ever holds what such an
operation wrote, so the wrap changes nothing).

An ordering an operation does not take is the panic Rust gives (sync/atomic.rs:4306, 4322,
4443): no acquire store, no release load, no release ordering for a failed compare.

Also here: the futex wait and wake of `blaze_core::world::threads`, its clock (`clock`, `now`,
`time`, `wait_until`) and its `two`.
-/

namespace Blaze.core.sync.atomic

open Blaze.Threads

/-- `Atomic<T>` for an integer `T`, and `&Atomic<T>`: the number of its cell. -/
structure Atomic (ty : ScalarTy) where
  cell : Nat
  deriving Repr, DecidableEq

/-- `AtomicBool`, and a reference to one: the number of its cell, which holds 0 or 1. -/
structure AtomicBool where
  cell : Nat
  deriving Repr, DecidableEq

/-- Whether `order` is one a load takes (sync/atomic.rs:4322: a load panics on `Release` and
`AcqRel`); the same of a failed compare (sync/atomic.rs:4443). -/
def Ordering.loads : Ordering → Bool
  | .Release | .AcqRel => false
  | _ => true

/-- Whether `order` is one a store takes (sync/atomic.rs:4306: a store panics on `Acquire`
and `AcqRel`). -/
def Ordering.stores : Ordering → Bool
  | .Acquire | .AcqRel => false
  | _ => true

namespace Atomic

variable {ty : ScalarTy}

/-- `Atomic::new`. -/
def new (v : Scalar ty) : Prog (Atomic ty) := do
  let got ← Prog.ask (.new v.val)
  pure ⟨got.value.toNat⟩

/-- `load`. -/
def load (a : Atomic ty) (order : Ordering) : Prog (Scalar ty) :=
  if order.loads then do
    let got ← Prog.ask (.load a.cell order)
    pure (Scalar.wrap ty got.value)
  else Prog.done (.fail .panic)

/-- `store`. -/
def store (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog Unit :=
  if order.stores then do
    let _ ← Prog.ask (.store a.cell v.val order)
    pure ()
  else Prog.done (.fail .panic)

/-- A read-modify-write that always writes: what the cell held before. -/
def update (a : Atomic ty) (f : Scalar ty → Scalar ty) (order : Ordering) : Prog (Scalar ty) := do
  let got ← Prog.ask (.update a.cell (fun old => some (f (Scalar.wrap ty old)).val) false order order)
  pure (Scalar.wrap ty got.value)

/-- `swap`. -/
def swap (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun _ => v) order

/-- `compare_exchange` and, with `weak`, `compare_exchange_weak`: `Ok` of what the cell held
where it held `current` and `new` was written, `Err` of what it held otherwise. -/
def compare (weak : Bool) (a : Atomic ty) (current new : Scalar ty) (success failure : Ordering) :
    Prog (core.result.Result (Scalar ty) (Scalar ty)) :=
  if failure.loads then do
    let got ← Prog.ask
      (.update a.cell (fun old => if old = current.val then some new.val else none) weak
        success failure)
    pure (if got.flag then .Ok (Scalar.wrap ty got.value) else .Err (Scalar.wrap ty got.value))
  else Prog.done (.fail .panic)

def compare_exchange (a : Atomic ty) (current new : Scalar ty) (success failure : Ordering) :
    Prog (core.result.Result (Scalar ty) (Scalar ty)) :=
  compare false a current new success failure

def compare_exchange_weak (a : Atomic ty) (current new : Scalar ty) (success failure : Ordering) :
    Prog (core.result.Result (Scalar ty) (Scalar ty)) :=
  compare true a current new success failure

/-- `fetch_add`: wraps on overflow. -/
def fetch_add (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => Scalar.wrapping_add old v) order

/-- `fetch_sub`: wraps on overflow. -/
def fetch_sub (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => Scalar.wrapping_sub old v) order

def fetch_and (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => Scalar.and old v) order

def fetch_or (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => Scalar.or old v) order

def fetch_xor (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => Scalar.xor old v) order

def fetch_nand (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => Scalar.not (Scalar.and old v)) order

def fetch_max (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => if old.val < v.val then v else old) order

def fetch_min (a : Atomic ty) (v : Scalar ty) (order : Ordering) : Prog (Scalar ty) :=
  update a (fun old => if v.val < old.val then v else old) order

end Atomic

namespace AtomicBool

private def ofInt (v : Int) : Bool := v != 0
private def toInt (b : Bool) : Int := if b then 1 else 0

/-- `AtomicBool::new`. -/
def new (v : Bool) : Prog AtomicBool := do
  let got ← Prog.ask (.new (toInt v))
  pure ⟨got.value.toNat⟩

def load (a : AtomicBool) (order : Ordering) : Prog Bool :=
  if order.loads then do
    let got ← Prog.ask (.load a.cell order)
    pure (ofInt got.value)
  else Prog.done (.fail .panic)

def store (a : AtomicBool) (v : Bool) (order : Ordering) : Prog Unit :=
  if order.stores then do
    let _ ← Prog.ask (.store a.cell (toInt v) order)
    pure ()
  else Prog.done (.fail .panic)

/-- A read-modify-write that always writes: what the cell held before. -/
def update (a : AtomicBool) (f : Bool → Bool) (order : Ordering) : Prog Bool := do
  let got ← Prog.ask (.update a.cell (fun old => some (toInt (f (ofInt old)))) false order order)
  pure (ofInt got.value)

def swap (a : AtomicBool) (v : Bool) (order : Ordering) : Prog Bool := update a (fun _ => v) order

def compare (weak : Bool) (a : AtomicBool) (current new : Bool) (success failure : Ordering) :
    Prog (core.result.Result Bool Bool) :=
  if failure.loads then do
    let got ← Prog.ask
      (.update a.cell (fun old => if ofInt old = current then some (toInt new) else none) weak
        success failure)
    pure (if got.flag then .Ok (ofInt got.value) else .Err (ofInt got.value))
  else Prog.done (.fail .panic)

def compare_exchange (a : AtomicBool) (current new : Bool) (success failure : Ordering) :
    Prog (core.result.Result Bool Bool) :=
  compare false a current new success failure

def compare_exchange_weak (a : AtomicBool) (current new : Bool) (success failure : Ordering) :
    Prog (core.result.Result Bool Bool) :=
  compare true a current new success failure

def fetch_and (a : AtomicBool) (v : Bool) (order : Ordering) : Prog Bool :=
  update a (fun old => old && v) order

def fetch_or (a : AtomicBool) (v : Bool) (order : Ordering) : Prog Bool :=
  update a (fun old => old || v) order

def fetch_xor (a : AtomicBool) (v : Bool) (order : Ordering) : Prog Bool :=
  update a (fun old => old != v) order

def fetch_nand (a : AtomicBool) (v : Bool) (order : Ordering) : Prog Bool :=
  update a (fun old => !(old && v)) order

def fetch_not (a : AtomicBool) (order : Ordering) : Prog Bool :=
  update a (fun old => !old) order

end AtomicBool

end Blaze.core.sync.atomic

namespace Blaze.Threads

open Blaze.core.sync.atomic

/-- Why a wait returned. The model of `blaze_core::world::threads::Woke`. -/
inductive Woke where
  /-- A wake took it. -/
  | Woken
  /-- The word did not hold what was expected: the thread never slept. -/
  | Changed
  /-- The kernel woke it for nothing. -/
  | Spurious
  /-- The kernel said its time ran out. -/
  | TimedOut
  deriving Repr, DecidableEq

/-- `blaze_core::world::threads::wait`: sleeps while `word` holds `expected`, until a wake of
it, a return for nothing or, where `timed`, the kernel's word that its time ran out. Run
alone, a wait that sleeps never returns. -/
def wait (word : Atomic .U32) (expected : U32) (timed : Bool) : Prog Woke := do
  let got ← Prog.ask (.wait word.cell expected.val timed)
  pure
    (if got.value = World.changed_code then .Changed
     else if got.value = World.spurious_code then .Spurious
     else if got.value = World.timed_out_code then .TimedOut
     else .Woken)

/-- `blaze_core::world::threads::wake`: wakes one thread asleep in a wait of `word`; whether
there was one. -/
def wake (word : Atomic .U32) : Prog Bool := do
  let got ← Prog.ask (.wake word.cell false)
  pure got.flag

/-- `blaze_core::world::threads::clock`: installs the clock's script, each reading in
nanoseconds; the clock does not move. -/
def clock (readings : Slice core.time.Duration) : Prog Unit := do
  let _ ← Prog.ask (.clock (readings.val.map core.time.Duration.toNanos))
  pure ()

/-- `blaze_core::world::threads::now`: the clock moves to the later of itself and the script's
next reading, and is the answer. -/
def now : Prog core.time.Duration := do
  let got ← Prog.ask (.now true)
  pure (core.time.Duration.ofNanosSat got.value)

/-- `blaze_core::world::threads::time`: the clock, which does not move. -/
def time : Prog core.time.Duration := do
  let got ← Prog.ask (.now false)
  pure (core.time.Duration.ofNanosSat got.value)

/-- `blaze_core::world::threads::wait_until`: `wait` with a deadline on the world's clock
(`none`: none). A sleeper whose deadline the clock has reached returns `TimedOut`; the kernel
may say the time ran out only where there is a deadline, and then the clock is at it. -/
def wait_until (word : Atomic .U32) (expected : U32) (deadline : Option core.time.Duration) :
    Prog Woke := do
  let got ← Prog.ask (.wait_until word.cell expected.val (deadline.map core.time.Duration.toNanos))
  pure
    (if got.value = World.changed_code then .Changed
     else if got.value = World.spurious_code then .Spurious
     else if got.value = World.timed_out_code then .TimedOut
     else .Woken)

/-- `blaze_core::world::threads::two`, its schedule a slice. -/
def two_of {α β : Type} (schedule : Slice Step) (first : Prog α) (second : Prog β) :
    Prog (Ended α × Ended β) :=
  two schedule.val first second

end Blaze.Threads
