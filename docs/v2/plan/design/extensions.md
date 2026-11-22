---
status: current
summary: The ledger of blaze's language extensions on the new item layer. 61 features from three sources (docs, the old front end's code, blaze's own uses), each probed through the new layer's stages on 2026-11-16 (crates/blaze-real/examples/extensions.rs, one fixture each); what blaze's code uses, where the old layer built it, what the new layer does today, the item-grain design and its floor, a proposed better form, and the port order. Replaces the 2026-11-10 page on extension crates (blaze-ext), whose crates were deleted in 8e87a017ff.
date: 2026-11-16
---
# Language extensions on the item layer

Every claim below is a line of a run on 2026-11-16 over the tree at `dbae95611d` plus the working
tree of /Volumes/Projects/blaze-fresh-1115. Reproduce: `examplerun.sh blaze-real extensions
crates/blaze-real/extensions` (the probe) and `examplerun.sh blaze-real extensions --tree <dir>`
(a tree). "Better form" cells are **proposals for Andrew to decide**, none decided.

## 1. The list, from three sources

- **(a) Docs.** No doc was an inventory. edition.md lists eleven changes, five gates and the lints
  (the nearest, about 40% of the list, and stale: its `#[blaze::may]`, `negative_effect` and
  `unmarked_*` rows describe blaze-lints, deleted 2026-11-15). language.md is Rust 1.80..1.100, no
  extension. This page was the blaze-ext design. 51 `#[blaze::..]` names appear in docs/.
