---
status: plan
summary: "blaze::unreachable!(): a site the compiler must prove no path reaches, by one abstract interpretation of a function's MIR (known variants, integer bounds, length relations, nonzero); the same proof takes dead panic sites off what #[blaze::nopanic] and the panic lints count. Also blaze::todo: a typed hole that warns locally and is an error in the merge gate and a release."
date: 2026-11-12
---
# Proved unreachable

```rust
fn get(v: &[u32], i: usize) -> u32 {
    if i < v.len() { v[i] } else { blaze::unreachable!() } // refused: nothing says `i < v.len()` fails never
}
```

`blaze::unreachable!()` is an opt-in extension (rule `superset-of-rust`): it compiles only when
blaze proves no path reaches it. Otherwise it is an error at the site naming the function and
what is missing: "`get`: can't prove this `blaze::unreachable!()` is never reached: can't show
`i < v.len()` here: no check before this branch rules the path out". There is no fallback:
what the prover cannot show is refused, never passed (rule `no-fallback`), and a body it
cannot analyse (it does not lower, its proof does not settle) is refused saying so.

The same prover discharges panic sites. `#[blaze::nopanic]` (effects.md) and the lints that
count a body's panics (`general_facts::panic_sites`: `panic_in_drop`, `unwind_across_ffi`) stop
counting a site the prover shows dead or shows cannot fail: `v[i]` under `if i < v.len()`,
`o.unwrap()` under `if o.is_some()` or `if let Some(_) = o`, `a / b` under `if b != 0`.

## What it is

- **The macro.** `blaze::unreachable!()` names `core::blaze::unreachable`, a builtin macro
  (`library/core/src/blaze.rs`), as `#[blaze::contract_test]` names `core::blaze::contract_test`:
  a path `blaze::unreachable` that names nothing else resolves there
  (`nameres/path_resolution.rs`, `bang_path_alias`; a crate's own `blaze::unreachable` wins, so
  no Rust program changes). It expands to `core::panicking::panic("entered
  blaze::unreachable!()")`: at run time it is a panic, never undefined behaviour, so a build
  that skipped the check is still sound. A proved site is dead code the optimizer may delete
  (not built: `SpendFacts` reads the proof, facts.md).
