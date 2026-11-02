# Requests of mem-db-checks-regions: `regions.rs` onto the body's frame

Part B6 of `v2/requests/mem-db-checks.md` (parent `mem-db-checks`). For lane `db` (owner of
`crates/hir-ty-borrowck/`) and, while it works in the crate, `db-ty-d-borrowck`. Programs
against the parent's B0-B4 (`crate::mem::{FVec, FMap, FSet, Frame, map, set, filled, collect,
owned}`, `BitSet<'f>`, `BitMatrix<'f>`, `Csr<'f, T>`, `Constraints::new(frame)`, `Ctx` with
`pub(crate) frame: &'a Frame`, the body's task's frame) and changes none of it.

Line numbers are of `crates/hir-ty-borrowck/src/regions.rs` (7359 lines) as of 2026-10-29
20:30. Every change keeps what the code computes, and the order it computes it in; only where
the memory comes from changes. Where `&Frame` comes from: `self.ctx.frame` in a `Builder`
method, `ctx.frame` in a free function that takes `ctx`; a free function that takes neither
gets `frame: &'f Frame` first. `'f` below is that frame's lifetime, which is `Ctx<'a, 'db>`'s
`'a` (B3).

What stays: hir-ty's (the `InferCtxt` in `ctx.infcx`, its `next_region_var`, the solver's
`evaluate`/canonical queries, `CapturedPlace`, `Clauses`/`GenericArgs` interning, the
`TypeFolder`s it runs) is mem-db-solver's and mem-db-infer's; each call is named where it is
made, unchanged. Error-path formatting keeps its `format!` with `// alloc: error path`.

Iteration order: every map and set below is only looked up (`get`, `contains`, `insert`,
`entry`), or iterated where the order cannot reach the output (a count, `any`, `all`); each
that is iterated is named at its line with why the order is free. None reaches the
constraints, the diagnostics or the region numbering, so none becomes an ordered vector; the
region numbering is the `InferCtxt`'s `next_region_var` order, which is the walk's order,
unchanged.

---

## R0. Setup

Lines 11-48, the imports. Line 20, before:

```rust
use rustc_hash::{FxHashMap, FxHashSet};
```

after (`FxHashMap`/`FxHashSet` stay only where a test or an owned value still needs one; once
every site below is applied neither is used, and the line goes):

```rust
use crate::mem::{self, FMap, FSet, FVec, Frame, filled};
```

Lines 43-48, before:

```rust
use super::{
    Ctx,
    bitset::BitMatrix,
    cfg::{Cfg, Location},
    constraints::Constraints,
};
```

after:

