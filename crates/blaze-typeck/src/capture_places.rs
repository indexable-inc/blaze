//! Closure captures as PLACES (the Reference, "Closure types": "Capture modes", "Capture
//! precision"; RFC 2229): what each closure and `async` block of a body captures, as an ordered
//! list of `(place, mode, captured type)`, the captured type `T`, `&T` or `&mut T`.
//!
//! Follows rustc_hir_typeck expr_use_visitor.rs (every use of a place, by consume, borrow or
//! mutate) and upvar.rs (`process_collected_capture_information`, `compute_min_captures`). One
//! walk per owner, children first (rules/closure.rs `capture_order`): a nested closure's places
//! are replayed in its parent as rustc's `walk_captures` does.
//!
//! THE RULES, each applied where rustc applies it:
//! - A use of a `Copy` place by value is a shared borrow (`consume_or_copy`), in any closure.
//! - A borrow through a raw pointer is a shared borrow ("Raw pointers don't inherit mutability").
//! - A place through a raw pointer or a union stops at it (`restrict_precision_for_unsafe`).
//! - An index projection captures the whole indexed place (`restrict_capture_precision`).
//! - A place through a shared reference is captured as that reference, by shared borrow
//!   (`truncate_capture_for_optimization`).
//! - The closure's kind is the strongest of its uses, read before the `move` rule: a by-value
//!   capture makes it `FnOnce`, a mutable or unique borrow `FnMut` (upvar.rs).
//! - A `move` closure (and an `async move` block) captures every place by value, stopped at its
//!   first deref; a by-value capture of any closure stops at its first deref.
//! - A by-value capture of a non-`Copy` place stops at the first ADT with a destructor on its path
//!   (`restrict_precision_for_drop_types`): the whole value moves, so its `Drop` runs once.
//! - A place captured with one of its ancestors merges into the ancestor, in the stronger mode; a
//!   `&mut` deref cut off a mutable borrow leaves a unique immutable borrow
//!   (`truncate_place_to_len_and_update_capture_kind`).
//! - A multi-variant (or `#[non_exhaustive]`) enum read by a pattern is a shared borrow of the
//!   whole enum, so a binding of one variant's field merges into it; a wildcard reads nothing;
//!   a literal, range or constant pattern reads its place (`walk_pat`).
//! - Before Rust 2021 a closure captures whole bindings: each binding it mentions is merged in
//!   by reference, or by value for a `move` closure (`closure_analyze`, `!enable_precise_capture`,
//!   by the closure expression's own edition).
//!
//! THE ORDER, which is the order of the environment's fields and so of their drops: roots
//! (bindings) in the order a use of each is first met in the walk, which is source order; each
//! root's places stably sorted by their field indices, which is declaration order
//! (`compute_min_captures`'s final sort). Of the rules above, these change WHAT drops and WHEN:
//! the drop-type truncation (a value with `Drop` moves whole), the deref truncations (the
//! pointer is what is moved, its target never), the merge (an ancestor is moved whole), the 2018
//! rule (a binding is moved whole), and the order itself.
//!
//! - A BORROW (a `Copy` read included) of a field of a `repr(packed)` struct captures the struct,
//!   whatever the field's alignment; a by-value use keeps the field
//!   (`restrict_repr_packed_field_ref_capture`; the world's `TypeAttrs::PACKED`).
//!
//! What the walk cannot decide is a [`CaptureGap`] naming the expression or pattern, never a
//! guess: the owner's caller (rules/closure.rs `analyze_closures`) then keeps the whole-binding
//! proof and its gaps.

use core::cmp::Ordering;
use core::fmt;
use core::mem::DropGuard;

use blaze_own::{Alloc, Vec};

use crate::asm::AsmKind;
use crate::body::{BinOp, Expr, Pat, Stmt, UnOp};
use crate::columns::{AdjustKind, CaptureKind};
use crate::diag::At;
use crate::error::{CheckError, Limit, Malformed, Stop};
use crate::ids::{Edition, ExprId, ItemKey, LangItem, Mutbl, PatId};
use crate::machine::{Lower, Machine, RECURSION_LIMIT};
use crate::ty::{Cause, ClosureKind, FulfillErrorKind, Goal, Obligation, Ty, TyList, TyView, TypeCtx};
use crate::world::{Adt, AdtKind, Def, TypeAttrs, World};

/// How a place is captured, weakest first: a merge of two uses takes the stronger.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum PlaceMode {
    ImmBorrow,
    /// A mutable borrow whose `&mut` deref was cut off: the reference is borrowed uniquely
    /// but not mutably (rustc's `UniqueImmBorrow`). Stored as `&mut`.
    UniqueImmBorrow,
    MutBorrow,
    ByValue,
}

impl PlaceMode {
    fn of(m: Mutbl) -> PlaceMode {
        match m {
            Mutbl::Not => PlaceMode::ImmBorrow,
            Mutbl::Mut => PlaceMode::MutBorrow,
        }
    }

    /// The whole-binding row (`Checked::captures`) able to hold a place captured this way.
    pub(crate) fn row(self) -> CaptureKind {
        match self {
            PlaceMode::ImmBorrow => CaptureKind::ByRef,
            PlaceMode::UniqueImmBorrow | PlaceMode::MutBorrow => CaptureKind::ByMutRef,
            PlaceMode::ByValue => CaptureKind::ByValue,
        }
    }

    /// The closure kind a capture in this mode needs (upvar.rs: before the `move` rule).
    fn kind(self) -> ClosureKind {
        match self {
            PlaceMode::ImmBorrow => ClosureKind::Fn,
            PlaceMode::UniqueImmBorrow | PlaceMode::MutBorrow => ClosureKind::FnMut,
            PlaceMode::ByValue => ClosureKind::FnOnce,
        }
    }
}

/// One projection of a place. A slice pattern's elements and rest are `Index`: both stop the
/// place there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjKind {
    Deref,
    Field { variant: u32, field: u32 },
    Index,
}

/// A projection and the type it yields.
#[derive(Clone, Copy, Debug)]
struct Proj {
    kind: ProjKind,
    ty: Ty,
}

/// A place: a binding of an enclosing body and a run of projections in the walk's arena.
#[derive(Clone, Copy, Debug)]
struct PlaceRef {
    base: PatId,
    base_ty: Ty,
    start: u32,
    len: u32,
}

/// One captured place of a finished owner, in capture order.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CapturedPlace {
    /// The captured binding.
    pub base: PatId,
    base_ty: Ty,
    /// Its projections in [`CapturePlaces`]'s finished arena.
    start: u32,
    /// How many projections: 0 is the whole binding.
    pub len: u32,
    pub mode: PlaceMode,
    /// The environment field's type: the place's type `T`, `&T` or `&mut T` by `mode`.
    pub stored: Ty,
}

/// One owner's captures.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Precise {
    first: u32,
    end: u32,
    /// The closure's kind, from its uses (an `async` block's is unused).
    pub kind: ClosureKind,
    /// Whether the owner's own body awaits.
    pub suspends: bool,
}

/// What the walk could not decide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureGapKind {
    /// Whether the used place's type is `Copy` is still ambiguous.
    CopyUndecided,
    /// A field expression has no field row.
    FieldRow,
    /// A pattern's own type does not follow from its place's type by peeling references.
    PatternType,
    /// A path, tuple-struct or struct pattern has no resolution this rule reads.
    PatternResolution,
    /// A struct pattern names a field its variant lacks.
    PatternField,
    /// An expression or pattern the lowering could not make.
    Missing,
    /// A destructuring assignment.
    DestructuringAssign,
    /// A call whose callee is no fn, no closure of known kind, and no selected `Fn*` call.
    CallMode,
    /// A nested closure or `async` block whose own captures are a gap.
    Child(ExprId),
}

/// A use the place analysis cannot decide, by where it is.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaptureGap {
    pub at: At,
    pub kind: CaptureGapKind,
}

impl fmt::Display for CaptureGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            CaptureGapKind::CopyUndecided => "whether the used place's type is `Copy` is undecided",
            CaptureGapKind::FieldRow => "a field expression has no field row",
            CaptureGapKind::PatternType => "a pattern's type does not follow from its place's type",
            CaptureGapKind::PatternResolution => "a pattern's path has no resolution the capture rule reads",
            CaptureGapKind::PatternField => "a struct pattern names a field its variant lacks",
            CaptureGapKind::Missing => "an expression or pattern the lowering could not make",
            CaptureGapKind::DestructuringAssign => "a destructuring assignment",
            CaptureGapKind::CallMode => "a call whose callee's capture mode is unknown",
            CaptureGapKind::Child(c) => return write!(f, "the captured places of nested closure or async block #{} are a gap (at {:?})", c.0, self.at),
        };
        write!(f, "the captured places are undecided: {what} (at {:?})", self.at)
    }
}

/// Why one binding's captured places were widened to the whole binding ([`Machine::widen`]):
/// a precise place blaze-build does not lower (walk/moves/env_places.rs), captured whole
/// instead, as before RFC 2229. A typed decision, logged per owner and binding
/// ([`CapturePlaces::widened`]), never a refusal: the closure's kind, its environment, the
/// auto-trait witness and every reader of the filed places see the widened capture alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CaptureWiden {
    /// A pattern's scrutinee or a struct update's base is a place strictly above a captured
    /// place (not the shared reference whose target it holds): only parts of it are held.
    Above,
    /// A by-value use strictly below a by-value captured place: a move of a part of the field.
    MoveBelow,
    /// A pattern binding moves a by-value captured place out (only a plain `let x = place;`
    /// moves it as a value; a `match` owns a scrutinee that is a local or a temporary).
    PatternMove,
    /// A captured place below an enum variant's field.
    EnumField,
    /// A captured place through a `Box`'s contents.
    BoxDeref,
}

