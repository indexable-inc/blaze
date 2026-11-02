# mem-db-infer-method: requests

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8). Files, all in
`crates/hir-ty/src/`: `method_resolution.rs`, `method_resolution/probe.rs`,
`method_resolution/confirm.rs`, `method_resolution/frozen.rs`, `autoderef.rs`, `traits.rs`,
`specialization.rs`, `opaques.rs`, `drop.rs`, `inhabitedness.rs`, `representability.rs`.

Owners (`docs/v2/plan/reference/crates.md`): every file is lane db's; `method_resolution.rs`,
`method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs` and `drop.rs` are lane
items' as well. Neither is done, so every change below is a request; each entry names its owners.
Lines are as of 2026-10-29 (UTC-7); other agents edit these files, so each entry quotes the
"before" code to find it by. Lane dedyn may edit these files (replacing hot `dyn` with concrete
types, enums or generics), and lane frontend-tree may move `SyntaxNode` callers to `syntax::view`;
where either changed a quoted line, apply the entry to the line as it is now and keep their
change. No entry adds a `dyn` or a `SyntaxNode` use. `db` parameters (`&Db`, `&'db Db`) are left exactly as they are; the
code holds unchanged if they go back to `&dyn HirDatabase`.

Notes and the speed-of-light bound: `docs/v2/plan/lanes/mem/db-infer-method.md` (and "Speed" at
the end of this file).

## What these files allocate, in short

| temporary | made | before | after |
|---|---|---|---|
| autoderef steps (`GeneralAutoderef::state.steps`) | per field access, call, index, method call, deref | `Vec<(Ty, AutoderefKind)>`, up to 3 global growths | `ArrayVec<_, 20>` inline (the walk stops at 20 steps), 0 allocations |
| `autoderef()`'s yielded types (hir's `Type::autoderef`) | per IDE call | `Vec<Ty>` | `ArrayVec<Ty, 21>` inline |
| probe's candidate lists (`inherent_candidates`, `extension_candidates`, `private_candidates`, `static_candidates`) | per method call and per path probed | 4 global `Vec<Candidate>`, growing | 4 frame `Vec`s |
| probe's `impl_dups` | per probe | `FxHashSet<ImplId>` | frame `hashbrown::HashSet` |
| probe's autoderef steps (`MethodAutoderefStepsResult::steps`) | per probe | `SmallVec<[CandidateStep; 3]>`, spills on a 4th step | frame `Vec` |
| `TraitValueItems::runs` | per probe | `SmallVec<[_; 4]>`, spills past 4 crates of traits in scope | frame `Vec` |
| `ProbeAllChoice::candidates` (completion) | per IDE probe | `FxIndexMap` (2 global tables) | frame `HashSet` + frame `Vec` |
| `ConfirmResult::adjustments` | per method call | `Vec` then `into_boxed_slice` (1 to 2 global) | frame `Vec` handed to `write_expr_adj` as a slice (expr's X1/X2) |
| `TraitImpls`/`InherentImpls` building (per crate, per block) | per crate, per block with impls | `FxHashMap<TraitId, OneTraitImplsBuilder>` of `SelfTyGroups` (an `FxHashMap` + a `Vec` per trait) of `(Vec, Vec)` per self type, then columns grown with `vec![0]`/`push`, radix sort scratch | frame builders (two maps, flat record `Vec`s), one counting pass, each column allocated once at its size |
| frozen impls read | per frozen crate | `Vec` per entry from `dec.many`, then the same groups | pushes straight into the frame builders |
| `trait_value_items` | per crate | global `Vec`, stable sort (global scratch), `into()` shrink | frame `Vec`, unstable sort with the position as tiebreak, exact `Box` |
| `loaded_crates` | per crate | `Vec` of dependencies, a `Vec` per module of macro calls, `FxHashSet`, `vec![krate]` grown then shrunk | frame `Vec`s/set, no per-module list, exact `Box` |
| `impls_in_bodies`, `macro_definitions` | per file | token `Vec`, brace `Vec`s, found `Vec` shrunk into the result; a `Box<[Symbol]>` per `macro_rules!` | frame `Vec`s; result exact; definitions as two flat columns |
| `macros_writing_impls` | per crate | `FxHashMap<Symbol, bool>` (a `Symbol` clone per name added), `FxHashSet`, `Vec`, `into_iter().collect()` | frame map keyed by `&Symbol`, frame set and `Vec`, exact result |
| other query results (`blanket_in_crate_and_deps`, `in_crate_for_trait`, `in_crate_and_deps_for_trait`, `in_crate_and_deps_for_trait_and_self_ty`, `in_crate_trait_value_items`, `crates_containing_incoherent_inherent_impls`, opaques' five) | per query run | `collect()`/`into()` of a growing `Vec` into `Box<[T]>` (grow, then a shrinking realloc) | frame `Vec`, then one exact `Box` |
| `rpit_hidden_types` | per function | `ArenaMap::new()` grown, then `shrink_to_fit` | `ArenaMap::with_capacity(highest index + 1)`, no shrink |
| `params_in_repr` | per ADT | `ThinVec` resized, then `shrink_to_fit` | frame `Vec` of words, one exact `ThinVec` |
| `constrained_params` (E0207) | per impl | 2 `FxHashSet`, `SmallVec` | frame sets |
| `has_drop_glue`'s visited set | per call (MIR, borrowck, codegen, lints) | `FxHashSet<Ty>` | frame set; `has_drop_glue_in(frame, ..)` for callers with a frame |
| `UninhabitedFrom::recursive_ty` | per call (match checking, MIR lowering, lints) | `FxHashSet<Ty>` | frame set; `_in(frame, ..)` forms |
| `where_predicate_must_hold` clauses | per IDE call | `Vec` | frame `Vec` in its own `frame::task` |

Not present in this tree, though the brief names them: the probe has no `unsatisfied_predicates`
(rustc's diagnostics list) and no per-step pick lists: `pick_core` walks `self.steps` and returns
the first pick, and each attempt's applicable candidates are one `ApplicableCandidates` SmallVec
(kept, see "Left as is"). `assemble_*` push into the context's lists and have no buffers of their
own.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): a summary of the sections below, each settled in its own subsection; in the tree.

## Prerequisites (other parts)

- **mem-db-infer** (`lib.rs`, `Cargo.toml`, `db.rs`): `#![feature(allocator_api)]`;
  `blaze-mem.workspace = true` and `hashbrown.workspace = true` in `crates/hir-ty/Cargo.toml`
  (hashbrown's `nightly` feature, filed by mem-db, is what makes `hashbrown::HashMap<K, V, S,
  &Frame>` take `core::alloc::Allocator`); `pub(crate) const INFER_TAG: blaze_mem::Tag =
  blaze_mem::Tag::Infer;` in `db.rs`. `crates/hir/Cargo.toml` needs `blaze-mem.workspace = true`
  (entry H1, H2; `crates/hir` is lane db's). `crates/blaze-items` already has it.
- **lane mem** (`crates/blaze-mem`): `pub fn current<R>(f: impl FnOnce(&Frame) -> R) -> R` in
  `frame.rs`, beside `task` (line 447). Everything below programs against that signature.
- **mem-db-infer-ctx** (`infer.rs`): `pub(crate) frame: &'f Frame` on `InferenceContext<'db,
  'f>` (R2), and expr's X1 (`write_expr_adj(&mut self, ExprId, &[Adjustment])`) and X2
  (`adjust_steps_as_infer_ok(&mut self, frame: &'f Frame) -> InferOk<'db, Vec<Adjustment, &'f
  Frame>>`), written out in `v2/requests/mem-db-infer-expr.md`. Every `impl<'db>
  InferenceContext<'db>` header in my files becomes `impl<'db, 'f> InferenceContext<'db, 'f>`.
- **mem-db-infer-lower** (`lower.rs`): `TyLoweringContext::new` takes the frame as its last
  parameter (R2); used by T1.
- **mem-db-solver** (`next_solver/`): `PredicateObligations`, `QueryResponse`,
  `OriginalQueryValues`, `elaborate`, `supertrait_def_ids`, `ObligationCtxt` allocate in the
  solver; see "Left as is".

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): in the tree: `crates/hir-ty/src/lib.rs:5` (`allocator_api`), `crates/hir-ty/src/db.rs:46` (`INFER_TAG`), `FrameMap`/`FrameSet` in `crates/hir-ty/src/lib.rs`, `crates/hir-ty/src/infer.rs:2049` (the context's frame), salsa's `mem_tag` (`vendor/salsa/src/function/execute.rs`).

## Shared helpers (method_resolution.rs, lane db and lane items)

> forwarded to db (from requests, pass 17; told by tmux).
> forwarded by db-mem-infer-b (21:25): every entry in `method_resolution.rs`, `method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs` and `drop.rs` (S1, P1 to P5, C1, C2, I1 to I6, M1 to M8, O1 to O4, SP1, D1, T1, T2 and their rows of the tables and tests below) is lane items', filed in `v2/requests/db.md` "mem-db-infer-method, for lane items". Lane db's entries here are P6, N1, R1, H1, H2.
> checked by db-audit-mem (22:46): in the tree at `crates/hir-ty/src/method_resolution.rs:83` (`pub(crate) use crate::{FrameMap, FrameSet};`).

### S1. Frame map and set aliases

Added once near the top of `method_resolution.rs` (after the `use`s), and used by probe.rs,
specialization.rs, drop.rs and inhabitedness.rs. mem-db-infer may move them to `lib.rs`; then
the paths below change to `crate::FrameMap`/`crate::FrameSet` and nothing else.

```rust
use blaze_mem::Frame;

/// A map of temporaries in a run's frame (rule R3 of `docs/v2/plan/lanes/mem/db-infer.md`).
pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
/// A set of temporaries in a run's frame.
pub(crate) type FrameSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;
```

`FxBuildHasher` is already imported in `method_resolution.rs` (`hash_self_ty` uses it).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (22:05): as mem-db-infer.md section 5 says, `pub(crate) use crate::{FrameMap, FrameSet};` after the `pub use self::{..}` block (`blaze_mem::Frame` was already imported).

## Hot path: method calls and paths

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): each subsection applied or superseded below (db-mem-infer-b, frontend-fast-method); in the tree at `crates/hir-ty/src/autoderef.rs:85` (`AutoderefSteps` an `ArrayVec`), `crates/hir-ty/src/method_resolution.rs:626` (`TraitValueItems::new_in`).

### P1. `MethodResolutionContext` carries the frame (method_resolution.rs:74, lane db + items)

Per probe. R1/R2: inside inference the frame is the `InferenceContext`'s; outside, the caller
opens a task (H1, H2). Before:

```rust
pub struct MethodResolutionContext<'a, 'db> {
    pub infcx: &'a InferCtxt<'db>,
    pub resolver: &'a Resolver<'db>,
    pub param_env: ParamEnv<'db>,
    pub traits_in_scope: &'a FxHashSet<TraitId>,
    pub edition: Edition,
    pub features: &'a UnstableFeatures,
    pub call_span: Span,
    pub receiver_span: Span,
}
```

After: the same, with one more field last:

```rust
    /// The frame the probe's temporaries live in: the inference run's, or the task a caller
    /// outside inference opened.
    pub frame: &'a Frame,
```

(Quote the struct as it is when applying; only the field is added.)

`with_method_resolution` (method_resolution.rs:188), in `impl<'db, 'f> InferenceContext<'db,
'f>` (was `impl<'db> InferenceContext<'db>`, line 139). Before:

```rust
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope,
            edition: self.edition,
            features: self.features,
            call_span,
            receiver_span,
        };
        f(&ctx)
```

After:

```rust
        let ctx = MethodResolutionContext {
            infcx: &self.table.infer_ctxt,
            resolver: &self.resolver,
            param_env: self.table.param_env,
            traits_in_scope,
            edition: self.edition,
            features: self.features,
            call_span,
            receiver_span,
            frame: self.frame,
        };
        f(&ctx)
```

`lookup_method_including_private` (line 168), with `ConfirmResult::adjustments` a frame `Vec`
(C2) and expr's X1. Before: `self.write_expr_adj(receiver, result.adjustments);` After:
`self.write_expr_adj(receiver, &result.adjustments);`

> forwarded to db (from requests, pass 17; told by tmux).

> applied by frontend-fast-method (holder of `method_resolution.rs`): the `frame` field, `frame: self.frame` in `with_method_resolution`, the `impl<'db, 'f> InferenceContext<'db, 'f>` header, and `write_expr_adj(receiver, &result.adjustments)`. H1/H2 can go in; they also need `TraitsInScope` (`v2/requests/mem-db-infer-ctx.md`, "frontend-fast-method: yes to A10/M1's split", items 2 and 3).

### P2. `ProbeContext`'s lists in the frame (probe.rs:62-72, 891-911, lane db + items)

Per probe: 4 lists and a set, each a global allocation (and regrowth) before. Before (fields):

```rust
    inherent_candidates: Vec<Candidate<'db>>,
    extension_candidates: Vec<Candidate<'db>>,
    impl_dups: FxHashSet<ImplId>,

    /// List of potential private candidates. Will be trimmed to ones that
    /// actually apply and then the result inserted into `private_candidate`
    private_candidates: Vec<Candidate<'db>>,

    /// Collects near misses when the candidate functions are missing a `self` keyword and is only
    /// used for error reporting
    static_candidates: Vec<CandidateSource>,
```

After:

```rust
    inherent_candidates: Vec<Candidate<'db>, &'a Frame>,
    extension_candidates: Vec<Candidate<'db>, &'a Frame>,
    impl_dups: FrameSet<'a, ImplId>,

    /// List of potential private candidates. Will be trimmed to ones that
    /// actually apply and then the result inserted into `private_candidate`
    private_candidates: Vec<Candidate<'db>, &'a Frame>,

    /// Collects near misses when the candidate functions are missing a `self` keyword and is only
    /// used for error reporting
    static_candidates: Vec<CandidateSource, &'a Frame>,
```

`ProbeContext::new`, before:

```rust
        ProbeContext {
            ctx,
            mode,
            inherent_candidates: Vec::new(),
            extension_candidates: Vec::new(),
            impl_dups: FxHashSet::default(),
            orig_steps_var_values,
            steps,
            private_candidates: Vec::new(),
            static_candidates: Vec::new(),
            choice,
        }
```

After:

```rust
        let frame = ctx.frame;
        ProbeContext {
            ctx,
            mode,
            inherent_candidates: Vec::new_in(frame),
            extension_candidates: Vec::new_in(frame),
            impl_dups: FrameSet::with_hasher_in(FxBuildHasher, frame),
            orig_steps_var_values,
            steps,
            private_candidates: Vec::new_in(frame),
            static_candidates: Vec::new_in(frame),
            choice,
        }
```

`ctx: &'a MethodResolutionContext<'a, 'db>`, so `ctx.frame: &'a Frame`. Imports in probe.rs:
`use blaze_mem::Frame;`, `use rustc_hash::{FxBuildHasher, FxHashSet};` and
`use super::{FrameSet, ..}` added to the existing `crate::method_resolution::{..}` import (line
34). `FxHashSet` stays for `collapse_candidates_to_subtrait_pick` and the `traits_in_scope`
parameter type. Users of the lists (`push_candidate` 959-964, `impl_dups.insert` 1092, the
`[("inherent", &self.inherent_candidates), ("extension", &self.extension_candidates)]` array at
1574, `&this.private_candidates` at 716/848, `record_static_candidate` 2116) compile unchanged:
both array elements have the same type, and `&Vec<_, A>` derefs to `&[_]`. The lists start empty
and take frame space only when a candidate is pushed.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (22:05): as written, with `frame_set(frame)` (lib.rs) for `FrameSet::with_hasher_in`; imports `crate::{FrameSet, frame_set}` and `blaze_mem::Frame`. `FxHashSet` stays (the collapse sets).

### P3. Probe's autoderef steps in the frame (probe.rs:246-256, 343, 497-498, 522, 546, lane db + items)

Per method call (MethodCall mode: one step per deref, 1 to 4 usually; Path mode: one). Before:

```rust
#[derive(Clone, Debug)]
struct MethodAutoderefStepsResult<'db> {
    /// The valid autoderef steps that could be found by following a chain
    /// of `Receiver<Target=T>` or `Deref<Target=T>` trait implementations.
    pub steps: SmallVec<[CandidateStep<'db>; 3]>,
```

After:

```rust
#[derive(Clone, Debug)]
struct MethodAutoderefStepsResult<'db, 'f> {
    /// The valid autoderef steps that could be found by following a chain
    /// of `Receiver<Target=T>` or `Deref<Target=T>` trait implementations.
    pub steps: Vec<CandidateStep<'db>, &'f Frame>,
```

`method_autoderef_steps` returns `MethodAutoderefStepsResult<'db, 'a>` (in `impl<'a, 'db>
MethodResolutionContext<'a, 'db>`). Path mode (line 342), before:

```rust
                MethodAutoderefStepsResult {
                    steps: smallvec![CandidateStep {
                        self_ty: self.infcx.make_query_response_ignoring_pending_obligations(
                            var_values,
                            self_ty,
                            prev_opaque_entries
                        ),
                        self_ty_is_opaque: false,
                        autoderefs: 0,
                        from_unsafe_deref: false,
                        unsize: false,
                        reachable_via_deref: true,
                    }],
                    opt_bad_ty: None,
                    reached_recursion_limit: false,
                }
```

After:

```rust
                let mut steps = Vec::with_capacity_in(1, self.frame);
                steps.push(CandidateStep {
                    self_ty: self.infcx.make_query_response_ignoring_pending_obligations(
                        var_values,
                        self_ty,
                        prev_opaque_entries,
                    ),
                    self_ty_is_opaque: false,
                    autoderefs: 0,
                    from_unsafe_deref: false,
                    unsize: false,
                    reachable_via_deref: true,
                });
                MethodAutoderefStepsResult { steps, opt_bad_ty: None, reached_recursion_limit: false }
```

MethodCall mode: the two `.collect()`s (497-498 `.collect::<SmallVec<[_; _]>>();` and 522
`.collect();`) become an extend into a frame `Vec`. Before (first arm, the second is the same
shape):

```rust
                let steps = autoderef_via_receiver
                    .by_ref()
                    .zip(reachable_via_deref)
                    .map(|((ty, d), reachable_via_deref)| {
                        // .. unchanged ..
                    })
                    .collect::<SmallVec<[_; _]>>();
```

After:

```rust
                let mut steps = Vec::with_capacity_in(4, self.frame);
                steps.extend(autoderef_via_receiver.by_ref().zip(reachable_via_deref).map(
                    |((ty, d), reachable_via_deref)| {
                        // .. unchanged ..
                    },
                ));
```

and in the `else` arm `let mut steps = Vec::with_capacity_in(4, self.frame);
steps.extend(autoderef_via_deref.by_ref().map(|(ty, d)| { .. unchanged .. }));`. The tuple
binding `let (mut steps, reached_recursion_limit) = if .. { .. (steps, ..) } else { .. }` and the
array-unsize `steps.push(..)` at 546 are unchanged. 4 covers `&Box<T>`, `&String`, `&Vec<T>` plus
the array unsize step without regrowth; a longer chain grows once in the frame.

`smallvec!` and `SmallVec` stay imported for `ApplicableCandidates`; `smallvec` (the macro) is
then unused: the import becomes `use smallvec::SmallVec;`.

> forwarded to db (from requests, pass 17; told by tmux).

> superseded by frontend-fast-method: `probe_op` borrows the steps in place from the steps cache (`pick_cache::with_steps`) or from one `method_autoderef_steps` call, and `Mode::Path`'s one step is inline in the `SmallVec`; there is no copy of the steps to move into the frame. Notes: docs/v2/plan/lanes/frontend/fast-method.md, section 3.3.

### P4. `ProbeForNameChoice` and `TraitValueItems` carry the frame (probe.rs:632-646, 282-300; method_resolution.rs:593-624, lane db + items)

Per probe. `TraitValueItems`, before:

```rust
#[derive(Debug, Default)]
pub struct TraitValueItems<'db> {
    runs: smallvec::SmallVec<[(Crate, &'db [(TraitId, CandidateId)]); 4]>,
}

impl<'db> TraitValueItems<'db> {
```

After:

```rust
#[derive(Debug)]
pub struct TraitValueItems<'db, 'f> {
    runs: Vec<(Crate, &'db [(TraitId, CandidateId)]), &'f Frame>,
}

impl<'db, 'f> TraitValueItems<'db, 'f> {
    pub fn new_in(frame: &'f Frame) -> Self {
        TraitValueItems { runs: Vec::new_in(frame) }
    }
```

(`get` unchanged.) `ProbeForNameChoice<'db>` becomes `ProbeForNameChoice<'db, 'f>` with
`trait_items: TraitValueItems<'db, 'f>`; `impl<'db> ProbeChoice<'db> for ProbeForNameChoice<'db>`
becomes `impl<'db, 'f> ProbeChoice<'db> for ProbeForNameChoice<'db, 'f>`; `impl<'a, 'db>
ProbeContext<'a, 'db, ProbeForNameChoice<'db>>` (1225) becomes `impl<'a, 'db, 'f> ProbeContext<'a,
'db, ProbeForNameChoice<'db, 'f>>`. In `probe_for_name_with` (296), before:
`trait_items: TraitValueItems::default(),` after: `trait_items: TraitValueItems::new_in(self.frame),`.

The test `crates/blaze-items/src/tests/narrow.rs:613` (lane items), before:

```rust
        for name in names {
            let mut index = TraitValueItems::default();
            for &trait_ in traits.iter().chain(traits.iter().rev()) {
                let alone = trait_value_item(&db, trait_, name);
                assert_eq!(index.get(&db, trait_, name), alone, "{trait_:?} {name:?}");
            }
        }
```

After:

```rust
        for name in names {
            blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                let mut index = TraitValueItems::new_in(frame);
                for &trait_ in traits.iter().chain(traits.iter().rev()) {
                    let alone = trait_value_item(&db, trait_, name);
                    assert_eq!(index.get(&db, trait_, name), alone, "{trait_:?} {name:?}");
                }
            });
        }
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (22:05): as written in `method_resolution.rs` and `probe.rs`. Two users newer than the entry: frontend-fast-method's `scope_sets.rs` (`ScopeItems::of_traits` and `TraitsInScope::named` take the caller's `frame`, the probe passes `self.frame`; the query `module_items_named` opens `frame::current`; both of the file's tracked fns get `mem_tag = Infer`) and `pick_cache/tests.rs` (its `TraitValueItems` inside `frame::task`). `crates/blaze-items/src/tests/narrow.rs:723` is lane items' crate: filed in `v2/requests/db.md`.

### P5. `ProbeAllChoice` in the frame (probe.rs:789-799, 302-309, 822-837, 627, 780-782, 881-883, lane db + items)

Per IDE completion probe. An `FxIndexMap` (two global tables, growing) becomes a frame set of the
ids seen and a frame `Vec` of candidates in insertion order, which is what the index map gave.
Before:

```rust
#[derive(Debug)]
struct ProbeAllChoice<'db> {
    candidates: RefCell<FxIndexMap<CandidateId, CandidateWithPrivate<'db>>>,
    considering_visible_candidates: bool,
}

impl ProbeAllChoice<'_> {
    fn new() -> Self {
        Self { candidates: RefCell::default(), considering_visible_candidates: true }
    }
}
```

After:

```rust
#[derive(Debug)]
struct ProbeAllChoice<'db, 'f> {
    candidates: RefCell<AllCandidates<'db, 'f>>,
    considering_visible_candidates: bool,
}

