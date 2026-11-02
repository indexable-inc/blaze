# Requests of mem-db-infer

hir-ty (all but `src/next_solver/`) onto `crates/blaze-mem`. Design and the rules every change
follows (R1 to R8): `docs/v2/plan/lanes/mem/db-infer.md`. The paths belong to lane db (db-ty and
its children; frontend-db's children change `db` parameters) and, for `method_resolution.rs`,
`method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout.rs`, `layout/`,
`drop.rs`, lane items; none is done, so these are requests. The file-by-file changes are in the
part files:

| part | files of `crates/hir-ty/src/` | request file |
|---|---|---|
| this file | `Cargo.toml`, `lib.rs`, `db.rs`, `tests.rs`, `tests/budget.rs` | `v2/requests/mem-db-infer.md` |
| ctx | `infer.rs`, `infer/{unify,columns,persist,fallback,diagnostics,ambiguity,at_error,mutability,opaques,path,autoderef}.rs` | `v2/requests/mem-db-infer-ctx.md` |
| expr | `infer/{expr,pat,coerce,callee,cast,op,place_op}.rs` | `v2/requests/mem-db-infer-expr.md` |
| closure | `infer/closure.rs`, `infer/closure/` | `v2/requests/mem-db-infer-closure.md` |
| lower | `lower.rs`, `lower/`, `generics.rs`, `utils.rs`, `variance.rs`, `variance/`, `dyn_compatibility.rs`, `builtin_derive.rs`, `effects.rs` | `v2/requests/mem-db-infer-lower.md` |
| method | `method_resolution.rs`, `method_resolution/`, `autoderef.rs`, `traits.rs`, `specialization.rs`, `opaques.rs`, `drop.rs`, `inhabitedness.rs`, `representability.rs` | `v2/requests/mem-db-infer-method.md` |
| eval | `layout.rs`, `layout/`, `consteval.rs`, `mir.rs`, `mir/`, `intrinsic.rs`, `target_feature.rs`, `target_feature/`, `lang_items.rs`, `primitive.rs`, `solver_errors.rs`, `evict.rs` | `v2/requests/mem-db-infer-eval.md` |

Order to apply: 1 to 3 here first (they add the dependencies, the feature and the tags every
part uses), then ctx (it changes `InferenceContext`'s and `columns.rs`'s types the others name),
then the other parts in any order, then 4 (the tests) last.

## 1. lane db: hir-ty's dependencies (`crates/hir-ty/Cargo.toml`)

`[dependencies]`, after `blaze-reclaim.workspace = true`:

```toml
blaze-mem.workspace = true
hashbrown = { workspace = true, features = ["nightly"] }
```

`blaze-mem` is solver-sync-glue's request 10 (`v2/requests/solver-sync-glue.md`): one line, not
two. `hashbrown` is a root workspace dependency (`Cargo.toml:294`); its `nightly` feature is
mem-db's root request 3, which makes `HashMap<K, V, S, A>`'s `A` `core::alloc::Allocator`, so
`&Frame` is one. If root request 3 turns `nightly` on in the workspace entry, the line is
`hashbrown.workspace = true`.

Add a `[dev-dependencies]` section:

```toml
[dev-dependencies]
blaze-alloc-count.workspace = true
test-fixture.workspace = true
```

`blaze-alloc-count` is solver-sync-glue's request 6 (one line, not two). `test-fixture` depends
on base-db, hir-expand and span, not on hir-ty, so this is no cycle. Run the manifest lint after.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a (21:16): `hashbrown.workspace = true` (the workspace entry has `nightly` on); covers solver-sync-glue 6 and 10's `Cargo.toml` lines (one line each). Manifest lint clean.

## 2. lane db: `crates/hir-ty/src/lib.rs`

Before (line 4):

```rust
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
```

After:

```rust
#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api)]
```

`Vec<T, &Frame>`, `Box<T, &Frame>` and hashbrown's `A` need it (contract 2 of
`docs/v2/plan/lanes/mem/db.md`); blaze builds with its pinned nightly.

After the module list (after `pub mod traits;`, line 47):

```rust
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
```

