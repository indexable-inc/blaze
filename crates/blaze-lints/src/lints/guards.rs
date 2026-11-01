//! What the lints of held values take for a guard, and what they ask of a body's MIR:
//! `significant_drop_in_scrutinee`, `significant_drop_tightening`, `await_holding_lock` and
//! `await_holding_refcell_ref`.
//!
//! A guard is known by resolution: std's lock guards and `RefCell`'s borrows by the diagnostic
//! items clippy names them by (`MutexGuard`, `RwLockReadGuard`, `RwLockWriteGuard`,
//! `RefCellRef`, `RefCellRefMut`), std's others by name in std, and `lock_api`'s (the guards
//! `parking_lot`'s locks hand out) by name in `lock_api`. A type holds one when it is one, or
//! one is among its generic arguments, its tuple's fields or its struct's fields, a few levels
//! down (`LockResult<MutexGuard<..>>`, `Option<RefMut<..>>`, a struct with a guard field); a
//! reference to a guard holds none of its own.

use base_db::Db;
use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId,
    hir::{ClosureKind, CoroutineSource, Expr, ExprId},
};
use hir_ty::{
    InferBodyId,
    db::HirDatabase as _,
    mir::{BasicBlockId, LocalId, MirBody, MirSpan, OperandKind, StatementKind, StoredPlace, TerminatorKind},
    next_solver::TyKind,
};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::TextRange;

use crate::{blaze::facts, check::Cx};

/// What a guard holds taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// A lock: std's, or `lock_api`'s.
    Lock,
    /// A `RefCell`'s borrow.
    Cell,
}

impl Kind {
    /// What the guard holds, as a message says it.
    pub(crate) fn held(self) -> &'static str {
        match self {
            Kind::Lock => "the lock",
            Kind::Cell => "the `RefCell` borrow",
        }
    }
}

/// std's and core's guards by diagnostic item.
const DIAGNOSTIC: [(&str, Kind); 5] = [
    ("MutexGuard", Kind::Lock),
    ("RwLockReadGuard", Kind::Lock),
    ("RwLockWriteGuard", Kind::Lock),
    ("RefCellRef", Kind::Cell),
    ("RefCellRefMut", Kind::Cell),
];

/// std's lock guards with no diagnostic item, by name in std.
const STD: &[&str] = &["MappedMutexGuard", "MappedRwLockReadGuard", "MappedRwLockWriteGuard", "ReentrantLockGuard"];

/// `lock_api`'s guards, by name.
const LOCK_API: &[&str] = &[
    "MutexGuard",
    "MappedMutexGuard",
    "ArcMutexGuard",
    "RwLockReadGuard",
    "RwLockWriteGuard",
    "RwLockUpgradableReadGuard",
    "MappedRwLockReadGuard",
    "MappedRwLockWriteGuard",
    "ArcRwLockReadGuard",
    "ArcRwLockWriteGuard",
    "ArcRwLockUpgradableReadGuard",
    "ReentrantMutexGuard",
    "MappedReentrantMutexGuard",
    "ArcReentrantMutexGuard",
];

/// How many levels of arguments and fields a type is looked into for a guard.
const DEPTH: u32 = 4;

/// The guards of a crate, resolved once per check.
pub(crate) struct Guards {
    diagnostic: [Option<hir::Adt>; DIAGNOSTIC.len()],
}

impl Guards {
    pub(crate) fn new(cx: &Cx<'_, '_>) -> Guards {
        let db = cx.db();
        let krate = cx.krate();
        Guards {
            diagnostic: DIAGNOSTIC.map(|(name, _)| krate.diagnostic_item_adt(db, &intern::Symbol::intern(name))),
        }
    }

    /// What `adt` holds taken, if it is a guard.
    pub(crate) fn kind(&self, db: &Db, adt: hir::Adt) -> Option<Kind> {
        if let Some(at) = self.diagnostic.iter().position(|it| *it == Some(adt)) {
            return Some(DIAGNOSTIC[at].1);
        }
        let name = adt.name(db);
        let krate = adt.module(db).krate(db);
        let named = |names: &[&str], krate_name| names.contains(&name.as_str()) && facts::crate_is(db, krate, krate_name);
        (named(STD, "std") || named(LOCK_API, "lock_api")).then_some(Kind::Lock)
    }