- **The check.** The lint `blaze::unreachable` (`blaze-lints`, `proved_unreachable.rs`),
  `deny` by default as `nopanic` is (it fires only where the code wrote the macro): one finding
  per unproved site. Sites are found by resolution (the call's macro is the builtin), never by
  text. An `#[expect(blaze::unreachable)]` is a written waiver; the site then panics if
  reached. Losing alternative: a hard error outside the lints (`hir::diagnostics`), which no
  level could waive but which would be a second place panic proofs are reported.
- **The prover.** `hir_ty_pure::reach` (rule `semantics-in-front-end`; beside `loops`, the other
  analysis the effect walk reads). `reach::prove(db, env, body) -> Reach` is a function of one
  body's runtime MIR (after drop elaboration and `transform::asserts`, before any optimization)
  and of the *signatures* of what it calls (which std function a callee is); it reads no callee
  body. So the answer is the same in every profile and at every optimization level, unlike the
  `no_panic` crate's linker trick, and it is cached per function: `reach::of_body(db, def)` is a
  query keyed by the definition, and the effect walk calls `prove` inside its per-instance
  summary query (rules `granular-caching`, `keys-cover-every-input`: the key is the body; the
  model table is code, covered by the binary's identity).

## Tiers

1. **Uninhabited types.** Already MIR's: lowering ends a block after a call or a value of an
   uninhabited type (`lower/uninhabited.rs`), a `match` on an empty enum is `Unreachable`. A
   site in such code has no reachable block, so it is proved by reachability alone.
2. **Facts from dominating control flow** (built). A forward abstract interpretation over the
   CFG. The state at a point is: a symbolic value per local (a hash-consed term: a constant, an
   unknown made at one program point, `Len(t)`, `Discr(t)`, a field or variant payload of a
   term, a comparison of two terms, the pointee of a reference) and a set of facts over terms
   (`a < b`, `a <= b`, `a == b`, `a != b`, and `p implies q`). Facts come from:
   - a `SwitchInt` edge (the value taken, or every listed value not taken) and an `Assert`'s
     success edge: `if i < n`, `match`, `if let`, `while`;
   - the type of a term: an unsigned integer is `>= 0`, a length is `>= 0`, a discriminant of
     `Option`/`Result` is 0 or 1;
   - the models (`reach/models.rs`): what a few std functions return, by path, as
     `effect_attrs::ROWS` are. `len`/`is_empty` of a slice, `str`, `Vec`, `String`
     (`Len`), `Option::{is_some, is_none}`, `Result::{is_ok, is_err}` (`Discr`),
     `<[T]>::{first, last, get}` (`Some` exactly when the length allows), `Vec`'s and
     `String`'s `deref`/`as_slice`/`as_str` (a view of the same length), `NonZero::get`
     (`!= 0`), `Range<int>`'s `Iterator::next` (`Some(x)` implies `start <= x < end`, and the
     end stays), and `IntoIterator::into_iter` of a `Range` (itself). A model also states when a call panics:
     `Option::{unwrap, expect}` unless `Some`, `Result::{unwrap, expect}` unless `Ok`,
     `{unwrap_err, expect_err}` unless `Err`, `Index::index`/`IndexMut::index_mut` of a slice,
     array or `Vec` by `usize` unless `i < len`.

   A site is discharged when the state before it entails its condition (an `Assert`'s, a
   model's panic condition) or when no feasible edge reaches its block. Entailment is small and
   decidable: syntactic equality of terms, constant bounds from the facts and the types (one
   step through a relation: `i < n` and `n <= MAX` give `i <= MAX - 1`), one step of
   transitivity, and implications whose premise holds. An overflow check (`i += 1` in a crate
   that keeps them) is discharged when the exact result fits the type for every value within
   the operands' bounds; `usize` is the target's width, never a guess. A `bool` two paths
   write (`a && b` is lowered to one) keeps, as implications, what each of its values meant
   on the paths that gave it.

   **Soundness of the memory model.** A term is a value, not a place. A shared borrow `&p`
   is a reference *to the value `p` has then* (`RefTo(v)`): safe code cannot change `p` while
   the reference lives, and the facts tracked (lengths, discriminants, integers) are not behind
   an `UnsafeCell` a place expression can reach. What is behind a `&mut`, a `Box` or a raw
   pointer is read at the state's *epoch*; any write through a pointer, any call no model
   names, any `Drop` and inline assembly start a new epoch and forget every local whose address
   a `&mut` or a raw pointer took. A slice's length is the reference's own metadata, so it
   survives the epoch. An unknown made at a program point is forgotten (with every fact that
   mentions it) when the point runs again; where two paths meet, a local with two values gets
   one unknown per (block, local) (per field for aggregates of one shape, so a range's end
   survives its loop) and only the facts both paths hold survive, so a loop is iterated to a
   fixed point; a joined value's constant bounds are widened after one turn. A coroutine body,
   inline assembly and a `for const` loop are not analysed (refused). Arithmetic is not
   followed in this slice (`i + 1` is an unknown).
3. **Contracts** (spec only). `#[blaze::requires(i < self.len())]` on a function is a
   precondition every caller must prove at the call with tier 2's entailment (a refusal names
   the call and the clause), and a fact the callee's body starts with; `ensures` is a fact at
   the call's return once Lean proves it (contracts.md part 3, `hir_ty_contracts`). The model
   table is the interim form of exactly this for std (`// when contracts land on library/: each
   row becomes the attribute on the function`), so tier 3 deletes the table. A requires clause
   inferred from the body (the branch facts over parameters that dominate each panic site,
   checked by proving the body again with them assumed) gives the same for unannotated
   callees.

## Cost against the floor

The floor for one function is one read of its MIR: O(statements). The prover visits each block
once per change of its entry state: once in a body with no loop, and at most
`2 + locals-that-change-in-the-loop` times per loop head (each visit either makes one more
local a join unknown or drops a fact). A visit costs O(statements of the block x facts held),
and the facts held are bounded by the comparisons on the path (tens). It runs only where asked:
a body that has a `blaze::unreachable!()`, a `#[blaze::nopanic]` walk's instance that has a
panic site, a lint that counts panics; elsewhere the cost is one scan of the body's macro calls
(O(calls)). A budget (`8 x blocks + 64` visits) ends a body that does not settle with nothing
proved (rule `bounded-progress`). After an edit, only the edited function's proof is redone
(its key is its body): the floor. Not measured yet on reth or tempo.

## Errors

One finding per site, at the site, naming the function, what could not be shown and why
(rule `errors-name-the-thing`): for an index "can't show `i < v.len()` here: no check dominates
this index"; for a site reached by a path, the last branch on a path to it. For `nopanic` the
refusal is unchanged; it just no longer lists a discharged site.

## Lean

Each discharge is a certificate a later Lean check can replay, not trust
(formal-verification.md, "The semantics of MIR"; verify.md's rule that a claim is recomputed):
`reach::Proved { site, goal, by }` where `goal` is an atom over terms and `by` lists the facts
used, each with its origin (the `SwitchInt` edge or `Assert` it came from, a type bound, or a
model row). The Lean side needs one lemma per kind of origin against the executable MIR
semantics (an edge taken gives its comparison; a model row is the callee's proved `ensures`)
and one per entailment rule (bounds, transitivity, modus ponens); the certificate then checks
by `decide`. The models are the trusted part until std carries contracts; the gate lists them
with the other trusted items. Not built.

## Prior art

- **SPARK (Ada)**: absence of run-time errors is a proof obligation per check, discharged by
  provers, with flow analysis first. Taken: every check is an obligation with a named site, and
  an undischarged one is an error, not a warning. Not taken: SMT in the build (nondeterministic
  time-outs break `deterministic-output`).
- **Verus, Prusti, Creusot**: `requires`/`ensures` proved by SMT or Why3. Taken: preconditions
  proved at the caller (tier 3). Not taken: a separate tool and annotation language; blaze's
  clauses are Rust expressions and the proof is Lean's (formal-verification.md).
- **Kani**: bounded model checking of each panic site. Taken: panic sites as the unit. Not
  taken: bounded unrolling, whose answer depends on a bound.
- **Zig `unreachable`, Rust `unreachable_unchecked`**: an unchecked promise, undefined behaviour
  when wrong. Taken: the meaning (the optimizer may assume it). Not taken: the trust; here the
  promise is proved or refused, and unproved code panics.
- **`no_panic` crate**: asks the linker whether the optimizer removed the panic. Not taken: the
  answer changes with the optimization level.

## `blaze::todo`: typed holes

`todo!()` is a hole, as in Agda and Haskell (`_`, typed holes: GHC reports the hole's type and
the bindings that fit). The lint `blaze::todo` reports each `todo!()` of non-test code with the
function, the type the expression must have (inference's type at the site) and, where cheap,
what in scope has that type: "`todo!()` in `open`: needs `Result<Dir, FsError>`; in scope:
`cached`". The prover never discharges a `todo!()`: it is unfinished, not dead.

- **The gate decides, not the profile.** Every local build, dev and `--release`, warns
  (`blaze::todo` is a `blaze::general` lint; the profile is not an input of it). The merge
  gate and a build for a release deny it with the lint's level, `--deny blaze::todo`. Built:
  the lint and its levels (tests: a warning with and without `debug_assertions`, an error
  under `--deny=blaze::todo`, test code allowed). Not built: the line in
  `.github/workflows/blaze.yaml`'s workspace check and in `release.yaml` that passes the
  level. blaze's own workspace already denies every general lint through its
  `lints-ratchet` (docs/lints.md), locally too, and has no `todo!()` outside fixtures.
  Test code (`#[test]`, `#[cfg(test)]`) is allowed.
- **At run time** a reached `todo!()` fails only its job with a typed error naming the site
  (rule `panics-fail-one-job`): the panic payload is `blaze_core::Todo { file, line, function }`,
  which the job's catch turns into its `JobError::Todo`. Spec only: the job plumbing is not
  reachable from the lints.
- **Listing.** `blaze check --message-format json` already carries the findings; `blaze status`
  lists the count per crate from the daemon's last lint results. Spec only.

## Proof and tests

`crates/blaze-lints/src/blaze/reach_tests.rs`: per discharged pattern one test that it passes
and near misses that are still reported (the check is on another index, slice or option, the
guard does not dominate, the value is reassigned between guard and use, the vector is cleared
between, the loop's bound is another length), for `nopanic`, for `panic_in_drop` and for
`blaze::unreachable!()` itself; a crate's own `blaze::unreachable!` keeps its meaning. The
invariant: *a site the prover discharges is not reached, or its condition holds, in every run
of the MIR*. Next level of evidence (not built): a Hegel test that runs random small bodies in
the MIR evaluator and checks no discharged site fails, with a planted bug (a fact kept across a
write).

## Not done

Arithmetic (`v[len - 1]`, `i + 1 < len`), `while let Some(x) = it.next()` over anything but a
`Range`, facts through user functions (tier 3), the Lean check of certificates, the optimizer's
use of a proved site, the run-time `Todo` error and `blaze status`.
