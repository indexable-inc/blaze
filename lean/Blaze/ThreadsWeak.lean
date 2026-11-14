import Blaze.Atomic

/-!
The weak-memory world of `Blaze.Threads` (docs/v2/plan/design/proofs-for-sans-io.md, section 8):
release and acquire by views, where a load may read a stale value. It runs the same `Prog`s and
the same `two` as the sequentially consistent `World`; only what one operation does differs.

**Messages and views.** A cell is the list of the messages written to it, oldest first; a
message's place in that list is its timestamp, and "newer" means later in the list. A message
has a unique id (in order of creation), a value, the view its writer published with it, and
`glued`: an update read it and wrote the message right after it in the same step, so nothing is
ever put between the two. A view names, per cell, the id of the newest message of that cell
seen; a cell it does not name it has seen the first message of. The join of two views takes,
per cell, the one whose message is newer.

**Threads.** Thread 0 is the program run alone (before `two`'s threads start and after they
end); threads 1 and 2 are `two`'s. A thread other than 0 with no view yet starts with thread
0's (a spawn shows the child everything its parent saw). Before each operation of thread 0 its
view becomes the join of every thread's view, and the others' views and `loaded` are dropped (a
join shows the parent everything its children saw; a later `two` starts fresh).

**Orderings.** A read acquires under `Acquire`, `AcqRel` and `SeqCst`, otherwise it is relaxed;
a write releases under `Release`, `AcqRel` and `SeqCst`. `SeqCst` means `AcqRel` here and no
more: that is sound, since `SeqCst` promises at least that, and it loses what only `SeqCst`
promises (one total order of all `SeqCst` operations: store buffering is shown here even with
`SeqCst`). A run whose every operation is `SeqCst` is the sequentially consistent world's
(`Prog.valueAny`). An update that writes reads and writes with its `success` ordering; one that
writes nothing reads with its `failure` ordering.

**The world's pick** (from the schedule's step; 0 for thread 0's own operations) says which
message a read reads, counted back from the newest (0 the newest), never older than the
thread's view of the cell. For a store it says where the new message goes: after the message
that many back from the newest, never at or before the thread's view; if that message is
glued, after the first one from there on that is not (the newest never is).

**Operations.**
- A new cell has one message, which its creator's view names.
- A load reads a message by the pick; the thread's view of the cell becomes it; an acquiring
  load also joins the message's view into the thread's; a relaxed one is recorded in `loaded`.
- A store puts a new message where the pick says; the thread's view of the cell becomes it.
  The message's view is the thread's whole view where the store releases, else only itself.
- An update (`weak = false`): pick k reads k back as a load does. Weak: pick 2k reads k back,
  pick 2k+1 fails for nothing having read k back (an LL/SC store-exclusive fails whenever its
  load read a stale value, and returns that value). Where `change` gives a new value and the
  read message is glued, it reads the first one from there on that is not, and decides again.
  Where it writes, the new message goes right after the one read, which becomes glued; the
  views change as an acquiring-or-not load with `success`, then a store with `success`; the
  new message's view is also joined with the view of the message read (C++20's release
  sequence, which aarch64 and x86-64 give: an acquire that reads an update's message
  synchronises with the release its chain of updates started from). Where it writes nothing,
  it is a load of the message with `failure`.
- A futex wait and `asleep` read the cell as a load does, by the pick, but never older than the
  newer of the thread's view and `futex`'s entry for the cell: the kernel's look sees at least
  what every completed wake's caller had seen (trusted, below: what a waker wrote before a
  wake that found no sleeper is seen by a wait that looks after it). The thread's view of the cell becomes the message read. A wait that returns
  woken joins `futex`'s entry for the cell into the thread's view. Where the thread is woken or
  asleep the pick keeps its meaning of the sequentially consistent world (1 for nothing, 2 its
  time ran out). `two`'s `ended` asks `asleep` with pick 0: the newest.
- `wait_until` and `asleep_until` are `wait` and `asleep` with the sequentially consistent
  world's deadline rules added the same way: a sleeper whose deadline the clock has reached
  returns timed out whatever the pick; pick 2 tells a sleeper with a deadline its time ran out
  and moves the clock to the deadline; a thread neither woken nor asleep reads by the pick and,
  where the message holds `expected` and the deadline has passed, returns timed out at once.
- `clock` and `now` are the sequentially consistent world's: the clock is not a cell.
- A wake makes `futex`'s entry for the cell the newer of itself and the waker's view of the
  cell, then wakes as the sequentially consistent world does.
- Rule R1: a write that does not release by a thread other than 0 to a cell `y` is refused
  (`none`, so `Error.unmodelled`) when that thread has done a relaxed read of a cell other
  than `y`. This world has no promises, so it cannot show load buffering, which ARM allows of
  plain loads and stores; a run that could show it is refused, not approximated. Thread 0 is
  exempt: alone, nothing can tell.