    /// The guard of kind `kind` that `ty` holds: itself, or one in its arguments or fields.
    pub(crate) fn in_ty<'db>(&self, db: &'db Db, ty: &hir::Type<'db>, kind: Kind) -> Option<hir::Type<'db>> {
        self.find(db, ty, kind, DEPTH)
    }

    fn find<'db>(&self, db: &'db Db, ty: &hir::Type<'db>, kind: Kind, depth: u32) -> Option<hir::Type<'db>> {
        if ty.is_reference() || ty.is_raw_ptr() {
            return None;
        }
        if let Some(adt) = ty.as_adt()
            && self.kind(db, adt) == Some(kind)
        {
            return Some(*ty);
        }
        let depth = depth.checked_sub(1)?;
        if let Some(found) = ty.type_arguments().find_map(|it| self.find(db, &it, kind, depth)) {
            return Some(found);
        }
        if let Some(found) = ty.tuple_fields(db).iter().find_map(|it| self.find(db, it, kind, depth)) {
            return Some(found);
        }
        // A library struct's fields are its own business: std's and core's never hold a guard
        // of their arguments' that the arguments don't show.
        match ty.as_adt() {
            Some(hir::Adt::Struct(it)) if it.module(db).krate(db).lang_origin(db).is_none() => {
                ty.fields(db).iter().find_map(|(_, field)| self.find(db, field, kind, depth))
            }
            _ => None,
        }
    }
}

/// A MIR body of the function linted: its own, a closure's, or an `async` block's.
pub(crate) struct Body<'db> {
    pub(crate) mir: &'db MirBody<'db>,
    /// The closure's or `async` block's expression; `None` for the function's own body.
    pub(crate) expr: Option<ExprId>,
    /// Whether it is an `async` block's, which yields at each `.await`.
    pub(crate) coroutine: bool,
    /// Whether it is the `async` block an `async fn`'s body is.
    pub(crate) async_fn: bool,
}

/// The runtime bodies of `def`'s function: its own, each closure's (nested ones too) and each
/// `async` block's (an `async fn`'s body is one), in the order of their expressions.
pub(crate) fn bodies<'db>(db: &'db Db, analysis: &hir::BodyAnalysis<'db>, def: DefWithBodyId) -> Vec<Body<'db>> {
    // alloc: the bodies of one function, a few
    let mut out = Vec::new();
    if let Ok(mir) = db.runtime_mir_body(InferBodyId::DefWithBodyId(def)) {
        out.push(Body { mir, expr: None, coroutine: false, async_fn: false });
    }
    let infer = analysis.infer();
    for (id, expr) in analysis.body().exprs() {
        let Expr::Closure { closure_kind, .. } = expr else { continue };
        let ty = infer.expr_ty(id);
        let mir = match (closure_kind, ty.kind()) {
            (ClosureKind::Closure, TyKind::Closure(closure, _)) => db.runtime_mir_body_for_closure(closure.0),
            (ClosureKind::Coroutine { .. }, TyKind::Coroutine(coroutine, _)) => {
                db.runtime_mir_body_for_coroutine(coroutine.0)
            }
            _ => continue,
        };
        let Ok(mir) = mir else { continue };
        out.push(Body {
            mir,
            expr: Some(id),
            coroutine: matches!(closure_kind, ClosureKind::Coroutine { .. }),
            async_fn: matches!(closure_kind, ClosureKind::Coroutine { source: CoroutineSource::Fn, .. }),
        });
    }
    out
}

/// Where `span` of a body of `analysis` is written: an expression's range, or the outermost
/// macro call's that wrote it.
pub(crate) fn range_of(analysis: &hir::BodyAnalysis<'_>, span: MirSpan) -> Option<TextRange> {
    match span {
        MirSpan::ExprId(it) | MirSpan::ExprEnd(it) => analysis.expr_range(it),
        _ => None,
    }
}