The allocator is solver-sync-glue's request 6: one static, not two (if it is in already, only
`mod tests;` is new).

New file `crates/hir-ty/src/tests.rs`:

```rust
//! Tests of hir-ty as a whole: what its queries allocate.

mod budget;
```

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a (21:26): as written; the one `#[global_allocator]` (solver-sync-glue 6's too).

## 3. lane db: the tags, `crates/hir-ty/src/db.rs`

After the `use` block (after line 41, `};` closing `use crate::{..}`):

```rust
/// What hir-ty's inference, lowering, method resolution, layout, const eval and trait
/// solving allocate (docs/v2/plan/lanes/mem/db.md, contract 3).
pub(crate) const INFER_TAG: blaze_mem::Tag = blaze_mem::Tag::Infer;
/// What hir-ty's MIR queries allocate.
pub(crate) const MIR_TAG: blaze_mem::Tag = blaze_mem::Tag::Mir;
```

These are the lines that set every hir-ty query's tag: each `#[salsa::tracked(..)]` fn of
hir-ty (outside `next_solver/`, which mem-db-solver files) adds `mem_tag = crate::db::INFER_TAG`,
the MIR ones `mem_tag = crate::db::MIR_TAG`, once mem-db-salsa's option exists
(`v2/requests/mem-db-salsa.md`; asked by tmux to take a path to a `Tag` constant). The one
tracked fn of `db.rs` itself:

Before (`db.rs:551`):

```rust
#[salsa::tracked(returns(deref))]
fn closure_exprs(db: &dyn HirDatabase, owner: ExpressionStoreOwnerId, _: ()) -> Box<[ExprId]> {
```

After:

```rust
#[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
fn closure_exprs(db: &dyn HirDatabase, owner: ExpressionStoreOwnerId, _: ()) -> Box<[ExprId]> {
```

(`db: &dyn HirDatabase` as it is when applied: frontend-db changes it to `&Db`.) The table of
every tracked fn with its line and tag is in "Tags of every query" below, from the parts.

The `HirDatabase` trait's MIR methods (`mir_body` and the rest, `db.rs:55` to `211`) are
implemented in hir-ty-mir (`hir_ty_mir::provide`, frontend-db's `hir_ty::providers`): their tag
is `MIR_TAG` there, which is mem-db-checks' or codegen's to file for hir-ty-mir, not here.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a (21:22): constants as written; `closure_exprs` gets `mem_tag = Infer`, not `crate::db::INFER_TAG`: mem-db-salsa M2 (not yet in `vendor/salsa*`) takes a bare `blaze_mem::Tag` variant (`$zalsa::MemTag::$mem_tag`). Every hir-ty tracked fn uses that form.

## 4. lane db: the budget tests, `crates/hir-ty/src/tests/budget.rs`

New file. It holds: the warm re-check of unchanged bodies to zero allocations; a cold `infer` of
each body to its owned result's allocations alone (counted as what cloning the result makes:
each owned heap block of the result is one allocation of its clone, interned types are
reference counts); the frame's peak to linear in the body's size.

```rust
//! What inference allocates (docs/v2/plan/lanes/mem/db-infer.md, "Budget tests"): nothing on a
//! re-check of bodies whose inputs did not change, its owned result alone on a run whose
//! dependencies are all memoized, and frame memory linear in the body.

use base_db::{Db, all_crates};
use blaze_alloc_count::{assert_at_most, measure};
use blaze_mem::{Tag, frame};
use hir_def::{AssocItemId, DefWithBodyId, LoweringMode, ModuleDefId, nameres::crate_def_map};
use salsa::{Database, Durability};
use test_fixture::ChangeFixture;

use crate::{InferenceResult, infer::infer_body_in};

const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, add, mul, fn, option, deref
struct Point { x: u32, y: u32 }

impl Point {
    fn new(x: u32, y: u32) -> Point { Point { x, y } }
    fn sum(&self) -> u32 { self.x + self.y }
}

enum Shape { Dot(Point), Line(Point, Point) }

fn area(shape: &Shape) -> u32 {
    match shape {
        Shape::Dot(p) => p.sum(),
        Shape::Line(a, b) => {
            let d = a.sum() + b.sum();
            d * 2
        }
    }
}

fn first(points: &(Point, Point)) -> Option<&Point> {
    if points.0.x > points.1.x { Some(&points.0) } else { None }
}

fn main() {
    let p = Point::new(1, 2);
    let shapes = (Shape::Dot(p), Shape::Line(Point::new(3, 4), Point::new(5, 6)));
    let total = area(&shapes.0) + area(&shapes.1);
    let add = |a: u32| a + total;
    let _ = add(7);
    let pair = (Point::new(1, 1), Point::new(2, 2));
    let _ = first(&pair).map(|it| it.sum());
}
"#;

fn db_of(fixture: &str) -> Db {
    let mut db = Db::for_tests();
    ChangeFixture::parse(fixture).change.apply(&mut db);
    db
}

/// Every body of the fixture's own crates (not minicore's): functions, consts and statics in
/// modules and impls.
fn bodies(db: &Db) -> Vec<DefWithBodyId> {
    let mut bodies = Vec::new();
    for &krate in all_crates(db).iter() {
        if !krate.data(db).origin.is_local() {
            continue;
        }
        let def_map = crate_def_map(db, krate);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            for def in scope.declarations() {
                match def {
                    ModuleDefId::FunctionId(it) => bodies.push(it.into()),
                    ModuleDefId::ConstId(it) => bodies.push(it.into()),
                    ModuleDefId::StaticId(it) => bodies.push(it.into()),
                    _ => {}
                }
            }
            for it in scope.impls() {
                for &(_, item) in it.impl_items(db).items.iter() {
                    match item {
                        AssocItemId::FunctionId(it) => bodies.push(it.into()),
                        AssocItemId::ConstId(it) => bodies.push(it.into()),
                        AssocItemId::TypeAliasId(_) => {}
                    }
                }
            }
        }
    }
    assert!(!bodies.is_empty(), "the fixture has bodies");
    bodies
}

fn infer_all(db: &Db, bodies: &[DefWithBodyId]) {
    for &body in bodies {
        let _ = InferenceResult::of(db, body);
    }
}

#[test]
fn a_recheck_of_unchanged_bodies_allocates_nothing() {
    let mut db = db_of(FIXTURE);
    let bodies = bodies(&db);
    infer_all(&db, &bodies);
    // The first re-check after a write warms what verification keeps per worker; the second is
    // the steady state.
    db.synthetic_write(Durability::LOW);
    infer_all(&db, &bodies);
    db.synthetic_write(Durability::LOW);
    assert_at_most(0, "re-checking the inference of unchanged bodies", || {
        infer_all(&db, &bodies)
    });
}

#[test]
fn a_cold_infer_allocates_its_result_alone() {
    let db = db_of(FIXTURE);
    let bodies = bodies(&db);
    // Every query inference asks is memoized and every type it makes interned: what the run
    // below allocates is its own.
    infer_all(&db, &bodies);
    for &body in &bodies {
        frame::task(Tag::Infer, |frame| {
            let (result, used) =
                measure(|| infer_body_in(&db, body, None, LoweringMode::Analysis, frame));
            let (copy, owned) = measure(|| result.clone());
            assert!(copy == result, "a run gives the memoized result: {body:?}");
            assert!(
                used.allocations <= owned.allocations,
                "{body:?}: inference made {} allocations ({} bytes), its result owns {} ({} bytes)",
                used.allocations,
                used.bytes,
                owned.allocations,
                owned.bytes,
            );
        });
    }
}

/// `fn chain() -> u32` of `lets` statements, each adding one to the last.
fn chain(lets: usize) -> String {
    let mut text = String::from("//- minicore: sized, copy, add\nfn chain() -> u32 {\n    let x0: u32 = 1;\n");
    for at in 1..lets {
        text.push_str(&format!("    let x{at} = x{} + 1;\n", at - 1));
    }
    text.push_str(&format!("    x{}\n}}\n", lets - 1));
    text
}

/// The frame bytes a run of the inference of `chain(lets)` uses at most.
fn peak_of_chain(lets: usize) -> usize {
    let db = db_of(&chain(lets));
    let bodies = bodies(&db);
    assert_eq!(bodies.len(), 1);
    infer_all(&db, &bodies);
    frame::task(Tag::Infer, |frame| {
        let _ = infer_body_in(&db, bodies[0], None, LoweringMode::Analysis, frame);
        frame.peak()
    })
}

#[test]
fn frame_use_is_linear_in_the_body() {
    let small = peak_of_chain(200);
    let large = peak_of_chain(800);
    assert!(small > 0, "inference's temporaries are frame values");
    // Four times the statements: at most four times the bytes and the fixed part again.
    assert!(large <= 4 * small + small / 2, "frame peak {small} bytes for 200 lets, {large} for 800");
}
```

Notes for whoever applies it:
- `infer_body_in(db, def, inspect, lowering_mode, frame)` is the body of inference that
  mem-db-infer-ctx's A2 moves out of `infer_query_with_inspect` (`pub(crate)` in `infer.rs`); the
  tests call it in a `frame::task` of their own, so `frame.peak()` is the run's and the run is not
  salsa's (it always executes). Its `db` is `&Db` (frontend-db's), which `&db` is.
