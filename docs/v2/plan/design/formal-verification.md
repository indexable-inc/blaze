---
status: plan
summary: The verifier: blaze translates its own borrow-checked MIR to Lean 4 and the pinned Lean checks proofs of specs written once in blaze's extensions. v0 (safe Rust by functional translation) is written in crates/hir-ty-lean with the first proof in the tree; lean/Blaze/Mir is the executable Lean semantics of MIR for a first subset that models, passes and the evaluator are held to, with its trusted base listed; the Verus tier was retired on 2026-11-10. Planned: v1 unsafe and interior mutability in an Iris-style logic, v2 atomics (SC, then RC11), v3 crashes on files and mmap (Perennial-style, sharing the DST's kernel model), v4 compiler correctness. For sans-IO code a proof takes the place of the simulation test (decision D38, proofs-for-sans-io.md); what is not proved keeps its simulation test. A second package, lean/analysis, proves blaze-diff's derivative rules and fast_math rewrites over the reals with Mathlib, from the rule table the Rust differentiator runs.
date: 2026-11-10
---
# Formal verification: blaze's MIR to Lean

This is the plan for proofs in blaze. blaze translates its own MIR to Lean 4, and a Lean
program logic proves the specs. v0 is written ("v0 as built" below); the later phases are plan. Other docs own the parts this one builds on, and it links
to them rather than repeating them:

- **The spec form**: the native clause grammar (`requires`, `ensures`, `invariant`,
  `decreases`, a named return) and `VerifyRefused`, which lane verus built and
  [verus.md](verus.md) records. The interface `proved(..)` is `crates/hir-ty-lean`'s since
  2026-11-10.
- **Checked, generated, used contracts**: [contracts.md](contracts.md).
- **Purity and effects**: [pure-fns.md](pure-fns.md) and [effects.md](effects.md).
- **How unsafe code is checked today**: [unsafe.md](unsafe.md), with the shared-memory rules in
  [concurrency.md](concurrency.md).
- **Testing, including DST**: the
  [testing skill](../../../../.agents/skills/testing/SKILL.md).
- **The store and incremental checks**: [verify.md](verify.md). Despite its name, that doc is
  not about program proofs.

## Decisions (2026-11-04; the Verus tier retired 2026-11-10)

### Lean, not Verus

- **One prover for everything.** Lean, with a program logic written in Lean, covers pure
  code, unsafe code (separation logic), concurrent code (atomics, then weak memory), durable
  code (crash plus concurrency), and later the compiler's own rules (layout, const eval, trait
  resolution). Verus is built around SMT queries over a first-order encoding. It can encode
  ownership ghost state, but weak memory and crash-aware logics are outside what it was
  designed for, and nobody has built them on it.
- **No SMT brittleness.** A Lean proof is a term that the kernel checks. It does not shift
  when a solver version changes, a seed changes or a trigger heuristic changes. Lean's
  `maxHeartbeats` is a deterministic budget, as Z3's `rlimit` was in lane verus. But a failed
  Lean proof fails for a reason the user can read, not because of an `unknown`.
