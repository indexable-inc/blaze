---
status: plan
summary: Lean proofs take the place of deterministic-simulation tests for sans-IO Rust. A theorem is an ordinary Rust function marked `#[blaze::theorem]` that returns `bool`; its statement is that it returns `true` for every argument, and returns. A state machine is proved by three of them over a well-formedness predicate written in Rust (it holds at the start, each step keeps it, it implies the property), and one Lean lemma gives every run. Worlds (clock, file system with crashes, locks, later atomics) are Rust predicates saying which answers the kernel may give; a theorem takes the answers as arguments. `blaze prove` emits the models, checks the proofs with the pinned Lean and reports open goals; agents write the proofs. Built so far (2026-11-13): the attribute; methods, integer and Option primitives and theorem statements in the translator; `Blaze.Machine`; `blaze_auto`; the plan of a proof check; `blaze prove` on the first driver (no sandbox yet); and the first proofs of blaze's own code, blaze-core's ETXTBSY retry schedule (two theorems, one sampled test deleted). No world model, no state machine and no simulation test yet. Reviewed from outside the work on 2026-11-13 (its changes are in).
date: 2026-11-13
---
# Proofs in place of simulation tests

[formal-verification.md](formal-verification.md) is the verifier: blaze's MIR to Lean, the
semantics of MIR, the trusted base. This doc is the part that makes it the way sans-IO code is
shown correct, for blaze's own crates first and for anyone's after. It changes one decision of
that doc and adds what was missing to act on it.

## The decision

Andrew, 2026-11-13: "can we make a system so that we can remove these DST stuff and formally
verify? ... this should be useable by anyone instead of doing dst :) for anything sans io rust".

- **A proved invariant has no simulation test.** Once Lean has checked an invariant's proof in
  the gate, the deterministic-simulation test that stood for it is deleted. This reverses
  "DST stays the requirement" of formal-verification.md for code in the provable subset.
- **What a proof does not replace**, and what stands there instead:
  - *The world is a model.* A proof is about a core against a model of the kernel. Each world
    model keeps one conformance test against each real driver (objective V4): every behaviour
    the real kernel shows must be one the model allows. One test per model, not per invariant.
  - *The model is of the MIR, not the binary.* Until the translator's output is proved equal to
    the MIR it came from (V7) and the lowering below MIR is validated (V9), every translated
    function is run on drawn inputs both as its executable Lean model and as built code, and
    the results compared (section 6). That run is generated; nobody writes it.
  - *Code outside the subset* (section 3) keeps its simulation test until the subset reaches it.

## The idea

A property test says "for the inputs I drew, this returned true". A theorem says "for every
input". The function is the same. So:

- **A theorem is a function.** `#[blaze::theorem] fn name(args..) -> bool` is ordinary Rust;
  cargo compiles it as written. Its statement, generated in Lean from its model: for all
  arguments it returns `true`. Returns: not a panic, and not a loop that never ends (there is
  fuel with which the model gives `ok true`). A test fails a hang by its timeout; a statement
  that let a hang pass would prove less than the test it replaces. A precondition is an early
  `return true`.
- **A state machine is proved a step at a time.** The core's author writes
  `fn wf(&self) -> bool` (what always holds of the state) and three theorems:
  `wf` holds of a new state; if `wf(s)` and the world allows the answer, `wf` holds after the
  step; `wf(s)` implies the property. One lemma in Lean (`Blaze.Machine`), proved once, turns
  those into "the property holds after every run". Each goal is about one method, which is
  what automation and agents close, and an edit to one method breaks one goal.
- **The world's answers are arguments.** What a simulation draws from its seed (which
  completion comes next, which effect fails, what a crash keeps) a theorem takes as an
  argument and constrains with the world's `allows` (section 2). "For all allowed answers" is
  every schedule, fault and crash.
- **No new specification language.** Contracts (`requires`, `ensures`) stay for what one call
  promises. A theorem function needs no clause grammar, so blaze's own crates carry theorems
  while cargo still builds them (`blaze-builds-blaze`).

## Pieces

### 1. Statements

- `#[blaze::theorem]` on a function returning `bool`. Refused on anything else, by name.
- The invariant's words stay where they are today, on the item that states it:
  `/// The invariant (proved: <proof file>): ...` on the theorem function.
- The whole-run form (`fn run(ops: Vec<Op>, answers: Vec<Answer>) -> bool`, asserting `wf`
  after each step) is the same statement as a property test. It is what Hegel runs before a
  proof is tried, so nobody proves a false statement; it is not what is proved.