/// The walk's stop: the checker's, or a gap of this owner.
enum Halt {
    Stop(Stop),
    Gap(CaptureGap),
}

impl From<Stop> for Halt {
    fn from(s: Stop) -> Halt {
        Halt::Stop(s)
    }
}

impl From<CheckError> for Halt {
    fn from(e: CheckError) -> Halt {
        Halt::Stop(Stop::Error(e))
    }
}

fn gap(at: At, kind: CaptureGapKind) -> Halt {
    Halt::Gap(CaptureGap { at, kind })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Copyness {
    Copy,
    NotCopy,
    Undecided,
}

/// One step of the use walk (expr_use_visitor.rs's recursion, made a bounded stack).
#[derive(Clone, Copy, Debug)]
enum Task {
    /// `consume_expr`: the value is used (copied or moved), then walked.
    Consume(ExprId),
    /// `walk_expr`, with the mutability its place is used with (rustc fixes the overloaded
    /// derefs and indexes up to `DerefMut`/`IndexMut` before this walk; blaze's rules/
    /// mut_places.rs runs after it, so the walk carries the need itself).
    Walk(ExprId, Mutbl),
    /// `borrow_expr` (or `mutate_expr` for `MutBorrow`).
    Borrow(ExprId, PlaceMode),
    /// `walk_pat` of `pat` over the place `init` is; `plain_let`: a `let` statement's.
    Pat { pat: PatId, init: ExprId, guard: bool, plain_let: bool },
    /// The fields a struct update takes from its base (`walk_struct_expr`).
    Fru { expr: ExprId, base: ExprId },
}

/// A captured place being merged, with its mode.
#[derive(Clone, Copy, Debug)]
struct Cap {
    place: PlaceRef,
    mode: PlaceMode,
}


/// The analysis's indices and scratch, made once per body, and every owner's finished places.
pub(crate) struct CapturePlaces<'a> {
    /// `(expression, adjustment row)`, sorted: an expression's rows in order.
    adjust_ix: Vec<(ExprId, u32), Alloc<'a>>,
    /// `(field expression, variant, field index)`, sorted by expression.
    field_ix: Vec<(ExprId, u32, u32), Alloc<'a>>,
    /// Each pattern's resolution, sorted.
    pat_defs: Vec<(PatId, Def), Alloc<'a>>,
    /// Struct updates' taken fields, sorted by expression, each in declaration order.
    fru_ix: Vec<(ExprId, u32, Ty), Alloc<'a>>,
    /// Calls with a selected `Fn*` method, sorted.
    calls: Vec<ExprId, Alloc<'a>>,
    copy_cache: Vec<(Ty, Copyness), Alloc<'a>>,
    owned_box: Option<ItemKey>,
    copy_trait: Option<ItemKey>,
    drop_trait: Option<ItemKey>,
    bound: Vec<bool, Alloc<'a>>,
    mentioned: Vec<bool, Alloc<'a>>,
    sweep: Vec<(ExprId, bool), Alloc<'a>>,
    exprs: Vec<ExprId, Alloc<'a>>,
    pats: Vec<PatId, Alloc<'a>>,
    tasks: Vec<Task, Alloc<'a>>,
    needs: Vec<Mutbl, Alloc<'a>>,
    chain: Vec<ExprId, Alloc<'a>>,
    pat_stack: Vec<(PatId, PlaceRef, u32), Alloc<'a>>,
    arena: Vec<Proj, Alloc<'a>>,
    uses: Vec<(PlaceRef, PlaceMode), Alloc<'a>>,
    /// The owner's places a pattern is matched against or a struct update takes fields of.
    scrutinees: Vec<PlaceRef, Alloc<'a>>,
    /// The owner's places a pattern binding moves out (not a plain `let x = place;`).
    pat_moves: Vec<PlaceRef, Alloc<'a>>,
    caps: Vec<Cap, Alloc<'a>>,
    roots: Vec<PatId, Alloc<'a>>,
    /// Every widening of the body's owners, in the order made: owner, binding, why.
    widened: Vec<(ExprId, PatId, CaptureWiden), Alloc<'a>>,
    done_arena: Vec<Proj, Alloc<'a>>,
    done_places: Vec<CapturedPlace, Alloc<'a>>,
    /// Each analysed owner's result, sorted by owner.
    done: Vec<(ExprId, Result<Precise, CaptureGap>), Alloc<'a>>,
}

impl<'a> CapturePlaces<'a> {
    /// The places of a finished owner, in capture order.
    pub(crate) fn places(&self, p: Precise) -> &[CapturedPlace] {
        &self.done_places[p.first as usize..p.end as usize]
    }

    /// Every binding an owner of the body captures whole although its uses name places below
    /// it, and why: owner, binding, reason, in the order the owners were analysed.
    pub(crate) fn widened(&self) -> &[(ExprId, PatId, CaptureWiden)] {
        &self.widened
    }

    fn result(&self, owner: ExprId) -> Option<Result<Precise, CaptureGap>> {
        self.done.binary_search_by_key(&owner, |d| d.0).ok().map(|i| self.done[i].1)
    }

    fn rows(&self, e: ExprId) -> core::ops::Range<usize> {
        let start = self.adjust_ix.partition_point(|r| r.0 < e);
        let end = self.adjust_ix.partition_point(|r| r.0 <= e);
        start..end
    }

    fn field(&self, e: ExprId) -> Option<(u32, u32)> {
        let i = self.field_ix.partition_point(|r| r.0 < e);
        self.field_ix.get(i).filter(|r| r.0 == e).map(|r| (r.1, r.2))
    }

    fn pat_def(&self, p: PatId) -> Option<Def> {
        let i = self.pat_defs.partition_point(|r| r.0 < p);
        self.pat_defs.get(i).filter(|r| r.0 == p).map(|r| r.1)
    }

    fn proj(&self, place: PlaceRef, i: u32) -> Proj {
        self.arena[(place.start + i) as usize]
    }

    /// The type before projection `i` (the base's for 0).
    fn ty_before(&self, place: PlaceRef, i: u32) -> Ty {
        if i == 0 { place.base_ty } else { self.proj(place, i - 1).ty }
    }

    fn place_ty(&self, place: PlaceRef) -> Ty {
        self.ty_before(place, place.len)
    }

    /// `place` with one more projection. The run is copied when another place was made after
    /// it, so every place stays one contiguous run.
    fn extend(&mut self, place: PlaceRef, kind: ProjKind, ty: Ty, limit: usize) -> Result<PlaceRef, Halt> {
        if self.arena.len() + place.len as usize + 1 > limit {
            return Err(CheckError::Limit(Limit::Steps).into());
        }
        let start = if (place.start + place.len) as usize == self.arena.len() {
            place.start
        } else {
            let start = self.arena.len() as u32;
            for i in 0..place.len {
                let p = self.proj(place, i);
                self.arena.push(p);
            }
            start
        };
        self.arena.push(Proj { kind, ty });
        Ok(PlaceRef { base: place.base, base_ty: place.base_ty, start, len: place.len + 1 })
    }

    fn first(&self, place: PlaceRef, kind: ProjKind) -> Option<u32> {
        (0..place.len).find(|&i| self.proj(place, i).kind == kind)
    }

    /// How `a` relates to `b`: `Some(Ordering::Less)` an ancestor, `Equal` the same place,
    /// `Greater` a descendant, `None` divergent.
    fn relation(&self, a: PlaceRef, b: PlaceRef) -> Option<Ordering> {
        if a.base != b.base {
            return None;
        }
        let n = a.len.min(b.len);
        if (0..n).any(|i| self.proj(a, i).kind != self.proj(b, i).kind) {
            return None;
        }
        Some(a.len.cmp(&b.len))
    }

    /// The finished places' order within one root: by field indices (upvar.rs's sort).
    fn field_order(&self, a: CapturedPlace, b: CapturedPlace) -> Ordering {
        let fields = |c: CapturedPlace| {
            self.done_arena[c.start as usize..(c.start + c.len) as usize].iter().filter_map(|p| match p.kind {
                ProjKind::Field { field, .. } => Some(field),
                ProjKind::Deref | ProjKind::Index => None,
            })
        };
        for (x, y) in fields(a).zip(fields(b)) {
            if x != y {
                return x.cmp(&y);
            }
        }
        Ordering::Equal
    }
}

/// rules/pat.rs `peeled_mode`: the default binding mode after peeling a reference (0 move,
/// 1 `ref`, 2 `ref mut`).
fn peeled_mode(mode: u32, m: Mutbl) -> u32 {
    match (m, mode) {
        (Mutbl::Not, _) => 1,
        (Mutbl::Mut, 0) => 2,
        (Mutbl::Mut, other) => other,
    }
}

/// rules/pat.rs `push_sub_pats`: element `i` of `len` written ones, around a `..` at `rest`, is
/// field `n - (len - i)` past the rest.
fn sub_index(i: usize, len: usize, rest: Option<u32>, n: usize) -> Option<usize> {
    match rest {
        Some(r) if i >= r as usize => n.checked_sub(len - i),
        _ => Some(i),
    }
}