- **(b) Code.** 50 `blaze::` attribute names in `.rs` files; hir-def's `AttrFlags` reads 31
  (crates/hir-def/src/attrs.rs), crate attributes the rest (nameres/crate_attrs.rs). Five gates
  (`blaze_comptime`, `_verify`, `_implied_bounds`, `_anon_params`, `_const_exprs`,
  crates/intern/src/symbol/symbols.rs:742). The old parser takes three syntax sets behind crate
  bits (crates/edition/src/lib.rs:47: `UNIQ`, `ERROR_UNIONS`, `RECORDS`) and seven forms with no
  bit: `for const`, `x.[f]`, `const static {}`, `where const {}`, `T is PAT`, `&self.{a}` views,
  `requires`/`ensures`/`decreases`/`recommends` (parser/src/grammar/*.rs). Analyses: hir-ty-pure
  (25k lines: effects, purity, program lifetime, roles, locks, refined), -linear (2.6k), -suspend,
  -coherence (sealed), -specialization, -lean (18k, theorems), -comptime (114), -ub (blaze's Miri),
  and in hir-ty-mir `early_drop/`, `handles.rs`, `bits.rs`, `checks.rs` (checkfree). Rust parity,
  nothing beyond Rust: -borrowck, -matchck, -unsafeck, -wfcheck, -tailck, -identity, -share, -print.
- **(c) Uses.** Counted by `rg '^\s*#!?\[blaze::NAME'` outside tests (`tests/`, `tests.rs`,
  test-utils): only **14 attributes appear in blaze's own code**: `noalloc` 542, `nonblocking` 520,
  `nopanic` 400, `sans_io` 262, `pure` 260, `renders_positions` 43, `consumes` 32, `trust` 21,
  `linear` 17, `theorem` 16, `policy` 9, `holds` 2, `canonical_cbor` 1, `arbitrary_ints` 1. Lint
  levels: `blaze::sans_io_core` 92, `unwired` 27, `as_cast` 11, `reads_positions` 8,
  `implicit_allocator` 6. Gates: **0** crates turn one on. Syntax outside the front end's own
  crates: `where const` 3 (crates/blaze-std/src/{own,flat}/extended.rs, under `cfg(blaze)`), the
  `core::reflect` API (library/core/src/reflect.rs). The brief's counts (all occurrences: `noalloc`
  566 .. `specialization` 15, `blaze_comptime` 142) are reproduced within 3% (566, 545, 422, 311,
  294 ..; `blaze_comptime` 144); the difference is test fixtures and doc comments: e.g. `bits` 87
  occurrences, 0 in code; `error_unions` 50, 0; `records` 72, 0.

**Differences.** In docs, not in code: `constant_time`, `secret`, `terminates`, `derivative`,
`transform`, `contract_tests`, `requires` (specified, never built). In code, not in docs:
`canonical_cbor`, `checkfree`, `pairs_with`, `renders_positions`, `other` (a derive helper),
`views` as an attribute spelling (built, unspecified). Built and unused by blaze's code: every gate,
all syntax but `where const`, and 36 of the 50 attributes. Built and gone: blaze-lints (the
checkers of `sans_io`, `capabilities`, `renders_positions`, `canonical_cbor`, `policy`,
`pairs_with`, `implicit_overflow`, `may`) and blaze-ext/blaze-ext-host, deleted in 8e87a017ff.

## 2. The probe

61 fixtures in crates/blaze-real/extensions/, each a `#![no_core]` program with one feature; a
fixture marked `expect: refuse` carries a violation of the feature's rule. Stages: outline
(`blaze_file_items::outline`), expansion census (`blaze_real::census`), bodies (`lower_body`,
`to_typeck`), then names, check and build (`blaze_ladder::run::refusals`, whose front stands in
for blaze-scope). Verdicts, counted from the run:

| first stage that refuses | n | features |
|---|---|---|
| ladder: ``"this attribute is not supported by this build of blaze yet"`` (a catch-all for any unknown attribute, not the feature's rule) | 28 | every item attribute: noalloc, nonblocking, nopanic, expect, may, answers_request, bounded_turn, lock_checked, linear, consumes, holds, may_suspend, invariant, program_lifetime, renders_positions, sealed, norefcount, pairs_with, checkfree, drop, early drop, bits, reflect, specialization, theorem, contract_test, multiversion, differentiable |
| **passes every stage with its violation: nothing reads it** | 7 | `sans_io` (module), `capabilities`, `effects`, `canonical_cbor`, `policy` (field), `implicit_overflow`, **`where const { false }`** (the impl holds anyway: a soundness hole for blaze-std's `Acyclic`; closed 2026-11-16: `{ false }` refused, `{ small::<T>() }` accepted, see the row below) |
| passes, accepted | 6 | control, `trust`, `edition`, `loop_match`, `views` (parsed with no bit; nothing checks the view), `where const { true }` |
| outline | 3 | error unions ``"expected a block"``, records ``"expected type, found `{`"``, uniq ``"expected `,`"``: the outline parses with `Extensions::NONE` (blaze-file-items/src/lib.rs:184) |
| bodies | 4 | `const static` ``to_typeck: a const block``; `become` ``to_typeck: `become` ``; `blaze::unreachable!`, `native_lanes!` ``lower_body: a macro call, not expanded`` |
| check | 8 | `for const`, `x.[f]` ``"`sum`: 5 type errors, the first UnresolvedPath"``; verify clauses ``"`next`: 1 type errors, the first Mismatch"``; implied bounds ``NoMethod``; anon params ``AnnotationsNeeded``; `u7` ``"the type `u7` names nothing"``; pattern types ``"a pattern type `T is ..` is not supported"``; const exprs ``"a const generic parameter is not supported"`` |
| ladder, other | 5 | `type_of!` (a macro in a type), handles (`#[lang = "blaze_handle"]`), `pure`, `memo`, `cache` (``"a `static` item is not supported"``, a ladder limit before the feature) |

The new layer's crates name no extension: `rg 'blaze::|Extensions'` over blaze-file-items, -lower,
-typeck, -solve, -consteval, -scope, -expand, -build, -lint, -drive, -ty finds only `sans_io` marks
on their own modules, `blaze::bits` in blaze-expand's derive and `#[blaze::sealed]` in
blaze-solve/src/facts.rs:82. blaze-consteval's `Intrinsic` has no reflection entry.

**blaze's own crates** (`--tree` on a `git archive` of `dbae95611d`, crates/, 3183 files and
1.73 M lines outside tests): outline syntax errors 1168, of which **1 on an extension's line** (a
parser test file's `become`) and 1161 in parser/syntax test data; bodies 103531, 20405 stopped
(80.3% clean): **12 on a line with extension syntax, all `lower_body: a macro call, not
expanded`**; the rest are Rust (19151 unexpanded macro calls, 245 `_` as an expression, 156 path
anchors, 151 const arguments, 143 format arguments, 125 range patterns ..). library/core: 299
files, 0 syntax errors, 5664 bodies 88.3% clean, 0 stopped on an extension line. `stages` itself
panicked on this tree before its body stage (`stages.rs:1102:75: index out of bounds: the len is
22656 but the index is 22656`, in `one_edit`). **So extension syntax costs blaze's own crates
nothing today; what they need is the 14 attributes read and checked.**

## 3. The ledger

Uses = attribute lines in code outside tests / all occurrences. Old = the old layer's home. Kind:
A analysis, B syntax or type system, C proof, D code generation. Lanes: E expansion (outline,
lower), K checker, S solver, V constant evaluator, L lint crate, B builder; ids from the
coordinator.

| feature (written) | uses | spec; old | kind | today | better form (proposal) | lane |
|---|---|---|---|---|---|---|
| `#[blaze::noalloc]`, `nonblocking`, `nopanic` | 542, 520, 400 / 566, 545, 422 | effects.md; hir-ty-pure effect_attrs.rs | A | ladder catch-all | one effect row: `#[blaze::expect(..)]` lists what a fn does; these three become its absence. Fix: `blaze::negative_effect` writes the inferred list (1462 lines change) | L |
| `#[blaze::pure]`, `memo`, `cache` | 260, 0, 0 / 311, 19, 9 | pure-fns.md; hir-ty-pure, hir-ty-codegen/fncache.rs | A, D | catch-all; `static` first | `pure` = the empty row plus "reads no global"; `memo` and `cache` merge into one library over `fn_info`/`code_id` (reflection) | L, V |
| `expect`, `may`, `#![blaze::effects]` | 0, 0, 0 / 58, 5, 7 | effects.md; hir-ty-pure | A | catch-all; gate passes with violation | as it was: `expect` is the one spelling; `may` stays refused by path | L |
| `answers_request`, `bounded_turn`, `lock_checked`, `may_suspend` | 0 each / 8, 10, 2, 19 | effect-summary.md, may-suspend.md; hir-ty-pure roles.rs, locks.rs; hir-ty-suspend | A | catch-all | effect words in the same row: `sync`, `unbounded`, `locks`, `suspend` | L |
| `#[blaze::sans_io]`, `#![blaze::capabilities]` | 262 (+92 lint levels), 0 / 294, 24 | std.md; blaze-lints (deleted) | A | **violation passes** | the module default of the row without `io`: `#![blaze::expect(..)]` on the module; fix: rewrite the mark | L |
| `#[blaze::renders_positions]` | 43 (+8 levels) / 52 | rule 71; blaze-lints (deleted) | A | catch-all | an effect word `positions`; fix: rename | L |
| `#[blaze::linear]`, `consumes`, `holds` | 17, 32, 2 / 182, 60, 11 | linear.md; hir-ty-linear | A | catch-all | as it was for `linear`, `consumes` (explicit end points); `holds` derived from a `PhantomData<T>` field by reflection (fix: add the field, 2 sites) | K |
| `#[blaze::drop(early/scoped)]`, `#![blaze::early_drop]` | 0, 0 / 40, 20 | early-drop.md; hir-ty-mir early_drop, hir-ty-pure drop_effects | B | catch-all | "frees only memory" derived from the `Drop` impl's effect row; the attribute only for `scoped` and `trusted` | K, B |
| `#![blaze::handles]`, `norefcount` | 0 / 24, 12 | edition.md change 1; hir-ty-mir handles.rs | B | lang item refused | as it was (a `Handle` trait is a library) | K |
| `#[blaze::sealed]` | 0 / 49 | sealed-orphan.md; hir-ty-coherence | A | catch-all | as it was | S |
| `#[blaze::program_lifetime]` | 0 / 21 | facts.md; hir-ty-pure program_lifetime.rs | A | catch-all | derived by reflection (no `Drop`, no parameter); the attribute asserts | L |
| `#![blaze::trust(..)]` | 21 / 32 | trust.md | A | passes, unread | the grant moves to the workspace manifest beside the trust list; the attribute keeps the reason | L |
| `#![blaze::canonical_cbor]` | 1 / 3 | none; blaze-lints (deleted) | A | **violation passes** | dropped: the encoder sorts fields at compile time by reflection (fix: delete, 1 site) | V |
| `#[blaze::policy]`, `pairs_with`, `checkfree` | 9, 0, 0 / 11, 1, 4 | lock-policy?; blaze-lints, hir-ty-mir checks.rs | A | policy passes; others catch-all | `policy` a `#[blaze::mark(Policy)]` read by a library lint; `pairs_with` dropped for a linear token; `checkfree` one use of the unreachable prover | L |
| `#[blaze::invariant(..)]`, `Int<LO, HI>` | 0 / 20 | refined-values.md; hir-ty-pure refined | C | catch-all | contracts and pattern types cover it: an invariant is a pattern type of the struct | K |
| `blaze::unreachable!()` | 0 / 12 | proved-unreachable.md | C | macro not expanded | as it was | L |
| `blaze::implicit_overflow`, `lossy_as` | 0 / lint | explicit-overflow.md | A | violation passes; `as` casts are blaze-lint's `Casts` token rule (17893 findings) | as it was | L |
| `#[blaze::theorem]` | 16 / 83 | proofs-for-sans-io.md, formal-verification.md; hir-ty-lean | C | catch-all | as it was | needs a lane |
| `requires`/`ensures` (`blaze_verify`), `contract_test` | 0, 0 / 19, 18 | verus.md, contracts.md | C | check: `Mismatch` | one contract form: the native clauses; nightly's `contracts` attributes rewritten to them; a contract test is a mode of `blaze test`, not an attribute | K |
| `for const`, `x.[f]`, `const static {}` (`blaze_comptime`) | 0 / 144 | comptime.md; hir-ty comptime.rs, hir-ty-mir reflection | B | check: `UnresolvedPath`; `to_typeck: a const block` | the primitive the others become libraries over (rules 32, 56) | V, K |
| `where const { COND }` | 3 / 42 | ownership.md; hir-ty-coherence | B | 2026-11-16: lowered as `WherePred::Const`; a solver goal `ConstHolds` (false, an evaluation error or a generic argument fails, never true; ambiguous under coherence; the item's own clause assumed); typeck's unmet-bound finding names the condition; the ladder evaluates each condition with blaze-consteval at the impl's ground arguments (a `const fn` call included): probe `b_where_const` accepted, `b_where_const_false` refused (`NoMethod`: a method probe's finding does not carry the reason yet) | as it was | S, V |
| `#[blaze::reflect]`, `#[blaze::mark(M)]`, `#[blaze::attr]` | 0 / 5, 7, 4 | comptime.md | B | catch-all | `mark` becomes THE attribute primitive: `policy`, `bits`, `canonical_cbor`, `invariant` are marks read by libraries | V |
| `type_of!(desc)` | 0 / 44 | type-of.md | B | macro in a type refused | as it was | V |
| `T is PAT` (`pattern_types`) | 0 / 49 | edition.md change 8; hir-ty pat | B | check: unsupported | as it was | K |
| `u1..u128` (`arbitrary_ints`), `#[blaze::bits]` | 1, 0 / 46, 87 | arbitrary-ints.md, bits.md; hir-ty arbitrary_ints.rs, bits.rs | B | `"the type u7 names nothing"`; catch-all | `uN` stays primitive; `bits` a library type computing its layout by reflection | K, B |
| `A \| B` (`error_unions`), records, `&uniq` | 0 / 50, 72, 27 | error-sets.md, records.md, edition.md | B | **outline refuses** | as they were; the bits go in the outline's key | E |
| views `&self.{a}` | 0 / 25 | edition.md change 3 | B | parsed, nothing checks it | spelled under a crate bit like the others | E, K |
| `#![blaze::specialization]`, `specialization_trait` | 0 / 15, 11 | specialization.md; hir-ty-specialization | B | catch-all | as it was | S |
| `blaze_implied_bounds`, `blaze_anon_params` | 0 / 18, 17 | implied-bounds.md; hir-ty implied.rs | B | check: `NoMethod`, `AnnotationsNeeded` | one gate (anon params already turns on implied bounds), on in the blaze edition | S |
| `blaze_const_exprs` | 0 / 12 | ownership.md; hir-ty const_exprs.rs | B | check: const generic unsupported | as it was | S, V |
| `#[blaze::multiversion]`, `#![blaze::simd]` | 0 / 21, 1 | simd.md; hir-ty-codegen | D | catch-all; `native_lanes!` unexpanded | as it was | B |
| `#[blaze::differentiable]` | 0 / 9 | docs/future/autodiff.md | D | catch-all | parked: a library over body reflection | none |
| `#![blaze::edition = ".."]` | 0 / 3 | edition.md | B | passes, unread | manifest only (`edition = "blaze-2026.11"`) | E |
| `loop_match`, `become` (nightly) | 0, 0 | loop-match.md, tail-calls.md | B | passes; `to_typeck: become` | not extensions: Rust parity | K |
| extension crates (blaze-ext) | 0 | this page before; deleted | A | gone | comptime over reflected bodies (the old page's own further direction) | none |

## 4. The design on the item layer, by kind

**A: analyses.** One record per monomorphizable item, the **effect summary**: a row of effect bits
(panic, alloc, block, diverge, io, sync, suspend, positions, unbounded), the locks taken and
held-across-wait edges, globals read, and whether it may drop a linear value; keyed by the body's
digest plus the digests of the summaries of what it calls (generic callees by their summary over
the bound, the instance's at build). An SCC of calls settles from bottom in one task. A check (an
attribute's rule) is a **finding record** keyed like a lint finding (item digest, rule set, scope)
with a read list of the callee summary digests it used. The summary changes only when the body's
calls or operations change; a summary that comes out byte-equal stops the walk (early cutoff), so a
one-line edit re-runs one body, one summary, and the callers whose input summary changed (usually
none). Floor: one body lowered, one summary, one finding check. `linear` and `consumes` read only
callee *signatures* (one body); `sealed`, `specialization`, `implied bounds`, `where const` are
facts on trait and impl records the solver reads; `trust`, `capabilities`, `canonical_cbor` are a
crate-level rule over the summaries (a reduction of per-item records).

**B: syntax and types.** The outline must take the crate's extension bits: the bits are in the key
of `FileItems` (as the old `EditionedFileId` held them), read from the crate root's attributes by
the units stage, so turning a bit on re-outlines that crate's files once. Lower learns `for
const`, `x.[f]`, `const static`, records, unions, views, `&uniq`; the checker learns pattern types,
`uN`, unions, records, views (a borrow of named fields) and field binders for `for const`
(checked once, generically); the solver learns `where const` (a goal settled by the evaluator),
const expressions, implied bounds, specialization. **Comptime is a task**: an evaluation keyed by
the code identity of what it runs and its arguments (type arguments by digest), its result a
`ConstRecord`, its read list the type and layout records it reflected on. blaze-consteval has the
machine (work stack, provenance, `Settle`, `Need`) and lacks: type values (a `Ty` as a constant),
the reflection intrinsics (`fields_of`, `variants_of`, `fn_info`, `code_id`, marks, `TypeDesc`),
unrolling per instance at build, and code produced at compile time (`type_of!`, built structs).
Floor after a one-line edit: the evaluations whose read list holds the edited item's record.

**C: proofs.** `theorem` and verify clauses: a Lean term per function from its lowered body, keyed
by the body's digest and its callees' terms; the proof result is a record, so an unchanged body is
never re-proved. Contract tests: a generated test keyed like any test (test-selection.md).

**D: code generation.** `multiversion` and `simd`: one code record per level, keyed with the level;
`memo`/`cache`: a library over `code_id` (no compiler work beyond reflection).

## 5. Port order

1. **blaze checks blaze** (the smallest set): list `blaze` as a tool so its attributes are inert,
   not refused (blaze-scope's `CallState::Inert`; the ladder's catch-all goes); evaluate `where
   const` (3 impls in blaze-std, today silently true). Nothing else in the extension list stands
   between the new layer and blaze's crates: their refusals are Rust (section 2).
2. Then six lanes, independent:
   - **Effects** (L, needs a lane): the summary record and the row; noalloc, nonblocking, nopanic,
     pure, expect, sans_io, capabilities, renders_positions, the role and lock words. Check: the 11
     `a_` effect fixtures refuse with a finding, blaze's crates give 0 findings.
   - **Ownership** (K): linear, consumes, holds, early drop, handles, norefcount, program lifetime.
     Check: their fixtures refuse; `a_consumes` passes.
   - **Comptime** (V with K): type values, reflection intrinsics, `for const`, `x.[f]`, `const
     static`, `type_of!`, marks, const expressions. Check: the `b_comptime_*`, `b_reflect`,
     `b_type_of`, `b_where_const_false` fixtures; library/core's reflect.rs evaluated.
   - **Syntax bits** (E with K): the bits in the outline's key; unions, records, uniq, views, pattern
     types, `uN`, bits. Check: those eight fixtures pass every stage.
   - **Traits** (S): sealed, specialization, implied bounds and anonymous parameters. Check: four
     fixtures.
   - **Proofs** (needs a lane): theorem, clauses, contract tests, invariant, unreachable!,
     checkfree, implicit overflow. Check: the `c_` fixtures; blaze's 16 theorems hold.
3. Codegen (B, after the build path): multiversion, simd, tail calls.

## 6. What bstd makes unnecessary (2026-11-16)

The owner, 2026-11-16: on bstd an effect is a value you are handed (an allocator, `Io` or a
narrower capability, `&Cx` for a wait), and under the blaze edition Rust's `std` has no name
(std.md, "The blaze edition: opting in"). So a property the signature already states needs no
attribute, no summary and no propagation (rules 30, 32). Everything below is a search or a file
read on 2026-11-16 in /Volumes/Projects/blaze-fresh-1115. Verdicts were written as proposals.

**Decided (coordinator, 2026-11-16 02:40, on the owner's direction "remove a lot of things ...
since a lot of people will use bstd"):** the cut list below is accepted as written. Removed, with a
fix that deletes the attribute: `noalloc`, `nonblocking`, `sans_io`, `capabilities`, the `effects`
gate, `pure`, the role words, `may_suspend`, `renders_positions`, `holds`, `program_lifetime`,
`canonical_cbor`; replaced by a library: lock order (`RankedLock`), the narrowed capability,
`policy` as a mark. Kept: `nopanic` with `diverge` as one two-bit summary per function (the only
propagation that stays), `linear` and `consumes` checked in one body, `trust` moved to the
manifest, `theorem`. The earlier decision of one effect row spelled `expect(..)` is withdrawn. The
three holes the audit found become edition rules, each checked per item with no call graph: no
global heap under the blaze edition (allocation only through an allocator value), no mutable
statics, no lock guard or non-`Send` value live across a wait. They are owed, behind tempo: until
they are built the derived properties are not enforced, and the docs must not say they are.

### 6.1 Is it enforced by construction, and where is the hole

The argument holds for a *checked* program (run.md: no `unsafe`, foreign code or Rust std that
`main` reaches outside crates trusted by digest; anything else is *ambient* and runs only with
`--ambient`). It is local by induction: if every body of the edition is checked alone against
named sources (a global-heap constructor, `unsafe`, a foreign call, a mutable static), the
transitive property follows with no call graph. Propagation is needed only where the property
is not in the signature and not local.

| effect | what enforces it under the edition | the hole today |
|---|---|---|
| alloc (global heap) | no allocator or frame-bearing value (`&Cx` has `cx.frame()`) in the signature means no handed allocator | **not enforced**. The prelude's `Vec`/`Box` are alloc's types, so `Vec::new()`, `Box::new` compile; `extern crate alloc` gives `vec!`/`format!`. `blaze::std_use` (the global-heap rows) lived in blaze-lints, deleted in 8e87a017ff; the new blaze-lint's `allocs` rule is text only and not wired to the edition; the S4 refuser `#[global_allocator]` is not built (`rg '// alloc:' crates/blaze-std/src` outside tests: 161 lines; bstd's lib.rs:90 "`alloc`'s global allocator is still underneath"); S5g not built. Closing it is local: a per-item rule on resolved global-heap paths in edition crates, then the refuser at run time |
| block | a wait is `Cx::wait`/`wait_for` (crates/blaze-std/src/cx.rs:426, 473), so no `&Cx` means no wait; core has no blocking primitive | a spin on an atomic handed in (`while !flag.load(..) { spin_loop() }`) needs no `Cx`: that is non-termination, covered by `diverge` below. A blocking syscall needs `unsafe`/foreign: ambient |
| io | no `Io` (or a narrower capability) in reach; `std::fs`, `std::env`, `println!` have no path | only through `unsafe` or foreign code, which makes the program ambient (run.md). The checked/ambient classification itself (S2) is not built on the new layer |
| sync | a sync is a method of a disk capability; an answer path handed one without it cannot sync (bstd `narrow.rs`) | none for a checked program |
| hidden state | std.md "No global mutable state": a `static` with interior mutability, `static mut` and thread-locals are refused in the edition (`Freeze` is the test) | **not enforced** on the new layer (the old `global_state` effect is gone). Local per item: a `static`'s type must be `Freeze` |
| lock order, lock across wait | bstd's `RankedLock<T, R>` (crates/blaze-std/src/sync/ranks.rs): taking a lock not above the token's rank is a compile error; a guard borrows the task's `Order`, the one way to `&Cx` | ranks.rs says it: code that also holds the bare `cx` can wait with a guard live "by convention until blaze's check". That check is local to one body: a guard or a non-`Send` value live across a call that takes `&Cx` |
| suspend | a park happens only inside a call taking `&Cx`: the signature says "may suspend" | same local check as above (rule 27) |
| positions | the new layer's records hold item-relative ranges by type (`BodyRange`, rule 71, 76); a file position is made only to print | none on the new layer; all 43 uses are in the old front end |
| panic | **not in the signature** (a `Result` return does not exclude a panic) | needs the callees: propagation |
| diverge | loops are local to a body; recursion is a cycle of calls | needs the call graph for cycles: propagation |

### 6.2 The uses, by where they live

`rg -n '^\s*#!?\[blaze::NAME\b'` over crates/ and library/, without `tests/`, `tests.rs`,
`*_tests.rs` and test-utils, minus the one fixture each in crates/blaze-real/extensions:

| attribute | total | in bstd itself | in crates that depend on bstd (blaze-cli, blaze-h1-net, blaze-ladder) | in crates on core/alloc/std |
|---|---|---|---|---|
| noalloc | 542 | 294 | 0 | 248: blaze-sys 97, -sql 63, -pg 57, -sim 28, -graph 1, -ids 1, -sqlite 1 |
| nonblocking | 520 | 343 | 0 | 177: blaze-sql 63, -pg 57, -sim 49, -graph 7, -sqlite 1 |
| nopanic | 400 | 234 | 0 | 166: blaze-sys 97, -sim 25, -sql 22, -pg 19, -sqlite 3 |
| sans_io | 272 | 4 | 0 | 268 in 58 crates: blaze-remote 17, -scope 16, -http 14, -cas 14, -sql 14, -keyed 12, -audit 12, -core 11, -watch 11, -program 10, the rest at most 9 |
| pure | 260 | 117 | 0 | 143: blaze-pg 55, -sim 41, -sql 31, -kernel 11, -ids 4, -sqlite 1 |
| renders_positions | 43 | 0 | 0 | 43, all old front end: hir-ty-codegen 29, blaze-items 6, hir-ty-mir 4, hir-ty, hir-ty-borrowck, hir-ty-pure, blaze-item-hash 1 each |
| consumes | 32 | 26 | 1 (h1-net) | 5: blaze-image 2, hir-ty 2, hir-ty-borrowck 1 |
| trust | 21 | 0 | 0 | 21, one per crate |
| linear | 17 | 13 | 1 (h1-net) | 3: blaze-image, hir-ty, hir-ty-borrowck |
| theorem | 16 | 0 | 0 | 16: blaze-core |
| policy, holds | 9, 2 | 9, 2 | 0 | 0 |
| canonical_cbor | 1 | 0 | 0 | 1: blaze-report |

What blocks a move: **no blaze crate is on the edition** (only harness/v2/smoke/edition and
harness/v2/versus/bstd-hello are), because cargo builds blaze (D25) and refuses the edition key;
three crates name bstd as a dependency. bstd itself is trusted code (261 lines naming `unsafe`):
the capability argument binds its *callers*, not its own bodies, so its 988 marks assert facts
about a trusted implementation that its signatures already give its callers coarsely
(`Cx::deadline(&self)` takes `&Cx`, so by signature it "may wait"). blaze-sys is below bstd (the
syscall layer, `core` only): its noalloc is true by construction (it links no `alloc`), its
nopanic is a real claim. blaze-sim, -graph, -kernel are std test or old-layer crates; -pg, -sql,
-sqlite are `no_std` + alloc libraries that could take bstd's allocator types after D25 ends.

### 6.3 The cut list (proposals; default remove)

| feature | verdict | what holds it instead | uses | the fix |
|---|---|---|---|---|
| noalloc | DERIVE | no allocator in the signature; a per-item rule on global-heap paths in edition crates; the refuser | 542 | delete the attribute |
| nonblocking | DERIVE | no `&Cx`/`Io`; spins are `diverge` | 520 | delete |
| sans_io, capabilities, `effects` gate | DELETE | no capability in reach; `std` unnameable; "this module is sans-IO" derived per item from signatures | 272, 0, 0 | delete |
| pure | DERIVE | no capability parameter, `Freeze` arguments by reference, `Freeze` statics, checked program | 260 | delete |
| answers_request, bounded_turn (its io/block/sync/lock part) | LIBRARY | a narrowed capability (bstd `narrow.rs`) | 0, 0 | delete |
| lock_checked, lock order | LIBRARY | `RankedLock<T, R>` | 0 | delete |
| may_suspend, guard across a wait | DERIVE + KEEP local | "takes `&Cx`" is the suspend fact; one body check: guard or non-`Send` value live across such a call | 0 | delete |
| renders_positions | DELETE | item-relative ranges by type | 43 (old front end, gone at the switch) | delete |
| expect, may | KEEP `expect` for two words only; `may` refused | | 0 | |
| nopanic, `diverge` | KEEP, propagated | the only two not local and not in the signature: one 2-bit summary per function (crate blaze-effects) | 400 | `nopanic` becomes `expect(diverge)`-style rows later |
| linear, consumes | KEEP local | callee signatures + one body (ownership lane) | 17, 32 | none |
| holds, program_lifetime | DERIVE | reflection (`PhantomData<T>`, no `Drop`, no parameter) | 2, 0 | delete |
| policy | LIBRARY | a mark read by a library lint | 9 | respell |
| canonical_cbor | DELETE | the encoder sorts by reflection | 1 | delete |
| trust | KEEP, moved | the manifest's trust list: the ambient boundary | 21 | move the grant |
| theorem, contracts | KEEP | proofs | 16 | none |
| B, C, D rows (syntax, traits, comptime, codegen) | unaffected | not effects | | |

**Code not on bstd** (every blaze crate while D25 holds, and Rust-edition crates): a DELETE or
DERIVE attribute there is inert (accepted, recorded, unchecked); the fix that removes it runs when
the crate moves onto the edition. No interim check. blaze's own crates lose nothing they have:
nothing has checked these words since blaze-lints and hir-ty-pure's lints were deleted
(8e87a017ff); they lose the general effect machine that would have checked 2,000 marks, and keep
only `nopanic`/`diverge` checked, until each crate moves.