- A cell number that is not a cell: `none`.

**Trusted.** That blaze compiles every atomic operation to one whose effect on real hardware,
and under Rust's memory model, is within what its ordering allows here. That an `Acquire` load
is ordered before the thread's later stores (aarch64's LDAR and every x86-64 load give it; the
language's own model does not promise it), and that a `Release` store is ordered after the
thread's earlier loads (aarch64's STLR and every x86-64 store give it; the language's model
does not, since load buffering with relaxed loads and release stores has no happens-before
cycle): R1 lets both through. That the kernel's futex look is as above.

**Not modelled.** Promises, so load buffering (rule R1 refuses what could show it); what
`SeqCst` adds over `AcqRel`; fences.

**No Rust runner.** The Rust `blaze_core::world::threads::two` takes turns and does the real
operation, so it runs the sequentially consistent world only and panics on exactly the runs
that world refuses: the Rust body and `Prog.value` agree wherever it runs, and `valueWeak` has
no Rust runner yet.
-/

namespace Blaze.Threads.Weak

/-- Per cell number, the id of the newest message of that cell seen. A cell not listed: its
first message. -/
abbrev View := List (Nat × Nat)

/-- One write to a cell. -/
structure Msg where
  /-- Unique, in order of creation. -/
  id : Nat
  value : Int
  /-- What its writer published with it. -/
  view : View
  /-- An update wrote the message right after this one in the step it read this one. -/
  glued : Bool
  deriving Repr, DecidableEq

/-- The value under `key` in an association list. -/
def getAt {β : Type} : List (Nat × β) → Nat → Option β
  | [], _ => none
  | (k, v) :: rest, key => if k == key then some v else getAt rest key

/-- The association list with `key` set to `v`. -/
def putAt {β : Type} : List (Nat × β) → Nat → β → List (Nat × β)
  | [], key, v => [(key, v)]
  | (k, u) :: rest, key, v => if k == key then (key, v) :: rest else (k, u) :: putAt rest key v

/-- The place of the message `id` in `msgs`. -/
def idxOf : List Msg → Nat → Option Nat
  | [], _ => none
  | m :: rest, id => if m.id == id then some 0 else (idxOf rest id).map (· + 1)

/-- The place a view's entry names in `msgs`: 0 where it names none. -/
def placeIn (msgs : List Msg) : Option Nat → Nat
  | none => 0
  | some id => (idxOf msgs id).getD 0

/-- The join of two views: per cell, the newer message. -/
def joinView (cells : List (List Msg)) : View → View → View
  | v, [] => v
  | v, (c, id) :: rest =>
    let msgs := (cells[c]?).getD []
    let mine := placeIn msgs (getAt v c)
    let theirs := (idxOf msgs id).getD 0
    joinView cells (if mine < theirs then putAt v c id else v) rest

/-- The place `k` back from the newest of `len` messages, never before `floor`. -/
def back (len k floor : Nat) : Nat :=
  let p := len - 1 - k
  if p < floor then floor else p

/-- The first place from `p` on whose message is not glued. -/
def unglued (msgs : List Msg) (p : Nat) : Nat := go (msgs.drop p) p
where
  go : List Msg → Nat → Nat
    | [], p => p
    | m :: rest, p => if m.glued then go rest (p + 1) else p

/-- `msgs` with `m` put at place `n`. -/
def insertAt : List Msg → Nat → Msg → List Msg
  | rest, 0, m => m :: rest
  | [], _, m => [m]
  | x :: rest, n + 1, m => x :: insertAt rest n m

/-- A weak update's pick `n` as `(k, odd)`, `n = 2k + (odd ? 1 : 0)`. -/
def halve : Nat → Nat × Bool
  | 0 => (0, false)
  | 1 => (0, true)
  | n + 2 => ((halve n).1 + 1, (halve n).2)

/-- A read acquires. -/
def acquires : Order → Bool
  | .Acquire | .AcqRel | .SeqCst => true
  | _ => false

/-- A write releases. -/
def releases : Order → Bool
  | .Release | .AcqRel | .SeqCst => true
  | _ => false

/-- What threads share: messages per cell, each thread's view, and what rule R1 and the futex
need. -/
structure World where
  /-- Per cell, its messages oldest first; a cell's number is its place. -/
  cells : List (List Msg)
  /-- Thread to its view. -/
  views : List (Nat × View)
  /-- The next message id. -/
  next : Nat
  /-- (thread, cell) of every relaxed read (rule R1). -/
  loaded : List (Nat × Nat)
  /-- Per cell, the newest message any thread had seen of it when it called wake on it. -/
  futex : View
  /-- (thread, cell): asleep in a wait of the cell. -/
  asleep : List (Nat × Nat)
  /-- (thread, cell): woken by a wake, its wait not yet returned. -/
  woken : List (Nat × Nat)
  /-- The clock's last reading, in nanoseconds. -/
  clock : Int := 0
  /-- The clock's script: the readings still to come. -/
  readings : List Int := []
  deriving Repr, DecidableEq

