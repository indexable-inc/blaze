//! `could_be_narrower`: an integer whose every value fits a narrower type than the one written:
//! fewer bits, unsigned where it is signed, or `NonZero` where it is never zero. A narrower
//! type is checked where the value is made instead of trusted where it is read, makes the
//! values that cannot happen unwritable, and packs tighter in every struct and record it is
//! stored in (and `NonZero` gives `Option` its niche).
//!
//! The rule is one join, `blaze_findings::fold::Narrow`: every site's value joined into one
//! range, decided against the declared type. It is the same rule for each subject it is asked
//! of:
//!
//! - **A function's result** (built here): every result of the body (the tail through blocks,
//!   `if` and `match`, and each `return` outside closures; diverging ones left out) is an
//!   integer literal, there are at least two distinct values (one is `always_same_result`'s),
//!   and they need fewer bits than the declared type. Per body, from the body alone.
//! - **A parameter or a field** (designed, not built): every call's argument, every
//!   construction's and every assignment's value is a site; one unknown value (not a literal,
//!   a const or a value already narrowed) and nothing is proved. That is a workspace fold over
//!   per-body rows (`blaze_findings::kinds::ItemRows`, one `Observed` per site), the shape of
//!   `weight::rows`' `Arg` join with a range in place of `Same`; it is in the deferred class,
//!   since a delta changes a verdict only through the leaves of the items it touched.
//!
//! Not found: an exported function (its type is a promise to callers outside the workspace),
//! a trait's function or an impl of one (the trait's signature is the choice), `async`, test
//! code, an `isize`/`usize` result whose values need more than 16 bits (an index type is chosen
//! for indexing, not range), and a result that is narrower only by being unsigned or nonzero.
//!
//! Class: cheap (per body, one walk of the body's results). No fix: the narrower type changes
//! every caller's arithmetic, and which callers want a widening is a choice.

use blaze_findings::fold::{Join as _, Narrow, Observed};
use hir_def::{
    DefWithBodyId, FunctionId,
    expr_store::Body,
    hir::{Expr, ExprId, Literal},
};
use hir_ty::{InferenceResult, next_solver::TyKind};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::view::{self as ast, AstNode as _, HasName as _};

use super::super::{
    Lint, std_mistakes, test_code,
    weight::rows::{self, Constant},
};
use crate::check::Cx;

const LINT: Lint = Lint::CouldBeNarrower;

/// The most expressions the walk of a body reads (rule 18).
const FUEL: usize = 1 << 14;

pub(super) fn check<'t>(cx: &mut Cx<'_, 't>, f: ast::Fn<'t>) {
    if !cx.on_blaze(LINT)
        || cx.is_exported(f.syntax())
        || f.async_token().is_some()
        || f.abi().is_some()
        || test_code::is_test_code(cx, f.syntax())
    {
        return;
    }
    let db = cx.db();
    let Some(func) = cx.fn_def(f) else { return };
    if let Some(assoc) = hir::AsAssocItem::as_assoc_item(func, db)
        && (assoc.implemented_trait(db).is_some()
            || matches!(assoc.container(db), hir::AssocItemContainer::Trait(_)))
    {
        return;
    }
    let Ok(id) = FunctionId::try_from(func) else { return };
    let def = DefWithBodyId::FunctionId(id);
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    let Some((signed, declared, written)) = declared(infer, body.root_expr()) else { return };
    let Some(seen) = results(db, infer, body) else { return };
    let mut distinct = seen.iter().map(|it| it.lo).collect::<Vec<i128, std::alloc::Global>>();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() < 2 {
        return;
    }
    let joined = seen.into_iter().fold(Observed::declared(signed, declared), Narrow::join);
    let Some(verdict) = Narrow::decide(joined) else { return };
    let Some(bits) = verdict.bits else { return };
    // An index type is chosen for indexing: only a need of 16 bits or less says otherwise.
    if matches!(written, "isize" | "usize") && bits > 16 {
        return;
    }
    let narrower = match verdict.lo < 0 {
        true => format!("i{bits}"),
        false if verdict.nonzero => format!("NonZero<u{bits}>"),
        false => format!("u{bits}"),
    };
    let name = func.name(db).as_str().to_owned();
    let message = format!(
        "`fn {name}` returns `{written}`, but every value it returns is in {lo}..={hi}: `{narrower}` holds them",
        lo = verdict.lo,
        hi = verdict.hi,
    );
    let note = "a narrower type makes the values that cannot happen unwritable and packs tighter where the \
                result is stored; for a value with a range of its own, `#[blaze::invariant(..)]` on the type \
                that holds it states the range once";
    let Some(range) = f.name().and_then(|it| cx.range(it.syntax())) else { return };
    std_mistakes::report(cx, LINT, f.syntax(), range, message, note, None);
}