### 2. Worlds

A world is what a core's effects go to. Each is a small, pure Rust type in
`crates/blaze-core/src/world/`: its state, one function per completion that gives the state
after it, and a predicate (`allows`) wherever the kernel has a choice. The relation is the
predicate, so nothing has to show that a script of choices reaches every allowed answer, and
conformance with a real driver is `allows(observed)` (objective V4).

- `clock::Clock` (built 2026-11-14): monotonic time; the next reading is never earlier, and a
  timer is told fired at a reading not before its deadline. Its theorem
  `a_timer_never_fires_early` is proved (`BlazeCore/World/Clock.lean`).
- `disk::Disk` (built 2026-11-14): per file its durable bytes and, in order, the changes made
  since the last sync that covered them; `sync`, `barrier`, `flush`, whole-file writes and
  renames that wait for the drive and that do not, removal; and the crash rule below.
- `asks::World` (built 2026-11-14): over the disk, what is asked and not yet answered, and
  which answer the kernel may give to each: a sync or barrier told done covers at least what
  it owes (the changes there when it was asked and still not durable), a failed append or
  write from memory lands what it may. A crash's witness adds the sectors that writes in
  flight landed, in any order, each at most once.
- `blocking::Blocking` (built 2026-11-14): one call that waits for an event with a time limit
  or none. The kernel may end it for a signal, for nothing or before its limit at any moment,
  and the caller reads the clock some time after; it never says ready before the event, never
  anything else once the event has come, and a limited call is read at most a stated lateness
  past its limit (one bound for the delay before the call starts and after it ends).
- `threads` (section 8): the cells threads share and who sleeps in a futex wait of which.
- Not built: a full disk, short reads, modes and links; a file's identity under a rename
  while a sync of its name is in flight (nothing is promised then); locks, the network,
  processes.

**A loop is proved by running it against its world.** A core that loops until the kernel
lets it stop (a retry loop, a state machine driven to its end) takes its world as one value:
a trait with the calls it makes and the clock it reads. The theorem's function gives the
real loop a kernel that answers from a list, each answer one the world allows where it is
given, and states what holds when the loop ends; a run whose list ends first, or holds an
answer the world refuses, is none of the theorem's. For every list is every run. The first:
`wait/retry.rs`, `a_wait_ends_by_its_deadline`, the whole retry loop of every blocking wait
against `Blocking`, proved 2026-11-14 by one induction on the loop's fuel over an invariant
that ties the retry's last reading to the world's clock; its simulation test and that
test's simulated kernel are deleted. How-to: `agents/howto/H-prove-a-loop-against-a-world.json`.

**A crash is stated by its witness.** `Disk::allows(cut)` takes how much of each file was
kept (`Kept`: the changes kept whole, what landed of the next, whether an unsynced write or
rename is undone) and is a plain check; `Disk::crash(cut)` is the disk after it. A core's
crash theorem quantifies over the witness: for every `cut` that `allows` permits of the disk
the core's own run left, recovery from `crash(cut)` gives a state whose well-formedness and
durable promises hold. A disk a real power cut left conforms when some allowed cut gives it.
The rule, which `Sim::crash` followed and the world now states:

- A crash keeps a file's durable bytes and a prefix of its changes; of the first change not
  kept, a prefix of an append or of a write at an offset may have landed, or any set of the
  512-byte sectors of a write from memory.
- A barrier (`Effect::Barrier`) orders: a crash that keeps any change, whole or in part, of
  any file, made after a barrier completed keeps every change that barrier covers. That
  couples files, so `allows` is one predicate over the whole disk.
- A whole-file write or a rename that waited for no sync may be undone as a whole: the name
  holds what it held before, as far as that was durable; a renamed file is back under its
  old name.

**One world for Linux and macOS.** The difference between the kernels is in the drivers, not
in what an effect promises: `Effect::Sync` is `fsync` on Linux and `F_FULLFSYNC` on Apple's
systems; `Effect::Barrier` is `F_BARRIERFSYNC` there and `fdatasync` on Linux, which is also
durable. The world states the least both give, and making more durable only removes crashes
it allows, so a proof against it holds on both.