/// Each candidate the first time its item is seen, in that order.
#[derive(Debug)]
struct AllCandidates<'db, 'f> {
    seen: FrameSet<'f, CandidateId>,
    candidates: Vec<CandidateWithPrivate<'db>, &'f Frame>,
}

impl<'f> ProbeAllChoice<'_, 'f> {
    fn new(frame: &'f Frame) -> Self {
        Self {
            candidates: RefCell::new(AllCandidates {
                seen: FrameSet::with_hasher_in(FxBuildHasher, frame),
                candidates: Vec::new_in(frame),
            }),
            considering_visible_candidates: true,
        }
    }
}
```

`impl<'db> ProbeChoice<'db> for ProbeAllChoice<'db>` becomes `impl<'db, 'f> ProbeChoice<'db> for
ProbeAllChoice<'db, 'f>`. `consider_candidates`, before:

```rust
        let mut all_candidates = this.choice.candidates.borrow_mut();
        for candidate in candidates {
            // We should not override existing entries, because inherent methods of trait objects (from the principal)
            // are also visited as trait methods, and we want to consider them inherent.
            all_candidates
                .entry(candidate.item)
                .or_insert(CandidateWithPrivate { candidate: candidate.clone(), is_visible });
        }
```

After:

```rust
        let mut all_candidates = this.choice.candidates.borrow_mut();
        let AllCandidates { seen, candidates: all } = &mut *all_candidates;
        for candidate in candidates {
            // We should not override existing entries, because inherent methods of trait objects (from the principal)
            // are also visited as trait methods, and we want to consider them inherent.
            if seen.insert(candidate.item) {
                all.push(CandidateWithPrivate { candidate: candidate.clone(), is_visible });
            }
        }
