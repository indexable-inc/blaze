# Requests of mem-db-checks-check: `check.rs` and `check/explain.rs` onto the frames (B7)

Part B7 of `v2/requests/mem-db-checks.md` (parent `mem-db-checks`), for lane `db` (owner of
`crates/hir-ty-borrowck/`) and `db-ty-d-borrowck`, which is editing `check.rs` for task 38 and
may apply this itself. Programs against B0-B4 there: `crate::mem` (`FVec`, `FMap`, `FSet`,
`FDeque`, `map`, `set`, `filled`, `collect`, `owned`), `BitSet<'f>` with `for_rows_of`,
`BitMatrix<'f>`, `Csr<'f, T>`, `Cfg<'f>`, `Dominators<'f>`, `IntervalSet<'f>`, `Ctx` with
`pub(crate) frame: &'a Frame`, and B4.2's signature of `check::check`.

Line numbers are of the working tree on 2026-10-29 at 21:00, which task 38 is still changing:
each change names its function and quotes the code it replaces, so it applies by the quote.
Every change keeps what the code computes and the order it computes it in; where an order
reaches the output and was hash order, B7.5 says so and gives rustc's.

Two frames (B4.2): `ctx.frame` (`'f`, the body's task) holds every temporary of the body's
check; `out_frame` (`'o`, the owner's) holds what `check` returns. Per-run temporaries never
use the global allocator. A diagnostic is drafted in the body's frame (`Draft<'f>`, B7.14) and
made owned once, when `check` hands it out: each `String` and each `Vec` of a
`BorrowckDiagnostic` in one allocation at its final length, none when empty (T1's rule).

---

## B7.0 Imports, the key type, lifetimes

`check.rs` lines 6-36, before:

```rust
use blaze_ids::radix;
use hir_def::hir::{BindingAnnotation, BindingId, ExprId, Pat, Statement};
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    ClauseKind, OutlivesClause, RegionKind,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;

use hir_ty::{
    mir::{
        AsmOperand, BinOp, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind, Operand,
        OperandKind, PlaceElem, ProjectionElem, Rvalue, StatementKind, StoredPlace, TerminatorKind,
    },
    next_solver::{GenericArgKind, Region, Ty, TyKind},
};

use super::{
    Applicability, BorrowckDiagnostic, Ctx, Mode, PartSpan,
    bitset::BitSet,
    cfg::{Cfg, Dominators, Location, index},
    intervals::IntervalSet,
    liveness,
    loans::{Activation, Bias, Depth, Loan, Loans, places_conflict},
    moves::{InitFlow, InitKind, Lookup, MoveData, Mpi, apply},
    polonius,
    regions::{self, Category, RegionValues, Regions, UniversalKind},
};
```

after (`radix`, `rustc_hash` and `smallvec` are no longer used in the file: B7.4, B7.6 say why
`sort_unstable` replaces `radix::sort`):

```rust
use hir_def::hir::{BindingAnnotation, BindingId, ExprId, Pat, Statement};
use rustc_type_ir::{
    ClauseKind, OutlivesClause, RegionKind,
    inherent::{GenericArgs as _, IntoKind},
};

use hir_ty::{
    mir::{
        AsmOperand, BasicBlockId, BinOp, BorrowKind, LocalId, MirBody, MirSpan, MutBorrowKind,
        Operand, OperandKind, PlaceElem, ProjectionElem, Rvalue, StatementKind, StoredPlace,
        TerminatorKind,
    },
    next_solver::{GenericArgKind, Region, Ty, TyKind},
};

use super::{
    Applicability, BorrowckDiagnostic, Ctx, Mode, PartSpan,
    bitset::BitSet,
    cfg::{Cfg, Dominators, Location, index},
    intervals::IntervalSet,
    liveness,
    loans::{Activation, Bias, Depth, Loan, Loans, places_conflict},
    mem::{self, FMap, FSet, FVec, Frame},
    moves::{InitFlow, InitKind, Lookup, MoveData, Mpi, apply},
    polonius,
    regions::{self, Category, RegionValues, Regions, UniversalKind},
};
```

(`Applicability` and `PartSpan` stay as task 38 has them.) After `mod parts;`, the key every
set and map of places uses, and the local index helper `explain.rs` has today, moved up so
both files share it:

```rust
/// A place's local and projection, as the checks keep it in their sets and maps.
type PlaceKey<'p> = (LocalId, &'p [PlaceElem]);

fn raw(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}
```

`explain.rs` lines 563-565 (`fn raw`) go; its `use super::Checker;` (line 19) becomes
`use super::{Checker, raw};`.

Why `PlaceKey<'p>` and not `(LocalId, Vec<PlaceElem>)`: a lookup builds the key from the
place's own projection, no copy (`PlaceRef::key`, B7.3); only an insert, which happens when an
error is reported, copies the projection into the body's frame (`Checker::kept`). A slice
hashes as the `Vec` did (length, then elements), so the sets hold what they held. A set of
`PlaceKey<'f>` is looked up with a `PlaceKey<'p>` of a shorter `'p` through covariance:
hashbrown's `HashSet<T, S, A>` is covariant in `T`, so `&FSet<'f, (PlaceKey<'f>, MirSpan)>`
coerces to `&FSet<'f, (PlaceKey<'p>, MirSpan)>` for `contains`.

`check.rs`'s `impl<'a, 'db> Checker<'a, '_, 'db>` (line 676) becomes
`impl<'a, 'f, 'o, 'db> Checker<'a, 'f, 'o, 'db>`; `explain.rs` line 80
`impl<'db> Checker<'_, '_, 'db>` becomes `impl<'f, 'db> Checker<'_, 'f, '_, 'db>` (B7.9 names
`'f`); `parts.rs` line 17 `impl<'db> Checker<'_, '_, 'db>` becomes
`impl<'db> Checker<'_, '_, '_, 'db>` (task 38's file: its only change).

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written; `use hir_ty::db::HirDatabase;` (in the tree before) stays, and B7.14 adds `Child`, `Sublevel`, `Suggestion`, `names::Text`, `std::fmt`. `parts.rs`'s impl is `impl<'f, 'db> Checker<'_, 'f, '_, 'db>`, not `<'_, '_, '_, 'db>`: B7.14's `Draft<'f>` sites there need `'f` named.

## B7.1 `check::check` (lines 41-286)

The signature, before:

```rust
pub(super) fn check<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
    requirements: &FxHashMap<ExprId, (usize, Vec<regions::Requirement<'db>>)>,
    mut_upvars: &FxHashMap<ExprId, Vec<usize>>,
    out: &mut Vec<BorrowckDiagnostic>,
    moved: &mut Vec<UseOfMoved>,
) -> (usize, Vec<regions::Requirement<'db>>, Vec<usize>) {
```

after (B4.2's, with `Ctx`'s lifetime named `'f`, which callers don't see):

```rust
pub(super) fn check<'o, 'f, 'db>(
    ctx: &Ctx<'f, 'db>,
    out_frame: &'o Frame,
    body: &MirBody<'db>,
    requirements: &FMap<'_, ExprId, (usize, FVec<'_, regions::Requirement<'db>>)>,
    mut_upvars: &FMap<'_, ExprId, FVec<'_, usize>>,
    out: &mut FVec<'o, BorrowckDiagnostic>,
    moved: &mut FVec<'o, UseOfMoved<'o>>,
) -> (usize, FVec<'o, regions::Requirement<'db>>, FVec<'o, usize>) {
```

Lines 50-61 (the moves phase), before:

```rust
            let cfg = Cfg::new(body);
            let moves = MoveData::gather(ctx, body, &cfg);
            let rustc_moves = rustc_move_indices(ctx, body, &moves);
            let upvar_sources = upvar_sources(ctx, body);
            let flow = InitFlow::compute(body, &cfg, &moves);
            let dominators = Dominators::new(&cfg);
            let capture_temps = capture_temps(body);
            let index_spans = index_spans(body);
```

after (`frame_only`, B7.10, holds the four of this file to the frame in tests; `Cfg::new`
and `Dominators::new` are B2.4's; `InitFlow::compute` gets the frame first, as B2's
constructors do: B5's to give it that parameter):

```rust
            let frame = ctx.frame;
            let cfg = Cfg::new(frame, body);
            let moves = MoveData::gather(ctx, body, &cfg);
            let rustc_moves = frame_only(|| rustc_move_indices(ctx, body, &moves));
            let upvar_sources = frame_only(|| upvar_sources(ctx, body));
            let flow = InitFlow::compute(frame, body, &cfg, &moves);
            let dominators = Dominators::new(frame, &cfg);
            let capture_temps = frame_only(|| capture_temps(frame, body));
            let index_spans = frame_only(|| index_spans(frame, body));
```

The inference phase, lines 67-93: the two `regions::infer(&regions, count, cfg.points, &live)`
calls become `regions::infer(ctx.frame, &regions, count, cfg.points, &live)` (B5's frame
parameter, as above). Lines 73-90, before:

```rust
        let chosen = member_choices(&regions, &values, count);
        let unnamed = chosen.iter().any(|(_, it)| it.is_none());
        let applied: Vec<regions::Outlives> = chosen
            .iter()
            .filter_map(|&(member, choice)| {
                ...
            })
            .collect();
```

after (made once per body with a returned `impl Trait`; `member_choices` returns an empty
frame vector at once for the others, B7.8):

```rust
        let chosen = frame_only(|| member_choices(ctx.frame, &regions, &values, count));
        let unnamed = chosen.iter().any(|(_, it)| it.is_none());
        let applied = mem::collect(
            ctx.frame,
            chosen.iter().filter_map(|&(member, choice)| {
                ... (the closure unchanged)
            }),
        );
```

The debugging block, lines 136-147: line 137-138
`let points: Vec<usize> = values.points_of(loan.region).iter().filter(|&it| it < cfg.points).collect();`
keeps its `Vec` with `// alloc: BLAZE_BORROWCK_DEBUG output only` at its end; line 146
`regions::constraint_path(&regions, count, loan.region, target)` keeps its text: per R0.1 of
`v2/requests/mem-db-checks-regions.md` it returns an `FVec<'f, Outlives>` in `regions`' own
frame (B7.11).

The `Checker`, lines 153-185, before:

```rust
    let mut checker = Checker {
        ctx,
        body,
        cfg: &cfg,
        moves: &moves,
        dominators: &dominators,
        uninit: BitSet::default(),
        ever: BitSet::default(),
        errors: Vec::new(),
        access_reported: FxHashSet::default(),
        uninit_reported: FxHashSet::default(),
        move_reported: FxHashMap::default(),
        rustc_moves,
        upvar_sources,
        move_errors: Vec::new(),
        capture_temps,
        index_spans,
        assigned_local: super::return_slot(),
        regions: &regions,
        region_count: count,
        values: &values,
        live: &live,
        loans: &loans,
        in_scope: BitSet::default(),
        dead_reported: FxHashSet::default(),
        reservation_reported: FxHashSet::default(),
        static_muts: static_muts(ctx, body),
        mut_errors: FxHashMap::default(),
        requirements: Vec::new(),
        mut_upvars,
        used_mut: FxHashSet::default(),
        used_mut_upvars: Vec::new(),
    };
```

after (B2.1: each of the three sets the walk loads rows into is sized by the matrix it loads
from, so `load` is a `copy_from_slice` of equal lengths; an empty `FVec`, `FMap` or `FSet`
allocates nothing until it is first pushed to):

```rust
    let frame = ctx.frame;
    let locals = body.locals.len();
    let mut checker = frame_only(|| Checker {
        ctx,
        out_frame,
        body,
        cfg: &cfg,
        moves: &moves,
        dominators: &dominators,
        uninit: BitSet::for_rows_of(frame, &flow.uninit_entry),
        ever: BitSet::for_rows_of(frame, &flow.ever_entry),
        errors: Vec::new_in(frame),
        access_reported: mem::set(frame),
        uninit_reported: BitSet::new(frame, locals),
        move_reported: Vec::new_in(frame),
        move_reported_at: mem::map(frame),
        rustc_moves,
        upvar_sources,
        move_errors: Vec::new_in(frame),
        capture_temps,
        index_spans,
        assigned_local: super::return_slot(),
        regions: &regions,
        region_count: count,
        values: &values,
        live: &live,
        loans: &loans,
        in_scope: BitSet::for_rows_of(frame, &borrows_entry),
        dead_reported: mem::set(frame),
        reservation_reported: mem::set(frame),
        static_muts: static_muts(ctx, body),
        mut_errors: mem::map(frame),
        requirements: Vec::new_in(out_frame),
        mut_upvars,
        used_mut: BitSet::new(frame, locals),
        used_mut_upvars: Vec::new_in(frame),
    });
```

The walk, lines 186-256, before:

```rust
    let walk = crate::timings::start();
    let (guards, guard_at) = guard_borrows(ctx, body);
    let index_borrows = index_borrows(ctx, body);
    let test_span = |span: MirSpan| match span {
        MirSpan::PatId(pat) => ctx.scan.tested_at(pat).map_or(span, MirSpan::ExprId),
        _ => span,
    };
    for &block in &cfg.order {
        ...
    }
    crate::timings::finish(Phase::Walk, walk);
```

after (the loop's body unchanged, lines 194-254):

```rust
    let walk = crate::timings::start();
    frame_only(|| {
        let (guards, guard_at) = guard_borrows(ctx, body);
        let index_borrows = index_borrows(ctx, body);
        let test_span = |span: MirSpan| match span {
            MirSpan::PatId(pat) => ctx.scan.tested_at(pat).map_or(span, MirSpan::ExprId),
            _ => span,
        };
        for &block in &cfg.order {
            ... (unchanged)
        }
    });
    crate::timings::finish(Phase::Walk, walk);
```

The reports and the output, lines 257-286, before:

```rust
    let errors = crate::timings::start();
    checker.report_move_errors();
    let before = checker.errors.len();
    checker.report_region_errors();
    if unnamed && checker.errors.len() == before {
        ... (the E0700)
    }
    let unused_mut = checker.unused_mut();
    crate::timings::finish(Phase::Errors, errors);
    out.extend(checker.errors.into_iter().flatten());
    for (_, (_, mut error)) in checker.move_reported {
        error.at = out.len();
        out.push(error.diagnostic.clone());
        moved.push(error);
    }
    out.extend(unused_mut);
    let count = regions.creator_positions.values().filter(|&&it| it < regions::IMPL_ANON).count();
    let mut used_mut_upvars = checker.used_mut_upvars;
    radix::sort(&mut used_mut_upvars);
    used_mut_upvars.dedup();
    (count, checker.requirements, used_mut_upvars)
}
```

after:

```rust
    let errors = crate::timings::start();
    let unused_mut = frame_only(|| {
        checker.report_move_errors();
        let before = checker.errors.len();
        checker.report_region_errors();
        if unnamed && checker.errors.len() == before {
            ... (the E0700, unchanged)
        }
        checker.unused_mut()
    });
    crate::timings::finish(Phase::Errors, errors);
    frame_only(|| {
        out.extend(checker.errors.iter().flatten().map(Draft::owned));
        // rustc's `buffered_move_errors` is a `BTreeMap` by the moves each error is about.
        let mut reported = checker.move_reported;
        reported.sort_unstable_by(|a, b| a.moves.cmp(b.moves));
        for MoveReported { diagnostic, mut error, .. } in reported {
            error.at = out.len();
            out.push(diagnostic.owned());
            moved.push(error);
        }
        out.extend(unused_mut.iter().map(Draft::owned));
        let count =
            regions.creator_positions.values().filter(|&&it| it < regions::IMPL_ANON).count();
        let mut used = checker.used_mut_upvars;
        used.sort_unstable();
        used.dedup();
        (count, checker.requirements, mem::collect(out_frame, used.iter().copied()))
    })
}
```

The diagnostic moves into `out` instead of being cloned: `UseOfMoved` loses its `diagnostic`
field (B7.5), which only line 278 read. `sort_unstable` for `radix::sort`: `used_mut_upvars`
holds a capture index per mutation, a few dozen at most; `blaze_radix::sort` takes a global
`Vec` of scratch past 64 items (`Radix::run`'s `&mut Vec::new()`), pdqsort sorts in place, and
on integers both give the one sorted order.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written by the quotes, with B7.14's drafts in the output section (`checker.errors` -> `Draft::owned` into `out`). The moves phase also makes `Decoded` and `liveness`/`Loans::gather` take it (frontend-fast-borrowck's request in `db-ty-d-borrowck.md`), and the `Checker`'s `uninit`/`ever`/`in_scope` are `BitSet::new(frame, <width>)` loaded from `Rows` per block, in place of `for_rows_of`/`BitMatrix::row`.

## B7.2 The `Checker` (lines 616-674) and its helper types

Every field, today's type and the new one; "per body" is one per `check` call:

| line | field | made | before | after | from |
|---|---|---|---|---|---|
| 625 | `cfg`, `moves`, `dominators` | per body | `&'a Cfg`, `&'a MoveData`, `&'a Dominators` | `&'a Cfg<'f>`, `&'a MoveData<'f>`, `&'a Dominators<'f>` | B2.4, B5 |
| 629-630 | `uninit`, `ever` | per body, a row loaded per block | `BitSet` (`default` + `load`, a `Vec` regrown per block) | `BitSet<'f>`, `for_rows_of` the flow's matrices | `ctx.frame` |
| 632 | `errors` | per body, a push per error | `Vec<Option<BorrowckDiagnostic>>` | `FVec<'f, Option<Draft<'f>>>` (B7.14) | `ctx.frame` |
| 633 | `access_reported` | per body; asked per access, added per error | `FxHashSet<((LocalId, Vec<PlaceElem>), MirSpan)>`, a `to_vec` per access | `FSet<'f, (PlaceKey<'f>, MirSpan)>` | `ctx.frame` |
| 634 | `uninit_reported` | per body; per E0381 | `FxHashSet<LocalId>` | `BitSet<'f>` of locals | `ctx.frame` |
| 636 | `move_reported` | per body; per E0382 | `FxHashMap<Vec<usize>, ((LocalId, Vec<PlaceElem>), UseOfMoved)>` | `FVec<'f, MoveReported<'f, 'o>>` + `move_reported_at: FMap<'f, &'f [usize], u32>` | `ctx.frame` |
| 638 | `rustc_moves` | per body | `Vec<Option<usize>>` | `FVec<'f, Option<usize>>` | `ctx.frame` |
| 640 | `upvar_sources` | per closure body | `FxHashMap<LocalId, StoredPlace>` (a `clone` per capture) | `FMap<'f, LocalId, PlaceRef<'a>>` | `ctx.frame` |
| 641 | `move_errors` | per body; per E0507-E0509 | `Vec<MoveError>` | `FVec<'f, MoveError<'f>>` | `ctx.frame` |
| 643, 645 | `capture_temps`, `index_spans` | per body | `FxHashMap<LocalId, MirSpan>` | `FMap<'f, LocalId, MirSpan>` | `ctx.frame` |
| 649 | `static_muts` | per body | `FxHashSet<LocalId>` | `BitSet<'f>` of locals | `ctx.frame` |
| 650-655 | `regions`, `values`, `live`, `loans` | per body | `&'a Regions<'db>`, `&'a RegionValues`, `&'a [IntervalSet]`, `&'a Loans` | `&'a Regions<'f, 'db>`, `&'a RegionValues<'f>`, `&'a [IntervalSet<'f>]`, `&'a Loans<'f>` | B5, B6 |
| 657 | `in_scope` | per body, a row loaded per block | `BitSet` (`default` + `load`) | `BitSet<'f>`, `for_rows_of` the entry sets | `ctx.frame` |
| 659 | `dead_reported` | per body; per E0597/E0713/E0596 | `FxHashSet<(LocalId, MirSpan)>` | `FSet<'f, ..>` | `ctx.frame` |
| 662 | `reservation_reported` | per body; per two-phase conflict | `FxHashSet<(LocalId, Vec<PlaceElem>)>` | `FSet<'f, PlaceKey<'f>>` | `ctx.frame` |
| 665 | `mut_errors` | per body; per E0596 | `FxHashMap<LocalId, (usize, usize)>` | `FMap<'f, ..>` | `ctx.frame` |
| 667 | `requirements` | per closure body, returned | `Vec<regions::Requirement<'db>>` | `FVec<'o, ..>` | `out_frame` |
| 669 | `mut_upvars` | the creator's, read | `&'a FxHashMap<ExprId, Vec<usize>>` | `&'a FMap<'a, ExprId, FVec<'a, usize>>` | B4.2 |
| 671 | `used_mut` | per body; per mutation | `FxHashSet<LocalId>` | `BitSet<'f>` of locals | `ctx.frame` |
| 673 | `used_mut_upvars` | per closure body; per mutation of a capture | `Vec<usize>` | `FVec<'f, usize>`, copied sorted into `out_frame` (B7.1) | `ctx.frame` |

`mut_upvars`: B4.2 passes `&FMap<'_, ExprId, FVec<'_, usize>>`; a frame map is covariant in
its allocator's lifetime, so it is stored at `'a`. The locals sets become bitsets of
`body.locals.len()` bits: each is asked per access or per mutation (`static_muts` per deref
mutated, `used_mut` per mutation), and a bit test is one load where a hash probe is a hash and
a group scan; every local of the body is below `body.locals.len()`.

Lines 616-674, before:

```rust
struct MoveError {
    place: (LocalId, Vec<PlaceElem>),
    location: Location,
    span: MirSpan,
    kind: MoveErrorKind,
}

struct Checker<'a, 'b, 'db> {
    ctx: &'a Ctx<'b, 'db>,
    body: &'a MirBody<'db>,
    cfg: &'a Cfg,
    moves: &'a MoveData,
    dominators: &'a Dominators,
    uninit: BitSet,
    ever: BitSet,
    /// In the order reported; `None` for one a later error replaced.
    errors: Vec<Option<BorrowckDiagnostic>>,
    access_reported: FxHashSet<((LocalId, Vec<PlaceElem>), MirSpan)>,
    uninit_reported: FxHashSet<LocalId>,
    /// The `E0382` reported for a set of moves, with the place it is about.
    move_reported: FxHashMap<Vec<usize>, ((LocalId, Vec<PlaceElem>), UseOfMoved)>,
    ... (to line 674)
}
```

after (doc comments kept where the field is kept):

```rust
struct MoveError<'f> {
    place: PlaceKey<'f>,
    location: Location,
    span: MirSpan,
    kind: MoveErrorKind,
}

/// An `E0382`, buffered by the moves it is about as rustc's `buffered_move_errors`.
struct MoveReported<'f, 'o> {
    /// The moves, sorted: the error's key in [`Checker::move_reported_at`].
    moves: &'f [usize],
    /// The place used, as the body has it.
    used: PlaceKey<'f>,
    diagnostic: Draft<'f>,
    error: UseOfMoved<'o>,
}

struct Checker<'a, 'f, 'o, 'db> {
    ctx: &'a Ctx<'f, 'db>,
    /// The owner's frame, for what `check` returns.
    out_frame: &'o Frame,
    body: &'a MirBody<'db>,
    cfg: &'a Cfg<'f>,
    moves: &'a MoveData<'f>,
    dominators: &'a Dominators<'f>,
    uninit: BitSet<'f>,
    ever: BitSet<'f>,
    /// In the order reported; `None` for one a later error replaced.
    errors: FVec<'f, Option<Draft<'f>>>,
    access_reported: FSet<'f, (PlaceKey<'f>, MirSpan)>,
    /// The locals an `E0381` was reported of.
    uninit_reported: BitSet<'f>,
    /// The `E0382`s, in the order first reported.
    move_reported: FVec<'f, MoveReported<'f, 'o>>,
    /// Where in `move_reported` the error of each set of moves is.
    move_reported_at: FMap<'f, &'f [usize], u32>,
    /// For each move out, its index in rustc's numbering, if rustc's MIR has it.
    rustc_moves: FVec<'f, Option<usize>>,
    /// In a closure, the place of the closure each local a capture is copied into is.
    upvar_sources: FMap<'f, LocalId, PlaceRef<'a>>,
    move_errors: FVec<'f, MoveError<'f>>,
    /// The temporaries that hold a borrow a closure captures, with the span of the closure.
    capture_temps: FMap<'f, LocalId, MirSpan>,
    /// The index expression each index temporary is evaluated for.
    index_spans: FMap<'f, LocalId, MirSpan>,
    /// The local the statement being checked assigns, if it assigns one.
    assigned_local: LocalId,
    /// The temporaries that point to a `static mut`.
    static_muts: BitSet<'f>,
    regions: &'a Regions<'f, 'db>,
    region_count: usize,
    values: &'a RegionValues<'f>,
    /// The points each region is live at by liveness alone, rustc's `liveness_constraints`.
    live: &'a [IntervalSet<'f>],
    loans: &'a Loans<'f>,
    /// The loans in scope where the checker is.
    in_scope: BitSet<'f>,
    /// The places a "does not live long enough" error was reported of, with the borrow.
    dead_reported: FSet<'f, (LocalId, MirSpan)>,
    /// The places a two-phase borrow's reservation was reported a conflict of, which its
    /// activation does not report again.
    reservation_reported: FSet<'f, PlaceKey<'f>>,
    /// For each variable declared without `mut` borrowed mutably, its error and how many
    /// borrows it is about.
    mut_errors: FMap<'f, LocalId, (usize, usize)>,
    /// In a closure, what it needs of its creator's regions.
    requirements: FVec<'o, regions::Requirement<'db>>,
    /// For each closure this body makes, the captures it needs the `mut` of.
    mut_upvars: &'a FMap<'a, ExprId, FVec<'a, usize>>,
    /// The variables a mutation needs the `mut` of, rustc's `used_mut`.
    used_mut: BitSet<'f>,
    /// In a closure, the captures a mutation needs the `mut` of.
    used_mut_upvars: FVec<'f, usize>,
}
```

A new method, first in the `impl` (line 676), for the one place a key outlives the place it
was made from:

```rust
    /// `place` as a key the checker keeps: its projection copied into the body's frame.
    fn kept(&self, place: PlaceRef<'_>) -> PlaceKey<'f> {
        let projection: &'f [PlaceElem] = self.ctx.frame.alloc_slice_copy(place.projection);
        (place.local, projection)
    }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written, with `errors: FVec<'f, Option<Draft<'f>>>` and `MoveError.diagnostic: Draft<'f>` per B7.14.

## B7.3 The per-access and per-statement paths: no allocation at all

These run on every body, error or not. After the change none of them allocates, from the frame
or the global allocator, on a body with no error.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): the subsections below, each in the tree.

### `PlaceRef::key` (lines 521-523)

Before (a global `Vec` per access to a place with a projection, from `access_place_only`):

```rust
    fn key(self) -> (LocalId, Vec<PlaceElem>) {
        (self.local, self.projection.to_vec())
    }
```

after:

```rust
    fn key(self) -> PlaceKey<'p> {
        (self.local, self.projection)
    }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:709 (`fn key(self) -> PlaceKey<'p>`).

### `access_place_only` (lines ~1246-1264), per access

Before:

```rust
        let key = (place.key(), span);
        if self.access_reported.contains(&key) {
            return;
        }
        let mutability_error = self.check_access_permissions(place, span, access, allowed);
        let conflict_error = self.check_access_for_conflict(location, place, span, access, depth);
        if mutability_error || conflict_error {
            self.access_reported.insert(key);
        }
```

after (on a body with no error the set is empty, and hashbrown's `contains` returns before
hashing on an empty table):

```rust
        if self.access_reported.contains(&(place.key(), span)) {
            return;
        }
        let mutability_error = self.check_access_permissions(place, span, access, allowed);
        let conflict_error = self.check_access_for_conflict(location, place, span, access, depth);
        if mutability_error || conflict_error {
            let key = (self.kept(place), span);
            self.access_reported.insert(key);
        }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:915 (`(self.kept(place), span)` only on an error).

### `check_access_for_conflict` (lines ~1268-1384), per access

Before:

```rust
        let loans = self.loans;
        let in_scope: SmallVec<[usize; 4]> = loans
            .of_local(place.local)
            .iter()
            .map(|&id| id as usize)
            .filter(|&id| self.in_scope.contains(id))
            .collect();
        for id in in_scope {
            let loan = &loans.loans[id];
```

after (no copy of the loans: `in_scope` is changed only by the walk's `load`, `before` and
`after`, never while an access is checked, so testing it as the loop goes gives the loans the
copy had, in the same order):

```rust
        let loans = self.loans;
        for &id in loans.of_local(place.local) {
            let id = id as usize;
            if !self.in_scope.contains(id) {
                continue;
            }
            let loan = &loans.loans[id];
```

In the same function, line ~1303
`(Access::Activation(_), _) if self.reservation_reported.contains(&place.key()) => {` is
unchanged text (now a borrowed key), and line ~1334, before:

```rust
                    if two_phase {
                        self.reservation_reported.insert(place.key());
                    }
```

after:

```rust
                    if two_phase {
                        let key = self.kept(place);
                        self.reservation_reported.insert(key);
                    }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:1513 (`for &id in loans.of_local(..)`) and crates/hir-ty-borrowck/src/check.rs:1566 (`self.kept(place)`).

### `check_for_invalidation_at_unwind` (line ~1009), per terminator that may unwind, and `check_for_invalidation_at_exit` (line ~1419), per `Return`

Before (a global `Vec` of every loan in scope at every call, drop, assert and loop back edge
with a loan in scope: the most frequent allocation of the walk):

```rust
    fn check_for_invalidation_at_unwind(&mut self, location: Location, span: MirSpan) {
        let in_scope: Vec<usize> = self.in_scope.iter().collect();
        let loans = self.loans;
        for id in in_scope {
            ...
        }
    }
```

```rust
    fn check_for_invalidation_at_exit(&mut self, location: Location, span: MirSpan) {
        let in_scope: Vec<usize> = self.in_scope.iter().collect();
        for id in in_scope {
            if !self.loans.projection(id).contains(&ProjectionElem::Deref) {
                self.report_does_not_live_long_enough(id, location, span, None);
            }
        }
    }
```

after (the set is taken out while it is walked and put back; `BitSet<'f>` is `Default`, an
empty slice, B2.1):

```rust
    fn check_for_invalidation_at_unwind(&mut self, location: Location, span: MirSpan) {
        // The reports read no `in_scope`.
        let in_scope = std::mem::take(&mut self.in_scope);
        let loans = self.loans;
        for id in in_scope.iter() {
            ... (the loop's body unchanged)
        }
        self.in_scope = in_scope;
    }
```

```rust
    fn check_for_invalidation_at_exit(&mut self, location: Location, span: MirSpan) {
        // The reports read no `in_scope`.
        let in_scope = std::mem::take(&mut self.in_scope);
        for id in in_scope.iter() {
            if !self.loans.projection(id).contains(&ProjectionElem::Deref) {
                self.report_does_not_live_long_enough(id, location, span, None);
            }
        }
        self.in_scope = in_scope;
    }
```

(Neither `check.rs`, `explain.rs` nor `parts.rs` reads `self.in_scope` outside the walk: the
loop sees the set the collected `Vec` had.)

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:1243 and crates/hir-ty-borrowck/src/check.rs:1654 (`std::mem::take(&mut self.in_scope)`).

### `mutate_place` (line ~1188)

`&& !self.access_reported.contains(&(place.key(), span))` is unchanged text, now borrowed.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree: `access_reported` is an `FSet<(PlaceKey, MirSpan)>` (crates/hir-ty-borrowck/src/check.rs:844), so the `contains` borrows.

### `guard_conflict` (lines 680-728), per statement of a `match` guard

Before, the description of the place, a `String` or two from `ctx.describe` and `format!`, is
made for every statement whose span is in a guard, before knowing there is an error:

```rust
    fn guard_conflict(&mut self, guard: &Guard, place: &StoredPlace, assign: bool, span: MirSpan) {
        let projection = place.projection.as_slice();
        let local = place.local;
        let what = self.quoted(PlaceRef { local, projection }, "value");
        if guard.bindings.contains(&local) {
            // The guard sees the variable through a shared reference, so assigning it is not
            // the reassignment of an immutable variable (E0384) but a mutability error.
            if assign {
                self.access_reported.insert((PlaceRef { local, projection }.key(), span));
                self.report(
```

after (the description made in each branch that reports; the rest of each branch unchanged):

```rust
    fn guard_conflict(
        &mut self,
        guard: &Guard<'_, '_>,
        place: &StoredPlace,
        assign: bool,
        span: MirSpan,
    ) {
        let projection = place.projection.as_slice();
        let local = place.local;
        let place = PlaceRef { local, projection };
        if guard.bindings.contains(&local) {
            let what = self.quoted(place, "value");
            // The guard sees the variable through a shared reference, so assigning it is not
            // the reassignment of an immutable variable (E0384) but a mutability error.
            if assign {
                let key = (self.kept(place), span);
                self.access_reported.insert(key);
                self.report(
                ... (unchanged)
        } else if guard.tested.iter().any(|(it, fake)| *it == local && fake.starts_with(projection))
        {
            let what = self.quoted(place, "value");
            let message = match assign {
                ... (unchanged)
        } else if guard.bound.iter().any(|(it, bound)| {
            *it == local && (bound.starts_with(projection) || projection.starts_with(bound))
        }) {
            let what = self.quoted(place, "value");
            match assign {
                ... (unchanged)
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:900 (`Guard<'_, '_>`, `what` made in each reporting branch).

### `index_conflict` (line ~742)

`protected: &[(LocalId, Vec<PlaceElem>)],` becomes `protected: &[PlaceKey<'_>],`; the body is
unchanged (`fake.starts_with(projection)` on a `&&[PlaceElem]` derefs as on a `&Vec`). The
walk passes `index_borrows.get(&it.span)`'s `&FVec<'_, PlaceKey<'_>>`, which derefs to it.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:967 (`protected: &[PlaceKey<'_>]`).

### `propagate_used_mut_upvars` (lines ~2232-2275), per closure the body makes

Before (a global `Vec` of every operand index when the closure was not checked):

```rust
        // A closure lowering does not support is not checked, and may mutate any of them.
        let every: Vec<usize>;
        let fields = match self.mut_upvars.get(&expr) {
            Some(it) => it.as_slice(),
            None => {
                every = (0..operands.len()).collect();
                &every
            }
        };
        for &field in fields {
```

after:

```rust
        // A closure lowering does not support is not checked, and may mutate any of them.
        let fields = self.mut_upvars.get(&expr).map(|it| it.as_slice());
        let count = fields.map_or(operands.len(), <[usize]>::len);
        for at in 0..count {
            let field = fields.map_or(at, |it| it[at]);
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:2484 (`let count = fields.map_or(..)`).

### The locals bitsets at their uses

- `is_mutable` (line ~2449): `self.static_muts.contains(&base.local)` ->
  `self.static_muts.contains(raw(base.local))`.
- `propagate_used_mut_place` (line ~2320): `self.used_mut.insert(place.local);` ->
  `self.used_mut.insert(raw(place.local));`.
- `add_used_mut` (line ~2422): `self.used_mut.insert(local);` ->
  `self.used_mut.insert(raw(local));`.
- `report_use_of_moved_or_uninitialized` (line ~2855):
  `!self.uninit_reported.insert(used.local)` -> `!self.uninit_reported.insert(raw(used.local))`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:2555, 2658, 2685, 3107 (`raw(..)` at each).

### `check_movable_place` (lines ~2766, ~2805), per move out of a place it may not move from

Both `self.move_errors.push(MoveError { place: place.key(), location, span, kind });` become:

```rust
                let place = self.kept(place);
                self.move_errors.push(MoveError { place, location, span, kind });
```

(only when the move is an error: the moved type is not `Copy`).

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:3052 and 3083 (`let place = self.kept(place)`).

### `is_predecessor_of` (lines ~2777-2791) and its two callers

Before (a global `Vec<bool>` of blocks and a stack per call; `explain.rs`'s `loop_head` calls
it once per loop head of a region, `check_parent_of_field` once per move of a union's path,
which runs on bodies with no error: a field written of a union not yet initialized):

```rust
    fn is_predecessor_of(&self, from: Location, to: Location) -> bool {
        if from.block == to.block && from.statement < to.statement {
            return true;
        }
        let mut seen = vec![false; self.body.basic_blocks.len()];
        let mut stack: Vec<_> = self.cfg.successors(from.block).to_vec();
        while let Some(block) = stack.pop() {
            if block == to.block {
                return true;
            }
            if std::mem::replace(&mut seen[index(block)], true) {
                continue;
            }
            stack.extend(self.cfg.successors(block).iter().copied());
        }
        false
    }
```

after (one pair of buffers per caller, reused across its calls):

```rust
    /// Whether control can go from `from` to `to`, in the caller's `seen` and `stack`, which
    /// it clears.
    fn reaches(
        &self,
        from: Location,
        to: Location,
        seen: &mut BitSet<'_>,
        stack: &mut FVec<'_, BasicBlockId>,
    ) -> bool {
        if from.block == to.block && from.statement < to.statement {
            return true;
        }
        seen.clear();
        stack.clear();
        stack.extend_from_slice(self.cfg.successors(from.block));
        while let Some(block) = stack.pop() {
            if block == to.block {
                return true;
            }
            if !seen.insert(index(block)) {
                continue;
            }
            stack.extend_from_slice(self.cfg.successors(block));
        }
        false
    }

    /// Buffers for [`Checker::reaches`].
    fn reach_buffers(&self) -> (BitSet<'f>, FVec<'f, BasicBlockId>) {
        let frame = self.ctx.frame;
        (BitSet::new(frame, self.body.basic_blocks.len()), Vec::new_in(frame))
    }
```

`check_parent_of_field` (lines ~2752-2762), before:

```rust
        let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
        if let TyKind::Adt(def, _) = ty.kind()
            && def.is_union()
            && self
                .moves
                .moves_of(path)
                .iter()
                .any(|&it| self.is_predecessor_of(self.moves.moves[it as usize].location, location))
        {
            return;
        }
```

after:

```rust
        let ty = self.ctx.place_ty(self.body, base.local, base.projection).ty;
        if let TyKind::Adt(def, _) = ty.kind()
            && def.is_union()
        {
            let (mut seen, mut stack) = self.reach_buffers();
            let moves = self.moves;
            if moves.moves_of(path).iter().any(|&it| {
                self.reaches(moves.moves[it as usize].location, location, &mut seen, &mut stack)
            }) {
                return;
            }
        }
```

`explain.rs`'s `loop_head`: B7.9.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:3012 (`reaches`), 3038 (`reach_buffers`); `is_predecessor_of` is gone.

### What stays as it is, and why

- The walk's loop itself (B7.1), `check_activations`, `bounds_check_reads`, `consume_*`,
  `statement`, `terminator`, `is_union_literal`, `is_disaligned`, `destructor_ty`,
  `temporary_source`, `upvar_field_projection`, `is_upvar_mutable`, `check_if_*_moved`,
  `move_path_*`: no allocation of their own. Their callees outside these files:
  `Loans::{before, after, of_local, activated_at, projection}`, `places_conflict`, `apply`,
  `MoveData::{find, first_in, children}`, `Ctx::{place_ty, project, is_copy, moved_place,
  drop_glue}` (`is_copy` and `place_ty` go to the trait solver: mem-db-solver's and
  mem-db-infer's frame work makes those take from `frame::current`, the body's frame here).
- `field_with_regions` (line ~752) runs `fold_regions` as a visitor, per compound assignment
  through a field: no allocation of this part's; whether the identity fold re-interns a type
  is the interner's (a `TypeVisitor` that stops at the first region would do less work, and
  is a speed change, not a memory one).

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): every subsection as written; the report sites in them build `Draft`s per B7.14.

## B7.4 The free functions of the setup, per body

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): the subsections below, each in the tree.

### `rustc_move_indices` (lines ~395-458)

Before:

```rust
fn rustc_move_indices(
    ctx: &Ctx<'_, '_>,
    body: &MirBody<'_>,
    moves: &MoveData,
) -> Vec<Option<usize>> {
    let upvar_locals: FxHashSet<LocalId> = ctx.upvars.iter().map(|it| it.local).collect();
    let variables: FxHashSet<LocalId> = body.binding_locals.values().copied().collect();
    // For each local, how often it is assigned and whether every assignment moves a place
    // into the whole of it.
    let mut assigned: FxHashMap<LocalId, (usize, bool)> = FxHashMap::default();
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, rvalue) = &statement.kind {
                let entry = assigned.entry(place.local).or_insert((0, true));
                entry.0 += 1;
                entry.1 &= place.projection.as_slice().is_empty()
                    && matches!(rvalue, Rvalue::Use(it) if matches!(it.kind, OperandKind::Move(_)));
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
        {
            assigned.entry(destination.local).or_insert((0, false)).1 = false;
        }
    }
    let lowering_temp = |local: LocalId| {
        !variables.contains(&local)
            && !body.param_locals.contains(&local)
            && !upvar_locals.contains(&local)
            && assigned.get(&local).is_some_and(|&(count, moved)| count == 1 && moved)
    };
    let mut order: Vec<usize> = (0..moves.moves.len())
        .filter(|&it| {
            ...
        })
        .collect();
    order.sort_by_key(|&it| {
        let location = moves.moves[it].location;
        (index(location.block), location.statement, it)
    });
    let mut out = vec![None; moves.moves.len()];
    for (rustc, it) in order.into_iter().enumerate() {
        out[it] = Some(rustc);
    }
    out
}
```

after (two bitsets and a dense column of locals for the two sets and the map; every local is
below `body.locals.len()`. A dense `(0, true)` start reads as the map's absent entry: absent,
`is_some_and(count == 1 && moved)` is false, and `(0, true)` is not `(1, true)`; each
statement and call then does to the entry what it did to the map's. `sort_unstable_by_key`:
the key ends in `it`, unique, so the order is the stable sort's, and pdqsort needs no buffer
where the stable sort takes one from the global allocator past a few hundred moves):

```rust
fn rustc_move_indices<'f>(
    ctx: &Ctx<'f, '_>,
    body: &MirBody<'_>,
    moves: &MoveData<'_>,
) -> FVec<'f, Option<usize>> {
    let frame = ctx.frame;
    let locals = body.locals.len();
    let mut upvar_locals = BitSet::new(frame, locals);
    for upvar in ctx.upvars.iter() {
        upvar_locals.insert(raw(upvar.local));
    }
    let mut variables = BitSet::new(frame, locals);
    for &local in body.binding_locals.values() {
        variables.insert(raw(local));
    }
    // For each local, how often it is assigned and whether every assignment moves a place
    // into the whole of it.
    let mut assigned = mem::filled(frame, locals, (0usize, true));
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, rvalue) = &statement.kind {
                let entry = &mut assigned[raw(place.local)];
                entry.0 += 1;
                entry.1 &= place.projection.as_slice().is_empty()
                    && matches!(rvalue, Rvalue::Use(it) if matches!(it.kind, OperandKind::Move(_)));
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
        {
            assigned[raw(destination.local)].1 = false;
        }
    }
    let lowering_temp = |local: LocalId| {
        !variables.contains(raw(local))
            && !body.param_locals.contains(&local)
            && !upvar_locals.contains(raw(local))
            && assigned[raw(local)] == (1, true)
    };
    let mut order = mem::collect(
        frame,
        (0..moves.moves.len()).filter(|&it| {
            ... (the closure unchanged but for its three `upvar_locals.contains(&local)`,
                 `upvar_locals.contains(&dest.local)`: `upvar_locals.contains(raw(local))`,
                 `upvar_locals.contains(raw(dest.local))`)
        }),
    );
    order.sort_unstable_by_key(|&it| {
        let location = moves.moves[it].location;
        (index(location.block), location.statement, it)
    });
    let mut out = mem::filled(frame, moves.moves.len(), None);
    for (rustc, &it) in order.iter().enumerate() {
        out[it] = Some(rustc);
    }
    out
}
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:570 (`-> FVec<'f, Option<usize>>`, frame bitsets).

### `upvar_sources` (lines ~462-485)

Before: `fn upvar_sources(ctx: &Ctx<'_, '_>, body: &MirBody<'_>) -> FxHashMap<LocalId, StoredPlace>`,
`let mut out = FxHashMap::default();`, `out.entry(dest.local).or_insert_with(|| source.clone());`.
After (the place borrowed from the body, no clone of its interned projection):

```rust
fn upvar_sources<'f, 'b>(
    ctx: &Ctx<'f, '_>,
    body: &'b MirBody<'_>,
) -> FMap<'f, LocalId, PlaceRef<'b>> {
    let mut out = mem::map(ctx.frame);
    ... (unchanged to)
            if source.local == closure {
                out.entry(dest.local).or_insert(PlaceRef::of(source));
            }
    ...
}
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:647 (`-> FMap<'f, LocalId, PlaceRef<'b>>`).

### `capture_temps` (lines ~3363-3385), `index_spans` (lines ~3353-3361), `static_muts` (lines ~3411-3428)

```rust
fn index_spans<'f>(frame: &'f Frame, body: &MirBody<'_>) -> FMap<'f, LocalId, MirSpan> {
    let statements = body.basic_blocks.iter().flat_map(|(_, block)| &block.statements);
    let mut out = mem::map(frame);
    out.extend(statements.filter_map(|statement| match statement.kind {
        StatementKind::StorageLive(local) => Some((local, statement.span)),
        _ => None,
    }));
    out
}

fn capture_temps<'f>(frame: &'f Frame, body: &MirBody<'_>) -> FMap<'f, LocalId, MirSpan> {
    let mut variables = BitSet::new(frame, body.locals.len());
    for &local in body.binding_locals.values() {
        variables.insert(raw(local));
    }
    let mut temps = mem::map(frame);
    ... (unchanged, with `!variables.contains(&place.local)` ->
         `!variables.contains(raw(place.local))`)
    temps
}

fn static_muts<'f>(ctx: &Ctx<'f, '_>, body: &MirBody<'_>) -> BitSet<'f> {
    let mut locals = BitSet::new(ctx.frame, body.locals.len());
    ... (unchanged, with `locals.insert(place.local);` -> `locals.insert(raw(place.local));`)
    locals
}
```

(`extend` on a hashbrown map keeps the last value of a repeated key, as `collect` into the
`FxHashMap` did.)

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:3631, 3641, 3692 (frame maps, `static_muts -> BitSet<'f>`).

### `guard_borrows` and `Guard` (lines ~3108-3232)

The projections a guard keeps are the body's own places' projections, so they are borrowed,
not copied: `Guard<'f, 'b>` holds `PlaceKey<'b>` with `'b` the body's borrow. The tested
places of each `match` are kept once and shared by its arms (`Vec::leak` of a frame vector is
a frame slice, freed with the frame), where each arm cloned them.

Before:

```rust
#[derive(Default)]
struct Guard {
    tested: Vec<(LocalId, Vec<PlaceElem>)>,
    bound: Vec<(LocalId, Vec<PlaceElem>)>,
    bindings: Vec<LocalId>,
}

fn guard_borrows(
    ctx: &Ctx<'_, '_>,
    body: &MirBody<'_>,
) -> (Vec<Guard>, FxHashMap<MirSpan, u32>) {
    use hir_def::hir::Expr;
    let crate::scan::GuardedMatches { matches, tested_by, bound_by, guards: guarded } =
        &ctx.scan.guarded;
    let mut out = FxHashMap::default();
    if *matches == 0 {
        return (Vec::new(), out);
    }
    // For each `match`, the places it tests; for each guarded arm, what it binds by value and
    // the locals of its variables.
    let mut tested_of: Vec<Vec<(LocalId, Vec<PlaceElem>)>> =
        (0..*matches).map(|_| Vec::new()).collect();
    let mut arms_of: Vec<(Vec<(LocalId, Vec<PlaceElem>)>, Vec<LocalId>)> =
        (0..guarded.len()).map(|_| (Vec::new(), Vec::new())).collect();
    let (mut guards, mut stack, mut pats) = (Vec::new(), Vec::new(), Vec::new());
    let add = |set: &mut Vec<(LocalId, Vec<PlaceElem>)>, local, projection: &[PlaceElem]| {
        if !set.iter().any(|(it, other)| *it == local && other == projection) {
            set.push((local, projection.to_vec()));
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        ...
            let Some(&index) = tested_by.get(&pat) else { continue };
            let tested: Vec<&StoredPlace> = match rvalue {
                Rvalue::Discriminant(place) | Rvalue::Len(place) => vec![place],
                Rvalue::CheckedBinaryOp(_, a, b) => [a, b]
                    .into_iter()
                    .filter_map(|operand| match &operand.kind {
                        OperandKind::Copy(place) => Some(place),
                        _ => None,
                    })
                    .collect(),
                _ => continue,
            };
            let set = &mut tested_of[index];
            for place in tested {
                ...
    }
    for (arm, &(index, guard)) in guarded.iter().enumerate() {
        let tested = &tested_of[index];
        let (bound, bindings) = std::mem::take(&mut arms_of[arm]);
        if tested.is_empty() && bound.is_empty() && bindings.is_empty() {
            continue;
        }
        let it = guards.len() as u32;
        guards.push(Guard { tested: tested.clone(), bound, bindings });
        ...
    }
    (guards, out)
}
```

after (the loops' other lines unchanged):

```rust
struct Guard<'f, 'b> {
    tested: &'f [PlaceKey<'b>],
    bound: FVec<'f, PlaceKey<'b>>,
    bindings: FVec<'f, LocalId>,
}

fn guard_borrows<'f, 'b>(
    ctx: &Ctx<'f, '_>,
    body: &'b MirBody<'_>,
) -> (FVec<'f, Guard<'f, 'b>>, FMap<'f, MirSpan, u32>) {
    use hir_def::hir::Expr;
    fn copied(operand: &Operand) -> Option<&StoredPlace> {
        match &operand.kind {
            OperandKind::Copy(place) => Some(place),
            _ => None,
        }
    }
    let crate::scan::GuardedMatches { matches, tested_by, bound_by, guards: guarded } =
        &ctx.scan.guarded;
    let frame = ctx.frame;
    let mut out = mem::map(frame);
    if *matches == 0 {
        return (Vec::new_in(frame), out);
    }
    // For each `match`, the places it tests; for each guarded arm, what it binds by value and
    // the locals of its variables.
    let mut tested_of: FVec<'_, FVec<'f, PlaceKey<'b>>> =
        mem::collect(frame, (0..*matches).map(|_| Vec::new_in(frame)));
    let mut arms_of: FVec<'_, (FVec<'f, PlaceKey<'b>>, FVec<'f, LocalId>)> =
        mem::collect(frame, (0..guarded.len()).map(|_| (Vec::new_in(frame), Vec::new_in(frame))));
    let (mut guards, mut stack, mut pats) =
        (Vec::new_in(frame), Vec::new_in(frame), Vec::new_in(frame));
    let add = |set: &mut FVec<'f, PlaceKey<'b>>, local, projection: &'b [PlaceElem]| {
        if !set.iter().any(|&(it, other)| it == local && other == projection) {
            set.push((local, projection));
        }
    };
    for (_, block) in body.basic_blocks.iter() {
        ...
            let Some(&index) = tested_by.get(&pat) else { continue };
            let tested: [Option<&StoredPlace>; 2] = match rvalue {
                Rvalue::Discriminant(place) | Rvalue::Len(place) => [Some(place), None],
                Rvalue::CheckedBinaryOp(_, a, b) => [copied(a), copied(b)],
                _ => continue,
            };
            let set = &mut tested_of[index];
            for place in tested.into_iter().flatten() {
                ...
    }
    let tested_of = mem::collect(frame, tested_of.into_iter().map(Vec::leak));
    for (arm, &(index, guard)) in guarded.iter().enumerate() {
        let tested: &'f [PlaceKey<'b>] = tested_of[index];
        let (bound, bindings) =
            std::mem::replace(&mut arms_of[arm], (Vec::new_in(frame), Vec::new_in(frame)));
        if tested.is_empty() && bound.is_empty() && bindings.is_empty() {
            continue;
        }
        let it = guards.len() as u32;
        guards.push(Guard { tested, bound, bindings });
        ...
    }
    (guards, out)
}
```

(`[Some(place), None]` and `[copied(a), copied(b)]` flattened visit what `vec![place]` and the
filtered `[a, b]` did, in that order. `Guard::default()` is used nowhere.)

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:3393 (`(FVec<'f, Guard<'f, 'b>>, FMap<'f, MirSpan, u32>)`).

### `index_borrows` (lines ~3238-3348)

Before (per place of the body with an index: a global `Vec` of its indexes; per index with a
slice before it, a `fake` `Vec` and a `to_vec` per `Deref`; the maps `FxHashMap`s):

```rust
fn index_borrows<'db>(
    ctx: &Ctx<'_, 'db>,
    body: &MirBody<'db>,
) -> FxHashMap<MirSpan, Vec<(LocalId, Vec<PlaceElem>)>> {
    ...
    let mut evaluated_in = FxHashMap::default();
    let mut places: Vec<&StoredPlace> = Vec::new();
    ...
    let mut protected: FxHashMap<ExprId, Vec<(LocalId, Vec<PlaceElem>)>> = FxHashMap::default();
    let mut seen = FxHashSet::default();
    for place in places {
        let projection = place.projection.as_slice();
        let indexes: Vec<(usize, LocalId)> = projection
            .iter()
            .enumerate()
            .filter_map(|(at, elem)| match elem {
                ProjectionElem::Index(local) => Some((at, *local)),
                _ => None,
            })
            .collect();
        if indexes.len() < 2 || !seen.insert((place.local, projection)) {
            continue;
        }
        for (n, &(at, _)) in indexes[..indexes.len() - 1].iter().enumerate() {
            if !is_slice(place.local, &projection[..at]) {
                continue;
            }
            let mut fake = Vec::new();
            for len in (0..at).rev() {
                match &projection[len] {
                    ProjectionElem::Deref => fake.push((place.local, projection[..len].to_vec())),
                    ProjectionElem::Index(_) if is_slice(place.local, &projection[..len]) => break,
                    _ => (),
                }
            }
            for &(_, local) in &indexes[n + 1..] {
                if let Some(&expr) = evaluated_in.get(&local)
                    && let Expr::Index { index, .. } = ctx.store.expr(expr)
                {
                    let set = protected.entry(index).or_default();
                    for it in &fake {
                        if !set.contains(it) {
                            set.push(it.clone());
                        }
                    }
                }
            }
        }
    }
    let mut out: FxHashMap<MirSpan, Vec<(LocalId, Vec<PlaceElem>)>> = FxHashMap::default();
    for (index, fake) in protected {
        let mut stack = vec![index];
        while let Some(expr) = stack.pop() {
            let set = out.entry(MirSpan::ExprId(expr)).or_default();
            for it in &fake {
                if !set.contains(it) {
                    set.push(it.clone());
                }
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
    }
    out
}
```

after (`indexes`, `fake` and `stack` are one buffer each, cleared per use; the projections are
the body's, borrowed):

```rust
fn index_borrows<'f, 'b, 'db>(
    ctx: &Ctx<'f, 'db>,
    body: &'b MirBody<'db>,
) -> FMap<'f, MirSpan, FVec<'f, PlaceKey<'b>>> {
    ... (`fn operand` unchanged)
    let frame = ctx.frame;
    let mut evaluated_in = mem::map(frame);
    let mut places: FVec<'_, &'b StoredPlace> = Vec::new_in(frame);
    ... (the walk over the blocks unchanged)
    let is_slice = ... (unchanged);
    let mut protected: FMap<'_, ExprId, FVec<'f, PlaceKey<'b>>> = mem::map(frame);
    let mut seen = mem::set(frame);
    let mut indexes: FVec<'_, (usize, LocalId)> = Vec::new_in(frame);
    let mut fake: FVec<'_, PlaceKey<'b>> = Vec::new_in(frame);
    for place in places {
        let projection = place.projection.as_slice();
        indexes.clear();
        indexes.extend(projection.iter().enumerate().filter_map(|(at, elem)| match elem {
            ProjectionElem::Index(local) => Some((at, *local)),
            _ => None,
        }));
        if indexes.len() < 2 || !seen.insert((place.local, projection)) {
            continue;
        }
        for (n, &(at, _)) in indexes[..indexes.len() - 1].iter().enumerate() {
            if !is_slice(place.local, &projection[..at]) {
                continue;
            }
            fake.clear();
            for len in (0..at).rev() {
                match &projection[len] {
                    ProjectionElem::Deref => fake.push((place.local, &projection[..len])),
                    ProjectionElem::Index(_) if is_slice(place.local, &projection[..len]) => break,
                    _ => (),
                }
            }
            for &(_, local) in &indexes[n + 1..] {
                if let Some(&expr) = evaluated_in.get(&local)
                    && let Expr::Index { index, .. } = ctx.store.expr(expr)
                {
                    let set = protected.entry(index).or_insert_with(|| Vec::new_in(frame));
                    for it in fake.iter() {
                        if !set.contains(it) {
                            set.push(*it);
                        }
                    }
                }
            }
        }
    }
    let mut out: FMap<'f, MirSpan, FVec<'f, PlaceKey<'b>>> = mem::map(frame);
    let mut stack: FVec<'_, ExprId> = Vec::new_in(frame);
    for (&index, fake) in protected.iter() {
        stack.push(index);
        while let Some(expr) = stack.pop() {
            let set = out.entry(MirSpan::ExprId(expr)).or_insert_with(|| Vec::new_in(frame));
            for it in fake.iter() {
                if !set.contains(it) {
                    set.push(*it);
                }
            }
            ctx.store.walk_child_exprs(expr, |it| stack.push(it));
        }
    }
    out
}
```

`protected` is iterated in hash order today and after, which orders the places within each of
`out`'s lists; those lists are only asked `any` (`index_conflict`), so no order reaches the
output. `(LocalId, &[PlaceElem])` compares by contents, as the `Vec`s did.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written except `guard_borrows`: each `match`'s tested places are frozen with `mem::collect(frame, tested_of.into_iter().map(|it| &*it.leak()))` so `Guard.tested` is `&'f [PlaceKey<'b>]` (the quoted `map(Vec::leak)` gives `&mut` slices that `Guard` cannot share), and an arm's lists are taken with `mem::replace(.., (Vec::new_in(frame), Vec::new_in(frame)))` since a frame `Vec` has no `Default`. Same values, same order.

## B7.5 Move errors: the order of the output

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): the subsections below, each in the tree.

### Today's order, and rustc's

`move_reported` is filled in `report_use_of_moved_or_uninitialized` (line ~2896-2902,
`self.move_reported.insert(move_outs, (used.key(), error))`), once per use of a moved value not
already reported for the same moves, a later one of the same moves replacing the earlier, and
drained into `out` at line 275 (`for (_, (_, mut error)) in checker.move_reported`). Its order
there is **hash order**: an `FxHashMap<Vec<usize>, ..>`'s iteration over std's copy of
hashbrown, which hashbrown 0.17 need not reproduce. rustc's
(`compiler/rustc_borrowck/src/diagnostics/mod.rs:107`,
`buffered_move_errors: BTreeMap<Vec<MoveOutIndex>, (PlaceRef<'tcx>, Diag<'diag>)>`, drained in
order at line 125) is the **order of the keys**: the move-out index vectors, compared
lexicographically, a replaced error keeping its key's place. It is a `BTreeMap`, not an
`FxIndexMap` (that is `buffered_mut_errors`, line 109).

The new order: the errors in an insertion-ordered frame vector with a frame index map by their
moves (B1's rule), sorted once at output by their moves, lexicographically: rustc's order over
blaze's move numbering (the sorted `move_outs` `moved_indexes` gives). It is deterministic
(keys are unique, so the unstable sort has one result) and independent of any hasher. It does
not reproduce rustc's numbering, which is its DFS visit order of the move sites
(`get_moved_indexes`), unsorted; blaze sorts and dedups the moves, and `lib.rs`'s cross-body
buffer (`move_errors`, B4.2) is keyed by the sorted `rustc_key`. Within one body no two errors
share a `rustc_key` (`rustc_moves` is injective), so `lib.rs`'s replacement logic does not
depend on this order; only the order of `result.diagnostics` does.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:848-850 (`move_reported` frame vector, `move_reported_at` index map) and crates/hir-ty-borrowck/src/check.rs:435 (sorted by moves at output).

### `UseOfMoved` (lines ~378-388)

Before:

```rust
pub(super) struct UseOfMoved {
    /// The indices of the moves, as rustc numbers them in the body; `None` when one of them is
    /// a move of rust-analyzer's lowering that rustc's MIR does not have.
    pub(super) rustc_key: Option<Vec<usize>>,
    /// The place used, as rustc's MIR has it: a capture is a field of the closure.
    pub(super) place: (LocalId, Vec<PlaceElem>),
    pub(super) diagnostic: BorrowckDiagnostic,
    /// Where the error is in what `check` reports.
    pub(super) at: usize,
}
```

after (B4.2's; the diagnostic goes to `out` by `MoveReported`, so it is not kept twice):

```rust
pub(super) struct UseOfMoved<'o> {
    /// The indices of the moves, as rustc numbers them in the body; `None` when one of them is
    /// a move of rust-analyzer's lowering that rustc's MIR does not have.
    pub(super) rustc_key: Option<FVec<'o, usize>>,
    /// The place used, as rustc's MIR has it: a capture is a field of the closure.
    pub(super) place: (LocalId, FVec<'o, PlaceElem>),
    /// Where the error is in what `check` reports.
    pub(super) at: usize,
}
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:556 (`UseOfMoved<'o>`, `FVec<'o, ..>` key and place).

### `report_use_of_moved_or_uninitialized` (lines ~2842-2903), per use of a moved place

Before (from the lookup):

```rust
        if let Some((reported, _)) = self.move_reported.get(&move_outs) {
            let reported = PlaceRef { local: reported.0, projection: &reported.1 };
            if used.is_prefix_of(reported) {
                return;
            }
        }
        ...
        let mut rustc_key =
            move_outs.iter().map(|&it| self.rustc_moves[it]).collect::<Option<Vec<usize>>>();
        if let Some(key) = &mut rustc_key {
            radix::sort(key);
        }
        let place = match self.upvar_sources.get(&used.local) {
            Some(source) => {
                let mut projection = source.projection.as_slice().to_vec();
                projection.extend_from_slice(used.projection);
                (source.local, projection)
            }
            None => used.key(),
        };
        let error = UseOfMoved { rustc_key, place, diagnostic, at: 0 };
        self.move_reported.insert(move_outs, (used.key(), error));
```

after (the diagnostic and `use_of_moved_parts`, between, unchanged; `use_of_moved_parts`
takes `&move_outs`, a `&FVec` that derefs to its `&[usize]`):

```rust
        let reported_at = self.move_reported_at.get(&move_outs.as_slice()).copied();
        if let Some(at) = reported_at {
            let (local, projection) = self.move_reported[at as usize].used;
            if used.is_prefix_of(PlaceRef { local, projection }) {
                return;
            }
        }
        ...
        let out_frame = self.out_frame;
        let rustc_key = move_outs.iter().all(|&it| self.rustc_moves[it].is_some()).then(|| {
            let mut key =
                mem::collect(out_frame, move_outs.iter().filter_map(|&it| self.rustc_moves[it]));
            key.sort_unstable();
            key
        });
        let place = match self.upvar_sources.get(&used.local) {
            Some(source) => {
                let mut projection = Vec::with_capacity_in(
                    source.projection.len() + used.projection.len(),
                    out_frame,
                );
                projection.extend_from_slice(source.projection);
                projection.extend_from_slice(used.projection);
                (source.local, projection)
            }
            None => (used.local, mem::collect(out_frame, used.projection.iter().copied())),
        };
        let error = UseOfMoved { rustc_key, place, at: 0 };
        let used = self.kept(used);
        let moves: &'f [usize] = move_outs.leak();
        let reported = MoveReported { moves, used, diagnostic, error };
        match reported_at {
            Some(at) => self.move_reported[at as usize] = reported,
            None => {
                self.move_reported_at.insert(moves, self.move_reported.len() as u32);
                self.move_reported.push(reported);
            }
        }
```

`all(is_some)` then `filter_map` gives what `collect::<Option<Vec<_>>>` gave. A replaced
error stays where the first of its moves was reported, as `insert` on a `BTreeMap` keeps its
key's place, and the output sorts by the key anyway.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:3137-3185 (`out_frame`, `move_reported_at` lookup).

### `moved_indexes` (lines ~2905-2978), per use of a place moved or uninitialized

Before: `-> (Vec<usize>, bool)`, `let mut stack = Vec::new();`, `let mut back_edges =
Vec::new();`, `let mut visited = BitSet::new(self.cfg.points);`, `let mut result =
Vec::new();`, `let mut visit = |result: &mut Vec<usize>, location: Location| -> bool {`,
`radix::sort(&mut result);`. After:

```rust
    fn moved_indexes(&self, location: Location, path: Mpi) -> (FVec<'f, usize>, bool) {
        let frame = self.ctx.frame;
        let above = |it: Mpi| self.moves.is_ancestor_or_self(it, path);
        let mut stack: FVec<'_, Location> = Vec::new_in(frame);
        let mut back_edges: FVec<'_, Location> = Vec::new_in(frame);
        ...
        let mut visited = BitSet::new(frame, self.cfg.points);
        let mut result: FVec<'f, usize> = Vec::new_in(frame);
        let mut reached_start = false;
        let mut visit = |result: &mut FVec<'f, usize>, location: Location| -> bool {
        ... (unchanged to)
        result.sort_unstable();
        result.dedup();
        (result, looped)
    }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:3196 (frame stack, back edges, `bits(frame, ..)` visited after dedup bits' rename).

### `report_move_errors` (lines ~2980-3028), once per body

Before:

```rust
        let errors = std::mem::take(&mut self.move_errors);
        let mut reported_groups: FxHashSet<(ExprId, LocalId, Vec<PlaceElem>)> =
            FxHashSet::default();
        ...
                        let base = error.place.1[..len.min(error.place.1.len())].to_vec();
                        if !reported_groups.insert((scrutinee, error.place.0, base)) {
```

after (the key a subslice of the error's own frame slice, no copy):

```rust
        let frame = self.ctx.frame;
        let errors = std::mem::replace(&mut self.move_errors, Vec::new_in(frame));
        let mut reported_groups: FSet<'_, (ExprId, LocalId, &'f [PlaceElem])> = mem::set(frame);
        ...
                        let projection = error.place.1;
                        let base = &projection[..len.min(projection.len())];
                        if !reported_groups.insert((scrutinee, error.place.0, base)) {
```

`is_slice_or_array` (line ~3030) `fn is_slice_or_array(&self, error: &MoveError<'_>, len:
usize)`, its `&error.place.1[..len]` unchanged text. `move_error_message` (line ~3047)
`fn move_error_message(&self, error: &MoveError<'_>) -> &'f str` (B7.14), its first line
`let place = PlaceRef { local: error.place.0, projection: error.place.1 };`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written; the order is rustc's as the section gives it (`sort_unstable_by` on the move indices).

## B7.6 `unused_mut` (lines ~2326-2413), once per body

Before:

```rust
    fn unused_mut(&mut self) -> Vec<BorrowckDiagnostic> {
        ...
        if self.ctx.infer.has_errors {
            return Vec::new();
        }
        let body = self.body;
        let temporaries: FxHashSet<LocalId> = self
            .used_mut
            .iter()
            .copied()
            .filter(|it| !self.ctx.bindings.contains_key(it))
            .collect();
        let mut assigned = FxHashSet::default();
        let mut moved_into_temporary = |operand: &Operand| {
            if let OperandKind::Move(moved) = &operand.kind
                && moved.projection.as_slice().is_empty()
            {
                self.used_mut.insert(moved.local);
            }
        };
        for &block in &self.cfg.order {
            ...
                    assigned.insert(place.local);
                    if temporaries.contains(&place.local) {
            ...
                assigned.insert(destination.local);
                if temporaries.contains(&destination.local) {
            ...
        }
        ...
        let mut unused: Vec<(LocalId, BindingId)> = self
            .ctx
            .bindings
            .iter()
            .map(|(&local, &binding)| (local, binding))
            .filter(|&(local, binding)| {
                let data = &self.ctx.store[binding];
                data.mode == BindingAnnotation::Mutable
                    && !data.name.as_str().starts_with('_')
                    && !self.used_mut.contains(&local)
                    && self.ctx.upvar_of_local(local).is_none()
                    && match body.param_locals.contains(&local) {
                        true => !moves_arguments,
                        false => assigned.contains(&local),
                    }
            })
            .collect();
        radix::sort_copy_by_key(&mut unused, |&(local, _)| local.into_raw().into_u32());
        let mut seen = FxHashSet::default();
        unused.retain(|&(_, binding)| seen.insert(binding));
        unused
            .into_iter()
            .map(|(_, binding)| { BorrowckDiagnostic::new(..) })
            .collect()
    }
```

after (`temporaries`, `assigned` and `used_mut` bitsets of locals; `bindings` is keyed by local,
so the sort key is unique and the unstable sort gives the radix sort's order):

```rust
    fn unused_mut(&mut self) -> FVec<'f, Draft<'f>> {
        let frame = self.ctx.frame;
        ...
        if self.ctx.infer.has_errors {
            return Vec::new_in(frame);
        }
        let body = self.body;
        let mut temporaries = BitSet::new(frame, body.locals.len());
        for (local, _) in body.locals.iter() {
            if self.used_mut.contains(raw(local)) && !self.ctx.bindings.contains_key(&local) {
                temporaries.insert(raw(local));
            }
        }
        let mut assigned = BitSet::new(frame, body.locals.len());
        let mut moved_into_temporary = |operand: &Operand| {
            if let OperandKind::Move(moved) = &operand.kind
                && moved.projection.as_slice().is_empty()
            {
                self.used_mut.insert(raw(moved.local));
            }
        };
        for &block in &self.cfg.order {
            ...
                    assigned.insert(raw(place.local));
                    if temporaries.contains(raw(place.local)) {
            ...
                assigned.insert(raw(destination.local));
                if temporaries.contains(raw(destination.local)) {
            ...
        }
        ...
        let mut unused = mem::collect(
            frame,
            self.ctx.bindings.iter().map(|(&local, &binding)| (local, binding)).filter(
                |&(local, binding)| {
                    let data = &self.ctx.store[binding];
                    data.mode == BindingAnnotation::Mutable
                        && !data.name.as_str().starts_with('_')
                        && !self.used_mut.contains(raw(local))
                        && self.ctx.upvar_of_local(local).is_none()
                        && match body.param_locals.contains(&local) {
                            true => !moves_arguments,
                            false => assigned.contains(raw(local)),
                        }
                },
            ),
        );
        unused.sort_unstable_by_key(|&(local, _)| raw(local));
        let mut seen = mem::set(frame);
        unused.retain(|&(_, binding)| seen.insert(binding));
        mem::collect(
            frame,
            unused.iter().map(|&(_, binding)| {
                Draft::new(
                    frame,
                    "unused_mut",
                    MirSpan::BindingId(binding),
                    "variable does not need to be mutable",
                )
            }),
        )
    }
```

(`temporaries` was the set of `used_mut`'s locals without a binding: every local of the body is
one of `body.locals`, so going over them and testing `used_mut` gives that set.)

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written; the diagnostics are `Draft`s owned in `check`'s output (B7.14).

## B7.7 Region errors (lines ~1783-2186)

`report_region_errors` runs once per body that checks its universal regions, and, for each
universal region and each type test, on bodies with no error too. After the change it
allocates nothing on such a body: `failing` is a frame vector pushed to only when a region
fails, the type tests walk the region values' bitset rows without collecting them.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): the subsections below, each in the tree.

### `report_region_errors`

Line ~1789 `let mut higher_ranked = FxHashSet::default();` ->
`let mut higher_ranked = mem::set(self.ctx.frame);`. Lines ~1796-1806, before:

```rust
            let failing: Vec<usize> = match self.representative(longer) {
                Some(representative) => Some(representative)
                    .filter(|&it| !self.regions.knows(longer, it))
                    .into_iter()
                    .collect(),
                None => self
                    .values
                    .universals_of(longer_rv)
                    .filter(|&it| !self.regions.knows(longer, it))
                    .collect(),
            };
```

after:

```rust
            let mut failing: FVec<'f, usize> = Vec::new_in(self.ctx.frame);
            match self.representative(longer) {
                Some(representative) => {
                    if !self.regions.knows(longer, representative) {
                        failing.push(representative);
                    }
                }
                None => failing.extend(
                    self.values.universals_of(longer_rv).filter(|&it| !self.regions.knows(longer, it)),
                ),
            }
```

Line ~1817 `regions::constraint_path(self.regions, self.region_count, longer_rv, shorter_rv)`
is unchanged (R0.1: a frame vector, only for a failing region). Line ~1892 `let mut reported =
FxHashSet::default();` -> `let mut reported = mem::set(self.ctx.frame);`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:2022, 2029, 2125 (`mem::set`, frame `failing`).

### `propagate` (lines ~1958-2017), per failing region of a closure

Before:

```rust
        let longer_minus: Vec<usize> = if non_local(longer) {
            vec![longer]
        } else {
            let lower: Vec<usize> =
                (0..universals.len()).filter(|&it| non_local(it) && knows(longer, it)).collect();
            lower
                .iter()
                .copied()
                .filter(|&it| {
                    !lower.iter().any(|&other| other != it && knows(other, it) && !knows(it, other))
                })
                .collect()
        };
        ...
            let path = regions::constraint_path(
                known,
                ...
                let outlived: Vec<usize> =
                    shorter_plus.iter().copied().filter(|&it| knows(minus, it)).collect();
```

after:

```rust
        let frame = self.ctx.frame;
        let longer_minus: FVec<'f, usize> = if non_local(longer) {
            let mut it = Vec::with_capacity_in(1, frame);
            it.push(longer);
            it
        } else {
            let lower = mem::collect(
                frame,
                (0..universals.len()).filter(|&it| non_local(it) && knows(longer, it)),
            );
            mem::collect(
                frame,
                lower.iter().copied().filter(|&it| {
                    !lower.iter().any(|&other| other != it && knows(other, it) && !knows(it, other))
                }),
            )
        };
        ...
            let path = regions::constraint_path(
                known,
                ...
                let outlived =
                    mem::collect(frame, shorter_plus.iter().copied().filter(|&it| knows(minus, it)));
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:2200 (`longer_minus: FVec<'f, usize>`).

### `non_local_upper_bounds` (lines ~2041-2063)

Before: `-> Vec<usize>`, `return vec![universal];`, `let upper: Vec<usize> = (0..universals
.len()).filter(..).collect();`, the result `.collect()`. After:

```rust
    fn non_local_upper_bounds(&self, universal: usize) -> FVec<'f, usize> {
        let frame = self.ctx.frame;
        let known = self.regions;
        let universals = &known.universals;
        let non_local =
            |it: usize| matches!(universals[it].1, UniversalKind::Creator | UniversalKind::Static);
        if non_local(universal) {
            let mut it = Vec::with_capacity_in(1, frame);
            it.push(universal);
            return it;
        }
        let upper = mem::collect(
            frame,
            (0..universals.len()).filter(|&it| non_local(it) && known.knows(it, universal)),
        );
        mem::collect(
            frame,
            upper.iter().copied().filter(|&it| {
                !upper
                    .iter()
                    .any(|&other| other != it && known.knows(it, other) && !known.knows(other, it))
            }),
        )
    }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check.rs:2284 (`-> FVec<'f, usize>`).

### `type_test_holds` and `alias_test_holds` (lines ~2078-2186), per type test of every body

Before (`type_test_holds`):

```rust
        let fn_body = self.regions.universals[1].0;
        let needed: Vec<usize> = self.values.universals_of(lower).collect();
        self.regions.type_bounds_of(ty).into_iter().chain([fn_body]).any(|bound| {
            let have: Vec<usize> = self.values.universals_of(bound).collect();
            needed
                .iter()
                .all(|&shorter| have.iter().any(|&longer| self.regions.knows(longer, shorter)))
        })
```

after (`universals_of` walks a bitset row: asking it again costs what reading the collected
`Vec` did, a row is `universals / 64` words; `type_bounds_of` returns an iterator, per R0.1,
so its `.into_iter()` goes and no `Vec` of the bounds is made either):

```rust
        let fn_body = self.regions.universals[1].0;
        self.regions
            .type_bounds_of(ty)
            .chain([fn_body])
            .any(|bound| self.outlives_all(bound, lower))
```

with, after `type_test_holds`:

```rust
    /// Whether `longer` outlives every universal region `shorter` does, as region inference
    /// left them: each of `shorter`'s is known to be outlived by one of `longer`'s.
    fn outlives_all(&self, longer: regions::Rv, shorter: regions::Rv) -> bool {
        self.values.universals_of(shorter).all(|needed| {
            self.values.universals_of(longer).any(|have| self.regions.knows(have, needed))
        })
    }
```

`alias_test_holds`, before:

```rust
        let interner = self.ctx.infcx.interner;
        let needed: Vec<usize> = self.values.universals_of(lower).collect();
        let outlives = |bound: usize| {
            let have: Vec<usize> = self.values.universals_of(bound).collect();
            needed
                .iter()
                .all(|&shorter| have.iter().any(|&longer| self.regions.knows(longer, shorter)))
        };
        ...
        let regions_of = |ty: Ty<'db>| {
            let mut out = Vec::new();
            ...
        };
        let knows_all = |longer: usize, shorter: usize| {
            let have: Vec<usize> = self.values.universals_of(longer).collect();
            self.values
                .universals_of(shorter)
                .all(|it| have.iter().any(|&longer| self.regions.knows(longer, it)))
        };
        ...
        let mut components = Vec::new();
        let mut params = Vec::new();
```

after:

```rust
        let interner = self.ctx.infcx.interner;
        let frame = self.ctx.frame;
        let outlives = |bound: usize| self.outlives_all(bound, lower);
        ...
        let regions_of = |ty: Ty<'db>| {
            let mut out: FVec<'_, Option<regions::Rv>> = Vec::new_in(frame);
            ... (unchanged)
        };
        let knows_all = |longer: usize, shorter: usize| self.outlives_all(longer, shorter);
        ...
        let mut components: FVec<'_, regions::Rv> = Vec::new_in(frame);
        let mut params: FVec<'_, Ty<'db>> = Vec::new_in(frame);
        let captured = match alias.kind {
            rustc_type_ir::AliasTyKind::Opaque { def_id } => {
                regions::opaque_captures(frame, self.ctx.db, def_id, alias.args.len())
            }
            _ => None,
        };
```

(R0.1: `regions::outlives_components(ty, &mut FVec<'_, Rv>, &mut FVec<'_, Ty<'db>>)` and
`regions::alias_components(ty, &mut FVec<'_, Ty<'db>>)` take these two as they are;
`opaque_captures(frame, db, opaque, len) -> Option<FVec<'f, bool>>` gains the frame, line
~2166, its uses of `captured` (indexing, `is_some`) unchanged.)

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written.

## B7.8 `member_choices` (lines ~292-340), per body

Before: `fn member_choices(regions: &regions::Regions<'_>, values: &regions::RegionValues,
count: usize) -> Vec<(regions::Rv, Option<regions::Rv>)>`, building the outlived-by graph of
every constraint of every body (members or not), then per member `lower`, `upper`, `seen`
(`FxHashSet`), `stack`, `fits`, each a fresh global allocation. After (no work for a body with
no member: the loop over no members returned an empty vector; one set of buffers reused across
members; `seen` a bitset of the `count` regions, as every `sup` of `outlived_by` is below
`count`, and `insert` says whether it was new, as `FxHashSet::insert` did):

```rust
fn member_choices<'f>(
    frame: &'f Frame,
    regions: &regions::Regions<'_, '_>,
    values: &regions::RegionValues<'_>,
    count: usize,
) -> FVec<'f, (regions::Rv, Option<regions::Rv>)> {
    if regions.members.is_empty() {
        return Vec::new_in(frame);
    }
    let fn_body = regions.universals[1].0;
    let constraints = &regions.outlives;
    let outlived_by = crate::relation::Csr::build(
        frame,
        count,
        (0..constraints.len())
            .filter(|&at| constraints.sup(at) < count && constraints.sub(at) < count)
            .map(|at| (constraints.sub(at) as u32, constraints.sup(at) as u32)),
    );
    let choices = mem::collect(frame, regions.choices.iter().filter_map(|&it| regions.universal(it)));
    let mut chosen = Vec::with_capacity_in(regions.members.len(), frame);
    let mut lower: FVec<'_, usize> = Vec::new_in(frame);
    let mut upper: FVec<'_, usize> = Vec::new_in(frame);
    let mut seen = BitSet::new(frame, count);
    let mut stack: FVec<'_, regions::Rv> = Vec::new_in(frame);
    let mut fits: FVec<'_, usize> = Vec::new_in(frame);
    for &member in &regions.members {
        lower.clear();
        lower.extend(values.universals_of(member).filter(|&it| it != 1));
        // A region of the signature written into the hidden type is that region on both sides.
        upper.clear();
        upper.extend(regions.universal(member));
        seen.clear();
        stack.push(member);
        while let Some(at) = stack.pop() {
            for sup in outlived_by.of(at).iter().map(|&it| it as regions::Rv) {
                if seen.insert(sup) {
                    if sup != fn_body
                        && let Some(universal) = regions.universal(sup)
                    {
                        upper.push(universal);
                    }
                    stack.push(sup);
                }
            }
        }
        fits.clear();
        fits.extend(
            choices
                .iter()
                .copied()
                .filter(|&choice| lower.iter().all(|&it| regions.knows(choice, it)))
                .filter(|&choice| upper.iter().all(|&it| regions.knows(it, choice))),
        );
        let ordered = fits
            .iter()
            .copied()
            .filter(|&a| fits.iter().all(|&b| regions.knows(a, b) || regions.knows(b, a)));
        let min = ordered.reduce(|a, b| if regions.knows(a, b) { b } else { a });
        chosen.push((member, min.map(|it| regions.universals[it].0)));
    }
    chosen
}
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written.

## B7.9 `check/explain.rs`: what the explanations build

The explanations run only for an error; their strings are labels of the owned diagnostic, what
they build to find them is in the body's frame. Line 6 `use std::collections::VecDeque;`
stays (for `VecDeque::with_capacity_in`); line 20-26 `use crate::{..}` gains
`mem::{FDeque, FVec}`; line 80 becomes `impl<'f, 'db> Checker<'_, 'f, '_, 'db> {`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): the subsections below, each in the tree.

### `borrowed_variable` (lines 236-254), per error with a borrow span

Before: `fn borrowed_variable(&self, mut expr: ExprId) -> Option<(hir_def::hir::BindingId,
String)>`, ending `Some((binding, store[binding].name.as_str().to_owned()))`. After (the name
borrowed from the store, which the label's `text!` copies into the frame, B7.14):

```rust
    fn borrowed_variable(&self, mut expr: ExprId) -> Option<(hir_def::hir::BindingId, &'f str)> {
        let store = self.ctx.store;
        ... (unchanged)
        Some((binding, store[binding].name.as_str()))
    }
```

(`Ctx::store` is `&'a ExpressionStore` of `Ctx<'a, 'db>`, here `&'f`.)

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check/explain.rs:240 (`-> Option<(BindingId, &'f str)>`).

### `sub_region_live_at` (lines 259-277), per explained loan

Before:

```rust
        let mut seen = BitSet::new(count);
        let mut queue = VecDeque::from([region]);
        seen.insert(region);
```

after (each region is queued at most once, so `count` is the queue's bound and it never grows):

```rust
        let frame = self.ctx.frame;
        let mut seen = BitSet::new(frame, count);
        let mut queue: FDeque<'_, Rv> = VecDeque::with_capacity_in(count, frame);
        queue.push_back(region);
        seen.insert(region);
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check/explain.rs:268 (frame `FDeque`, `BitSet` seen).

### `loop_head` (lines 284-296), per explained loan whose region holds where the access is

Before: `self.is_predecessor_of(from, location)` inside the `find_map`, a fresh block vector and
stack per loop head of the region. After (one pair for the walk, B7.3):

```rust
    fn loop_head(&self, region: Rv, from: Location) -> Option<Location> {
        let (mut seen, mut stack) = self.reach_buffers();
        self.values.points_of(region).iter().take_while(|&it| it < self.cfg.points).find_map(
            |point| {
                let location = self.cfg.location(point);
                (location.statement == 0
                    && self.cfg.predecessors(location.block).iter().any(|&pred| {
                        self.dominators.dominates(location.block, pred)
                    })
                    && self.reaches(from, location, &mut seen, &mut stack))
                .then_some(location)
            },
        )
    }
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> checked by db-audit-mem (22:33): in the tree at crates/hir-ty-borrowck/src/check/explain.rs:290-291 (`reach_buffers`, `reaches`).

### `find_use` (lines 300-338), per explained loan

Before:

```rust
        let mut queue = VecDeque::from([start]);
        let mut seen = BitSet::new(self.cfg.points);
        let mut tys = Vec::new();
```

after (`tys` reused per mention as today; `regions::regions_of(ty, &mut FVec<'_, Rv>)` takes it
as it is, R0.1; line 312's call unchanged):

```rust
        let frame = self.ctx.frame;
        let mut queue: FDeque<'_, Location> = VecDeque::with_capacity_in(16, frame);
        queue.push_back(start);
        let mut seen = BitSet::new(frame, self.cfg.points);
        let mut tys: FVec<'_, Rv> = Vec::new_in(frame);
```

(A point is queued once per predecessor that reaches it, so the queue's length is not bounded
by the points; it starts at 16 and grows in the frame.)

`explain`, `add_explanation`, `mentions`, `use_spans`, `capture_span`, `capture_of`,
`later_use_kind`, `function_span`, `span_at`, `operand`: no temporaries. `add_explanation`'s
`format!`s become `text!`s into the frame (B7.14); the owned copies are made once, by
`Draft::owned`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written; `explain.rs`'s `fn raw` went and it imports `super::{Checker, Draft, raw}`.

## B7.10 The budget test

The budget: **the check of a body with no error takes nothing from the global allocator** in
this part's code: the walk, the setup functions of B7.4 and B7.8, the `Checker`, the reports
and the output. Every temporary is in the body's frame and what is returned in the owner's.

How the test sees it: `frame_only`, new, after `operand_place` at the end of `check.rs`, wraps
each part (B7.1) and in tests adds what it allocated globally to a thread-local counter.
Nested measurement adds the inner part to the outer one, so no part is wrapped twice.

```rust
/// Runs `f`, a part of a body's check that takes memory only from the frames; in tests, adds
/// what it took from the global allocator to [`tests::GLOBAL`].
#[inline]
fn frame_only<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(test)]
    {
        let (out, used) = blaze_alloc_count::measure(f);
        tests::GLOBAL.with(|it| it.set(it.get() + used.allocations));
        out
    }
    #[cfg(not(test))]
    f()
}
```

The test, a module at the end of `check.rs` (the crate's `#[global_allocator]` counting one
under `cfg(test)` and `blaze-alloc-count` as a dev-dependency are B0's; the test database is
`scan::tests::db`, `pub(crate)` per B0):

```rust
#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use base_db::all_crates;
    use hir_def::{ModuleDefId, nameres::crate_def_map};
    use test_fixture::WithFixture;

    use crate::{Mode, borrowck_in_mode, scan::tests::db::TestDB};

    thread_local! {
        /// What the parts of the checks run on this thread took from the global allocator.
        pub(super) static GLOBAL: Cell<u64> = const { Cell::new(0) };
    }

    /// Bodies with no error that reach each part: two-phase borrows, loans in scope at calls
    /// that may unwind and across a loop's way back, guards, an index after an index of a
    /// slice, a union, a `static mut`, closures by reference and by value, a returned
    /// `impl Trait`, moves out and reinitialization.
    const FIXTURE: &str = r#"
//- minicore: sized, copy, clone, fn
struct Point { x: u32, y: u32 }
struct Stack { items: [u32; 4], len: usize }
enum Shape { Circle(Point), Square(u32), Empty }
union U { a: u32, b: f32 }
static mut COUNT: u32 = 0;

impl Stack {
    fn len(&self) -> usize { self.len }
    fn top(&self) -> &u32 { &self.items[0] }
    fn push(&mut self, value: u32) {
        self.items[self.len] = value;
        self.len += 1;
    }
}

fn borrows(p: &mut Point) -> u32 {
    let a = &p.x;
    let b = &p.y;
    let sum = *a + *b;
    p.x = sum;
    let c = &mut p.y;
    *c += 1;
    p.x + p.y
}

fn two_phase(s: &mut Stack) {
    s.push(s.len() as u32);
}

fn loops(s: &mut Stack) {
    let mut i = 0;
    while i < 3 {
        let top = s.top();
        s.push(*top);
        i += 1;
    }
}

fn guards(shape: Shape, limit: u32) -> u32 {
    match shape {
        Shape::Circle(point) if point.x > limit => point.y,
        Shape::Circle(point) => point.x,
        Shape::Square(side) if side > limit => side,
        Shape::Square(_) | Shape::Empty => 0,
    }
}

fn indexes(rows: &[&[u32]], grid: &mut [[u32; 4]; 4], i: usize, j: usize) -> u32 {
    grid[i][j] += 1;
    rows[1][i] + grid[j][i]
}

fn unions() -> u32 {
    let mut u = U { a: 1 };
    u.a = 2;
    unsafe { u.a }
}

fn statics() -> u32 {
    unsafe {
        COUNT += 1;
        COUNT
    }
}

fn closures(p: Point) -> u32 {
    let mut n = p.x;
    let mut add = |k: u32| n += k;
    add(1);
    add(2);
    let read = move || p.y;
    n + read()
}

fn opaque<'a>(p: &'a Point) -> impl Copy + 'a {
    &p.x
}

fn moves(shape: Shape) -> Shape {
    let mut held = shape;
    let taken = held;
    held = Shape::Empty;
    match held {
        Shape::Empty => taken,
        other => other,
    }
}
"#;

    #[test]
    fn a_body_with_no_error_takes_nothing_from_the_global_allocator() {
        let (db, _) = TestDB::with_single_file(FIXTURE);
        hir_ty::attach_db(&db, || {
            let mut bodies = 0;
            for &krate in all_crates(&db).iter() {
                let def_map = crate_def_map(&db, krate);
                for (_, module) in def_map.modules(&db) {
                    for def in module.scope(&db).declarations() {
                        let ModuleDefId::FunctionId(function) = def else { continue };
                        for mode in [Mode::Nll, Mode::Polonius] {
                            // The first run warms what outlives a check: the queries it asks
                            // and the types it interns.
                            let first = borrowck_in_mode(&db, function.into(), mode);
                            assert!(first.skipped.is_none(), "{function:?}: {:?}", first.skipped);
                            assert!(first.not_checked.is_none(), "{function:?}");
                            assert!(first.diagnostics.is_empty(), "{:?}", first.diagnostics);
                            GLOBAL.with(|it| it.set(0));
                            let _ = borrowck_in_mode(&db, function.into(), mode);
                            let global = GLOBAL.with(Cell::get);
                            assert_eq!(global, 0, "{function:?} in {mode:?}: global allocations");
                        }
                        bodies += 1;
                    }
                }
            }
            assert!(bodies >= 10, "{bodies} bodies");
        });
    }
}
```

`Mode` is `Copy + Debug` (`lib.rs` line 70); `NotChecked` is asserted `None` without printing
it. The 10 bodies counted are the free functions (`module.scope(..).declarations()` does not
list `Stack`'s methods, which `two_phase` and `loops` call); the closures in `closures` are
checked inside their owner by `borrowck_in_mode`, so their walks are measured too.

What else the test needs to pass, outside this part: the walk calls `Ctx::is_copy` and
`Ctx::place_ty`, which ask the trait solver; until mem-db-solver and mem-db-infer put the
solver's temporaries in `frame::current`, those allocate globally inside `frame_only` and the
test names the function where they do. That failure is theirs to clear, not a reason to widen
the budget.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): `frame_only` at the end of `check.rs` as written. The test is in `check/tests.rs`, not a new `mod tests` in `check.rs`: `check.rs` already declares `mod tests;` for task 38's tests. `GLOBAL` is there as `pub(super)`, the fixture is `CLEAN` (`FIXTURE` is task 38's), and it uses that file's `TestDB = base_db::Db` (what `scan::tests::db::TestDB` aliases).

## B7.11 What this part asks of `mem-db-checks-regions` (`regions.rs`)

Settled: this part is written against R0.1 of `v2/requests/mem-db-checks-regions.md`:

1. `build(ctx, body, &cfg, requirements) -> Regions<'f, 'db>`, `requirements` the
   `&FMap<'_, ExprId, (usize, FVec<'_, Requirement<'db>>)>` B4.2 passes `check`: line ~65's
   text unchanged.
2. `constraint_path(&Regions<'f, '_>, count, from, to) -> FVec<'f, Outlives>`: in `regions`'
   frame, so the four calls (`check`'s debugging ~149, `report_does_not_live_long_enough`
   ~1563, `report_region_errors` ~1817, `propagate` ~1984; all error paths) keep their text.
   `blame`/`blame_with` take `&[Outlives]`, unchanged.
3. `regions_of(ty, &mut FVec<'_, Rv>)`, `outlives_components(ty, &mut FVec<'_, Rv>, &mut
   FVec<'_, Ty<'db>>)`, `alias_components(ty, &mut FVec<'_, Ty<'db>>)`: `explain.rs`'s
   `find_use` and `alias_test_holds` pass frame vectors (B7.7, B7.9).
4. `type_bounds_of(ty) -> impl Iterator<Item = Rv> + '_`: `type_test_holds` chains it with no
   `.into_iter()` (B7.7).
5. `opaque_captures(frame, db, opaque, len) -> Option<FVec<'f, bool>>`: `alias_test_holds`
   passes `self.ctx.frame` (B7.7).
6. `adt_variants(db, adt) -> impl Iterator<Item = VariantId> + '_` (R8): neither `check.rs` nor
   `explain.rs` calls it. `parts.rs` ~267's `adt_variants(db, adt).into_iter().all(..)` still
   compiles (an iterator is `IntoIterator`; its `.into_iter()` may go). `names.rs` ~105 is in
   B7.14. `member_regions(frame, ..)` (R8): not called by this part.
7. `path_binding`, `spells_generics`, `alias_def_id`, `rv_of`, `normalize`: unchanged.
8. `type_bounds_of` stays the iterator (`mem-db-checks-regions` withdrew its delete), as item
   4 and B7.7 use it.
9. `Regions<'f, 'db>`, `RegionValues<'f>` and `Constraints<'f>` per B3; `universals`,
   `type_bounds`, `members`, `choices`, `creator_positions`, `outlives`, `local_tys`,
   `related_placeholders`, `normalized_placeholders`, `unsuggested` keep their names and stay
   indexable, iterable and `contains`/`get`-able as this part uses them.

And of the parent (B5): `InitFlow::compute(frame, body, cfg, moves)` and
`regions::infer(frame, regions, count, points, live)` take the frame first; `Loans`,
`MoveData`, `InitFlow` (its `uninit_entry`, `ever_entry`: `BitMatrix<'f>`) get `'f`;
`loans.entry_sets(..)` returns a `BitMatrix<'f>`. B5's table (written since) gives exactly
these; B7.12 has the call sites against it.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): nothing to edit here; check.rs calls `regions.rs` with R0.1's signatures.

## B7.12 `check`'s calls of B5's analyses

Against the table at the top of B5 in `v2/requests/mem-db-checks.md`. Already as B5 has them
(no change): `MoveData::gather(ctx, body, &cfg)`, `liveness::region_liveness(ctx, body, &cfg,
&moves, &regions, count)`, `Loans::gather(ctx, body, &cfg, &regions, unique)`,
`polonius::live_loans(ctx, body, &cfg, &regions, count, &live, &loans)`, and
`regions.outlives.extend(applied)` (B5.6 keeps `impl Extend<Outlives> for Constraints<'_>`;
`applied` is now an `FVec`, consumed the same way). `Constraints::new(frame)` is called inside
`regions::build`, not in this part. B7.1 already has `Cfg::new(frame, body)`,
`Dominators::new(frame, &cfg)`, `InitFlow::compute(frame, body, &cfg, &moves)` and both
`regions::infer(ctx.frame, ..)`.

Three calls gain `ctx`. B5 says `entry_sets`'s call "already passes `ctx`"; in the working tree at
20:40 none of them does. Lines ~105-112, before:

```rust
    phase(Phase::Scopes, || match ctx.mode {
        Mode::Polonius => {
            let live_loans = polonius::live_loans(ctx, body, &cfg, &regions, count, &live, &loans);
            loans.compute_polonius_scopes(body, &cfg, &live_loans);
        }
        _ => loans.compute_scopes(body, &cfg, &values),
    });
    let borrows_entry = phase(Phase::EntrySets, || loans.entry_sets(body, &cfg));
```

after:

```rust
    phase(Phase::Scopes, || match ctx.mode {
        Mode::Polonius => {
            let live_loans = polonius::live_loans(ctx, body, &cfg, &regions, count, &live, &loans);
            loans.compute_polonius_scopes(ctx, body, &cfg, &live_loans);
        }
        _ => loans.compute_scopes(ctx, body, &cfg, &values),
    });
    let borrows_entry = phase(Phase::EntrySets, || loans.entry_sets(ctx, body, &cfg));
```

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): as written (`compute_polonius_scopes`/`compute_scopes`/`entry_sets` take `ctx`).

## B7.13 With `dedup: bits` (`v2/requests/db.md`): settled

Agreed with `dedup-bits-db`. In `v2/requests/db.md` the first borrowck section is marked
superseded by "## dedup: bits: hir-ty-borrowck on frame words (supersedes the one above)". That
section is applied after B2 and B7:

- `bitset.rs` keeps `BitSet<'f> = blaze_ids::bits::BitSliceMut<'f>` and
  `BitMatrix<'f> = blaze_ids::bits::BitMatrix<usize, &'f mut [u64]>`, with the frame
  constructors `bits`, `filled_bits`, `bits_for_rows_of`, `bits_copy` and `matrix`.
  `matrix(frame, rows, columns)` builds it with
  `BitMatrix::<usize, &mut [u64]>::from_words(rows, columns, data)`.
- `Transfer` is implemented for `Bits<S: WordsMut>`, so `moves::apply` still drives
  `checker.uninit`/`ever` (`check.rs` ~235).
- `BitMatrix`'s storage parameter has landed in `crates/blaze-ids/src/bits.rs` with the index
  type first: `BitMatrix<I = usize, S = Vec<u64>>`. With the storage first, `overlap.rs`'s
  `BitMatrix<usize>` and diagnostics' `BitMatrix<Index>` would break.

Its section 2 renames B2's and B7's spellings across `crates/hir-ty-borrowck/src/`. This
request keeps B2.1's spellings, and that rename turns them into the dedup's. This part uses:

| this request (B2.1) | after that section |
|---|---|
| `BitSet::new(frame, n)` (the locals sets, `reach_buffers`, `moved_indexes`, `rustc_move_indices`, `capture_temps`, `static_muts`, `unused_mut`, `member_choices`, `explain.rs`) | `bits(frame, n)` |
| `BitSet::for_rows_of(frame, &m)` (`uninit`, `ever`, `in_scope`, B7.1) | `bits_for_rows_of(frame, &m)` |
| `set.load(m.row(index(block)))` (the walk, B7.1) | `set.copy_from(&m.row(index(block)))` |
| `BitSet<'f>` fields and returns, `BitSet::default()` in `mem::take` (B7.3) | unchanged (aliases; `&mut [u64]` is `Default`) |
| `insert`, `contains`, `remove` | unchanged (`insert` returns `bool` in both, so `!self.uninit_reported.insert(raw(..))` stands) |

With the sets in the frame, B7.10 and T1 hold under either spelling; nothing else in B7 changes.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): nothing to edit (settled).

## B7.14 Diagnostics drafted in the frame, owned once (T1)

T1 (`budget.rs`) asserts that checking a body allocates exactly `owned_blocks(&result)` global
blocks: one per `String` and per `Vec` with room. `blaze_alloc_count` counts every `alloc` and
every `realloc`. Today these all count:

- **Growing with `format!`:** `format!` reserves an estimate and may grow.
- **Growing by push:** `labels`, `children` and `suggestions` grow by `push` (4, then 8: a
  `realloc`). `mut_errors` adds up to ten labels to one error (`check.rs` ~2570).
- **Throwaway text:** every intermediate string is a block thrown away: `describe`'s
  `format!` per projection element, `quoted`, `array_name`, `reason`, `capture_name`,
  `field.to_string()`.
- **Replaced errors:** an error a later one replaces (`errors[i] = None`, `move_reported`)
  was built for nothing.

So a diagnostic is built as a `Draft<'f>` in the body's frame: texts `&'f str`, lists `FVec<'f, _>`
or `&'f [_]`. It is made a `BorrowckDiagnostic` once, at output (B7.1's `Draft::owned`), each text
by `String::from(&str)` (one allocation of its length, none when empty) and each list by
`collect` of a slice iterator (`TrustedLen`: one `with_capacity(len)`, none when empty). That
is exactly `owned_blocks` and nothing else. Only `check/` builds borrowck diagnostics (no
other file calls `label`, `note`, `suggest` or `BorrowckDiagnostic::new`), so the draft
lives in `check.rs`.

### The types and helpers (new, `check.rs`, before `mod explain;` and `mod parts;`)

A `macro_rules!` is visible only to items after it, so `text!` goes above the two `mod` lines
for `explain.rs` and `parts.rs` to use it.

```rust
/// `format!` into the body's frame: `text!(it, "..", ..)` of a [`Checker`] or a [`Draft`].
macro_rules! text {
    ($in:expr, $($arg:tt)*) => {
        $in.text(format_args!($($arg)*))
    };
}

/// A diagnostic while it is built: its texts and lists in the body's frame, so building it,
/// relabelling it, and dropping one a later error replaces take nothing from the global heap.
/// [`Draft::owned`] makes the [`BorrowckDiagnostic`] the result keeps.
pub(super) struct Draft<'f> {
    frame: &'f Frame,
    pub(super) code: &'static str,
    pub(super) span: MirSpan,
    pub(super) message: &'f str,
    pub(super) at_signature: bool,
    pub(super) labels: FVec<'f, (PartSpan, &'f str)>,
    pub(super) children: FVec<'f, DraftChild<'f>>,
    suggestions: FVec<'f, DraftSuggestion<'f>>,
}

/// A [`Child`] while drafted.
pub(super) struct DraftChild<'f> {
    level: Sublevel,
    pub(super) message: &'f str,
    spans: &'f [PartSpan],
    labels: &'f [(PartSpan, &'f str)],
}

/// A [`Suggestion`] while drafted.
struct DraftSuggestion<'f> {
    message: &'f str,
    parts: &'f [(PartSpan, &'f str)],
    applicability: Applicability,
    verbose: bool,
}

impl<'f> Draft<'f> {
    pub(super) fn new(
        frame: &'f Frame,
        code: &'static str,
        span: MirSpan,
        message: &'f str,
    ) -> Self {
        Draft {
            frame,
            code,
            span,
            message,
            at_signature: false,
            labels: Vec::new_in(frame),
            children: Vec::new_in(frame),
            suggestions: Vec::new_in(frame),
        }
    }

    pub(super) fn text(&self, args: fmt::Arguments<'_>) -> &'f str {
        Text::format(self.frame, args)
    }

    pub(super) fn label(&mut self, span: PartSpan, text: &'f str) {
        self.labels.push((span, text));
    }

    pub(super) fn note(&mut self, message: &'f str) {
        self.child(Sublevel::Note, message, &[], &[]);
    }

    pub(super) fn child(
        &mut self,
        level: Sublevel,
        message: &'f str,
        spans: &[PartSpan],
        labels: &[(PartSpan, &'f str)],
    ) {
        let (spans, labels) = (self.frame.alloc_slice_copy(spans), self.frame.alloc_slice_copy(labels));
        self.children.push(DraftChild { level, message, spans, labels });
    }

    pub(super) fn suggest(
        &mut self,
        message: &'f str,
        parts: &[(PartSpan, &'f str)],
        applicability: Applicability,
        verbose: bool,
    ) {
        let parts = self.frame.alloc_slice_copy(parts);
        self.suggestions.push(DraftSuggestion { message, parts, applicability, verbose });
    }

    /// The diagnostic the result keeps: each text and each list copied once at its length,
    /// one allocation each and none for an empty one (T1's `owned_blocks`).
    pub(super) fn owned(&self) -> BorrowckDiagnostic {
        fn labels(labels: &[(PartSpan, &str)]) -> Vec<(PartSpan, String)> {
            labels.iter().map(|&(span, text)| (span, String::from(text))).collect()
        }
        BorrowckDiagnostic {
            code: self.code,
            span: self.span,
            message: String::from(self.message),
            at_signature: self.at_signature,
            labels: labels(&self.labels),
            children: self
                .children
                .iter()
                .map(|it| Child {
                    level: it.level,
                    message: String::from(it.message),
                    spans: it.spans.to_vec(),
                    labels: labels(it.labels),
                })
                .collect(),
            suggestions: self
                .suggestions
                .iter()
                .map(|it| Suggestion {
                    message: String::from(it.message),
                    parts: labels(it.parts),
                    applicability: it.applicability,
                    verbose: it.verbose,
                })
                .collect(),
        }
    }
}
```

No `help`: nothing in `check/` calls it today; add it beside `note` when something does.
Imports: `use std::fmt;`, `super::{Child, Sublevel, Suggestion, names::Text}` beside
B7.0's. `lib.rs`'s `BorrowckDiagnostic::label`, `note`, `help`, `suggest` (lines 191-226,
`pub(crate)`) then have no callers and go (B4's file: the parent's to apply). Keep
`BorrowckDiagnostic::new`, which is `pub`.

`Text` is in `names.rs` (below), which builds place names in the same buffer.

### The `Checker`'s text helpers (`check.rs` lines ~781-836), before:

```rust
    fn report(&mut self, code: &'static str, span: MirSpan, message: String) {
        self.report_with(BorrowckDiagnostic::new(code, span, message));
    }

    fn report_with(&mut self, mut diagnostic: BorrowckDiagnostic) {
    ...
    fn describe(&self, place: PlaceRef<'_>) -> Option<String> {
        self.ctx.describe(self.body, place.local, place.projection)
    }

    fn quoted(&self, place: PlaceRef<'_>, otherwise: &str) -> String {
        match self.describe(place) {
            Some(name) => format!("`{name}`"),
            None => otherwise.to_owned(),
        }
    }
```

after (`report_with`'s body unchanged):

```rust
    fn text(&self, args: fmt::Arguments<'_>) -> &'f str {
        Text::format(self.ctx.frame, args)
    }

    fn draft(&self, code: &'static str, span: MirSpan, message: &'f str) -> Draft<'f> {
        Draft::new(self.ctx.frame, code, span, message)
    }

    fn report(&mut self, code: &'static str, span: MirSpan, message: &'f str) {
        let diagnostic = self.draft(code, span, message);
        self.report_with(diagnostic);
    }

    fn report_with(&mut self, mut diagnostic: Draft<'f>) {
    ...
    fn describe(&self, place: PlaceRef<'_>) -> Option<&'f str> {
        self.ctx.describe(self.body, place.local, place.projection)
    }

    fn quoted(&self, place: PlaceRef<'_>, otherwise: &'f str) -> &'f str {
        match self.describe(place) {
            Some(name) => text!(self, "`{name}`"),
            None => otherwise,
        }
    }
```

### The rules for every site in `check.rs`, `check/explain.rs` and `check/parts.rs`

`parts.rs` is task 38's file; it is included so the change applies in one pass, by the same
rules. None of these changes text or order.

1. `format!(..)` becomes `text!(self, ..)` in a `Checker` method, or `text!(diagnostic, ..)`
   where only a draft is at hand (`parts.rs`'s free `type_no_copy`, line ~688). The sites:
   `check.rs` (the 43 `format!`s of the tree at 20:40, lines 706-3104), `explain.rs` lines
   145-223 (9), `parts.rs` (32).
2. `"..".to_owned()` becomes `".."` and `String::new()` becomes `""`. A `&'static str`
   coerces to `&'f str`. The sites: `check.rs` lines 271, 834, 1122, 1518, 1618-1637, 1691,
   1830-1862, 2411, 2533-2551; `explain.rs` 194-195; `parts.rs` 280-281, 317-327, 387-422,
   537.
3. `BorrowckDiagnostic::new(code, span, message)` becomes `self.draft(code, span, message)`.
   The three struct updates `BorrowckDiagnostic { at_signature: X, ..BorrowckDiagnostic::new(..) }`
   (`check.rs` lines ~265, ~1886, ~1945) become `Draft { at_signature: X, ..self.draft(..) }`.
   `Draft`'s private `frame` is visible there, since it is the same module.
4. `diagnostic: &mut BorrowckDiagnostic` parameters become `&mut Draft<'f>`: `explain.rs`'s
   `add_explanation`, and the fourteen `parts.rs` methods and `type_no_copy` (which gains `<'f>`).
5. These texts become `&'f str`, from `String`:
   - `report`, `describe`, `quoted`, `array_name` (~3059) and `move_error_message` (~3071).
   - The `(code, message)` tuples of `report_region_errors` (~1826, ~1935).
   - `report_mutability_error`'s `reason` (~2530) and `message` (~2554).
   - `report_illegal_reassignment`'s `name` (~2606; `unwrap_or_default()` gives `""` on
     `Option<&str>`).
   - `explain.rs`'s `what` (~188).
   - `parts.rs` line ~625's `&self.quoted(place, "value")` loses its `&`.
6. A `Child { level, message, spans: vec![..], labels: vec![..] }` pushed onto
   `diagnostic.children` becomes `diagnostic.child(level, message, &[..], &[..])`. The sites:
   `parts.rs` lines ~275-283 and ~543-548. A suggestion's `vec![(at, "mut ".to_owned())]`
   becomes `&[(at, "mut ")]` (`parts.rs` ~387, ~415, ~422, ~537). `suggest_cloning_inner`'s
   `let mut parts = Vec::with_capacity(2);` (~313) becomes
   `let mut parts: FVec<'_, (PartSpan, &str)> = Vec::new_in(self.ctx.frame);`, passed as
   `&parts`.
7. In `report_mutability_error`'s count of borrows (`check.rs` ~2570-2584), `self` is
   borrowed by `&mut self.errors[*index]`, so the note is formatted through the draft. Before:

```rust
                        let note = format!("...and {} other attempted mutable borrows", count - 10);
                        match error.children.last_mut() {
                            Some(last) if count > 11 => last.message = note,
                            _ => error.note(note),
                        }
```

   after:

```rust
                        let note = text!(error, "...and {} other attempted mutable borrows", count - 10);
                        match error.children.last_mut() {
                            Some(last) if count > 11 => last.message = note,
                            _ => error.note(note),
                        }
```

   The note is rewritten from the 11th borrow on: today that is a global `String` per borrow
   past ten; now it is a few frame bytes per borrow, and the owned note is made once.

`self.report(code, span, text!(self, ..))` borrows `self` shared inside a `&mut self` call. It
compiles by two-phase borrows (the receiver's reborrow is activated after the arguments), as
`v.push(v.len())` does. `text!(self, ..)`'s result is `&'f str`, not tied to the borrow of
`self`.

Formatting a type (`{ty:?}`, `{def:?}`) writes into the formatter with no heap of its own:
`Ty`'s `Debug` is its `TyKind`'s, and `AdtDef`'s writes the name of a memoized signature.
A memo miss there is salsa's (mem-db-salsa), as in T1's notes.

### `names.rs`: the place names in the frame (outside this part; what B7 needs of it)

`names.rs` builds a name with a `format!` per projection element and a `String` per
field name. Every message of `check/` goes through it, and it allocates more than the
diagnostic does. `local_name` copies a name the store holds. After, with `Ctx.frame`
(B4.4):

```rust
use std::fmt;

use crate::mem::{FVec, Frame};

/// A string written in a frame: a diagnostic's text while it is drafted.
pub(super) struct Text<'f>(FVec<'f, u8>);

impl<'f> Text<'f> {
    pub(super) fn new(frame: &'f Frame) -> Self {
        Text(Vec::new_in(frame))
    }

    /// `args` formatted into `frame`: a literal as it is, anything else written in pieces.
    pub(super) fn format(frame: &'f Frame, args: fmt::Arguments<'_>) -> &'f str {
        if let Some(text) = args.as_str() {
            return text;
        }
        let mut text = Text::new(frame);
        fmt::write(&mut text, args).expect("a diagnostic's text formats");
        text.finish()
    }

    pub(super) fn push(&mut self, piece: &str) {
        self.0.extend_from_slice(piece.as_bytes());
    }

    /// Puts the ASCII `byte` before what is written: the `*` of a dereference.
    pub(super) fn prefix(&mut self, byte: u8) {
        debug_assert!(byte.is_ascii());
        self.0.insert(0, byte);
    }

    pub(super) fn finish(self) -> &'f str {
        std::str::from_utf8(self.0.leak()).expect("written as `str`s")
    }
}

impl fmt::Write for Text<'_> {
    fn write_str(&mut self, piece: &str) -> fmt::Result {
        self.push(piece);
        Ok(())
    }
}
```

`from_external_macro` (lines 75-92) is not touched here. It reads a `SyntaxNode`, which
`frontend-tree` may move to `syntax::view`, and nothing in B7 adds a `SyntaxNode` use.

`local_name` (lines 22-28) returns `Option<&'a str>`: `Some(self.store[binding].name.as_str())`,
no copy. Its callers only test it or print it (`explain.rs` 116-117, 198, 530; `check.rs`
~2856). `describe` (lines 31-70) returns `Option<&'a str>`, written into one `Text`:

```rust
    pub(super) fn describe(
        &self,
        body: &MirBody<'db>,
        local: LocalId,
        projection: &[PlaceElem],
    ) -> Option<&'a str> {
        let mut name = Text::new(self.frame);
        let mut start = 0;
        match self.upvar_of_projection(body, local, projection) {
            Some((upvar, used)) => {
                start = used;
                self.capture_name(upvar, &mut name);
            }
            None if self.bindings.get(&local).is_some_and(|&it| self.from_external_macro(it)) => {
                return None;
            }
            None => name.push(self.local_name(local)?),
        }
        let mut ty = self.place_ty(body, local, &projection[..start]);
        for (index, elem) in projection.iter().enumerate().skip(start) {
            match elem {
                ProjectionElem::Deref => {
                    let next_is_field =
                        matches!(projection.get(index + 1), Some(ProjectionElem::Field(_)));
                    if !next_is_field {
                        name.prefix(b'*');
                    }
                }
                ProjectionElem::Field(field) => {
                    name.push(".");
                    self.field_name(ty, field.0, &mut name);
                }
                ProjectionElem::Index(_) => name.push("[_]"),
                ProjectionElem::ConstantIndex { .. } | ProjectionElem::Subslice { .. } => {
                    name.push("[..]")
                }
                ProjectionElem::Downcast(_) => (),
            }
            ty = self.project(ty, elem);
        }
        Some(name.finish())
    }
```

`capture_name(&self, upvar: usize, out: &mut Text<'_>)` does the same: `out.push` the binding's
name, `out.prefix(b'*')` for a `Deref`, and `out.push(".")` then the field. Its variant lookup
(line ~105) `adt_variants(self.db, def.def_id()).get(variant_idx as usize).copied()` becomes
`adt_variants(self.db, def.def_id()).nth(variant_idx as usize)`, as `adt_variants` now
returns an iterator (R8 of `v2/requests/mem-db-checks-regions.md`). `variant_field_name`
and `field_name` gain `out: &mut Text<'_>` and write `data.name.as_str()` with `out.push`, or
the index with `write!(out, "{field}")` (`fmt::Write`; infallible here, so
`let _ = write!(..)` or `.expect(..)`), for `.to_string()`. Each name builds the same bytes as
before: a `*` prefixed is today's `format!("*{name}")`, and an appended piece is today's
`format!("{name}..")`.

### What it costs

The error path writes each text once into the frame (plus `describe`'s `prefix` shifts, a
few bytes per `*`). `Draft::owned` copies each text and list once. Global allocations per
reported diagnostic: exactly `owned_blocks`: 1 for the message, 1 per label, plus 1 for the
`labels` list when it has any, likewise for children, their spans, labels and suggestions.
A body with no error allocates none (B7.10). A replaced error (`errors[i] = None`,
`move_reported` overwritten) costs frame bytes only.

No new `dyn` on a hot path: `Text::format`'s `fmt::write` goes through `&mut dyn fmt::Write`, as
every `format!` does today, and runs only when an error is reported. The walk (B7.3) calls
nothing through a `dyn`.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck (21:40): the `names.rs` subsection only (`Text`, `local_name`/`describe` -> `Option<&'a str>`, `capture_name`/`variant_field_name`/`field_name` writing into a `Text`); the rest of B7.14 is db-mem-borrowck-check's. The variant lookup is `adt_variants(..).into_iter().nth(i)`, which serves both today's `Vec` and R8's iterator.
> applied by db-mem-borrowck-check (21:46): the types and helpers, the `Checker`'s text helpers and every site in `check.rs`, `check/explain.rs` and `check/parts.rs` (task 38's `*_parts` builders: `text!(self, ..)` for each `format!`, `diagnostic.child(..)`/`suggest(.., &[..])` with frame slices, `type_no_copy` drafting through `text!(diagnostic, ..)`). The `names.rs` subsection is db-mem-borrowck's, applied above. Two `".clone()"` suggestion literals in `parts.rs` carry `// clone:` for hot_lint.

## B7.15 The bound, and how close the code comes to it

**Budget.** For a body with no error: zero global allocations in `check.rs`/`explain.rs`
(B7.10's test), and zero frame allocations per statement and per access: every frame
allocation of this part is made once per body (B7.1, B7.4, B7.8) or once per error. For a
body with errors: exactly T1's `owned_blocks` of the result, one allocation per non-empty
`String` and `Vec` of each `BorrowckDiagnostic`, made by `Draft::owned` (B7.14); drafting,
relabelling and replacing errors take frame bytes only. This holds while the sets stay frame
slices (B7.13).

**Bytes per access: 0.** The walk's per-access work in this part is an `is_empty` test of
`access_reported` (empty on a body with no error: hashbrown returns before hashing), the CSR
row of the local's loans (`of_local`, two loads) with one bit test of `in_scope` per loan, and
the bit tests of `uninit`, `ever`, `used_mut`, `static_muts`. Before the change it was a
`malloc` + `free` per access to a place with a projection (`place.key()`), a `SmallVec` copy
of the in-scope loans, a `malloc` of every in-scope loan at each call, drop, assert and back
edge (`check_for_invalidation_at_unwind`), and the place's description (`ctx.describe` and a
`format!`) at every statement of a guard. At ~20 ns for a `malloc`/`free` pair against ~1 ns
for an L1 bit test, those were most of the walk's own cost on a body whose places have
projections.

**Bytes per point: 0 during the walk; per body, in the body's frame:**

| what | bytes |
|---|---|
| `uninit`, `ever`, `in_scope` | `(paths + locals + loans) / 8`, rounded up to words |
| `uninit_reported`, `static_muts`, `used_mut`, `unused_mut`'s two, `capture_temps`'s `variables` | `6 × locals / 8` |
| `rustc_move_indices` | `2 × locals / 8` + `16 × locals` (`assigned`) + `8 × moves` (`order`) + `16 × moves` (`out`) |
| `index_spans`, `capture_temps` | ~17 bytes per `StorageLive` and per closure capture (a 12-byte entry and its control byte at hashbrown's 7/8 load) |
| `guard_borrows`, `index_borrows`, `upvar_sources`, `member_choices` | only in bodies with guarded `match`es, two indexes into slices, closures, a returned `impl Trait`: their places are borrowed, not copied |
| the empty sets and maps | 0 (hashbrown's empty table allocates nothing) |

With about one local and one move per point, that is ~40 bytes per point, all in memory the
frame already holds warm from the previous body (B4.2's nested task per body), written once
and read from cache.

**Time.** The floor of the walk is the work rustc's `MirBorrowckCtxt` does per access: the
conflict test over the local's in-scope loans, the move path lookup per projection element
(one hash probe each, `MoveData::find`), the init bit test, the mutability walk. The walk's
own overhead above that floor after the change is the `is_empty` test and the loop over a
CSR row: at most three L1 loads per access, against the floor's at least one hash probe per
projection element and one bit test per loan, which is within the 10%; nothing in it is
allocation-, lock- or hash-bound. The row loads per block (`load` of three rows,
`(paths + locals + loans) / 8` bytes) are the same copies rustc's
`reset_to_entry_of` makes, at memory bandwidth.

**Where each order of the output comes from.** `errors` in the order reported; the move
errors sorted by their moves (B7.5); `unused_mut` by local (B7.6); nothing iterates a hash map
into the output.

> forwarded to db (`hir-ty-borrowck`) (from requests, pass 17; told by tmux).
> applied by db-mem-borrowck-check (21:46): nothing to edit (the bound).

