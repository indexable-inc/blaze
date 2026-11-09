---
status: plan
summary: The long-term verifier: blaze translates its own borrow-checked MIR to Lean 4 (a query in the front end) and proves specs written once in blaze's extensions. v0 safe Rust by functional translation, v1 unsafe and interior mutability in an Iris-style logic, v2 atomics (SC, then RC11), v3 crashes on files and mmap (Perennial-style, sharing the DST's kernel model), v4 compiler correctness. DST stays on everything.
date: 2026-11-04
---
# Formal verification: blaze's MIR to Lean

This is the long-term plan for proofs in blaze. blaze translates its own MIR to Lean 4, and a
Lean program logic proves the specs. Other docs own the parts this one builds on, and it links
to them rather than repeating them:

- **The spec form and the first prover**: lane verus ([verus.md](verus.md)). It owns
  the native clause grammar (`requires`, `ensures`, `invariant`, `spec fn`, `ghost`/`tracked`),
  `VerifyRefused`, the interface `proved(..) -> Option<ProofKey>`, and `blaze check --verify`.
- **Checked, generated, used contracts**: [contracts.md](contracts.md).
- **Purity and effects**: [pure-fns.md](pure-fns.md) and [effects.md](effects.md).
- **How unsafe code is checked today**: [unsafe.md](unsafe.md), with the shared-memory rules in
  [concurrency.md](concurrency.md).
- **Testing, including DST**: the
  [testing skill](../../../../.agents/skills/testing/SKILL.md).
- **The store and incremental checks**: [verify.md](verify.md). Despite its name, that doc is
  not about program proofs.

## Decisions (2026-11-04)

### Lean, not Verus, in the long term

- **One prover for everything.** Lean, with a program logic written in Lean, covers pure
  code, unsafe code (separation logic), concurrent code (atomics, then weak memory), durable
  code (crash plus concurrency), and later the compiler's own rules (layout, const eval, trait
  resolution). Verus is built around SMT queries over a first-order encoding. It can encode
  ownership ghost state, but weak memory and crash-aware logics are outside what it was
  designed for, and nobody has built them on it.
- **No SMT brittleness.** A Lean proof is a term that the kernel checks. It does not shift
  when a solver version changes, a seed changes or a trigger heuristic changes. Lean's
  `maxHeartbeats` is a deterministic budget, as Z3's `rlimit` is in lane verus. But a failed
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
- **What Verus is better at.** First proofs come faster, SMT automates linear arithmetic and
  quantifier reasoning, and
  [AutoVerus](https://www.microsoft.com/en-us/research/publication/autoverus-automated-proof-generation-for-rust-code/)
  shows LLMs proving Verus-Bench tasks. That is why lane verus stays as the near-term
  prover (see "Verus in the meantime").
- **No rustc.** Upstream Verus (`rust_verify`) and Aeneas's front end, Charon, are both
  rustc drivers. `no-rustc` forbids them. Lane verus already replaced Verus's rustc end with
  a lowering from blaze's HIR. This plan does the same for Aeneas: blaze translates from its
  own front end, and nothing runs rustc. Neither prover is ruled out by rustc. Lean wins on
  capability.

### DST on everything, proofs on small stable cores

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

- **One home for specs: the Rust source.** Specs are lane verus's native clauses, plus
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

### Verus in the meantime

Lane verus continues with its VIR/AIR/Z3 tier for arithmetic clauses, which feed
contracts part 3.

- **Both provers answer the same `proved(..)`.** The `ProofKey`'s "how" names the backend,
  so a caller's code identity records which prover stands behind each fact it relies on.
- **The specs are shared.** The spec has one home, and the two provers are two back ends
  behind one interface.
- **Retiring Verus.** When v0 proves every clause the Verus tier proves, retiring the Verus
  tier gets decided, and its `ProofKey` variant is banned through the usual lint (open
  question 1).

## Shared pieces (all phases)

### The pipeline

1. **Structured MIR.** A query over blaze's borrow-checked, built MIR (before optimization).
   Without optimization, the model is of what the programmer wrote, under the pinned
   nightly's semantics. The query reconstructs `if`/`match`/`loop` from the CFG, the way
   Charon makes LLBC from ULLBC ([Charon](https://arxiv.org/abs/2410.18042)). It also folds
   checked arithmetic and bounds assertions back into single operations. Every back end below
   reads this one query.
2. **Spec lowering.** The clause groups from lane verus, which MIR building drops, are
   lowered from typed HIR into Lean terms: `int`/`nat` for spec arithmetic, `forall`,
   `==>`, `old`, and `spec fn`.
3. **Lean emission.** One Lean module per item instance, so a module's imports are exactly
   the item's dependencies. Each module holds the model, the generated theorem statements
   for its clauses, and a reference to the proof by name.
4. **Proof check.** blaze runs the pinned Lean toolchain on the generated module plus the
   user's proof file. The toolchain is a pinned input that blaze fetches itself, as the
   remove-nix plan requires. Lane verus gets `BLAZE_Z3` from the flake today, and that moves
   the same way.

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
    modular, as in lane verus.
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

`blaze check --verify` (lane verus's decision: one command for checks) runs every changed
obligation. It reports each obligation as one record, rendered for a terminal and given to
agents as data over the agent API ([roadmap §8](../../plan.md)):

```text
{ item, clause, prover: lean|verus, key,
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

**Refused.** These produce lane verus's `VerifyRefused` diagnostic, which names the item, the
rule and the construct; there is no second diagnostic.

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

## v1: interior mutability and unsafe, by deep embedding

**Scope.** Code that uses `UnsafeCell`, raw pointers, `unsafe` blocks, and the safe APIs built
on them.

**How.**
- A deep embedding: an operational semantics of structured MIR in Lean, over a heap with
  permissions, plus a separation logic over it.
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

**Trust base:** v0's, plus the MIR semantics in Lean and its memory model. It must agree with
the MIR evaluator, which blaze's `--ub` checker uses ([unsafe.md](unsafe.md)), on a corpus,
and differential runs keep the two aligned.

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

**CI:** as in v0. The semantics also gets a differential run against the MIR evaluator.

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
2. **Const eval.** blaze's MIR evaluator refines v1's MIR semantics.
3. **The translator itself.** The structured-MIR-to-Lean translation is sound against v1's
   semantics: v0's functional model is a refinement of the deep embedding, as Aeneas proved
   its symbolic semantics against a borrow checker (ICFP 2024). This removes the emitter from
   the trust base.
4. **Trait resolution.** Coherence, and determinism of selection. Full equivalence with
   rustc's solver is out of scope, because the solver itself is still changing upstream.

**Refused:** codegen and linking (the stencils and LLVM) stay in the trust base. Proving them
would mean a CompCert-scale effort and is not planned.

**Trust base:** shrinks from v0's by the parts proved above.

**Size:** each target is a project of its own. Layout is the smallest, at about 10k lines of
Lean.

**Risks:** the pinned nightly moves. A spec of rustc's rules has to be re-checked on every
nightly bump, and a proof that breaks on a bump blocks the bump. It must not be skipped.

**CI:** release train only.

## Open questions

1. **When to retire Verus.** Do we retire lane verus's Z3 tier once v0 matches it, or keep
   SMT as a fast first try that hands off to Lean? Keeping both means keeping two trust
   bases.
2. **`blaze verify` or `blaze check --verify`.** This brief asked for a `blaze verify`
   command. Lane verus decided on `blaze check --verify`. Does interactive proof work (goal
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