```

`final_choice_from_err` takes the choice, so the error path of `ProbeAllChoice` returns the one it
was given (empty, as `Self::new()` was) instead of making a new one. Trait (627), before:
`fn final_choice_from_err(err: MethodError<'db>) -> Self::FinalChoice;` after:
`fn final_choice_from_err(choice: Self, err: MethodError<'db>) -> Self::FinalChoice;`.
`ProbeForNameChoice` (780): `fn final_choice_from_err(_choice: Self, err: MethodError<'db>) ->
Self::FinalChoice { Err(err) }`. `ProbeAllChoice` (881): `fn final_choice_from_err(choice: Self,
_err: MethodError<'db>) -> Self::FinalChoice { choice }`. `probe_op` (406), before:
`return Choice::final_choice_from_err(MethodError::ErrorReported);` after:
`return Choice::final_choice_from_err(choice, MethodError::ErrorReported);` (`choice` is still
unmoved there: it moves into `ProbeContext::new` at 415).

`probe_all` (302), before:

```rust
        self.probe_op(mode, self_ty, ProbeAllChoice::new()).candidates.into_inner().into_values()
```

After:

```rust
        self.probe_op(mode, self_ty, ProbeAllChoice::new(self.frame))
            .candidates
            .into_inner()
            .candidates
            .into_iter()
```

The iterator borrows the frame; both callers (`crates/hir/src/lib.rs:5871, 5972`) consume it
inside `with_method_resolution`'s closure with `try_for_each`, so it never outlives the task
(H2). `FxIndexMap` leaves probe.rs's imports (`use base_db::Db;`).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (22:05): as written (`frame_set(frame)` for the seen set; `final_choice_from_err(choice, ..)` in the trait, both impls and `probe_steps`, where `choice` is still unmoved). `FxIndexMap` left the imports. The kept `candidate.clone()` carries a `// clone:` mark.

### P6. Autoderef steps inline (autoderef.rs:31, 40-67, 82-89, 135-146, 152, 166-169, 246-260, lane db)

Per autoderef (field access, method call twice: probe and confirm, index, overloaded call,
coercion's reborrow). The walk refuses a 21st step (`if self.state.steps.len() >=
AUTODEREF_RECURSION_LIMIT { .. return None; }` before every push at line 221), so the steps fit
an `ArrayVec` of 20 and never allocate. Before:

```rust
impl<'db> TrackAutoderefSteps<'db> for Vec<(Ty<'db>, AutoderefKind)> {
    fn len(&self) -> usize {
        self.len()
    }
    fn push(&mut self, ty: Ty<'db>, kind: AutoderefKind) {
        self.push((ty, kind));
    }
}
```

After:

```rust
/// The steps of a walk, inline: [`GeneralAutoderef::next`] takes none past
/// [`AUTODEREF_RECURSION_LIMIT`].
pub(crate) type AutoderefSteps<'db> = ArrayVec<(Ty<'db>, AutoderefKind), AUTODEREF_RECURSION_LIMIT>;

impl<'db> TrackAutoderefSteps<'db> for AutoderefSteps<'db> {
    fn len(&self) -> usize {
        self.len()
    }
    fn push(&mut self, ty: Ty<'db>, kind: AutoderefKind) {
        self.push((ty, kind));
    }
}
```

and every `Steps = Vec<(Ty<'db>, AutoderefKind)>` default (lines 152, 166, 168) becomes `Steps =
AutoderefSteps<'db>`. `use arrayvec::ArrayVec;` (hir-ty already depends on arrayvec). Users read
`steps()` as a slice (`steps.iter()`, `steps.is_empty()`, `steps.len()` in
`infer/autoderef.rs:33`), which `ArrayVec` derefs to; nothing names the `Vec` type outside
autoderef.rs (checked with `rg -n "AutoderefKind\)>" crates`). The state moves by value in the
builder methods (`include_raw_pointers(mut self) -> Self`): 20 × 16 + 4 bytes, a `memcpy`, where
the `Vec` cost a `malloc` on the first step.

R2 (expr's X3): before:

```rust
pub(crate) struct InferenceContextAutoderefCtx<'a, 'db>(&'a mut InferenceContext<'db>);
impl<'db> AutoderefCtx<'db> for InferenceContextAutoderefCtx<'_, 'db> {
```

After:

```rust
pub(crate) struct InferenceContextAutoderefCtx<'a, 'db, 'f>(&'a mut InferenceContext<'db, 'f>);
impl<'db> AutoderefCtx<'db> for InferenceContextAutoderefCtx<'_, 'db, '_> {
```

Before:

```rust
pub(crate) type InferenceContextAutoderef<'a, 'db, Steps = Vec<(Ty<'db>, AutoderefKind)>> =
    GeneralAutoderef<'db, InferenceContextAutoderefCtx<'a, 'db>, Steps>;
```

After:

```rust
pub(crate) type InferenceContextAutoderef<'a, 'db, 'f, Steps = AutoderefSteps<'db>> =
    GeneralAutoderef<'db, InferenceContextAutoderefCtx<'a, 'db, 'f>, Steps>;
```

Before:

```rust
impl<'a, 'db> InferenceContextAutoderef<'a, 'db> {
    #[inline]
    pub(crate) fn new_from_inference_context(
        ctx: &'a mut InferenceContext<'db>,
        base_ty: Ty<'db>,
        span: Span,
    ) -> Self {
        Self::new_impl(InferenceContextAutoderefCtx(ctx), base_ty, span)
    }

    #[inline]
    pub(crate) fn ctx(&mut self) -> &mut InferenceContext<'db> {
        self.ctx.0
    }
}
```

After:

```rust
impl<'a, 'db, 'f> InferenceContextAutoderef<'a, 'db, 'f> {
    #[inline]
    pub(crate) fn new_from_inference_context(
        ctx: &'a mut InferenceContext<'db, 'f>,
        base_ty: Ty<'db>,
        span: Span,
    ) -> Self {
        Self::new_impl(InferenceContextAutoderefCtx(ctx), base_ty, span)
    }

    #[inline]
    pub(crate) fn ctx(&mut self) -> &mut InferenceContext<'db, 'f> {
        self.ctx.0
    }
}
```

The pub `autoderef` (hir's entry, called by `Type::autoderef_` at
`crates/hir/src/lib.rs:5571`), before:

```rust
    let mut v = Vec::new();
    for (ty, _steps) in autoderef {
        // ..
        if v.contains(&resolved) {
            break;
        }
        v.push(resolved);
    }
    v.into_iter()
```

After (the start type and at most 20 steps; same comment kept):

```rust
    let mut v = ArrayVec::<Ty<'db>, { AUTODEREF_RECURSION_LIMIT + 1 }>::new();
    for (ty, _steps) in autoderef {
        // ..
        if v.contains(&resolved) {
            break;
        }
        v.push(resolved);
    }
    v.into_iter()
```

The `XXX` comment's "`Vec::contains()`" reads "`contains`". The return type `impl Iterator<Item =
Ty<'db>> + use<'db>` holds: `arrayvec::IntoIter` owns its items.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (21:25): the `AutoderefSteps` `ArrayVec`, the three `Steps` defaults and the pub `autoderef`'s `ArrayVec` of 21, as written. The R2 part (`'f` on `InferenceContextAutoderefCtx`/`InferenceContextAutoderef`) applied at 21:35, as written, once ctx A0 and expr X3's callers (`callee.rs`, `place_op.rs`) were in.

## Confirm

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (frontend-fast-method); in the tree at `crates/hir-ty/src/method_resolution/confirm.rs:184` to `:195` (the context's frame, `adjust_steps_as_infer_ok(frame)`).

### C1. `ConfirmContext` and its lowerer carry `'f` (confirm.rs:40-45, 53, 74-80, 316, 352-358, lane db + items)

R2 (contexts holding `&mut InferenceContext` gain `'f`). Before:

```rust
struct ConfirmContext<'a, 'db> {
    ctx: &'a mut InferenceContext<'db>,
    candidate: FunctionId,
    call_expr: ExprId,
}
```

After:

```rust
struct ConfirmContext<'a, 'db, 'f> {
    ctx: &'a mut InferenceContext<'db, 'f>,
    candidate: FunctionId,
    call_expr: ExprId,
}
```

`impl<'db> InferenceContext<'db>` (53) becomes `impl<'db, 'f> InferenceContext<'db, 'f>`;
`impl<'a, 'db> ConfirmContext<'a, 'db>` (74) becomes `impl<'a, 'db, 'f> ConfirmContext<'a, 'db,
'f>` with `fn new(ctx: &'a mut InferenceContext<'db, 'f>, ..) -> ConfirmContext<'a, 'db, 'f>`;
the bound at 316 `F: FnMut(&ConfirmContext<'a, 'db>, Ty<'db>, PolyExistentialTraitRef<'db>) ->
R` becomes `F: FnMut(&ConfirmContext<'a, 'db, 'f>, Ty<'db>, PolyExistentialTraitRef<'db>) -> R`.
In `instantiate_method_args`, before:

```rust
        struct LowererCtx<'a, 'db> {
            ctx: &'a mut InferenceContext<'db>,
```

```rust
        impl<'db> GenericArgsLowerer<'db> for LowererCtx<'_, 'db> {
```

After:

```rust
        struct LowererCtx<'a, 'db, 'f> {
            ctx: &'a mut InferenceContext<'db, 'f>,
```

```rust
        impl<'db> GenericArgsLowerer<'db> for LowererCtx<'_, 'db, '_> {
```

> forwarded to db (from requests, pass 17; told by tmux).

> applied by frontend-fast-method, as written.

### C2. The receiver's adjustments in the frame (confirm.rs:47-51, 173, 179-245, lane db + items)

Per method call: a `Vec` (X2 made it a frame `Vec`) was boxed (`into_boxed_slice`, a shrinking
realloc after the pushes) only to be copied into the table by `write_expr_adj`. With expr's X1
taking a slice, the list stays in the frame. Before:

```rust
#[derive(Debug)]
pub(crate) struct ConfirmResult<'db> {
    pub(crate) callee: MethodCallee<'db>,
    pub(crate) illegal_sized_bound: bool,
    pub(crate) adjustments: Box<[Adjustment]>,
}
```

After:

```rust
#[derive(Debug)]
pub(crate) struct ConfirmResult<'db, 'f> {
    pub(crate) callee: MethodCallee<'db>,
    pub(crate) illegal_sized_bound: bool,
    pub(crate) adjustments: Vec<Adjustment, &'f Frame>,
}
```

`confirm_method` and `confirm` return `ConfirmResult<'db, 'f>` (line 173 `ConfirmResult { callee,
illegal_sized_bound, adjustments }` unchanged). `adjust_self_ty`, before:

```rust
    ) -> (Ty<'db>, Box<[Adjustment]>) {
        // Commit the autoderefs by calling `autoderef` again, but this
        // time writing the results into the various typeck results.
        let mut autoderef =
            self.ctx.table.autoderef_with_tracking(unadjusted_self_ty, self.call_expr.into());
        let Some((mut target, n)) = autoderef.nth(pick.autoderefs) else {
            return (Ty::new_error(self.interner(), ErrorGuaranteed), Box::new([]));
        };
        assert_eq!(n, pick.autoderefs);

        let mut adjustments =
            self.ctx.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok());
```

After:

```rust
    ) -> (Ty<'db>, Vec<Adjustment, &'f Frame>) {
        let frame = self.ctx.frame;
        // Commit the autoderefs by calling `autoderef` again, but this
        // time writing the results into the various typeck results.
        let mut autoderef =
            self.ctx.table.autoderef_with_tracking(unadjusted_self_ty, self.call_expr.into());
        let Some((mut target, n)) = autoderef.nth(pick.autoderefs) else {
            return (Ty::new_error(self.interner(), ErrorGuaranteed), Vec::new_in(frame));
        };
        assert_eq!(n, pick.autoderefs);

        let mut adjustments =
            self.ctx.table.register_infer_ok(autoderef.adjust_steps_as_infer_ok(frame));
```

and the last line `(target, adjustments.into_boxed_slice())` becomes `(target, adjustments)`. The
two `adjustments.push(..)` after it (autoref, unsize, or the const-pointer cast: at most 2) grow
the frame `Vec` at most once. `use blaze_mem::Frame;` in confirm.rs.

> forwarded to db (from requests, pass 17; told by tmux).

> applied by frontend-fast-method, as written (`autoderef.rs`'s X2 was already in).

## Impl tables: built once at exact size (method_resolution.rs:1220-1530, 1651-1711; frozen.rs:78-122; lane db + items)

> **Hold (2026-10-29 21:17):** `requests-impls` is applying `v2/requests/items.md`
> "frontend-nameres-impls" parts 1 to 4 in `method_resolution.rs` now. That change builds the
> tables per module (`for_module`, `merge_into`, `modules_by_trait` and the other bucket queries)
> and moves `InherentImpls`/`TraitImpls::for_crate`, `collect_def_map` and the `in_crate_*` memos.
> Do not apply I1 to I6, or M6's `collect` callers, until requests-impls reports done. The design
> (frame builders, flat records, one counting pass, exact columns, frozen reads pushed straight
> in) carries over: the per-module tables and the crate merge become the builders' inputs. The
> "before" quotes here are of the old per-crate walk and will be refreshed against the new code
> then.

> forwarded to db (from requests, pass 17; told by tmux).

### I1. What changes

Each crate's and each block's `TraitImpls` and `InherentImpls` is built from per-impl records.
Before: an `FxHashMap<TraitId, OneTraitImplsBuilder>` whose builders hold a `SelfTyGroups` (an
`FxHashMap` + `Vec`) of `(Vec<ImplId>, Vec<BuiltinDeriveImplId>)`, so a `Vec` per (trait, self
type) row grown per impl; then `from_traits` collects the map into a `Vec`, radix-sorts it
(global scratch above 64 traits), and grows every column (`vec![0]` + `push`, `Vec::new()` +
`extend`, then `into_boxed_slice`, a shrinking realloc). std's ~8,000 trait impls make ~5,000
row `Vec`s.

After: a builder in the query's frame takes each impl as a flat record `(group, id)`; groups are
numbered as first seen through one frame map; `finish` orders the groups (traits by their id
key, rows by `(trait rank, hash, first seen)`), counts each run's length, and writes every column
once at its size (counting sort; sort-and-CSR). The result is equal (`PartialEq`) to before:
the same traits order, rows in the same order (by hash, ties in first-seen order within the
trait, which is what the stable radix sort of `into_sorted` kept), runs in found order, and
`starts`/`trait_rows` beginning with 0. Frozen reads push into the same builders, so a frozen
crate's tables equal the found ones exactly as before (`Mode::Check` compares them).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05): the hold is over (requests-impls DONE). The rule applied to the per-module code: every table (`for_crate`, `for_module`, `for_block`, `in_module_for_trait`, `for_crate_and_trait`, frozen reads) is built by a frame builder from flat records, and the crate/bucket merges (`merge_into`, `merge_trait_into`, `merge_at`) push each module table's runs into the same builders, so first-seen order and the result stay as before. `modules_by_self_ty`/`modules_by_trait` (`SelfTyModules`, `TraitModules`) moved to the same counting construction.

### I2. New code (replaces `Runs::new`, `SelfTys::new`, `SelfTyGroups`, `OneTraitImplsBuilder`, `InherentImpls::from_groups`, `TraitImpls::from_traits`)

`Runs` (1224) keeps its fields; `Runs::new` (1230-1241) is replaced by:

```rust
impl<V: Copy> Runs<V> {
    /// The runs of `values`, each `(group, value)`: run `rank[group]` holds a group's values in
    /// the order they come. Counted first, so each column is allocated once at its size.
    fn from_groups(frame: &Frame, rank: &[u32], values: &[(u32, V)]) -> Self {
        u32::try_from(values.len()).expect("more than 2^32 impls in a crate");
        let ends = frame.alloc_slice_fill_with(rank.len() + 1, |_| 0u32);
        for &(group, _) in values {
            ends[rank[group as usize] as usize + 1] += 1;
        }
        for run in 1..ends.len() {
            ends[run] += ends[run - 1];
        }
        let starts = Box::<[u32]>::from(&*ends);
        let Some(&(_, first)) = values.first() else {
            return Runs { starts, values: Box::default() };
        };
        // `ends[run]` is now where run `run` starts, and is moved past each value placed.
        let placed = frame.alloc_slice_fill_with(values.len(), |_| first);
        for &(group, value) in values {
            let next = &mut ends[rank[group as usize] as usize];
            placed[*next as usize] = value;
            *next += 1;
        }
        Runs { starts, values: Box::from(&*placed) }
    }
}

/// Where each group goes: `rank[order[i]] == i`.
fn ranks<'f>(frame: &'f Frame, order: &[u32]) -> &'f mut [u32] {
    let rank = frame.alloc_slice_fill_with(order.len(), |_| 0u32);
    for (at, &group) in order.iter().enumerate() {
        rank[group as usize] = at as u32;
    }
    rank
}
```

`SelfTys::new` (1260-1264) is replaced by:

```rust
impl<'db> SelfTys<'db> {
    /// The self types of `groups` (each with its hash) in the order `order` gives.
    fn in_order(groups: &[(u64, SimplifiedType<'db>)], order: &[u32]) -> Self {
        SelfTys {
            hashes: order.iter().map(|&group| groups[group as usize].0).collect(),
            self_tys: order.iter().map(|&group| groups[group as usize].1).collect(),
        }
    }
```

(`len`, `is_empty`, `find` unchanged; `Map<slice::Iter>` collects into an exact `Box`.)
`SelfTyGroups` (1284-1314) is replaced by:

```rust
/// The group of `self_ty`, numbered as first seen: a new one is pushed on `self_tys` with its
/// hash.
fn group_of<'db>(
    groups: &mut FrameMap<'_, SimplifiedType<'db>, u32>,
    self_tys: &mut Vec<(u64, SimplifiedType<'db>), &Frame>,
    self_ty: SimplifiedType<'db>,
) -> u32 {
    *groups.entry(self_ty).or_insert_with(|| {
        self_tys.push((hash_self_ty(&self_ty), self_ty));
        u32::try_from(self_tys.len() - 1).expect("more than 2^32 self types in a crate")
    })
}

/// The groups sorted by the hash of their type; types of equal hash stay in the order they
/// were first seen.
fn order_by_hash<'f>(frame: &'f Frame, self_tys: &[(u64, SimplifiedType<'_>)]) -> &'f mut [u32] {
    let order = frame.alloc_slice_fill_with(self_tys.len(), |group| group as u32);
    order.sort_unstable_by_key(|&group| (self_tys[group as usize].0, group));
    order
}

/// The inherent impls of a crate or block while they are found or read, every temporary in the
/// query's frame.
struct InherentImplsBuilder<'db, 'f> {
    frame: &'f Frame,
    groups: FrameMap<'f, SimplifiedType<'db>, u32>,
    /// Each self type with its hash, in the order first seen.
    self_tys: Vec<(u64, SimplifiedType<'db>), &'f Frame>,
    /// Each impl with its group, in the order found.
    impls: Vec<(u32, ImplId), &'f Frame>,
}

impl<'db, 'f> InherentImplsBuilder<'db, 'f> {
    fn new(frame: &'f Frame) -> Self {
        InherentImplsBuilder {
            frame,
            groups: FrameMap::with_hasher_in(FxBuildHasher, frame),
            self_tys: Vec::new_in(frame),
            impls: Vec::new_in(frame),
        }
    }

    fn group(&mut self, self_ty: SimplifiedType<'db>) -> u32 {
        group_of(&mut self.groups, &mut self.self_tys, self_ty)
    }

    fn push(&mut self, group: u32, impl_id: ImplId) {
        self.impls.push((group, impl_id));
    }

    fn finish(self) -> InherentImpls<'db> {
        let order = order_by_hash(self.frame, &self.self_tys);
        let rank = ranks(self.frame, order);
        InherentImpls {
            self_tys: SelfTys::in_order(&self.self_tys, order),
            impls: Runs::from_groups(self.frame, rank, &self.impls),
        }
    }
}
```

`ranks(self.frame, order)` takes `&[u32]` from the `&mut` (`order` reborrows as shared).
`OneTraitImplsBuilder` (1444-1448) and `TraitImpls::from_traits` (1471-1498) are replaced by:

```rust
/// The trait impls of a crate or block while they are found or read, every temporary in the
/// query's frame. A trait and a row (trait, self type) are numbered as first seen.
struct TraitImplsBuilder<'db, 'f> {
    frame: &'f Frame,
    traits: FrameMap<'f, TraitId, u32>,
    trait_ids: Vec<TraitId, &'f Frame>,
    rows: FrameMap<'f, (u32, SimplifiedType<'db>), u32>,
    /// Each row's trait, and its self type with its hash, in the order first seen.
    row_keys: Vec<(u32, (u64, SimplifiedType<'db>)), &'f Frame>,
    impls: Vec<(u32, ImplId), &'f Frame>,
    derives: Vec<(u32, BuiltinDeriveImplId), &'f Frame>,
    /// Each blanket impl with its trait.
    blanket: Vec<(u32, ImplId), &'f Frame>,
}

impl<'db, 'f> TraitImplsBuilder<'db, 'f> {
    fn new(frame: &'f Frame) -> Self {
        TraitImplsBuilder {
            frame,
            traits: FrameMap::with_hasher_in(FxBuildHasher, frame),
            trait_ids: Vec::new_in(frame),
            rows: FrameMap::with_hasher_in(FxBuildHasher, frame),
            row_keys: Vec::new_in(frame),
            impls: Vec::new_in(frame),
            derives: Vec::new_in(frame),
            blanket: Vec::new_in(frame),
        }
    }

    fn trait_(&mut self, trait_: TraitId) -> u32 {
        let trait_ids = &mut self.trait_ids;
        *self.traits.entry(trait_).or_insert_with(|| {
            trait_ids.push(trait_);
            (trait_ids.len() - 1) as u32
        })
    }

    fn row(&mut self, t: u32, self_ty: SimplifiedType<'db>) -> u32 {
        let row_keys = &mut self.row_keys;
        *self.rows.entry((t, self_ty)).or_insert_with(|| {
            row_keys.push((t, (hash_self_ty(&self_ty), self_ty)));
            u32::try_from(row_keys.len() - 1).expect("more than 2^32 impls in a crate")
        })
    }

    fn push_impl(&mut self, row: u32, impl_id: ImplId) {
        self.impls.push((row, impl_id));
    }

    fn push_derive(&mut self, row: u32, impl_id: BuiltinDeriveImplId) {
        self.derives.push((row, impl_id));
    }

    fn push_blanket(&mut self, t: u32, impl_id: ImplId) {
        self.blanket.push((t, impl_id));
    }

    fn finish(self) -> TraitImpls<'db> {
        let frame = self.frame;
        let trait_ids = &self.trait_ids;
        let trait_order = frame.alloc_slice_fill_with(trait_ids.len(), |t| t as u32);
        trait_order.sort_unstable_by_key(|&t| {
            let id = trait_ids[t as usize].as_id();
            radix::pack_32(id.index(), id.generation())
        });
        let trait_rank = ranks(frame, trait_order);
        let row_keys = &self.row_keys;
        let row_order = frame.alloc_slice_fill_with(row_keys.len(), |r| r as u32);
        row_order.sort_unstable_by_key(|&r| {
            let (t, (hash, _)) = row_keys[r as usize];
            (trait_rank[t as usize], hash, r)
        });
        let row_rank = ranks(frame, row_order);
        let trait_rows = frame.alloc_slice_fill_with(trait_ids.len() + 1, |_| 0u32);
        for &(t, _) in row_keys.iter() {
            trait_rows[trait_rank[t as usize] as usize + 1] += 1;
        }
        for t in 1..trait_rows.len() {
            trait_rows[t] += trait_rows[t - 1];
        }
        TraitImpls {
            traits: trait_order.iter().map(|&t| trait_ids[t as usize]).collect(),
            trait_rows: Box::from(&*trait_rows),
            blanket: Runs::from_groups(frame, trait_rank, &self.blanket),
            self_tys: SelfTys {
                hashes: row_order.iter().map(|&r| row_keys[r as usize].1.0).collect(),
                self_tys: row_order.iter().map(|&r| row_keys[r as usize].1.1).collect(),
            },
            impls: Runs::from_groups(frame, row_rank, &self.impls),
            derives: Runs::from_groups(frame, row_rank, &self.derives),
        }
    }
}
```

Keys: `pack_32(index, generation)` is unique per trait (salsa ids differ in index), so the
unstable sort is total and gives the order the radix sort gave; `trait_index`'s `binary_search`
over `traits` is unchanged. A trait gets a number only when an impl of it is pushed, so every
trait has at least one impl, as before. `radix` stays imported for `pack_32`; `radix::sort_by_key`
(global scratch above 64 items) is no longer called here. `FxHashMap` leaves the imports of
`method_resolution.rs` if nothing else uses it (`macros_writing_impls` no longer does, M4).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05): `Runs::from_groups`, `ranks`, `SelfTys::in_order`, `group_of`, `order_by_hash` and `TraitImplsBuilder` as written. Differences: the inherent builder is generic, `SelfTyRunsBuilder<'db, 'f, V>` (`columns()` gives `(SelfTys, Runs<V>)`), with `type InherentImplsBuilder<'db, 'f> = SelfTyRunsBuilder<'db, 'f, ImplId>` and its `finish`, so `modules_by_self_ty` builds `SelfTyModules` with `V = ModuleId`; trait numbering and the trait sort are shared free fns `trait_number` and `order_by_trait_id` (used by `TraitImplsBuilder` and `TraitModules::from_found`, which replaces `TraitModules::new`); maps via `crate::frame_map`. `FxHashMap` left the imports.

### I3. Collecting (method_resolution.rs:1382-1418, 1652-1711)

`InherentImpls::collect_def_map`, before:

```rust
    fn collect_def_map(db: &'db Db, def_map: &'db DefMap) -> Self {
        let mut map = SelfTyGroups::default();
        collect(db, def_map, &mut map);
        return Self::from_groups(map);

        fn collect<'db>(
            db: &'db Db,
            def_map: &DefMap,
            map: &mut SelfTyGroups<'db, Vec<ImplId>>,
        ) {
```

After:

```rust
    fn collect_def_map(db: &'db Db, frame: &Frame, def_map: &'db DefMap) -> Self {
        let mut builder = InherentImplsBuilder::new(frame);
        collect(db, def_map, &mut builder);
        return builder.finish();

        fn collect<'db>(db: &'db Db, def_map: &DefMap, builder: &mut InherentImplsBuilder<'db, '_>) {
```

with, in its body, `map.entry(self_ty).push(impl_id);` becoming `let group =
builder.group(self_ty); builder.push(group, impl_id);`, `for owner in impl_holders(db,
def_map.krate(), module_data.scope(db))` becoming `for owner in impl_holders(db, builder.frame,
def_map.krate(), module_data.scope(db))` (M6), and `collect(db, block_def_map, map);` becoming
`collect(db, block_def_map, builder);`. `from_groups` (1413-1418) is deleted.

`TraitImpls::collect_def_map`, before:

```rust
    fn collect_def_map(db: &'db Db, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut map = FxHashMap::default();
        collect(db, def_map, lang_items, &mut map);
        return Self::from_traits(map);

        fn collect<'db>(
            db: &'db Db,
            def_map: &DefMap,
            lang_items: &LangItems,
            map: &mut FxHashMap<TraitId, OneTraitImplsBuilder<'db>>,
        ) {
```

After:

```rust
    fn collect_def_map(db: &'db Db, frame: &Frame, def_map: &DefMap) -> Self {
        let lang_items = hir_def::lang_item::lang_items(db, def_map.krate());
        let mut builder = TraitImplsBuilder::new(frame);
        collect(db, def_map, lang_items, &mut builder);
        return builder.finish();

        fn collect<'db>(
            db: &'db Db,
            def_map: &DefMap,
            lang_items: &LangItems,
            builder: &mut TraitImplsBuilder<'db, '_>,
        ) {
```

In its body, before:

```rust
                    let interner = DbInterner::new_no_crate(db);
                    let entry = map.entry(trait_ref.def_id.0).or_default();
                    match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
                        Some(self_ty) => entry.non_blanket_impls.entry(self_ty).0.push(impl_id),
                        None => entry.blanket_impls.push(impl_id),
                    }
```

After:

```rust
                    let interner = DbInterner::new_no_crate(db);
                    let t = builder.trait_(trait_ref.def_id.0);
                    match simplify_type(interner, self_ty, TreatParams::InstantiateWithInfer) {
                        Some(self_ty) => {
                            let row = builder.row(t, self_ty);
                            builder.push_impl(row, impl_id);
                        }
                        None => builder.push_blanket(t, impl_id),
                    }
```

Before:

```rust
                    let Some(trait_id) = loc.trait_.get_id(lang_items) else { continue };
                    let entry = map.entry(trait_id).or_default();
                    let entry = entry.non_blanket_impls.entry(SimplifiedType::Adt(loc.adt.into()));
                    entry.1.push(impl_id);
```

After:

```rust
                    let Some(trait_id) = loc.trait_.get_id(lang_items) else { continue };
                    let t = builder.trait_(trait_id);
                    let row = builder.row(t, SimplifiedType::Adt(loc.adt.into()));
                    builder.push_derive(row, impl_id);
```

and `impl_holders(db, def_map.krate(), ..)` / `collect(db, block_def_map, lang_items, map)` as
for inherent impls (`builder.frame`, `builder`).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05): on the per-module form: both `collect_def_map(db, frame, def_map)` loop `collect_module(db, module, .., &mut builder)` and `finish()`; `collect_module` takes `&mut {Inherent,Trait}ImplsBuilder<'db, '_>`, its bodies as written, and calls `impl_holders(db, builder.frame, module.krate(db), scope)` (M6's signature, by db-mem-infer-b-queries). `merge_into`/`merge_trait_into`/`merge_at` push into the builders; `from_groups`/`from_traits` deleted.

### I4. The queries open the frame (method_resolution.rs:1332-1342, 1355-1365, 1531-1553)

R1 + R6. Before:

```rust
    #[salsa::tracked(returns(ref))]
    pub fn for_crate(db: &'db Db, krate: Crate) -> InherentImpls<'db> {
        let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
        if let Some(frozen) = frozen::inherent_impls(db, krate) {
            return frozen;
        }

        let crate_def_map = crate_def_map(db, krate);

        Self::collect_def_map(db, crate_def_map)
    }
```

After:

```rust
    #[salsa::tracked(returns(ref), mem_tag = crate::db::INFER_TAG)]
    pub fn for_crate(db: &'db Db, krate: Crate) -> InherentImpls<'db> {
        blaze_mem::frame::current(|frame| {
            let _p = tracing::info_span!("inherent_impls_in_crate_query", ?krate).entered();
            if let Some(frozen) = frozen::inherent_impls(db, frame, krate) {
                return frozen;
            }

            let crate_def_map = crate_def_map(db, krate);

            Self::collect_def_map(db, frame, crate_def_map)
        })
    }
```

`InherentImpls::for_block`, before:

```rust
    #[salsa::tracked(returns(ref))]
    pub fn for_block(
        db: &'db Db,
        block: BlockIdLt<'db>,
    ) -> Option<Box<InherentImpls<'db>>> {
        let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

        let block_def_map = block_def_map(db, block);
        let result = Self::collect_def_map(db, block_def_map);
        if result.self_tys.is_empty() { None } else { Some(Box::new(result)) }
    }
```

After:

```rust
    #[salsa::tracked(returns(ref), mem_tag = crate::db::INFER_TAG)]
    pub fn for_block(
        db: &'db Db,
        block: BlockIdLt<'db>,
    ) -> Option<Box<InherentImpls<'db>>> {
        blaze_mem::frame::current(|frame| {
            let _p = tracing::info_span!("inherent_impls_in_block_query").entered();

            let block_def_map = block_def_map(db, block);
            let result = Self::collect_def_map(db, frame, block_def_map);
            if result.self_tys.is_empty() { None } else { Some(Box::new(result)) }
        })
    }
```

`TraitImpls::for_crate` and `TraitImpls::for_block` the same way: `frame::current` around the
body, `frozen::trait_impls(db, frame, krate)`, `Self::collect_def_map(db, frame, ..)`, and
`mem_tag = crate::db::INFER_TAG` added to `returns(ref)` / `returns(as_deref)`. The `Arc::new` /
`Box::new` of the result stay: they are the memo's one allocation for its handle.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05): `frame::current` around `for_crate` and `for_block` of both tables, and also around the new `for_module`, `modules_by_self_ty`, `modules_by_trait`, `in_module_for_trait` and `for_crate_and_trait`; frozen reads take the frame. Tags as `mem_tag = Infer`.

### I5. Frozen reads push into the builders (frozen.rs:80-122, lane db + items)

Per frozen crate. Before: `dec.many` made a `Vec` of entries, each with two `Vec`s of ids, then
the old groups. Before:

```rust
pub(super) fn trait_impls<'db>(db: &'db Db, krate: Crate) -> Option<TraitImpls<'db>> {
    read(db, krate, 0, |dec| {
        let n = dec.len()?;
        let mut map = FxHashMap::default();
        map.reserve(n);
        for _ in 0..n {
            let ModuleDefId::TraitId(trait_) = dec.def()? else {
                return Err("impls of no trait".to_owned());
            };
            let entries = dec.many(|dec| {
                let self_ty = read_simplified(dec)?;
                let impls = dec.many(|dec| dec.impl_id())?;
                let derived = dec.many(|dec| dec.builtin_derive_impl())?;
                Ok((self_ty, (impls, derived)))
            })?;
            let mut one = OneTraitImplsBuilder::default();
            for (self_ty, (impls, derived)) in entries {
                let entry = one.non_blanket_impls.entry(self_ty);
                entry.0.extend(impls);
                entry.1.extend(derived);
            }
            one.blanket_impls = dec.many(|dec| dec.impl_id())?;
            map.insert(trait_, one);
        }
        Ok(TraitImpls::from_traits(map))
    })
}
```

After:

```rust
pub(super) fn trait_impls<'db>(
    db: &'db Db,
    frame: &Frame,
    krate: Crate,
) -> Option<TraitImpls<'db>> {
    read(db, krate, 0, |dec| {
        let n = dec.len()?;
        let mut builder = TraitImplsBuilder::new(frame);
        for _ in 0..n {
            let ModuleDefId::TraitId(trait_) = dec.def()? else {
                return Err("impls of no trait".to_owned());
            };
            let t = builder.trait_(trait_);
            // `many` of `()` allocates nothing: each id goes straight into the builder.
            dec.many(|dec| {
                let row = builder.row(t, read_simplified(dec)?);
                dec.many(|dec| Ok(builder.push_impl(row, dec.impl_id()?)))?;
                dec.many(|dec| Ok(builder.push_derive(row, dec.builtin_derive_impl()?)))?;
                Ok(())
            })?;
            dec.many(|dec| Ok(builder.push_blanket(t, dec.impl_id()?)))?;
        }
        Ok(builder.finish())
    })
}
```

`inherent_impls`, before:

```rust
    read(db, krate, 1, |dec| {
        let entries = dec.many(|dec| {
            let self_ty = read_simplified(dec)?;
            Ok((self_ty, dec.many(|dec| dec.impl_id())?))
        })?;
        let mut groups = SelfTyGroups::default();
        for (self_ty, impls) in entries {
            groups.entry(self_ty).extend(impls);
        }
        Ok(InherentImpls::from_groups(groups))
    })
```

After (signature `inherent_impls<'db>(db: &'db Db, frame: &Frame, krate: Crate)`):

```rust
    read(db, krate, 1, |dec| {
        let mut builder = InherentImplsBuilder::new(frame);
        dec.many(|dec| {
            let group = builder.group(read_simplified(dec)?);
            dec.many(|dec| Ok(builder.push(group, dec.impl_id()?)))?;
            Ok(())
        })?;
        Ok(builder.finish())
    })
```

`Dec::many` is `len_and_room` then `Vec::with_capacity(room)` and a push per item
(`hir_def::nameres::frozen`); a `Vec<()>` allocates nothing, so these lists cost no heap. A
nested `many` closure borrows `builder` mutably while the outer closure holds it: each is a
separate `FnMut` call, and the outer closure's borrow is reborrowed for the inner one (same
shape as today's nested `dec.many`). Imports: `use super::{InherentImpls, InherentImplsBuilder,
TraitImpls, TraitImplsBuilder};` and `use blaze_mem::Frame;`; `FxHashMap` stays for
`FrozenImpls::maps`. One behaviour on corrupt bytes changes: a trait written twice in a part was
replaced by its second copy and is now merged with the first. The encoder writes each trait once
(`enc.sorted(trait_impls.trait_indices(), ..)`), so frozen bytes never have it.

`encode` (170), before: `let mut out = len.to_le_bytes().to_vec();` followed by two
`extend_from_slice`s. After:

```rust
    let mut out = Vec::with_capacity(4 + traits.len() + inherent.len());
    out.extend_from_slice(&len.to_le_bytes());
```

(the two `extend_from_slice` calls stay). Run once per crate frozen, outside queries; its result
is the caller's.

> forwarded to db (from requests, pass 17; told by tmux).
> held by requests-items-mem: the readers need I2's `TraitImplsBuilder`/`InherentImplsBuilder` (not in `method_resolution.rs` yet) and change I4's call sites, so they cannot land apart; handed to frontend-fast-method to apply with I2/I4 (`v2/requests/requests-items-mem.md`, told by tmux). `encode`'s half was already in.
> applied by db-mem-infer-b-impls (22:05): both readers as written, with I2/I4; imports `InherentImplsBuilder`, `TraitImplsBuilder`, `blaze_mem::Frame`; `FxHashMap` stays for `FrozenImpls::maps`. dedup's cursor change (`part`, the writer) untouched (mem-db-infer.md 6 item 9).

### I6. Lookups over many crates collect exactly (method_resolution.rs:1562-1648)

Per query run (per (crate, trait, self type) asked). `Box<[T]>: FromIterator` from a growing
`Vec` reallocates on growth and shrinks at the end; a frame `Vec`, then `iter().copied().collect()`
over the slice (TrustedLen), allocates the result once.

`in_crate_and_deps_for_trait_and_self_ty`, before:

```rust
    #[salsa::tracked(returns(deref))]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        let mut found = Vec::new();
        for impls in Self::for_crate_and_deps(db, krate) {
            let (impls, builtin_derive_impls) = impls.for_trait_and_self_ty(trait_, &self_ty.0);
            found.extend(impls.iter().map(|&it| AnyImplId::from(it)));
            found.extend(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)));
        }
        found.into()
    }