- The first test needs mem-db-salsa's zero-allocation deep verify and mem-db-def's hit paths as
  well as this part's: it holds all of them for inference's dependencies.
- The second holds mem-db-solver's part too (inference variables, goals and the search graph in
  its recycled per-worker storage, warm after the first run).
- `peak()` is the most the frame's cursor was since the task began (`frame.rs:88`).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-mem-infer-a (21:26): `src/tests/budget.rs` as written. Needs `infer::infer_body_in(db, def, inspect, lowering_mode, frame)` from ctx A2 (db-mem-infer-a-ctx applies it) and, to pass, mem-db-salsa M1/M2.

## 5. lane db: one definition of the frame map and set, `crates/hir-ty/src/lib.rs`

(5 to 7 and the two sections after them are new since pass 16: written once every part was done.)

Three parts define the same aliases (ctx C2 in `infer/columns.rs` with the constructors
`frame_map`/`frame_set`, method S1 in `method_resolution.rs`, lower 1 in `lower.rs`). Apply them
once, in `lib.rs`, after the `use` block:

```rust
use blaze_mem::Frame;
use rustc_hash::FxBuildHasher;

/// A map of a query run's temporaries, in its frame (docs/v2/plan/lanes/mem/db-infer.md, R3).
pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
/// A set of a query run's temporaries, in its frame.
pub(crate) type FrameSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;

pub(crate) fn frame_map<K, V>(frame: &Frame) -> FrameMap<'_, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

pub(crate) fn frame_set<T>(frame: &Frame) -> FrameSet<'_, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}
```