**`Sim` is held to the world** by the refinement test `crates/blaze-core/src/sim/world.rs`:
runs of drawn effects, overlapping, failing and crashing, after each of which the
simulation's files are the world's change for change, and each cut `Sim` draws is one the
world allows and leaves the world's disk. `Sim::crash` is split for it into drawing the cut
and applying it. Writing the world found two places `Sim` left its own rule, fixed with it
and kept as controls: a rename's flush missed the file it renamed, and a file a crash put
back kept the barriers of the changes it lost. End state: `Sim` is rebuilt on the world's
types, so the kernel's rules have one home (`one-concept-one-home`).

Worlds are written in the translator's subset as it stands (index loops, `Vec`), so a crash
proof does not wait for iterators and closures.

**The lemmas beside the disk** are Lean theorems about the generated models, proved once and
used by every core's crash proof. Proved: `allows_orders` and `allows_total`
(`BlazeCore/World/DiskAllows.lean`): a cut `allows` accepts that keeps, whole or in part, a
change made after a barrier completed keeps whole all that barrier covers. Stated as theorem
functions in disk.rs, run on drawn disks and cuts, and proved (`BlazeCore/World/Disk.lean`,
2026-11-14): a synced file survives every allowed crash; a file that is only appended to is
cut to a prefix that holds all its durable bytes; a barrier and a later append are ordered.
The first attempt at the third found the statement false (nothing bounded a file's
barriers), which no run of it could. A loop of a model is specified as
an equation that holds with enough fuel, by induction on the fuel, and one constant fuel
serves a whole theorem, since a slice is at most 2^64 - 1 long.

Losing alternatives. A script of choices (`Vec<u64>`) read by a generator: sound only if
every allowed answer has a script, which no proof checks, so a bound off by one quietly
proves less. The kernel written by hand in Lean with a Perennial-style crash logic
(formal-verification.md v3): a second home for the kernel's rules and a logic to port; a
sans-IO core is a step function, and a step function against a world needs only induction
over the run.

### 3. The translator's subset

`hir-ty-lean` v0 took free, non-generic functions over integers, slices, structs and enums.
The subset grows by steps, each a refusal removed and held by golden tests:

| Step | Construct | How |
|---|---|---|
| 0 | opaque items: a type or function the property never looks inside (`io::Error`, `PathBuf`, `format!`'s result, a third-party type) | declared in Lean with no definition; a theorem holds for every meaning of it |
| a | methods, associated functions, `self` in any form (built 2026-11-13) | the same translation, named `Type.method`; each type in a module of its own |
| b | generic functions and types, trait bounds, derived impls, `?` | one module per instance; a bound is resolved to its impl at the instance |
| c | closures, and std's safe combinators (`Option::map`, `is_some_and`, ..) | a closure is a Lean function over its captures; a combinator is translated from `library/` where its source is safe |
| d | `Vec`, `VecDeque`, `BTreeMap`, `String`, `Duration`, `Box` and blaze's own containers | a table from Rust path to Lean model (`Blaze.Std.*`); an entry is trusted until the deep embedding proves it, and what a model assumes of a type (a lawful `Ord` for a map's key) is an obligation on that type, not an assumption |
| e | `for`, ranges and iterator chains | base iterators from the table, adapters translated; loops stay fuel-indexed |
| f | `&mut` inside a value or returned | backward functions, as Aeneas |
| g | recursion | fuel, as loops |

- **Generics are translated by instance** (step b; methods of non-generic trait impls built
  2026-11-14, the rest designed here). A theorem function is closed, so everything it reaches
  is reached at concrete type arguments, as in codegen:
  - An *instance* is a function with every generic argument concrete. A body is translated
    at an instance: each type in it is substituted before it is modelled, each callee's
    generic arguments are substituted and name the callee's instance, and a call of a trait
    method is resolved to its impl at that instance, with the lookup codegen uses (the one
    static dispatch already goes through). A generic struct or enum is a Lean structure per
    instance. Lifetimes are dropped; a const argument is part of the instance.
  - A Lean name is the function's path and its arguments, written so that two instances
    never share one (`Wheel.insert` at `u32` and at `Vec<u8>` are two modules); two that
    would is a refusal, never one replacing the other. Instances are found from the roots,
    so none is made that no theorem reaches; a chain of instances deeper than a stated bound
    (a function that calls itself at an ever larger type) is refused by name.
  - This is also what closures and std's combinators need (step c): a closure is an
    instance of its body over its captures, and `Option::map` is `library/`'s own source at
    the instance of its closure.
  - `blaze prove --subset` counts a generic function by the instances the crate's own
    functions reach: translated when each translates, refused under the first refusal, and
    in a line of its own when nothing reaches it.
  - Losing alternative: Lean polymorphism with a dictionary per trait bound (Aeneas). One
    proof would cover every `T`, but a statement has to be closed to be a theorem function,
    bounds with associated types need a second encoding, and codegen already defines what
    an instance means. It can be added for a theorem over all `T` when one is wanted.
