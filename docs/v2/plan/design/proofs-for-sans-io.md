---
status: plan
summary: Lean proofs take the place of deterministic-simulation tests for sans-IO Rust. A theorem is an ordinary Rust function marked `#[blaze::theorem]` that returns `bool`; its statement is that it returns `true` for every argument, and returns. A state machine is proved by three of them over a well-formedness predicate written in Rust (it holds at the start, each step keeps it, it implies the property), and one Lean lemma gives every run. Worlds (clock, file system with crashes, locks, later atomics) are Rust predicates saying which answers the kernel may give; a theorem takes the answers as arguments. `blaze prove` emits the models, checks the proofs with the pinned Lean and reports open goals; agents write the proofs. Built so far: the translator takes methods; the base is formal-verification.md's v0. Reviewed from outside the work on 2026-11-13 (its changes are in).
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

A world is what a core's effects go to. Each is a small, pure Rust type with two functions:

- `allows(&self, asked: &Effect, answer: &Answer) -> bool`: whether the kernel may answer
  so, in this state. A crash is an answer that carries the disk it left.
- `apply(&mut self, asked: &Effect, answer: &Answer)`: the world after it.

The relation is the predicate, so nothing has to show that a script of choices reaches
every allowed answer, and conformance with a real driver is `allows(observed)` on what the
kernel did (objective V4). For tests, `pick(&self, asked, choices: &mut Choices) -> Answer`
draws an answer, with `allows(pick(..))` proved once.

- `Clock`: monotonic time; an answer is the next reading, never earlier.
- `Fs`: per file what is durable and what is only written; `sync`, `rename`; a crash may
  leave what was synced plus a prefix of the later changes with the last torn at a 512-byte
  sector, an unsynced rename undone, and the write barriers `Sim` models
  (`crates/blaze-core/src/sim.rs`, `crash`); `ENOSPC`; the differences between Linux and
  macOS (what `fsync` makes durable, a rename before its data) as a parameter.
- `Locks`, `Net`, processes: as `Sim` has them.
- Later, `Atomics` (section 8).

Worlds are written in the translator's subset as it stands (index loops, `Vec`, sorted
`Vec` maps), so the first crash proof does not wait for iterators and closures. End state:
`Sim` is rebuilt on these types, so the kernel's rules have one home
(`one-concept-one-home`); until then a refinement test holds `Sim` to them.

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

### 8. Atomics and weak memory

The same idea one level down. A function that uses atomics is translated into a monad whose
atomic operations are effects; a memory world (`Atomics`) says which value a load may see,
and which thread steps is an answer like any other. A theorem about a ring is then "for all
schedules and all allowed reads".

- **The world must allow everything the hardware and the compiler do.** RC11 forbids load
  buffering, which ARM allows for relaxed accesses, so a proof under RC11 says nothing of a
  `Relaxed` load on the Mac. Sequentially consistent and release-acquire code gets the
  view-based model; `Relaxed` is refused until the world has promises (the promising
  semantics).
- **Stepping only at atomic operations** is sound for safe code, whose other accesses cannot
  race; unsafe code needs a race check with it.

Not started. The first target is formal-verification.md's (the flight recorder's ring move).

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

1. The pipeline on one real function with nothing new in the translator:
   `quiet_turns_after` (blaze-cargo). `#[blaze::theorem]`, `blaze prove`, the first
   `(proved: ..)` in invariants.md, its table test deleted.
2. Opaque items, methods and `Duration`: the wait's retry step (`Retry::left`, `Retry::after`
   in `blaze_core::wait::retry`) against a clock world. Its loop `drive` (a closure and a
   trait bound) follows with steps b and c; until then the DST stays.
3. A refinement against a reference model (a chunked map against a sorted map).
4. A crash theorem on the `Fs` world: the profile history is exactly-once after any crash.
5. Then by class, in lanes, in the order invariants.md's classes are cheapest to prove: pure
   functions, state-machine invariants, refinements, crashes; incremental-equals-clean as one
   theorem about the query engine.

## Checks

Ledger rows V12 to V16 (objectives.json): the first proved invariant with its test deleted;
the worlds with `Sim` held to them; the subset's coverage of sans-IO functions as a number;
the count of invariants at `proved`; atomics.