and in place of each part's own definition: ctx C2 (`infer/columns.rs`) writes
`pub(crate) use crate::{FrameMap, FrameSet, frame_map, frame_set};`, method S1
(`method_resolution.rs`) and lower 1 (`lower.rs`) write `pub(crate) use crate::{FrameMap, FrameSet};`
(lower keeps its `exact_thin` there). Every path the parts use (`columns::FrameMap`,
`crate::method_resolution::FrameMap`, `crate::lower::FrameSet`) still resolves. `blaze_mem::Frame`
and `blaze_mem::frame::Frame` are the same type (`crates/blaze-mem/src/lib.rs:59`). If `lib.rs`
already imports `FxBuildHasher` or `Frame` when this is applied, drop the repeated `use`.

> forwarded to db (db-ty-next2) (requests, pass 21; told by tmux).
> applied by db-mem-infer-a (21:26): db-ty handed mem-db-infer-ctx and these to db-mem-infer-a. Definitions after `pub mod closure_analysis`; `FxBuildHasher` joins the existing `rustc_hash` import, `use blaze_mem::Frame;` after `base_db`. columns C2 re-exports them (db-mem-infer-a-columns).
> outcome (db-ty-next2, 21:20): offered to db-mem-infer-a along with ctx A, since 7 puts it
> before A. If db-mem-infer-a does not take it, db-ty applies it when 7's step 1 is due (after
> mem-db-salsa's `mem_tag` and `frame::current` exist).