- **Agents write most proofs, and the AI-prover ecosystem is strongest on Lean.** Microsoft's
  SymCrypt work verifies production Rust through Aeneas and Lean, and agents write the proofs:
  237 KLOC of Lean for 16.7 KLOC of Rust, re-checked by the Lean kernel
  ([blog, 2026-07](https://www.microsoft.com/en-us/research/blog/verifying-rust-cryptography-in-symcrypt-from-standards-to-code/),
  [paper](https://www.microsoft.com/en-us/research/publication/scaling-verification-of-cryptographic-software-with-aeneas-rust-and-lean/)).
  A wrong proof from an agent costs nothing, because the kernel rejects it.
- **Expressive enough for what blaze needs to prove.** Iris is now ported to Lean
  ([iris-lean](https://github.com/leanprover-community/iris-lean)). Perennial's default
  branch is now `lean`, built on iris-lean ([mit-pdos/perennial](https://github.com/mit-pdos/perennial)).
  So the crash-aware concurrent separation logic that v3 needs already exists in the target
  prover.
- **What Verus was better at.** First proofs came faster, SMT automates linear arithmetic and
  quantifier reasoning, and
  [AutoVerus](https://www.microsoft.com/en-us/research/publication/autoverus-automated-proof-generation-for-rust-code/)
  shows LLMs proving Verus-Bench tasks. That was the reason to keep lane verus as the
  near-term prover; on 2026-11-10 Andrew decided against two provers (see "Verus retired").
- **No rustc.** Upstream Verus (`rust_verify`) and Aeneas's front end, Charon, are both
  rustc drivers. `no-rustc` forbids them. Lane verus had replaced Verus's rustc end with a
  lowering from blaze's HIR. blaze does the same for Aeneas: it translates from its own front
  end, and nothing runs rustc. Neither prover was ruled out by rustc. Lean won on capability.

### DST on everything, proofs on small stable cores

Changed on 2026-11-13 (decision D40, [proofs-for-sans-io.md](proofs-for-sans-io.md)): for
sans-IO code the translator reads, a proof Lean has checked in the gate takes the place of
the simulation test, which is deleted; that doc says what stands for what a proof does not
cover (a conformance test per world model, and each translated function run as its Lean model
and as built code). The first two points below are as they were decided on 2026-11-04 and
hold for code that is not proved.

- **DST stays the requirement.** The testing skill's `dst-not-review` rule holds for all
  concurrent, durable, crash-recovering, billing and data-deleting code, proved or not. A
  proof covers a model of the code. A DST runs the real binary that blaze built.
- **What gets a proof.** A small core whose interface has stopped moving and whose bug would
  cost the most: a codec, a merge plan, a ring's move protocol, a commit protocol. Proving
  code whose shape is still changing wastes the proof, just as `testing` says about unit
  tests.
- **One kernel and crash model, shared.** The semantics of fsync, rename, mmap and torn
  writes are written once, as a Lean model. `hegel::sim`'s simulated kernel follows the same
  model. Two checks keep the model honest:
  1. **DST traces checked against the model.** The sim's trace of effects and crash states
     is replayed through the Lean model, compiled to an executable, which must allow every
     state the sim produced.
  2. **Real-kernel replay.** Crash tests run on real Linux and macOS in ix VMs, cutting
     power at the block layer (for example with dm-log-writes). Every state observed after
     the crash must be one the model allows.

  If the model allows too little, the replay finds it. If it allows too much, both proofs
  and DSTs get weaker, so each new rule in the model is tied to a case that actually happened.

### Specs written once, in blaze's extensions

- **One home for specs: the Rust source.** Specs are the native clauses, plus
  `#[blaze::pure]`, the effect attributes, and `ghost`/`tracked`/`proof fn` code
  (`one-concept-one-home`). There are no specs in a Lean-side DSL and no second annotation
  syntax. Every Lean theorem statement about Rust code is generated from these specs.
  Proofs, meaning tactic scripts, live in Lean files.
- **The translation is a front-end query** in the front end's query crates, keyed like
  any other query (`semantics-in-front-end`). The policy (which crates get verified, the
  proof cache, how output is rendered) lives outside the front end. The Lean-side library (the
  monad, machine integers, the std model) is library code anyone can read
  (`library-over-language`).
- **Effects make models simpler.** If the effects walk proves a function `#[blaze::nopanic]`
  and the function terminates, its model is a plain Lean function rather than a `Result`. A
  `#[blaze::pure]` function's model can be called directly from specs. These are facts the
  front end has already proved, so they cost no new trust.

### Verus retired (2026-11-10)

Andrew decided on 2026-11-10 that Lean is the prover and the Verus tier goes now, not when v0
proves every clause it proved: two provers are two trust bases and two lowerings of one spec.

- **Deleted:** `crates/hir-ty-verus` (the VIR lowering and the Z3 driver), `vendor/verus`,
  `tools/v2/verus_strip.py`, the pinned Z3 and `BLAZE_Z3`. `.clippy.toml` bans the removed
  names with their replacements. [verus.md](verus.md) is the record.
- **One home for specs.** The clauses are in the Rust source; hir-def lowers them to clause
  groups (`ExpressionStore::spec_clauses`); `crates/hir-ty-lean` states them in Lean
  (`spec.rs`) and owns `proved(..)`, `Clause` and `ProofKey`; `crates/hir-ty-contracts` reads
  the groups for the facts the optimizer may use and asks `proved` through the prover it is
  given.
- **What lost its prover.** Each is reported "not proved", with why, and never passed:
  - an `ensures` with no proof file: Z3 proved linear arithmetic over integers and booleans
    from the clause and the body alone; Lean checks a proof someone wrote and searches for
    none. Contracts part 3's `r < K` facts are in this class until their callees have proofs;
  - a loop's `invariant` as an obligation, and termination from `decreases`: the Lean
    statement of a function with a loop is partial correctness, and `decreases`, `recommends`
    and a loop's `ensures` are refused by name;
  - a counterexample from the solver's model: contracts part 2's Hegel run is the one source.

## Shared pieces (all phases)

### The pipeline

1. **Structured MIR.** A query over blaze's borrow-checked, built MIR (before optimization).
   Without optimization, the model is of what the programmer wrote, under the pinned
   nightly's semantics. The query reconstructs `if`/`match`/`loop` from the CFG, the way
   Charon makes LLBC from ULLBC ([Charon](https://arxiv.org/abs/2410.18042)). It also folds
   checked arithmetic and bounds assertions back into single operations. Every back end below
   reads this one query.
2. **Spec lowering.** The clause groups (hir-def's), which MIR building drops, are
   lowered from typed HIR into Lean terms: `int`/`nat` for spec arithmetic, `forall`,
   `==>`, `old`, and `spec fn`.
3. **Lean emission.** One Lean module per item instance, so a module's imports are exactly
   the item's dependencies. Each module holds the model, the generated theorem statements
   for its clauses, and a reference to the proof by name.
4. **Proof check.** blaze runs the pinned Lean toolchain on the generated module plus the
   user's proof file. The toolchain is a pinned input that blaze fetches itself, as the
   remove-nix plan requires; today `BLAZE_LEAN_PROVER` names its `lean` by an absolute path.

### Where proofs live and what counts as proved

- **Proof files.** A crate's proofs live in `lean/` beside its `src/`, one file per module,
  with theorems named by item path. blaze generates the statement, and the proof file only
  supplies `:= by ...`. If the proof file restates a theorem whose generated statement has
  changed, that is a typed error naming the item and the clause, never a silent re-binding.
- **Proved means all of these:**
  - the kernel accepts the theorem;
  - it contains no `sorry`;
  - `#print axioms` lists only allowed axioms: Lean's three (`propext`, `Classical.choice`,
    `Quot.sound`), the std model's named axioms, and the axioms that phase lists.
- **Native evaluation is opt-in.** Tactics that trust the compiler (`native_decide`,
  `bv_decide`'s `ofReduceBool`) are off unless a crate opts in. The proof key records the
  choice (open question 5).

### Incremental keys (`keys-cover-every-input`)

- **The translation key** for an item instance covers:
  - its behaviour identity ([code-identity.md](code-identity.md));
  - the translator's version;
  - the profile's `overflow-checks` and panic strategy;
  - the target's pointer width and endianness;
  - the std model's version;
  - the specs of every callee. A callee's body is not an input, because verification is
    modular.
- **The proof key** covers:
  - the generated module and every module it imports;
  - the proof file;
  - the Lean toolchain and the pins of the Lean libraries (fork of Aeneas's Lean library,
    iris-lean, Mathlib if used);
  - `maxHeartbeats`;
  - the axiom allow-list.
- **Storage and round trip.** Verdicts persist in the store under the proof key. A
  round-trip test restores them and compares them byte for byte with a fresh run
  (`persisted-state-round-trips`). Two verdicts under one key is a refusal.

### The command and its output

`blaze check --verify` (one command for checks; not built) runs every changed obligation. It reports each obligation as one record, rendered for a terminal and given to
agents as data over the agent API ([roadmap §8](../../plan.md)):

```text
{ item, clause, prover: lean, key,
  outcome: Proved
         | Open { goals }            -- the remaining Lean goals, pretty-printed, hypotheses named
         | Failed { tactic_error }
         | Unknown { heartbeats }
         | Refused { rule, at },     -- the VerifyRefused rule and the construct
  counterexample?,                   -- from contracts part 2's Hegel run, which goes first
  model: path of the generated Lean }
```

- **The goal state is the main value for an agent.** An agent edits the proof file, reruns,
  and sees the goals that are left.
- **`Open` and `Unknown` never count as success.**
- **Hegel goes first.** A Hegel counterexample (contracts part 2) is reported before any
  proof is attempted, so nobody spends effort proving a false spec.

## v0: safe Rust, functional translation

**Scope.** Safe, sequential Rust, translated the way Aeneas does it
([ICFP 2022](https://dl.acm.org/doi/10.1145/3547647),
[repo](https://github.com/AeneasVerif/aeneas)).

**Ownership.**
- Borrow checking has already run, so ownership lets each `&mut` become value in, value out.
- A function that returns a borrow gets a forward function plus one backward function per
  lifetime. The backward function takes the updated borrowed value and returns the new values
  of what was lent.
- Shared borrows become plain values.

**Control flow.**
- Each loop becomes a recursive function over its live variables. A loop `invariant` and
  `decreases` from the native clauses become its induction hypothesis and termination
  measure.
- A loop without `decreases` is translated as partial, either through Lean's
  `partial_fixpoint` or through a divergence case in the monad, as Aeneas's Lean backend does.
  Only partial correctness is proved for it.

**Panics and overflow** match the pinned nightly (nightly-2026-10-27):
- The model's monad is `Result` with `fail panic`.
- Arithmetic `+ - * << >>` panics on overflow when the profile's `overflow-checks` is on and
  wraps when it is off. The flag is part of the translation key.
- Division by zero, `MIN / -1`, and indexing out of bounds always panic.
- `panic=abort` and `panic=unwind` produce the same model in v0, and the reason is
  structural. `catch_unwind` is refused. Safe v0 code has no interior mutability. So nothing
  a caller can observe survives a panic: drops during unwinding change only owned memory, and
  that memory is gone. A panic inside a drop during unwinding aborts, which is also `fail`.

**Traits and generics.**
- Each trait becomes a Lean structure, each impl an instance, and associated types become
  fields or `outParam`s.
- Generics become Lean polymorphism. Const generics become values (`Usize`).
- Trait method calls resolve through blaze's own trait solver to an instance, or to a
  dictionary argument when the item is generic.

**Closures.** A closure becomes a structure of its captures plus a `Fn`, `FnMut` or `FnOnce`
instance. An `FnMut` call is value in, value out over the captures. A function pointer is a
closure with no captures.

**The std/core model.** Hand-written Lean specs for the subset blaze's targets use: integers,
`Option`/`Result`, slices, arrays, `Vec`, `String`/`str` basics, the iterator adapters actually
used, `sort_unstable_by_key`, and `chunk_by`.
- **Starting point.** It begins from a fork of Aeneas's Lean library (its `Std`, the monad,
  and the `progress` tactic), so SymCrypt-style proof scripts and agent prompts carry over.
- **Trust.** It is part of the trust base. Each model function carries the pinned library
  path it stands for, and a Hegel differential test compares the Lean model, compiled, with
  the real std function that blaze built, on drawn inputs.

**Refused.** Each names the item, the rule and the construct (`hir_ty_lean::NotTranslated`
today; the `VerifyRefused` diagnostic once `blaze check --verify` reports it).

| Rule | What it covers |
|---|---|
| `Unsafe` | an `unsafe` block or `unsafe fn` in a translated body |
| `RawPointer` | raw pointers in a translated body |
| `InteriorMutability` | a type that is not `Freeze`, naming the `UnsafeCell` path |
| `Atomic` | atomic types and operations |
| `Ffi` | `extern` calls, `static mut` |
| `Async` | `async`/`gen`/coroutines |
| `CatchUnwind` | `catch_unwind` |
| `InlineAsm` | inline assembly |
| `DynTrait` | trait objects (v0.x lifts this) |
| `NestedBorrow` | a borrow inside a type argument or in an ADT field, in the first cut (v0.x lifts this with per-lifetime backward functions) |
| `NoModel` | a call into std or another crate with neither a translated body nor a model |

Unsafe code inside std is fine when std's model covers it. The walk that decides is pure's
transitive walk, so a refusal names the shortest call chain to the construct.

**LLBC and Aeneas versus our own translator: we write our own, and keep LLBC as an oracle.**
- **The primary path emits Lean directly** from structured MIR. Aeneas is an OCaml batch tool
  over a whole crate's LLBC. It does not fit per-item queries and proof caching, it cannot
  read our extensions (`ghost`/`tracked`, effect facts, the native clauses), and v1 needs a
  deep embedding that Aeneas does not have. Having one translator that we own also gives v4
  something to prove correct.
- **An LLBC emitter is a second, small back end** on the same structured-MIR query. It writes
  Charon's JSON at Charon's pinned version, so upstream Aeneas can run on a corpus. Its
  output is checked two ways: (a) both Lean models run on Hegel-drawn inputs and must agree,
  including on panics; and (b) for small functions, a Lean equivalence theorem between the two.
  This is the translator's test oracle in CI.
- **It is not the bootstrap.** Charon's LLBC tracks rustc's MIR closely, and blaze's MIR
  differs in places. Matching it costs effort that only pays off as a cross-check.
- **Shared with Aeneas:** the Lean library (forked) and the translation scheme. Not shared:
  the code that produces the model.

**How the translator is checked.**
- The LLBC oracle above.
- Differential execution: blaze-built Rust against the compiled Lean model on Hegel-drawn
  inputs, for every function in the corpus.
- Later, v4's soundness proof.

**Trust base:**
- the Lean kernel;
- blaze's parser through MIR building and borrow checking;
- the structured-MIR query and the Lean emitter;
- the std model and its axioms;
- blaze's code generator, which turns the same MIR into machine code. v4 shrinks this, but
  until then the proof is about the program's MIR semantics, not the binary.

**First target: `compact::plan` preserves rows.** `crates/blaze-profile/src/history/compact.rs`
is already a pure function of file names with no IO, and it uses only slices, `Vec`, sorting,
`chunk_by`, and `Vec::remove`. The lemma, stated as an `ensures` on `plan`, says:
- every input index is in at most one merge;
- each merge's inputs share one (build, table);
- each merge's output covers exactly the union of its inputs' `first..=last`, at a level above
  every input.

So the multiset of (build, table, seq) rows listed after the merges are applied equals the
multiset listed before.

Second targets:
- `Name::parse(n.file_name()) == Some(n)` for every well-formed `n`. This needs a model of
  `format!` for integers and `str::parse::<u64>`.
- The manifest's `encode`/`decode` round trip. Today it goes through `serde_json`'s derive,
  which brings in a large std and serde model. It becomes a target once the manifest codec is
  a small sans-IO core of its own (open question 3).

**Rough size:**
- structured MIR plus the emitter: 10 to 15k lines of Rust;
- the forked Lean library plus std model for the targets: 3 to 6k lines of Lean;
- the `plan` proof: 1 to 3k lines of Lean, mostly written by agents.

**Risks:**
- The std model grows with every target, and it is trusted.
- Iterator-heavy code produces large models.
- `NestedBorrow` refuses more of blaze's code than expected. Measure it over blaze's own
  crates before committing to the first-cut limit.

**CI:**
- The merge gate runs `blaze check --verify` over the verified crates with the warm proof
  cache, so only changed keys rerun.
- The release train runs a cold replay of every proof, plus the LLBC oracle and differential
  runs on the corpus.

## v0 as built (2026-11-10)

`crates/hir-ty-lean` (the translator and the driver), `lean/` (the library and the checked-in
models) and `crates/blaze-graph/lean/` (the first proof). On 2026-11-10 `lake build` of
`lean/` passed with Lean 4.20.0 and the crate's library compiled under `blaze check`; its
tests have not run, so the checked-in models are predictions of what the translator emits.
Where the plan above left a choice, this is the choice.

**Structured MIR** (`structured.rs`, `body.rs`; `hir_ty_lean::structured(db, func)`).
- Over built MIR (`mir_body`), after type checking and borrow checking accept the function.
  It is in its own crate and not in hir-ty-mir: it is a reading of MIR for back ends, not a
  pass, and hir-ty-mir need not build it. It is computed from memoized queries and is not yet
  a memoized query itself.
- Control flow is reconstructed by Ramsey's algorithm ("Beyond Relooper") over the dominator
  tree: `Branch`, `Loop`, `Block`, `Break`, `Continue`, `Return`. The code a loop exits to is
  put after the loop. An irreducible graph is refused, naming the edge; nothing is duplicated.
- Unwind edges are not followed and cleanup blocks are never reached: a panic is `fail`.
  Storage markers, fake reads and drops go (a type with a destructor is refused).
- Built MIR already has a checked operation in one statement (`CheckedBinaryOp`) and an index
  in its place; runtime MIR's spread forms (`AddWithOverflow`, `Assert`, the read of `.0`;
  `Len`, `Lt`, `Assert` before an index) are folded back to the same.
- A `&mut` temporary assigned once is replaced by the place it borrows.
- Its refinement test lowers random structured programs to graphs, reconstructs them and runs
  all three (`structured_code_runs_as_its_graph`, with two planted bugs).

**The translation** (`translate.rs`).
- Integers are `Blaze.Scalar ty`: an `Int` with its range, as in Aeneas's first Lean library,
  so `omega` proves arithmetic facts for signed and unsigned alike and nothing trusts native
  evaluation. `usize` is 64 bits; v0 translates for 64-bit little-endian targets.
- A local is a Lean variable rebound at each assignment; a write of a field, a tuple field or
  an index rebinds the local to an updated copy.
- `fn f(x: &mut T) -> R` is `f (x : T) : R × T` (`T` alone when `R` is `()`): one function,
  the backward function merged into it. A function that returns a `&mut` is refused.
- A branch or block nothing jumps out of is an expression of the locals it changes that are
  live after it; one with a `return` or an outer `break` takes what follows into its arms.
- **Loops: fuel.** A loop is `f.loop_N (fuel : Nat) (live locals)`, recursive on `fuel`
  (structural, so Lean needs no termination proof), `Result.div` at zero. A function with a
  loop, or calling one, takes `fuel` first. This form is emitted for every loop: neither
  `partial_fixpoint` nor a `decreases` measure is used yet. A jump from a loop to a loop
  around it other than a plain `break` is refused (the two functions would be mutually
  recursive).
- **Plain function or `Result`.** A model is in `Result` when a fallible operation is left in
  its body: checked arithmetic (`+ - * << >>` under `overflow-checks`, read from the crate's
  cfg as MIR building reads it; `/`, `%` and `-` always), an index, an assert, a panic, a
  loop, or a call of a model in `Result`. With none, it is a plain Lean function. This is
  decided from the body, not yet from the effects walk's `nopanic`.
- Calls: a non-generic free function with a body is translated into its own module, which
  the caller's module imports. Core's `wrapping_add/sub/mul`, `saturating_add/sub` on
  integers and `len` on slices have models in the library. Any other call into the standard
  library is refused (`NoModel`).

**Refused** (`NotTranslated { function, what, at }`): raw pointers, `unsafe fn`, statics,
inline assembly, extern functions, `catch_unwind`, closures, function pointers, `async` and
coroutines, trait objects, floats, `char`, `str`, `Box`, unions, generic functions and types,
methods and trait calls, a type with a destructor, a `&mut` inside another type or returned or
chosen at run time, slice patterns, unsizing and pointer casts, recursion, tail calls,
`for const`, irreducible control flow, a field of an enum's variant read outside its `match`
arm. `unsafe` blocks are refused through what they do (a raw pointer, a static, an extern
call), not yet by themselves.

**Specs** (`spec.rs`). `requires` and `ensures` (with a named return) over literals,
parameters, the result, `!`, `-`, `+ - *`, comparisons, `&&`, `||`, a field and a slice's
`len()`. Integers are their `.val : Int`. `forall`, `==>`, `old` and `spec fn` are not in the
grammar on main; a loop's `invariant` is not lowered (a Lean proof brings its induction);
`decreases`, `recommends` and a loop's `ensures` are refused by name.

**Emission** (`hir_ty_lean::emit`). One module per function, `BlazeModel.<krate>.<path>`:
its types, its loops, the model, and `def <fn>.spec.statement : Prop`: for all arguments (and
all fuel) under the `requires`, the model does not fail and its result meets the `ensures`;
out of fuel is allowed, so a function with a loop gets partial correctness.

**Where things live.**
- The library: `lean/Blaze` (`Result`, `Scalar`, `Slice`, `Std`), core Lean only, no Mathlib.
  It is built into blaze (`hir_ty_lean::LIBRARY`), so a proof key covers its text. It is
  blaze's own, written for this translation, not a fork of Aeneas's library.
- Generated modules: a directory blaze owns (`Config::out`), never the source tree. The
  copies under `lean/BlazeModel` are test expectations: the golden tests hold each to what
  the translator emits, byte for byte, so `lake build` in `lean/` type-checks the
  translator's output and the proofs over it without blaze in the loop.
- Proofs: `lean/` beside the crate's `src/`, one file per Rust module, each path segment in
  upper camel case (`blaze_graph::ledger` is `crates/blaze-graph/lean/BlazeGraph/Ledger.lean`,
  the crate's root `<Crate>/Lib.lean`). A proof file imports the model and gives
  `theorem <fn>.spec.proof : <fn>.spec.statement`. It never restates the statement, so there
  is nothing to re-bind: blaze's check module states
  `theorem <fn>.spec : <fn>.spec.statement := <fn>.spec.proof`, which fails to elaborate, naming
  the theorem, when the proof is of anything else.

**The check** (`hir_ty_lean::check`, `proved`). The pinned Lean is 4.35.0
(`lean/lean-toolchain`), named by `BLAZE_LEAN_PROVER` (an absolute path to `lean`; no `PATH`,
no elan; `BLAZE_LEAN` was taken by the code generator), and refused when `--version` says
another. Proved means Lean accepts the check module and `#print axioms` lists only `propext`,
`Classical.choice` and `Quot.sound`: a `sorry` (`sorryAx`) and native evaluation
(`Lean.ofReduceBool`) are refused by name. The proof key covers the Lean version, the
translator's version, the allowed axioms, the library, every model the proof can see, the
check module and the proof file. Lean is not yet run in blaze's sandbox, verdicts are not yet
persisted, and `blaze check --verify` does not exist: the entry is the function.

**The first proof** (objective V5). `blaze_graph::ledger::fnv1a_from`, the checksum of the
memory ledger's file. blaze's own crates do not turn `blaze_verify` on yet, so its `ensures`
is on a test fixture that reads the function's text from the tree. The proof file
`crates/blaze-graph/lean/BlazeGraph/Ledger.lean` states and proves: the loop never fails (no
index out of bounds, no counter overflow), what it returns is FNV-1a's fold of the seed over
the bytes, it returns with more fuel than bytes, and the generated statement follows. The
test `fnv1a_from_is_the_fold_of_its_bytes` states the invariant as proved.
`harness/v2/lean_differential.py` runs the compiled model against the function blaze built.

**How to run it.** The Rust tests: `blaze test -p hir-ty-lean` (one needs
`BLAZE_LEAN_PROVER`, and `mir_semantics_corpus` is run by `harness/v2/mir_semantics.py`). The Lean: `lake build` in `lean/` with the pinned toolchain builds the
library, every golden model and the proofs. After a deliberate change to the translator,
`UPDATE_EXPECT=1` rewrites the goldens, and `lake build` must pass again.

## The semantics of MIR (2026-11-10)

Rule `verified-compiler` needs one thing every proof and every pass is held to: what a MIR
body means. It is `lean/Blaze/Mir`, a deep embedding of blaze's MIR with an executable
semantics, for a first subset. `lake build` of `lean/` passes with Lean 4.20.0 up to commit cc0f8f3ee7 (2026-11-11), and
the gate (below) holds every theorem of `lean/theorems.json`; what was written after a
checked commit says so where it is described. The Rust tests of `crates/hir-ty-lean` have
not run, so every embedded body is a prediction of the encoder's output. Objectives V6, V7 and V8 decide it.

**Prior art, and the one lesson taken from each.**
- [MiniRust](https://github.com/minirust/minirust): a specification of Rust's operational
  semantics is best written as an interpreter, so it can be run against the implementation.
  `step` and `run` are total Lean functions, and V6 runs them against the evaluator.
- [a-mir-formality](https://github.com/rust-lang/a-mir-formality): a formal model stays true
  only while it is tested against the compiler it models. The embedded bodies are goldens of
  the encoder, and the differential harness is in the ledger.
- [CompCert](https://compcert.org/): a pass is proved by a forward simulation, and
  correctness is stated as "every behaviour of the target is a behaviour of the source".
  `Outcome.Refines` is that statement; `run_mapBlocks` is the simulation for block-local
  passes.
- Translation validation (Pnueli, Siegel, Singerman 1998; Necula 2000) and
  [Alive2](https://github.com/AliveToolkit/alive2): a pass too large to prove is checked on
  each run, by a checker that is itself small. V8's second half does this.
- [Aeneas](https://github.com/AeneasVerif/aeneas): the functional translation is trusted
  there (its ICFP 2024 proof is of the symbolic semantics against a borrow checker, not of the
  emitted model). V7 proves each emitted model against the semantics instead.
- [CakeML](https://cakeml.org/) and [seL4](https://sel4.systems/): a verified stack is a
  chain of refinements, each between two stated semantics, and what is not in the chain is
  listed. "The trusted base" below is that list.

**Syntax** (`Syntax.lean`). Types (`unit`, `bool`, integers by `ScalarTy`, references, slices,
arrays, and `adt`: structs, enums and tuples by their declared layout, a list of variants
with a discriminant and field types), places (a local and projections: `deref`, `field`,
`downcast`, `index`, `constIndex`), operands (`copy`, `move`, `const`), rvalues (`use`,
`binary`, `checked`, `withOverflow`, `unary`, `cast`, `aggregate`, `replicate`,
`discriminant`, `ref`, `len`), statements (`assign`, `storageLive`, `storageDead`, `nop`),
terminators (`goto`, `switchInt`, `call`, `ret`, `assert`, `drop`, `unreachable`), bodies and
programs (functions by index). blaze's MIR has no `SetDiscriminant`, so neither does this.
Core's panic entry points, `wrapping_*`, `saturating_*`, slice `len` and constructor
functions are intrinsics, as the functional models have them.

**Values and memory: structured places, not bytes** (`Value.lean`, `State.lean`). A local is a
slot: `uninit`, `moved` or a structured value (`unit`, `bool`, `int`, `adt`, `array`, `ref`).
A reference is a pointer: a frame, a local of it and a path of fields and indices. Frame 0
holds what the entry function's reference arguments point to.
- **Why.** It is what safe code that borrows locals and their parts needs, it is the model the
  functional translation reads a `&mut` in, and every definition reduces in Lean's kernel, so
  tests and translation validation are `rfl`.
- **Frames are named by an id from a counter**, never by depth, so a reference into a frame
  that returned is `dangling` for the rest of the run. A move out of a part of a local leaves
  a hole (`Value.dead`) there, and a read of the hole is undefined; a value with a hole
  inside is still read whole, and the hole is undefined when it is used.
- **What it cannot express:** raw pointers, pointer-integer casts, transmutes, unions,
  anything that depends on layout (padding, niches, alignment), the heap (`Box`, `Vec`),
  `static`s and interior mutability. Storage markers do nothing, so a use of a dead local is
  not detected.
- **Aliasing is not modelled.** Two live `&mut` to one place are two pointers to it: a write
  through one is read through the other, where Rust says undefined behaviour (Stacked or
  Tree Borrows). No program the borrow checker accepts does this, and the encoder embeds
  only accepted programs, so every theorem about a Rust program has "borrow-checked" as its
  hypothesis; a pass that needs `noalias` to be correct cannot be proved here yet.
- **How it grows.** Heap cells addressed by an allocation id, for `Box` and `Vec`. Then bytes
  with provenance, as MiniRust has, with a stated relation between the two models: a
  structured value is related to the bytes its type's layout encodes it as. That relation is
  also what ties this semantics to LIR's ("Two semantics, one lowering").

**Semantics** (`Step.lean`, `Run.lean`). `step : Program → State → Outcome Next` runs the
running frame's block: its statements, then its terminator. `run prog fuel state` steps until
the entry function returns. Both are total (`def`, structural recursion), with no `partial`
and no axiom.
- **Outcomes:** `ok value`, `panic kind` (`Blaze.Error`'s kinds: `panic`, `arithmetic`,
  `bounds`), `undefined why` (`uninit`, `moved`, `dangling`, `illTyped`, `illFormed`,
  `unreachable`), `diverge` (out of fuel).
- **Against `Blaze.Result`.** `Result` is `Outcome` without `undefined`:
  `Outcome.ofResult` maps `ok`, `fail`, `div` to `ok`, `panic`, `diverge`, and
  `Outcome.toResult?` is its inverse, `none` exactly on `undefined`. The functional models
  assume a borrow-checked safe program has no undefined behaviour; a V7 theorem discharges
  the assumption for its function, because it shows the run is an `ofResult`. Two differences
  to know: the models read MIR's `Unreachable` as `fail unreachable`, the semantics as
  undefined; a model counts loop turns as fuel, the semantics counts blocks.
- **Integers** are `Blaze.Scalar`'s, the models' own: checked operations panic where
  `Scalar.add` fails, `BinaryOp` wraps, casts are `Scalar.cast`. One home.
- **Choices where MIR is open.** A `move` of a whole local leaves it `moved` (the evaluator
  and Miri leave it readable; this semantics has more undefined behaviour there, never less).
  An index out of bounds panics, as built MIR's index does; runtime MIR asserts first, so the
  two agree. The rvalue is evaluated before the place it is assigned to.
- **Proved** (`Run.lean`): `step_deterministic` (a state has one next outcome: `step` is a
  function), `run_mono` (a run that ends, ends the same with any more fuel) and `run_agree`
  (two runs of one state that both end agree). `State.lean`: the frame lemma
  `State.read_write_other` (a write at a place leaves the read of every place in another
  slot as it was); two places in one slot are not covered.

**V6: the differential harness.** `harness/v2/mir_semantics.py differential` draws programs
(arithmetic with overflow, a struct through a `&mut` parameter, an enum built and matched, a
loop, an array index, an explicit panic with an early return and a division, shifts) and gets
three answers per call: the Lean semantics on the body `hir_ty_lean::embed_mir` writes
(`crates/hir-ty-lean/src/body/embed.rs`), blaze's MIR evaluator (`interpret_mir`), and the
executable blaze builds. They must agree on the value or the kind of panic. The program is
Lean source, the encoder's one output: the runner (`Blaze/Mir/Runner.lean`) has the programs
compiled in and reads calls, so no second form of a body is parsed. The Rust half is the
test `mir_semantics_corpus`, the smallest hook that reaches the encoder and the evaluator
from a script; there is no `blaze debug` command for either.

**V7: a model is tied to the semantics.** The chain for `fnv1a_from`:

```text
MIR of crates/blaze-graph/src/ledger.rs
  = (golden test fnv1a_from_is_embedded_as_the_body_its_proof_walks)
lean/BlazeMir/blaze_graph/ledger/fnv1a_from.lean       the embedded body
  = (theorem BlazeGraph.LedgerMir.fnv1a_from_model_agrees)
lean/BlazeModel/blaze_graph/ledger/fnv1a_from.lean     the functional model
  = (theorem BlazeGraph.Ledger.fnv1a_from_returns)
FNV-1a's fold of the seed over the bytes
```

The middle link says: for every seed, slice and `extra`, `run` with `4 * len + 4 + extra`
steps is `Outcome.ofResult` of the model run with `len + 1` turns; with less fuel the run is
`diverge` or the same value (`fnv1a_from_any_fuel`). The embedded body is a prediction of the
encoder's output until the golden test runs; when it differs, the file is rewritten and the
block theorems of `LedgerMir.lean` (one per block, each failing alone) are redone.

Three more functions are tied the same way (checked by Lean and held by the gate), each a file beside its
model: `arith::add_one` (`BlazeModel/arith/add_one_agrees.lean`: the value, and the overflow
panic), `early::clamp` (both branches) and `loops::count` (through `run_loop`, the one
lemma for loops: an invariant and a variant give the loop's result). The other four goldens
(`structs::bump`, `enums::side`, `slices::first`, `calls::quadruple`) have none, and the
Rust emitter that prints these scripts is not written.

How it scales: **the translator emits the proof with the model**, a script that follows the
translation's structure and applies one lemma per construct (`Blaze/Mir/Lemmas.lean`, the
first step, used by the `fnv1a_from` proof): an assignment is `execStmt_assign`; a copy or
move `evalOperand_copy`/`_move`; a binary rvalue `evalTwo_ok` with the operator's `Scalar`
fact (`binaryOp_checked_add`, `binaryOp_same`); an index `inBounds_some`; a block
`step_block`; a branch a case split on the tested value; a loop an induction on the model's
fuel, with the live locals as the invariant's state; a call the callee's theorem. The other
route, a checker `agree : Body → Model → Bool` proved sound once and evaluated per function,
was not taken: a model is a Lean function, not data, so the checker would need a second deep
embedding of the model language and its own semantics, and the evaluation of a loop's
agreement does not terminate by `rfl`. `native_decide` is not allowed either way.

**V8: the first proved transformations** (`Blaze/Mir/Opt`). A pass is a Lean function on
programs; it is the specification of the Rust pass of the same name.
- **Refinement.** `Outcome.Refines target source`: the source is undefined, or the two are
  equal. Behaviours of the target are behaviours of the source.
- **`run_mapBlocks`** (`Opt/Local.lean`). A rewrite that leaves each block's meaning as it was
  from every state preserves every run, with the same fuel. Proved with it:
  `removeStorageMarkers` (true in this semantics because a marker does nothing in it),
  `simplifyConstCondition` (the direct form: a `switchInt` or an `assert` on a constant),
  `removeNops`, and `constFold` (a binary, checked, unary operation or a cast on constants is
  its value; one that panics is left, so it still panics). blaze has no Rust pass that only
  folds constants or only removes `nop`s, so those two have no pair to check.
- **`Sim`** (`Opt/Sim.lean`). A lockstep simulation through a map on states, under an
  invariant, gives equal runs (`Sim.run`). Its clients are the two renumbering passes below.
- **The CFG passes: checked by Lean** (`lake build` of `lean/` at 4357e681bf, 2026-11-11;
  what was written after that commit is marked).
  - **Control is not memory** (`Opt/Ctl.lean`). Every function of `Step.lean` up to
    `execStmt` leaves each frame's function, block and return target as they were
    (`execStmt_ctl`). Each proof below that depends on where control is rests on it.
  - **The commutation lemmas** (`Opt/Rename.lean`). `State.rename ρ` renames each frame's
    block by its function's map and its return target by its caller's. Every function of
    `Step.lean`, in the file's order, commutes with it, up to `execTerm_rename` and
    `step_rename`. `Opt/Bind.lean` has the four facts about `Outcome.bind` they are applied
    through, as terms: no proof there rewrites.
  - **A permutation of blocks** (`Opt/Renumber.lean`, `renumber orders`). The orders are
    checked by `checkPermutation` (every block listed, the entry block first), which the
    kernel evaluates; `permuteBlocks_correct` then needs no invariant, and
    `permuteBlocks_call` says every call gives the same outcome.
  - **Dead-block removal** (`removeDeadBlocks`). The invariant is stated (`Opt/Reach.lean`,
    `Reaches`): the running frame is at a kept block and every frame returns to one; it is
    kept by every step when the kept blocks are closed under jumps (`step_reaches`). The pass
    computes the blocks block 0 reaches and `checkDead` checks they are closed;
    `removeDeadBlocks_correct` is under the invariant, `removeDeadBlocks_call` for every
    call, where it holds of the start state.
  - **Goto threading** (`Opt/Thread.lean`, `threadGotos`), by the stuttering form of `Sim`
    (`Stutter`): one step of the target is `k + 1` steps of the source, `k` below a bound
    `K`. The fuel is stated: a target run of `n` steps that ends is the source's run of
    `K * n` (`Stutter.run_target`), and a source run of `n` steps that ends is the target's
    run of `n` (`Stutter.run_source`). For threading `K` is 2. Whole chains are the pass
    again: `threadGotosN k` threads chains of up to `k` empty blocks, with the bounds
    composed, `2 ^ k * n` one way and `n` the other (`threadGotosN_target`,
    `threadGotosN_source`; checked at e15b208b49).
  - **A `switchInt` whose arms all agree is a `goto`** (`Opt/SwitchSame.lean`,
    `switchToGoto`; checked at 6a85685c71). A refinement, not an equality: the `goto` drops the
    evaluation of the discriminant, which here can be undefined, so `switchToGoto_refines`
    says the rewritten run is the program's unless the program's is undefined. The
    discriminant must be a `copy` of a place with no index (a `move` changes the state, an
    index can panic); the Rust `simplify_branch` rewrites those too. The file has what
    every such pass uses: `run_refines` (steps that refine give runs that refine) and
    `Outcome.Calm` (a value or undefined, never a panic), proved of reads and plain places.
  - **Where these cover less than the Rust `SimplifyCfg`** (`optimize/cfg.rs`): it merges a block that ends in a call too, and rewrites a `switchInt` whose
    discriminant is a `move` or has an index; and
    `remove_dead_blocks` merges every empty `unreachable` block into one. None of those is
    specified, so `BlazeValidated` has no `after = spec before` pair for a CFG pass, and
    `validate`'s normal form does not include them: `validate_sound` is equality of runs
    with the same fuel from the same state, which a renumbering (the state is renamed) and
    threading (the fuel differs) do not have. The witness validator is where they join.
  - **Merging a block into its predecessor** (`Opt/Merge.lean`, `mergeGotos pick`; checked
    at 2ebcfbbab5). A block that ends in `goto t` gets `t`'s statements and terminator. It is
    right whatever jumps to `t`, so it is proved for every choice of targets; `mergeSuccessors`
    chooses as the Rust `merge_successor` does (one predecessor). A stuttering simulation
    through the identity with bound 2 (`mergeGotos_target`, `mergeGotos_source`). It rests on
    `execBlockFrom_top`: a block that does not end in a call does the same whatever block
    the running frame is at; `Opt/MemMap.lean` has the commutation lemmas of `Opt/Rename.lean`
    for any map on the stack that leaves memory alone, which that needs. A target that ends
    in a call is not merged (the caller's frame would keep a stale `block` under the
    callee, so the two states would differ): there the specification covers less.
  - **Threading through every target, a call's return target too** (`Opt/ThreadCalls.lean`,
    `threadAll`; checked at 9f791ac218). A frame's `target` is then another block in the
    threaded program, so states are mapped: `retargetStack` sends each frame's `target`
    through its caller's map and leaves every `block`; it is a `MemMap`. A terminator from
    the mapped state gives the mapped next state, or the mapped jump one hop further, and
    that hop is the program's second step: a stuttering simulation through the map with
    bound 2 (`threadAll_target`, `threadAll_source`, `threadAll_call_source`).
  - **Not written: merging a target that ends in a call.** The caller's frame keeps a stale
    `block` under the callee; the map is `block` renamed on the frames under the running
    one, another `MemMap`.
  - **`SimplifyCfg` composed** (`Opt/SimplifyCfg.lean`, `simplifyCfg k`; checked at
    167c97fb61): `nop`s removed, `k` rounds of threading every target, the `switchInt` rule,
    dead blocks, merging, then dead blocks, in the order of the Rust loop. Proved of it
    (`simplifyCfg_call`): a call that ends, and not in undefined behaviour, gives the same
    outcome with the same fuel, which is what an equality, a refinement and two stuttering
    simulations have in common. It is not yet the Rust pass block for block (that changes
    blocks in place with predecessor counts kept as it goes, merges a target that ends in a
    call, rewrites a `switchInt` on a `move`, merges empty `unreachable` blocks), so
    `after = spec before` is to be tried per body, and no pair is in `BlazeValidated` yet.
  - **Dead-store elimination: first piece** (`Opt/Ptr.lean`; checked at eadd58c04c).
    `MemOk P st` says every pointer in every slot of the state satisfies `P`; a read gives a
    value whose pointers do (`State.read_ok`) and a write of such a value keeps it
    (`State.write_ok`).
  - **Dead-store elimination: second piece** (`Opt/Ids.lean`, `Opt/Borrow.lean`; checked at
    b248b3e485). `PtrOk B ids p`: the pointer names frame 0, a frame that is gone, or a
    local its frame's function may borrow (`B`); `StateOk B st`: every pointer in memory is
    such. Writes and moves keep it, and a place whose base may be borrowed, or that goes
    through a dereference, evaluates to such a pointer (`evalPlace_ptr`). A frame's id and
    function are left by statements (`execStmt_ids`, `Opt/Ctl.lean`'s proofs for that
    pair).
  - **Dead-store elimination: third piece** (`Opt/BorrowExec.lean`; checked at fc96514550). An
    operator gives a value with no pointer in it, an operand a value read from memory, a
    `ref` the pointer its place evaluates to; so a statement keeps the invariant
    (`execStmt_okAt`) when every `ref` in it is of a place whose base may be borrowed or
    that goes through a dereference (`StmtFine`: the Rust pass's "ever borrowed" as a
    condition on the body).
  - **Dead-store elimination: fourth piece** (`Opt/NextId.lean`, `Opt/BorrowStep.lean`;
    checked at dde643da71). `Borrows B g st`: `g` lists every frame the run has made, those that
    returned too, with ids below the next one, and memory holds only pointers to frame 0 or
    to a local that the function of its frame may borrow. A call adds its frame to `g`, a
    return leaves it. `step_borrows`: every step keeps it, for a program whose every `ref`
    is of a place that may be borrowed or goes through a dereference (`ProgFine`).
  - **Dead-store elimination: fifth piece, first part** (`Opt/SlotAlgebra.lean`; checked
    at 9d978075d2). The two programs' states are the same but in one slot, which is stated
    with a map (the one is the other with that slot replaced), so the proof computes with
    replacements: two of different slots commute (`State.setSlot_comm`), a write elsewhere
    commutes with one (`State.write_setSlot_other`), and a write of the whole slot forgets
    it (`State.write_setSlot_same`).
  - **Dead-store elimination: fifth piece, second part** (`Opt/SlotExec.lean`; checked at
    9d978075d2). When a slot is the running frame's, of a local its function does not
    borrow, a statement that mentions the local neither as a place's base nor as an index
    does the same whatever the slot holds (`execStmt_put`), through every function of
    `Step.lean` up to `execStmt`.
  - **`removeOverwritten`** (`Opt/RemoveOverwritten.lean`; checked at 9aabce8584): the end
    theorem of the pieces above. In a block, `l = rv1; ..; l = rv2` becomes
    `nop; ..; l = rv2` when `l` is a whole local its function never borrows, `rv1` is a copy
    of a place with no index or a constant, and nothing between mentions `l`. A step runs a
    whole block, so the two programs' states are equal at every step:
    `removeOverwritten_refines` is `run_refines_inv` (a refinement of runs under an invariant
    the source's steps keep) with the block lemma, from every state the borrow invariant
    holds of, for a program `checkBorrows` accepts (evaluated by the kernel, proved sound);
    `removeOverwritten_call` is the same for every call whose base slots and arguments have
    pointers into frame 0 only, where the start state holds the invariant.
  - **Liveness across blocks is the next entry**, `removeDead`: this pass needs none. No
    `BlazeValidated` pair for DSE yet.
- **Global dead-store elimination (ledger V11): proved for `removeDead`.** One relation serves
  the Rust `DeadStoreElimination`, the deleting half of `CopyProp` (a use rewritten is
  `symEq`; the copy left unused is a dead store) and of `SimplifyLocals` (which also
  renumbers locals: a rename of locals, a commutation family like `Opt/Rename.lean`'s, not
  part of this).
  - **No change to the semantics.** `step` runs a whole block, and that grain is right: the
    relation is stated at block boundaries, and inside a block the proof is an induction
    over the statements, as `Opt/RemoveOverwritten.lean`'s is. A per-statement small step is
    not needed.
  - **The certificate** is liveness as data: per function and block, the locals live on
    entry. `checkLiveness` (a `Bool` the kernel evaluates) recomputes each block backwards
    from the union of its successors' sets (a `ret` reads local 0; a call's destination is
    written whole or in part; every borrowed local is always live) and accepts when what it
    computes is within the set given. The pass removes `l = rv` where `l` is a whole local
    not live after the statement and never borrowed, and `rv` cannot panic or move
    (`rvalueCalm`, to be widened to total operators).
  - **The relation** is a map, as in `Opt/SlotAlgebra.lean`: the source state is the target
    state with a list of slots replaced, `(frame id, local, slot)`, each a local dead at its
    frame's resume point (the running frame: the block it is at; a frame under it: after
    its call returns and writes the destination) and never borrowed, so by `step_borrows` no
    pointer is into it. A step keeps it: a statement that does not read a replaced slot
    commutes with the replacements (`execStmt_put`, generalized to a frame that is not the
    running one, where no place of the statement can be in it); a store to a replaced local
    drops it from the list (`State.write_setSlot_same`); a removed store adds one; a call
    leaves the caller's list as it is and starts the callee with none; a return drops the
    callee's.
  - **Built, and checked by Lean** (`lake build` of `lean/` at c70276a7d8, 2026-11-11; the
    `Rewrite` in `Opt/Witness.lean` was written after that build).
    (a) `Opt/SlotExecFar.lean`, `execStmt_putFar`. (b) `Opt/Puts.lean`: `puts st reps`
    (`Rep := Nat × Nat × Slot`), `execStmt_puts`, `execStmt_absorb` (`keepReps`),
    `puts_append`. (c) `Opt/Liveness.lean`: the certificate, `checkLiveness`,
    `stmtLive_dead`, the pass `removeDead`. (d) `Opt/Skel.lean`: the frames but for what
    their locals hold (`State.skel`; from `Opt/Ctl.lean` by substitution), which statements
    keep; the relation speaks of each frame's `dest`, which `ids` and `ctl` lack.
    `Opt/PutsTerm.lean`: operands, places and a call's destination through the list
    (`evalOperand_puts`, `evalPlace_puts`, `evalDest_puts`), then `jump_puts`, `write_puts`
    (an entry whose whole slot is written leaves: `dropAt`), `enter_puts`,
    `Intrinsic.eval_same`, `callOn_puts`, `ret_puts`. `Opt/RemoveDead.lean`: the relation
    and `block_rel`, the induction over a block's statements with the certificate's points.
    (e) `run_refines_rel` (`Opt/RemoveOverwritten.lean`), `removeDead_step`, and:

    ```lean
    theorem removeDead_refines (cert : List (List (List (List Local))))
        (borrowed : Body → Local → Bool) (prog : Program)
        (hlive : checkLiveness prog cert = true) (hb : checkBorrows borrowed prog = true)
        (fuel : Nat) (st : State) (hinv : ∃ g, Borrows (BorrowedIn borrowed prog) g st)
        (hord : Ordered st.skel) :
        (run (removeDead cert borrowed prog) fuel st).Refines (run prog fuel st)
    ```

    `removeDead_call` is the same for every call whose base slots and arguments have
    pointers into frame 0 only (a start state is `Ordered`: `start_ordered`).
    `Rewrite.removeDead cert` makes it a rewrite of the witness validator
    (`removeDeadStoresChecked_preserves`, `PreservesWhen FrameZero`).
  - **The relation as built** (`Rel` in `Opt/RemoveDead.lean`), where it differs from the
    design above. The source state is `puts t reps`; of the target state `t`, `Borrows B g t`
    and `Ordered t.skel`: the frames' ids strictly decrease down the stack and each frame's
    `dest.frame` is below its id. Both are needed and were not foreseen: an entry of a frame
    that returned stays in the list (`Placed .. [] = True`) and replaces nothing only
    because no frame under it has its id; and a removed store's new entry is off every
    frame's destination only because those are in older frames. Each entry is `Good`: its
    slot holds an allowed value; its frame's function never borrows its local; every
    frame's `dest` is its whole slot or off it; and it is dead where its frame resumes
    (`Placed`, `Below`): on entry to the running frame's block, or on entry to the callee's
    return target unless the callee's `dest` is its whole slot. Inside a block the running
    frame's entries are dead at the current point (`Here`). A removed store appends its
    entry (`puts_append`); the older entries of that local stay, still dead.
  - **What the Rust `DeadStoreElimination` does beyond `removeDead`, none proved.** A store
    of an rvalue that can panic or move (here only a copy of a place without an index, or a
    constant: `rvalueCalm`; a total operator on such operands is the next widening, and needs
    only `evalRvalue_calm` for it); a store through a projection of a dead local; `Deinit`;
    a copy into a call made a move; unused storage markers; liveness along unwind edges (the
    semantics has no unwinding). And the pass is not yet held to the specification: it must
    export its liveness as the certificate, and the pair in `BlazeValidated` be accepted by
    `validateWith` with `Rewrite.removeDead cert`.
  - **What `CopyProp` and `SimplifyLocals` need on top.** `CopyProp`: the uses rewritten are
    `symEq`'s, and the copy left unused is a dead store, so a script
    `[symEq step, Rewrite.removeDead cert]`; what is missing is `symEq` as a `Rewrite`
    (`validateSym` compares at the end only) and `removeDead` for a `move` operand, which
    `CopyProp` leaves when the source is dead after (the source then holds `moved` in one
    program and its value in the other: one more entry in the list, which `Rel` already
    allows, with `evalOperand` of a move shown to add it). `SimplifyLocals`: after
    `removeDead`, a renaming of locals, a commutation family like `Opt/Rename.lean`'s over
    `State` with each frame's `locals` permuted and pointers' `slot` mapped; and the
    removal of the locals no statement mentions, which changes `FnSig.locals` and so needs
    the relation between a frame and one with fewer slots.
  - **Idioms that elaborate here, and ones that do not** (Lean 4.20.0; learned on this
    branch, each from a build):
    - Prove through `Opt/Bind.lean`, as terms: `Outcome.bind_commutes hx (fun a ha => ..)`,
      `bind_commutes_map`, `bind_congr`, `bind_map_congr`, and `Outcome.all_bind` for a
      predicate on the value. The unifier unfolds a definition to its `bind`; no `rw`.
    - `congrTo f h` (in `Opt/Bind.lean`), never `congrArg f h`, where an expected type is
      known: `congrArg` lets the goal choose the right-hand side and picks wrongly when it
      only unfolds to the same.
    - A structure literal `{ a := .., b := .. }` broken across lines inside parentheses
      in a `have`'s type does not parse: write `State.mk .. .. ..` / `Frame.mk ..`.
    - `Preserves.comp (f := ..) (g := ..)` and the like: give the functions; higher-order
      unification of `fun prog => g (f prog)` picks the identity.
    - `nomatch h, fun ..` parses as a list: parenthesize `(fun ⟨_, h⟩ => nomatch h)`.
    - A hypothesis whose type is a `def` (`Reaches`, `RepOk`, `Redirects`): use
      `And.left`/`And.right`/`And.intro`/`Or.inl`, not `h.1` or `⟨..⟩`; before `cases` on
      it, restate it with its `∨` written out (`have h' : A ∨ B := h`), or take
      `And.right (And.right h)` whose type is then the `∨` itself.
    - A `match` on a value that is not a constructor does not reduce: give every function
      a helper that matches on its argument (`slotIn`, `switchOr`, `symNext`), and state
      `f_cons := rfl`; for an inline one, `simp only [f]` then `cases h : scrutinee`
      (`callOn`), or `unfold f; cases h : scrutinee` when the `match` is the body's head
      (`Program.start`); `unfold f; split <;> first | ..` for a many-pattern one
      (`Intrinsic.eval`).
    - `by omega` as an argument runs before its goal is known: put it in a
      `have h : <type> := by omega`.
    - `rw [e] at h` matches up to instances: `k + 1 + 0` also matches `k + 1`. Build
      arithmetic steps as terms (`congrArg (fun j => run prog j st) e`).
    - After `subst`, do not name the variable that may be gone; state the lemma for one
      variable and derive the other form with `congrTo`.
    - `cases e with | c => ..` needs `e` to appear in the goal as written: `show` the
      unfolded goal first, or `cases h : e` and `rw [h] at ..`.
    - `first | exact A | exact B | ..` after `cases x <;> cases y` chooses by unification;
      order the alternatives so that an earlier one cannot fit a later one's goal.
    - Generating a file by substitution from one that builds works and is cheap to get
      right: `Opt/MemMap.lean` from `Opt/Rename.lean`, `Opt/Ids.lean` and `Opt/NextId.lean`
      from `Opt/Ctl.lean`, `Opt/SlotExecFar.lean` from `Opt/SlotExec.lean`, the operator
      lemmas of `Opt/SymBlock.lean` from `Opt/BorrowExec.lean`. Assert that no old name is
      left, and read the result once.
    - In `decide` examples, state what the pass does by hand first; twice here the
      expectation was wrong and the pass right.
    - After `cases hs : e` on a goal with `match e with ..`, the branch's goal is
      `match some s with ..`, not reduced: `show` the reduced goal before a `rw` into it.
    - `rw [e] at h` rewrites `h` only: when the goal speaks of the same variable, rewrite
      the goal.
    - An intermediate equation gets a `have` with its type, not `(congrTo f a).trans
      (congrTo g b)`: the middle term is then known before either `by rfl` runs.
    - A function with a many-pattern match (`Intrinsic.eval`) is proved about by a theorem
      whose patterns split its arguments until every case is `rfl` (`Intrinsic.eval_same`).
    - To use a fact about `x.jump? target = Outcome.ok ..` with `target` a variable, a
      lemma by pattern matching (`jump?_ok`) that returns `target = some tb`, not `cases`.
- **A validator proved sound** (`Opt/Validate.lean`). `validate a b` accepts when the two
  programs have one normal form under the proved rewrites; `validate_sound`: an accepted
  pair runs alike for every fuel and state. It takes no witness and accepts only what the
  normal form identifies.
- **A validator that takes a witness** (`Opt/Witness.lean`; checked at 9aabce8584). The witness
  is a script: the proved rewrites, in order, that take the program before a pass to the
  program after it (`Rewrite`: the block-local passes, `switchToGoto`, both threadings,
  merging, dead-block removal, a block permutation with its orders as data).
  `validateWith a b w` replays the script on `a` and accepts when it gives `b`, which the
  kernel decides; `validateWith_sound`: an accepted pair gives every call that ends, and not
  in undefined behaviour, the same outcome with the same fuel. The Rust pass is then not
  trusted and need not be a fixed specification block for block: a script that reproduces
  its output is the evidence, and the pass can say which rewrites it applied.
  `removeOverwritten` and `removeDead cert` (global dead-store elimination, with its
  liveness certificate as the rewrite's data) are rewrites too, proved for the calls whose
  caller hands over pointers into frame 0 only (`FrameZero`): `validateWith_sound` is for
  scripts without them (`allPlain`), `validateWith_sound_frameZero` for any script and those
  calls.
  **The validator by symbolic execution, first piece** (`Opt/Sym.lean`; checked at
  9aabce8584): `Expr`, what a local holds as a function of the locals when its block began,
  with `Expr.eval`; a symbolic store, and `Reads` (the state agrees with it);
  `evalRvalue_sym`: an rvalue of the subset (`use`, `binary`, `checked`, `withOverflow`,
  `unary`, `cast` on constants and copies of whole locals: no move, no projection, no
  `ref`) evaluates to the value of its expression and leaves the state.
  **Second piece** (`Opt/SymBlock.lean`; checked at 9aabce8584): a block's statements as a
  trace, the locals assigned in order with their expressions (`symStmts`), and what a trace
  means (`den`); `execStmts_sym`: executing the statements is the meaning of their trace,
  so statement lists with one trace do the same from every state (`execStmts_same`).
  **Third piece** (`Opt/SymEq.lean`; checked at 9aabce8584): a terminator with a symbolic
  condition (`symTerm`), a block as its trace and symbolic terminator (`symBlock`,
  `execBlock_sym`), `symEq a b` (the same types and, block by block, blocks that are equal
  or have one symbolic form) with `symEq_sound` (they run alike, for every fuel and state),
  and `validateSym a b w`: a script of proved rewrites replayed on `a`, the result compared
  with `b` by `symEq` (`validateSym_sound`). The script is the block map; symbolic equality
  is the rest.
  **What symbolic equality accepts, and what it does not.** It accepts a pass that rewrites
  operands inside a block and keeps every store: copy and constant propagation of uses,
  on the scalar, move-free subset (blocks outside it must be equal). It does not accept a
  store or a local removed (the deleting half of the Rust `CopyProp`, `SimplifyLocals`):
  there the two programs' states differ on a dead local across steps, which is the relation
  `removeDead_refines` is proved with (ledger V11, above). The inliner needs a relation between one frame and two; loop transforms need a
  witness that is not per block.
- **The Rust passes are held to them by translation validation.** The test
  `the_passes_are_their_lean_specifications` and `mir_semantics.py validate` run each pass
  alone on runtime MIR, embed the body before and after, and Lean checks
  `example : after = spec before := rfl` (`lean/BlazeValidated`): a syntactic check the
  kernel decides. `RemoveStorageMarkers` does exactly its specification. The Rust
  `SimplifyConstCondition` also takes a condition that reads a place the statement before
  assigned a constant; there `after ≠ spec before` and the build says so.
**The gate holds the proofs** (`harness/v2/lean_gate.py`; written 2026-11-10, its first run
in CI pending). What is claimed proved is one list, `lean/theorems.json` (each theorem's name,
module, what it says and its ledger row), and [proofs.md](../proofs.md) is generated from it
with each statement as its module states it. The `lean` job of `blaze.yaml` runs the gate's
`check` on every change, with the Lean `lean/lean-toolchain` pins:
- `lake build` of `lean/` and of `lean/analysis` (after `lake exe cache get`), and neither log
  may say `declaration uses 'sorry'`;
- one generated Lean file per package runs `#print axioms` on each listed theorem: it must
  exist and rest on nothing but `propext`, `Classical.choice` and `Quot.sound` (so a `sorry`,
  `native_decide` or a new `axiom` under a listed theorem fails by name);
- without Lean (`sources`, in the `scripts` job): no Lean source says `sorry`, `admit`,
  `native_decide`, `decide +native`, `axiom`, `partial def` or `debug.skipKernelTC` outside a
  comment, except where the list's `allow` names the file and the word (one entry: the
  `partial def` that reads standard input in `lean/Blaze/Tests/Fnv1aMain.lean`); every listed
  theorem is stated in its module; `lean/analysis/lake-manifest.json` is checked in and pins
  Mathlib at `lakefile.toml`'s commit; proofs.md is current.

The gate does not check that a theorem's statement says what its one line in words says: the
statement is printed in proofs.md so a reader can.

**The trusted base** (what a `(proved: ..)` of rule 57 rests on, beyond what is proved):
- Lean's kernel, and the axioms `propext`, `Classical.choice`, `Quot.sound`.
- That `lean/Blaze/Mir` is what Rust means, on the subset. Reduced by V6's three-way run.
- The encoder (`body/embed.rs`): the blocks it keeps, false edges as `goto`, the intrinsics.
  Reduced by V6 and by the golden tests.
- The memory model's coverage: what it cannot express, above.
- `Blaze.Scalar` and the intrinsics as the meaning of core's integer operations.
- The functional translation, for every model without a V7 theorem (all but `fnv1a_from`;
  `add_one`, `clamp` and `count` leave the list when Lean has checked their proofs).
- blaze's front end up to MIR: parsing, name resolution, type and borrow checking, MIR
  building. A proof is about the MIR blaze built.
- Runtime lowering (`crates/hir-ty-mir/src/mir/transform`: `AddAsserts`, `ElaborateDrops`,
  cleanup, upvars, packed drops) and every MIR pass without a proof or a validation
  (`crates/hir-ty-mir/src/mir/optimize`): `CleanupPostBorrowck`, `SimplifyCfg`, `HeapToStack`,
  `BoxInPlace`, `CheckAlignment`, `CheckNull`, `CheckEnums`, `LowerSliceLenCalls`,
  `InstSimplify`, `Devirtualize`, `Inline`, `RemoveUnneededDrops`, `UnreachablePropagation`,
  `SimplifyLocals`, `DeadStoreElimination`, `SingleUseConsts`, `RemoveNoopLandingPads`,
  `CopyProp`, `CriticalCallEdges`, `Specialize`, monomorphization, and
  `SimplifyConstCondition` where it does more than its specification.
- MIR to Cranelift IR lowering in `crates/hir-ty-codegen` (today's path), with layout, ABI
  and unwinding.
- Below it: the stencils, LLVM, the linker, the loader and live patching, the OS, hardware.
- For the derivative theorems, what ["Derivatives"](#derivatives-2026-11-10) lists: Mathlib,
  the rule generator, the link from the Lean differentiator to the Rust one, and floats.

## Two semantics, one lowering

MIR's semantics, above, is the specification of Rust programs: typed, with places, panics
and undefined behaviour. LIR ([lir.md](lir.md), `crates/blaze-lir`, ledger L1) will get its
own, lower semantics: SSA with block parameters, machine types, bytes and pointers with
provenance, no Rust types. It is not written now: LIR has no producer yet (code generation
still builds Cranelift IR from MIR), and a specification nothing runs against rots.

The chain, once L1 is built:
1. **MIR to LIR is the one large lowering** (layout, ABI, drop elaboration, unwinding). It is
   held by translation validation per function, against both semantics (ledger V9).
2. **LIR passes** are proved or validated against LIR's semantics, as MIR passes are here.
3. **LIR to machine code** (the stencils, LLVM) stays in the trusted base.

The two memory models meet in one relation, which this semantics must keep statable: a
structured value is related to the bytes its type's layout encodes it as (and a `Pointer` to
an address with provenance for its allocation). Every choice in `Value.lean` is made so that
relation can be written: values carry no layout, and nothing in the semantics reads one.

## Derivatives (2026-11-10)

`crates/blaze-diff` takes symbolic derivatives by a table of rules and simplifies them by a
table of rewrites ([autodiff.md](../../../future/autodiff.md)). Both tables are proved, over
the reals, in a second Lake package, `lean/analysis`. Objective V10 is the row.

**What Lean has checked and what nothing has compiled.** `lake build` of `lean/analysis`
passes with Lean 4.20.0 at commit a36c7907ae: every theorem below is checked by Lean (their
axioms are printed and held by the gate, above). The Rust side of the same work has never been compiled: the
generated table's wiring into `diff::RULES` and `simplify::REWRITES`, the fix of `powi` at
`i32::MIN`, `eval::dual` and every new test are written and not built.

**Two packages.**
- `lean/` is unchanged: core Lean only, no dependency, as fast to build as before
  (`lake build` in `lean/`).
- `lean/analysis` (package `blazeAnalysis`, library `BlazeDiff`) is for proofs that need real
  analysis. Its `lean-toolchain` equals `lean/`'s. It requires Mathlib at commit
  `56a005855ea17e0b0958f73ec801ae140b0a1679`, which is what Mathlib's tag `v4.35.0` names (the
  Mathlib built for Lean 4.35.0), and it requires `lean/` by path, so it reuses `Blaze.*` and
  does not copy it: a `powi` exponent is `Blaze.I32`, Rust's `i32` as `Blaze.Scalar` models it.
  Build: in `lean/analysis`, `lake exe cache get` (Mathlib's prebuilt files; Mathlib is never
  compiled here), then `lake build`. `lake-manifest.json` is checked in, so what Mathlib itself
  needs is pinned too; `lake update` rewrites it, and is run only to move the pin.

**One source for the rules.** `crates/blaze-diff/rules.json` lists the operations, each
derivative rule (its tangent as a template, its kink, its domain, its IEEE condition) and each
rewrite (both sides, its class, and for a `fast_math` one the condition its theorem assumes).
`scripts/src/bin/diff_rules.rs` writes from it:
- `crates/blaze-diff/src/rules_gen.rs`, which `lib.rs` includes: `diff::RULES` and
  `simplify::REWRITES` are these tables, and the Rust types (`Rule`, `P`, `Rewrite`, `Pat`)
  are unchanged;
- `lean/analysis/BlazeDiff/Ops.lean` (`Op`), `Rules.lean` (`Rules.tangent`, `Rules.domain`,
  `Rules.kink`) and `Rewrites.lean` (one statement per `fast_math` rewrite).

Its `check` fails when any of the four is not what `rules.json` generates, and runs in the
gate. So the table blaze runs and the table the theorems are about cannot differ. Nothing is
hand-written on both sides; what stays hand-written once is the Rust `Op` enum (a test holds
it to the generated list), the meaning of each operation and of each `Domain` in Lean
(`Expr.lean`, `Template.lean`), and the Rust `Domain::distance`.

**The Lean side** (`lean/analysis/BlazeDiff`).
- `Expr` is a `Graph` node as a tree, over the generated `Op`. `Expr.eval env e : ℝ` and
  `Expr.holds env e : Prop` are its meaning at a point `env : ℕ → ℝ`, with Mathlib's total
  functions (`Real.log`, `Real.sqrt`, `/`, `Real.rpow`, `zpow`).
- `diff c x e` is `d(e)/d(variable x)` with `c` the value chosen at a kink. At each node it
  instantiates `Rules.tangent op`: no derivative is written in `Diff.lean`.
- `DiffAt env x e` is where the rules claim `e` differentiable: every node is inside its
  rule's `Rules.domain`, and a `select` (whose kink is at its condition) has a condition that
  does not change near the point and a differentiable taken arm.
- `Sound.lean` has one lemma per operation (`rule_sound_add`, ..., `rule_sound_select`), each
  one Mathlib lemma away. `Correct.lean` checks by unfolding that the generated template is
  the tangent each lemma is about, then proves by induction:

```lean
theorem diff_correct (c : ℝ) (env : ℕ → ℝ) (x : ℕ) (e : Expr) (h : DiffAt env x e) :
    HasDerivAt (fun t => e.eval (Function.update env x t)) ((diff c x e).eval env) (env x)

theorem gradient_correct (c : ℝ) (env : ℕ → ℝ) (vars : List ℕ) (e : Expr)
    (h : ∀ x ∈ vars, DiffAt env x e) :
    ∀ x ∈ vars,
      HasDerivAt (fun t => e.eval (Function.update env x t)) ((diff c x e).eval env) (env x)

theorem jacobian_correct (c : ℝ) (env : ℕ → ℝ) (vars : List ℕ) (roots : List Expr)
    (h : ∀ e ∈ roots, ∀ x ∈ vars, DiffAt env x e) :
    ∀ e ∈ roots, ∀ x ∈ vars,
      HasDerivAt (fun t => e.eval (Function.update env x t)) ((diff c x e).eval env) (env x)

theorem hessian_entry_correct (c : ℝ) (env : ℕ → ℝ) (x y : ℕ) (e : Expr)
    (h : DiffAt env y (diff c x e)) :
    HasDerivAt (fun t => (diff c x e).eval (Function.update env y t))
      ((diff c y (diff c x e)).eval env) (env y)
```

- **At a kink** (`Kink.lean`), where `diff_correct` says nothing, what
  `Choice::Subgradient(c)` gives is proved of the generated template: `abs` at 0 has tangent
  `t0 * c`, and a `c` in `[-1, 1]` is a subgradient; `min` and `max` at a tie have
  `c * t0 + (1 - c) * t1`, for `c` in `[0, 1]` a supergradient of `min` and a subgradient of
  `max`; `sqrt` at 0 has `c * t0`, a convention (there is no subgradient); `select` has its
  taken arm's.

```lean
theorem abs_choice_is_subgradient (c : ℝ) (h1 : -1 ≤ c) (h2 : c ≤ 1) (y : ℝ) :
    |(0 : ℝ)| + c * (y - 0) ≤ |y|

theorem max_choice_is_subgradient (c : ℝ) (h0 : 0 ≤ c) (h1 : c ≤ 1) (a u v : ℝ) :
    max a a + (c * (u - a) + (1 - c) * (v - a)) ≤ max u v

theorem min_choice_is_supergradient (c : ℝ) (h0 : 0 ≤ c) (h1 : c ≤ 1) (a u v : ℝ) :
    min u v ≤ min a a + (c * (u - a) + (1 - c) * (v - a))
```

- **Rewrites** (`RewriteProofs.lean`). Each of the 28 `fast_math` rewrites is a generated
  statement, for example

```lean
def mul_div_cancel : Prop :=
  ∀ (env : ℕ → ℝ) (a b : Expr),
    b.eval env ≠ (0 : ℝ) →
    (Expr.bin .div (Expr.bin .mul a b) b).eval env = a.eval env
```

  and `theorem rewrites_sound : Rewrite.AllSound` proves them all; `Rewrite.AllSound` is a
  generated structure with one field per rewrite, so a rewrite added without a proof does not
  build. For the 21 exact rewrites nothing is generated: they claim equal IEEE 754 bits, which
  is not a statement about the reals. The crate's tests hold them, on `f64` and `f32` bits
  (`each_exact_rewrite_keeps_every_bit_f32_sampled`, and the same over all 2^32 patterns
  under `--ignored`).

**What the proof found.** `powi`'s rule took the exponent less one by wrapping, so at
`i32::MIN` the derivative was `n * x^i32::MAX`. Stating the rule with the exponent a
`Blaze.I32`, not an unbounded integer, showed it; `PowiDown` is now the node over its argument
there, in Rust and in Lean.

**The Rust side is held to a second method.** `eval::dual` is forward-mode differentiation by
dual numbers over the same graph. It does not read `RULES`: each operation's derivative is
written out from its definition, on purpose, because an oracle must not share the thing it
checks. The refinement test `the_symbolic_derivative_is_the_dual_tangent` draws expressions
and points inside the domains and requires the symbolic derivative to equal the dual tangent
within `1e-10` of the sum of the tangent's terms, with central finite differences as a third
check and four planted bugs, one of them too small for finite differences to see.

**Trusted, beyond what is proved** (added to the list above):
- **Mathlib** at the pinned commit, with Lean's three axioms, which its real analysis uses.
- **The generator** (`scripts/src/bin/diff_rules.rs`): that the Rust table and the Lean table it writes say
  the same thing. Its selftest and the dual-number test are the evidence; it is not proved.
- **The link from the Lean `diff` to the Rust `Differentiator`.** The Lean one recurses over a
  term and builds every zero; the Rust one walks a hash-consed DAG once, never builds a
  structural zero or one, and asks a rule's domain only at nodes the variable reaches. That
  sharing and elision change no value is assumed, and tested by the dual-number refinement.
  One consequence is not covered: `powf` by a constant exponent with a base that is not
  positive, which Rust differentiates and `DiffAt` excludes.
- **Floats.** Every theorem is over `ℝ`. `Blaze` has no model of IEEE 754 and none is built
  here: Rust evaluates in `f32` or `f64`, where a derivative agrees with the real one up to
  rounding only while every value is finite, and a `fast_math` rewrite can change a result.
  NaN, infinities, signed zeros and rounding are covered by tests alone.
- **The simplifier's engine.** Each rewrite is proved; that the e-graph (union-find, rebuild,
  constant folding, extraction) applies only rewrites and keeps congruence is tested, not
  proved.
- **Lifting and lowering** (`crates/blaze-diff-mir`): that a `Graph` means what the MIR it
  was lifted from means.

## v1: interior mutability and unsafe, by deep embedding

**Scope.** Code that uses `UnsafeCell`, raw pointers, `unsafe` blocks, and the safe APIs built
on them.

**How.**
- The deep embedding of "The semantics of MIR", grown to a heap with permissions (then
  bytes), plus a separation logic over it.
- The logic follows [RustBelt](https://plv.mpi-sws.org/rustbelt/popl18/) and
  [Iris](https://iris-project.org/), built on
  [iris-lean](https://github.com/leanprover-community/iris-lean): its MoSeL proof mode, the
  base logic, and invariants are already ported. Porting status is tracked in its
  `PORTING.md`. RustBelt's lifetime logic is not ported, and porting it is v1's largest
  piece of work.
- [RefinedRust](https://dl.acm.org/doi/10.1145/3656422) (Iris in Rocq, safe and unsafe Rust,
  a verified `Vec` variant) is the closest design to follow.

**The two levels are linked by specs.** An unsafe module proves, in the deep embedding, that
its safe API meets a functional spec. v0 code that calls the module uses that spec as the
callee's model. This replaces the hand-written std model entry with a proved one wherever the
module is blaze's own code.

**Refused:** atomics (v2), FFI except through a modelled boundary, inline assembly, and
anything that depends on layout beyond what the layout query proves (until v4).

**Trust base:** v0's, plus the MIR semantics in Lean and its memory model, held to the MIR
evaluator and to built code by V6's differential runs.

**First target:** the bump arena behind `Vec::new_in(frame)` (`bump-allocate`). Every block it
hands out is in bounds, aligned, and disjoint from every other live block, and a reset frees
exactly the frame's blocks.

**Size:** the MIR semantics and the logic instance take 20 to 40k lines of Lean. The
lifetime-logic port is a separate effort of similar size.

**Risks:**
- iris-lean's coverage and performance.
- Whether the embedding's memory model matches the pinned nightly's (Tree Borrows versus
  Stacked Borrows is an open question in Rust itself). Proofs must target the rule set the
  pinned nightly's Miri uses, and change when that changes.

**CI:** as in v0.

## v2: atomics, sequentially consistent first, then weak memory

**Scope.** `core::sync::atomic` and fences.
- **Step one** gives atomics an SC semantics in v1's embedding.
- **Step two** moves to [RC11](https://plv.mpi-sws.org/scfix/) or the
  [promising semantics](https://dl.acm.org/doi/10.1145/3009837.3009850).
- **Prior art for the logic:** iRC11 from
  [RustBelt Meets Relaxed Memory](https://plv.mpi-sws.org/rustbelt/rbrlx/) (it found a data
  race in `Arc`), and [Cosmo](https://dl.acm.org/doi/10.1145/3408978) for views as an
  abstract type.

**First target: the flight recorder's ring move** (`crates/blaze-profile/src/flight.rs`,
`move_store` and `flushed`).
- **The invariant:** a store's `flushed` is never below a value that a `flushed()` call has
  published for that ring.
- **The bug it catches:** the 2026-11-04 bug breaks it. `copy_to` reads the old store's
  `flushed`, a concurrent `flushed()` stores a higher value into the old store, and the move
  then publishes the new store with the stale mark, so a postmortem writes rows twice.
- **Step one is enough to find it.** The bug is a lost update under SC already, so the SC
  model finds it.
- **Weak memory is needed for the fix.** Proving the fixed protocol with its actual
  `Relaxed`, `Release` and `Acquire` orderings needs step two.
- **The DST must catch it too.** The same bug is a planted case that the DST must catch
  under `hegel::sim`'s scheduled atomics. The proof and the DST pin the same invariant.

**Refused:** `SeqCst` mixed with weaker accesses to the same location until the RC11 SC
condition is modelled. Also refused: memory-model questions RC11 leaves open, such as
mixed-size accesses.

**Trust base:** v1's, plus the memory model. Each axiom is cited to the paper it comes from.

**Size:** 10 to 20k lines of Lean for the SC step. Porting the iRC11-style model is about the
size of v1.

**Risks:** relaxed-memory logics are hard to use, so a single proof can take far longer than
the code took to write. This is why it is only for small, stable rings, and why the DST stays.

**CI:** as in v0.

## v3: crashes on files and mmap

**Scope.** A crash-aware concurrent logic, in the style of
[Perennial](https://github.com/mit-pdos/perennial) (crash framing and logically atomic crash
specs; [GoJournal, OSDI 2021](https://www.usenix.org/conference/osdi21/presentation/chajed)).
It sits over the Lean model of the kernel that `hegel::sim` shares: what fsync and
`F_FULLFSYNC` make durable, renames made durable before their data, errseq, ENOSPC, and what a
crash keeps of a dirty mapped page. The code under proof is a sans-IO core on blaze-core's
effects, so the model's effects are the core's effects.

**First target: the profile history is exactly-once after any crash.**
- **The property:** every row that was recorded and flushed appears exactly once after any
  crash.
- **What the proof covers:** the commit protocol (manifest rename under `manifest.lock`,
  retire, pin) and the postmortem ring's `flushed` mark, together
  (`crates/blaze-profile/src/history/`, `flight/postmortem.rs`).
- **The crash classes it must rule out**, from the 2026-11-04 review and the testing skill's
  table:
  - no fsync before the manifest rename;
  - a crash between commit and consumption;
  - a flush that fails part way;
  - a hold lost in a crash.

  Each of these is also a planted bug that the DST catches.

**Refused:** multi-machine state, and file systems outside the model. A file system the
model doesn't describe is a typed refusal naming it, never an assumption that it behaves like
ext4.

**Trust base:** v2's, plus the kernel model. The model is held honest by the DST trace check
and real-kernel replay (see the decisions above).

**Size:** 15 to 30k lines of Lean, depending on how much of Perennial's Lean port can be
reused directly.

**Risks:**
- The kernel model is wrong in a way both the proof and the DST share. Real-kernel replay
  is the only defence.
- Perennial's Lean port is young.

**CI:** as in v0, plus a real-kernel replay job in ix VMs on the release train.

## v4: compiler correctness

**Scope.** blaze's front end is mostly safe Rust, so v0 to v1 can translate it. Targets, in
order:

1. **Layout and ABI.** A Lean spec of the pinned nightly's layout rules for `repr(Rust)`, `C`,
   `transparent` and niche filling, and a proof that blaze's layout query implements it.
   Every rule in the spec is cited to the rustc source at the pinned commit.
2. **Const eval.** blaze's MIR evaluator refines the MIR semantics (V6 tests it today).
3. **The translator itself.** Each emitted model is proved against the MIR semantics
   (objective V7, "The semantics of MIR"); a proof of the translator once and for all would
   replace the per-model proofs.
4. **Trait resolution.** Coherence, and determinism of selection. Full equivalence with
   rustc's solver is out of scope, because the solver itself is still changing upstream.

**Refused:** machine code generation and linking (the stencils and LLVM) stay in the trust
base. The lowering above them is validated, not proved ("Two semantics, one lowering").

**Trust base:** shrinks from v0's by the parts proved above.

**Size:** each target is a project of its own. Layout is the smallest, at about 10k lines of
Lean.

**Risks:** the pinned nightly moves. A spec of rustc's rules has to be re-checked on every
nightly bump, and a proof that breaks on a bump blocks the bump. It must not be skipped.

**CI:** release train only.

## Open questions

1. **When to retire Verus.** Decided 2026-11-10: now ("Verus retired"). Open instead: which
   automation closes the clauses Z3 proved unaided (`omega` and `simp` tried on the generated
   statement before a proof file is asked for, or agents writing the proofs).
2. **`blaze verify` or `blaze check --verify`.** This brief asked for a `blaze verify`
   command. Lane verus had decided on `blaze check --verify`. Does interactive proof work (goal
   states, holes, `Open` loops) need its own subcommand, or is the agent API enough?
3. **The manifest codec.** Does the manifest move from `serde_json` derive to a small
   hand-written codec, so its round trip is provable without modelling serde?
4. **Proofs inline in Rust?** Should short proofs (`by simp`, `by omega`) be allowed inside a
   Rust `proof { }` block, or must every proof live in `lean/`? Inline is easier for agents.
   Separate files keep Rust free of Lean syntax.
5. **Native evaluation.** Do we allow `bv_decide` and `native_decide`? SymCrypt trusts
   `bv_decide`. If we allow them, is the default per crate or per theorem?
6. **Who owns the Lean library fork.** Do we fork Aeneas's library at a pinned commit and
   merge upstream changes, or contribute upstream and pin a release?
7. **Model the binary?** Should v0 proofs model what the code generator emits, so that the
   proof covers the binary, or stay at MIR until v4?
8. **The C1 to C4 labels.** The 2026-11-04 review's crash-class numbering is not in the
   tree. Should it be recorded in the testing skill, so that v3's target can cite the labels?
