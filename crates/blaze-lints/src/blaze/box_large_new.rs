//! `box_large_new`: `Box::new(value)` of a value whose layout is over `large-box-threshold`
//! bytes (4 KiB unless `clippy.toml` sets it). The value is built on the stack, then copied into
//! the heap: a stack frame that size (in a debug build, where nothing is elided, a
//! `Box::new([0u8; 1 << 20])` overflows a fiber's stack) and a `memcpy` of all of it. Built in
//! place in the heap it is neither: `vec![0; n].into_boxed_slice()` for an array of zeros or
//! one value repeated, `Box::<T>::new_uninit()` and writing the fields, or `Box::default()` (its
//! `Default` builds in the box's slot in release builds). Goose builds every value in its final
//! place.
//!
//! Found: a call of std's `Box::new` whose argument's layout is known and over the threshold.
//! Not found: what blaze's `BoxInPlace` MIR pass builds in the box already, at every level
//! (`hir_ty_mir`'s `optimize/box_in_place.rs`): a value of at least [`BOX_IN_PLACE_MIN`] bytes
//! written as an array, tuple or struct literal, or returned by a call, whose inputs have
//! nothing to drop. What it still reports is a place moved in (a local, a field: on the stack
//! already), a value built over branches (`if`, `match`, a block) and one with inputs to drop.
//! Left to the lints that already speak for the size, when they are on: an array argument over
//! clippy's `large_stack_arrays` limit (`array-size-threshold`), any argument over
//! `large_stack_frames`' (`stack-size-threshold`), and a place (a local, a field) moved into the
//! call when `blaze::large_move` is on. Not checked: what a macro wrote and test code. No fix:
//! which in-place form fits depends on the value.

use hir::HirDisplay as _;
use hir_def::hir::{Array, Expr, ExprId, RecordSpread};
use hir_ty::drop::DropGlue;
use hir_ty_mir::mir::BOX_IN_PLACE_MIN;

use super::{Finding, Lint, exempt, facts, general_facts};
use crate::{
    check::Cx,
    lints::{Implemented, large_stack_arrays, large_stack_frames},
};

const LINT: Lint = Lint::BoxLargeNew;

/// 4 KiB: one page, the size past which a stack copy is more than a few cache lines' work and
/// the frame starts touching guard pages one at a time (stack probes).
pub(super) const DEFAULT_LIMIT: u64 = 4 << 10;

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) {
        return;
    }
    let found = find(cx);
    facts::report(cx, found);
}

fn find(cx: &Cx<'_, '_>) -> Vec<Finding> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    if cx.expansion().is_some() || cx.is_in_test(cx.root()) {
        return found;
    }
    let db = cx.db();
    let body = analysis.body();
    let limit = u64::try_from(cx.conf().int("large-box-threshold", DEFAULT_LIMIT as i64))
        .unwrap_or(DEFAULT_LIMIT);
    let arrays = cx.on(Implemented::LargeStackArrays).then(|| large_stack_arrays::limit(cx));
    let frames = cx.on(Implemented::LargeStackFrames).then(|| large_stack_frames::limit(cx));
    let large_move = cx.on_blaze(Lint::LargeMove);
    let mut parents: Option<facts::Parents> = None;
    for (id, expr) in body.exprs() {
        let Expr::Call { args, .. } = expr else { continue };
        let [arg] = &*args else { continue };
        let Some(callee) = general_facts::callee(db, analysis, id) else { continue };
        let boxed = callee.self_adt.is_some_and(|it| {
            facts::adt_is(db, it, "alloc", "Box") || facts::adt_is(db, it, "std", "Box")
        });
        if callee.name != "new" || !boxed || callee.trait_.is_some() {
            continue;
        }
        let ty = analysis.ty(analysis.infer().expr_ty(*arg));
        let Ok(layout) = ty.layout(db) else { continue };
        let size = layout.size();
        if size <= limit {
            continue;
        }
        if size >= BOX_IN_PLACE_MIN && built_in_place(cx, *arg) {
            // The pass does not run in a coroutine's body (an `async fn`, an `async` block):
            // there it is still built on the stack.
            let parents = parents.get_or_insert_with(|| facts::Parents::of(body));
            if !in_coroutine(body, parents, id) {
                continue;
            }
        }
        let array = ty.as_array(db).is_some();
        if array && arrays.is_some_and(|it| size > it) {
            continue;
        }
        if frames.is_some_and(|it| size > it) {
            continue;
        }
        let place = matches!(
            body.expr(*arg),
            Expr::Path(_) | Expr::Field { .. } | Expr::Index { .. } | Expr::UnaryOp { .. }
        );
        if place && large_move {
            continue;
        }
        let Some(at) = analysis.expr_range(id) else { continue };
        if exempt::exempt(cx.text(), at, LINT) {
            continue;
        }
        let target = cx.krate().to_display_target(db);
        let message = format!(
            "`Box::new` of a `{}` ({size} bytes): it is built on the stack, then copied into the heap",
            ty.display(db, target)
        );
        let repeat = matches!(body.expr(*arg), Expr::Array(Array::Repeat { .. }));
        let note = if repeat {
            // alloc: a message's part
            "`vec![value; n].into_boxed_slice()` (or `.try_into()` for a `Box<[T; N]>`) builds it in the heap, with no stack \
             copy and no frame that size (Goose builds values in their final place)"
                .to_owned()
        } else {
            // alloc: a message's part
            "`Box::<T>::new_uninit()` and writing it in place, or `Box::default()`, builds it in the heap with no stack copy \
             and no frame that size (Goose builds values in their final place); blaze builds `Box::new(<literal or call>)` \
             in place itself when its inputs have nothing to drop, but not a value already in a local or built over branches"
                .to_owned()
        };
        facts::push(&mut found, LINT, at, message, Some(note));
    }
    found
}