/// Where `local` is first given a value: the expression of the first assignment of the whole
/// of it, or of the call that returns into it, in the order of the blocks.
pub(crate) fn first_assigned(mir: &MirBody<'_>, local: LocalId) -> Option<MirSpan> {
    for (_, block) in mir.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, _) = &statement.kind
                && place.local == local
                && place.projection.as_slice().is_empty()
                && !matches!(statement.span, MirSpan::Unknown)
            {
                return Some(statement.span);
            }
        }
        if let Some(terminator) = &block.terminator
            && let TerminatorKind::Call { destination, .. } = &terminator.kind
            && destination.local == local
            && destination.projection.as_slice().is_empty()
        {
            return Some(terminator.span);
        }
    }
    None
}

/// Whether `operand` moves the whole of `local` out.
pub(crate) fn moves_whole(operand: &hir_ty::mir::Operand, local: LocalId) -> bool {
    matches!(&operand.kind, OperandKind::Move(place) if place.local == local && place.projection.as_slice().is_empty())
}

/// Whether `local` may hold a value at the start of each block, by the block's index: an
/// assignment of the whole of it or a call returning into it gives it one, a move of the whole
/// of it out, a `Deinit` or a `Drop` of it ends it. Cleanup blocks are left out.
pub(crate) fn maybe_init(mir: &MirBody<'_>, local: LocalId) -> Vec<bool> {
    let blocks = mir.basic_blocks.len();
    // alloc: a flag per block of one body
    let mut init_in = vec![false; blocks];
    // Every pass that changes nothing ends it; each change sets a flag that stays set.
    for _ in 0..=blocks {
        let mut changed = false;
        for (id, block) in mir.basic_blocks.iter() {
            if block.is_cleanup {
                continue;
            }
            let Some(terminator) = &block.terminator else { continue };
            let init = init_after(mir, id, local, init_in[block_index(id)]);
            for next in terminator.kind.successors() {
                if init && !init_in[block_index(next)] {
                    init_in[block_index(next)] = true;
                    changed = true;
                }
            }
        }
        if !changed {
            break;
        }
    }
    init_in
}

/// Whether `local` may hold a value at the terminator of `block`, given whether it may at its
/// start.
pub(crate) fn init_before_terminator(mir: &MirBody<'_>, block: BasicBlockId, local: LocalId, mut init: bool) -> bool {
    for statement in &mir.basic_blocks[block].statements {
        match &statement.kind {
            StatementKind::Assign(place, rvalue) => {
                rvalue.for_each_operand(&mut |it| init &= !moves_whole(it, local));
                if whole(place, local) {
                    init = true;
                }
            }
            StatementKind::Deinit(place) if whole(place, local) => init = false,
            _ => {}
        }
    }
    init
}

/// Whether `local` may hold a value at the end of `block`, given whether it may at its start.
fn init_after(mir: &MirBody<'_>, block: BasicBlockId, local: LocalId, init: bool) -> bool {
    let mut init = init_before_terminator(mir, block, local, init);
    let Some(terminator) = &mir.basic_blocks[block].terminator else { return init };
    terminator.kind.for_each_operand(&mut |it| init &= !moves_whole(it, local));
    match &terminator.kind {
        TerminatorKind::Drop { place, .. } if whole(place, local) => false,
        TerminatorKind::Call { destination, .. } if whole(destination, local) => true,
        _ => init,
    }
}

fn whole(place: &StoredPlace, local: LocalId) -> bool {
    place.local == local && place.projection.as_slice().is_empty()
}

pub(crate) fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

/// A local as a message names it: the binding it holds, or a temporary of its type.
pub(crate) fn say_local<'db>(cx: &Cx<'db, '_>, analysis: &hir::BodyAnalysis<'db>, mir: &MirBody<'db>, local: LocalId) -> String {
    let db = cx.db();
    let ty = analysis.ty(mir.locals[local].ty.as_ref());
    let shown = ty.display(db, cx.krate().to_display_target(db));
    match mir.binding_locals.iter().find(|(_, it)| **it == local) {
        Some((binding, _)) => format!("`{}`, a `{shown}`,", analysis.body()[binding].name.as_str()),
        None => format!("a temporary `{shown}`"),
    }
}

/// The function a body of the walk is of, as a message names it.
pub(crate) fn function_name(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>) -> String {
    match analysis.def() {
        hir::DefWithBody::Function(f) => format!("`{}`", f.name(cx.db()).as_str()),
        _ => "this item".to_owned(), // alloc: a message's part
    }
}
