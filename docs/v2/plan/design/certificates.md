---
status: plan
summary: "A trait goal's check result kept as a checkable certificate (runtime.md rows V1 and X2): a derivation DAG of steps (goal, by) in columns, a checker that proves it by matching only, and the questions the solver's choice rested on, so a moved content read costs one linear check instead of a solve and a fetched certificate is checked, not believed. First piece in crates/blaze-cert."
date: 2026-11-17
---
# Certificates for trait goals

A solve answers `yes` and today keeps only the answer and its read list: any moved read reruns the
solve. A certificate keeps WHY: the steps, checkable without search. The coordinator's decisions:

- **V-a, the derivation.** A DAG of steps `(goal, by)`. `by` is one of Impl {key, args, nested
  where-clauses in order}, Env {clause index}, Builtin {rule, nested}, Auto {nested}, Object
  {bound}, AliasBound {bound}, Normalize {impl, args, nested}, Coinductive {back-edge}. Rows are
  columns; a sub-derivation two steps share is one row.
- **V-b, the checker.** Matching only: instantiate the impl header with the given args, compare it
  to the goal, check the nested goals are exactly the where-clauses in order. No candidates, no
  unification, no backtracking; linear, and it counts its cost.
- **V-c, the questions.** Certificate = derivation + the questions the choice rested on, with
  their answer digests. VALID TODAY iff every question answers as recorded AND the derivation
  checks.
- **V-d, expression level.** Spec only, below.

## The rows

Little-endian; header 28 bytes: `bcrt`, version 1, then the counts of rows, goal words, argument
words, nested references and impl keys (`u32` each). Then the columns, each one array:

| column | bytes per row | holds |
|---|---|---|
| goal_end | 4 | end of the row's goal in goal words |
| arg_end | 4 | end of its arguments in argument words |
| nested_end | 4 | end of its nested list |
| payload | 4 | impl key index, clause index, builtin code, bound index or back-edge target |
| tag | 1 | Impl 0, Env 1, Builtin 2, Auto 3, Object 4, AliasBound 5, Normalize 6, Coinductive 7 |

17 fixed bytes a row; then the goal words, argument words, nested row ids (4 bytes each) and the
impl keys (16 bytes each, stored once per step that names one). A goal is the solver's canonical
words (canon.rs): keys, not handles; variables numbered by first occurrence across the whole
certificate. The three-deep chain of the test is 4 rows. Rows are in post-order: a nested
reference names an EARLIER row, the root is the last, a back-edge names a LATER row (an ancestor).
`View::read` checks the length, every cap (rows 2^20, words 2^24 each, nested 2^22, keys 2^20),
every end, tag, payload, nested and back-edge reference before a row is read: a typed
`FormatError` naming the row, never a panic. Equal steps give equal bytes on every machine.

## Each step's check

Each row's goal is decoded once into the caller's arena; terms compare structurally, regions
equal to regions (the solver does not relate them). Nothing is normalized implicitly.

- **Impl {impl, args}**: the header's trait is the goal's; polarities match (positive/positive,
  negative/negative; a reservation never); `args` has the impl's generics count; the header's
  trait args instantiated by `args` equal the goal's; the nested steps' goals are the header's
  where-clauses instantiated by `args`, same count, same order. Errors: `HeaderMismatch`,
  `Polarity`, `ArgCount`, `NestedCount`, `NestedMismatch{clause}`, `UnknownImpl`.
- **Env {clause}**: the index is in the elaborated env the certificate is keyed by; the clause is
  not under a binder; it equals the goal; no nested. `EnvIndex`, `HigherRanked`, `EnvMismatch`.