## 6. lane db: where two parts' entries meet

Each is a site that two parts' entries both change, or that one part's signature change reaches
in another part's file without an entry of that part. The resolution is written out.

1. **`infer/closure.rs` `lower_closure_params` (about line 1020):** lower 22 makes
   `lower_closure_params` fill a caller's list and return the bound vars; closure C2 keeps the old
   call and turns the `collect` into an iterator. Both, together, after:

   ```rust
        let mut supplied = Vec::with_capacity_in(decl_inputs.len(), self.frame);
        let bound_vars =
            self.with_ty_lowering(|ctx| ctx.lower_closure_params(decl_inputs, &mut supplied));
        let supplied_arguments = supplied.into_iter().map(|input| match input {
            // The solver takes no type with a lifetime bound outside it.
            Some(ty) if ty.has_escaping_bound_vars() => self.table.insert_type_vars(ty),
            Some(ty) => self.process_user_written_ty(ty),
            None => self.table.next_ty_var(closure_expr.into()),
        });
   ```

   and the rest as closure C2 has it (`interner.mk_fn_sig(supplied_arguments, ..)`).
   `supplied` borrows the frame, not `self`, so the map's `self.table` borrows don't conflict.
2. **`infer.rs` `resolve_all`'s closure-data loop:** ctx A8 keeps the `shrink_to_fit` calls on
   the closure data; closure X3 replaces that loop, since closure A4 builds each list once at its
   length. Apply ctx A8, then closure X3 over the same loop.
   > applied by db-mem-infer-a-ctx (21:36): ctx A8, then closure X3, in that order.
3. **`infer/coerce.rs` `reified_sig` (line 963 now):** lower 12 adds `frame: &Frame` last to
   `bind_elided_lifetimes`; expr has no entry for its caller. Before:
   `crate::lower::bind_elided_lifetimes(self.db, function, sig)`, after:
   `crate::lower::bind_elided_lifetimes(self.db, function, sig, self.frame)`. `reified_sig` takes
   `&self` of an `InferenceContext<'db, 'f>`, so `self.frame` is there.
4. **`method_resolution/confirm.rs` (line 463 now):** lower 14a adds `frame: &Frame` last to the
   free fn `substs_from_args_and_bindings`; method has no entry for this caller. The argument
   list reborrows `self.ctx` mutably (`&mut LowererCtx { ctx: self.ctx, .. }`), so the frame is
   read first. Before:

   ```rust
        substs_from_args_and_bindings(
            self.db(),
   ```

   After:

   ```rust
        let frame = self.ctx.frame;
        substs_from_args_and_bindings(
            self.db(),
   ```

   and after the last argument (`&mut LowererCtx { .. },`) a new last argument `frame,`.
5. **`method_resolution.rs` `with_method_resolution` (about line 200):** method P1 adds
   `frame: self.frame` to the `MethodResolutionContext` literal and ctx M1 turns
   `traits_in_scope` into `TraitsInScope::Inference(self.get_traits_in_scope())`. ctx M1's "after"
   already has both; apply method P1's field, then ctx M1. `TraitsInScope` is only iterated
   (`probe.rs:1220`), which its `IntoIterator` covers; `crates/hir`'s two literals take
   `TraitsInScope::Caller` (ctx M1).
6. **`remove_deferred_call_resolutions`:** closure's "Notes for other parts" says it returns a
   global `Vec`; expr X4 (taken by ctx A11) makes it a frame `Vec`. Its one caller
   (`infer/closure/analysis.rs:532`, a `for` loop calling `resolve(self)`) compiles with either:
   the list borrows the frame, not `self`.
7. **`autoderef.rs` steps:** method P6 makes the default `Steps` an `ArrayVec` of 20
   (`AutoderefSteps<'db>`); ctx's `adjust_steps_as_infer_ok` (expr X2) reads `self.steps()` with
   `len`, `is_empty` and `iter`, which the `ArrayVec` gives through `Deref<Target = [_]>`. No
   change beyond the two entries.