namespace World

/-- Nothing shared yet. -/
def new : World := ⟨[], [], 0, [], [], [], [], 0, []⟩

/-- Thread `who`'s view: thread 0's where it has none yet. -/
def viewOf (w : World) (who : Nat) : View :=
  match getAt w.views who with
  | some v => v
  | none => (getAt w.views 0).getD []

/-- Thread 0's view becomes the join of every thread's; the others' and `loaded` are dropped. -/
def rejoin (w : World) : World :=
  let all := w.views.foldl (fun acc tv => joinView w.cells acc tv.2) []
  { w with views := [(0, all)], loaded := [] }

/-- Rule R1: a write by `who` to `cell` with `order` is refused. -/
def refused (w : World) (who cell : Nat) (order : Order) : Bool :=
  who != 0 && !releases order && w.loaded.any (fun tc => tc.1 == who && tc.2 != cell)

/-- `who` reads `m` of `cell` with `order`. -/
def readMsg (w : World) (who cell : Nat) (m : Msg) (order : Order) : World :=
  let v := putAt (w.viewOf who) cell m.id
  let v := if acquires order then joinView w.cells v m.view else v
  { w with
    views := putAt w.views who v
    loaded :=
      if acquires order || who == 0 || w.loaded.contains (who, cell) then w.loaded
      else (who, cell) :: w.loaded }

/-- `who` writes `value` to `cell` with `order`, at place `at` of `msgs` (the cell's messages
as they are to be, but for the new one); its view is also joined with `extra`. Answers the
world after. -/
def writeMsg (w : World) (who cell : Nat) (msgs : List Msg) (at_ : Nat) (value : Int)
    (order : Order) (extra : View) : World :=
  let id := w.next
  let v := putAt (w.viewOf who) cell id
  let placed := w.cells.set cell (insertAt msgs at_ ⟨id, value, [], false⟩)
  let view := joinView placed (if releases order then v else [(cell, id)]) extra
  { w with
    cells := w.cells.set cell (insertAt msgs at_ ⟨id, value, view, false⟩)
    views := putAt w.views who v
    next := id + 1 }

/-- The message a futex look by `who` of `cell` reads with `pick`. -/
def look (w : World) (who cell pick : Nat) (msgs : List Msg) : Option Msg :=
  let mine := placeIn msgs (getAt (w.viewOf who) cell)
  let woke := placeIn msgs (getAt w.futex cell)
  msgs[back msgs.length pick (if mine < woke then woke else mine)]?