```

After:

```rust
    #[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
    pub fn in_crate_and_deps_for_trait_and_self_ty(
        db: &'db Db,
        krate: Crate,
        trait_: TraitId,
        self_ty: SelfTyKey<'db>,
    ) -> Box<[AnyImplId]> {
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            for impls in Self::for_crate_and_deps(db, krate) {
                let (impls, builtin_derive_impls) =
                    impls.for_trait_and_self_ty(trait_, &self_ty.0);
                found.extend(impls.iter().map(|&it| AnyImplId::from(it)));
                found.extend(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)));
            }
            found.iter().copied().collect()
        })
    }
```

`blanket_in_crate_and_deps`, before:

```rust
        Self::for_crate_and_deps(db, krate)
            .iter()
            .flat_map(|impls| impls.blanket_impls(trait_))
            .copied()
            .collect()
```

After:

```rust
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            for impls in Self::for_crate_and_deps(db, krate) {
                found.extend_from_slice(impls.blanket_impls(trait_));
            }
            found.iter().copied().collect()
        })
```

`in_crate_for_trait`, before:

```rust
        let mut found = Vec::new();
        Self::for_crate(db, krate).for_trait(trait_, |impls| {
            if let Either::Left(impls) = impls {
                found.extend_from_slice(impls);
            }
        });
        found.into()
```

After:

```rust
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            Self::for_crate(db, krate).for_trait(trait_, |impls| {
                if let Either::Left(impls) = impls {
                    found.extend_from_slice(impls);
                }
            });
            found.iter().copied().collect()
        })
```

`in_crate_and_deps_for_trait`, before:

```rust
        loaded_crates(db, krate)
            .iter()
            .flat_map(|&dep| Self::in_crate_for_trait(db, dep, trait_))
            .copied()
            .collect()
```

After:

```rust
        blaze_mem::frame::current(|frame| {
            let mut found = Vec::new_in(frame);
            for &dep in loaded_crates(db, krate) {
                found.extend_from_slice(Self::in_crate_for_trait(db, dep, trait_));
            }
            found.iter().copied().collect()
        })
```

Each with `mem_tag = crate::db::INFER_TAG` added to its `#[salsa::tracked(returns(deref))]`.
`for_crate_and_deps` (1556), `in_crate_for_self_ty` (1347), `in_block_for_self_ty` (1369),
`in_crate_for_trait_and_self_ty` (1594), `in_block_for_trait_and_self_ty` (1605): tag only;
their results are `collect` over `slice::Iter` (exact) or `Box::from(&[T])` (`.into()` of a
slice, exact).

`crates_containing_incoherent_inherent_impls` (1165), before:

```rust
#[salsa::tracked(returns(ref))]
fn crates_containing_incoherent_inherent_impls(db: &Db, krate: Crate) -> Box<[Crate]> {
    let _p = tracing::info_span!("crates_containing_incoherent_inherent_impls").entered();
    // We assume that only sysroot crates contain `#[rustc_has_incoherent_inherent_impls]`
    // impls, since this is an internal feature and only std uses it.
    krate.transitive_deps(db).into_iter().filter(|krate| krate.data(db).origin.is_lang()).collect()
}
```

After:

```rust
#[salsa::tracked(returns(ref), mem_tag = crate::db::INFER_TAG)]
fn crates_containing_incoherent_inherent_impls(db: &Db, krate: Crate) -> Box<[Crate]> {
    blaze_mem::frame::current(|frame| {
        let _p = tracing::info_span!("crates_containing_incoherent_inherent_impls").entered();
        // We assume that only sysroot crates contain `#[rustc_has_incoherent_inherent_impls]`
        // impls, since this is an internal feature and only std uses it.
        let mut lang = Vec::new_in(frame);
        lang.extend(krate.transitive_deps(db).into_iter().filter(|krate| krate.data(db).origin.is_lang()));
        lang.iter().copied().collect()
    })
}
```

(`transitive_deps` returns base-db's owned `Vec`; see "Left as is".)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05): on the per-module code. `in_crate_and_deps_for_trait_and_self_ty`, `blanket_in_crate_and_deps` (both now read each crate's `for_crate_and_trait`), `in_crate_for_trait` and `in_crate_and_deps_for_trait` collect in a frame `Vec`, result exact; so does `InherentImpls::in_crate_for_self_ty`'s several-modules path (was a global `Vec` with `// alloc:`). `crates_containing_incoherent_inherent_impls` in mem-db-infer.md 6 item 8's form (B6 is in: `transitive_deps(db).iter().copied()`). Every lookup tagged `Infer`.

## Other method_resolution.rs queries (lane db + items)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): M1 to M8 applied below (db-mem-infer-b-queries); 33 `mem_tag = Infer` in `crates/hir-ty/src/method_resolution.rs`.