- **What is translated** is the code of the build being proved: no `cfg(test)` field or
  planted bug is in a model.
- **Order.** After step a the next step is the one that unlocks the most functions blocked
  by it alone. `blaze prove` reports that count over every function in a
  `#![blaze::sans_io]` module. On the two cores read so far: the wait's retry needs 0, b, c
  and `Duration`; the profile history's files need f first (a context of five `&mut` fields).
- **Refused**, each by name, until formal-verification.md's v1 and v2: `unsafe`, raw
  pointers, interior mutability, trait objects, atomics, threads. A core that keeps its state
  in a lock-free map is outside the subset until section 8.

### 4. The Lean side

- **Toolchain.** The pin moves from 4.20.0 to 4.35.0 (the current release, 2026-11-13). It
  ships what agent-written proofs lean on: the `grind` tactic, `mvcgen` with `Std.Do`'s Hoare
  triples for monadic code, and verified `TreeMap` and `HashMap` in `Std`.
- **Library.** `Blaze.Std.*` holds the models of step d. `Blaze.Machine` holds what every
  run-shaped proof uses: an invariant of a fold over an operation list, and forward
  simulation giving refinement.
- **Automation first.** Each generated statement is tried with a fixed tactic
  (`blaze_auto`: unfold the model, `mvcgen`, then `grind` and `omega`) before a proof file is
  asked for. What it closes needs no proof file.

### 5. The command

`blaze prove [-p crate]` (formal-verification.md's open question 2, decided: its own command):

1. emits the model and statement of every theorem function;
2. runs the pinned Lean over them and the crate's proof files (`lean/` beside `src/`);
3. reports one record per theorem: `Proved`, `Open { goals }`, `Failed`, `Refused { rule,
   at }`, and the subset's count of section 3. An agent reads the goals, edits the proof file
   and runs it again.

Proved means what formal-verification.md says: the kernel accepts it, no `sorry`, only
Lean's three axioms.

Every theorem's model is compiled, proof file or none, before anything is said of it: `Open`
for want of a proof file means the model builds. A model Lean refuses is `ModelBroken`, never
`Open`: blaze's translator's bug, which no proof can fix, with Lean's first error at its file
and line; like `Open`, it makes the command fail.

**How Lean is run.** The check has a sans-IO core and a driver, as everything does
(`sans-io-cores`):

- *The plan* (`hir_ty_lean::plan`) is pure. From the theorems of one crate and the text of
  its proof files it gives the files to write and the runs of Lean to make, each after the
  runs it imports from, and from what each run printed it gives each theorem's verdict.
  Each run has a key over the Lean release, its module's text and the keys of the runs it
  imports from (`keys-cover-every-input`). One proof file may prove many theorems; the
  library is one set of runs for every crate.
- *The driver* is a run of the daemon's loop, as a build script's is: each step is an
  `Effect::Spawn` of Lean in blaze's sandbox, reading the Lean toolchain and reading and
  writing blaze's own directory, with no network, under a deadline (a timer; past it the
  process is killed and the theorem is `Open`, naming the module). Steps that do not
  import from each other run at once. A proof file's tactics can do IO, so nothing here
  runs outside the sandbox (`sandbox-everything`).
- *Lean reads what blaze keyed.* The proof files are written into blaze's directory from
  the texts the plan was given; Lean never reads the checkout, which may have changed since.
- *Nothing is checked twice.* The driver keeps each accepted run's output under its key; a
  step whose key it holds is not run. The floor of `blaze prove` on an unchanged tree is to
  run Lean not at all; its cost above that is one key lookup a step. After an edit to one
  function it is the runs whose keys moved: that function's model, the proof files that
  import it and their checks.

Generated models are build outputs, keyed by the function's code identity
(`outputs-are-records`); the tree holds the proofs and, per theorem, the hash of the
statement it was checked against. The models of hir-ty-lean's own fixtures stay checked in
under `lean/BlazeModel/`: they are that crate's golden tests.