/// The declared result type, when it is an integer: (signed, bits, as written).
fn declared(infer: &InferenceResult<'_>, root: ExprId) -> Option<(bool, u8, &'static str)> {
    use rustc_type_ir::{IntTy, UintTy};
    Some(match infer.expr_ty(root).kind() {
        TyKind::Int(it) => (
            true,
            match it {
                IntTy::I8 => 8,
                IntTy::I16 => 16,
                IntTy::I32 => 32,
                IntTy::I64 | IntTy::Isize => 64,
                IntTy::I128 => 128,
                // An arbitrary-width integer already states its width: nothing narrower to say.
                IntTy::Bits(_) => return None,
            },
            it.name_str(),
        ),
        TyKind::Uint(it) => (
            false,
            match it {
                UintTy::U8 => 8,
                UintTy::U16 => 16,
                UintTy::U32 => 32,
                UintTy::U64 | UintTy::Usize => 64,
                UintTy::U128 => 128,
                UintTy::Bits(_) => return None,
            },
            it.name_str(),
        ),
        _ => return None,
    })
}

/// Every result of `body` as a value, or `None` when one is not an integer literal (or the walk
/// ran out of fuel, or the body returns without a value).
fn results(db: &base_db::Db, infer: &InferenceResult<'_>, body: &Body) -> Option<Vec<Observed, std::alloc::Global>> {
    let mut at = Vec::new_in(std::alloc::Global);
    tails(body, body.root_expr(), &mut at);
    let mut stack = Vec::new_in(std::alloc::Global);
    stack.push(body.root_expr());
    let mut fuel = FUEL;
    while let Some(expr) = stack.pop() {
        fuel = fuel.checked_sub(1)?;
        match body.expr(expr) {
            Expr::Closure { .. } => continue,
            Expr::Return { expr: Some(value) } => tails(body, value, &mut at),
            Expr::Return { expr: None } => return None,
            _ => {}
        }
        body.walk_child_exprs(expr, |child| stack.push(child));
    }
    let mut seen = Vec::new_in(std::alloc::Global);
    for expr in at {
        if let TyKind::Never = infer.expr_ty(expr).kind() {
            continue;
        }
        let value = match rows::constant(db, infer, body, expr)? {
            Constant::Literal(Literal::Int(value, _)) => value,
            Constant::Literal(Literal::Uint(value, _)) => i128::try_from(value).ok()?,
            _ => return None,
        };
        seen.push(Observed::value(value));
    }
    Some(seen)
}

/// The results of `expr`: itself, or through a block's tail, an `if`'s branches and a
/// `match`'s arms.
fn tails(body: &Body, expr: ExprId, out: &mut Vec<ExprId, std::alloc::Global>) {
    match body.expr(expr) {
        Expr::Block { tail: Some(tail), .. } => tails(body, tail, out),
        Expr::If { then_branch, else_branch: Some(other), .. } => {
            tails(body, then_branch, out);
            tails(body, other, out);
        }
        Expr::Match { arms, .. } => {
            for arm in arms {
                tails(body, arm.expr, out);
            }
        }
        _ => out.push(expr),
    }
}