```rust
use super::{
    Ctx,
    bitset::{BitMatrix, BitSet},
    cfg::{Cfg, Location},
    constraints::Constraints,
};
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-regions (2026-10-29 21:40): both import edits as written; R3's `infer::region_constraints::Constraint` added to the `next_solver` group here too.

### R0.1 What other files call, after this part

The check.rs sibling (`mem-db-checks-check`, B7) and the parent's B5 call these; told by tmux.

| function | before | after |
|---|---|---|
| `build` (712) | `(ctx, body, &Cfg, &FxHashMap<ExprId, (usize, Vec<Requirement>)>) -> Regions<'db>` | `(ctx, body, &Cfg<'_>, &FMap<'_, ExprId, (usize, FVec<'_, Requirement<'db>>)>) -> Regions<'a, 'db>` |
| `Regions::type_bounds_of` (229) | `-> Vec<Rv>` | `-> impl Iterator<Item = Rv> + '_` (check.rs 2087 drops its `.into_iter()`, B7.7) |
| `opaque_captures` (293) | `(db, opaque, len) -> Option<Vec<bool>>` | `(frame, db, opaque, len) -> Option<FVec<'f, bool>>`; liveness.rs 812 passes `self.ctx.frame`, polonius.rs 280 `ctx.frame`, check.rs 2166 `self.ctx.frame` |
| `regions_of` (699) | `(ty, &mut Vec<Rv>)` | `(ty, &mut FVec<'_, Rv>)` (liveness.rs 604/621 pass B5.1's `found: &mut FVec<'_, Rv>`; explain.rs 312 a frame vector) |
| `outlives_components` (6873) | `(ty, &mut Vec<Rv>, &mut Vec<Ty>)` | `(ty, &mut FVec<'_, Rv>, &mut FVec<'_, Ty<'db>>)` |
| `alias_components` (6941) | `(ty, &mut Vec<Ty>)` | `(ty, &mut FVec<'_, Ty<'db>>)` |
| `constraint_path` (7005) | `(&Regions, count, from, to) -> Vec<Outlives>` | `(&Regions<'f, '_>, count, from, to) -> FVec<'f, Outlives>` |
| `blame`, `blame_with` (7042, 7048) | `(&[Outlives])` | unchanged (a frame vector derefs to the slice) |
| `normalize` (7075) | `(ctx, ty)` | unchanged signature |
| `adt_variants` (6858) | `(db, adt) -> Vec<VariantId>` | `(db, adt) -> impl Iterator<Item = VariantId> + '_` (R8.5; names.rs 105 `.get(i).copied()` -> `.nth(i)`, check/parts.rs 267 drops `.into_iter()`) |
| `path_binding`, `spells_generics`, `alias_def_id`, `rv_of` | | unchanged |

---

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree: crates/hir-ty-borrowck/src/regions.rs:706 `build`, 220 `type_bounds_of -> impl Iterator`, 281 `opaque_captures(frame, ..)` (callers liveness.rs:780, polonius.rs:284, check.rs:2403 pass the frame), 693 `regions_of`, 7078 `outlives_components`, 7146 `alias_components`, 7214 `constraint_path`, 7056 `adt_variants`.

## R1. `Regions<'db>` becomes `Regions<'f, 'db>` (lines 169-243)

Once per body. Every field a frame value of the body's frame; the struct is what the body's
analyses read (`liveness`, `inference`, `loans`, `polonius`, `check`), all in the same task.

Lines 169-217, before (fields only):

```rust
pub(super) struct Regions<'db> {
    pub(super) universals: Vec<(Rv, UniversalKind)>,
    universal_index: FxHashMap<Rv, usize>,
    pub(super) static_rv: Rv,
    pub(super) early: FxHashMap<u32, Rv>,
    known: BitMatrix,
    pub(super) type_bounds: Vec<(Ty<'db>, Rv)>,
    pub(super) local_tys: Vec<Ty<'db>>,
    pub(super) loan_regions: FxHashMap<Location, Rv>,
    pub(super) outlives: Constraints,
    pub(super) type_tests: Vec<TypeTest<'db>>,
    pub(super) check_universals: bool,
    pub(super) related_placeholders: FxHashSet<usize>,
    pub(super) normalized_placeholders: FxHashSet<usize>,
    pub(super) is_impl_item: bool,
    pub(super) unsuggested: FxHashSet<usize>,
    pub(super) closure_env: Option<usize>,
    pub(super) creator_positions: FxHashMap<usize, usize>,
    pub(super) members: Vec<Rv>,
    pub(super) choices: Vec<Rv>,
    pub(super) returns_static_opaque: bool,
    pub(super) callee_reads: Vec<(LocalId, Location)>,
}
```

after (doc comments unchanged; `universal_index` gets one):

```rust
pub(super) struct Regions<'f, 'db> {
    pub(super) universals: FVec<'f, (Rv, UniversalKind)>,
    /// The position in `universals` of each region variable that is universal, by the
    /// variable, `u32::MAX` for one that is not; as long as the largest universal variable.
    universal_index: FVec<'f, u32>,
    pub(super) static_rv: Rv,
    pub(super) early: FMap<'f, u32, Rv>,
    known: BitMatrix<'f>,
    pub(super) type_bounds: FVec<'f, (Ty<'db>, Rv)>,
    pub(super) local_tys: FVec<'f, Ty<'db>>,
    pub(super) loan_regions: FMap<'f, Location, Rv>,
    pub(super) outlives: Constraints<'f>,
    pub(super) type_tests: FVec<'f, TypeTest<'db>>,
    pub(super) check_universals: bool,
    pub(super) related_placeholders: FSet<'f, usize>,
    pub(super) normalized_placeholders: FSet<'f, usize>,
    pub(super) is_impl_item: bool,
    pub(super) unsuggested: FSet<'f, usize>,
    pub(super) closure_env: Option<usize>,
    pub(super) creator_positions: FMap<'f, usize, usize>,
    pub(super) members: FVec<'f, Rv>,
    pub(super) choices: FVec<'f, Rv>,
    pub(super) returns_static_opaque: bool,
    pub(super) callee_reads: FVec<'f, (LocalId, Location)>,
}
```

Why each:
- `universal_index`: looked up by every `universal(rv)` (per constraint in `known_relations`
  and check.rs's universal-region errors, per region in `clauses`). A region variable is a
  dense `usize` below the body's region count, so a direct index replaces a hash and probe;
  4 bytes per region up to the largest universal one (the universals come first, the
  placeholders made later are the only ones past them).
- `early`, `loan_regions`, `creator_positions`: looked up (`liveness.rs` 895, `loans.rs` 555,
  check.rs 1914/1999/2003); `creator_positions.values()` at check.rs 284 is counted, and
  `.len()` at 1677 is a count: order free. hashbrown with the frame.
- `related_placeholders`, `normalized_placeholders`, `unsuggested`: universal indices, looked
  up (check.rs 1827, 1828, 1849); grown while universals are still made, so a frame set, not a
  fixed-size `BitSet`.
- `known`: the parent's `BitMatrix<'f>`, made once in `known_relations` (R5).
- `outlives`: the parent's `Constraints<'f>` (B5.6).

Lines 219-243 (`impl`), before:

```rust
impl<'db> Regions<'db> {
    pub(super) fn universal(&self, rv: Rv) -> Option<usize> {
        self.universal_index.get(&rv).copied()
    }
    ...
    pub(super) fn type_bounds_of(&self, ty: Ty<'db>) -> Vec<Rv> {
        self.type_bounds
            .iter()
            .filter(|&&(bounded, _)| bounded == ty)
            .map(|&(_, region)| region)
            .collect()
    }
```

after:

```rust
impl<'f, 'db> Regions<'f, 'db> {
    pub(super) fn universal(&self, rv: Rv) -> Option<usize> {
        match self.universal_index.get(rv) {
            Some(&at) if at != u32::MAX => Some(at as usize),
            _ => None,
        }
    }
    ...
    pub(super) fn type_bounds_of(&self, ty: Ty<'db>) -> impl Iterator<Item = Rv> + '_ {
        self.type_bounds.iter().filter(move |&&(bounded, _)| bounded == ty).map(|&(_, region)| region)
    }
```

(`knows`, `type_outlives_known` unchanged.) `type_bounds_of`'s one caller, check.rs 2087
`self.regions.type_bounds_of(ty).into_iter().chain([fn_body]).any(..)`, becomes
`self.regions.type_bounds_of(ty).chain([fn_body]).any(..)`: one vector per type test gone.
Settled with `mem-db-checks-check` (2026-10-29 21:09): their B7.7 chains
`type_bounds_of(ty)` without `.into_iter()` at check.rs ~2087, so the iterator above is the
change (their 20:53 note that check.rs reads `type_bounds` directly is withdrawn).

Every other file that names `Regions<'db>` / `&Regions<'_>` names `Regions<'_, 'db>` /
`&Regions<'_, '_>` (the parent's B5 already writes `liveness`, `loans`, `polonius`,
`inference` so; check.rs is B7's).

> applied by db-mem-borrowck-regions (2026-10-29 21:45): fields, `universal` and `type_bounds_of` as written. `outlives: Constraints<'f>` and `Constraints::new` are the parent's B5.6, not yet in `constraints.rs`.

---

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).

## R2. `build` and `Builder` (lines 712-876)

Once per body: the regions' tables, every field of `Builder`, and two sets made from the body
up front.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): R2.1-R2.4 applied below by db-mem-borrowck-regions; `build` at regions.rs:706 takes the frame maps.

### R2.1 `build` (lines 712-784)

Before (lines 712-763):

```rust
pub(super) fn build<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg,
    requirements: &FxHashMap<hir_def::hir::ExprId, (usize, Vec<Requirement<'db>>)>,
) -> Regions<'db> {
    let mut builder = Builder {
        ctx,
        body,
        regions: Regions {
            universals: Vec::new(),
            universal_index: FxHashMap::default(),
            static_rv: 0,
            known: BitMatrix::default(),
            type_bounds: Vec::new(),
            early: FxHashMap::default(),
            local_tys: Vec::new(),
            loan_regions: FxHashMap::default(),
            outlives: Constraints::default(),
            type_tests: Vec::new(),
            check_universals: ctx.closure_local.is_none(),
            related_placeholders: FxHashSet::default(),
            normalized_placeholders: FxHashSet::default(),
            is_impl_item: false,
            unsuggested: FxHashSet::default(),
            closure_env: None,
            creator_positions: FxHashMap::default(),
            members: Vec::new(),
            returns_static_opaque: false,
            callee_reads: Vec::new(),
            choices: Vec::new(),
        },
        requirements,
        early: FxHashMap::default(),
        late: FxHashMap::default(),
        parent_late: FxHashMap::default(),
        impl_self: None,
        returns_opaque: false,
        opaque_bounds: Vec::new(),
        opaque_hidden: None,
        elided_output: None,
        inferred: Default::default(),
        location: None,
        env_aliases: Vec::new(),
        annotated: FxHashMap::default(),
        const_positions: 0,
        aggregate_temps: aggregate_temps(body),
        synthesized: synthesized(ctx),
        placeholders: FxHashMap::default(),
        relate_universes: FxHashSet::default(),
        elided: Default::default(),
    };
```

after:

```rust
pub(super) fn build<'a, 'db>(
    ctx: &Ctx<'a, 'db>,
    body: &MirBody<'db>,
    cfg: &Cfg<'_>,
    requirements: &FMap<'_, hir_def::hir::ExprId, (usize, FVec<'_, Requirement<'db>>)>,
) -> Regions<'a, 'db> {
    let frame = ctx.frame;
    let mut builder = Builder {
        ctx,
        body,
        regions: Regions {
            universals: Vec::new_in(frame),
            universal_index: Vec::new_in(frame),
            static_rv: 0,
            known: BitMatrix::default(),
            type_bounds: Vec::new_in(frame),
            early: mem::map(frame),
            local_tys: Vec::new_in(frame),
            loan_regions: mem::map(frame),
            outlives: Constraints::new(frame),
            type_tests: Vec::new_in(frame),
            check_universals: ctx.closure_local.is_none(),
            related_placeholders: mem::set(frame),
            normalized_placeholders: mem::set(frame),
            is_impl_item: false,
            unsuggested: mem::set(frame),
            closure_env: None,
            creator_positions: mem::map(frame),
            members: Vec::new_in(frame),
            returns_static_opaque: false,
            callee_reads: Vec::new_in(frame),
            choices: Vec::new_in(frame),
        },
        requirements,
        early: mem::map(frame),
        late: mem::map(frame),
        parent_late: mem::map(frame),
        impl_self: None,
        returns_opaque: false,
        opaque_bounds: Vec::new_in(frame),
        opaque_hidden: None,
        elided_output: None,
        inferred: Default::default(),
        location: None,
        env_aliases: Vec::new_in(frame),
        annotated: mem::map(frame),
        const_positions: 0,
        aggregate_temps: aggregate_temps(frame, body),
        synthesized: synthesized(ctx),
        placeholders: mem::map(frame),
        relate_universes: mem::set(frame),
        elided: std::cell::RefCell::new(mem::set(frame)),
        scratch: Scratch::new(frame),
    };
```

(`BitMatrix::default()` is the parent's derived `Default`, an empty `&mut []`, replaced in
`known_relations`.) Lines 764-784 (the walk over `cfg.order`) unchanged: `cfg.order` is the
parent's frame vector of `Cfg<'f>` (B2.4), iterated by `&`. `scratch` is new, R3.

`requirements`'s map is the owner's frame (B4.2), longer-lived than the body's; `FMap<'o, ..>`
and `FVec<'o, ..>` are covariant in `'o` (hashbrown's `RawTable<T, A>` and `Vec<T, A>` hold `A`
by value), so `Builder` can hold it as `&'c FMap<'c, ..>`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-regions (2026-10-29 21:46): as written (now at line 699).

### R2.2 `Builder` (lines 786-839)

Before / after, the fields that allocate (comments unchanged):

```rust
struct Builder<'c, 'a, 'db> {
    ctx: &'c Ctx<'a, 'db>,
    env_aliases: Vec<Ty<'db>>,
    annotated: FxHashMap<LocalId, Option<Ty<'db>>>,
    const_positions: usize,
    body: &'c MirBody<'db>,
    regions: Regions<'db>,
    requirements: &'c FxHashMap<hir_def::hir::ExprId, (usize, Vec<Requirement<'db>>)>,
    early: FxHashMap<u32, Region<'db>>,
    late: FxHashMap<u32, Region<'db>>,
    parent_late: FxHashMap<u32, Region<'db>>,
    ...
    opaque_bounds: Vec<Clause<'db>>,
    ...
    aggregate_temps: FxHashSet<LocalId>,
    synthesized: FxHashSet<hir_def::hir::ExprId>,
    placeholders: FxHashMap<Region<'db>, Region<'db>>,
    relate_universes: FxHashSet<rustc_type_ir::UniverseIndex>,
    elided: std::cell::RefCell<FxHashSet<rustc_type_ir::RegionVid>>,
}
```

```rust
struct Builder<'c, 'a, 'db> {
    ctx: &'c Ctx<'a, 'db>,
    env_aliases: FVec<'a, Ty<'db>>,
    annotated: FMap<'a, LocalId, Option<Ty<'db>>>,
    const_positions: usize,
    body: &'c MirBody<'db>,
    regions: Regions<'a, 'db>,
    requirements: &'c FMap<'c, hir_def::hir::ExprId, (usize, FVec<'c, Requirement<'db>>)>,
    early: FMap<'a, u32, Region<'db>>,
    late: FMap<'a, u32, Region<'db>>,
    parent_late: FMap<'a, u32, Region<'db>>,
    ...
    opaque_bounds: FVec<'a, Clause<'db>>,
    ...
    /// rustc's `AggregateTemp`s, by local.
    aggregate_temps: BitSet<'a>,
    /// By expression.
    synthesized: BitSet<'a>,
    placeholders: FMap<'a, Region<'db>, Region<'db>>,
    relate_universes: FSet<'a, rustc_type_ir::UniverseIndex>,
    elided: std::cell::RefCell<FSet<'a, rustc_type_ir::RegionVid>>,
    /// The walk's per-statement and per-call buffers, cleared at each use (R3).
    scratch: Scratch<'a, 'db>,
}
```

The maps `early`/`late`/`parent_late`/`placeholders` are looked up and inserted into only
(lines 929-1027, 1688, 3297, 3317); `annotated` too (1656, 1927, 3992); `relate_universes`
(933, 3497); `elided` (898, 6232 `contains`). Order free.

`impl<'db> Builder<'_, '_, 'db>` (line 885) becomes `impl<'a, 'db> Builder<'_, 'a, 'db>` so the
methods below can name `'a` for frame values they return.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-regions (2026-10-29 21:47): as written; the `scratch` field's doc says "cleared at each use" without the "(R3)". The one `impl Builder` (now line 881) is `impl<'a, 'db> Builder<'_, 'a, 'db>`.

### R2.3 `synthesized` and `aggregate_temps` (lines 841-876)

Once per body: a set of expressions and a set of locals, each only asked `contains` (3064,
3969). Both are dense indices of a known size, so frame bitsets: one bit each, no hash.

Before:

```rust
fn synthesized(ctx: &Ctx<'_, '_>) -> FxHashSet<hir_def::hir::ExprId> {
    let InferBodyId::DefWithBodyId(def) = ctx.owner else { return FxHashSet::default() };
    let source_map = hir_def::expr_store::Body::unmoved_source_map(ctx.db, def);
    ctx.store.expr_ids().filter(|&expr| source_map.expr_syntax(expr).is_err()).collect()
}

fn aggregate_temps(body: &MirBody<'_>) -> FxHashSet<LocalId> {
    let mut out = FxHashSet::default();
    let mut add = |operand: &Operand| {
        if let OperandKind::Move(place) = &operand.kind
            && place.projection.as_slice().is_empty()
        {
            out.insert(place.local);
        }
    };
```

after:

```rust
fn synthesized<'a>(ctx: &Ctx<'a, '_>) -> BitSet<'a> {
    let exprs = ctx.store.expr_ids();
    let mut out = BitSet::new(ctx.frame, exprs.len());
    let InferBodyId::DefWithBodyId(def) = ctx.owner else { return out };
    let source_map = hir_def::expr_store::Body::unmoved_source_map(ctx.db, def);
    for expr in exprs.filter(|&expr| source_map.expr_syntax(expr).is_err()) {
        out.insert(expr.into_raw().into_u32() as usize);
    }
    out
}

fn aggregate_temps<'f>(frame: &'f Frame, body: &MirBody<'_>) -> BitSet<'f> {
    let mut out = BitSet::new(frame, body.locals.len());
    let mut add = |operand: &Operand| {
        if let OperandKind::Move(place) = &operand.kind
            && place.projection.as_slice().is_empty()
        {
            out.insert(place.local.into_raw().into_u32() as usize);
        }
    };
```

(`ExpressionStore::expr_ids` is `ExactSizeIterator`, `hir-def/src/expr_store.rs` 1620; the rest
of `aggregate_temps` unchanged.) The two readers:

- line 3064 `self.synthesized.contains(&expr)` ->
  `self.synthesized.contains(expr.into_raw().into_u32() as usize)`;
- line 3969 `self.aggregate_temps.contains(&place.local)` ->
  `self.aggregate_temps.contains(place.local.into_raw().into_u32() as usize)`.

An expression of a nested body is in the owner's store (`ctx.store` is the owner's), so the
bitset over `ctx.store`'s expressions covers every `ExprId` a body's MIR spans name.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-regions (2026-10-29 21:48): as written, both readers too (now lines 3060, 3965). `expr_ids` is `ExactSizeIterator` at `expr_store.rs` 1819.

### R2.4 `new_universal` (lines 920-926)

Before:

```rust
        let rv = rv_of(region).expect("a fresh region is a variable");
        self.regions.universal_index.insert(rv, self.regions.universals.len());
        self.regions.universals.push((rv, kind));
```

after:

```rust
        let rv = rv_of(region).expect("a fresh region is a variable");
        let index = &mut self.regions.universal_index;
        if index.len() <= rv {
            index.resize(rv + 1, u32::MAX);
        }
        index[rv] = self.regions.universals.len() as u32;
        self.regions.universals.push((rv, kind));
```

> applied by db-mem-borrowck-regions (2026-10-29 21:48): as written.

---

## R3. The walk's reused buffers: `Shelf` and `Scratch` (new, after `At`, line 883)

`Frame::reset_to` and `Frame::scope` take `&mut Frame`, and a body's checks hold `&'a Frame`,
so nothing allocated in the frame during the walk is given back before the body's task ends.
A temporary made per statement, per constraint or per solver call would then add its bytes
to the frame once per use. These are instead taken from a shelf of frame vectors and given
back cleared: after the first few uses no call writes new memory, and every buffer is in
cache. The walk recurses (`type_outlives_at` -> `alias_outlives` -> `type_outlives_at`,
`clauses` -> `type_outlives`, `relate` -> `normalize_at`), so one buffer per kind is not
enough; a shelf holds as many as the deepest nesting needs.

New, after `struct At` (lines 878-883), with `use hir_ty::next_solver::infer::
region_constraints::Constraint;` added to the imports (line 33-37's `next_solver` group gets
`infer::region_constraints::Constraint,`):

```rust
/// Frame vectors to reuse: [`Shelf::take`] one for a call, [`Shelf::give`] it back cleared, so a
/// nested call takes another. There are as many as the deepest nesting, each as long as its
/// longest use.
struct Shelf<'a, T> {
    frame: &'a Frame,
    free: FVec<'a, FVec<'a, T>>,
}

impl<'a, T> Shelf<'a, T> {
    fn new(frame: &'a Frame) -> Self {
        Shelf { frame, free: Vec::new_in(frame) }
    }

    fn take(&mut self) -> FVec<'a, T> {
        let frame = self.frame;
        self.free.pop().unwrap_or_else(|| Vec::new_in(frame))
    }

    fn give(&mut self, mut it: FVec<'a, T>) {
        it.clear();
        self.free.push(it);
    }
}

/// The walk's buffers, by what they hold.
struct Scratch<'a, 'db> {
    rvs: Shelf<'a, Rv>,
    tys: Shelf<'a, Ty<'db>>,
    regions: Shelf<'a, Region<'db>>,
    clauses: Shelf<'a, Clause<'db>>,
    pairs: Shelf<'a, (Region<'db>, Region<'db>)>,
    obligations: Shelf<'a, (Ty<'db>, Region<'db>)>,
    constraints: Shelf<'a, Constraint<'db>>,
    projections: Shelf<'a, (hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>,
}

impl<'a, 'db> Scratch<'a, 'db> {
    fn new(frame: &'a Frame) -> Self {
        Scratch {
            rvs: Shelf::new(frame),
            tys: Shelf::new(frame),
            regions: Shelf::new(frame),
            clauses: Shelf::new(frame),
            pairs: Shelf::new(frame),
            obligations: Shelf::new(frame),
            constraints: Shelf::new(frame),
            projections: Shelf::new(frame),
        }
    }
}
```

A buffer taken and not given back (an early `return` between them) is not an error: it stays
in the frame until the task ends, and the next `take` makes another. Every `take` below is
paired with a `give` on each path that returns, so none is lost in practice.

Test (in `regions.rs`'s test module, or `regions/tests.rs` if lane db keeps one):

```rust
#[test]
fn a_shelf_gives_back_what_it_took() {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        let mut shelf: Shelf<'_, u32> = Shelf::new(frame);
        let mut outer = shelf.take();
        outer.extend(0..100);
        let ptr = outer.as_ptr();
        let inner = shelf.take();
        assert_ne!(inner.as_ptr(), ptr);
        shelf.give(inner);
        shelf.give(outer);
        let again = shelf.take();
        assert_eq!((again.len(), again.as_ptr()), (0, ptr));
        blaze_alloc_count::assert_at_most(0, "a shelf's take and give", || {
            let mut it = shelf.take();
            it.extend(0..100);
            shelf.give(it);
        });
    });
}
```

---

> forwarded to db (db-ty-d-borrowck, in `crates/hir-ty-borrowck/`) (requests, pass 18; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:55): not applied, waits on R0-R2 and B0-B1 of
> `mem-db-checks.md`: `Regions` is still `Regions<'db>`, there is no `crate::mem` (`mem.rs`), and
> the crate has no `blaze-mem` dependency, so `Shelf`/`Scratch` have no frame to be in.
> applied by db-mem-borrowck-regions (2026-10-29 21:50): `Shelf`, `Scratch` after `At` as written (`Constraint` imported in R0). The test is in a new `#[cfg(test)] mod tests` at the end of regions.rs, with one difference: `again` is given back before the counted block, so the counted `take` reuses the 100-long buffer.

## R4. Relating and outliving: per statement, per constraint

`relate` (3448-3642), `relate_args` (3644-3664), `relate_regions` (3356-3366),
`region_outlives` (3371-3389), `outlives` (3059-3079): no allocation; each constraint is one
`Constraints::push` into the parent's frame columns (B5.6). What they reach that allocates:

> forwarded to db (db-ty-d-borrowck) (requests, pass 18; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:55): not applied, waits on R3 (its `Scratch`) and so on
> R0-R2 and B0-B1.
> applied by db-mem-borrowck-regions (2026-10-29 21:58): R4.1-R4.6 as written (R4.3 at lines 3593 and 3637 now, R4.5 at 3067). R4.5's callers in `implied_bounds` are changed with R6.3; `closure_requirements`'s `parents` with R7.3.

### R4.1 `type_outlives_at` (lines 3672-3697): per type that must outlive a region

Three vectors per call (`components`, `params`, `aliases`): the most frequent temporaries of
the walk after the constraints themselves (every `T: 'a` clause, every well-formed reference,
every unsize to `dyn Trait + 'a`). Before:

```rust
    fn type_outlives_at(&mut self, ty: Ty<'db>, region: Region<'db>, at: At, depth: usize) {
        let Some(rv) = self.region_rv(region) else { return };
        let mut components = Vec::new();
        let mut params = Vec::new();
        outlives_components(ty, &mut components, &mut params);
        for longer in components {
            self.outlives(longer, rv, at);
        }
        for param in params {
            self.regions.type_tests.push(TypeTest {
                ...
            });
        }
        if depth > 4 {
            return;
        }
        let mut aliases = Vec::new();
        alias_components(ty, &mut aliases);
        for alias in aliases {
            self.alias_outlives(alias, region, at, depth);
        }
    }
```

after:

```rust
    fn type_outlives_at(&mut self, ty: Ty<'db>, region: Region<'db>, at: At, depth: usize) {
        let Some(rv) = self.region_rv(region) else { return };
        let mut components = self.scratch.rvs.take();
        let mut params = self.scratch.tys.take();
        outlives_components(ty, &mut components, &mut params);
        for &longer in components.iter() {
            self.outlives(longer, rv, at);
        }
        for &param in params.iter() {
            self.regions.type_tests.push(TypeTest {
                ...
            });
        }
        self.scratch.rvs.give(components);
        self.scratch.tys.give(params);
        if depth > 4 {
            return;
        }
        let mut aliases = self.scratch.tys.take();
        alias_components(ty, &mut aliases);
        for &alias in aliases.iter() {
            self.alias_outlives(alias, region, at, depth);
        }
        self.scratch.tys.give(aliases);
    }
```

(`TypeTest { .. }` fields unchanged.) Same order: the loops ran over the vectors by value
front to back; they run over them by reference front to back.

### R4.2 `alias_outlives` (lines 3703-3781): per alias that must outlive a region

`from_trait` per call; `captured` per opaque. Lines 3725-3736, before:

```rust
        let from_trait: Vec<Region<'db>> =
            rustc_type_ir::Interner::item_bounds(interner, alias_def_id(alias.kind))
                .iter_instantiated(interner, alias.args)
                .filter_map(|it| match it.skip_norm_wip().kind().skip_binder() {
                    ...
                })
                .collect();
```

after:

```rust
        let mut from_trait = self.scratch.regions.take();
        from_trait.extend(
            rustc_type_ir::Interner::item_bounds(interner, alias_def_id(alias.kind))
                .iter_instantiated(interner, alias.args)
                .filter_map(|it| match it.skip_norm_wip().kind().skip_binder() {
                    ...
                }),
        );
```

and each of the three exits gives it back: line 3759 `return;` -> `self.scratch.regions.give(
from_trait); return;`, line 3772 the same, and after the final `self.regions.type_tests.push(
TypeTest { .. });` (3774-3780) `self.scratch.regions.give(from_trait);`. Lines 3743-3744:

```rust
            let captured =
                opaque.and_then(|opaque| opaque_captures(self.ctx.db, opaque, alias.args.len()));
```

->

```rust
            let captured = opaque
                .and_then(|opaque| opaque_captures(self.ctx.frame, self.ctx.db, opaque, alias.args.len()));
```

(`it[index]` on the frame vector unchanged.)

### R4.3 `relate`: the opaque variances and the upcast (lines 3542-3544, 3585-3595)

Per relation of two `dyn` upcasts and two opaque types: rare, but per statement where they
happen. Lines 3542-3544, before:

```rust
                                let args = GenericArgs::new_from_iter(
                                    found.args.iter().skip(1).collect::<Vec<_>>(),
                                );
```

after (the interned slice is already there; interning a subslice copies nothing first):

```rust
                                let args = GenericArgs::new_from_slice(
                                    found.args.as_slice().get(1..).unwrap_or(&[]),
                                );
```

Lines 3585-3595, before:

```rust
                    rustc_type_ir::AliasTyKind::Opaque { def_id } => (
                        opaque_captures(self.ctx.db, def_id, x.args.len()).map(|it| {
                            it.into_iter()
                                .map(|captured| match captured {
                                    true => Variance::Invariant,
                                    false => Variance::Bivariant,
                                })
                                .collect::<Vec<_>>()
                        }),
                        variance,
                    ),
```

after:

```rust
                    rustc_type_ir::AliasTyKind::Opaque { def_id } => {
                        let frame = self.ctx.frame;
                        (
                            opaque_captures(frame, self.ctx.db, def_id, x.args.len()).map(|it| {
                                mem::collect(
                                    frame,
                                    it.into_iter().map(|captured| match captured {
                                        true => Variance::Invariant,
                                        false => Variance::Bivariant,
                                    }),
                                )
                            }),
                            variance,
                        )
                    }
```

(`variances.as_deref()` at 3599 derefs the frame vector the same.)

### R4.4 `instantiate_sig` (lines 3394-3429): per higher-ranked `fn` pointer related

Line 3405, before:

```rust
        let mut existentials: Vec<(rustc_type_ir::BoundVar, Region<'db>)> = Vec::new();
```

after (the fold's closure holds `infcx`, not `self`, so the frame is read first):

```rust
        let mut existentials: FVec<'_, (rustc_type_ir::BoundVar, Region<'db>)> =
            Vec::new_in(self.ctx.frame);
```

At most one entry per bound variable of the signature, once per relation; not reused (the
shelf holds no `(BoundVar, Region)`, and a `for<'a>` `fn` pointer related is rare).

### R4.5 `implied_components` and `implied_by_clauses` (lines 3016-3057)

Per where clause and per type of the signature (`implied_bounds`, R6): once per body, but per
clause. Before:

```rust
    fn implied_components(&mut self, ty: Ty<'db>) -> (Vec<Rv>, Vec<Ty<'db>>) {
        let mut components = Vec::new();
        let mut params = Vec::new();
        outlives_components(ty, &mut components, &mut params);
        alias_components(ty, &mut params);
        let aliases: Vec<_> =
            params.iter().filter(|it| matches!(it.kind(), TyKind::Alias(_, _))).copied().collect();
        for alias in aliases {
            ...
        }
        (components, params)
    }

    fn implied_by_clauses(&mut self, clauses: Vec<Clause<'db>>, pairs: &mut Vec<(Rv, Rv)>) {
        for clause in clauses {
            ...
                        let (components, params) = self.implied_components(ty);
                        pairs.extend(components.into_iter().map(|it| (it, shorter)));
                        self.regions
                            .type_bounds
                            .extend(params.into_iter().chain([ty]).map(|it| (it, shorter)));
```

after (the caller's buffers are filled; `aliases` from the shelf):

```rust
    fn implied_components(
        &mut self,
        ty: Ty<'db>,
        components: &mut FVec<'a, Rv>,
        params: &mut FVec<'a, Ty<'db>>,
    ) {
        outlives_components(ty, components, params);
        alias_components(ty, params);
        let mut aliases = self.scratch.tys.take();
        aliases.extend(params.iter().filter(|it| matches!(it.kind(), TyKind::Alias(_, _))).copied());
        for &alias in aliases.iter() {
            if let Some(normalized) = self.normalize(alias)
                && normalized != alias
            {
                outlives_components(normalized, components, params);
                alias_components(normalized, params);
            }
        }
        self.scratch.tys.give(aliases);
    }

    fn implied_by_clauses(&mut self, clauses: &[Clause<'db>], pairs: &mut FVec<'a, (Rv, Rv)>) {
        for &clause in clauses {
            ...
                        let mut components = self.scratch.rvs.take();
                        let mut params = self.scratch.tys.take();
                        self.implied_components(ty, &mut components, &mut params);
                        pairs.extend(components.iter().map(|&it| (it, shorter)));
                        self.regions
                            .type_bounds
                            .extend(params.iter().copied().chain([ty]).map(|it| (it, shorter)));
                        self.scratch.rvs.give(components);
                        self.scratch.tys.give(params);
```

Their callers are in `implied_bounds` (R6.3): line 2845's `let (components, params) =
self.implied_components(referent);` and 2890/2955's `self.implied_by_clauses(clauses, pairs)`.

### R4.6 `closure_requirements` and `apply_requirements` (lines 3288-3354)

Per closure and `const { .. }` block made in the body. `apply_requirements` takes the regions
by value only to read them. Before:

```rust
        let mut regions = Vec::new();
        regions_of(ty, &mut regions);
        self.apply_requirements(closure, regions);
    }

    fn apply_requirements(&mut self, nested: hir_def::hir::ExprId, regions: Vec<Rv>) {
```

after:

```rust
        let mut regions = self.scratch.rvs.take();
        regions_of(ty, &mut regions);
        self.apply_requirements(closure, &regions);
        self.scratch.rvs.give(regions);
    }

    fn apply_requirements(&mut self, nested: hir_def::hir::ExprId, regions: &[Rv]) {
```

(the body reads `regions.len()` and `regions.get(position)`, both on the slice.) `parents:
Vec<(u32, Region<'db>)>` (3292) is made by the caller at 5141, R7.

`inline_const_ty` (4152-4178), line 4162 `let mut positions = Vec::new();` -> `let mut
positions = self.scratch.rvs.take();`, line 4177 `self.apply_requirements(konst.loc(db).expr,
positions);` -> `self.apply_requirements(konst.loc(db).expr, &positions);
self.scratch.rvs.give(positions);`. The fold's closure (4164-4176) pushes to `positions` and
calls `self.fresh()`/`self.signature_region(..)`: `positions` is a local now, not a field of
`self`, so the closure borrows both as before.

---

## R5. The solver: per where clause, per proof, per normalization

### R5.1 `predicates` (lines 5716-5749): per item used (call, path, ADT)

The clauses of the item instantiated, per use. Before:

```rust
        let mut clauses: Vec<_> = GenericPredicates::query_all(self.ctx.db, def)
            .iter_instantiated(interner, args.as_slice())
            .map(|it| it.skip_norm_wip())
            .collect();
        ...
        self.clauses(clauses, at, type_outlives);
    }
```

after:

```rust
        let mut clauses = self.scratch.clauses.take();
        clauses.extend(
            GenericPredicates::query_all(self.ctx.db, def)
                .iter_instantiated(interner, args.as_slice())
                .map(|it| it.skip_norm_wip()),
        );
        ...
        self.clauses(&clauses, at, type_outlives);
        self.scratch.clauses.give(clauses);
    }
```

(`clauses.push(..)` at 5739 and the `continue` at 5744 unchanged.)

### R5.2 `clauses` (lines 5752-5886): per clause

Line 5752 `fn clauses(&mut self, clauses: Vec<Clause<'db>>, at: At, type_outlives: bool) {` ->
`fn clauses(&mut self, clauses: &[Clause<'db>], at: At, type_outlives: bool) {`, and line 5760
`for clause in clauses {` -> `for &clause in clauses {`. The slice is a shelf buffer the caller
holds (a local, not `self`), so `&mut self` calls inside the loop borrow nothing it does.

Lines 5826-5828 (and 5291-5293 in `unsize`, R7), before:

```rust
                    for region in self.trait_outlives(trait_ref) {
                        self.type_outlives(trait_ref.self_ty(), region, at);
                    }
```

after:

```rust
                    let mut regions = self.scratch.regions.take();
                    self.trait_outlives(trait_ref, &mut regions);
                    for &region in regions.iter() {
                        self.type_outlives(trait_ref.self_ty(), region, at);
                    }
                    self.scratch.regions.give(regions);
```

(at 5291: `self.type_outlives(source, region, at)` in the loop.) Lines 5837-5847: the
`instantiate` closure takes `&mut dyn FnMut(_) -> Region<'db>`, called twice per higher-ranked
trait clause with two closures; no allocation, but hot_lint's `dyn [A-Z]` finds it. Not
memory: lane dedyn's (a local generic `fn` over `impl FnMut` does it). Nothing here touches
that parameter, so whatever dedyn makes of it stands; this request adds no `dyn`.

The other two callers of `clauses`:
- `signature_higher_ranked` (2451-2483), once per body: line 2456 `let mut clauses =
  Vec::new();` -> `let mut clauses = self.scratch.clauses.take();`, line 2482
  `self.clauses(clauses, ..);` -> `self.clauses(&clauses, ..); self.scratch.clauses.give(
  clauses);`. (`!clauses.contains(&clause)` at 2475 is linear in the higher-ranked trait
  clauses of the signature's ADTs, a handful; unchanged.)
- `statement` (3980-3982), once per body (the first assignment of the return slot): before

  ```rust
                let bounds = std::mem::take(&mut self.opaque_bounds);
                let at = At { category: Category::Signature, span };
                self.clauses(bounds, at, true);
  ```

  after (a frame vector has no `Default`; `Vec::new_in` allocates nothing):

  ```rust
                let bounds = std::mem::replace(&mut self.opaque_bounds, Vec::new_in(self.ctx.frame));
                let at = At { category: Category::Signature, span };
                self.clauses(&bounds, at, true);
  ```

### R5.3 `trait_outlives` (lines 5890-5914): per trait clause

Before:

```rust
    fn trait_outlives(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
    ) -> Vec<Region<'db>> {
        let db = self.ctx.db;
        let mut regions = Vec::new();
        ...
        regions
    }
```

after (the body pushes to `regions` as before):

```rust
    fn trait_outlives(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        regions: &mut FVec<'a, Region<'db>>,
    ) {
        let db = self.ctx.db;
        ...
    }
```

(lines 5897 and 5908 `regions.push(..)` unchanged; the final `regions` expression goes.)

### R5.4 `prove` (lines 6144-6302): per solver call

Four temporaries per call: `pairs`, `obligations` (from the query's answer or the body's
solver), the `unknown` set, and `prove_canonical`'s `made` map and `vars`. Before (6165-6208):

```rust
        let (pairs, obligations) = match self.prove_canonical(trait_ref) {
            Some(Ok(it)) => it,
            Some(Err(true_error)) => return !regionless && true_error,
            None => {
                ...
                let errors = ocx.evaluate_obligations_error_on_ambiguity();
                if !errors.is_empty() {
                    return !regionless && errors.iter().any(|it| it.is_true_error());
                }
                let mut inner = self.ctx.infcx.inner.borrow_mut();
                let obligations: Vec<_> = inner.region_obligations()[obligations_before..]
                    .iter()
                    .map(|it| (it.sup_type, it.sub_region))
                    .collect();
                let pairs = constraint_pairs(
                    interner,
                    inner
                        .unwrap_region_constraints()
                        .region_constraint_data()
                        .constraints
                        .iter_from(constraints_before),
                );
                (pairs, obligations)
            }
        };
        if regionless && pairs.is_empty() && obligations.is_empty() {
            return false;
        }
```

after:

```rust
        let mut pairs = self.scratch.pairs.take();
        let mut obligations = self.scratch.obligations.take();
        let proven = self.prove_canonical(trait_ref, &mut pairs, &mut obligations);
        let refused = match proven {
            Some(Ok(())) => None,
            Some(Err(true_error)) => Some(!regionless && true_error),
            None => {
                ... (lines 6169-6189 unchanged)
                let errors = ocx.evaluate_obligations_error_on_ambiguity();
                if !errors.is_empty() {
                    Some(!regionless && errors.iter().any(|it| it.is_true_error()))
                } else {
                    let mut inner = self.ctx.infcx.inner.borrow_mut();
                    obligations.extend(
                        inner.region_obligations()[obligations_before..]
                            .iter()
                            .map(|it| (it.sup_type, it.sub_region)),
                    );
                    constraint_pairs(
                        interner,
                        inner
                            .unwrap_region_constraints()
                            .region_constraint_data()
                            .constraints
                            .iter_from(constraints_before),
                        &mut pairs,
                    );
                    None
                }
            }
        };
        if let Some(refused) = refused {
            self.scratch.pairs.give(pairs);
            self.scratch.obligations.give(obligations);
            return refused;
        }
        if regionless && pairs.is_empty() && obligations.is_empty() {
            self.scratch.pairs.give(pairs);
            self.scratch.obligations.give(obligations);
            return false;
        }
```

(`errors` is hir-ty's `Vec` of the `ObligationCtxt`, mem-db-solver's.) The debug print
(6215-6217) formats `pairs`/`obligations` as slices, the same text. Lines 6232-6243, before:

```rust
        let elided = self.elided.borrow();
        let unknown: FxHashSet<Region<'db>> = pairs
            .iter()
            ...
            .map(|(it, _)| it)
            .collect();
        drop(elided);
        // `a <= b`: `b` outlives `a`.
        for (shorter, longer) in pairs {
```

after (`unknown` is empty but for a placeholder equated with an error region, so a frame set
that allocates only then; only `contains` is asked of it):

```rust
        let elided = self.elided.borrow();
        let mut unknown: FSet<'_, Region<'db>> = mem::set(self.ctx.frame);
        unknown.extend(
            pairs
                .iter()
                ...
                .map(|(it, _)| it),
        );
        drop(elided);
        // `a <= b`: `b` outlives `a`.
        for (shorter, longer) in pairs.drain(..) {
```

line 6270 `for (ty, region) in obligations {` -> `for (ty, region) in obligations.drain(..) {`,
and before the final `false` (6301):

```rust
        self.scratch.pairs.give(pairs);
        self.scratch.obligations.give(obligations);
        false
```

(`drain(..)` yields front to back, as the by-value loops did, and leaves the buffers empty.)

### R5.5 `prove_canonical` (lines 6307-6342): per solver call asked of the query

Before:

```rust
    fn prove_canonical(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
    ) -> Option<Result<(Vec<(Region<'db>, Region<'db>)>, Vec<(Ty<'db>, Region<'db>)>), bool>> {
        ...
        let (canonical, vars) = canonical_regions(self.ctx, trait_ref)?;
        ...
        let mut made = FxHashMap::default();
        let mut map = |region| region_from_query(self.ctx, &vars, &universes, &mut made, region);
        let pairs =
            pairs.iter().map(|(a, b)| (map(a.as_ref()), map(b.as_ref()))).collect::<Vec<_>>();
        let obligations = obligations
            .iter()
            .map(|(ty, region)| {
                let ty = rustc_type_ir::fold_regions(interner, ty.as_ref(), |it, _| map(it));
                (ty, map(region.as_ref()))
            })
            .collect();
        Some(Ok((pairs, obligations)))
    }
```

after (the answer goes into the caller's buffers; the query's `pairs`/`obligations` are the
memo's, read in place, so the locals are renamed `answer_pairs`/`answer_obligations`):

```rust
    fn prove_canonical(
        &mut self,
        trait_ref: rustc_type_ir::TraitRef<hir_ty::next_solver::DbInterner<'db>>,
        pairs: &mut FVec<'a, (Region<'db>, Region<'db>)>,
        obligations: &mut FVec<'a, (Ty<'db>, Region<'db>)>,
    ) -> Option<Result<(), bool>> {
        use rustc_type_ir::TypeVisitableExt as _;
        if trait_ref.has_placeholders() || trait_ref.has_non_region_infer() {
            return None;
        }
        let interner = self.interner();
        let mut vars = self.scratch.regions.take();
        let Some(canonical) = canonical_regions(self.ctx, trait_ref, &mut vars) else {
            self.scratch.regions.give(vars);
            return None;
        };
        let env = hir_ty::ParamEnvAndCrate { param_env: self.ctx.env, krate: self.ctx.krate };
        let (answer_pairs, answer_obligations, universes) = match prove_query(
            self.ctx.db,
            env.store(),
            hir_ty::next_solver::StoredTraitRef::new(canonical),
            vars.len() as u32,
        ) {
            Proven::Here => {
                self.scratch.regions.give(vars);
                return None;
            }
            &Proven::Error(true_error) => {
                self.scratch.regions.give(vars);
                return Some(Err(true_error));
            }
            Proven::Gives { pairs, obligations, universes, in_universe } => (
                pairs,
                obligations,
                Universes::new(self.ctx, self.ctx.frame, *universes, in_universe),
            ),
        };
        let mut made = mem::map(self.ctx.frame);
        let mut map = |region| region_from_query(self.ctx, &vars, &universes, &mut made, region);
        pairs.extend(answer_pairs.iter().map(|(a, b)| (map(a.as_ref()), map(b.as_ref()))));
        obligations.extend(answer_obligations.iter().map(|(ty, region)| {
            let ty = rustc_type_ir::fold_regions(interner, ty.as_ref(), |it, _| map(it));
            (ty, map(region.as_ref()))
        }));
        self.scratch.regions.give(vars);
        Some(Ok(()))
    }
```

The order `map` is called in is unchanged (each pair's `a` then `b`, then each obligation's
type's regions then its region), so the fresh variables `region_from_query` makes are numbered
as before. `made` stays a per-call map: it is empty when the answer makes no variable of its
own, the common case, and hashbrown allocates nothing until the first insert.

### R5.6 `Universes` (lines 6384-6400): per proof answered with universes

Before:

```rust
#[derive(Default)]
struct Universes {
    fresh: Vec<rustc_type_ir::UniverseIndex>,
    of_var: FxHashMap<u32, u32>,
}

impl Universes {
    fn new(ctx: &Ctx<'_, '_>, universes: u32, in_universe: &[(u32, u32)]) -> Self {
        Universes {
            fresh: (0..universes).map(|_| ctx.infcx.create_next_universe()).collect(),
            of_var: in_universe.iter().copied().collect(),
        }
    }
```

after (`in_universe` is the memo's, sorted by variable (6488), so it is searched in place
rather than copied into a map; `fresh` is a frame slice of its exact length):

```rust
struct Universes<'f> {
    fresh: &'f [rustc_type_ir::UniverseIndex],
    /// Each variable the answer made outside the root universe and that universe, sorted by
    /// the variable: the memo's own.
    of_var: &'f [(u32, u32)],
}

impl<'f> Universes<'f> {
    fn new(
        ctx: &Ctx<'_, '_>,
        frame: &'f Frame,
        universes: u32,
        in_universe: &'f [(u32, u32)],
    ) -> Self {
        Universes {
            fresh: frame
                .alloc_slice_fill_with(universes as usize, |_| ctx.infcx.create_next_universe()),
            of_var: in_universe,
        }
    }

    /// The universe of the answer's variable `var`, if it made it outside the root.
    fn of_var(&self, var: u32) -> Option<u32> {
        let at = self.of_var.binary_search_by_key(&var, |&(it, _)| it).ok()?;
        Some(self.of_var[at].1)
    }

    const NONE: Universes<'static> = Universes { fresh: &[], of_var: &[] };
```

(`get` unchanged.) `alloc_slice_fill_with` calls `create_next_universe` in index order, as the
`map` did. `frame` is passed apart from `ctx` because `Ctx<'a, 'db>` is invariant in `'a`
(its `RefCell<FMap<'a, ..>>`s): `'f` is then the shorter of the frame's borrow and the memo's
(the database's), both shared references. At 6327 (in R5.5's after code, the `Proven::Gives`
arm) `Universes::new(self.ctx, *universes, in_universe)` ->
`Universes::new(self.ctx, self.ctx.frame, *universes, in_universe)` (`in_universe: &Vec<(u32,
u32)>` derefs to the slice). The radix sort at
6488 sorts by `pack_32(vid, universe)`, vid first, and the vids are distinct (they were a
map's keys), so the slice is sorted by vid and `binary_search_by_key` finds each.

`region_from_query` (7216-7245), line 7220 `made: &mut FxHashMap<u32, Region<'db>>,` ->
`made: &mut FMap<'_, u32, Region<'db>>,`, line 7219 `universes: &Universes,` ->
`universes: &Universes<'_>,`, and line 7227
`match universes.of_var.get(&vid.as_u32()).and_then(|&it| universes.get(it)) {` ->
`match universes.of_var(vid.as_u32()).and_then(|it| universes.get(it)) {`.

`normalize_canonical` (7271) `let universes = Universes::default();` ->
`let universes = Universes::NONE;`.

### R5.7 `canonical_regions` (lines 7187-7211): per proof and per normalization asked

Before:

```rust
fn canonical_regions<'db, T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
    ctx: &Ctx<'_, 'db>,
    value: T,
) -> Option<(T, Vec<Region<'db>>)> {
    let interner = ctx.infcx.interner;
    let mut vars: Vec<Region<'db>> = Vec::new();
    ...
    (!here).then_some((canonical, vars))
}
```

after (the variables go into the caller's buffer, empty when called):

```rust
fn canonical_regions<'db, T: rustc_type_ir::TypeFoldable<hir_ty::next_solver::DbInterner<'db>>>(
    ctx: &Ctx<'_, 'db>,
    value: T,
    vars: &mut FVec<'_, Region<'db>>,
) -> Option<T> {
    let interner = ctx.infcx.interner;
    ...
    (!here).then_some(canonical)
}
```

(the fold's `vars.iter().position(..)` / `vars.push(..)` unchanged: linear in the trait
reference's distinct region variables, a few.) `normalize_canonical` (7261), a free function
without the shelf: `let (canonical, vars) = canonical_regions(ctx, ty)?;` ->
`let mut vars = Vec::new_in(ctx.frame); let canonical = canonical_regions(ctx, ty, &mut vars)?;`.

### R5.8 `normalize_at`, `normalize_full`, `normalize_canonical` (2974-3011, 7120-7181, 7251-7289)

Per normalization: the region constraints it recorded, as a vector, and the `same` map. Of
`normalize`/`normalize_impl` (the free form, from `liveness` too) the constraints are thrown
away after `same` is built from them.

`normalize_full`, before / after the signature and the two places that make the vector:

```rust
fn normalize_full<'db>(
    ctx: &Ctx<'_, 'db>,
    ty: Ty<'db>,
    allow_fresh: bool,
) -> Option<(Ty<'db>, Vec<hir_ty::next_solver::infer::region_constraints::Constraint<'db>>)> {
    ...
    let (normalized, constraints) = match normalize_canonical(ctx, ty) {
        Some(it) => it?,
        None => {
            ...
            let constraints = infcx
                .inner
                .borrow_mut()
                .unwrap_region_constraints()
                .region_constraint_data()
                .constraints
                .iter_from(constraints_before)
                .collect::<Vec<_>>();
            (normalized, constraints)
        }
    };
    let mut same = FxHashMap::default();
    for constraint in &constraints {
    ...
    (allow_fresh || !unmapped).then_some((normalized, constraints))
}
```

```rust
fn normalize_full<'db>(
    ctx: &Ctx<'_, 'db>,
    ty: Ty<'db>,
    allow_fresh: bool,
    constraints: &mut FVec<'_, hir_ty::next_solver::infer::region_constraints::Constraint<'db>>,
) -> Option<Ty<'db>> {
    ...
    let normalized = match normalize_canonical(ctx, ty, constraints) {
        Some(it) => it?,
        None => {
            ...
            constraints.extend(
                infcx
                    .inner
                    .borrow_mut()
                    .unwrap_region_constraints()
                    .region_constraint_data()
                    .constraints
                    .iter_from(constraints_before),
            );
            normalized
        }
    };
    let mut same = mem::map(ctx.frame);
    for constraint in constraints.iter() {
    ...
    (allow_fresh || !unmapped).then_some(normalized)
}
```

(`same` only looked up, `entry`/`get`; it allocates only for a normalization that made
variables of its own.) `normalize_canonical`:
`fn normalize_canonical<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, constraints: &mut FVec<'_,
Constraint<'db>>) -> Option<Option<Ty<'db>>>`, its lines 7275-7288 `let constraints = pairs
.iter().map(..).collect(); Some(Some((normalized, constraints)))` ->
`constraints.extend(pairs.iter().map(..)); Some(Some(normalized))` (the `map` body unchanged;
`made` there `mem::map(ctx.frame)`, line 7270).

`normalize_impl` (7114-7116), before / after:

```rust
fn normalize_impl<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, allow_fresh: bool) -> Option<Ty<'db>> {
    normalize_full(ctx, ty, allow_fresh).map(|(it, _)| it)
}
```

```rust
fn normalize_impl<'db>(ctx: &Ctx<'_, 'db>, ty: Ty<'db>, allow_fresh: bool) -> Option<Ty<'db>> {
    normalize_full(ctx, ty, allow_fresh, &mut Vec::new_in(ctx.frame))
}
```

(a normalization that records no constraint, most, allocates nothing; one that does writes
its constraints once to the frame. The free form has no shelf: it is called from `liveness`
and `check` with only `ctx`; a buffer on `Ctx` would change the parent's B4.4, which it does
not need for the bound.)

`normalize_at` (2974-3011), lines 2977 and 2998, before:

```rust
        let (normalized, constraints) = normalize_full(self.ctx, ty, false)?;
        ...
        for constraint in constraints {
```

after:

```rust
        let mut constraints = self.scratch.constraints.take();
        let Some(normalized) = normalize_full(self.ctx, ty, false, &mut constraints) else {
            self.scratch.constraints.give(constraints);
            return None;
        };
        ...
        for constraint in constraints.drain(..) {
```

and before `Some(normalized)` (3010): `self.scratch.constraints.give(constraints);`.

### R5.9 `constraint_pairs` (lines 6403-6410)

Before:

```rust
fn constraint_pairs<'db>(
    interner: hir_ty::next_solver::DbInterner<'db>,
    constraints: impl IntoIterator<
        Item = hir_ty::next_solver::infer::region_constraints::Constraint<'db>,
    >,
) -> Vec<(Region<'db>, Region<'db>)> {
    constraints.into_iter().map(|constraint| constraint.regions(interner)).collect()
}
```

after:

```rust
fn constraint_pairs<'db>(
    interner: hir_ty::next_solver::DbInterner<'db>,
    constraints: impl IntoIterator<
        Item = hir_ty::next_solver::infer::region_constraints::Constraint<'db>,
    >,
    out: &mut FVec<'_, (Region<'db>, Region<'db>)>,
) {
    out.extend(constraints.into_iter().map(|constraint| constraint.regions(interner)));
}
```

### R5.10 The two queries: `prove_query` (6415-6498), `normalize_query` (7307-7358)

Salsa tracked functions, run once per environment and goal, not per body: each runs in a
frame task of its own (as the parent's B4.1 does for `borrowck`), its temporaries in it, its
memo (`Proven`, `Normalized`) owned and of exact size. They are called from inside a body's
task, so the task is nested (a frame off the thread's spares).

`prove_query`, before (6421, 6451-6497):

```rust
) -> Proven {
    ...
    let mut inner = infcx.inner.borrow_mut();
    let obligations: Vec<_> =
        inner.region_obligations().iter().map(|it| (it.sup_type, it.sub_region)).collect();
    let mut constraints = inner.unwrap_region_constraints();
    let pairs = constraint_pairs(interner, constraints.region_constraint_data().constraints.iter());
    let mut universes = 0;
    let mut in_universe = FxHashMap::default();
    ...
    let mut in_universe: Vec<_> = in_universe.into_iter().collect();
    radix::sort_copy_by_key(&mut in_universe, |&(vid, universe)| radix::pack_32(vid, universe));
    Proven::Gives {
        pairs: pairs.into_iter().map(|(a, b)| (a.store(), b.store())).collect(),
        obligations: obligations
            .into_iter()
            .map(|(ty, region)| (ty.store(), region.store()))
            .collect(),
        universes,
        in_universe,
    }
}
```

after (the whole body from `let env = env.as_ref(db);` inside the task):

```rust
) -> Proven {
    blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
        ...
        let mut inner = infcx.inner.borrow_mut();
        let obligations = mem::collect(
            frame,
            inner.region_obligations().iter().map(|it| (it.sup_type, it.sub_region)),
        );
        let mut constraints = inner.unwrap_region_constraints();
        let mut pairs = Vec::new_in(frame);
        constraint_pairs(
            interner,
            constraints.region_constraint_data().constraints.iter(),
            &mut pairs,
        );
        let mut universes = 0;
        let mut in_universe: FMap<'_, u32, u32> = mem::map(frame);
        ...
        let mut in_universe = mem::owned(in_universe.into_iter());
        radix::sort_copy_by_key(&mut in_universe, |&(vid, universe)| radix::pack_32(vid, universe));
        Proven::Gives {
            pairs: mem::owned(pairs.iter().map(|&(a, b)| (a.store(), b.store()))),
            obligations: mem::owned(
                obligations.iter().map(|&(ty, region)| (ty.store(), region.store())),
            ),
            universes,
            in_universe,
        }
    })
}
```

`in_universe`'s map order does not reach the memo: it is sorted by its distinct keys right
after. hashbrown's `IntoIter` is `ExactSizeIterator`, which `mem::owned` needs. The two early
returns (6449, 6485) return from the task's closure the same `Proven`. `infcx` and
`ObligationCtxt` (hir-ty's) are built inside the task, so what mem-db-solver puts on
`frame::current` is this task's frame.

`normalize_query`, the same: its body in `blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame|
{ .. })`, line 7338 `let pairs = constraint_pairs(..);` -> `let mut pairs = Vec::new_in(frame);
constraint_pairs(interner, constraints.region_constraint_data().constraints.iter(), &mut
pairs);`, and line 7357 `pairs: pairs.into_iter().map(|(a, b)| (a.store(), b.store()))
.collect(),` -> `pairs: mem::owned(pairs.iter().map(|&(a, b)| (a.store(), b.store()))),`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-regions (2026-10-29 22:10): R5.1-R5.10 as written, with every caller (`unsize` 5360, `signature_higher_ranked`, `statement`'s `opaque_bounds`). Differences: `prove_canonical`'s `#[allow(clippy::type_complexity)]` and `normalize_canonical`'s go (the types are simple now), and `normalize_canonical`'s local `use ..::Constraint` goes for R3's module import. `instantiate` in `clauses` was already generic (dedyn).

---

## R6. The signature and the body's setup: once per body

Each of these runs once per body (or once per closure parameter, annotated `let`, spelled
path); each temporary becomes a frame value of `self.ctx.frame`. Where a closure needs the
frame, `let frame = self.ctx.frame;` comes first. Where the walk's shelves (R3) serve a
per-path or per-clause buffer, it takes one.

> forwarded to db (db-ty-d-borrowck) (requests, pass 18; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:55): not applied, waits on R1-R2 (`Regions<'f, 'db>`,
> `build`'s frame) and B0-B1.
> applied by db-mem-borrowck-regions (2026-10-29 22:25): R6.1-R6.3 as written, found by old text (all moved ~50 lines down). `errors` (now line 1167) keeps `vec!` with the `// alloc:` note, as `ElidedLifetimes::regions` (lower.rs 4008) still takes `&[Vec<Region>]`. `positions.iter().flatten()` works on `spelled_positions`'s return before and after R7.5.

### R6.1 `universal_regions` (lines 1037-1666)

| line | what | before | after |
|---|---|---|---|
| 1042 | each local's type | `let mut local_tys: Vec<Option<Ty<'db>>> = vec![None; body.locals.len()];` | `let mut local_tys: FVec<'_, Option<Ty<'db>>> = filled(self.ctx.frame, body.locals.len(), None);` |
| 1062 | the parameters' types with `Self` free | `let mut self_free: Vec<Option<Ty<'db>>> = Vec::new();` | `let mut self_free: FVec<'_, Option<Ty<'db>>> = Vec::new_in(self.ctx.frame);` |
| 1073 | the signature's inputs | `let mut inputs: Vec<Ty<'db>> = sig.inputs().iter().copied().collect();` | `let mut inputs: FVec<'_, Ty<'db>> = mem::collect(self.ctx.frame, sig.inputs().iter().copied());` |
| 1133 | per input: a region per elided group | `let mut of_group: Vec<Option<Region<'db>>> = Vec::new();` | `let mut of_group: FVec<'_, Option<Region<'db>>> = Vec::new_in(self.ctx.frame);` |
| 1204-1212 | `self_free` | `self_free = inputs.iter().zip(&written.params).map(..).collect();` | `self_free = mem::collect(self.ctx.frame, inputs.iter().zip(&written.params).map(..));` (the `map` body unchanged) |
| 1218 | the parameters' universal regions | `let mut inputs = Vec::new();` | `let mut inputs = Vec::new_in(self.ctx.frame);` |
| 1490 | the return type's regions | `let mut unsuggested = Vec::new();` | `let mut unsuggested = Vec::new_in(self.ctx.frame);` |
| 1512 | the parameters' regions | `let mut in_params = Vec::new();` | `let mut in_params = Vec::new_in(self.ctx.frame);` |
| 1538 | the member constraints' choices | `let mut choices = vec![self.regions.static_rv];` | `let mut choices = Vec::new_in(self.ctx.frame); choices.push(self.regions.static_rv);` |
| 1557 | the hidden type's outlived regions | `let mut outlived = Vec::new();` | `let mut outlived = Vec::new_in(self.ctx.frame);` |
| 1600 | the member regions | `let mut members = Vec::new();` | `let mut members = Vec::new_in(self.ctx.frame);` |
| 1607 | the bounded regions | `let mut bounded = Vec::new();` | `let mut bounded = Vec::new_in(self.ctx.frame);` |

Line 1116, `let mut errors = vec![Vec::new(); inputs.len()];`, goes to
`hir_ty::elided_lifetimes(..).regions(&errors)` (1167), which takes `&[Vec<Region<'db>>]`
(`hir-ty/src/lower.rs` 3948). The parent's W1 (`elided_output`) handles the same call this
way: the line stays with `// alloc: hir-ty's ElidedLifetimes::regions takes vectors (asked of
mem-db)` until that takes `&[impl AsRef<[Region<'db>]>]`, then becomes
`let mut errors = mem::collect(self.ctx.frame, (0..inputs.len()).map(|_| Vec::new_in(self.ctx.frame)));`.
What `regions` returns, a `Vec<Option<Region>>`, is hir-ty's.

Lines 1518-1525, a set of universal indices only asked `contains`, before:

```rust
            let in_params: FxHashSet<usize> =
                in_params.into_iter().filter_map(|rv| self.regions.universal(rv)).collect();
            for index in 0..self.regions.universals.len() {
                if self.regions.universals[index].1 == UniversalKind::AnonParam
                    && !in_params.contains(&index)
                {
```

after (a universal index is below `universals.len()`):

```rust
            let mut universal_in_params =
                BitSet::new(self.ctx.frame, self.regions.universals.len());
            for rv in in_params {
                if let Some(index) = self.regions.universal(rv) {
                    universal_in_params.insert(index);
                }
            }
            for index in 0..self.regions.universals.len() {
                if self.regions.universals[index].1 == UniversalKind::AnonParam
                    && !universal_in_params.contains(index)
                {
```

Lines 1535-1536, before:

```rust
                && let Some(captured) = opaque_captures(self.ctx.db, opaque_id, alias.args.len())
                    .or_else(|| variadic.then(|| vec![true; alias.args.len()]))
```

after:

```rust
                && let Some(captured) =
                    opaque_captures(self.ctx.frame, self.ctx.db, opaque_id, alias.args.len())
                        .or_else(|| variadic.then(|| filled(self.ctx.frame, alias.args.len(), true)))
```

Lines 1601 and 1608, `member_regions(self.ctx.db, hidden, true, &mut members);` ->
`member_regions(self.ctx.frame, self.ctx.db, hidden, true, &mut members);` (and `false`,
`&mut bounded`); see R8.3. `radix::sort(&mut members)` takes `&mut [K]`: unchanged.

Line 1665, before / after:

```rust
        self.regions.local_tys = local_tys.into_iter().map(|it| it.unwrap()).collect();
```

```rust
        self.regions.local_tys = mem::collect(self.ctx.frame, local_tys.into_iter().map(|it| it.unwrap()));
```

(`closure_signature(&mut local_tys)` takes `&mut [Option<Ty<'db>>]`, which the frame vector
derefs to.)

### R6.2 Closures, annotations, paths (lines 1699-2382)

| line | per | before | after |
|---|---|---|---|
| 1759 | coroutine closure | `let mut late = FxHashMap::default();` | `let mut late = mem::map(self.ctx.frame);` |
| 1807 | closure | `let mut late = FxHashMap::default();` | `let mut late = mem::map(self.ctx.frame);` |
| 1986 | closure with written parameter types | `let written: Vec<_> = params.iter().copied().zip(arg_types.iter().copied()).chain([(super::return_slot(), ret_type)]).collect();` | `let written = mem::collect(self.ctx.frame, params.iter().copied().zip(arg_types.iter().copied()).chain([(super::return_slot(), ret_type)]));` |
| 2059 | struct pattern in an annotated `let` | `let parts: Vec<_> = args.iter().filter_map(..).collect();` | `let parts = mem::collect(self.ctx.frame, args.iter().filter_map(..));` |
| 2078 | tuple-struct pattern there | `let parts: Vec<_> = args.iter().zip(types.iter()).map(..).collect();` | `let parts = mem::collect(self.ctx.frame, args.iter().zip(types.iter()).map(..));` |
| 2101 | body: spelled paths' type arguments | `let mut found = Vec::new();` | `let mut found = Vec::new_in(self.ctx.frame);` |
| 2102 | body: spelled callees | `let mut items = Vec::new();` | `let mut items = Vec::new_in(self.ctx.frame);` |

(`late` at 1767/1815 is only `*late.entry(..).or_insert_with(..)`; order free.)

`elided_own` (1870-1912), lines 1879-1898, per closure parameter, before:

```rust
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new();
            ...
            found
        };
        let elided: Vec<bool> = regions(written)
            .into_iter()
            .map(|region| { .. })
            .collect();
```

after:

```rust
        let frame = self.ctx.frame;
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new_in(frame);
            ...
            found
        };
        let elided: FVec<'_, bool> =
            mem::collect(frame, regions(written).into_iter().map(|region| { .. }));
```

`path_arguments`, line 2115, `let positions = self.spelled_positions(expr, path).unwrap_or_default();`
-> `let positions = self.spelled_positions(expr, path);` (a frame vector has no `Default`),
and line 2124 `positions.iter()` -> `positions.iter().flatten()` (the same items, `&(TypeRefId,
usize)`, none for `None`).

`ascribed_place` (2206-2217): both callers (1949-1951, 2163-2165) use it only for
`self.place_ty(local, &projection).ty`, and it copies the projection to a `Vec` for that. It
becomes `ascribed_ty`, which reads the projection in place and allocates nothing. Before:

```rust
    fn ascribed_place(&self, pat: hir_def::hir::PatId) -> Option<(LocalId, Vec<PlaceElem>)> {
        self.body.basic_blocks.iter().find_map(|(_, block)| {
            block.statements.iter().find_map(|statement| match &statement.kind {
                StatementKind::FakeRead(place) if statement.span == MirSpan::PatId(pat) => {
                    let place = place.as_ref();
                    Some((place.local, place.projection.as_slice().to_vec()))
                }
                _ => None,
            })
        })
    }
```

after:

```rust
    /// The type of the place the `let` of the pattern `pat` reads its value from: the fake
    /// read lowered at the pattern.
    fn ascribed_ty(&self, pat: hir_def::hir::PatId) -> Option<Ty<'db>> {
        self.body.basic_blocks.iter().find_map(|(_, block)| {
            block.statements.iter().find_map(|statement| match &statement.kind {
                StatementKind::FakeRead(place) if statement.span == MirSpan::PatId(pat) => {
                    let place = place.as_ref();
                    Some(self.place_ty(place.local, place.projection.as_slice()).ty)
                }
                _ => None,
            })
        })
    }
```

and at 1949-1951 and 2163-2165, `None => self.ascribed_place(pat).map(|(local, projection)|
self.place_ty(local, &projection).ty),` -> `None => self.ascribed_ty(pat),`.

`path_well_formed` (2345-2369), per path written through a type, lines 2357 and 2366-2368,
before:

```rust
        let mut tys: Vec<Ty<'db>> = args.types().collect();
        ...
        for ty in tys {
            self.well_formed(ty, at, 0);
        }
```

after:

```rust
        let mut tys = self.scratch.tys.take();
        tys.extend(args.types());
        ...
        for &ty in tys.iter() {
            self.well_formed(ty, at, 0);
        }
        self.scratch.tys.give(tys);
```

`annotated_locals` (2371-2381), once per body, only looked up (1645): `fn annotated_locals(&self)
-> FMap<'a, LocalId, Annotation> { let mut found = mem::map(self.ctx.frame); .. }`.

### R6.3 The environment and the signature's bounds (lines 2383-2965)

`env_aliases` (2383-2443): `behind_refs` (2384) `out: &mut Vec<Ty<'db>>` ->
`out: &mut FVec<'_, Ty<'db>>`; line 2410 `let mut aliases = Vec::new();` ->
`let mut aliases = Vec::new_in(self.ctx.frame);`; 2419-2425 `let signature: Vec<Ty<'db>> =
self.body.param_locals.iter()...collect();` -> `let signature = mem::collect(self.ctx.frame,
self.body.param_locals.iter()...);` (the chain and `map` unchanged); 2426 `let mut found =
Vec::new();` -> `let mut found = Vec::new_in(self.ctx.frame);`. Lines 2437-2442, before:

```rust
        self.env_aliases = aliases
            .into_iter()
            .map(|ty| {
                rustc_type_ir::fold_regions(interner, ty, |_, _| Region::new_static(interner))
            })
            .collect();
```

after (in place: the same types in the same order, one vector instead of two):

```rust
        for ty in aliases.iter_mut() {
            *ty = rustc_type_ir::fold_regions(interner, *ty, |_, _| Region::new_static(interner));
        }
        self.env_aliases = aliases;
```

`signature_projections` (2493-2589): 2498 `let mut trait_refs = Vec::new();` ->
`Vec::new_in(self.ctx.frame)`; 2499-2505 `let mut tys: Vec<Ty<'db>> = self.body.param_locals
...collect();` -> `let mut tys = mem::collect(self.ctx.frame, self.body.param_locals...);`;
2507 `let mut seen = FxHashSet::default();` -> `let mut seen = mem::set(self.ctx.frame);`
(`insert` and `len` only); 2575 `let mut hidden = Vec::new();` ->
`let mut hidden = Vec::new_in(self.ctx.frame);` (`hidden_types` takes it, R8.2).

`known_relations` (2590-2692):

| line | before | after |
|---|---|---|
| 2597 | `let mut pairs = Vec::new();` | `let mut pairs = Vec::new_in(self.ctx.frame);` |
| 2627 | `let clauses: Vec<_> = self.ctx.env.caller_bounds().collect();` | `let clauses = mem::collect(self.ctx.frame, self.ctx.env.caller_bounds());` |
| 2651-2657 | `let signature: Vec<Ty<'db>> = self.body.param_locals...collect();` | `let signature = mem::collect(self.ctx.frame, self.body.param_locals...);` |
| 2658-2659 | `let mut signature = signature; signature.extend(self.impl_header_types());` | `let mut signature = signature; self.impl_header_types(&mut signature);` |
| 2660 | `let mut seen = FxHashSet::default();` | `let mut seen = mem::set(self.ctx.frame);` |
| 2665 | `let mut known = BitMatrix::new(count, count);` | `let mut known = BitMatrix::new(self.ctx.frame, count, count);` |

(`for clause in clauses`, `for ty in signature`, `for (longer, shorter) in pairs` by value,
unchanged; `self.regions.known = known;` moves the frame matrix in.)

`self_reference` (2699-2729), once per method with a `self`: 2709-2710 `let mut found =
Vec::new(); let mut stack = vec![ty];` -> `let mut found = Vec::new_in(self.ctx.frame); let mut
stack = Vec::new_in(self.ctx.frame); stack.push(ty);`.

`impl_anon_region` (2763-2786), per creator position past `IMPL_ANON` asked: 2770-2771
`let mut anonymous = Vec::new(); let mut errors = Vec::new();` ->
`let mut anonymous = Vec::new_in(self.ctx.frame); let mut errors = Vec::new_in(self.ctx.frame);`.

`impl_header_types` (2789-2825), before:

```rust
    fn impl_header_types(&mut self) -> Vec<Ty<'db>> {
        if self.ctx.closure_local.is_some() {
            return Vec::new();
        }
        let container = match self.body.owner {
            ...
            _ => return Vec::new(),
        };
        let ItemContainerId::ImplId(impl_) = container else {
            return Vec::new();
        };
        let db = self.ctx.db;
        let raw = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let mut tys = vec![match self.impl_self {
            Some((from, to)) if from == raw => to,
            _ => raw,
        }];
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            tys.extend(trait_ref.args.iter().skip(1).filter_map(|arg| match arg.kind() {
                GenericArgKind::Type(it) => Some(it),
                _ => None,
            }));
        }
        tys.into_iter()
            .map(|ty| {
                rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                    self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
                })
            })
            .collect()
    }
```

after (appended to the caller's vector and folded in place, in the same order, so the
universal and fresh regions are made in the same order):

```rust
    fn impl_header_types(&mut self, out: &mut FVec<'a, Ty<'db>>) {
        if self.ctx.closure_local.is_some() {
            return;
        }
        let container = match self.body.owner {
            ...
            _ => return,
        };
        let ItemContainerId::ImplId(impl_) = container else {
            return;
        };
        let db = self.ctx.db;
        let raw = db.impl_self_ty(impl_).instantiate_identity().skip_norm_wip();
        let start = out.len();
        out.push(match self.impl_self {
            Some((from, to)) if from == raw => to,
            _ => raw,
        });
        if let Some(trait_ref) = db.impl_trait(impl_) {
            let trait_ref = trait_ref.instantiate_identity().skip_norm_wip();
            out.extend(trait_ref.args.iter().skip(1).filter_map(|arg| match arg.kind() {
                GenericArgKind::Type(it) => Some(it),
                _ => None,
            }));
        }
        for at in start..out.len() {
            let ty = out[at];
            out[at] = rustc_type_ir::fold_regions(self.interner(), ty, |region, depth| {
                self.signature_region(region, depth).unwrap_or_else(|| self.fresh())
            });
        }
    }
```

`implied_bounds` (2827-2965), recursive over the signature's types, per type:
- signature (2829-2832): `pairs: &mut Vec<(Rv, Rv)>, seen: &mut FxHashSet<(Ty<'db>, bool)>,`
  -> `pairs: &mut FVec<'a, (Rv, Rv)>, seen: &mut FSet<'a, (Ty<'db>, bool)>,`.
- lines 2845-2847, before:

  ```rust
                    let (components, params) = self.implied_components(referent);
                    pairs.extend(components.into_iter().map(|longer| (longer, shorter)));
                    self.regions.type_bounds.extend(params.into_iter().map(|it| (it, shorter)));
  ```

  after:

  ```rust
                    let mut components = self.scratch.rvs.take();
                    let mut params = self.scratch.tys.take();
                    self.implied_components(referent, &mut components, &mut params);
                    pairs.extend(components.iter().map(|&longer| (longer, shorter)));
                    self.regions.type_bounds.extend(params.iter().map(|&it| (it, shorter)));
                    self.scratch.rvs.give(components);
                    self.scratch.tys.give(params);
  ```

- lines 2867-2870 and 2890, before:

  ```rust
                let mut clauses: Vec<_> = GenericPredicates::query_all(self.ctx.db, adt.into())
                    .iter_instantiated(self.interner(), args.as_slice())
                    .map(|it| it.skip_norm_wip())
                    .collect();
                ...
                self.implied_by_clauses(clauses, pairs);
  ```

  after:

  ```rust
                let mut clauses = self.scratch.clauses.take();
                clauses.extend(
                    GenericPredicates::query_all(self.ctx.db, adt.into())
                        .iter_instantiated(self.interner(), args.as_slice())
                        .map(|it| it.skip_norm_wip()),
                );
                ...
                self.implied_by_clauses(&clauses, pairs);
                self.scratch.clauses.give(clauses);
  ```

  (`clauses.push(..)` at 2879 and the inner loop's `continue` at 2886 unchanged.)
- lines 2947-2955, the same: `let mut clauses = self.scratch.clauses.take();
  clauses.extend(GenericPredicates::query_all(..).iter_instantiated(..).map(..).filter(..));
  self.implied_by_clauses(&clauses, pairs); self.scratch.clauses.give(clauses);`.

`well_formed` (2239-2314), `well_formed_unbound` (2315-2344) and the loops of `env_aliases`
and `signature_higher_ranked` (R5.2) allocate nothing else.

---

## R7. The rest of the walk: calls, aggregates, written types

`statement` (3952-4001), `assign` (4003-4102), `rvalue_ty` (4694-5150), `terminator`
(5302-5556), `reborrow_constraints`, `unsize`, `unsize_pointee`, `tail`, `copy_bound`,
`sized_local`, `constant_ty`, `fn_item`, `asm_sym`, `written_self`, `impl_self_args`,
`place_ty`, `operand_ty`: no allocation of their own but those below.

> forwarded to db (db-ty-d-borrowck) (requests, pass 18; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:55): not applied, waits on R2-R3 (`Builder`'s frame,
> `Shelf`/`Scratch`) and B0-B1.
> applied by db-mem-borrowck-regions (2026-10-29 21:50): R7.1-R7.6 as written, found by old text (all ~110 lines down). `elide`'s `errors` keeps `vec!` with the `// alloc:` note (ElidedLifetimes::regions still takes vectors).

### R7.1 `terminator`, `Call` (lines 5355-5497): per call

Per call: the callee's inputs as a vector, `elide`'s copy of them, a vector of projections per
input type, and on a method with a written `Self` the `writes` flags. The inputs go in one
shelf buffer that `elide` fills; the projections in another, reused per type.

Lines 5399-5403, before / after (per method call with a written `Self`):

```rust
                                    let writes: Vec<bool> = written
                                        .params
                                        .iter()
                                        .map(|&param| writes_self(&written.store, param))
                                        .collect();
```

```rust
                                    let writes: FVec<'_, bool> = mem::collect(
                                        self.ctx.frame,
                                        written.params.iter().map(|&param| writes_self(&written.store, param)),
                                    );
```

Lines 5427-5452 (the `FnDef` arm's end), before:

```rust
                        let (inputs, output) = self.elide(
                            function,
                            sig.inputs().iter().copied().collect(),
                            sig.output(),
                            self_param,
                            keep_static,
                            returns_self,
                            static_self,
                            impl_self,
                        );
                        ...
                        let at = At { category: Category::Predicate, span };
                        for ty in inputs.iter().copied().chain([output]) {
                            for (def, args) in projections(ty) {
                                if let Ok(def) = GenericDefId::try_from(def) {
                                    self.predicates(def, args, at, false);
                                }
                            }
                        }
                        (inputs, output)
```

after:

```rust
                        let mut inputs = self.scratch.tys.take();
                        let output = self.elide(
                            function,
                            sig.inputs().as_slice(),
                            sig.output(),
                            self_param,
                            keep_static,
                            returns_self,
                            static_self,
                            impl_self,
                            &mut inputs,
                        );
                        ...
                        let at = At { category: Category::Predicate, span };
                        let mut found = self.scratch.projections.take();
                        for ty in inputs.iter().copied().chain([output]) {
                            projections(ty, &mut found);
                            for (def, args) in found.drain(..) {
                                if let Ok(def) = GenericDefId::try_from(def) {
                                    self.predicates(def, args, at, false);
                                }
                            }
                        }
                        self.scratch.projections.give(found);
                        (inputs, output)
```

(the `well_formed` loop between them unchanged; `sig.inputs().as_slice()` as at 5515.)

Line 5461 (the `FnPtr` arm), before / after:

```rust
                        (sig.inputs().iter().copied().collect::<Vec<_>>(), sig.output())
```

```rust
                        let mut inputs = self.scratch.tys.take();
                        inputs.extend(sig.inputs().iter().copied());
                        (inputs, sig.output())
```

(the `_ => return,` arm takes nothing.) Lines 5476-5490, before:

```rust
                let inputs = match (func_ty.kind(), inputs.as_slice()) {
                    (TyKind::FnDef(def, _), &[receiver, tupled])
                        if ... =>
                    {
                        std::iter::once(receiver).chain(elements.iter()).collect()
                    }
                    _ => inputs,
                };
                for (arg, input) in args.iter().zip(inputs) {
                    ...
                }
```

after (the untupled inputs written over the buffer instead of into a new one; the guard
unchanged, its `elements` an interned `Tys`, `Copy`):

```rust
                let untupled = match (func_ty.kind(), inputs.as_slice()) {
                    (TyKind::FnDef(def, _), &[receiver, tupled])
                        if ... =>
                    {
                        Some((receiver, elements))
                    }
                    _ => None,
                };
                if let Some((receiver, elements)) = untupled {
                    inputs.clear();
                    inputs.push(receiver);
                    inputs.extend(elements.iter());
                }
                for (arg, input) in args.iter().zip(inputs.iter().copied()) {
                    ...
                }
                self.scratch.tys.give(inputs);
```

(`inputs` is not read after line 5497; the `inputs` at 5516 is another binding.)

`projections` (lines 268-284), before:

```rust
fn projections<'db>(ty: Ty<'db>) -> Vec<(hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)> {
    struct Find<'db>(Vec<(hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>);
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find<'db> {
        ...
                self.0.push((def_id.into(), alias.args));
        ...
    }
    let mut find = Find(Vec::new());
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut find);
    find.0
}
```

after:

```rust
fn projections<'db>(
    ty: Ty<'db>,
    out: &mut FVec<'_, (hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>,
) {
    struct Find<'o, 'f, 'db>(&'o mut FVec<'f, (hir_ty::next_solver::SolverDefId<'db>, GenericArgs<'db>)>);
    impl<'db> rustc_type_ir::TypeVisitor<hir_ty::next_solver::DbInterner<'db>> for Find<'_, '_, 'db> {
        ...
                self.0.push((def_id.into(), alias.args));
        ...
    }
    rustc_type_ir::TypeVisitable::visit_with(&ty, &mut Find(out));
}
```

(the stray first doc line at 266, "Each region in `ty`, in order, those under a binder aside.",
belongs to `regions_of` at 699; unchanged here.)

### R7.2 `elide` (lines 5592-5701): per call of a function item

Before (signature and the allocations):

```rust
    fn elide(
        &self,
        function: Option<hir_def::FunctionId>,
        inputs: Vec<Ty<'db>>,
        output: Ty<'db>,
        self_param: bool,
        keep_static: bool,
        returns_self: bool,
        static_self: bool,
        impl_self: Option<(Ty<'db>, Vec<bool>, bool)>,
    ) -> (Vec<Ty<'db>>, Ty<'db>) {
        ...
        let mut errors = vec![Vec::new(); inputs.len()];
        let (impl_self, writes_self, ret_self) = match impl_self {
            ...
            None => (None, Vec::new(), false),
        };
        ...
        let inputs: Vec<Ty<'db>> = inputs
            .into_iter()
            .enumerate()
            .map(|(index, ty)| { .. })
            .collect();
        let self_region = match inputs.first().map(|it| it.kind()) {
        ...
        let elided = self_region.or_else(|| {
            let mut distinct = Vec::new();
            ...
        });
        ...
        (inputs, rustc_type_ir::TypeFoldable::fold_with(output, &mut defaults))
    }
```

after (the inputs into the caller's buffer, `distinct` from the shelf, so `&mut self`; every
other line unchanged):

```rust
    fn elide(
        &mut self,
        function: Option<hir_def::FunctionId>,
        inputs: &[Ty<'db>],
        output: Ty<'db>,
        self_param: bool,
        keep_static: bool,
        returns_self: bool,
        static_self: bool,
        impl_self: Option<(Ty<'db>, FVec<'_, bool>, bool)>,
        out: &mut FVec<'a, Ty<'db>>,
    ) -> Ty<'db> {
        ...
        let mut errors = vec![Vec::new(); inputs.len()]; // alloc: hir-ty's ElidedLifetimes::regions takes vectors (asked of mem-db)
        let (impl_self, writes_self, ret_self) = match impl_self {
            ...
            None => (None, Vec::new_in(self.ctx.frame), false),
        };
        ...
        out.extend(inputs.iter().copied().enumerate().map(|(index, ty)| { .. }));
        let inputs: &[Ty<'db>] = out;
        let self_region = match inputs.first().map(|it| it.kind()) {
        ...
        let mut distinct = self.scratch.regions.take();
        let elided = self_region.or_else(|| {
            ...
        });
        self.scratch.regions.give(distinct);
        ...
        rustc_type_ir::TypeFoldable::fold_with(output, &mut defaults)
    }
```

where the `or_else` closure's `let mut distinct = Vec::new();` line goes (it pushes to the
taken `distinct` and reads `distinct.as_slice()` as before). The `map` closure borrows
`self` shared (`fresh_elided`), `errors` and `defaults`; `out` is the caller's, apart from
`self`. `errors` is the one global allocation left per call (its outer vector; the inner ones
are empty until an elided lifetime is found): hir-ty's `ElidedLifetimes::regions` (lower.rs
3948) takes `&[Vec<Region<'db>>]`. Once it takes `&[impl AsRef<[Region<'db>]>]` (the parent's
ask of mem-db), `errors` becomes `mem::collect(self.ctx.frame, (0..inputs.len()).map(|_|
Vec::new_in(self.ctx.frame)))`, and it is the per-call bump that remains.

### R7.3 `rvalue_ty`, closure and coroutine aggregates (lines 5068-5142): per closure made

Lines 5070-5079, before:

```rust
                let lifetimes = |parents: &[hir_ty::next_solver::GenericArg<'db>]| {
                    parents
                        .iter()
                        .enumerate()
                        .filter_map(|(index, arg)| match arg.kind() { .. })
                        .collect::<Vec<_>>()
                };
```

after:

```rust
                let frame = self.ctx.frame;
                let lifetimes = |parents: &[hir_ty::next_solver::GenericArg<'db>]| {
                    mem::collect(
                        frame,
                        parents.iter().enumerate().filter_map(|(index, arg)| match arg.kind() { .. }),
                    )
                };
```

and `closure_requirements` (3292) `parents: Vec<(u32, Region<'db>)>,` ->
`parents: FVec<'_, (u32, Region<'db>)>,` (its `for (index, region) in parents` unchanged).
Lines 5091-5096, before / after:

```rust
                    self.predicates(
                        def,
                        GenericArgs::new_from_iter(parent_args.iter().copied().collect::<Vec<_>>()),
                        at,
                        true,
                    );
```

```rust
                    self.predicates(def, GenericArgs::new_from_slice(parent_args.as_slice()), at, true);
```

(`parent_args` is the slice `lifetimes(args.as_coroutine().parent_args().as_slice())` at 5124
reads the same way.)

### R7.4 `written` and `written_callee_ty` (lines 3820-3909): per written type

Line 3831 `let mut vars = FxHashMap::default();` -> `let mut vars = mem::map(self.ctx.frame);`
(`entry` only; a written type with no inference region in it, most, allocates nothing).

Lines 3887-3888, before / after (`take(parent)` of the arguments, a prefix):

```rust
                    let args =
                        GenericArgs::new_from_iter(args.iter().take(parent).collect::<Vec<_>>());
```

```rust
                    let args = GenericArgs::new_from_slice(
                        args.as_slice().get(..parent).unwrap_or(args.as_slice()),
                    );
```

Lines 3900-3904, before / after:

```rust
                let args = args.iter().enumerate().map(|(index, arg)| match index < parent {
                    true => self.renumber(arg),
                    false => self.written(arg),
                });
                let args = GenericArgs::new_from_iter(args.collect::<Vec<_>>());
```

```rust
                let frame = self.ctx.frame;
                let args = mem::collect(
                    frame,
                    args.iter().enumerate().map(|(index, arg)| match index < parent {
                        true => self.renumber(arg),
                        false => self.written(arg),
                    }),
                );
                let args = GenericArgs::new_from_slice(&args);
```

### R7.5 The annotation paths (lines 4022, 4301-4639): per annotated `let`, cast, spelled path

`assign`, line 4022 `self.zip_annotation(lowered, inferred, &mut FxHashMap::default(), 0)` ->
`self.zip_annotation(lowered, inferred, &mut mem::map(self.ctx.frame), 0)`.

`elided_groups` (4301-4353) -> `Option<FVec<'a, usize>>`; lines 4326-4339, before:

```rust
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new();
            ...
        };
        ...
        let mut distinct = Vec::new();
        let mut groups = Vec::new();
```

after:

```rust
        let frame = self.ctx.frame;
        let regions = |ty: Ty<'db>| {
            let mut found = Vec::new_in(frame);
            ...
        };
        ...
        let mut distinct = Vec::new_in(frame);
        let mut groups = Vec::new_in(frame);
```

(its caller, 1127-1129, indexes `groups[position]`: unchanged.)

`spelled_positions` (4395-4437) -> `Option<FVec<'a, (hir_def::type_ref::TypeRefId, usize)>>`;
lines 4420-4436, before:

```rust
        let written: Vec<_> = args
            .as_ref()?
            .args
            .iter()
            .filter_map(..)
            .collect();
        let positions: Vec<_> = inferred
            .iter()
            .enumerate()
            .skip(skip)
            .filter(..)
            .map(..)
            .collect();
        (written.len() == positions.len()).then(|| written.into_iter().zip(positions).collect())
```

after:

```rust
        let frame = self.ctx.frame;
        let written = mem::collect(frame, args.as_ref()?.args.iter().filter_map(..));
        let positions =
            mem::collect(frame, inferred.iter().enumerate().skip(skip).filter(..).map(..));
        (written.len() == positions.len())
            .then(|| mem::collect(frame, written.into_iter().zip(positions)))
```

Its other caller, `spelled_projections` line 4452,
`for (type_ref, index) in self.spelled_positions(expr, path).unwrap_or_default() {` ->
`for (type_ref, index) in self.spelled_positions(expr, path).into_iter().flatten() {`.

`projection_holes` (4467-4521) -> `FMap<'a, Ty<'db>, Ty<'db>>` (only `get`, `entry`,
`is_empty` asked of it: order free); lines 4472-4473 and 4504-4515, before:

```rust
        let mut holes = FxHashMap::default();
        let mut in_projection = Vec::new();
        ...
        let found: Vec<(Ty<'db>, Ty<'db>)> = infcx.probe(|_| {
            ...
                return Vec::new();
            ...
            in_projection
                .iter()
                .map(..)
                .filter(..)
                .collect()
        });
```

after:

```rust
        let frame = self.ctx.frame;
        let mut holes = mem::map(frame);
        let mut in_projection = Vec::new_in(frame);
        ...
        let found: FVec<'_, (Ty<'db>, Ty<'db>)> = infcx.probe(|_| {
            ...
                return Vec::new_in(frame);
            ...
            mem::collect(frame, in_projection.iter().map(..).filter(..))
        });
```

`zip_annotation` (4525-4639), recursive over the written type: line 4529
`holes: &mut FxHashMap<Ty<'db>, Ty<'db>>,` -> `holes: &mut FMap<'_, Ty<'db>, Ty<'db>>,`;
after line 4532 (`let interner = self.interner();`) `let frame = self.ctx.frame;`. Line 4554,
before / after (the same elements, `zip` called in the same order, stopping at the first
`None` as `collect::<Option<_>>` did):

```rust
                let tys: Option<Vec<_>> = a.iter().zip(b.iter()).map(|(a, b)| zip(a, b)).collect();
                Ty::new_tup(interner, &tys?)
```

```rust
                let mut tys = Vec::with_capacity_in(a.len(), frame);
                for (a, b) in a.iter().zip(b.iter()) {
                    tys.push(zip(a, b)?);
                }
                Ty::new_tup(interner, &tys)
```

Line 4560 `let mut args = Vec::with_capacity(a.len());` ->
`let mut args = Vec::with_capacity_in(a.len(), frame);`, and line 4572
`GenericArgs::new_from_iter(args)` -> `GenericArgs::new_from_slice(&args)`. Lines 4607-4614,
before / after:

```rust
                let tys: Option<Vec<_>> = a
                    .skip_binder()
                    .inputs_and_output
                    .iter()
                    .zip(b.skip_binder().inputs_and_output.iter())
                    .map(|(a, b)| zip(a, b))
                    .collect();
                let tys = hir_ty::next_solver::Tys::new_from_iter(tys?);
```

```rust
                let mut tys = Vec::with_capacity_in(a.skip_binder().inputs_and_output.len(), frame);
                for (a, b) in a
                    .skip_binder()
                    .inputs_and_output
                    .iter()
                    .zip(b.skip_binder().inputs_and_output.iter())
                {
                    tys.push(zip(a, b)?);
                }
                let tys = hir_ty::next_solver::Tys::new_from_slice(&tys);
```

(`Tys` is an `interned_slice!`, `hir-ty/src/next_solver/ty.rs` 1466, so it has
`new_from_slice`.)

### R7.6 `normalize_closure_args`, `binds_alike` (lines 6049-6108): per trait clause naming a closure

`normalize_closure_args`, lines 6080-6106, before:

```rust
        let args: Vec<hir_ty::next_solver::GenericArg<'db>> = trait_ref
            .args
            .iter()
            .map(|arg| match arg.as_type() { .. })
            .collect();
        rustc_type_ir::TraitRef::new(self.interner(), trait_ref.def_id, args)
```

after:

```rust
        let frame = self.ctx.frame;
        let args: FVec<'_, hir_ty::next_solver::GenericArg<'db>> =
            mem::collect(frame, trait_ref.args.iter().map(|arg| match arg.as_type() { .. }));
        rustc_type_ir::TraitRef::new(self.interner(), trait_ref.def_id, args)
```

(`TraitRef::new` takes `impl IntoIterator`, which the frame vector is.) `binds_alike`, lines
6051-6052: `let shape = |ty: Ty<'db>| { let mut bound = Vec::new();` ->
`let frame = self.ctx.frame; let shape = |ty: Ty<'db>| { let mut bound = Vec::new_in(frame);`
(`shape(a) == shape(b)` compares two `(Ty, FVec<bool>)`: `Vec<T, A>: PartialEq`).

---

## R8. The free functions and the `async fn` output

> forwarded to db (db-ty-d-borrowck) (requests, pass 19; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:59): not applied, waits on R0-R2 and B0-B1 as R3-R7 do.
> applied by db-mem-borrowck-regions (2026-10-29 22:00): R8.1-R8.8 as written, found by old text. Differences: R6.1's two `opaque_captures` callers in `universal_regions` (now 1409, 1438) and R8.4's at 3306 were still `(self.ctx.db, ..)` and take the frame here; R8.8's `closure_annotations` was already in. polonius.rs 284 still calls `opaque_captures(ctx.db, ..)`: filed in `v2/requests/db-ty-d-borrowck.md` for frontend-fast-borrowck. names.rs and check/parts.rs already work with R8.5's iterator.

### R8.1 `opaque_captures` (lines 293-464): per opaque type visited

Called from type visitors (here, `liveness`, `polonius`, `check`) for each opaque type they
meet, so per live type with an `impl Trait` in it. Three temporaries per call (the parameters,
the named set, the result) and a tuple per nested opaque. It gets the frame first; its result
is a frame vector the caller zips with the arguments as before.

| line | before | after |
|---|---|---|
| 293-297 | `pub(super) fn opaque_captures<'db>(db: &'db Db, opaque: OpaqueTyIdWrapper<'db>, len: usize) -> Option<Vec<bool>> {` | `pub(super) fn opaque_captures<'f, 'db>(frame: &'f Frame, db: &'db Db, opaque: OpaqueTyIdWrapper<'db>, len: usize) -> Option<FVec<'f, bool>> {` |
| 309 | `let params: Vec<_> = hir_ty::generics::generics(db, function.into()).iter(true).collect();` | `let params = mem::collect(frame, hir_ty::generics::generics(db, function.into()).iter(true));` |
| 336 | `let mut named = FxHashSet::default();` | `let mut named = mem::set(frame);` |
| 350 | `&& let Some(captured) = opaque_captures(db, def_id, alias.args.len()) =>` | `&& let Some(captured) = opaque_captures(frame, db, def_id, alias.args.len()) =>` |
| 352-367 | `let named: Vec<Ty<'_>> = alias.args.iter().zip(captured).filter(..).filter_map(..).collect();` | `let named: FVec<'_, Ty<'_>> = mem::collect(frame, alias.args.iter().zip(captured).filter(..).filter_map(..));` |
| 407-439 | `let captured = params.iter().map(..).chain(..).collect::<Vec<_>>();` | `let captured = mem::collect(frame, params.iter().map(..).chain(..));` |
| 453 | `return Some(vec![true; len]);` | `return Some(filled(frame, len, true));` |
| 455-463 | `Some(params.iter().map(..).chain(..).enumerate().map(..).collect())` | `Some(mem::collect(frame, params.iter().map(..).chain(..).enumerate().map(..)))` |

(`named` is asked `insert`, `contains` and `iter().all(..)`: order free. `Ty::new_tup(interner,
&named)` takes the slice.) Its callers in this file, each `opaque_captures(self.ctx.db, ..)`
-> `opaque_captures(self.ctx.frame, self.ctx.db, ..)`: lines 1358, 1387, 1535 (R6.1), 3215,
3587 (R4.3), 3744 (R4.2); and in the visitors, 604 and 684, `opaque_captures(self.db, ..)` ->
`opaque_captures(self.frame, self.db, ..)` (R8.3).

### R8.2 `hidden_types`, `regions_of`, the components (lines 541-573, 699-710, 6873-7001)

Appenders to the caller's buffer, recursive; only their buffer types change:

| line | before | after |
|---|---|---|
| 541 | `fn hidden_types<'db>(declared: Ty<'db>, actual: Ty<'db>, out: &mut Vec<Ty<'db>>) {` | `fn hidden_types<'db>(declared: Ty<'db>, actual: Ty<'db>, out: &mut FVec<'_, Ty<'db>>) {` |
| 545 | `out: &mut Vec<Ty<'db>>\| {` | `out: &mut FVec<'_, Ty<'db>>\| {` |
| 699 | `pub(super) fn regions_of(ty: Ty<'_>, out: &mut Vec<Rv>) {` | `pub(super) fn regions_of(ty: Ty<'_>, out: &mut FVec<'_, Rv>) {` |
| 700 | `struct Collect<'o>(&'o mut Vec<Rv>);` | `struct Collect<'o, 'f>(&'o mut FVec<'f, Rv>);` |
| 701 | `impl<'db> rustc_type_ir::TypeVisitor<..> for Collect<'_> {` | `impl<'db> rustc_type_ir::TypeVisitor<..> for Collect<'_, '_> {` |
| 6875-6876 | `regions: &mut Vec<Rv>, params: &mut Vec<Ty<'db>>,` | `regions: &mut FVec<'_, Rv>, params: &mut FVec<'_, Ty<'db>>,` |
| 6941 | `pub(super) fn alias_components<'db>(ty: Ty<'db>, out: &mut Vec<Ty<'db>>) {` | `pub(super) fn alias_components<'db>(ty: Ty<'db>, out: &mut FVec<'_, Ty<'db>>) {` |
| 6942 | `let args = \|args: GenericArgs<'db>, out: &mut Vec<Ty<'db>>\| {` | `let args = \|args: GenericArgs<'db>, out: &mut FVec<'_, Ty<'db>>\| {` |
| 6993 | `fn args_components<'db>(args: GenericArgs<'db>, regions: &mut Vec<Rv>, params: &mut Vec<Ty<'db>>) {` | `fn args_components<'db>(args: GenericArgs<'db>, regions: &mut FVec<'_, Rv>, params: &mut FVec<'_, Ty<'db>>) {` |

(Bodies unchanged: `push`, `extend` and recursion on `&mut FVec` are the same calls.) Their
callers outside this file (liveness.rs 604/621, explain.rs 312, check.rs 2175-2178) pass frame
vectors: B5.1 and B7.

### R8.3 `names_captured_lifetime`, `unbounded_opaque`, `member_regions` (lines 589-697)

Visitors that call `opaque_captures`, so they carry the frame:

- `names_captured_lifetime` (589): `fn names_captured_lifetime<'db>(frame: &Frame, db: &Db, ty:
  Ty<'db>) -> bool`; `struct Find<'d> { db: &'d Db, found: bool }` ->
  `struct Find<'d> { db: &'d Db, frame: &'d Frame, found: bool }` (both shared borrows shrink
  to one lifetime; the `impl` at 594 is unchanged), line 604 as R8.1, line 616
  `Find { db, found: false }` -> `Find { db, frame, found: false }`.
- `unbounded_opaque` (624): `fn unbounded_opaque<'db>(frame: &Frame, db: &Db, interner: ..,
  ty: Ty<'db>) -> bool`; line 636 `!names_captured_lifetime(db, ty) || unbounded_opaque(db,
  interner, ty)` -> `!names_captured_lifetime(frame, db, ty) || unbounded_opaque(frame, db,
  interner, ty)`.
- `member_regions` (646): `fn member_regions<'db>(frame: &Frame, db: &Db, ty: Ty<'db>, aliases:
  bool, out: &mut FVec<'_, Rv>)`; `struct Collect<'o, 'd> { db, aliases, out: &'o mut Vec<Rv> }`
  -> `struct Collect<'o, 'f, 'd> { db: &'d Db, frame: &'d Frame, aliases: bool, out: &'o mut
  FVec<'f, Rv> }` (the frame shares `'d` with `db`, not `'f`: `&'o mut FVec<'f, _>` is
  invariant in `'f`, so the frame `member_regions` is given need not be the buffer's),
  both `impl`s (657, 664) for `Collect<'_, '_, '_>`, line 684 as R8.1, line 696
  `&mut Collect { db, aliases, out }` -> `&mut Collect { db, frame, aliases, out }`.

Callers: 1587-1588 `names_captured_lifetime(self.ctx.db, ty) && !unbounded_opaque(self.ctx.db,
self.interner(), ty)` -> `names_captured_lifetime(self.ctx.frame, self.ctx.db, ty) &&
!unbounded_opaque(self.ctx.frame, self.ctx.db, self.interner(), ty)`; 3239
`names_captured_lifetime(self.ctx.db, ty)` -> `names_captured_lifetime(self.ctx.frame,
self.ctx.db, ty)`; `member_regions` at 1601, 1608 (R6.1), 3260, 3277 (R8.4).

### R8.4 `async_fn_output`, `unnamed_hidden`, `async_output_members` (lines 3085-3287): once per `async fn`

| line | before | after |
|---|---|---|
| 3128 | `let mut unsuggested = Vec::new();` | `let mut unsuggested = Vec::new_in(self.ctx.frame);` |
| 3221 | `let mut outlived = Vec::new();` | `let mut outlived = Vec::new_in(self.ctx.frame);` |
| 3248 | `let mut choices = vec![self.regions.static_rv];` | `let mut choices = Vec::new_in(self.ctx.frame); choices.push(self.regions.static_rv);` |
| 3259 | `let mut bounded = Vec::new();` | `let mut bounded = Vec::new_in(self.ctx.frame);` |
| 3260 | `member_regions(self.ctx.db, output, false, &mut bounded);` | `member_regions(self.ctx.frame, self.ctx.db, output, false, &mut bounded);` |
| 3276 | `let mut members = Vec::new();` | `let mut members = Vec::new_in(self.ctx.frame);` |
| 3277 | `member_regions(self.ctx.db, output, true, &mut members);` | `member_regions(self.ctx.frame, self.ctx.db, output, true, &mut members);` |

(3281-3282 `self.regions.members = members; self.regions.choices = choices;` move frame
vectors in.) `unnamed_hidden` (3164-3202), recursive over the output type: after line 3165
`let frame = self.ctx.frame;`; lines 3167-3174, `GenericArgs::new_from_iter(typed.iter().zip(
declared.iter()).map(..))` (heap past eight arguments, in `CollectAndApply`'s `SmallVec`) ->
`GenericArgs::new_from_slice(&mem::collect(frame, typed.iter().zip(declared.iter()).map(..)))`;
lines 3188-3192 `let tys: Vec<Ty<'db>> = tys.iter().zip(declared.iter()).map(..).collect();`
-> `let tys = mem::collect(frame, tys.iter().zip(declared.iter()).map(..));`.

### R8.5 `adt_variants` (lines 6858-6869): per ADT a place or a clone check names

`enum_variants` returns a borrow of the database (`hir-def/src/lib.rs` 316), so the variants
are read in place; nothing is allocated. Before:

```rust
pub(super) fn adt_variants(
    db: &Db,
    adt: AdtId,
) -> Vec<hir_def::VariantId> {
    match adt {
        AdtId::StructId(it) => vec![it.into()],
        AdtId::UnionId(it) => vec![it.into()],
        AdtId::EnumId(it) => {
            it.enum_variants(db).variants.values().map(|&(it, _)| it.into()).collect()
        }
    }
}
```

after:

```rust
pub(super) fn adt_variants(
    db: &Db,
    adt: AdtId,
) -> impl Iterator<Item = hir_def::VariantId> + '_ {
    let single: Option<hir_def::VariantId> = match adt {
        AdtId::StructId(it) => Some(it.into()),
        AdtId::UnionId(it) => Some(it.into()),
        AdtId::EnumId(_) => None,
    };
    let variants = match adt {
        AdtId::EnumId(it) => Some(it.enum_variants(db)),
        _ => None,
    };
    single.into_iter().chain(
        variants
            .into_iter()
            .flat_map(|it| it.variants.values().map(|&(it, _)| hir_def::VariantId::from(it))),
    )
}
```

Its two callers, in this crate (lane db's, outside this part, given here so they fit):
- `names.rs` 105-107 `super::regions::adt_variants(self.db, def.def_id()).get(variant_idx as
  usize).copied()` -> `super::regions::adt_variants(self.db, def.def_id()).nth(variant_idx as
  usize)`;
- `check/parts.rs` 267 `crate::regions::adt_variants(db, adt).into_iter().all(|variant| {` ->
  `crate::regions::adt_variants(db, adt).all(|variant| {` (B7's file; `into_iter()` on an
  iterator also compiles, so this one is only tidier).

### R8.6 `constraint_path` (lines 7005-7039): per error that blames a constraint

A breadth-first search over the constraints by `sup`, per borrow error and per universal
region error (check.rs 149, 1563, 1817, 1984). Before:

```rust
pub(super) fn constraint_path(
    regions: &Regions<'_>,
    count: usize,
    from: Rv,
    to: Rv,
) -> Vec<Outlives> {
    let constraints = &regions.outlives;
    let mut came_from: Vec<Option<usize>> = vec![None; count];
    let mut seen = vec![false; count];
    let mut queue = std::collections::VecDeque::from([from]);
    seen[from] = true;
    while let Some(rv) = queue.pop_front() {
        if rv == to {
            let mut path = Vec::new();
            let mut at = to;
            while let Some(index) = came_from[at] {
                path.push(constraints.get(index));
                at = constraints.sup(index);
            }
            path.reverse();
            return path;
        }
        let ids = if rv < count { constraints.of_sup(rv) } else { &[] };
        for &index in ids {
            let index = index as usize;
            let sub = constraints.sub(index);
            if !seen[sub] {
                seen[sub] = true;
                came_from[sub] = Some(index);
                queue.push_back(sub);
            }
        }
    }
    Vec::new()
}
```

after (each region enters the queue at most once, so a vector of `count` read from a head
index is the queue and never grows; a constraint index is a `u32` already, as `of_sup` gives
it, so the back links are 4 bytes each instead of 16; the seen flags a bit each):

```rust
pub(super) fn constraint_path<'f>(
    regions: &Regions<'f, '_>,
    count: usize,
    from: Rv,
    to: Rv,
) -> FVec<'f, Outlives> {
    const NONE: u32 = u32::MAX;
    let frame = *regions.universals.allocator();
    let constraints = &regions.outlives;
    let mut came_from = filled(frame, count, NONE);
    let mut seen = BitSet::new(frame, count);
    let mut queue: FVec<'_, Rv> = Vec::with_capacity_in(count, frame);
    queue.push(from);
    seen.insert(from);
    let mut head = 0;
    while let Some(&rv) = queue.get(head) {
        head += 1;
        if rv == to {
            let mut path = Vec::new_in(frame);
            let mut at = to;
            while came_from[at] != NONE {
                let index = came_from[at] as usize;
                path.push(constraints.get(index));
                at = constraints.sup(index);
            }
            path.reverse();
            return path;
        }
        let ids = if rv < count { constraints.of_sup(rv) } else { &[] };
        for &index in ids {
            let sub = constraints.sub(index as usize);
            if seen.insert(sub) {
                came_from[sub] = index;
                queue.push(sub);
            }
        }
    }
    Vec::new_in(frame)
}
```

The same visiting order (first in, first out), so the same path. Its callers keep `&path` for
`blame`/`blame_with`, which take `&[Outlives]`.

### R8.7 What allocates nothing

`alias_def_id`, `rv_of`, `nth_impl_trait`, `has_opaque`, `names_closure`, `ObjectDefaults`
and `Replace` (6504-6593), `static_trait`, `writes_self`, `writes_elided`, `elided_lifetimes`
(6691), `writes_object_lifetime`, `writes_static_object`, `names_through_type`,
`returned_opaque`, `path_binding`, `spells_generics`, `blame`, `blame_with`,
`normalizes_to_error`, `normalize`, `normalize_with_fresh`: unchanged.

### R8.8 Two left from R6 and R7

- `closure_annotations` (1969), once per closure body: lines 1986-1991 `let written: Vec<_> =
  params.iter().copied().zip(arg_types.iter().copied()).chain([(super::return_slot(),
  ret_type)]).collect();` -> `let written = mem::collect(self.ctx.frame,
  params.iter().copied().zip(arg_types.iter().copied()).chain([(super::return_slot(),
  ret_type)]));` (the copy ends the borrow of the body and the store before the loop's
  `&mut self` calls, as the `Vec` did).
- `rvalue_ty`'s reify cast to a `for<'a> fn` (4763-4804), per such cast: lines 4793-4800
  `BoundVarKinds::new_from_iter(bound_vars.iter().chain(std::iter::repeat_n(..., elided)))`
  (heap past eight in `CollectAndApply`) ->
  `BoundVarKinds::new_from_slice(&mem::collect(self.ctx.frame,
  bound_vars.iter().chain(std::iter::repeat_n(..., elided))))` (`BoundVarKinds` is an
  `interned_slice!` type, `hir-ty/src/next_solver/interner.rs` 122 and 486).
- `prove` line 6216, `eprintln!("prove {trait_ref:?}: {pairs:?} {obligations:?}")` under
  `crate::debugging(..)`: off unless asked for; gets `// alloc: debug output`.

How the list was checked: `rg` over regions.rs for `Vec::new()`, `vec![`, `.to_vec()`,
`.collect`, `FxHashMap`, `FxHashSet`, `::default()`, `.clone()`, `with_capacity`, `Vec<`,
`new_from_iter`, `format!`, `to_string`, `String`, `Box::new`, `VecDeque`, `eprintln!` (277
lines); each is in a before/after above or inside a function shown whole (`Regions`, `build`,
`Builder`, `impl_header_types`, `constraint_path`, `adt_variants`). `tools/v2/hot_lint.py`
does not scan `hir-ty-borrowck` today (no finding in the crate), so `rg` is the check until
it does.

---

## R9. Speed of light

What `regions::build` and the solver calls write, once the body's frame is warm (the shelves
have grown to their high-water mark on an earlier statement):

**Per outlives constraint: 25 bytes, written once.** `Constraints::push`
(`constraints.rs` 35-52) writes six columns: category 1 byte, `sup` 4, `sub` 4, location 8
(`[u32; 2]`), span 8 (`MirSpan`), `from_closure` one bit (8 bytes per 64). Nothing else is
written per constraint: no `Outlives` is kept (the columns are the record), no map entry, no
copy. The columns are frame vectors that double (B2), so each byte is copied at most once
more on average and the frame holds at most twice the final size (B5.6's
`Constraints::new(frame)` starts empty; the outgrown blocks stay in the frame until the body's
task ends). The `by_sup` index (`Csr`) adds 4 bytes per constraint plus 4 per
region, built once after the walk.

**Per region variable: 4 bytes** for `universal_index` (dense, `u32::MAX` for none, up to the
largest universal one, R1), and per universal region 16 bytes in `universals`
(`(Rv, UniversalKind)`). `known_relations` (R6) is a `BitMatrix` of universals squared bits,
`count * count / 8` bytes, closed by Warshall in `count^3 / 64` word operations; `count` is
the function's universal regions (tens), so this is per body, not per constraint.

**Per statement, per call terminator, per relate, per type test: 0 new frame bytes.** Every
buffer the walk needs (R3's `Scratch`: `rvs`, `tys`, `regions`, `clauses`, `pairs`,
`obligations`, `constraints`, `projections`) is taken from a shelf and given back, so after
the first statement that needs the most, no statement grows the frame; what they write is the
constraints above. Interned arguments (`GenericArgs`, `Tys`, `Clauses`, `BoundVarKinds`
`new_from_slice`) go to the salsa interner, once per distinct value across the build, not per
body.

**Per solver call (`prove`, `normalize_at`): the frame bytes of `made` (one entry per region
the canonical query names) and `Universes` (4 bytes per fresh universe, 8 per variable in
one)**, both tens of bytes; the answers are memos of `prove_query`/`normalize_query`, owned at
their exact size once per distinct query in the build (16 bytes per region pair, 16 per type
outlives obligation, 8 per variable in a universe), and a repeat of the query writes nothing.

**Per error: `constraint_path`'s 12 bytes per region plus a bit** (R8.6), and the error's text.

**What still touches the global heap**, each named so the budget test can allow it and no
more:
1. `ElidedLifetimes::regions(&self, errors: &[Vec<Region>])` (`hir-ty/src/lower.rs` 3948)
   takes global vectors: `universal_regions` line 1116 and `elide` line 5605, both
   `// alloc: hir-ty's ElidedLifetimes::regions takes vectors (asked of mem-db)`; once per body
   and once per call to a function with elided lifetimes in its signature.
2. hir-ty's `InferCtxt`, the next solver and canonicalization (region obligations, the
   solver's caches, the `Canonical` values): named, not changed; theirs is mem-db's lane.
3. `blaze_radix::sort`'s own scratch in `prove_query`'s `in_universe` (R5), once per distinct
   query.
4. `opaque_captures` re-derives its answer from the database on each call (generics, the
   signature, the bounds): with R8.1 that costs frame bytes, not heap, but it is per opaque
   type visited and depends only on `(opaque, len)`. Making it a salsa query (an owned
   `Box<[bool]>` returned by reference) would make it zero bytes and zero work per repeat
   call; that changes its callers in liveness, polonius and check from an owned vector to a
   slice, so it is for lane db with B5/B7, not this part.
5. The debug `eprintln!` at 6216, off unless asked for.

**The budget test.** T1 in `v2/requests/mem-db-checks.md` measures the whole checker; for
regions.rs's heavy paths to be in it, its `CLEAN` fixture should also have: a function
returning `impl Iterator<Item = &'a u32> + 'a` (opaque captures, member constraints), an
`async fn` taking a reference (R8.4), a closure whose requirements reach its creator (R4.6),
a reify of `fn(&u32) -> &u32` to `for<'a> fn(&'a u32) -> &'a u32` (R8.8), and a call to a
generic function with more than eight type arguments (no `CollectAndApply` heap). Then T1's
bound, the result's owned blocks plus the named hir-ty allocations above and nothing else,
covers regions.rs too, and a second, warm run allocates nothing.

> applied: nothing to apply (notes) (requests, pass 19).


## dedyn: crates/hir-ty-borrowck/src/regions.rs: `InferVars` goes, lowering takes the `InferCtxt`
> Being applied in the tree by dedyn-ty (dedyn, 2026-10-29 21:22, new edit rule): don't apply it again.


Part of one change across hir-ty, written in full in `v2/requests/db-ty.md` under
"## dedyn: crates/hir-ty/src/lower.rs" (section "hir-ty-borrowck/src/regions.rs"): delete
`struct InferVars` and its `impl hir_ty::TyLoweringInferVarsCtx`, and call
`.with_infer_ctxt_vars(Some(&self.ctx.infcx))` / `.with_infer_ctxt_vars(vars.then_some(&self.ctx.infcx))`
in place of the two `.with_infer_vars_behavior(..)`. Apply it with the hir-ty part, not alone.

> forwarded to db (db-ty-d-borrowck) (requests, pass 19; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:59): not applied yet, `with_infer_ctxt_vars` is not in
> `hir-ty/src/lower.rs`; applied with db-ty-next2's `lower.rs` part (see `v2/requests/db-ty.md`).
> checked by db-mem-borrowck (2026-10-29 21:45): applied by dedyn-ty with hir-ty's `lower.rs`
> (`with_infer_ctxt_vars` at `lower.rs` 448). regions.rs matches the section.

> mem-db-checks-regions (2026-10-29 21:09): fits with R0-R9 in either order. It touches lines
> 54-63 (`InferVars`), 4312/4323 and 4676/4687; R0 changes only the imports (20, 43-48), R7.5
> rewrites 4326-4339 in `elided_groups`, and nothing here touches 4660-4693. No `dyn` is added
> by R0-R9.

## R10. What regions.rs uses of the dataflow files (for frontend-fast-borrowck)

`frontend-fast-borrowck` takes `bitset`, `dataflow`, `relation`, `cfg`, `moves`, `liveness`,
`loans`, `polonius` and `lib` and keeps B5's signature table. regions.rs, today and after
R0-R9, relies on these of theirs and nothing else (`dataflow`, `relation`, `moves`, `loans`
and `polonius` not at all):

| file | what | where in regions.rs |
|---|---|---|
| `bitset.rs` | `BitMatrix::new(frame, rows, columns)`, `contains(row, bit)`, `insert(row, bit)`, `union_rows(into, from)` | `known` in `known_relations` (2590-2698, R6), `Regions::knows` (225) |
| `bitset.rs` | `BitSet::new(frame, size)`, `contains(bit) -> bool` (false past the end), `insert(bit) -> bool` (true when newly set) | `aggregate_temps`, `synthesized` (R2), `universal_in_params` (R6), `constraint_path`'s `seen` (R8.6, relies on `insert`'s `bool`) |
| `cfg.rs` | `Cfg` with `order: Vec<BlockId>`-like iterable of blocks (`for &block in &cfg.order`, 770); `Location { block, statement }` built by its fields (773, 777) and read by `Constraints::push` | `build` (712-784) |
| `liveness.rs` | `live_regions(ctx, &regions, ty)` -> B5's `FVec<'a, Rv>`; only `.is_empty()` is read | 1447 |
| `liveness.rs` | `normalize_erased(ctx, ty) -> Option<Ty>`, unchanged | 2706 |
| `lib.rs` | `Ctx` with `pub(crate) frame: &'a Frame` (B1) and the fields read today (`db`, `infcx`, `owner`, `store`, `closure_expr`, ...); `return_slot()`; `debugging(db, owner)` | throughout; 1043, 1730-1830, 2423-2655, 3915-3979; 6215 |

Outside their files: `constraints.rs` (`Constraints::new(frame)`, `push`, `get`, `sup`, `sub`,
`of_sup -> &[u32]`, `Extend<Outlives>`) and `inference.rs` (`RegionValues`, `infer`,
re-exported at 42), per B5.6. If `dedup: bits` renames `BitSet`/`BitMatrix` and their
constructors (`bits`, `matrix`, per `mem-db-checks-check` B7.13), regions.rs takes the rename
as written there; what it needs is the operations above.

> forwarded to frontend-fast-borrowck (it holds those dataflow files now; told by tmux; requests, pass 22).
> frontend-fast-borrowck (2026-10-29 22:00): kept, all of it. Checked in the tree: `bitset.rs`
> `BitMatrix<'f>::new(frame, rows, columns)`, `contains`, `insert`, `union_rows`; `BitSet<'f>::new(frame,
> size)`, `contains` (false past the end), `insert -> bool`; `cfg.rs` `Cfg<'f>.order: FVec<'f,
> BasicBlockId>`, `Location { block, statement }` with public fields; `liveness::live_regions ->
> FVec<'a, Rv>`, `normalize_erased` unchanged; `Ctx.frame`, `return_slot()`, `debugging(db, owner)`.