8. **`method_resolution.rs` `crates_containing_incoherent_inherent_impls` (line 1170 now):**
   mem-db-def's B6 (`v2/requests/mem-db-def.md`) memoizes `Crate::transitive_deps`, which then
   returns `&[Crate]`; method's entry for this query (I6) still calls `.into_iter()` on the old
   owned `Vec`. With B6, method's "after" keeps its frame list (a `filter` has no exact length, so
   a direct `collect` into `Box<[Crate]>` would grow and shrink, against R4) and changes one line,
   before:
   `lang.extend(krate.transitive_deps(db).into_iter().filter(|krate| krate.data(db).origin.is_lang()));`
   after:
   `lang.extend(krate.transitive_deps(db).iter().copied().filter(|krate| krate.data(db).origin.is_lang()));`
   (If B6 lands without method's entry, line 1170 is mem-db-def's form,
   `.iter().copied().filter(..).collect()`.) Method's "Left as is" note on `transitive_deps`
   (its line 2892) no longer applies once B6 is in: a hit is a memo read.
9. **dedup-cursor-front's `frozen.rs` change** (below, applied in pass 18) touches `part` and the
   writer only; method I5 (`trait_impls`, `inherent_impls` readers, lines 80 to 122) quotes code
   it left as it was, so both apply.

> applied by db-mem-infer-b-impls (22:05): item 8 with B6 in (`transitive_deps` returns `&[Crate]`): `crates_containing_incoherent_inherent_impls` keeps method I6's frame list with the `.iter().copied().filter(..)` line, result exact. Item 9: method I5's readers applied over dedup's cursor change, which stays.