### M1. `trait_value_items` (559-579)

Per crate. A global `Vec` grown per trait, a stable sort (std's stable sort allocates scratch of
up to half the length on the global heap), then `into()` (a shrinking realloc). Before:

```rust
#[salsa::tracked(returns(deref))]
fn trait_value_items(db: &Db, krate: Crate) -> Box<[(Name, TraitId, CandidateId)]> {
    let mut items: Vec<(Name, TraitId, CandidateId)> = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            let ModuleDefId::TraitId(trait_) = def else { continue };
            items.extend(trait_.trait_items(db).items.iter().filter_map(|&(name, item)| {
                match item {
                    AssocItemId::FunctionId(it) => Some((name, trait_, it.into())),
                    AssocItemId::ConstId(it) => Some((name, trait_, it.into())),
                    AssocItemId::TypeAliasId(_) => None,
                }
            }));
        }
    }
    // Stable, so of a trait's items of one name the first stays first, as `value_item_named`
    // takes it.
    items.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    items.dedup_by(|later, first| (later.0, later.1) == (first.0, first.1));
    items.into()
}
```

After:

```rust
#[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
fn trait_value_items(db: &Db, krate: Crate) -> Box<[(Name, TraitId, CandidateId)]> {
    blaze_mem::frame::current(|frame| {
        // Each with where it was found: of a trait's items of one name the first stays first,
        // as `value_item_named` takes it.
        let mut items: Vec<(Name, TraitId, usize, CandidateId), &Frame> = Vec::new_in(frame);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            for def in module.scope(db).declarations() {
                let ModuleDefId::TraitId(trait_) = def else { continue };
                for &(name, item) in trait_.trait_items(db).items.iter() {
                    let item = match item {
                        AssocItemId::FunctionId(it) => CandidateId::from(it),
                        AssocItemId::ConstId(it) => CandidateId::from(it),
                        AssocItemId::TypeAliasId(_) => continue,
                    };
                    let at = items.len();
                    items.push((name, trait_, at, item));
                }
            }
        }
        items.sort_unstable_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
        items.dedup_by(|later, first| (later.0, later.1) == (first.0, first.1));
        items.iter().map(|&(name, trait_, _, item)| (name, trait_, item)).collect()
    })
}
```

The key `(name, trait, position)` is unique, so the unstable sort gives exactly the stable
sort's order.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): as written, with `mem_tag = Infer` for `crate::db::INFER_TAG` (settled); `// alloc:` on the result's `collect()`.

### M2. `in_crate_trait_value_items` (583-592)

Per (crate, name) asked. `take_while(..).collect()` is not TrustedLen. Before:

```rust
    let items = trait_value_items(db, krate);
    let start = items.partition_point(|&(it, _, _)| it < name);
    items[start..].iter().take_while(|&&(it, _, _)| it == name).map(|&(_, t, i)| (t, i)).collect()
```

After (tag added; no frame needed):

```rust
    let items = trait_value_items(db, krate);
    let start = items.partition_point(|&(it, _, _)| it < name);
    let end = items.partition_point(|&(it, _, _)| it <= name);
    items[start..end].iter().map(|&(_, t, i)| (t, i)).collect()
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): as written (`mem_tag = Infer`); `TraitValueItems` (P4) left to db-mem-infer-b.

### M3. `loaded_crates` (683-763)

Per crate. Before (the parts that change):

```rust
    let mut unnamed: Vec<_> = data
        .dependencies
        .iter()
        .filter(|dep| {
            let data = dep.crate_id.data(db);
            !(dep.is_sysroot() || data.origin.is_lang() || data.target_kind.is_proc_macro())
        })
        .collect();
```

```rust
    let named_in = |unnamed: &mut Vec<&base_db::BuiltDependency>, root: &syntax::SyntaxNode| {
```

```rust
    let mut calls = def_map
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).all_macro_calls().collect::<Vec<_>>())
        .filter(|call| {
            let def = call.loc(db).def;
            def.krate != krate
                && matches!(
                    def.kind,
                    hir_expand::MacroDefKind::ProcMacro(..)
                        | hir_expand::MacroDefKind::Declarative(..)
                )
        });
    while !unnamed.is_empty()
        && let Some(call) = calls.next()
    {
        named_in(&mut unnamed, &call.parse_macro_expansion(db).value.0.syntax_node());
    }
    let mut loaded = FxHashSet::default();
    let mut ordered = vec![krate];
    loaded.insert(krate);
```

```rust
    ordered.into()
```

After: the body is wrapped in `blaze_mem::frame::current(|frame| { .. })` (tag added), and:

```rust
    let mut unnamed = Vec::new_in(frame);
    unnamed.extend(data.dependencies.iter().filter(|dep| {
        let data = dep.crate_id.data(db);
        !(dep.is_sysroot() || data.origin.is_lang() || data.target_kind.is_proc_macro())
    }));
```

```rust
    let named_in = |unnamed: &mut Vec<&base_db::BuiltDependency, &Frame>,
                    root: syntax::view::Node<'_>| {
        let (tree, at) = root.flat();
```

replacing the closure's first three lines (`let named_in = |unnamed: &mut
Vec<&base_db::BuiltDependency>, root: &syntax::SyntaxNode| {`, `let root = root.to_flat();`,
`let (tree, at) = root.flat().expect("a flat tree");`); the rest of its body is unchanged. New
code walks views, not `SyntaxNode` handles (lane frontend-tree). The two are the same walk:
`SyntaxNode::to_flat` is `self.clone()`, and its `flat()` and the view's `Node::flat()` both give
the tree the node is in and its number there (`syntax-tree/src/cursor.rs:309-323`, `view.rs:194`),
so the same identifiers are read, lazy parts included. The file loop's call, before:
`named_in(&mut unnamed, &file.parse(db).syntax_node());` after:
`named_in(&mut unnamed, file.parse(db).root());` (`Parse::root`, `syntax/src/view.rs:170`; the
parse lives to the end of the statement).

```rust
    'modules: for (_, module) in def_map.modules(db) {
        for call in module.scope(db).all_macro_calls() {
            if unnamed.is_empty() {
                break 'modules;
            }
            let def = call.loc(db).def;
            if def.krate == krate
                || !matches!(
                    def.kind,
                    hir_expand::MacroDefKind::ProcMacro(..)
                        | hir_expand::MacroDefKind::Declarative(..)
                )
            {
                continue;
            }
            named_in(&mut unnamed, call.parse_macro_expansion(db).value.0.root());
        }
    }
    let mut loaded = FrameSet::with_hasher_in(FxBuildHasher, frame);
    let mut ordered = Vec::new_in(frame);
    ordered.push(krate);
    loaded.insert(krate);
```

```rust
    ordered.iter().copied().collect()
```

The per-module `collect::<Vec<_>>()` existed only because `flat_map`'s closure could not return
an iterator borrowing `module.scope(db)`; nested loops borrow it in place. The order calls are
visited in and the stop when `unnamed` empties are unchanged. `FxHashSet` stays imported in
method_resolution.rs if other code uses it (`with_incoherent_inherent_impls` callers do not;
check at apply time).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): the closure was already on `syntax::view::Node` (frontend-tree), so only its `&mut Vec<_, &Frame>` parameter changed; the set is `crate::frame_set(frame)`; `FxHashSet` had no other use in the file and was dropped from the `rustc_hash` import.

### M4. `macros_writing_impls` (992-1051) and `macro_definitions` (1057-1132)

Per crate and per file. `macro_definitions` returned a `Box<[Symbol]>` per `macro_rules!`; it
becomes two flat columns. New type (beside `BodyMark`):

```rust
/// The `macro_rules!` of a file ([`macro_definitions`]): each one's name, whether it may write
/// an impl, and the end of its run in `calls`, the names of the other macros it calls.
#[derive(Debug, Default, PartialEq, Eq, SalsaValue)]
struct MacroDefinitions {
    defs: Box<[(Symbol, bool, u32)]>,
    calls: Box<[Symbol]>,
}

impl MacroDefinitions {
    fn iter(&self) -> impl Iterator<Item = (&Symbol, bool, &[Symbol])> + '_ {
        self.defs.iter().scan(0, |start, (name, writes, end)| {
            let calls = &self.calls[*start as usize..*end as usize];
            *start = *end;
            Some((name, *writes, calls))
        })
    }
}
```

`macro_definitions`, before (signature, the buffers and the result):

```rust
#[salsa::tracked(returns(deref))]
fn macro_definitions(
    db: &Db,
    file: base_db::EditionedFileId,
) -> Box<[(Symbol, bool, Box<[Symbol]>)]> {
    let text = db.file_text(file.file_id(db)).text(db);
    if !text.contains("macro_rules") {
        return Box::default();
    }
    let lexed = syntax::LexedStr::new(file.edition(db), text);
    let tokens: Vec<usize> = (0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()).collect();
    let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
    let mut found = Vec::new();
```

```rust
        let (mut writes, mut calls, mut depth, mut groups) = (false, Vec::new(), 0usize, 0usize);
```

```rust
                        calls.push(Symbol::intern(lexed.text(tokens[at - 1])));
```

```rust
        found.push((name, writes, calls.into()));
        at += 1;
    }
    found.into()
}
```

After:

```rust
#[salsa::tracked(returns(ref), mem_tag = crate::db::INFER_TAG)]
fn macro_definitions(db: &Db, file: base_db::EditionedFileId) -> MacroDefinitions {
    let text = db.file_text(file.file_id(db)).text(db);
    if !text.contains("macro_rules") {
        return MacroDefinitions::default();
    }
    blaze_mem::frame::current(|frame| {
    let lexed = syntax::LexedStr::new(file.edition(db), text);
    let mut tokens = Vec::with_capacity_in(lexed.len(), frame);
    tokens.extend((0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()));
    let kind = |at: usize| tokens.get(at).map_or(SyntaxKind::EOF, |&i| lexed.kind(i));
    let mut found = Vec::new_in(frame);
    let mut calls = Vec::new_in(frame);
```

```rust
        let (mut writes, mut depth, mut groups) = (false, 0usize, 0usize);
```

```rust
                        calls.push(Symbol::intern(lexed.text(tokens[at - 1])));
```

```rust
        let end = u32::try_from(calls.len()).expect("more than 2^32 macro calls in a file");
        found.push((name, writes, end));
        at += 1;
    }
    MacroDefinitions { defs: found.drain(..).collect(), calls: calls.drain(..).collect() }
    })
}
```

(the closure's body is reindented by rustfmt when applied; `vec::Drain` is TrustedLen, so each
column is one exact allocation). The empty-file `Box::default()` returned before the frame is
opened: no frame is touched for a file without `macro_rules`.

`macros_writing_impls`, before (all of it is quoted at lines 992-1051 above in this tree):

```rust
#[salsa::tracked(returns(deref))]
fn macros_writing_impls(db: &Db, krate: Crate) -> Box<[(Symbol, bool)]> {
    fn add(known: &mut FxHashMap<Symbol, bool>, name: &Symbol, writes: bool) {
        *known.entry(name.clone()).or_default() |= writes;
    }
    let mut known = FxHashMap::default();
```

After (the whole function):

```rust
#[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
fn macros_writing_impls(db: &Db, krate: Crate) -> Box<[(Symbol, bool)]> {
    fn add<'a>(known: &mut FrameMap<'_, &'a Symbol, bool>, name: &'a Symbol, writes: bool) {
        *known.entry(name).or_default() |= writes;
    }
    blaze_mem::frame::current(|frame| {
        // Keyed by the names the queries below hold: a name is cloned once, into the result.
        let mut known = FrameMap::with_hasher_in(FxBuildHasher, frame);
        for dep in &krate.data(db).dependencies {
            for (name, writes) in macros_writing_impls(db, dep.crate_id) {
                add(&mut known, name, *writes);
            }
            if dep.crate_id.data(db).target_kind.is_proc_macro() {
                for (_, module) in crate_def_map(db, dep.crate_id).modules(db) {
                    for (name, item) in module.scope(db).macros() {
                        if matches!(item.def, MacroId::ProcMacroId(_)) {
                            add(&mut known, name.symbol(), true);
                        }
                    }
                }
            }
        }
        let mut calling = Vec::new_in(frame);
        let mut files = FrameSet::with_hasher_in(FxBuildHasher, frame);
        for (_, module) in crate_def_map(db, krate).modules(db) {
            let Some(file) = module.origin.file_id() else { continue };
            if !files.insert(file) {
                continue;
            }
            for (name, writes, calls) in macro_definitions(db, file).iter() {
                add(&mut known, name, writes);
                if !writes {
                    calling.push((name, calls));
                }
            }
        }
        let may_write = |known: &FrameMap<'_, &Symbol, bool>, call: &Symbol| {
            if *call == sym::macro_rules {
                false
            } else if *call == sym::include {
                true
            } else {
                known.get(call).copied().unwrap_or(true)
            }
        };
        loop {
            let before = calling.len();
            calling.retain(|&(name, calls)| {
                let writes = calls.iter().any(|it| may_write(&known, it));
                if writes {
                    known.insert(name, true);
                }
                !writes
            });
            if calling.len() == before {
                break;
            }
        }
        let mut sorted = Vec::with_capacity_in(known.len(), frame);
        sorted.extend(known.iter().map(|(&name, &writes)| (name, writes)));
        sorted.sort_unstable_by(|a, b| a.0.cmp_order(b.0));
        sorted.iter().map(|&(name, writes)| (name.clone(), writes)).collect()
    })
}
```

`known.get(call)` with `call: &Symbol` looks up `&Symbol` keys through `Borrow<Symbol>`. Every
key borrows a memo of `db` (`macros_writing_impls`' and `macro_definitions`' results, the def
map's macro names), which outlive the run. The only caller of `macro_definitions` is this
function (`rg -n "macro_definitions\(" crates`).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): as written, with `crate::frame_map`/`crate::frame_set(frame)` for `with_hasher_in`, a type on the sort closure's argument, `mem_tag = Infer`, and the body reindented inside `current`.

### M5. `impls_in_bodies` (887-982)

Per file. Before (the buffers and the result):

```rust
    let tokens: Vec<usize> = (0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()).collect();
```

```rust
    let mut braces = Vec::new();
```

```rust
    let mut initializers: Vec<usize> = Vec::new();
    let mut found = Vec::new();
```

```rust
    found.into()
```

After (body in `blaze_mem::frame::current(|frame| { .. })`, tag added):

```rust
    let mut tokens = Vec::with_capacity_in(lexed.len(), frame);
    tokens.extend((0..lexed.len()).filter(|&i| !lexed.kind(i).is_trivia()));
```

```rust
    let mut braces = Vec::new_in(frame);
```

```rust
    let mut initializers: Vec<usize, &Frame> = Vec::new_in(frame);
    let mut found = Vec::new_in(frame);
```

```rust
    found.drain(..).collect()
```

`tokens` takes the token count as its capacity in the frame (an upper bound, written once, never
regrown); the result is one exact allocation.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): as written (`tokens` typed `Vec<usize, &Frame>`), body reindented inside `current`, `mem_tag = Infer`.

### M6. `impl_holders` returns a frame `Vec` (775-807)

Called per module while finding impls (I3) and by `impl_work`. Before:

```rust
fn impl_holders(db: &Db, krate: Crate, scope: &ItemScope) -> Vec<DefWithBodyId> {
    let mut holders: Vec<DefWithBodyId> = scope.consts().map(DefWithBodyId::from).collect();
```

After:

```rust
fn impl_holders<'f>(
    db: &Db,
    frame: &'f Frame,
    krate: Crate,
    scope: &ItemScope,
) -> Vec<DefWithBodyId, &'f Frame> {
    let mut holders = Vec::new_in(frame);
    holders.extend(scope.consts().map(DefWithBodyId::from));