### 6. Evidence that the model is the code

Until the translation is proved (V7): every translated function is run two ways on the same
drawn arguments, as the executable Lean model and as built code, and the whole results are
compared (harness/v2/lean_differential.py does this for one function today). Running only the
built theorem function would test the code and say nothing of the model.

### 7. Controls

A proof cannot be blind the way a test can, but a statement can be empty. Each proved
invariant keeps one control: a planted bug and one input on which the theorem function,
run on the planted core, is `false`. It is a unit test, or a Lean `example` by evaluation.

### 8. Atomics and threads

A function that touches an atomic is not a function of its arguments: what it does next
depends on what other threads did to the word meanwhile. So its model is not a `Result` but a
program (`Blaze.Threads.Prog`, `lean/Blaze/Threads.lean`): a tree that ends with a `Result`,
or does one operation on what is shared and goes on with the answer. What a thread does
between two operations is its own and is not a step, which is sound for safe code: nothing
else of it can be seen by another thread.

- **Operations.** A new cell, a load, a store, a read-modify-write (a swap, a
  compare-and-swap, weak or not, and every `fetch_*` are each one), a futex wait and a wake.
- **The world** (`Threads.World`) is what is shared: the cells, and who sleeps in a wait of
  which. One step is one operation by one thread, with the world's free choice at that step
  (whether a weak compare-and-swap fails for nothing, whether a sleeper returns for nothing or
  is told its time ran out, which sleeper a wake takes). A wait compares and goes to sleep in
  one step, and a wake finds every sleeper.
