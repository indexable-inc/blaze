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
- **The check.** The lint `blaze::unreachable` (`blaze-lints`, `proved_unreachable.rs`): one
  finding per unproved site, found by resolution (the call's macro is the builtin), never by
  text. It fails the compile and **no level waives it** (`blaze::UNWAIVABLE`):
  `Levels::of` gives it `deny` whatever an attribute, a flag, `warnings` or `--cap-lints`
  says; an `#[allow]`, `#[expect]` or `#[warn]` that names it is itself an error at the
  attribute; and a flag that names it below `deny` (the command line's, a `[lints]` table's,
  `[workspace.metadata.blaze.lints]`, its `lints-ratchet`, a baseline: all reach the lints as
  flags) refuses the crate's check (`CheckError::Unwaivable`); blaze-units refuses the same
  line in a workspace table when it loads the manifest. A group or `warnings` that covers the
  lint does not reach it and is no error. Whoever cannot prove a site returns a `Result` or
  writes core's `unreachable!()` and accepts that it counts as a panic site. The run-time
  panic stays as defence in depth (a crate whose lints are not run, a dependency).
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
   model's panic condition) or when no feasible edge reaches its block. A model that states
   no panic condition (`len`) discharges nothing.

   **Linear arithmetic.** `a + b`, `a - b`, a multiple or a shift by a constant are a linear
   form, `sum of coefficient * term + constant` (`Term::Lin`: exact integers, hash-consed, the
   terms in order). `x / d` and `x >> c` of a value known not negative, `d` a positive
   constant, are a quotient `q` with `0 <= q`, `d*q <= x <= d*q + d - 1`, `q <= x`, and
   `q < x` when `d >= 2` and `x >= 1` (which gives `lo + (hi - lo) / 2 < hi`). A form is the
   value of a local **only where no wraparound is shown**: the state's facts (not the type's
   range) put the exact result inside the type, or the operation is checked and the path is
   past its overflow `Assert` (the flag being false implies the value is the form). A wrapping
   operation not shown to fit is the form wrapped into its type (`Term::Wrap`), an unknown
   the same operands give again, so `if i + 1 < n { v[i + 1] }` agrees with itself and
   `v[v.len() - 1]` with no guard is not discharged without overflow checks. A product of two
   unknowns is an unknown.

   **Entailment** is small and decidable: syntactic equality of terms; constant bounds from
   the facts and the types; implications whose premise holds; and for `<`, `<=`, `==` adding
   facts up: every `a < b`, `a <= b`, `a == b` of the state is a bound `form <= c`, and
   `goal <= c` holds when the bounds of the goal's own terms give it, or some fact that
   cancels one of its terms leaves a remainder shown the same way. On two-term facts
   (`x - y <= c`) this is the shortest path of a difference-constraint graph, as ABCD
   (Bodik, Gupta, Sarkar, PLDI 2000) and the octagon domain use, without the closure: it is
   goal-directed. **The budget:** at most 4 facts added up per proof and 256 tried per
   question (`LINEAR_DEPTH`, `LINEAR_FUEL`); a proof that needs more is not found and the
   site stays reported. A `bool` two paths write (`a && b` is lowered to one) keeps, as
   implications, what each of its values meant on the paths that gave it; an implication
   whose premise holds is written down as a fact, since the linear proofs read facts.

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
   fixed point; a joined value's constant bounds are widened after one turn. A joined integer
   also keeps `j <= y` and `y <= j` for each value `y` a path brings for it, when every path's
   own value satisfies it in that path's state: a binary search loop's `hi <= len` holds on
   the way in and on every way round, so it holds at the head. A coroutine body, inline
   assembly and a `for const` loop are not analysed (refused).
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
  (`blaze::todo` is a `blaze::general` lint; the profile is not an input of it). The gate
  denies it on the command line: `blaze check --deny blaze::todo`. `--allow`, `--warn`,
  `--deny` and `--forbid LINT` are `check`'s lint levels (`blaze_units::lint_level`,
  `crates/blaze-cargo/src/cli/args.rs`): each workspace member gets them after the
  workspace's tables, the ratchet's baselines and its own `[lints]`, and they are part of the
  load's key (the schema and producer versions of the asked invocation and the plan are
  bumped). blaze's own workspace denies `general` in its `lints-ratchet`, read on every run,
  so the table names `todo = "warn"` to keep a hole a warning locally.
  Where the level is passed today: the `lint-baselines` job of `.github/workflows/blaze.yaml`,
  the one step that checks with the blaze built from the tree (that job is not yet among the
  gate's required ones). The `workspace-check` steps and `release.yaml` run the released
  blaze (`.github/blaze-version`), which has neither the option nor the lint: each carries a
  comment saying what to add once a release has them, and nothing in the release workflow
  denies `blaze::todo` yet. A lint flag in `RUSTFLAGS` still comes after the command line's
  (`blaze-lower`'s `lint_flags`), so `-A blaze::todo` there would undo the gate's level:
  not fixed.
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
`blaze::unreachable!()` itself; a crate's own `blaze::unreachable!` keeps its meaning; a
waiver by attribute or by flag is refused. Linear arithmetic is tested in both overflow
profiles: `v[len - 1]` under a non-empty check, `v[i + 1]` under `i + 1 < len`, a pair under
`i < len - 1`, the middle of `lo < hi <= len`, `v[len - k]` under `1 <= k <= len`; and still
reported: `v[len - 1]` unguarded (the wrap, with overflow checks off), `v[i + 2]` under
`i + 1 < len`, `v[i * j]`. A binary search loop passes in both profiles and the same loop
started one past the length is reported.

The invariant: *a site the prover discharges is not reached, or its condition holds, in every
run of the MIR*. It is held by a Hegel test against the MIR evaluator
(`crates/hir-ty-pure/src/tests/reach_oracle.rs`): random guards (variants, bounds, lengths,
linear expressions with `+`, `-`, `/ 2`, `&&`) around a random site, random arguments with
the ends of `usize` among them, both overflow profiles; a body all of whose panic sites are
discharged must not panic when run. Controls: a guarded site is proved and runs while the
bare one is not proved and panics; a binary search loop is proved and runs while the one
started past the length is not proved and panics; and a planted model entry (`first` is
always `Some`) is caught.

## Not done

A loop's invariants beyond a joined integer against the values its paths bring (a relation
between two values that both change round the loop, such as `i + j == n`), products and
other non-linear arithmetic, division
by a non-constant or of a value that may be negative, proofs past the linear budget,
`while let Some(x) = it.next()` over anything but a `Range`, facts through user functions
(tier 3), the Lean check of certificates, the optimizer's use of a proved site, the run-time
`Todo` error and `blaze status`.