/-- One operation by thread `who` with the world's pick. `none`: a cell that is not there, or a
write rule R1 refuses. -/
def step (who pick : Nat) (o : Op) (w : World) : Option (Got × World) :=
  let w := if who == 0 then w.rejoin else w
  match o with
  | .new value =>
    let cell := w.cells.length
    let id := w.next
    some (⟨cell, true⟩,
      { w with
        cells := w.cells ++ [[⟨id, value, [(cell, id)], false⟩]]
        views := putAt w.views who (putAt (w.viewOf who) cell id)
        next := id + 1 })
  | .load cell order =>
    match w.cells[cell]? with
    | none => none
    | some msgs =>
      match msgs[back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell))]? with
      | none => none
      | some m => some (⟨m.value, true⟩, w.readMsg who cell m order)
  | .store cell value order =>
    match w.cells[cell]? with
    | none => none
    | some msgs =>
      if w.refused who cell order then none
      else
        let after := unglued msgs
          (back msgs.length pick (placeIn msgs (getAt (w.viewOf who) cell)))
        some (⟨0, true⟩, w.writeMsg who cell msgs (after + 1) value order [])
  | .update cell change weak success failure =>
    match w.cells[cell]? with
    | none => none
    | some msgs =>
      let (k, spurious) := if weak then halve pick else (pick, false)
      let p := back msgs.length k (placeIn msgs (getAt (w.viewOf who) cell))
      match msgs[p]? with
      | none => none
      | some m =>
        let fail (m : Msg) : Option (Got × World) :=
          some (⟨m.value, false⟩, w.readMsg who cell m failure)
        if spurious then fail m
        else
          match change m.value with
          | none => fail m
          | some _ =>
            let q := unglued msgs p
            match msgs[q]? with
            | none => none
            | some m =>
              match change m.value with
              | none => fail m
              | some new =>
                if w.refused who cell success then none
                else
                  let w' := w.readMsg who cell m success
                  let glued := msgs.set q { m with glued := true }
                  some (⟨m.value, true⟩, w'.writeMsg who cell glued (q + 1) new success m.view)
  | .wait cell expected timed =>
    match w.cells[cell]? with
    | none => none
    | some msgs =>
      if w.woken.contains (who, cell) then
        let v := match getAt w.futex cell with
          | some id => joinView w.cells (w.viewOf who) [(cell, id)]
          | none => w.viewOf who
        some (⟨Threads.World.woken_code, true⟩,
          { w with woken := w.woken.erase (who, cell), views := putAt w.views who v })
      else if w.asleep.contains (who, cell) then
        if pick == 1 then
          some (⟨Threads.World.spurious_code, true⟩, { w with asleep := w.asleep.erase (who, cell) })
        else if pick == 2 && timed then
          some (⟨Threads.World.timed_out_code, true⟩,
            { w with asleep := w.asleep.erase (who, cell) })
        else some (⟨0, false⟩, w)
      else
        match w.look who cell pick msgs with
        | none => none
        | some m =>
          let w := { w with views := putAt w.views who (putAt (w.viewOf who) cell m.id) }
          if m.value != expected then some (⟨Threads.World.changed_code, true⟩, w)
          else some (⟨0, false⟩, { w with asleep := (who, cell) :: w.asleep })
  | .wake cell all =>
    if cell < w.cells.length then
      let w := match getAt (w.viewOf who) cell with
        | some id => { w with futex := joinView w.cells w.futex [(cell, id)] }
        | none => w
      let sleepers := w.asleep.filter (fun it => it.2 == cell)
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
    | some msgs =>
      match w.look who cell pick msgs with
      | none => none
      | some m =>
        some (⟨0, !w.woken.contains (who, cell)
          && (w.asleep.contains (who, cell) || m.value == expected)⟩, w)
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
    | some msgs =>
      if w.woken.contains (who, cell) then
        let v := match getAt w.futex cell with
          | some id => joinView w.cells (w.viewOf who) [(cell, id)]
          | none => w.viewOf who
        some (⟨Threads.World.woken_code, true⟩,
          { w with woken := w.woken.erase (who, cell), views := putAt w.views who v })
      else if w.asleep.contains (who, cell) then
        if Threads.passed w.clock deadline then
          some (⟨Threads.World.timed_out_code, true⟩,
            { w with asleep := w.asleep.erase (who, cell) })
        else if pick == 1 then
          some (⟨Threads.World.spurious_code, true⟩, { w with asleep := w.asleep.erase (who, cell) })
        else
          match deadline with
          | some d =>
            if pick == 2 then
              some (⟨Threads.World.timed_out_code, true⟩,
                { w with asleep := w.asleep.erase (who, cell), clock := d })
            else some (⟨0, false⟩, w)
          | none => some (⟨0, false⟩, w)
      else
        match w.look who cell pick msgs with
        | none => none
        | some m =>
          let w := { w with views := putAt w.views who (putAt (w.viewOf who) cell m.id) }
          if m.value != expected then some (⟨Threads.World.changed_code, true⟩, w)
          else if Threads.passed w.clock deadline then
            some (⟨Threads.World.timed_out_code, true⟩, w)
          else some (⟨0, false⟩, { w with asleep := (who, cell) :: w.asleep })
  | .asleep_until cell expected deadline =>
    match w.cells[cell]? with
    | none => none
    | some msgs =>
      match w.look who cell pick msgs with
      | none => none
      | some m =>
        some (⟨0, !w.woken.contains (who, cell)
          && (w.asleep.contains (who, cell) || m.value == expected)
          && !Threads.passed w.clock deadline⟩, w)

end World

end Blaze.Threads.Weak

namespace Blaze.Threads.Prog

/-- A program run alone in the weak world, from `w`: `exec`'s shape. A wait of its own that
sleeps never returns (`div`); a step the world refuses is `Error.unmodelled`. -/
def execWeak {α : Type} : Prog α → Weak.World → Result (α × Weak.World)
  | done (.ok v), w => .ok (v, w)
  | done (.fail e), _ => .fail e
  | done .div, _ => .div
  | op who pick o next, w =>
    match Weak.World.step who pick o w with
    | none => .fail .unmodelled
    | some (got, w') => if who == 0 && o.sleeps got then .div else execWeak (next got) w'

/-- What a program run alone in the weak world with nothing shared returns. -/
def valueWeak {α : Type} (p : Prog α) : Result α :=
  match p.execWeak Weak.World.new with
  | .ok (v, _) => .ok v
  | .fail e => .fail e
  | .div => .div

/-- A run the sequentially consistent world is the meaning of (every operation of the threads
`SeqCst`, or one cell) is run there; any other is run in the weak world. -/
def valueAny {α : Type} (p : Prog α) : Result α :=
  match p.value with
  | .fail .unmodelled => p.valueWeak
  | r => r

end Blaze.Threads.Prog
