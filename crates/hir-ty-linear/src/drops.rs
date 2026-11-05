//! The drops a body may run of a linear value.
//!
//! At each `Drop` of a built body that does not unwind, what the dropped place may hold is read
//! off the maybe-init analysis of the move paths of the locals that hold a linear value
//! ([`drop_states`]). A path whose type is linear itself is consumed when a part of it is
//! moved out on every way to the drop (it was destructured, in part or in full); otherwise it
//! is dropped if it may hold a value. A part no path below the dropped place reaches is dropped
//! whole with it, so one that holds a linear value is an error too.

use hir_def::hir::{Expr, ExprId};
use hir_ty::next_solver::{Ty, TyKind};
use hir_ty_mir::mir::{
    DropState, HeldPath, MirBody, MirSpan, PlaceElem, ProjectionElem, StatementKind, drop_states,
};
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind};

use crate::{LinearDiagnostic, names, ty::Types};

/// Errors for every drop of `body` that may drop a linear value implicitly.
pub(crate) fn check_drops<'db>(
    types: &Types<'_, 'db>,
    body: &MirBody<'db>,
    out: &mut Vec<LinearDiagnostic>,
) {
    let holds = |ty: Ty<'db>| types.holds(ty);
    if !body.locals.iter().any(|(_, local)| holds(local.ty.as_ref())) {
        return;
    }
    for state in drop_states(types.db, body, holds) {
        // A drop on the way out of a panic: unwinding runs the value's own drop.
        if state.cleanup {
            continue;
        }
        match &state.paths {
            None => {
                if types.holds(state.ty.ty) {
                    let elems = state.place.projection.as_slice();
                    report(types, body, &state, elems, state.ty.ty, Dropped::Behind, out);
                }
            }
            Some(held) => check_held(types, body, &state, held, out),
        }
    }
}

/// How a linear value is dropped at a drop.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dropped {
    /// The path is dropped and may hold its value.
    Whole { maybe_moved: bool },
    /// A part of the path no move reaches is dropped with it.
    Part,
    /// The place is behind a reference or a pointer, or a part of a value whose type
    /// implements `Drop`: what it holds is not tracked, so it may hold all of its value.
    Behind,
}

fn check_held<'db>(
    types: &Types<'_, 'db>,
    body: &MirBody<'db>,
    state: &DropState<'db>,
    held: &[HeldPath<'db>],
    out: &mut Vec<LinearDiagnostic>,
) {
    let mut reported = vec![false; held.len()]; // alloc: one flag a path, error path's size
    for (at, path) in held.iter().enumerate() {
        if let Some(parent) = path.parent
            && reported[parent as usize]
        {
            reported[at] = true;
            continue;
        }
        if !path.maybe_init {
            continue;
        }
        let children = || held.iter().filter(move |it| it.parent == Some(at as u32));
        let ty = path.ty.ty;
        if path.ty.variant_id.is_none() && types.is_linear(ty) {
            let consumed = children().any(|it| !it.maybe_init);
            if !consumed {
                let dropped = Dropped::Whole { maybe_moved: path.maybe_uninit };
                report(types, body, state, path.place.projection.as_slice(), ty, dropped, out);
                reported[at] = true;
                continue;
            }
        }
        // The parts no path below reaches, dropped with this one.
        let covered = |step: &PlaceElem| {
            children().any(|it| it.place.projection.as_slice().last() == Some(step))
        };
        for (step, part) in parts_of(types, path) {
            if step.as_ref().is_some_and(covered) || !types.holds(part) {
                continue;
            }
            let mut elems = path.place.projection.as_slice().to_vec(); // alloc: error path
            elems.extend(step);
            report(types, body, state, &elems, part, Dropped::Part, out);
        }
    }
}