impl<'b, 'w, 'c, 'a, W: World, C: TypeCtx> Machine<'b, 'w, 'c, 'a, W, C> {
    /// The indices the analysis reads, once per body, after selection.
    pub(crate) fn capture_places_new(&mut self) -> Result<CapturePlaces<'a>, Stop> {
        let alloc = self.alloc;
        let rows = self.adjust.column::<0>();
        let mut adjust_ix = Vec::with_capacity_in(rows.len(), alloc);
        for (row, &e) in rows.iter().enumerate() {
            adjust_ix.push((e, row as u32));
        }
        adjust_ix.sort_unstable();
        let mut field_ix = Vec::with_capacity_in(self.fields.len(), alloc);
        field_ix.extend_from_slice(&self.fields);
        field_ix.sort_unstable_by_key(|r| r.0);
        let mut pat_defs = Vec::new_in(alloc);
        for &(at, def) in self.defs.iter() {
            if let At::Pat(p) = at {
                pat_defs.push((p, def));
            }
        }
        pat_defs.sort_unstable_by_key(|r| r.0);
        let mut fru_ix = Vec::with_capacity_in(self.fru_fields.len(), alloc);
        fru_ix.extend_from_slice(&self.fru_fields);
        // Within one struct update, by field index: declaration order.
        fru_ix.sort_unstable_by_key(|r| (r.0, r.1));
        let mut calls = Vec::with_capacity_in(self.callees.len(), alloc);
        calls.extend(self.callees.iter().map(|r| r.0));
        calls.sort_unstable();
        Ok(CapturePlaces {
            adjust_ix,
            field_ix,
            pat_defs,
            fru_ix,
            calls,
            copy_cache: Vec::new_in(alloc),
            owned_box: self.lang(LangItem::OwnedBox)?,
            copy_trait: self.lang(LangItem::Copy)?,
            drop_trait: self.lang(LangItem::Drop)?,
            bound: Vec::new_in(alloc),
            mentioned: Vec::new_in(alloc),
            sweep: Vec::new_in(alloc),
            exprs: Vec::new_in(alloc),
            pats: Vec::new_in(alloc),
            tasks: Vec::new_in(alloc),
            needs: Vec::new_in(alloc),
            chain: Vec::new_in(alloc),
            pat_stack: Vec::new_in(alloc),
            arena: Vec::new_in(alloc),
            uses: Vec::new_in(alloc),
            scrutinees: Vec::new_in(alloc),
            pat_moves: Vec::new_in(alloc),
            caps: Vec::new_in(alloc),
            roots: Vec::new_in(alloc),
            widened: Vec::new_in(alloc),
            done_arena: Vec::new_in(alloc),
            done_places: Vec::new_in(alloc),
            done: Vec::new_in(alloc),
        })
    }

    /// The captured places of `owner` (a closure or `async` block whose nested owners were
    /// analysed first), filed in `cp` for its parent. `async_fn_root`: the root block of an
    /// `async fn`, which moves every argument in, whole and in order.
    pub(crate) fn capture_places(
        &mut self,
        cp: &mut CapturePlaces<'a>,
        owner: ExprId,
        async_fn_root: bool,
    ) -> Result<Result<Precise, CaptureGap>, Stop> {
        let got = match self.places_of(cp, owner, async_fn_root) {
            Ok(p) => Ok(p),
            Err(Halt::Gap(g)) => Err(g),
            Err(Halt::Stop(s)) => return Err(s),
        };
        let at = cp.done.partition_point(|d| d.0 < owner);
        cp.done.insert_shift(at, (owner, got));
        Ok(got)
    }

    /// Sets `owner`'s whole-binding rows (`self.captures[first..]`, the syntactic walk's, one
    /// per binding mentioned) from its places: a binding with places gets the strongest mode
    /// any of them is captured by. A binding mentioned with no place (`let _ = x;` captures
    /// nothing in Rust 2021) keeps the walk's row: the layout still names it, and the builder
    /// refuses a mentioned binding with no row (blaze-build coroutine.rs); [`Self::places_tuple`]
    /// counts it when that row moves it. Answers whether every row is exactly one whole-binding
    /// place in the row's mode.
    pub(crate) fn rows_from_places(&mut self, cp: &CapturePlaces<'a>, owner: ExprId, first: usize, p: Precise) -> bool {
        let places = cp.places(p);
        let strongest = |base: PatId| places.iter().filter(|c| c.base == base).map(|c| c.mode.row()).max();
        let mut whole = places.iter().all(|c| c.len == 0);
        for r in first..self.captures.len() {
            match strongest(self.captures[r].1) {
                Some(mode) => self.captures[r].2 = mode,
                None => whole = false,
            }
        }
        for c in places {
            if !self.captures[first..].iter().any(|r| r.1 == c.base) {
                let mode = strongest(c.base).unwrap_or(c.mode.row());
                self.captures.push((owner, c.base, mode));
            }
        }
        self.captures[first..].sort_unstable_by_key(|r| r.1);
        whole
    }

    /// The environment's witness tuple: the places' stored types in capture order (the
    /// Reference's environment), then, for each binding whose row is by value although its
    /// places are not exactly the whole binding by value, that binding's type: a row by value
    /// is laid out as the whole binding moved, which is no superset of its places (a row by
    /// reference is: the reference reaches only the places the body uses). Until the layout holds one field per place (blaze-build layout.rs
    /// `env_holds`), the environment really holds those values, so a `Yes` of an auto trait,
    /// `Copy` or `Clone` over this tuple holds for what the closure carries; a `No` resting
    /// only on such an extra is the layout's, not the Reference's (owed with the layout).
    pub(crate) fn places_tuple(&mut self, cp: &CapturePlaces<'a>, first: usize, p: Precise) -> Result<Ty, Stop> {
        let places = cp.places(p);
        let mut tys: Vec<Ty, Alloc<'a>> = Vec::with_capacity_in(places.len(), self.alloc);
        tys.extend(places.iter().map(|c| c.stored));
        for r in first..self.captures.len() {
            let (_, base, kind) = self.captures[r];
            if kind != CaptureKind::ByValue {
                continue;
            }
            let mut of = places.iter().filter(|c| c.base == base);
            let exact = matches!((of.next(), of.next()), (Some(c), None) if c.len == 0 && c.mode == PlaceMode::ByValue);
            if !exact {
                tys.push(self.local_ty[base.ix()]);
            }
        }
        let list = self.mk_list(&tys)?;
        self.mk(TyView::Tuple(list))
    }

    /// Files `owner`'s decided places as rows beside its whole-binding rows
    /// (`capture_place_rows`, `capture_proj_rows`; written back as `Checked::capture_places` and
    /// `Checked::capture_projs`): one row per place in capture order, the environment's field
    /// order (the Reference, "Closure types": one field per captured place).
    pub(crate) fn file_places(&mut self, cp: &CapturePlaces<'a>, owner: ExprId, p: Precise) {
        for c in cp.places(p) {
            let start = self.capture_proj_rows.len() as u32;
            for i in 0..c.len {
                let pr = cp.done_arena[(c.start + i) as usize];
                let (k, v, f) = match pr.kind {
                    ProjKind::Deref => (0u8, 0, 0),
                    ProjKind::Field { variant, field } => (1u8, variant, field),
                    ProjKind::Index => (2u8, 0, 0),
                };
                self.capture_proj_rows.push((k, v, f, pr.ty));
            }
            self.capture_place_rows.push((owner, c.base, start, c.len, c.mode.row(), c.stored));
        }
    }

    fn places_of(&mut self, cp: &mut CapturePlaces<'a>, owner: ExprId, async_fn_root: bool) -> Result<Precise, Halt> {
        let body = match self.body.expr(owner)? {
            Expr::Closure { body, .. } | Expr::AsyncBlock { body, .. } => body,
            _ => return Err(CheckError::Malformed(Malformed::Expr(owner)).into()),
        };
        let is_move = self.captures_by_move(owner)?;
        let suspends = self.mark_owner_bound(cp, owner)?;
        cp.arena.clear();
        cp.uses.clear();
        cp.scrutinees.clear();
        cp.pat_moves.clear();
        cp.caps.clear();
        cp.roots.clear();
        cp.tasks.clear();
        let mut kind = ClosureKind::Fn;
        if async_fn_root {
            // An async fn's lowering moves each argument into the root block, in order.
            for &p in self.body.params {
                let base_ty = self.ty.resolve(self.local_ty[p.ix()])?;
                let place = PlaceRef { base: p, base_ty, start: cp.arena.len() as u32, len: 0 };
                self.merge(cp, place, PlaceMode::ByValue);
            }
        } else {
            // `consume_body`: the parameters are the owner's own; its value is consumed.
            cp.tasks.push(Task::Consume(body));
            self.run_tasks(cp)?;
            kind = self.settle_uses(cp, is_move)?;
            if self.body.expr_edition(owner) < Edition::E2021 {
                self.whole_bindings(cp, owner, is_move)?;
            }
            self.widen(cp, owner)?;
        }
        self.finish(cp, kind, suspends)
    }

    /// Marks every binding under `owner` (its parameters and nested owners' included) as its
    /// own; answers whether `owner`'s own body awaits (not a nested owner's).
    fn mark_owner_bound(&mut self, cp: &mut CapturePlaces<'a>, owner: ExprId) -> Result<bool, Halt> {
        cp.bound.clear();
        cp.bound.resize(self.body.pats.len(), false);
        cp.sweep.clear();
        cp.sweep.push((owner, false));
        let mut suspends = false;
        let mut steps = 0usize;
        while let Some((e, nested)) = cp.sweep.pop() {
            steps += 1;
            if steps > self.body.exprs.len() + 1 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let node = self.body.expr(e)?;
            if !nested && matches!(node, Expr::Await(_)) {
                suspends = true;
            }
            let inner = nested || (e != owner && matches!(node, Expr::AsyncBlock { .. } | Expr::Closure { .. } | Expr::ConstBlock { .. }));
            cp.exprs.clear();
            cp.pats.clear();
            self.expr_children(e, &mut cp.exprs, &mut cp.pats)?;
            self.mark_bound(&mut cp.pats, &mut cp.bound)?;
            for &c in cp.exprs.iter().rev() {
                cp.sweep.push((c, inner));
            }
        }
        Ok(suspends)
    }

    /// Before Rust 2021: every binding of an enclosing body the owner mentions (nested owners
    /// included), in source order, is captured whole, by value for a `move` owner and by shared
    /// borrow otherwise, merged with what the uses found.
    fn whole_bindings(&mut self, cp: &mut CapturePlaces<'a>, owner: ExprId, is_move: bool) -> Result<(), Halt> {
        cp.mentioned.clear();
        cp.mentioned.resize(self.body.pats.len(), false);
        cp.sweep.clear();
        cp.sweep.push((owner, false));
        let mode = if is_move { PlaceMode::ByValue } else { PlaceMode::ImmBorrow };
        let mut steps = 0usize;
        while let Some((e, _)) = cp.sweep.pop() {
            steps += 1;
            if steps > self.body.exprs.len() + 1 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            if let Expr::Local(p) = self.body.expr(e)?
                && !cp.bound.get(p.ix()).copied().unwrap_or(true)
                && !cp.mentioned[p.ix()]
            {
                cp.mentioned[p.ix()] = true;
                let base_ty = self.ty.resolve(self.local_ty[p.ix()])?;
                let place = PlaceRef { base: p, base_ty, start: cp.arena.len() as u32, len: 0 };
                self.merge(cp, place, mode);
            }
            cp.exprs.clear();
            cp.pats.clear();
            self.expr_children(e, &mut cp.exprs, &mut cp.pats)?;
            for &c in cp.exprs.iter().rev() {
                cp.sweep.push((c, false));
            }
        }
        Ok(())
    }

    /// The cap on the projections the walk makes: a place is made per use, each as long as its
    /// expression chain, so a chain of `d` adjusted links costs `d * d`; the cap allows that for
    /// any chain a body holds and stops a runaway.
    fn arena_cap(&self) -> usize {
        self.walk_limit().saturating_mul(64).max(1 << 20)
    }

    fn walk_limit(&self) -> usize {
        8 * (self.body.exprs.len() + self.body.pats.len() + self.body.stmts.len()) + 64
    }

    fn run_tasks(&mut self, cp: &mut CapturePlaces<'a>) -> Result<(), Halt> {
        let limit = self.walk_limit();
        let mut steps = 0usize;
        while let Some(task) = cp.tasks.pop() {
            steps += 1;
            if steps > limit || cp.tasks.len() > limit {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            match task {
                Task::Consume(e) => {
                    let place = self.cat(cp, e, true)?;
                    self.consume_or_copy(cp, place, At::Expr(e))?;
                    self.walk(cp, e, Mutbl::Not)?;
                }
                Task::Walk(e, need) => self.walk(cp, e, need)?,
                Task::Borrow(e, mode) => {
                    let place = self.cat(cp, e, true)?;
                    if let Some(place) = place {
                        self.borrow(cp, place, mode);
                    }
                    let need = if mode >= PlaceMode::UniqueImmBorrow { Mutbl::Mut } else { Mutbl::Not };
                    self.walk(cp, e, need)?;
                }
                Task::Pat { pat, init, guard, plain_let } => {
                    if let Some(place) = self.cat(cp, init, true)? {
                        cp.scrutinees.push(place);
                        self.walk_pat(cp, pat, place, guard, plain_let)?;
                    }
                }
                Task::Fru { expr, base } => self.fru(cp, expr, base)?,
            }
        }
        Ok(())
    }

    /// `walk_expr`: the adjustments' uses, then the sub-expressions as tasks (pushed last
    /// first, so they run in source order).
    fn walk(&mut self, cp: &mut CapturePlaces<'a>, e: ExprId, need: Mutbl) -> Result<(), Halt> {
        self.walk_adjustment(cp, e, need)?;
        let b = self.body;
        match b.expr(e)? {
            Expr::Missing => return Err(gap(At::Expr(e), CaptureGapKind::Missing)),
            Expr::Lit(_) | Expr::Local(_) | Expr::Path(_) | Expr::Continue { .. } | Expr::Underscore | Expr::OffsetOf { .. } => {}
            // An inline constant sees no local of the body.
            Expr::ConstBlock { .. } => {}
            Expr::Paren(i) => cp.tasks.push(Task::Walk(i, need)),
            Expr::Field { base, .. } | Expr::TupleField { base, .. } => cp.tasks.push(Task::Walk(base, need)),
            Expr::Unary { op: UnOp::Deref, operand } => cp.tasks.push(Task::Walk(operand, need)),
            Expr::Unary { operand, .. } | Expr::Cast { operand, .. } | Expr::Await(operand) | Expr::Loop { body: operand } => {
                cp.tasks.push(Task::Consume(operand));
            }
            Expr::Repeat { elem, .. } => cp.tasks.push(Task::Consume(elem)),
            Expr::Break { value, .. } | Expr::Return { value } => cp.tasks.extend(value.map(Task::Consume)),
            // A comparison takes its operands by reference (rustc's autoref of `PartialEq` and
            // `PartialOrd`, present when the closure is analysed); the others by value.
            Expr::Binary { op, lhs, rhs } => {
                if matches!(op, BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge) {
                    cp.tasks.push(Task::Borrow(rhs, PlaceMode::ImmBorrow));
                    cp.tasks.push(Task::Borrow(lhs, PlaceMode::ImmBorrow));
                } else {
                    cp.tasks.push(Task::Consume(rhs));
                    cp.tasks.push(Task::Consume(lhs));
                }
            }
            Expr::Assign { lhs, rhs } => match b.expr(lhs)? {
                // `_ = rhs` is `let _ = rhs;`: the right side is walked, not consumed.
                Expr::Underscore => cp.tasks.push(Task::Walk(rhs, Mutbl::Not)),
                Expr::Tuple { .. } | Expr::Array { .. } | Expr::Struct { .. } | Expr::Call { .. } => {
                    return Err(gap(At::Expr(e), CaptureGapKind::DestructuringAssign));
                }
                _ => {
                    cp.tasks.push(Task::Consume(rhs));
                    cp.tasks.push(Task::Borrow(lhs, PlaceMode::MutBorrow));
                }
            },
            // Builtin: the place is mutated; overloaded: borrowed `&mut` by the operator's autoref.
            Expr::AssignOp { lhs, rhs, .. } => {
                cp.tasks.push(Task::Consume(rhs));
                cp.tasks.push(Task::Borrow(lhs, PlaceMode::MutBorrow));
            }
            Expr::Ref { mutbl, operand } | Expr::RawRef { mutbl, operand } => cp.tasks.push(Task::Borrow(operand, PlaceMode::of(mutbl))),
            Expr::Tuple { elems } | Expr::Array { elems } => {
                for &x in b.expr_list(elems)?.iter().rev() {
                    cp.tasks.push(Task::Consume(x));
                }
            }
            Expr::Struct { fields, base, .. } | Expr::Record { fields, base } => {
                if let Some(base) = base.expr() {
                    cp.tasks.push(Task::Walk(base, Mutbl::Not));
                    cp.tasks.push(Task::Fru { expr: e, base });
                }
                for fi in b.field_init_list(fields)?.iter().rev() {
                    cp.tasks.push(Task::Consume(fi.expr));
                }
            }
            Expr::Index { base, index } => {
                cp.tasks.push(Task::Consume(index));
                if self.place_index_is_builtin(cp, base, index)? {
                    cp.tasks.push(Task::Walk(base, need));
                } else {
                    // `Index::index(&base, i)`: the base is borrowed, mutably where the place is
                    // used mutably (`IndexMut`).
                    cp.tasks.push(Task::Borrow(base, PlaceMode::of(need)));
                }
            }
            Expr::Call { callee, args } => {
                for &x in b.expr_list(args)?.iter().rev() {
                    cp.tasks.push(Task::Consume(x));
                }
                let t = self.ty.resolve(self.expr_ty[callee.ix()])?;
                let task = if cp.calls.binary_search(&e).is_ok() || matches!(self.view(t), TyView::FnDef(..) | TyView::FnPtr(..)) {
                    // A selected `Fn*` call's autoref is the callee's adjustment row.
                    Task::Consume(callee)
                } else {
                    match self.callee_capture_use(e, callee) {
                        Some(0) => Task::Borrow(callee, PlaceMode::ImmBorrow),
                        Some(1) => Task::Borrow(callee, PlaceMode::MutBorrow),
                        Some(_) => Task::Consume(callee),
                        None => return Err(gap(At::Expr(e), CaptureGapKind::CallMode)),
                    }
                };
                cp.tasks.push(task);
            }
            Expr::MethodCall { receiver, args, .. } => {
                for &x in b.expr_list(args)?.iter().rev() {
                    cp.tasks.push(Task::Consume(x));
                }
                cp.tasks.push(Task::Consume(receiver));
            }
            Expr::Block { stmts, tail, .. } => {
                cp.tasks.extend(tail.map(Task::Consume));
                for &s in b.stmt_list(stmts)?.iter().rev() {
                    match b.stmt(s)? {
                        // `walk_local`: the initializer is walked, its place matched, then the
                        // `else` block.
                        Stmt::Let { pat, init: Some(init), else_, .. } => {
                            cp.tasks.extend(else_.map(Task::Consume));
                            cp.tasks.push(Task::Pat { pat, init, guard: false, plain_let: true });
                            cp.tasks.push(Task::Walk(init, Mutbl::Not));
                        }
                        Stmt::Let { init: None, .. } | Stmt::Item => {}
                        Stmt::Expr { expr, .. } => cp.tasks.push(Task::Consume(expr)),
                    }
                }
            }
            Expr::If { cond, then, else_ } => {
                cp.tasks.extend(else_.map(Task::Consume));
                cp.tasks.push(Task::Consume(then));
                cp.tasks.push(Task::Consume(cond));
            }
            Expr::Let { pat, init } => {
                cp.tasks.push(Task::Pat { pat, init, guard: false, plain_let: false });
                cp.tasks.push(Task::Walk(init, Mutbl::Not));
            }
            Expr::Match { scrutinee, arms } => {
                for arm in b.arm_list(arms)?.iter().rev() {
                    cp.tasks.push(Task::Consume(arm.body));
                    cp.tasks.extend(arm.guard.map(Task::Consume));
                    cp.tasks.push(Task::Pat { pat: arm.pat, init: scrutinee, guard: arm.guard.is_some(), plain_let: false });
                }
                cp.tasks.push(Task::Walk(scrutinee, Mutbl::Not));
            }
            // `format_args!` takes each argument by shared reference.
            Expr::FormatArgs { args, .. } => {
                for &x in b.expr_list(args)?.iter().rev() {
                    cp.tasks.push(Task::Borrow(x, PlaceMode::ImmBorrow));
                }
            }
            Expr::Asm { operands, .. } => {
                for o in b.asm_operand_list(operands)?.iter().rev() {
                    match (o.kind, o.out_expr) {
                        (AsmKind::In | AsmKind::Label, _) => cp.tasks.push(Task::Consume(o.expr)),
                        (AsmKind::InOut | AsmKind::InLateOut, Some(out)) => {
                            cp.tasks.push(Task::Borrow(out, PlaceMode::MutBorrow));
                            cp.tasks.push(Task::Consume(o.expr));
                        }
                        (AsmKind::Out | AsmKind::LateOut | AsmKind::InOut | AsmKind::InLateOut, _) => {
                            cp.tasks.push(Task::Borrow(o.expr, PlaceMode::MutBorrow));
                        }
                        (AsmKind::Sym | AsmKind::Const, _) => {}
                    }
                }
            }
            Expr::Closure { .. } | Expr::AsyncBlock { .. } => self.replay_child(cp, e)?,
        }
        Ok(())
    }

    /// `walk_captures`: a nested owner's places that are this owner's captures too, each
    /// consumed (by value) or borrowed in its own mode.
    fn replay_child(&mut self, cp: &mut CapturePlaces<'a>, child: ExprId) -> Result<(), Halt> {
        let p = match cp.result(child) {
            Some(Ok(p)) => p,
            Some(Err(_)) => return Err(gap(At::Expr(child), CaptureGapKind::Child(child))),
            None => return Err(CheckError::Malformed(Malformed::Expr(child)).into()),
        };
        let limit = self.arena_cap();
        for i in p.first..p.end {
            let c = cp.done_places[i as usize];
            if cp.bound.get(c.base.ix()).copied().unwrap_or(true) {
                continue;
            }
            if cp.arena.len() + c.len as usize > limit {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let start = cp.arena.len() as u32;
            for j in 0..c.len {
                let proj = cp.done_arena[(c.start + j) as usize];
                cp.arena.push(proj);
            }
            let place = PlaceRef { base: c.base, base_ty: c.base_ty, start, len: c.len };
            match c.mode {
                PlaceMode::ByValue => self.consume_or_copy(cp, Some(place), At::Expr(child))?,
                mode => self.borrow(cp, place, mode),
            }
        }
        Ok(())
    }

    /// Whether `base[index]` is the builtin index of an array or slice by `usize` (rules/
    /// field.rs `builtin_index`), not `Index::index`.
    fn place_index_is_builtin(&mut self, cp: &CapturePlaces<'a>, base: ExprId, index: ExprId) -> Result<bool, Stop> {
        let rows = cp.rows(base);
        let t = match rows.end.checked_sub(1).filter(|&last| last >= rows.start) {
            Some(last) => self.adjust.column::<2>()[cp.adjust_ix[last].1 as usize],
            None => self.expr_ty[base.ix()],
        };
        let t = self.ty.resolve(t)?;
        let i = self.ty.resolve(self.expr_ty[index.ix()])?;
        Ok(matches!(self.view(t), TyView::Array(..) | TyView::Slice(_))
            && matches!(self.view(i), TyView::Uint(crate::ids::UintTy::Usize)))
    }

    /// `cat_expr`: the place `e` is (after its adjustments when `adjusted`), when that place is
    /// a binding of an enclosing body or a projection of one; `None` for every other value.
    fn cat(&mut self, cp: &mut CapturePlaces<'a>, e: ExprId, adjusted: bool) -> Result<Option<PlaceRef>, Halt> {
        cp.chain.clear();
        let mut x = e;
        let leaf = loop {
            if cp.chain.len() > self.body.exprs.len() {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            cp.chain.push(x);
            match self.body.expr(x)? {
                Expr::Paren(i) => x = i,
                Expr::Field { base, .. } | Expr::TupleField { base, .. } | Expr::Index { base, .. } => x = base,
                Expr::Unary { op: UnOp::Deref, operand } => x = operand,
                Expr::Local(p) => break Some(p),
                _ => break None,
            }
        };
        let Some(p) = leaf else { return Ok(None) };
        if cp.bound.get(p.ix()).copied().unwrap_or(true) {
            return Ok(None);
        }
        let limit = self.arena_cap();
        let base_ty = self.ty.resolve(self.local_ty[p.ix()])?;
        let mut place = PlaceRef { base: p, base_ty, start: cp.arena.len() as u32, len: 0 };
        let n = cp.chain.len();
        for k in (0..n).rev() {
            let x = cp.chain[k];
            if k + 1 < n {
                let child = cp.chain[k + 1];
                match self.body.expr(x)? {
                    Expr::Paren(_) => {}
                    Expr::Field { .. } | Expr::TupleField { .. } => {
                        let Some((variant, field)) = cp.field(x) else {
                            return Err(gap(At::Expr(x), CaptureGapKind::FieldRow));
                        };
                        let ty = self.ty.resolve(self.expr_ty[x.ix()])?;
                        place = cp.extend(place, ProjKind::Field { variant, field }, ty, limit)?;
                    }
                    Expr::Unary { .. } => {
                        // An overloaded `*` is its operand's last row (rules/op.rs), applied
                        // with the operand's rows below.
                        let rows = cp.rows(child);
                        let overloaded = rows.end > rows.start
                            && matches!(self.adjust.column::<1>()[cp.adjust_ix[rows.end - 1].1 as usize], AdjustKind::Deref { overloaded: Some(_) });
                        if !overloaded {
                            let ty = self.ty.resolve(self.expr_ty[x.ix()])?;
                            place = cp.extend(place, ProjKind::Deref, ty, limit)?;
                        }
                    }
                    Expr::Index { index, .. } => {
                        if !self.place_index_is_builtin(cp, child, index)? {
                            return Ok(None);
                        }
                        let ty = self.ty.resolve(self.expr_ty[x.ix()])?;
                        place = cp.extend(place, ProjKind::Index, ty, limit)?;
                    }
                    _ => return Err(CheckError::Malformed(Malformed::Expr(x)).into()),
                }
            }
            if k > 0 || adjusted {
                match self.apply_rows(cp, x, place, limit)? {
                    Some(next) => place = next,
                    None => return Ok(None),
                }
            }
        }
        Ok(Some(place))
    }

    /// `cat_expr_adjusted`: `x`'s adjustment rows over `place`; `None` once a row makes an
    /// rvalue (a borrow, an overloaded deref, a coercion).
    fn apply_rows(&mut self, cp: &mut CapturePlaces<'a>, x: ExprId, place: PlaceRef, limit: usize) -> Result<Option<PlaceRef>, Halt> {
        let mut place = place;
        let mut before = self.ty.resolve(self.expr_ty[x.ix()])?;
        for r in cp.rows(x) {
            let row = cp.adjust_ix[r].1 as usize;
            let kind = self.adjust.column::<1>()[row];
            let target = self.ty.resolve(self.adjust.column::<2>()[row])?;
            match kind {
                AdjustKind::Deref { overloaded } => match (self.view(before), self.view(target), overloaded) {
                    // The slice step of an array (rules/field.rs): the same place, unsized.
                    (TyView::Array(..), TyView::Slice(_), None) => {}
                    (TyView::Ref(..) | TyView::Ptr(..), _, None) => place = cp.extend(place, ProjKind::Deref, target, limit)?,
                    // `Box`'s deref is builtin (the place `*b`), whatever the row says.
                    (TyView::Adt(k, _), _, Some(_)) if Some(k) == cp.owned_box => {
                        place = cp.extend(place, ProjKind::Deref, target, limit)?;
                    }
                    (_, _, Some(_)) => return Ok(None),
                    (_, _, None) => return Err(CheckError::Malformed(Malformed::Expr(x)).into()),
                },
                _ => return Ok(None),
            }
            before = target;
        }
        Ok(Some(place))
    }

    /// `walk_adjustment`: the uses `e`'s adjustment rows make of its place. `need` is how the
    /// adjusted value's place is used; read backwards, a borrow row sets it for the rows before,
    /// a deref passes it on (rustc's `convert_place_derefs_to_mutable`), any other row clears it.
    fn walk_adjustment(&mut self, cp: &mut CapturePlaces<'a>, e: ExprId, need: Mutbl) -> Result<(), Halt> {
        let rows = cp.rows(e);
        if rows.is_empty() {
            return Ok(());
        }
        cp.needs.clear();
        let mut cur = need;
        for r in rows.clone().rev() {
            cp.needs.push(cur);
            cur = match self.adjust.column::<1>()[cp.adjust_ix[r].1 as usize] {
                AdjustKind::Borrow { mutbl, .. } => mutbl,
                AdjustKind::Deref { .. } => cur,
                _ => Mutbl::Not,
            };
        }
        cp.needs.reverse();
        let limit = self.arena_cap();
        let mut place = self.cat(cp, e, false)?;
        let mut before = self.ty.resolve(self.expr_ty[e.ix()])?;
        for (i, r) in rows.enumerate() {
            let row = cp.adjust_ix[r].1 as usize;
            let kind = self.adjust.column::<1>()[row];
            let target = self.ty.resolve(self.adjust.column::<2>()[row])?;
            match kind {
                AdjustKind::NeverToAny
                | AdjustKind::ReifyFnPointer
                | AdjustKind::ClosureFnPointer
                | AdjustKind::UnsafeFnPointer
                | AdjustKind::MutToConstPointer
                | AdjustKind::Unsize
                | AdjustKind::UnionInject { .. }
                | AdjustKind::UnionWiden
                | AdjustKind::RecordProject => {
                    // Creating a fn pointer or unsizing consumes the value into an rvalue.
                    self.consume_or_copy(cp, place, At::Expr(e))?;
                    place = None;
                }
                AdjustKind::Deref { overloaded } => {
                    let builtin = match (self.view(before), overloaded) {
                        (TyView::Array(..), None) if matches!(self.view(target), TyView::Slice(_)) => None,
                        (_, None) => Some(true),
                        (TyView::Adt(k, _), Some(_)) if Some(k) == cp.owned_box => Some(true),
                        (_, Some(_)) => Some(false),
                    };
                    match (builtin, place) {
                        (None, _) | (_, None) => {}
                        (Some(true), Some(p)) => place = Some(cp.extend(p, ProjKind::Deref, target, limit)?),
                        (Some(false), Some(p)) => {
                            // `Deref::deref(&place)` (or `deref_mut` where the result is used
                            // mutably): an autoref of the place.
                            let m = overloaded.unwrap_or(Mutbl::Not).max(cp.needs[i]);
                            self.borrow(cp, p, PlaceMode::of(m));
                            place = None;
                        }
                    }
                }
                AdjustKind::Borrow { mutbl, .. } => {
                    if let Some(p) = place {
                        self.borrow(cp, p, PlaceMode::of(mutbl));
                    }
                    place = None;
                }
            }
            before = target;
        }
        Ok(())
    }

    /// `consume_or_copy`: a `Copy` place is read (a shared borrow), any other moved.
    fn consume_or_copy(&mut self, cp: &mut CapturePlaces<'a>, place: Option<PlaceRef>, at: At) -> Result<(), Halt> {
        let Some(place) = place else { return Ok(()) };
        let t = cp.place_ty(place);
        match self.copy_of(cp, t)? {
            Copyness::Copy => self.borrow(cp, place, PlaceMode::ImmBorrow),
            Copyness::NotCopy => cp.uses.push((place, PlaceMode::ByValue)),
            Copyness::Undecided => return Err(gap(at, CaptureGapKind::CopyUndecided)),
        }
        Ok(())
    }

    /// A borrow of `place`; through a raw pointer only a shared one (upvar.rs `borrow`: "Raw
    /// pointers don't inherit mutability").
    fn borrow(&mut self, cp: &mut CapturePlaces<'a>, place: PlaceRef, mode: PlaceMode) {
        let mut mode = mode;
        for i in 0..place.len {
            if cp.proj(place, i).kind == ProjKind::Deref && matches!(self.view(cp.ty_before(place, i)), TyView::Ptr(..)) {
                mode = PlaceMode::ImmBorrow;
                break;
            }
        }
        cp.uses.push((place, mode));
    }

    /// Whether `t` is `Copy`: decided once per type. No `Copy` lang item: nothing is (as
    /// rules/closure.rs `moved_out` decides); an obligation still ambiguous after selection is
    /// [`Copyness::Undecided`].
    fn copy_of(&mut self, cp: &mut CapturePlaces<'a>, t: Ty) -> Result<Copyness, Stop> {
        let at = match cp.copy_cache.binary_search_by_key(&t, |c| c.0) {
            Ok(i) => return Ok(cp.copy_cache[i].1),
            Err(at) => at,
        };
        let answer = match cp.copy_trait {
            None => Copyness::NotCopy,
            Some(copy) => {
                let args = self.mk_list(&[t])?;
                let goal = Goal::Trait { trait_: copy, args };
                if !self.ty.may_hold(goal, &mut self.reads)? { Copyness::NotCopy } else { self.probe_copy(goal)? }
            }
        };
        cp.copy_cache.insert_shift(at, (t, answer));
        Ok(answer)
    }

    /// Selects `goal` in a probe (stalled.rs `ambiguous_interior_goal`'s method): rolling back
    /// restores the obligations and the error cursor.
    fn probe_copy(&mut self, goal: Goal) -> Result<Copyness, Stop> {
        let obligation = Obligation { goal, cause: Cause::Body };
        let snapshot = self.ty.snapshot();
        let mut probe = DropGuard::new((&mut *self.ty, snapshot), |(ty, snap)| ty.rollback_to(snap));
        while probe.0.next_error().is_some() {}
        probe.0.register(obligation)?;
        probe.0.select_all(&mut self.reads)?;
        let mut answer = Copyness::Copy;
        while let Some(error) = probe.0.next_error() {
            if error.obligation == obligation {
                answer = if error.kind == FulfillErrorKind::Ambiguous { Copyness::Undecided } else { Copyness::NotCopy };
            }
        }
        Ok(answer)
    }

    /// The struct update's taken fields, each `base.f` consumed (`walk_struct_expr`).
    fn fru(&mut self, cp: &mut CapturePlaces<'a>, expr: ExprId, base: ExprId) -> Result<(), Halt> {
        let Some(place) = self.cat(cp, base, true)? else { return Ok(()) };
        cp.scrutinees.push(place);
        let start = cp.fru_ix.partition_point(|r| r.0 < expr);
        let end = cp.fru_ix.partition_point(|r| r.0 <= expr);
        let limit = self.arena_cap();
        for i in start..end {
            let (_, field, ty) = cp.fru_ix[i];
            let ty = self.ty.resolve(ty)?;
            let p = cp.extend(place, ProjKind::Field { variant: 0, field }, ty, limit)?;
            self.consume_or_copy(cp, Some(p), At::Expr(base))?;
        }
        Ok(())
    }

    /// Whether `t` is an enum whose discriminant a pattern reads: more than one variant, or
    /// `#[non_exhaustive]` in any crate (`is_multivariant_adt`).
    fn multivariant(&mut self, t: Ty, at: At) -> Result<bool, Halt> {
        let TyView::Adt(k, _) = self.view(t) else { return Ok(false) };
        let Some(adt) = self.reads.adt(k)? else { return Err(gap(at, CaptureGapKind::PatternType)) };
        if adt.kind != AdtKind::Enum {
            return Ok(false);
        }
        Ok(adt.variants.len() > 1 || self.reads.attrs(k)?.0 & TypeAttrs::NON_EXHAUSTIVE != 0)
    }

    /// The ADT, variant and arguments a tuple-struct or struct pattern `p` matches at type `t`.
    fn pat_variant(&mut self, cp: &CapturePlaces<'a>, p: PatId, t: Ty) -> Result<(Adt<'w>, u32, TyList), Halt> {
        let TyView::Adt(k, args) = self.view(t) else { return Err(gap(At::Pat(p), CaptureGapKind::PatternType)) };
        let Some(adt) = self.reads.adt(k)? else { return Err(gap(At::Pat(p), CaptureGapKind::PatternType)) };
        let variant = match cp.pat_def(p) {
            Some(Def::Ctor { variant, .. }) => variant,
            Some(Def::Adt(_)) => 0,
            Some(Def::SelfTy | Def::Alias(_)) if adt.variants.len() == 1 => 0,
            _ => return Err(gap(At::Pat(p), CaptureGapKind::PatternResolution)),
        };
        if variant as usize >= adt.variants.len() {
            return Err(gap(At::Pat(p), CaptureGapKind::PatternResolution));
        }
        Ok((adt, variant, args))
    }

    /// Field `field` of `variant`'s type at `args`.
    fn variant_field_ty(&mut self, adt: Adt<'w>, variant: u32, field: usize, args: TyList, sub: PatId) -> Result<Ty, Halt> {
        let fields = adt.fields.get(adt.variants[variant as usize].fields.range()).unwrap_or(&[]);
        let Some(f) = fields.get(field).copied() else { return Err(gap(At::Pat(sub), CaptureGapKind::PatternType)) };
        let t = self.lower(adt.types, f.ty, Lower::Args(args), Cause::Pat(sub))?;
        Ok(self.ty.resolve(t)?)
    }

    /// `walk_pat` of `pat` over `place` (`cat_pattern`'s pre-order): each pattern's references
    /// peeled as rules/pat.rs `step_pat` peeled them, each binding a borrow or a consume, each
    /// test of the value a shared borrow of the place it tests. A by-value binding that moves
    /// is noted in `pat_moves`, but the one binding a plain `let x = place;` is (`plain_let`).
    fn walk_pat(&mut self, cp: &mut CapturePlaces<'a>, pat: PatId, place: PlaceRef, guard: bool, plain_let: bool) -> Result<(), Halt> {
        let limit = self.arena_cap();
        cp.pat_stack.clear();
        cp.pat_stack.push((pat, place, 0));
        let mut steps = 0usize;
        while let Some((p, mut place, mut mode)) = cp.pat_stack.pop() {
            steps += 1;
            if steps > self.body.pats.len() + 1 {
                return Err(CheckError::Limit(Limit::Steps).into());
            }
            let node = self.body.pat(p)?;
            let target = self.ty.resolve(self.pat_ty[p.ix()])?;
            let mut t = cp.place_ty(place);
            let mut peels = 0u32;
            while t != target {
                peels += 1;
                let TyView::Ref(m, inner) = self.view(t) else { return Err(gap(At::Pat(p), CaptureGapKind::PatternType)) };
                if peels > RECURSION_LIMIT {
                    return Err(gap(At::Pat(p), CaptureGapKind::PatternType));
                }
                let inner = self.ty.resolve(inner)?;
                place = cp.extend(place, ProjKind::Deref, inner, limit)?;
                mode = peeled_mode(mode, m);
                t = inner;
            }
            match node {
                Pat::Missing => return Err(gap(At::Pat(p), CaptureGapKind::Missing)),
                Pat::Wild => {}
                Pat::Bind { ann, sub, .. } => {
                    // A guard's bindings are shared borrows of the scrutinee first (MIR's).
                    if guard {
                        self.borrow(cp, place, PlaceMode::ImmBorrow);
                    }
                    let by_ref = match ann.by_ref {
                        Some(m) => Some(m),
                        None if ann.mutable => None,
                        None => match mode {
                            1 => Some(Mutbl::Not),
                            2 => Some(Mutbl::Mut),
                            _ => None,
                        },
                    };
                    match by_ref {
                        Some(m) => self.borrow(cp, place, PlaceMode::of(m)),
                        None => {
                            let before = cp.uses.len();
                            self.consume_or_copy(cp, Some(place), At::Pat(p))?;
                            let moved = cp.uses.len() > before && cp.uses.last().is_some_and(|u| u.1 == PlaceMode::ByValue);
                            if moved && !(plain_let && p == pat && sub.is_none()) {
                                cp.pat_moves.push(place);
                            }
                        }
                    }
                    if let Some(s) = sub {
                        cp.pat_stack.push((s, place, mode));
                    }
                }
                Pat::Ref { inner, .. } => {
                    let TyView::Ref(_, it) = self.view(t) else { return Err(gap(At::Pat(p), CaptureGapKind::PatternType)) };
                    let it = self.ty.resolve(it)?;
                    let sub = cp.extend(place, ProjKind::Deref, it, limit)?;
                    cp.pat_stack.push((inner, sub, 0));
                }
                Pat::Tuple { elems, rest } => {
                    let TyView::Tuple(l) = self.view(t) else { return Err(gap(At::Pat(p), CaptureGapKind::PatternType)) };
                    let n = self.ty.list_len(l) as usize;
                    let pats = self.body.pat_list(elems)?;
                    for (i, &c) in pats.iter().enumerate().rev() {
                        let Some(at) = sub_index(i, pats.len(), rest, n).filter(|&at| at < n) else {
                            return Err(gap(At::Pat(c), CaptureGapKind::PatternType));
                        };
                        let ty = self.ty.resolve(self.ty.list_get(l, at as u32))?;
                        let sub = cp.extend(place, ProjKind::Field { variant: 0, field: at as u32 }, ty, limit)?;
                        cp.pat_stack.push((c, sub, mode));
                    }
                }
                Pat::TupleStruct { elems, rest, .. } => {
                    if self.multivariant(t, At::Pat(p))? {
                        self.borrow(cp, place, PlaceMode::ImmBorrow);
                    }
                    let (adt, variant, args) = self.pat_variant(cp, p, t)?;
                    let n = adt.variants[variant as usize].fields.range().len();
                    let pats = self.body.pat_list(elems)?;
                    for (i, &c) in pats.iter().enumerate().rev() {
                        let Some(at) = sub_index(i, pats.len(), rest, n).filter(|&at| at < n) else {
                            return Err(gap(At::Pat(c), CaptureGapKind::PatternType));
                        };
                        let ty = self.variant_field_ty(adt, variant, at, args, c)?;
                        let sub = cp.extend(place, ProjKind::Field { variant, field: at as u32 }, ty, limit)?;
                        cp.pat_stack.push((c, sub, mode));
                    }
                }
                Pat::Struct { fields, .. } => {
                    if self.multivariant(t, At::Pat(p))? {
                        self.borrow(cp, place, PlaceMode::ImmBorrow);
                    }
                    let (adt, variant, args) = self.pat_variant(cp, p, t)?;
                    let declared = adt.fields.get(adt.variants[variant as usize].fields.range()).unwrap_or(&[]);
                    for fp in self.body.field_pat_list(fields)?.iter().rev() {
                        let Some(at) = declared.iter().position(|d| d.name == fp.name) else {
                            return Err(gap(At::Pat(fp.pat), CaptureGapKind::PatternField));
                        };
                        let ty = self.variant_field_ty(adt, variant, at, args, fp.pat)?;
                        let sub = cp.extend(place, ProjKind::Field { variant, field: at as u32 }, ty, limit)?;
                        cp.pat_stack.push((fp.pat, sub, mode));
                    }
                }
                Pat::Record { fields, .. } => {
                    let TyView::Record(schema, field_tys) = self.view(t) else {
                        return Err(gap(At::Pat(p), CaptureGapKind::PatternType));
                    };
                    let n = self.ty.list_len(field_tys);
                    for fp in self.body.field_pat_list(fields)?.iter().rev() {
                        let Some(at) = self.record_field_index(schema, fp.name).filter(|&i| i < n) else {
                            return Err(gap(At::Pat(fp.pat), CaptureGapKind::PatternField));
                        };
                        let ty = self.ty.resolve(self.ty.list_get(field_tys, at))?;
                        let sub = cp.extend(place, ProjKind::Field { variant: 0, field: at }, ty, limit)?;
                        cp.pat_stack.push((fp.pat, sub, mode));
                    }
                }
                Pat::Or { alts } => {
                    for &a in self.body.pat_list(alts)?.iter().rev() {
                        cp.pat_stack.push((a, place, mode));
                    }
                }
                Pat::Slice { elems, rest } => {
                    let pats = self.body.pat_list(elems)?;
                    let (elem, array) = match self.view(t) {
                        TyView::Array(e, _) => (e, true),
                        TyView::Slice(e) => (e, false),
                        _ => return Err(gap(At::Pat(p), CaptureGapKind::PatternType)),
                    };
                    // `[..]` and an array's length need no read.
                    if !(array || (pats.len() == 1 && rest == Some(0))) {
                        self.borrow(cp, place, PlaceMode::ImmBorrow);
                    }
                    let elem = self.ty.resolve(elem)?;
                    for (i, &c) in pats.iter().enumerate().rev() {
                        let ty = if rest == Some(i as u32) { self.ty.resolve(self.pat_ty[c.ix()])? } else { elem };
                        let sub = cp.extend(place, ProjKind::Index, ty, limit)?;
                        cp.pat_stack.push((c, sub, mode));
                    }
                }
                // A literal or a range is compared with the place: a read.
                Pat::Lit(_) | Pat::NegLit(_) | Pat::Range { .. } => self.borrow(cp, place, PlaceMode::ImmBorrow),
                Pat::Path(_) => match cp.pat_def(p) {
                    Some(Def::Const(_)) => self.borrow(cp, place, PlaceMode::ImmBorrow),
                    Some(_) => {
                        if self.multivariant(t, At::Pat(p))? {
                            self.borrow(cp, place, PlaceMode::ImmBorrow);
                        }
                    }
                    None => return Err(gap(At::Pat(p), CaptureGapKind::PatternResolution)),
                },
            }
        }
        Ok(())
    }

    /// Whether `t` is a raw pointer or a union: a place stops there.
    fn raw_or_union(&mut self, t: Ty) -> Result<bool, Stop> {
        Ok(match self.view(t) {
            TyView::Ptr(..) => true,
            TyView::Adt(k, _) => self.reads.adt(k)?.is_some_and(|a| a.kind == AdtKind::Union),
            _ => false,
        })
    }

    /// The first projection of `place` that is a field of a `repr(packed)` struct or union
    /// (`TypeAttrs::PACKED`, from the type's repr), if any: the length to truncate a borrow to.
    fn packed_field(&mut self, cp: &CapturePlaces<'a>, place: PlaceRef) -> Result<Option<u32>, Stop> {
        for i in 0..place.len {
            if !matches!(cp.proj(place, i).kind, ProjKind::Field { .. }) {
                continue;
            }
            if let TyView::Adt(k, _) = self.view(cp.ty_before(place, i))
                && self.reads.attrs(k)?.0 & TypeAttrs::PACKED != 0
            {
                return Ok(Some(i));
            }
        }
        Ok(None)
    }

    /// Whether `t` is an ADT with a destructor (a `Drop` impl: one applies to every instance).
    fn has_dtor(&mut self, cp: &CapturePlaces<'a>, t: Ty) -> Result<bool, Stop> {
        let (TyView::Adt(..), Some(drop)) = (self.view(t), cp.drop_trait) else { return Ok(false) };
        let args = self.mk_list(&[t])?;
        self.ty.may_hold(Goal::Trait { trait_: drop, args }, &mut self.reads)
    }

    /// `truncate_place_to_len_and_update_capture_kind`.
    fn truncate(&mut self, cp: &CapturePlaces<'a>, place: PlaceRef, mode: PlaceMode, len: u32) -> (PlaceRef, PlaceMode) {
        if len >= place.len {
            return (place, mode);
        }
        let mut mode = mode;
        if mode == PlaceMode::MutBorrow {
            for i in len..place.len {
                if cp.proj(place, i).kind == ProjKind::Deref && matches!(self.view(cp.ty_before(place, i)), TyView::Ref(Mutbl::Mut, _)) {
                    mode = PlaceMode::UniqueImmBorrow;
                    break;
                }
            }
        }
        (PlaceRef { len, ..place }, mode)
    }

    /// `process_collected_capture_information`: each use's place cut by the rules, the kind
    /// from the cut modes, then merged. Answers the closure's kind.
    fn settle_uses(&mut self, cp: &mut CapturePlaces<'a>, is_move: bool) -> Result<ClosureKind, Halt> {
        let mut kind = ClosureKind::Fn;
        for u in 0..cp.uses.len() {
            let (mut place, mut mode) = cp.uses[u];
            // `restrict_repr_packed_field_ref_capture`, which rustc applies in `borrow` and
            // `fake_read` before anything else (a `Copy` read is a borrow too): a reference into
            // a `repr(packed)` struct may be unaligned, so a BORROW of a field (any alignment)
            // captures the struct; a by-value use keeps the field. Before the raw-pointer rule
            // here as there: `borrow` already made such a use `ImmBorrow`, which truncation
            // keeps.
            if mode != PlaceMode::ByValue
                && let Some(i) = self.packed_field(cp, place)?
            {
                (place, mode) = self.truncate(cp, place, mode, i);
            }
            // `restrict_precision_for_unsafe`.
            if self.raw_or_union(place.base_ty)? {
                (place, mode) = self.truncate(cp, place, mode, 0);
            } else {
                for i in 0..place.len {
                    if self.raw_or_union(cp.proj(place, i).ty)? {
                        (place, mode) = self.truncate(cp, place, mode, i + 1);
                        break;
                    }
                }
            }
            // `restrict_capture_precision`: an index captures the whole indexed place.
            if let Some(i) = cp.first(place, ProjKind::Index) {
                (place, mode) = self.truncate(cp, place, mode, i);
            }
            // `truncate_capture_for_optimization`: through a shared reference, the reference.
            if let Some(i) = (0..place.len).rev().find(|&i| cp.proj(place, i).kind == ProjKind::Deref)
                && matches!(self.view(cp.ty_before(place, i)), TyView::Ref(Mutbl::Not, _))
            {
                (place, mode) = self.truncate(cp, place, PlaceMode::ImmBorrow, i + 1);
            }
            kind = kind.max(mode.kind());
            if is_move {
                if let Some(i) = cp.first(place, ProjKind::Deref) {
                    (place, mode) = self.truncate(cp, place, mode, i);
                }
                mode = PlaceMode::ByValue;
            } else if mode == PlaceMode::ByValue
                && let Some(i) = cp.first(place, ProjKind::Deref)
            {
                (place, mode) = self.truncate(cp, place, mode, i);
            }
            // `restrict_precision_for_drop_types`.
            if mode == PlaceMode::ByValue {
                let t = cp.place_ty(place);
                match self.copy_of(cp, t)? {
                    Copyness::Copy => {}
                    Copyness::Undecided => return Err(gap(At::Pat(place.base), CaptureGapKind::CopyUndecided)),
                    Copyness::NotCopy => {
                        for i in 0..place.len {
                            if self.has_dtor(cp, cp.ty_before(place, i))? {
                                (place, mode) = self.truncate(cp, place, mode, i);
                                break;
                            }
                        }
                    }
                }
            }
            self.merge(cp, place, mode);
        }
        Ok(kind)
    }

    /// `compute_min_captures` for one place: a captured descendant merges into it, or it merges
    /// into a captured ancestor or the same place, each in the stronger mode; else it is added.
    fn merge(&mut self, cp: &mut CapturePlaces<'a>, place: PlaceRef, mode: PlaceMode) {
        if !cp.roots.contains(&place.base) {
            cp.roots.push(place.base);
            cp.caps.push(Cap { place, mode });
            return;
        }
        let mut place = place;
        let mut mode = mode;
        let mut descendant = false;
        let mut i = 0;
        while i < cp.caps.len() {
            let c = cp.caps[i];
            if cp.relation(place, c.place) == Some(Ordering::Less) {
                let (_, cut) = self.truncate(cp, c.place, c.mode, place.len);
                mode = mode.max(cut);
                cp.caps.remove_shift(i);
                descendant = true;
            } else {
                i += 1;
            }
        }
        if !descendant {
            for i in 0..cp.caps.len() {
                let c = cp.caps[i];
                match cp.relation(place, c.place) {
                    Some(Ordering::Equal) => {
                        cp.caps[i].mode = c.mode.max(mode);
                        return;
                    }
                    Some(Ordering::Greater) => {
                        (place, mode) = self.truncate(cp, place, mode, c.place.len);
                        cp.caps[i].mode = c.mode.max(mode);
                        return;
                    }
                    _ => {}
                }
            }
        }
        cp.caps.push(Cap { place, mode });
    }

    /// Captures WHOLE each binding of `owner` one of whose merged places blaze-build does not
    /// lower ([`CaptureWiden`], the first reason found): its places become the binding itself,
    /// in the strongest mode any of them is captured by once cut to the binding (`truncate`:
    /// a `&mut` place through a `&mut` is a unique borrow of the reference), as the
    /// whole-binding rule captures it; logged in `cp.widened`. Before `finish`, so the parent's
    /// replay (`replay_child`), the kind, the environment and the witness all read the widened
    /// capture.
    fn widen(&mut self, cp: &mut CapturePlaces<'a>, owner: ExprId) -> Result<(), Halt> {
        for r in 0..cp.roots.len() {
            let root = cp.roots[r];
            let mut why = None;
            for c in 0..cp.caps.len() {
                let cap = cp.caps[c];
                if cap.place.base == root && cap.place.len > 0 {
                    why = self.widen_reason(cp, cap)?;
                    if why.is_some() {
                        break;
                    }
                }
            }
            let Some(why) = why else { continue };
            let mut whole: Option<Cap> = None;
            let mut i = 0;
            while i < cp.caps.len() {
                let cap = cp.caps[i];
                if cap.place.base != root {
                    i += 1;
                    continue;
                }
                let (place, mode) = self.truncate(cp, cap.place, cap.mode, 0);
                whole = Some(match whole {
                    Some(w) => Cap { place: w.place, mode: w.mode.max(mode) },
                    None => Cap { place, mode },
                });
                cp.caps.remove_shift(i);
            }
            if let Some(w) = whole {
                cp.caps.push(w);
                cp.widened.push((owner, root, why));
            }
        }
        Ok(())
    }

    /// Why captured place `cap` (below its binding) is not lowered, if it is not: a projection
    /// blaze-build does not walk (an enum variant's field, a `Box`'s contents, a wide pointer's
    /// target), a move below it, a pattern moving it out, or a pattern or struct update over a
    /// place above it (not the reference whose shared target it is: env_fields.rs `cover`'s
    /// `RefOf`).
    fn widen_reason(&mut self, cp: &CapturePlaces<'a>, cap: Cap) -> Result<Option<CaptureWiden>, Halt> {
        for i in 0..cap.place.len {
            let p = cp.proj(cap.place, i);
            let before = cp.ty_before(cap.place, i);
            match p.kind {
                ProjKind::Field { .. } => {
                    if let TyView::Adt(k, _) = self.view(before)
                        && self.reads.adt(k)?.is_some_and(|a| a.kind == AdtKind::Enum)
                    {
                        return Ok(Some(CaptureWiden::EnumField));
                    }
                }
                ProjKind::Deref => {
                    if let TyView::Adt(k, _) = self.view(before)
                        && Some(k) == cp.owned_box
                    {
                        return Ok(Some(CaptureWiden::BoxDeref));
                    }
                }
                ProjKind::Index => {}
            }
        }
        if cap.mode == PlaceMode::ByValue {
            if cp.uses.iter().any(|&(u, m)| m == PlaceMode::ByValue && cp.relation(cap.place, u) == Some(Ordering::Less)) {
                return Ok(Some(CaptureWiden::MoveBelow));
            }
            if cp.pat_moves.iter().any(|&m| matches!(cp.relation(cap.place, m), Some(Ordering::Less | Ordering::Equal))) {
                return Ok(Some(CaptureWiden::PatternMove));
            }
        }
        for &s in cp.scrutinees.iter() {
            if cp.relation(s, cap.place) != Some(Ordering::Less) {
                continue;
            }
            let last = cap.place.len - 1;
            let ref_of = cap.place.len == s.len + 1
                && cap.mode == PlaceMode::ImmBorrow
                && cp.proj(cap.place, last).kind == ProjKind::Deref
                && matches!(self.view(cp.ty_before(cap.place, last)), TyView::Ref(Mutbl::Not, _));
            if !ref_of {
                return Ok(Some(CaptureWiden::Above));
            }
        }
        Ok(None)
    }

    /// The merged places, filed in capture order with their stored types.
    fn finish(&mut self, cp: &mut CapturePlaces<'a>, kind: ClosureKind, suspends: bool) -> Result<Precise, Halt> {
        let first = cp.done_places.len();
        for r in 0..cp.roots.len() {
            let root = cp.roots[r];
            let from = cp.done_places.len();
            for c in 0..cp.caps.len() {
                let cap = cp.caps[c];
                if cap.place.base != root {
                    continue;
                }
                let t = cp.place_ty(cap.place);
                let stored = match cap.mode {
                    PlaceMode::ByValue => t,
                    PlaceMode::ImmBorrow => self.mk(TyView::Ref(Mutbl::Not, t))?,
                    PlaceMode::UniqueImmBorrow | PlaceMode::MutBorrow => self.mk(TyView::Ref(Mutbl::Mut, t))?,
                };
                let start = cp.done_arena.len() as u32;
                for j in 0..cap.place.len {
                    let proj = cp.proj(cap.place, j);
                    cp.done_arena.push(proj);
                }
                cp.done_places.push(CapturedPlace {
                    base: root,
                    base_ty: cap.place.base_ty,
                    start,
                    len: cap.place.len,
                    mode: cap.mode,
                    stored,
                });
            }
            // A stable insertion sort by field indices: declaration order within the root.
            for i in from + 1..cp.done_places.len() {
                let mut j = i;
                while j > from && cp.field_order(cp.done_places[j - 1], cp.done_places[j]) == Ordering::Greater {
                    cp.done_places.swap(j - 1, j);
                    j -= 1;
                }
            }
        }
        Ok(Precise { first: first as u32, end: cp.done_places.len() as u32, kind, suspends })
    }
}