> forwarded to db (db-ty-next2) (requests, pass 21; told by tmux).
> outcome (db-ty-next2, 21:20): proposed to db-mem-infer-a that each lane db applier applies
> each resolution here together with the part it is applying, at that part's sites. Items 4, 5
> and 8 are in items' `method_resolution*` files. db-ty applies any item that is still open when
> the parts are in.
> applied by db-mem-infer-a (21:50): item 3 as written (`coerce.rs` `reified_sig`; lower 12 is in `lower.rs`). Item 2 by db-mem-infer-a-ctx (its line under ctx A8/closure X3). Items 4, 5, 8 are db-mem-infer-b's (method); 7 and 9 need no edit.
> applied by db-mem-infer-a-closure (21:52): item 1 as written in `infer/closure.rs` (lower 22's `lower_closure_params(&mut impl Extend<Option<Ty>>) -> bound vars` agrees; told db-mem-infer-b-lower). Item 6: no edit, the `analysis.rs` loop compiles with the frame `Vec`.

## 7. Order to apply

1. This file's 1 to 3 and 5 (dependencies, feature, tags, the shared aliases).
2. ctx A0 (every `InferenceContext<'db>` header, one mechanical edit across files), then the rest
   of ctx with expr X1 to X4 (ctx takes them as written).
3. lower (it changes `TyLoweringContext::new`, which ctx B4, method T1/C1 and `crates/hir`'s
   three callers follow; lower 22 lists them).
4. expr, closure, method, eval, with 6's resolutions at the sites it names.
5. This file's 4 (the tests) last.

Method I1 to I6 (the impl tables, `v2/requests/mem-db-infer-method.md`) are on hold until
requests-impls finishes its per-module rewrite of `method_resolution.rs`; the rest of method's
entries stand. The `mem_tag = crate::db::INFER_TAG` form is the one salsa M2 takes, as amended
(`v2/requests/mem-db-salsa.md`, the note after M2: a path, or a variant name).

mem-db-salsa's `mem_tag` option and lane mem's `frame::current` (mem-db's request 1) are needed
by every part; until they exist nothing here compiles, so the parts land after them.

> forwarded to db (db-ty-next2) (requests, pass 21; told by tmux).
> outcome (db-ty-next2, 21:20): agreed, this is the order. Step 2 (ctx A0 to A13) is
> db-mem-infer-a's.
> applied by db-mem-infer-a (21:52): steps 1, 2 (A10/M1 superseded by frontend-fast-method's `TraitsInScope`, ctx's lines say how), 4's expr and closure, and 5 are in; 3 and 4's method/eval are db-mem-infer-b's. Compiling still waits on mem-db-salsa M1/M2.

## Tags of every query

Every `#[salsa::tracked]` fn of hir-ty outside `next_solver/` adds
`mem_tag = crate::db::INFER_TAG`, last in its options. None is a MIR query: hir-ty's MIR queries
are `HirDatabase` methods implemented in hir-ty-mir, and `mir/held.rs`'s entry point opens its own
`task(crate::db::MIR_TAG, ..)` (eval H1). The fns, by the part that lists each with its line and
memoized result:

| file | fns | part entry |
|---|---|---|
| `db.rs` | `closure_exprs` | 3 above |
| `infer.rs` | `infer_body_query`, `infer_anon_const_body_query` | ctx A2 |
| `lower.rs` | `impl_trait_of_impl`, `type_for_const_with_diagnostics`, `type_for_static_with_diagnostics`, `type_for_type_alias_with_diagnostics`, `impl_self_ty_of_impl`, `const_param_types_with_diagnostics`, `field_types_of_variant`, `supertraits_info`, `resolve_type_param_assoc_type_shorthand`, `type_alias_bounds_with_diagnostics`, `generic_predicates_of`, `trait_environment_query`, `const_conditions_query`, `generic_defaults_with_diagnostics`, `fn_sig_for_fn`, `elided_lifetimes`, `fn_sig_for_struct_constructor`, `fn_sig_for_enum_variant_constructor` | lower 10, 25 |
| `lower/batch.rs` | `crate_field_types`, `crate_impl_self_tys`, `crate_impl_traits`, `crate_impl_predicates` | lower 15e |
| `variance.rs` | `variances_of_query`, `ctor_variances_query` | lower 18, 25 |
| `dyn_compatibility.rs` | `dyn_compatibility_of_trait_query`, `generics_require_sized_self` | lower 25 |
| `builtin_derive.rs`, `effects.rs` | `predicates`, `const_param_env_query` | lower 21, 25 |
| `method_resolution.rs` | the 25 of `InherentImpls`, `TraitImpls` and the free queries | method "Tracked functions" |
| `traits.rs` | `inherent_impl_coherent`, `orphan_rules_hold` | method T2 |
| `specialization.rs`, `opaques.rs`, `drop.rs`, `representability.rs` | `specializes_query`, `impl_has_unconstrained_params`, the seven of `opaques.rs`, `destructor`, `representability`, `params_in_repr` | method "Tracked functions" |
| `layout.rs`, `layout/adt.rs`, `layout/target.rs`, `consteval.rs`, `target_feature.rs` | 14 | eval "Tracked fns of these files" |

> forwarded to db (db-ty-next2) (requests, pass 21; told by tmux).
> outcome (db-ty-next2, 21:20): each `mem_tag` goes in with its part's entry, as the table says.
> The `method_resolution.rs`, `traits.rs`, `specialization.rs`, `opaques.rs`, `drop.rs` and
> `layout*` rows are items' files; `target_feature.rs` is db-unsafeck's. db-ty adds any tag in
> its own files that is still missing once the parts are in. Every one waits on
> mem-db-salsa's `mem_tag` option.
> applied by db-mem-infer-a (21:52): `db.rs` and `infer.rs` rows in, as `mem_tag = Infer` (salsa M2's bare-variant form). At 21:52 `method_resolution.rs` and `method_resolution/scope_sets.rs` (items / db-mem-infer-b) have none yet; `providers.rs` (32 fns) and `check_cache.rs` (2) are not in this table: told db-next3.

> applied by db (db-next4, 23:50): the rows the table missed. `providers.rs`'s MIR queries
> carry `mem_tag = Mir` (put in by another agent at 23:48). `check_cache.rs` `served_part` and
> `served_items` and `fn_cache.rs` `uses_fn_cache` now carry `mem_tag = Infer`. No hir-ty tracked fn
> outside `next_solver/` is left without a tag.
> applied by db-audit-mem (23:08, for db-next4): the two files the table lacks. `crates/hir-ty/src/providers.rs`: all 25 tracked fns `mem_tag = Mir`, last in the options (lines 249 to 490). They are the MIR memos hir-ty now declares and hir-ty-mir computes (`mir_body*`, `monomorphized_*`, `runtime_*`, `optimized_*`, `coroutine_witnesses_query`), so the table's MIR rule (`MIR_TAG`, as `mir/held.rs`) applies, not `Infer`. `crates/hir-ty/src/check_cache.rs`: `served_part` and `served_items` were given `mem_tag = Infer` at 23:0x by another agent (in the tree at `:183`, `:189`); db-apply-late holds nothing there.

## What the parts found

- **The solver gets no frame** (mem-db-solver's decision, `v2/requests/mem-db-solver.md`, "The
  one design decision"): `InferCtxt`, goals and the search graph use recycled per-worker
  storage. So `could_unify`/`could_unify_deeply` need no task (ctx U8), and the cold-infer test
  (4) holds the solver to zero allocations warm through that storage, not through a frame.
- **A frame can't roll back under `&Frame`** (`scope`/`reset_to` take `&mut`), so every per-node
  and per-probe temporary stays until the run ends: the frame's peak is the sum of a run's
  temporaries, about 400 KB for a 1,000-expression body (method's probes 250 KB, closure 45 KB,
  ctx 40 KB, expr 35 KB, lower 8 KB), written once and linear in the body (4's third test holds
  the linearity).
- **Frames past nesting depth 8** map and unmap a 4 GiB region per run (eval's objection);
  forwarded to mem-db for lane mem: one frame per thread with a nested task as `mark`/`reset_to`.
  With that, a probe could also run as a nested task and give its 1 KB back at once.
- **New files since the split** (`infer/sized_bound.rs`, `infer/prelude_lints.rs`): ctx A13 and
  G6 cover them.
- **Outside hir-ty**: `crates/hir`'s callers of `TyLoweringContext::new` (lower 22),
  `MethodResolutionContext` (ctx M1, method H1/H2) and `adt_unused_params` (lower 24); hir-def's
  `Resolver::traits_in_scope_into` (mem-db-def-resolve, filed); borrowck's and wfcheck's
  lowering calls (lower 22, mem-db-checks'); a frame-generic radix sort (method, lower:
  blaze-radix, lane mem's, only if a sort here shows up in a profile).

> applied: nothing to apply (notes) (requests, pass 21).

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor-front` (report `/tmp/blaze-lanes/v2-dedup-cursor-front.md`; the cursor is
`crates/blaze-format/src/cursor.rs`, API in `docs/v2/plan/lanes/dedup/cursor.md`). `hir-ty`
already depends on `blaze-format`; no `Cargo.toml` change.

`crates/hir-ty/src/method_resolution/frozen.rs`:

1. Imports: add `use blaze_format::cursor::{Put, Reader};`.
2. `fn part` becomes
   ```rust
   /// Part `which` of frozen bytes: the first is after its length.
   fn part(bytes: &[u8], which: usize) -> R<&[u8]> {
       let mut at = Reader::new(bytes);
       let first = at.bytes_u32()?;
       Ok(if which == 0 { first } else { at.take_rest() })
   }
   ```
3. At the end of the writer (the fn that ends `let inherent = enc.finish()?;`):
   ```rust
       let mut out = len.to_le_bytes().to_vec();
       out.extend_from_slice(&traits);
   ```
   becomes
   ```rust
       let mut out = Vec::with_capacity(4 + traits.len() + inherent.len());
       out.put_u32(len);
       out.put_bytes(&traits);
   ```
   (the `let len = u32::try_from(..).map_err(|_| "impls over 4 GiB")?;` line above it stays).

Same bytes written, same accepted; the two error words ("no length", "a part past the end")
become the cursor's ("at byte 0: 4 bytes wanted, 2 left"), which no test matches.

> applied: `method_resolution/frozen.rs` is items' (DONE): `part` reads through `Reader::bytes_u32`/`take_rest`, the writer puts `len` and both parts through `Put` into one `Vec` sized once (same bytes) (requests, pass 18).