- **Builtin {rule}**: the trait's lang item is the rule's and the self type's shape gives it;
  the nested goals are the rule's constituents (builtin.rs `step_sizedness`, `step_copy_clone`):
  Sized/MetaSized (leaves; str, slices and dyn for MetaSized only; a tuple's last element; an
  ADT's sized tail), Copy/Clone (fn items and pointers; a tuple's elements; a pattern type's base;
  a closure's upvars), Tuple, FnPtr, PointeeSized, AnyType (Pointee, DiscriminantKind, Destruct),
  Error (an error self type).
- **Auto**: the trait is auto (no movable-coroutine `Unpin`); the nested goals are the
  structural constituents (structural.rs `step_auto`), an ADT's fields instantiated.
- **Object {bound}**: self is `dyn`; the bound is `+ Trait` for the goal's auto trait, or the
  principal (index 0, dyn compatible) with the goal's args; nested are the trait's supertraits
  then its non-generic associated types' bounds over `[self] ++ args`. A `dyn` with projection
  bounds is refused (`ObjectProjections`), not guessed.
- **AliasBound {bound}**: self is a rigid projection (its item's bound at that index) or an
  opaque type with no visible hidden type (its bound); equal to the goal; no nested.
- **Normalize {impl, args}**: the goal is `<.. as Trait>::A == U`; the impl is of `Trait`, its
  header args equal the alias's parent args; its value for `A` (not `default`), over `args` then
  the alias's own args, equals `U`; nested are the where-clauses.
- **Coinductive {target}**: the goal equals the target's; every row in `(self, target]` is an
  auto or coinductive trait's goal or a projection, and at least one is a trait's (eval.rs path
  kinds). Else `InductiveCycle`. Prefix counts make each O(1).

Every fact is read through the solver's recorder (`facts::Ask`, already public): the check's
reads are recorded like the solver's. A pass returns `Checked<'d>` (rule 74): the view, the reads
and the `Cost` (rows, headers instantiated, node pairs compared, words decoded).

## The invariant

Stated once at the top of crates/blaze-cert/src/check.rs: valid today iff every question stands
and the derivation checks; and the claim the tests raise evidence for: **valid today implies the
solver, run today, returns the same response.** The derivation proves the positive half (these
steps apply); the questions carry the negative half (no other candidate was or is applicable):
every read of the solve except the impl headers and ADTs the check itself read, which it proves
again. So an edit to an impl's where-clause or a field's type is one linear check, and a new
impl in a searched bucket voids the certificate.

## The differential test

crates/blaze-cert/src/tests.rs, over blaze-solve's test world (prelude and fixture text). Each
case lists the root then each step's goal; the solver answers `yes` to every one, and a hand-built
derivation checks: a three-deep impl chain, an env clause, `Send` for a recursive type
(back-edge), a projection by an impl's value, builtin structural rules (a shared row for
`(K, K): Clone`; `Sized` through a tuple and a struct), object bounds. Planted controls the
checker must refuse, each by the row it names: a wrong impl argument, a nested step for the wrong
where-clause, a missing nested step, an env clause that does not hold the goal, an inductive cycle
closed by a back-edge; every truncation of valid bytes is a `FormatError`, and every one-byte flip
is an error or a result, never a panic. Questions: a second impl that could match moves one; an
unrelated impl moves none, and the derivation checks again in a fresh arena. A cost line prints
the check's counts beside the solver's frames and candidates for the same goal.

## What is out (this piece)

Steps for FnTrait/AsyncFn, TraitAlias, Unsize, Future, WellFormed and the builtin normalizations
(fn output, discriminant, metadata, future output); env projections and normalizing through a
rigid alias; object projection replacement; higher-ranked goals and clauses; responses with
inference variables bound (the certificate proves a ground or as-given goal); the questions' own
byte format; `ConstHolds` questions (arena lists, not portable); coherence-mode goals. The emitter
(the solver appending steps as it picks candidates) is the next piece.

## V-d: expression-level certificates (spec only)

A body's type check is a set of goals plus a typing of each expression. The same scheme lifts:
a body certificate is the typing (one row per expression: its type in canonical words and the
rule that typed it: a literal, a path's signature instantiated with args, a method's resolved
impl and autoref steps, a coercion's kind) plus, per obligation, the trait certificate above by
reference. Its checker re-derives each expression's type from its children's rows and the rule,
matching only: a method row names the impl and the probe's adjustments, so no probe runs; a
coercion row names the coercion. Its questions are the union of the trait certificates' plus the
signature and field reads the rows name. An edit inside a body whose items' signatures did not
move rechecks that body linearly and nothing else; a signature edit voids exactly the rows that
named it. Built only after the trait emitter, on blaze-typeck's own expression ids (item-relative,
rule 71).