```

(the three `holders.extend(..)` and `holders` at the end unchanged). R7: a helper on the hit
path of the finding queries takes the caller's frame.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): as written. Its two callers in `InherentImpls`/`TraitImpls::collect_module` are I3's (`builder.frame`), left to db-mem-infer-b-impls (told by tmux).

### M7. `impl_work` (1136-1145): entry point outside queries

Called by `crates/blaze-engine/src/lib.rs:3718` once per crate before threads run the work.
Before:

```rust
pub fn impl_work(db: &Db, krate: Crate) -> Vec<ImplWork> {
    let def_map = crate_def_map(db, krate);
    let mut work = Vec::new();
    for (_, module) in def_map.modules(db) {
        let scope = module.scope(db);
        work.extend(scope.impls().map(ImplWork::Impl));
        work.extend(impl_holders(db, krate, scope).into_iter().map(ImplWork::Holder));
    }
    work
}
```

After:

```rust
pub fn impl_work(db: &Db, krate: Crate) -> Vec<ImplWork> {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let def_map = crate_def_map(db, krate);
        let mut work = Vec::new_in(frame);
        for (_, module) in def_map.modules(db) {
            let scope = module.scope(db);
            work.extend(scope.impls().map(ImplWork::Impl));
            work.extend(impl_holders(db, frame, krate, scope).into_iter().map(ImplWork::Holder));
        }
        work.iter().copied().collect()
    })
}
```

The returned `Vec<ImplWork>` (exact) is the caller's.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): as written, with `blaze_mem::Tag::Infer` for `crate::db::INFER_TAG`. The signature is unchanged, so its callers (`blaze-engine/src/lib.rs:3835`, `blaze-items/src/tests/narrow.rs:590`) need no edit and nothing was filed.

### M8. Tags only (method_resolution.rs)

`lookup_impl_method_stored` (428: `#[salsa::tracked(cycle_result =
lookup_impl_method_cycle_result, returns(clone), mem_tag = crate::db::INFER_TAG)]`),
`impl_value_item` (541), `trait_value_item` (547): `#[salsa::tracked(mem_tag =
crate::db::INFER_TAG)]`. No temporaries: `lookup_impl_method_stored` runs the solver
(mem-db-solver's) and returns a `(FunctionId, StoredGenericArgs)` value; the two item queries
scan a memo slice.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): the three tags, as `mem_tag = Infer`.

## opaques.rs (lane db + items)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (requests-items-mem; O4 nothing); `crates/hir-ty/src/opaques.rs` carries its 7 `mem_tag = Infer`.

### O1. `type_aliases_of` and its two queries (118-139)

Per impl and per trait asked. `filter_map(..).collect()` into `Box` grows and shrinks. Before:

```rust
fn type_aliases_of(items: &[(Name, AssocItemId)]) -> Box<[TypeAliasId]> {
    items
        .iter()
        .filter_map(|&(_, assoc_id)| match assoc_id {
            AssocItemId::TypeAliasId(it) => Some(it),
            AssocItemId::FunctionId(_) | AssocItemId::ConstId(_) => None,
        })
        .collect()
}
```

After (R7: takes the query's frame):

```rust
fn type_aliases_of(frame: &Frame, items: &[(Name, AssocItemId)]) -> Box<[TypeAliasId]> {
    let mut aliases = Vec::new_in(frame);
    aliases.extend(items.iter().filter_map(|&(_, assoc_id)| match assoc_id {
        AssocItemId::TypeAliasId(it) => Some(it),
        AssocItemId::FunctionId(_) | AssocItemId::ConstId(_) => None,
    }));
    aliases.iter().copied().collect()
}
```

`impl_type_aliases` / `trait_type_aliases`, before: `type_aliases_of(&impl_id.impl_items(db).items)`
after: `blaze_mem::frame::current(|frame| type_aliases_of(frame, &impl_id.impl_items(db).items))`
(and `trait_id.trait_items(db)` likewise), each `#[salsa::tracked(returns(deref), mem_tag =
crate::db::INFER_TAG)]`. `use blaze_mem::Frame;`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `type_aliases_of(frame, items)` collects in the frame, result exact; `impl_type_aliases`/`trait_type_aliases` open `frame::current`, tag `mem_tag = Infer`.

### O2. `rpit_hidden_types` (143-155)

Per function with return-position `impl Trait`. Before:

```rust
    let infer = InferenceResult::of(db, DefWithBodyId::from(function));
    let mut result = ArenaMap::new();
    for (opaque, hidden_type) in infer.return_position_impl_trait_types(db) {
        result.insert(opaque, StoredEarlyBinder::bind(hidden_type.store()));
    }
    result.shrink_to_fit();
    result
```

After:

```rust
    let infer = InferenceResult::of(db, DefWithBodyId::from(function));
    // `insert` grows the map to the index it is given: sized once to the highest, nothing is
    // shrunk after.
    let len = infer
        .return_position_impl_trait_types(db)
        .map(|(opaque, _)| opaque.into_raw().into_u32() as usize + 1)
        .max()
        .unwrap_or(0);
    let mut result = ArenaMap::with_capacity(len);
    for (opaque, hidden_type) in infer.return_position_impl_trait_types(db) {
        result.insert(opaque, StoredEarlyBinder::bind(hidden_type.store()));
    }
    result
```

with `mem_tag = crate::db::INFER_TAG`. Equal to before (`ArenaMap`'s `PartialEq` compares its
slots): `shrink_to_fit` truncated trailing empty slots, and the last slot is the highest index
inserted, so there are none. `return_position_impl_trait_types` is a filter over the result's
opaque table (`infer.rs:1700`), cheap to walk twice; no frame needed.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `ArenaMap::with_capacity(highest index + 1)`, no `shrink_to_fit`; tag `Infer`.

### O3. `defining_functions` (267-274), `define_opaque_functions` (296-302), `define_opaque_at` (380-385)

Per type alias, per crate, per file. Before:

```rust
    define_opaque_functions(db, type_alias.loc(db).module(db).krate(db))
        .iter()
        .copied()
        .filter(|&func| defined_type_aliases(db, func).any(|alias| alias == type_alias))
        .collect()
```

After:

```rust
    blaze_mem::frame::current(|frame| {
        let mut found = Vec::new_in(frame);
        found.extend(
            define_opaque_functions(db, type_alias.loc(db).module(db).krate(db))
                .iter()
                .copied()
                .filter(|&func| defined_type_aliases(db, func).any(|alias| alias == type_alias)),
        );
        found.iter().copied().collect()
    })
