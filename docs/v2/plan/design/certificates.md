---
status: plan
summary: "A trait goal's check result kept as a checkable certificate (runtime.md rows V1 and X2): a derivation DAG of steps (goal, by) in columns, a checker that proves it by matching only, and the questions the solver's choice rested on, so a moved content read costs one linear check instead of a solve and a fetched certificate is checked, not believed. Built, not wired, in crates/blaze-cert; expression-level certificates are spec only."
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

## Built, not wired

crates/blaze-cert: the row format (`derivation.rs`, every limit checked before a row is read),
the checker (`check.rs`, which states the invariant: valid today implies the solver, run today,
returns the same response), the questions (`questions.rs`), the emitter from a recording solver
(`emit.rs`), and the differential test with planted controls (`tests.rs`, `emit_tests.rs`). No
crate but its own tests calls it: the solver neither stores a certificate nor checks one before a
solve. That wiring is the next piece.

## Not built in this piece

Steps for FnTrait/AsyncFn, TraitAlias, Unsize, Future, WellFormed and the builtin normalizations
(fn output, discriminant, metadata, future output); env projections and normalizing through a
rigid alias; object projection replacement; higher-ranked goals and clauses; responses with
inference variables bound (the certificate proves a ground or as-given goal); the questions' own
byte format; `ConstHolds` questions (arena lists, not portable); coherence-mode goals.

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