/// The parts of the value at `path`, each with the step that reaches it and its type: a
/// struct's or variant's fields, an enum's variants (each as its downcast, of the enum's
/// type), a tuple's or closure's fields, a `Box`'s contents, an array's elements
/// (`ConstantIndex` from its start). A value whose parts no move path reaches (an opaque
/// type's, a coroutine's) gives its parts with no step: no path below covers them.
fn parts_of<'db>(
    types: &Types<'_, 'db>,
    path: &HeldPath<'db>,
) -> Vec<(Option<PlaceElem>, Ty<'db>)> {
    use hir_def::{AdtId, VariantId};
    use hir_ty::db::HirDatabase;
    use hir_ty_mir::mir::FieldIndex;
    let db = types.db;
    let interner = types.interner;
    let ty = path.ty.ty;
    let field = |index: usize| Some(ProjectionElem::Field(FieldIndex(index as u32)));
    let fields = |variant: VariantId, args: hir_ty::next_solver::GenericArgs<'db>| {
        db.field_types(variant)
            .iter()
            .enumerate()
            .map(|(index, (_, it))| {
                (field(index), it.ty().instantiate(interner, args).skip_norm_wip())
            })
            .collect::<Vec<_>>() // alloc: a held path's parts, on a body with a linear value
    };
    match ty.kind() {
        TyKind::Adt(def, args) if def.is_box() => {
            vec![(Some(ProjectionElem::Deref), args.type_at(0))]
        } // alloc: as above
        TyKind::Adt(def, args) => match (path.ty.variant_id, def.def_id()) {
            (Some(variant), _) => fields(variant, args),
            (None, AdtId::StructId(id)) => fields(id.into(), args),
            // A variant is a part of the enum's type, that holds a linear value when one of its
            // fields does.
            (None, AdtId::EnumId(id)) => hir_ty::layout::enum_variant_ids(db, id)
                .iter()
                .filter(|&&variant| {
                    fields(variant.into(), args).iter().any(|&(_, it)| types.holds(it))
                })
                .map(|&variant| (Some(ProjectionElem::Downcast(variant.into())), ty))
                .collect(), // alloc: as above
            (None, AdtId::UnionId(_)) => Vec::new(), // alloc: empty
        },
        TyKind::Tuple(tys) => {
            tys.iter().enumerate().map(|(index, it)| (field(index), it)).collect()
        } // alloc: as above
        TyKind::Closure(_, args) => args
            .as_closure()
            .upvar_tys()
            .iter()
            .enumerate()
            .map(|(index, it)| (field(index), it))
            .collect(), // alloc: as above
        TyKind::Array(element, len) => {
            let len = hir_ty::consteval::try_const_usize(db, len).unwrap_or(0) as u64;
            (0..len)
                .map(|offset| {
                    let step =
                        ProjectionElem::ConstantIndex { offset, min_length: len, from_end: false };
                    (Some(step), element)
                })
                .collect() // alloc: as above
        }
        _ => {
            let mut out = Vec::new(); // alloc: as above
            types.parts(ty, &mut |part| {
                out.push((None, part));
                false
            });
            out
        }
    }
}

/// How the way to a drop leaves the code that holds the value.
#[derive(Clone, Copy)]
enum Exit {
    /// The end of the value's scope.
    ScopeEnd,
    /// A new value assigned over it.
    Overwrite,
    /// An early exit, written at the expression.
    Early(ExprId, &'static str),
    /// Lowering says no more of where.
    Here,
}

fn report<'db>(
    types: &Types<'_, 'db>,
    body: &MirBody<'db>,
    state: &DropState<'db>,
    elems: &[PlaceElem],
    ty: Ty<'db>,
    dropped: Dropped,
    out: &mut Vec<LinearDiagnostic>,
) {
    let db = types.db;
    let value = names::place(db, body, state.place.local, elems);
    let shown = names::ty(db, body, ty);
    let exit = exit_of(db, body, state);
    let how = match exit {
        Exit::ScopeEnd => "at the end of its scope".to_owned(), // alloc: error path
        Exit::Overwrite => "when a new value is assigned over it".to_owned(), // alloc: error path
        Exit::Early(_, what) => format!("when {what}"),         // alloc: error path
        Exit::Here => "here".to_owned(),                        // alloc: error path
    };
    let some = match dropped {
        Dropped::Whole { maybe_moved: true } => ", on a path that does not move it,",
        _ => "",
    };
    let message = match dropped {
        Dropped::Whole { .. } => format!(
            "`{value}`, a `#[blaze::linear]` `{shown}`, is dropped{some} {how} without being consumed"
        ),
        Dropped::Part => format!(
            "`{value}` holds a `#[blaze::linear]` value (its `{shown}`) and is dropped {how}: \
             the linear value is dropped with it"
        ),
        Dropped::Behind => format!(
            "`{value}` holds a `#[blaze::linear]` `{shown}` and is dropped {how}, through a \
             place whose value is not tracked (behind a reference or inside a type with a \
             `Drop` impl)"
        ),
    };
    let span = match exit {
        Exit::Early(expr, _) => MirSpan::ExprId(expr),
        _ => state.span,
    };
    let mut labels = Vec::new(); // alloc: error path
    if let Some(binding) = names::binding_of(body, state.place.local) {
        labels.push((
            MirSpan::BindingId(binding),
            format!("`{}` is declared here", names::binding(db, body, binding)),
        ));
    }
    if let Exit::Early(_, _) = exit {
        labels.push((state.span, "and dropped here, at the end of its scope".to_owned())); // alloc: error path
    }
    out.push(LinearDiagnostic {
        span,
        message,
        labels,
        notes: vec![CONSUME.to_owned()], // alloc: error path
    });
}