```

`define_opaque_functions`, before:

```rust
    let local = krate.data(db).origin.is_local();
    let mut result = Vec::new();
    collect(db, krate, crate_def_map(db, krate), local, &mut result);
    return result.into_boxed_slice();

    fn collect(
        db: &Db,
        krate: Crate,
```

After (the nested `collect`'s `result: &mut Vec<FunctionId>` parameter becomes `result: &mut
Vec<FunctionId, &Frame>`; its body is unchanged):

```rust
    let local = krate.data(db).origin.is_local();
    return blaze_mem::frame::current(|frame| {
        let mut result = Vec::new_in(frame);
        collect(db, krate, crate_def_map(db, krate), local, &mut result);
        result.iter().copied().collect()
    });

    fn collect(
        db: &Db,
        krate: Crate,
```

`define_opaque_at`, before:

```rust
    let text = db.file_text(file.file_id(db)).text(db);
    let found = text.match_indices("define_opaque").map(|(at, _)| TextSize::from(at as u32));
    found.collect()
```

After:

```rust
    let text = db.file_text(file.file_id(db)).text(db);
    blaze_mem::frame::current(|frame| {
        let mut found = Vec::new_in(frame);
        found.extend(text.match_indices("define_opaque").map(|(at, _)| TextSize::from(at as u32)));
        found.iter().copied().collect()
    })
```

Each with `mem_tag = crate::db::INFER_TAG`. `tait_hidden_types` (157): tag only; it already
sizes its map once (`ArenaMap::with_capacity(taits_count)`).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `defining_functions`, `define_opaque_functions` (nested `collect`/`holder` take `&mut Vec<FunctionId, &Frame>`) and `define_opaque_at` collect in `frame::current`, results exact; all three and `tait_hidden_types` tagged `Infer`.

### O4. `opaque_types_defined_by` and helpers

Fill the caller's `&mut Vec` (`next_solver/interner.rs:2277, 2289`, mem-db-solver's); no change
here. Whether that `Vec` moves to a frame is the solver's entry.

> forwarded to db (from requests, pass 17; told by tmux).
> seen by requests-items-mem: no change here, as the entry says (the `Vec` is mem-db-solver's).
> applied: nothing to change (requests-items-mem's note: O4 needs nothing).

## specialization.rs (lane db + items)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (requests-items-mem); in the tree at `crates/hir-ty/src/specialization.rs:325` (`constrained_params<'f>`).

### SP1. `constrained_params` and `parameters_for` in the frame (231-289, 294-361)

Per impl (E0207 check once per impl, `impl_has_unconstrained_params`). Two `FxHashSet`s and a
`SmallVec` before. `parameters_for`, before:

```rust
fn parameters_for<'db>(
    value: impl TypeVisitable<DbInterner<'db>>,
    include_nonconstraining: bool,
    out: &mut FxHashSet<Parameter>,
) {
    struct Collect<'a> {
        include_nonconstraining: bool,
        out: &'a mut FxHashSet<Parameter>,
    }
    impl<'db> TypeVisitor<DbInterner<'db>> for Collect<'_> {
```

After:

```rust
fn parameters_for<'db>(
    value: impl TypeVisitable<DbInterner<'db>>,
    include_nonconstraining: bool,
    out: &mut FrameSet<'_, Parameter>,
) {
    struct Collect<'a, 'f> {
        include_nonconstraining: bool,
        out: &'a mut FrameSet<'f, Parameter>,
    }
    impl<'db> TypeVisitor<DbInterner<'db>> for Collect<'_, '_> {
```

`constrained_params`, before:

```rust
fn constrained_params(db: &Db, impl_id: ImplId) -> Option<FxHashSet<Parameter>> {
```

```rust
    let mut constrained = FxHashSet::default();
```

```rust
    let projections: SmallVec<[_; 4]> = GenericPredicates::query_own_explicit(db, impl_id.into())
        .iter_identity()
        .filter_map(|it| match it.skip_norm_wip().kind().skip_binder() {
            // ..
        })
        .collect();
    let mut inputs = FxHashSet::default();
```

After:

```rust
fn constrained_params<'f>(
    db: &Db,
    frame: &'f Frame,
    impl_id: ImplId,
) -> Option<FrameSet<'f, Parameter>> {
```

```rust
    let mut constrained = FrameSet::with_hasher_in(FxBuildHasher, frame);
```

```rust
    let mut projections = Vec::new_in(frame);
    projections.extend(GenericPredicates::query_own_explicit(db, impl_id.into()).iter_identity().filter_map(
        |it| match it.skip_norm_wip().kind().skip_binder() {
            // .. unchanged ..
        },
    ));
    let mut inputs = FrameSet::with_hasher_in(FxBuildHasher, frame);
```

(`inputs.is_subset(&constrained)`: both are `FrameSet<'f, Parameter>`, which hashbrown requires.)
`impl_has_unconstrained_params`, before:

```rust
#[salsa::tracked(returns(copy))]
pub(crate) fn impl_has_unconstrained_params(db: &Db, impl_id: ImplId) -> bool {
    let Some(constrained) = constrained_params(db, impl_id) else { return false };
    type_or_const_params(db, impl_id)
        .any(|param| !constrained.contains(&Parameter::TypeOrConst(param)))
}
```

After:

```rust
#[salsa::tracked(returns(copy), mem_tag = crate::db::INFER_TAG)]
pub(crate) fn impl_has_unconstrained_params(db: &Db, impl_id: ImplId) -> bool {
    blaze_mem::frame::current(|frame| {
        let Some(constrained) = constrained_params(db, frame, impl_id) else { return false };
        type_or_const_params(db, impl_id)
            .any(|param| !constrained.contains(&Parameter::TypeOrConst(param)))
    })
}
```

`impl_unconstrained_params` is an entry point outside queries (`crates/blaze-items/src/tests/
coherence.rs:133`, re-exported at `lib.rs:115`). Before:

```rust
pub fn impl_unconstrained_params(db: &Db, impl_id: ImplId) -> Vec<TypeOrConstParamId> {
    let Some(constrained) = constrained_params(db, impl_id) else { return Vec::new() };
    type_or_const_params(db, impl_id)
        .filter(|&param| !constrained.contains(&Parameter::TypeOrConst(param)))
        .collect()
}
```

After:

```rust
pub fn impl_unconstrained_params(db: &Db, impl_id: ImplId) -> Vec<TypeOrConstParamId> {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        let Some(constrained) = constrained_params(db, frame, impl_id) else { return Vec::new() };
        let mut unconstrained = Vec::new_in(frame);
        unconstrained.extend(
            type_or_const_params(db, impl_id)
                .filter(|&param| !constrained.contains(&Parameter::TypeOrConst(param))),
        );
        unconstrained.iter().copied().collect()
    })
}
```

Imports: `use blaze_mem::Frame;`, `use rustc_hash::FxBuildHasher;` (replacing `FxHashSet`), `use
crate::method_resolution::{FrameSet, SelfTyKey, TraitImpls};`. `specializes_query` (55): tag only.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `parameters_for`/`Collect<'a, 'f>` over `FrameSet`, `constrained_params(db, frame, impl_id)` with frame sets and a frame `Vec` of projections, `impl_has_unconstrained_params` in `frame::current`, `impl_unconstrained_params` in `frame::task(INFER_TAG, ..)`, `specializes_query` tagged. Departure: `crate::{FrameSet, frame_set}` from `lib.rs` (mem-db-infer section 5) in place of `method_resolution::FrameSet` and `with_hasher_in`; `SmallVec` stays for `specialized_ancestor`.

## drop.rs (lane db + items)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (requests-items-mem); in the tree at `crates/hir-ty/src/drop.rs:106` (`has_drop_glue_in`).

### D1. `has_drop_glue`'s visited set (100-108)

Per call, from MIR building, borrowck, codegen, lints and hir (`rg -n "has_drop_glue\(" crates`:
hir-ty-mir, hir-ty-borrowck, blaze-codegen, blaze-diagnostics, hir, hir-ty-share, hir-ty-ub).
Before:

```rust
pub fn has_drop_glue<'db>(infcx: &InferCtxt<'db>, ty: Ty<'db>, env: ParamEnv<'db>) -> DropGlue {
    has_drop_glue_impl(infcx, ty, env, &mut FxHashSet::default())
}

fn has_drop_glue_impl<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
    visited: &mut FxHashSet<Ty<'db>>,
) -> DropGlue {
```

After:

```rust
/// [`has_drop_glue_in`] in a task of its own, for a caller with no frame at hand.
pub fn has_drop_glue<'db>(infcx: &InferCtxt<'db>, ty: Ty<'db>, env: ParamEnv<'db>) -> DropGlue {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| has_drop_glue_in(frame, infcx, ty, env))
}

/// Whether dropping a `ty` runs code, its visited set in the caller's `frame`.
pub fn has_drop_glue_in<'db>(
    frame: &Frame,
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
) -> DropGlue {
    has_drop_glue_impl(infcx, ty, env, &mut FrameSet::with_hasher_in(FxBuildHasher, frame))
}

fn has_drop_glue_impl<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    env: ParamEnv<'db>,
    visited: &mut FrameSet<'_, Ty<'db>>,
) -> DropGlue {
```

The recursive calls pass `visited` on unchanged. Callers inside a query with a frame (MIR
building's `frame`, borrowck's) move to `has_drop_glue_in(frame, ..)` in their own parts
(mem-db-mir, mem-db-checks); `has_drop_glue` stays for the rest. `frame::task` nests inside a
running query's frame (R1: "entry points outside a query"), so the old name is correct wherever
it is called. Imports: `use blaze_mem::Frame;`, `use rustc_hash::FxBuildHasher;` (replacing
`FxHashSet`), `use crate::method_resolution::{FrameSet, SelfTyKey, TraitImpls};`.
`destructor` (20): tag only. `is_freeze` has no temporaries.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `has_drop_glue` opens `frame::task(INFER_TAG, ..)` over the new `pub fn has_drop_glue_in(frame, ..)`, `has_drop_glue_impl` takes `&mut FrameSet<'_, Ty>` (`crate::frame_set`), `destructor` tagged `Infer`. Callers with a frame move to `has_drop_glue_in` in their own parts.

## inhabitedness.rs (lane db)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (db-mem-infer-b); in the tree at `crates/hir-ty/src/inhabitedness.rs:33`, `:62`, `:161` (`_in` forms, `frame_set(frame)`).

### N1. `UninhabitedFrom::recursive_ty` in the frame (22-49, 51-58, 101-104)

Per call (match checking per arm and scrutinee, MIR lowering at `hir-ty-mir/src/lower.rs:3297`,
`hir-ide` and `hir-ty-matchck` match checks, `hir/src/match_lints.rs:151`,
`blaze-diagnostics/.../statements.rs:421`). Before:

```rust
pub fn is_ty_uninhabited_from<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    let _p = tracing::info_span!("is_ty_uninhabited_from", ?ty).entered();
    let mut uninhabited_from = UninhabitedFrom::new(infcx, target_mod, env);
    let inhabitedness = ty.visit_with(&mut uninhabited_from);
    inhabitedness == BREAK_VISIBLY_UNINHABITED
}
```

After:

```rust
pub fn is_ty_uninhabited_from<'db>(
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    blaze_mem::frame::task(crate::db::INFER_TAG, |frame| {
        is_ty_uninhabited_from_in(frame, infcx, ty, target_mod, env)
    })
}

/// [`is_ty_uninhabited_from`], its set of the types being visited in the caller's `frame`.
pub fn is_ty_uninhabited_from_in<'db>(
    frame: &Frame,
    infcx: &InferCtxt<'db>,
    ty: Ty<'db>,
    target_mod: ModuleId,
    env: ParamEnv<'db>,
) -> bool {
    let _p = tracing::info_span!("is_ty_uninhabited_from", ?ty).entered();
    let mut uninhabited_from = UninhabitedFrom::new(frame, infcx, target_mod, env);
    let inhabitedness = ty.visit_with(&mut uninhabited_from);
    inhabitedness == BREAK_VISIBLY_UNINHABITED
}
```

`is_enum_variant_uninhabited_from` the same way: the existing name opens `frame::task` and calls
`is_enum_variant_uninhabited_from_in(frame, infcx, variant, subst, target_mod, env)`, which holds
the current body with `UninhabitedFrom::new(frame, infcx, target_mod, env)`. The struct, before:

```rust
struct UninhabitedFrom<'a, 'db> {
    target_mod: ModuleId,
    recursive_ty: FxHashSet<Ty<'db>>,
```

After:

```rust
struct UninhabitedFrom<'a, 'db> {
    target_mod: ModuleId,
    recursive_ty: FrameSet<'a, Ty<'db>>,
```

`new`, before:

```rust
    fn new(infcx: &'a InferCtxt<'db>, target_mod: ModuleId, env: ParamEnv<'db>) -> Self {
        Self { target_mod, recursive_ty: FxHashSet::default(), max_depth: 500, infcx, env }
    }
```

After:

```rust
    fn new(
        frame: &'a Frame,
        infcx: &'a InferCtxt<'db>,
        target_mod: ModuleId,
        env: ParamEnv<'db>,
    ) -> Self {
        Self {
            target_mod,
            recursive_ty: FrameSet::with_hasher_in(FxBuildHasher, frame),
            max_depth: 500,
            infcx,
            env,
        }
    }
```

Imports: `use blaze_mem::Frame;`, `use rustc_hash::FxBuildHasher;` (replacing `FxHashSet`), `use
crate::method_resolution::FrameSet;`. Callers with a frame (MIR lowering, match checking) move to
the `_in` forms in their parts. Seen while reading, not changed (semantics): `visit_ty` inserts
`ty` before normalizing an alias and removes the normalized `ty`, so an alias type stays in
`recursive_ty` for the rest of the walk. rustc's `inhabited_predicate` has no such set; whether
this differs from rustc for an alias reached twice is a correctness question for lane db, filed
here only as a note.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (21:25): as written, with mem-db-infer.md section 5's `crate::{FrameSet, frame_set}` (`frame_set(frame)` for `FrameSet::with_hasher_in`) in place of `crate::method_resolution::FrameSet`. Callers' moves to the `_in` forms are their parts'.

## representability.rs (lane db)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (db-mem-infer-b, then db-apply-ty's bits on top); in the tree at `crates/hir-ty/src/representability.rs:101` and `:128`.

### R1. `params_in_repr` builds its words in the frame (93-155, 175-201)

Per ADT. `rest.resize` grew the `ThinVec` per word and `shrink_to_fit` reallocated it (R4).
Before:

```rust
impl ParamsInRepr {
    #[inline]
    fn get(&self, index: usize) -> bool {
        // ..
    }

    fn insert(&mut self, index: usize) {
        let bit = 1 << (index % 64);
        match index / 64 {
            0 => self.first |= bit,
            at => {
                if self.rest.len() < at {
                    self.rest.resize(at, 0);
                }
                self.rest[at - 1] |= bit;
            }
        }
    }
}
```

After (`get` unchanged; `insert` moves to a builder):

```rust
/// A [`ParamsInRepr`] while its ADT's fields are walked, the words past the first in a frame.
struct ParamsInReprBuilder<'f> {
    first: u64,
    rest: Vec<u64, &'f Frame>,
}

impl<'f> ParamsInReprBuilder<'f> {
    fn new(frame: &'f Frame) -> Self {
        ParamsInReprBuilder { first: 0, rest: Vec::new_in(frame) }
    }

    fn insert(&mut self, index: usize) {
        let bit = 1 << (index % 64);
        match index / 64 {
            0 => self.first |= bit,
            at => {
                if self.rest.len() < at {
                    self.rest.resize(at, 0);
                }
                self.rest[at - 1] |= bit;
            }
        }
    }

    /// The set, its words past the first in one allocation of their number.
    fn finish(self) -> ParamsInRepr {
        let mut rest = ThinVec::with_capacity(self.rest.len());
        rest.extend_from_slice(&self.rest);
        ParamsInRepr { first: self.first, rest }
    }
}
```

`params_in_repr`, before:

```rust
#[salsa::tracked(cycle_result = params_in_repr_cycle, returns(ref))]
fn params_in_repr(db: &Db, def_id: AdtId) -> ParamsInRepr {
    let mut params_in_repr = ParamsInRepr::default();
```

```rust
    params_in_repr.rest.shrink_to_fit();
    params_in_repr
}

fn params_in_repr_ty<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    params_in_repr: &mut ParamsInRepr,
) {
```

After:

```rust
#[salsa::tracked(cycle_result = params_in_repr_cycle, returns(ref), mem_tag = crate::db::INFER_TAG)]
fn params_in_repr(db: &Db, def_id: AdtId) -> ParamsInRepr {
    blaze_mem::frame::current(|frame| {
    let mut params_in_repr = ParamsInReprBuilder::new(frame);
```

```rust
    params_in_repr.finish()
    })
}

fn params_in_repr_ty<'db>(
    db: &'db Db,
    ty: Ty<'db>,
    params_in_repr: &mut ParamsInReprBuilder<'_>,
) {
```

(reindented by rustfmt). Equal to before: `rest` only grows to the word of the highest index
inserted, which is then non-zero, so the frame words and the old shrunk `ThinVec` hold the same
words. `ThinVec::with_capacity(0)` allocates nothing (the shared empty header), so nearly every
ADT still allocates nothing. Tests (176-201), before: `let mut set = ParamsInRepr::default();` then
`set.insert(..)` and `set.get(..)`, `set.rest.len()`. After, each test body inside
`blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| { .. })` with `let mut set =
ParamsInReprBuilder::new(frame);`, the inserts, then `let set = set.finish();` before the `get`
and `rest` assertions (both tests keep their assertions: bits 0, 5, 63, 64, 130 and `rest.len()
== 2`; few params allocate nothing). `use super::{ParamsInRepr, ParamsInReprBuilder};`. Import
`use blaze_mem::Frame;`. `representability` (28): tag only.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (21:25): as written, tags as `mem_tag = Infer` (salsa M2 takes a variant ident). The dedup request onto `blaze_ids::bits` (`v2/requests/db.md` "dedup: bits: hir-ty `ParamsInRepr`") is still pending; db-apply-ty told to apply it to the builder form. Done by db-apply-ty (21:40): `ParamsInRepr = Bits<SmallVec<[u64; 1]>>`, the builder's words a frame `Bits<Vec<u64, &Frame>>`; tags and `frame::current` kept.

## traits.rs (lane db + items)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (requests-items-mem, db-mem-infer-b); `crates/hir-ty/src/traits.rs` has both `mem_tag = Infer`.

### T1. `where_predicate_must_hold` (395-470): entry point outside queries

Called by `crates/hir/src/source_analyzer.rs:395` per IDE request. Before:

```rust
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let generics = OnceCell::<Generics<'db>>::new();
    let mut ctx = TyLoweringContext::new(
        db,
        resolver,
        store,
        def,
        generic_def,
        &generics,
        LifetimeElisionKind::Infer,
        LifetimeLoweringMode::Bound,
    )
    .with_interning_mode(LoweringMode::Ide);
    let clauses =
        ctx.lower_where_predicate(predicate, false).map(|(clause, _)| clause).collect::<Vec<_>>();
```

After: the whole body inside `blaze_mem::frame::task(crate::db::INFER_TAG, |frame| { .. })`
(its `return`s return from the closure, which is the function's value), with:

```rust
    let interner = DbInterner::new_with(db, env.krate);
    let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);
    let generics = OnceCell::<Generics<'db>>::new();
    let mut ctx = TyLoweringContext::new(
        db,
        resolver,
        store,
        def,
        generic_def,
        &generics,
        LifetimeElisionKind::Infer,
        LifetimeLoweringMode::Bound,
        frame,
    )
    .with_interning_mode(LoweringMode::Ide);
    let mut clauses = Vec::new_in(frame);
    clauses.extend(ctx.lower_where_predicate(predicate, false).map(|(clause, _)| clause));
```

(`for clause in clauses` inside `infcx.probe(|snapshot| ..)` moves the frame `Vec` as it moved
the global one.)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: the body runs in `frame::task(INFER_TAG, ..)` and the clauses are a frame `Vec`.
> held by requests-items-mem: the `frame` argument to `TyLoweringContext::new` waits on mem-db-infer-lower's R2 (`lower.rs`'s `new` takes no frame yet); whoever lands R2 adds `frame,` after `LifetimeLoweringMode::Bound,` here.

### T2. Tags only

`inherent_impl_coherent` (475), `orphan_rules_hold` (569): `#[salsa::tracked(mem_tag =
crate::db::INFER_TAG)]`. Both return `bool` and allocate nothing of their own (the solver's
`ObligationCtxt` aside). `implements_traits_unique` returns `traits.iter().map(..).collect()`,
an exact `Vec<bool>` the caller (`blaze-items/src/form.rs:753`) owns: no change.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `inherent_impl_coherent` and `orphan_rules_hold` take `mem_tag = Infer`.

## Callers outside hir-ty (lane db: `crates/hir`)

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): applied below (db-mem-infer-b, H1/H2); in the tree at `crates/hir/src/attrs.rs:558` (`frame::task(Tag::Infer, ..)`).

### H1. `crates/hir/src/attrs.rs:556`

Per doc-link resolution. Before:

```rust
    let ctx = MethodResolutionContext {
        infcx: &infcx,
        resolver: &resolver,
        param_env: param_env.param_env,
        traits_in_scope: &traits_in_scope,
        edition: krate.data(db).edition,
        features,
        call_span: hir_ide::Span::Dummy,
        receiver_span: hir_ide::Span::Dummy,
    };
    let resolution =
        ctx.probe_for_name(method_resolution::Mode::Path, name.clone(), ty.ty.skip_binder());
```

After:

```rust
    let resolution = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        let ctx = MethodResolutionContext {
            infcx: &infcx,
            resolver: &resolver,
            param_env: param_env.param_env,
            traits_in_scope: &traits_in_scope,
            edition: krate.data(db).edition,
            features,
            call_span: hir_ide::Span::Dummy,
            receiver_span: hir_ide::Span::Dummy,
            frame,
        };
        ctx.probe_for_name(method_resolution::Mode::Path, name.clone(), ty.ty.skip_binder())
    });
```

(`PickResult<'db>` borrows nothing of the frame.)

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (21:55): as written, once P1's `frame` field was in; the literal keeps db-mem-infer-a-ctx's `TraitsInScope::Set(TraitSet::collect(..))` (frontend-fast-method's type, superseding ctx M1). `blaze-mem` was already in `crates/hir/Cargo.toml`.

### H2. `crates/hir/src/lib.rs:5782-5814` (`Type::with_method_resolution`)

Per IDE method/path iteration (completion, `iterate_method_candidates*`,
`iterate_path_candidates*`). Before:

```rust
        let ctx = MethodResolutionContext {
            infcx: &infcx,
            resolver,
            param_env: environment.param_env,
            traits_in_scope,
            edition: resolver.krate().data(db).edition,
            features,
            call_span: hir_ide::Span::Dummy,
            receiver_span: hir_ide::Span::Dummy,
        };
        f(&ctx)
```

After:

```rust
        blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let ctx = MethodResolutionContext {
                infcx: &infcx,
                resolver,
                param_env: environment.param_env,
                traits_in_scope,
                edition: resolver.krate().data(db).edition,
                features,
                call_span: hir_ide::Span::Dummy,
                receiver_span: hir_ide::Span::Dummy,
                frame,
            };
            f(&ctx)
        })
```

`R` is chosen by the caller outside the closure, so it cannot borrow the frame; `probe_all`'s
iterator is consumed inside `f` at both call sites (5871, 5972).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b (21:55): as written. `traits_in_scope,` left as it was for db-mem-infer-a-ctx's `TraitsInScope::Set(..)` edit.

## Tracked functions: tag and memo result

Every one gets `mem_tag = crate::db::INFER_TAG` (R6). "Result allocations" counts heap blocks
of one memoized value.

| line | function | result | result allocations | entry |
|---|---|---|---|---|
| method_resolution.rs:428 | `lookup_impl_method_stored` | `(FunctionId, StoredGenericArgs)` | 0 | M8 |
| method_resolution.rs:541 | `impl_value_item` | `Option<CandidateId>` | 0 | M8 |
| method_resolution.rs:547 | `trait_value_item` | `Option<CandidateId>` | 0 | M8 |
| method_resolution.rs:559 | `trait_value_items` | `Box<[(Name, TraitId, CandidateId)]>` | 1 exact | M1 |
| method_resolution.rs:583 | `in_crate_trait_value_items` | `Box<[(TraitId, CandidateId)]>` | 1 exact (0 when empty) | M2 |
| method_resolution.rs:683 | `loaded_crates` | `Box<[Crate]>` | 1 exact | M3 |
| method_resolution.rs:887 | `impls_in_bodies` | `Box<[(TextSize, BodyMark)]>` | 1 exact (0 when empty) | M5 |
| method_resolution.rs:992 | `macros_writing_impls` | `Box<[(Symbol, bool)]>` | 1 exact | M4 |
| method_resolution.rs:1057 | `macro_definitions` | `MacroDefinitions` | 2 exact (0 without `macro_rules`; was 1 + one per macro) | M4 |
| method_resolution.rs:1165 | `crates_containing_incoherent_inherent_impls` | `Box<[Crate]>` | 1 exact | I6 |
| method_resolution.rs:1332 | `InherentImpls::for_crate` | `InherentImpls` | 4 exact (2 `SelfTys` columns, 2 `Runs` columns) | I4 |
| method_resolution.rs:1346 | `InherentImpls::in_crate_for_self_ty` | `Box<[ImplId]>` | 1 exact | I6 |
| method_resolution.rs:1355 | `InherentImpls::for_block` | `Option<Box<InherentImpls>>` | 5 exact (0 for a block without inherent impls) | I4 |
| method_resolution.rs:1368 | `InherentImpls::in_block_for_self_ty` | `Box<[ImplId]>` | 1 exact | I6 |
| method_resolution.rs:1531 | `TraitImpls::for_crate` | `Arc<TraitImpls>` | 1 `Arc` + 10 exact columns | I4 |
| method_resolution.rs:1543 | `TraitImpls::for_block` | `Option<Box<TraitImpls>>` | 1 + 10 (0 for a block without trait impls) | I4 |
| method_resolution.rs:1555 | `TraitImpls::for_crate_and_deps` | `Box<[Arc<TraitImpls>]>` | 1 exact | I6 |
| method_resolution.rs:1562 | `TraitImpls::in_crate_and_deps_for_trait_and_self_ty` | `Box<[AnyImplId]>` | 1 exact | I6 |
| method_resolution.rs:1579 | `TraitImpls::blanket_in_crate_and_deps` | `Box<[ImplId]>` | 1 exact | I6 |
| method_resolution.rs:1593 | `TraitImpls::in_crate_for_trait_and_self_ty` | `Box<[ImplId]>` | 1 exact | I6 |
| method_resolution.rs:1604 | `TraitImpls::in_block_for_trait_and_self_ty` | `Box<[ImplId]>` | 1 exact | I6 |
| method_resolution.rs:1619 | `TraitImpls::in_crate_for_trait` | `Box<[ImplId]>` | 1 exact | I6 |
| method_resolution.rs:1637 | `TraitImpls::in_crate_and_deps_for_trait` | `Box<[ImplId]>` | 1 exact | I6 |
| traits.rs:475 | `inherent_impl_coherent` | `bool` | 0 | T2 |
| traits.rs:569 | `orphan_rules_hold` | `bool` | 0 | T2 |
| specialization.rs:55 | `specializes_query` | `bool` | 0 | SP1 |
| specialization.rs:294 | `impl_has_unconstrained_params` | `bool` | 0 | SP1 |
| opaques.rs:120 | `impl_type_aliases` | `Box<[TypeAliasId]>` | 1 exact | O1 |
| opaques.rs:126 | `trait_type_aliases` | `Box<[TypeAliasId]>` | 1 exact | O1 |
| opaques.rs:143 | `rpit_hidden_types` | `ArenaMap<..>` | 1 exact (0 when none) | O2 |
| opaques.rs:157 | `tait_hidden_types` | `ArenaMap<..>` | 1 exact | O3 |
| opaques.rs:267 | `defining_functions` | `Box<[FunctionId]>` | 1 exact | O3 |
| opaques.rs:296 | `define_opaque_functions` | `Box<[FunctionId]>` | 1 exact | O3 |
| opaques.rs:380 | `define_opaque_at` | `Box<[TextSize]>` | 1 exact | O3 |
| drop.rs:20 | `destructor` | `Option<ImplId>` | 0 | D1 |
| representability.rs:28 | `representability` | `Representability` | 0 | R1 |
| representability.rs:131 | `params_in_repr` | `ParamsInRepr` | 0 for ≤ 64 params, else 1 exact | R1 |

`Box<[T]>` of an empty slice or iterator allocates nothing. A `#[salsa::tracked] impl` block's
own attribute (1330, 1529) takes no tag; its methods do.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05), the `InherentImpls`/`TraitImpls` rows and `crates_containing_incoherent_inherent_impls`: `mem_tag = Infer` on all of them and on requests-impls' new ones (`InherentImpls::{for_module, modules_by_self_ty, modules_of_self_ty, in_module_for_self_ty}`, `TraitImpls::{for_module, modules_by_trait, modules_of_trait, in_module_for_trait, for_crate_and_trait}`). The other rows are db-mem-infer-b-queries' (M1-M8) and requests-items-mem's.
> applied by db-mem-infer-b-queries (21:59): the M1 to M8 rows (method_resolution.rs 428 to 1057), as `mem_tag = Infer`; the I, T, SP, O, D, R rows are other appliers'.

## Entry points outside queries (R1: `frame::task(INFER_TAG, ..)`)

| function | callers | entry |
|---|---|---|
| `Type::with_method_resolution` (hir) | `iterate_method_candidates_split_inherent`, `iterate_path_candidates_split_inherent` and their wrappers (hir/src/lib.rs 5835, 5944) | H2 |
| `resolve_assoc_or_field` path probe (hir/src/attrs.rs:556) | doc-link resolution | H1 |
| `method_resolution::impl_work` | blaze-engine/src/lib.rs:3718 | M7 |
| `has_drop_glue` | hir-ty-mir, hir-ty-borrowck, blaze-codegen, blaze-diagnostics, hir, hir-ty-share, hir-ty-ub | D1 |
| `is_ty_uninhabited_from`, `is_enum_variant_uninhabited_from` | hir-ide match_check, hir match_lints.rs:151, hir-ty-mir lower.rs:3297, hir-ty-matchck cx.rs/visit.rs, blaze-diagnostics statements.rs:421 | N1 |
| `specialization::impl_unconstrained_params` | blaze-items tests/coherence.rs:133 | SP1 |
| `traits::where_predicate_must_hold` | hir/src/source_analyzer.rs:395 | T1 |

No frame needed (no temporaries of their own, results owned by the caller or none):
`autoderef::autoderef` (inline `ArrayVec`, P6), `traits::implements_trait_unique*`,
`implements_traits_unique`, `could_unify*`, `impl_may_apply`, `is_impossible_associated_item`,
`method_resolution::with_incoherent_inherent_impls`, `simplified_type_module`,
`InherentImpls`/`TraitImpls` lookups and `for_each_*` visitors, `frozen::set/served/encode`
(`encode`'s output is the caller's, I5). `TraitImpls::for_each_*` (and
`InherentImpls::for_each_crate_and_block`) take `&mut dyn FnMut`: no allocation, so nothing here
for memory, but they are hot `dyn` (called per trait goal from `next_solver/interner.rs:1921,
2055` and `hir/src/lib.rs:4071, 5648`, all with closures). Turning them into generics (`F:
FnMut(&TraitImpls<'db>) -> R`, passing `&mut *for_each` down) is lane dedyn's; no entry in this
request touches those signatures or adds a `dyn`, so its edits and these apply in either order.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-queries (21:59): the `impl_work` row (M7), as `frame::task(blaze_mem::Tag::Infer, ..)`.

## R7 hit-path helpers (take the caller's frame)

`impl_holders` (M6), `type_aliases_of` (O1), `constrained_params`/`parameters_for` (SP1),
`has_drop_glue_in`/`has_drop_glue_impl` (D1), `is_ty_uninhabited_from_in`,
`is_enum_variant_uninhabited_from_in`, `UninhabitedFrom::new` (N1), `InherentImplsBuilder`,
`TraitImplsBuilder`, `Runs::from_groups`, `ranks`, `order_by_hash`, `group_of` (I2),
`frozen::trait_impls`/`inherent_impls` (I5), `TraitValueItems::new_in` (P4),
`ProbeAllChoice::new` (P5), `ParamsInReprBuilder::new` (R1). None opens `current` or a scratch of
its own.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-method-mem (22:08): nothing left to write in lane items' files; checked that `impl_holders`, `TraitValueItems::new_in`, `ProbeAllChoice::new` and the builders (`SelfTyRunsBuilder::new`, `TraitImplsBuilder::new`, db-mem-infer-b-impls' I2) take the caller's frame and open no `current` of their own. frontend-fast-method's newer helpers follow the same rule: `ScopeItems::of_traits`/`TraitsInScope::named` take the probe's frame; only the query `module_items_named` opens `current`.