- **Two threads.** `blaze_core::world::threads::two(schedule, first, second)` runs two
  closures as threads, one operation at a time in the order `schedule` gives (each entry: who
  goes, and the world's pick), and says how each stood when the schedule ended: returned,
  able to go on, or asleep with no wake coming. It is an ordinary function, so a theorem about
  two threads is an ordinary theorem function that takes the schedule as an argument; proved
  for every schedule, it is every interleaving. In Rust `two` runs real threads that hand a
  baton at each operation, so the theorem's function runs where it is stated, and a drawn
  schedule is a simulation test of the same statement. In Lean it is `Threads.two`, itself a
  program.
- **Lost wakeups are a safety property here.** "The waker returned and the sleeper is asleep
  with no wake coming" is a state `two` reports, so "no wakeup is lost" is "no schedule ends
  there".

**Which memory.** The world is sequentially consistent: an operation sees the last value
written, whatever its `Ordering`. That is what a run means only where its orderings cannot be
told from `SeqCst`, and the world checks that at every step and refuses the run otherwise
(`Error.unmodelled`), never reading a weaker ordering as `SeqCst`:

- every operation of the run's threads is `SeqCst`; or
- the threads touch one cell. Every memory model blaze's targets and Rust have gives the
  writes to one cell a single order that every thread's reads and writes of it respect, so a
  run over one cell is a sequentially consistent run. This covers a park, a once, a flag, a
  counter, a lock word.

**The weak world** (`lean/Blaze/ThreadsWeak.lean`, built 2026-11-14) is what the sequentially
consistent one refuses: two or more cells with an ordering weaker than `SeqCst` on any (a
ring's head and tail). It runs the same programs and the same `two`. Per cell it keeps the
messages written, oldest first; per thread a view (the newest message of each cell it has
seen); the world's pick says which message a load reads, never older than the thread's view,
and where a store's message goes. A release write carries its thread's view and an acquire
read joins it, so what was written before a release is seen after the acquire that reads it.
An update is glued to the message it read (nothing goes between) and continues its release
sequence. A futex look sees at least what the caller of every completed wake had seen. A
spawn gives a thread its parent's view and a join gives the parent its children's.
`Prog.valueAny` runs a program in the sequentially consistent world where that world is its
meaning and in this one where it refuses.

- It has no promises, so it cannot show load buffering, which ARM allows of plain loads and
  stores. A run that could show it is refused (rule R1: a write that does not release, after
  the thread's relaxed read of another cell), not approximated.
- `SeqCst` means `AcqRel` there, which is sound and loses what only `SeqCst` promises; a run
  whose every operation is `SeqCst` is the sequentially consistent world's.
- Its litmus tests are runs by evaluation (`lean/Blaze/Tests/ThreadsWeak.lean`): store
  buffering shown with release and acquire and not with `SeqCst`; message passing kept by a
  release and an acquire and lost without them; coherence; a weak compare-and-swap that fails
  on a stale read.
- The Rust `two` takes turns and makes the real operation, so it runs the sequentially
  consistent world only and refuses the runs that world refuses.
- Which world a theorem is stated in is said by its code (decision D46): `threads::two` is
  the sequentially consistent world, `threads::two_weak` the weak one, and a theorem that
  reaches `two_weak` has its statement over `Prog.valueWeak`. To build: the Rust `two_weak`,
  which is the weak world's step run (each scheduled atomic a list of messages in the run's
  world, each thread a view, a read's message and a store's place chosen by the step's pick,
  rule R1 refused by a panic that names it; when the run ends every word holds its newest
  message's value), held to the Lean world by its litmus runs restated in Rust with the same
  schedules and the same answers; the translator's entry for it and the statement's choice;
  the induction over a schedule for the weak world (`two_exec` over `execWeak`); and the
  first proof, `handoff::Wake`: a sender's relaxed store of its slot and its `wake`, and the
  driver's `drain` and relaxed read of the slot, for every schedule and every stale read:
  the driver saw the slot or the sender was told to wake it. `Prog.valueAny` goes with it.

**Trusted.** That the kernel's wait is atomic with its comparison and a wake finds every
sleeper, and that what a waker wrote before a wake that found no sleeper is seen by a wait
that looks after it; that blaze compiles each atomic operation to one whose effect is within
what its `Ordering` allows in the world; that an `Acquire` load is ordered before its thread's
later stores and a `Release` store after its earlier loads (aarch64's LDAR and STLR and every
x86-64 load and store give both; Rust's own memory model promises neither); that safe code
shares nothing but atomics.

**The method** (`lean/Blaze/ThreadsAuto.lean`): name each thread's program points (the
continuation after each of its operations), list the reachable states (both threads' points,
the cells, who sleeps), show each thread's step with any pick keeps the list (`two_ends`, by
induction on the schedule), and read the theorem off the states with no step left. The first
proof, of the park's order of atomics written by hand, has seven points for the park, three
for the unpark and 21 reachable states (`lean/Blaze/Tests/ThreadsParkProof.lean`).

Built 2026-11-14, all of it: both worlds; the method; the Rust `two` with scheduled atomics
behind the facade `blaze_core::atomic` (`crates/blaze-core/src/world/threads.rs`); the
translator (a function that reaches an atomic operation, `two`, `wait` or `wake`, or a
function that does, is emitted as a `Threads.Prog`, and a shared theorem's statement is that
its program's value is `ok true`); and the first proof of real lock-free code,
`no_unpark_is_lost` (`wait/parking.rs`, `BlazeCore/Wait/Parking.lean`): the real `park` and
`unpark` as two threads, every schedule, every choice of the kernel's, the retry's count and
the error path of a clock that stands still: 8 kinds of park point, 3 of the unpark, 26
reachable states. The park's simulation test ran an unpark whole between two of the park's
atomics and so never tried a park's step between the unpark's swap and its wake; it stays
for what the theorem does not state, that a park ends by its deadline and promptly after its
wake on a clock that moves.

A model names its thread's program points (built 2026-11-14): the code after the n-th
operation of `f` is the definition `f.at_n`, and inside its k-th loop `f.loop_k.at_n`, with
the loop itself a parameter; a tail two branches share is one definition, and a loop is
written once. A proof states its points as these names and does not copy the model's
continuations (the park's proof was ported to them; agents/annoyances/proofs-1113-11.json).

Next: the proofs of the clock's three statements below; a multi-cell structure against the
weak world. How-to: `agents/howto/H-prove-two-threads.json`.

**A clock for the threads world** (built 2026-11-14, its theorems stated and run, not yet
proved; it is what the park's simulation test still states and `no_unpark_is_lost` does not:
that a park ends by its deadline, and promptly after its wake).

- The world gets a clock and a script of readings. `threads::clock(&readings)` installs the
  script before a run; `threads::now()` is one operation: the clock becomes the later of
  itself and the script's next reading, and that is the answer; `threads::time()` reads the
  clock without moving it (for a theorem's function to ask after a run). Time moves only
  where someone looks, which loses nothing: a run's observations are its readings, in order,
  and any nondecreasing sequence of them is a script.
- `threads::wait_until(word, expected, deadline)` is the wait with a deadline on that clock:
  the kernel may say the time ran out only where there is a deadline, and saying so moves
  the clock to at least the deadline (it said so at or after it); a thread asleep in such a
  wait whose deadline the clock has reached does not stay asleep (its next look returns
  timed out, whatever the pick), and is not `Asleep` when a schedule ends.
- The theorems, of the real `park` on a kernel whose wait is `wait_until` and whose clock is
  `now`: (1) a park that says its deadline passed says so only with the clock at or past it;
  (2) with the clock at or past its deadline, no schedule ends with the park asleep; (3)
  left alone for eight of its own steps with the kernel quiet (pick 0), a park has returned
  or sleeps, from every state a schedule reaches. With `no_unpark_is_lost`, (2) and (3) say
  what the simulation test's "promptly" and "by its deadline" say, in steps, and (1) its
  "never before". The test's simulated latencies have no counterpart: a step is a step.
- Built as the first slice was: the Lean world (`Op.clock`, `Op.now`, `Op.wait_until` in
  `lean/Blaze/Threads.lean`) and its examples by evaluation (`lean/Blaze/Tests/ThreadsClock.lean`);
  the Rust `two` to match (`threads::{clock, now, time, wait_until}`, a reading in whole
  nanoseconds, exact); their entries in the translator's table; the statements
  (`a_park_keeps_its_deadline` for (1) and (2), `a_park_left_alone_returns_or_sleeps` for (3),
  in `wait/parking.rs`), run on drawn schedules and readings with a planted kernel for each
  (one that says a deadline early, one whose wait has no deadline, one that never sleeps).
  Their proofs (`BlazeCore/Wait/ParkingTime.lean`) are a port of the park's to a kernel and a
  clock that are operations of the run; until they are checked the park's simulation test
  stays.

## Costs

- **Checking.** The floor is to re-check only theorems whose generated module, imports or
  proof file changed; the proof key of formal-verification.md already covers exactly those.
  Measured 2026-11-13 on the build VM at e6635c9ba7: the whole of `lean/` (18.7k lines, 85
  modules) builds from nothing in 9 s with Lean 4.20.0.
- **Writing.** The one proof in the tree is 597 hand-written lines for a 9-line function, 445
  of them the link from model to MIR. So proofs are written by agents, the model-to-MIR link
  is generated or it does not scale (V7), and automation is tried first.
- **Keeping.** A change to proved code that changes its model fails the gate until the proof
  is repaired. That is the price of deleting the test; it is paid by an agent run on the
  failing goals.

## What this does not reach

- What a simulation test also covered by running: the drivers, the lowering below MIR, a
  hang in real code. The conformance tests, section 6 and the statement's "returns" stand
  for these; where they do not, the simulation test stays and the invariant is not marked
  proved.

- A property of the real kernel, real hardware timing, or another program's format has no
  statement to prove. It keeps a test.
- Liveness under fairness. "Ends within N steps" is provable (fuel with a bound); "ends
  eventually if the scheduler is fair" is not attempted.
- A proof an agent cannot find. The invariant then stays at the level it has.

## Order of work

1. The pipeline on real functions of blaze (done 2026-11-13 for
   `blaze_core::busy`: two theorems proved, one sampled test deleted; held by the lake gate
   and a golden test until `blaze prove`'s driver is the gate).
2. Opaque items, methods and `Duration`: the wait's retry step (`Retry::left`, `Retry::after`
   in `blaze_core::wait::retry`) against a clock world. Its loop `drive` (a closure and a
   trait bound) follows with steps b and c; until then the DST stays.
3. A refinement against a reference model (a chunked map against a sorted map).
4. A crash theorem on the `Disk` world: the profile history is exactly-once after any crash.
5. Then by class, in lanes, in the order invariants.md's classes are cheapest to prove: pure
   functions, state-machine invariants, refinements, crashes; incremental-equals-clean as one
   theorem about the query engine.

## Checks

Ledger rows V13 to V17 (objectives.json): the first proved invariant with its test deleted;
the worlds with `Sim` held to them; the subset's coverage of sans-IO functions as a number;
the count of invariants at `proved`; atomics.

The models under `lean/BlazeModel/blaze_core` are what the proofs in the tree were checked
against, so they must be what `blaze prove` emits from the source today:
`harness/v2/lean_gate.py '{"command": "models", "emitted": "<target>/lean/lib"}'` compares
every model a run emitted with the checked-in one, byte for byte. It needs a built blaze and
is run by hand after `blaze prove -p blaze-core` until the release train runs it.