/// How to consume a linear value, the note of every drop error.
pub(crate) const CONSUME: &str = "a `#[blaze::linear]` value must be consumed on every path: \
    moved into a function that takes it by value and destructures it, destructured in place, \
    or given to a function marked `#[blaze::consumes]` (`fn consume<#[blaze::linear] T>(_: T)` \
    with `#[blaze::consumes]` drops any)";

/// How the way to `state`'s drop leaves the value's code: an overwrite when the drop is of a
/// place assigned next, an early exit when a block that goes to the drop chain ends in a
/// `return`, `break`, `continue` or `?`, else the end of a scope where lowering says so.
fn exit_of<'db>(db: &'db base_db::Db, body: &MirBody<'db>, state: &DropState<'db>) -> Exit {
    if state.replace {
        return Exit::Overwrite;
    }
    let assigned = body.basic_blocks[state.target].statements.first().is_some_and(
        |it| matches!(&it.kind, StatementKind::Assign(place, _) if *place == state.place),
    );
    if assigned {
        return Exit::Overwrite;
    }
    // The blocks that go to the drop, back through the drop chain an exit shares (each only
    // ends storage and drops or goes on), the nearest first.
    let store = names::store(db, body);
    let mut seen = vec![false; body.basic_blocks.len()]; // alloc: error path
    let mut queue = std::collections::VecDeque::from([state.block]); // alloc: error path
    let index = |it: hir_ty_mir::mir::BasicBlockId| it.into_raw().into_u32() as usize;
    seen[index(state.block)] = true;
    // Bounded by the blocks: each is queued once.
    while let Some(at) = queue.pop_front() {
        for (pred, block) in body.basic_blocks.iter() {
            let Some(terminator) = &block.terminator else { continue };
            if block.is_cleanup
                || seen[index(pred)]
                || !terminator.kind.successors().any(|it| it == at)
            {
                continue;
            }
            seen[index(pred)] = true;
            if let MirSpan::ExprId(expr) = terminator.span
                && let Some(what) = early_exit(db, body, store, expr)
            {
                return Exit::Early(expr, what);
            }
            let chain = block
                .statements
                .iter()
                .all(|it| matches!(it.kind, StatementKind::StorageDead(_) | StatementKind::Nop));
            if chain {
                queue.push_back(pred);
            }
        }
    }
    match state.span {
        MirSpan::ExprEnd(_) => Exit::ScopeEnd,
        _ => Exit::Here,
    }
}

/// What the early exit `expr` is, said as the error says it.
fn early_exit<'db>(
    db: &'db base_db::Db,
    body: &MirBody<'db>,
    store: &hir_def::expr_store::ExpressionStore,
    expr: ExprId,
) -> Option<&'static str> {
    Some(match store.expr(expr) {
        Expr::Return { .. } if names::is_try(db, body, expr) => "`?` returns early",
        Expr::Return { .. } => "`return` leaves early",
        Expr::Break { .. } => "`break` leaves its loop or block",
        Expr::Continue { .. } => "`continue` goes to the next iteration",
        Expr::Become { .. } => "`become` leaves with a tail call",
        _ => return None,
    })
}

/// Errors for a coroutine's body (an `async fn` or block) that holds a linear value: the
/// future can be dropped at any `.await`, which drops what it holds there, and no code of the
/// body runs to consume it.
pub(crate) fn check_coroutine<'db>(
    types: &Types<'_, 'db>,
    body: &MirBody<'db>,
    out: &mut Vec<LinearDiagnostic>,
) {
    for (local, data) in body.locals.iter() {
        let ty = data.ty.as_ref();
        if !types.holds(ty) {
            continue;
        }
        let value = names::place(types.db, body, local, &[]);
        let shown = names::ty(types.db, body, ty);
        let span = match names::binding_of(body, local) {
            Some(binding) => MirSpan::BindingId(binding),
            None => first_use(body, local),
        };
        out.push(LinearDiagnostic {
            span,
            message: format!(
                "`{value}` holds a `#[blaze::linear]` `{shown}` in an `async` body: the future \
                 may be dropped at any `.await`, which drops it without consuming it"
            ),
            labels: Vec::new(),              // alloc: error path, empty
            notes: vec![CONSUME.to_owned()], // alloc: error path
        });
    }
}

/// The span of the first statement that names `local`, where a temporary is made.
fn first_use(body: &MirBody<'_>, local: hir_ty_mir::mir::LocalId) -> MirSpan {
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            if let StatementKind::Assign(place, _) = &statement.kind
                && place.local == local
            {
                return statement.span;
            }
        }
    }
    MirSpan::Unknown
}