## Left as is, with why

Every hit of `.clone()`, `Vec::new()`, `vec![`, `.collect()`, `.to_vec()`, `Box::new(`,
`FxHashMap::default()`, `FxHashSet::default()`, `SmallVec`, `ThinVec`, `IndexMap` in my files
that is not changed above (`rg` over the eleven files):

- `probe.rs:90` `ApplicableCandidates = SmallVec<[&Candidate; 4]>`, collected at 1608: the
  candidates of one pick attempt that match; 0 or 1 almost always, 5 or more only on an
  ambiguity error. Never spills in practice (R3 keeps it).
- `probe.rs:697` and `probe.rs:1358` `.collect()` into `MethodError::Ambiguity(Vec)`: the error
  value escapes to the caller (and into diagnostics); error path only.
- `probe.rs:2007, 2230` `shadowed_candidates: vec![]`: empty, no allocation; `probe.rs:2088`
  `.collect()` into `Pick::shadowed_candidates`: part of the `Pick` returned, owned by the caller,
  built only when a subtrait pick collapses.
- `probe.rs:2026-2051` `collapse_candidates_to_subtrait_pick`'s two `FxHashSet` collects,
  `to_vec`, `vec![]`: run only with `#![feature(supertrait_item_shadowing)]` and two or more
  applicable candidates (an ambiguity otherwise). Cold; `supertrait_def_ids` is
  `rustc_type_ir`'s and allocates on its own.
- `probe.rs:834` `candidate.clone()`: the `Candidate` stored for the IDE (P5), owned.
- `probe.rs` per-step `make_query_response_ignoring_pending_obligations` and
  `OriginalQueryValues::default()` (317), `PredefinedOpaques::new_from_iter` (318): the solver's
  query-response types (region constraints, opaque type lists, var values). mem-db-solver's;
  they are the probe's dominant remaining allocations (see "Speed").
- `confirm.rs:132, 135, 401, 408` `.into()`: generic-arg conversions, no allocation.
  `predicates_require_illegal_sized_bound`'s `elaborate` and `upcast_choices`: solver code.
- `method_resolution.rs:256, 259` `.into()`: generic-arg conversions. `lookup_method_for_operator`'s
  `PredicateObligations::new()` + `extend` (in `impl InferenceTable`): the solver's type and the
  value returned in `InferOk`, owned by the caller.
- `method_resolution.rs:513` `name.clone()`: `Name` is an 8-byte `Copy`-like handle; no
  allocation.
- `method_resolution.rs:1277` `range.clone()`: `Range<usize>`.
- `method_resolution.rs:1352, 1375, 1600, 1612` `.into()` from `&[ImplId]`: exact `Box`, the
  memo result.
- `method_resolution.rs:1364, 1552` `Box::new(result)`, `1535, 1540` `Arc::new`: the memo's
  handle, one allocation each.
- `method_resolution.rs:1557` `Self::for_crate(db, dep).clone()`: an `Arc` refcount per loaded
  crate into the exact result; sharing the tables is the point of the query.
- `method_resolution.rs:1877-1883` (`inert_attribute_tests`): test code.
- `frozen.rs:36` `StdArc::new(maps)`: the input's value, once per `set`. `frozen.rs:61, 87, 189,
  214, 224, 296, 324` `to_owned()`/`format!`: error strings on corrupt bytes.
- `traits.rs:179` `.collect()` into `Vec<bool>`: exact (`Map<slice::Iter>`), the caller's result.
- `traits.rs:149, 203, 218` `&mut dyn FnMut`: parameters, no allocation; whether they are hot
  `dyn` is lane dedyn's to judge and change.
- `specialization.rs:215` `SmallVec<[ImplId; 4]>` of ancestors: impls a specializing impl
  specializes; 0 to 2 (only under `feature(specialization)`), never spills in practice.
- `representability.rs:190` `collect()` in a test.
- `opaques.rs` `tait_hidden_types`' `ArenaMap::with_capacity`: already exact.
- `autoderef.rs` `structurally_normalize_ty`'s `ObligationCtxt` and `into_pending_obligations`,
  `AutoderefSnapshot::obligations: PredicateObligations`: solver types, mem-db-solver's.
- `crates_containing_incoherent_inherent_impls`: `Crate::transitive_deps` returns base-db's owned
  `Vec` (base-db, lane db; a `_in(frame)` form would be base-db's entry).
- `impls_in_bodies`/`macro_definitions`: `syntax::LexedStr::new` allocates its token arrays
  (lane syntax's).
- `loaded_crates`: `file.parse(db)`, `parse_macro_expansion`, `to_flat()`: syntax and expansion
  memos.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): notes, nothing to apply; db-mem-infer-b's hot_lint (22:10) found only these listed lines left.

## Tests (R8; budget tests are mem-db-infer's)

Semantics stay pinned by the tests that exist; each change above keeps results `PartialEq`-equal
to before, and these exercise them:

- `crates/hir-ide/src/tests/method_resolution.rs` and the `hir-ide` inference tests: probes, picks,
  autoderef chains (`ArrayVec` steps), receiver adjustments (C2), completion's `probe_all` order
  (P5).
- `crates/hir-ide/src/tests/frozen.rs` (frozen impls in `Mode::Check`: read tables equal found
  ones, I5) and `crates/blaze-items/src/tests/narrow.rs` (`TraitValueItems` against
  `trait_value_item`, P4; the narrow queries of I6, M1, M2).
- `crates/blaze-items/src/tests/coherence.rs` (E0207, SP1); representability's two tests (R1).

New unit tests, beside the code (`method_resolution.rs`'s test module, `#[cfg(test)] mod
columns_tests`), against the old construction kept in the test as a reference:

```rust
#[cfg(test)]
mod columns_tests {
    use blaze_mem::{Tag, frame};
    use rustc_type_ir::{IntTy, UintTy};

    use super::{Runs, SimplifiedType, hash_self_ty, order_by_hash, ranks};

    /// The old `Runs::new`: each run copied after the one before.
    fn reference(runs: &[Vec<u32>]) -> (Vec<u32>, Vec<u32>) {
        let mut starts = vec![0];
        let mut values = Vec::new();
        for run in runs {
            values.extend_from_slice(run);
            starts.push(values.len() as u32);
        }
        (starts, values)
    }

    #[test]
    fn runs_from_groups_equal_runs_copied_in_order() {
        frame::task(Tag::Infer, |frame| {
            // Values of 4 groups, interleaved; groups go in the order 2, 0, 3, 1.
            let values = [(0, 10), (2, 20), (0, 11), (3, 30), (2, 21), (0, 12)];
            let order = [2u32, 0, 3, 1];
            let rank = ranks(frame, &order);
            let runs = Runs::from_groups(frame, rank, &values);
            let by_group = |g: u32| values.iter().filter(|it| it.0 == g).map(|it| it.1).collect();
            let (starts, flat) = reference(&order.map(by_group));
            assert_eq!(&*runs.starts, &starts[..]);
            assert_eq!(&*runs.values, &flat[..]);
            assert_eq!(runs.run(3), &[] as &[u32]);
        });
    }

    #[test]
    fn runs_of_nothing_have_one_start() {
        frame::task(Tag::Infer, |frame| {
            let runs = Runs::<u32>::from_groups(frame, &[], &[]);
            assert_eq!(&*runs.starts, &[0]);
            assert!(runs.values.is_empty());
        });
    }

    #[test]
    fn order_by_hash_is_the_stable_sort_by_hash() {
        frame::task(Tag::Infer, |frame| {
            let tys: [SimplifiedType<'static>; 6] = [
                SimplifiedType::Bool,
                SimplifiedType::Int(IntTy::I32),
                SimplifiedType::Uint(UintTy::U8),
                SimplifiedType::Char,
                SimplifiedType::Str,
                SimplifiedType::Never,
            ];
            // Six groups, three hashes: some share one, and ties keep first-seen order.
            let groups: Vec<(u64, SimplifiedType<'static>)> =
                tys.iter().map(|&ty| (hash_self_ty(&ty) % 3, ty)).collect();
            let order = order_by_hash(frame, &groups);
            let mut expected: Vec<u32> = (0..groups.len() as u32).collect();
            expected.sort_by_key(|&g| groups[g as usize].0);
            assert_eq!(&*order, &expected[..]);
        });
    }
}
```

`MacroDefinitions::iter` is covered by `macros_writing_impls` through the existing
`may_hold`/narrow tests; a direct test beside it:

```rust
    #[test]
    fn macro_definitions_iterate_their_runs() {
        let s = Symbol::intern;
        let defs = MacroDefinitions {
            defs: Box::new([(s("a"), false, 2), (s("b"), true, 2), (s("c"), false, 3)]),
            calls: Box::new([s("x"), s("y"), s("z")]),
        };
        let runs: Vec<(Symbol, bool, Vec<Symbol>)> =
            defs.iter().map(|(n, w, c)| (n.clone(), w, c.to_vec())).collect();
        assert_eq!(runs, [
            (s("a"), false, vec![s("x"), s("y")]),
            (s("b"), true, vec![]),
            (s("c"), false, vec![s("z")]),
        ]);
    }
```

(in `inert_attribute_tests`, with `use super::MacroDefinitions; use intern::Symbol;`.)

Budget coverage for mem-db-infer's budget tests: on a warm re-check none of these queries
reruns, so their allocations are 0 (salsa hits); on a cold `InferenceResult::of`, a method call
costs no global allocation from my files (P2-P6, C2): the solver's responses (mem-db-solver) are
what remains.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-impls (22:05), `columns_tests`: as written, at the end of `method_resolution.rs`; `values` typed `[(u32, u32); 6]` and `by_group` given `-> Vec<u32>` so the array `map` infers. `macro_definitions_iterate_their_runs` is db-mem-infer-b-queries'.
> applied by db-mem-infer-b-queries (21:59): `macro_definitions_iterate_their_runs` in `inert_attribute_tests`, as written; `columns_tests` is db-mem-infer-b-impls'.

## Rules that do not fit

None of R1-R8 fails here. Two notes for mem-db-infer:

- R3 and `blaze_radix`: `radix::sort_by_key` allocates its pair scratch on the global heap above
  64 items, and `sort_copy_by_key_in` takes a global `&mut Vec<T>`. I2 avoids it with
  `sort_unstable_by_key` and a total key; a frame-generic radix sort (`sort_by_key_in(frame, ..)`)
  would be blaze-radix's (lane mem) to add if a sort here shows up in a profile.
- R1 in entry points that hir and MIR call per expression (`has_drop_glue`,
  `is_ty_uninhabited_from`): `frame::task` per call is correct but costs a task push; the `_in`
  forms let their callers pass the frame they have, which their parts should do.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): notes, nothing to apply.

## Speed

Bytes per node and the dominant temporaries per run, on a 1,000-expression body with about 150
method calls and 100 paths probed (R8's yardstick; frame allocation about 2 ns, `malloc`/`free`
15 to 25 ns):

| temporary | per probe | per body (250 probes) | before |
|---|---|---|---|
| probe candidate lists (4 lists, `Candidate` 40 bytes, 2 to 6 entries) | ~250 B, 2 to 3 frame bumps | ~60 KB | 2 to 4 `malloc` + regrowth |
| `impl_dups` (16 `ImplId`s' table) | ~200 B, 1 bump | ~50 KB | 1 `malloc` |
| probe steps (`CandidateStep` ~100 B, 1 to 4) | ~400 B, 1 bump | ~100 KB | 0 to 1 `malloc` (spill) |
| `TraitValueItems::runs` (24 B each, 2 to 6 crates) | ~100 B, 1 bump | ~25 KB | 0 to 1 `malloc` |
| autoderef steps (twice per method call, more for fields/index) | 0 (inline) | 0 | 1 to 3 `malloc` each |
| receiver adjustments (24 B, 1 to 3) | ~70 B, 1 bump | ~10 KB (150 calls) | 1 to 2 `malloc` |

So about 1 KB of frame per probe written, about 250 KB per 1,000-expression body (linear in
probes: R8's bound is "bytes per expression times expressions", here about 250 B per expression
on average), where before the same body made roughly 2,000 to 3,000 global allocations from these
files (about 40 to 75 µs of `malloc`/`free` at 15 to 25 ns). After: about 1,500 frame bumps
(about 3 µs). The probe itself (solver goals per candidate, several µs each) stays the cost; the
solver's query responses (`make_query_response_ignoring_pending_obligations` per step, one
`QueryResponse` with its own `Vec`s) are the largest allocation left per probe and are
mem-db-solver's.

Impl tables, per crate (std: ~8,000 trait impls in ~5,000 rows of ~900 traits; ~2,000 inherent
impls): frame bytes per record 16 (`(u32, ImplId)`) plus 32 per row (`(u32, (u64,
SimplifiedType))` padded) plus about 1.15 × 16 per map entry, so about 400 KB of frame for std,
written once and sorted (`n log n` on 4-byte indices); the result is 10 exact columns, 12 bytes per
row plus 8 per impl plus 4 per trait, about 140 KB, written once. Before: ~5,000 row `Vec`s plus
~900 per-trait maps, each grown (about 15,000 `malloc`s, 0.3 ms) and every column regrown and
shrunk. The bound is the write of the result columns plus one pass over the records: about
0.5 MB touched, well under a millisecond, once per crate per change.

> forwarded to db (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:46): notes, nothing to apply.