/// `arg`, the argument of a `Box::new` of at least [`BOX_IN_PLACE_MIN`] bytes, is one the
/// `BoxInPlace` pass builds in the box, with nothing to drop:
/// - a call of a function, none of whose inputs has something to drop (a method's receiver is
///   counted by its own type, which a by-reference receiver overstates: such a call is still
///   reported, never one the pass leaves silenced);
/// - a literal (array, tuple, struct, tuple struct) the pass builds in one block, all of it in
///   the box: see [`in_one_block`].
fn built_in_place(cx: &Cx<'_, '_>, arg: ExprId) -> bool {
    let Some(analysis) = cx.analysis() else { return false };
    let db = cx.db();
    let plain =
        |id: ExprId| analysis.ty(analysis.infer().expr_ty(id)).drop_glue(db) == DropGlue::None;
    if !plain(arg) {
        return false;
    }
    match analysis.body().expr(arg) {
        Expr::Call { callee, args } if general_facts::callee(db, analysis, arg).is_some() => {
            plain(callee) && args.iter().all(|&it| plain(it))
        }
        Expr::MethodCall { receiver, args, .. } => {
            plain(receiver) && args.iter().all(|&it| plain(it))
        }
        _ => in_one_block(cx, arg),
    }
}

/// `id` is a literal whose parts lower without a call, a check or a branch, so that MIR builds
/// all of it in one block, where `BoxInPlace` builds each part in its place in the box: its
/// leaves are literals and reads of small values, and each part at least
/// [`BOX_IN_PLACE_MIN`] bytes is a literal itself (an array literal's elements are not placed,
/// so they are small).
fn in_one_block(cx: &Cx<'_, '_>, id: ExprId) -> bool {
    let Some(analysis) = cx.analysis() else { return false };
    let db = cx.db();
    let body = analysis.body();
    let small = |id: ExprId| {
        let ty = analysis.ty(analysis.infer().expr_ty(id));
        ty.layout(db).is_ok_and(|it| it.size() < BOX_IN_PLACE_MIN)
    };
    match body.expr(id) {
        Expr::Literal(_) => true,
        Expr::Path(_) => small(id),
        Expr::Array(Array::Repeat { initializer, .. }) => {
            small(initializer) && in_one_block(cx, initializer)
        }
        Expr::Array(Array::ElementList { elements }) => {
            elements.iter().all(|&it| small(it) && in_one_block(cx, it))
        }
        Expr::Tuple { exprs } => exprs.iter().all(|&it| in_one_block(cx, it)),
        // A struct's fields are each built in their place; an enum variant's or a union's are
        // not, so they are small.
        Expr::RecordLit { fields, spread: RecordSpread::None, .. } => {
            let fieldwise = matches!(analysis.ty(analysis.infer().expr_ty(id)).as_adt(),
                Some(hir::Adt::Struct(it)) if !is_packed(db, it));
            fields.iter().all(|it| (fieldwise || small(it.expr)) && in_one_block(cx, it.expr))
        }
        Expr::Call { callee, args } => match constructor(cx, callee) {
            Some(fieldwise) => {
                args.iter().all(|&it| (fieldwise || small(it)) && in_one_block(cx, it))
            }
            None => false,
        },
        _ => false,
    }
}

/// Whether `callee` is a tuple struct's constructor (`Some(true)`: its fields are built in
/// their places) or a tuple variant's (`Some(false)`), or neither.
fn constructor(cx: &Cx<'_, '_>, callee: ExprId) -> Option<bool> {
    let analysis = cx.analysis()?;
    let ty = analysis.ty(analysis.infer().expr_ty(callee));
    match ty.as_callable(cx.db())?.kind() {
        hir::CallableKind::TupleStruct(it) => Some(!is_packed(cx.db(), it)),
        hir::CallableKind::TupleEnumVariant(_) => Some(false),
        _ => None,
    }
}

/// `it` is `#[repr(packed)]`: `BoxInPlace` does not write its fields one by one, as they may be
/// misaligned.
fn is_packed(db: &base_db::Db, it: hir::Struct) -> bool {
    it.repr(db).is_some_and(|it| it.pack.is_some())
}

/// `expr` runs in the body of a coroutine (an `async fn`'s, an `async` block's or closure's, a
/// `gen` block's): the innermost closure or coroutine above it is not a plain closure. Its MIR
/// is not optimized, so `BoxInPlace` does not run there.
fn in_coroutine(body: &hir_def::expr_store::Body, parents: &facts::Parents, expr: ExprId) -> bool {
    use hir_def::hir::ClosureKind;
    for parent in parents.up(expr, general_facts::UP) {
        let Expr::Closure { closure_kind, .. } = body.expr(parent) else { continue };
        return !matches!(closure_kind, ClosureKind::Closure);
    }
    false
}
