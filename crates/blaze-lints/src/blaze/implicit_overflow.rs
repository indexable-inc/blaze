//! `implicit_overflow` (in `blaze::edition`, allowed by default;
//! docs/v2/plan/design/explicit-overflow.md): integer arithmetic whose meaning is the profile's.
//! With overflow checks an overflowing `a + b` panics, without them it wraps, so code that
//! writes plain `+` has said neither. The lint refuses each such operation unless the code
//! says which is meant, or blaze's prover (`hir_ty_pure::reach::overflow_of_body`) shows it
//! cannot overflow on any path, in either profile.
//!
//! What it judges, of a function's body and its closures, test code left out: integer `+`,
//! `-`, `*`, `<<` and `>>`, their compound forms, unary `-` of a signed integer, an `as` that
//! can lose an integer's value (to a narrower integer, between signed and unsigned, `usize`
//! and `isize` taken as narrow as 32 bits on the target side, as `lossy_as` takes them), and a
//! call whose overflow is the profile's or a panic (`pow`, `abs`, `next_power_of_two`,
//! `Iterator::sum` and `product` of integers, `strict_*`). The forms that say what overflow
//! means are not found, being calls the prover does not need: `checked_*`, `wrapping_*` and
//! `Wrapping<T>`, `saturating_*`, `overflowing_*`, `TryFrom`. `/` and `%` are left out: their
//! panics (a zero divisor, `MIN / -1`) are the same in every profile, and are panic sites
//! `nopanic` and the prover already judge. Floats are left out but for a float cast to an
//! integer, which the prover does not follow: always found.
//!
//! A shift is judged by its amount alone: `<<` drops high bits in every profile, and only an
//! amount of the bits or more panics or masks. Arithmetic the code wrote inside a macro call's
//! arguments is judged where it is written; what a macro made up is not. A length's
//! subtraction that `len_minus` reports (`v.len() - n` with no check before it) is left to
//! `len_minus` when that lint is on, so the place is found once, with the advice for a length.
//!
//! There is no fallback: a body that does not lower, or whose proof does not settle, has each
//! of its operations found, saying so. The fix, machine-applicable, is offered in a function
//! returning `Option` (outside closures): `a + b` becomes `a.checked_add(b)?`, `x as u8`
//! becomes `u8::try_from(x).ok()?`; and for a cast in a function returning `Result` whose error
//! takes a `TryFromIntError` by `From`, `u8::try_from(x)?`.

use hir::HirDisplay as _;
use hir_def::{
    DefWithBodyId, FunctionId, ItemContainerId,
    expr_store::Body,
    hir::{ArithOp, BinaryOp, Expr, ExprId, Literal, UnaryOp},
};
use hir_ty::{
    db::HirDatabase as _,
    next_solver::{Ty, TyKind},
};
use hir_ty_pure::reach::{Missing, OpKind, Overflow};
use rustc_type_ir::inherent::IntoKind as _;
use syntax::{NodeOrToken, TextRange};

use super::{Lint, facts, lossy_as};
use crate::{check::Cx, diag::Applicability};

const LINT: Lint = Lint::ImplicitOverflow;

/// The longest operand or expression a message quotes; a longer one is named by its place.
const QUOTED: usize = 40;

/// A finding, before it is reported.
struct Found {
    range: TextRange,
    message: String,
    note: &'static str,
    fix: Option<(TextRange, String)>,
}

pub(super) fn check(cx: &mut Cx<'_, '_>) {
    if !cx.on_blaze(LINT) || cx.is_in_test(cx.root()) {
        return;
    }
    let found = find(cx);
    let root = cx.root();
    for it in found {
        let at = match root.text_range().contains_range(it.range) {
            true => match root.covering_element(it.range) {
                NodeOrToken::Node(node) => node,
                NodeOrToken::Token(token) => token.parent().unwrap_or(root),
            },
            false => root,
        };
        let span = cx.span(it.range);
        let fix = it.fix.map(|(at, text)| (cx.span(at), text));
        let diagnostic = cx.blaze(LINT, at, span, it.message);
        diagnostic.note(it.note);
        if let Some(part) = fix {
            // alloc: the fix's one part
            diagnostic.multipart_suggestion(
                "say that overflow is a failure",
                vec![part],
                Applicability::MachineApplicable,
            );
        }
        diagnostic
            .help(LINT.help())
            .help(format!("for further information visit {}", LINT.docs_url())); // alloc: a help line
    }
}

const NOTE: &str = "an integer operation that overflows panics with overflow checks and wraps without, so plain arithmetic means what the profile says; say which is meant (`checked_*` with `?`, `wrapping_*`, `saturating_*`, `overflowing_*`, `T::try_from(x)`), or bound the operands with a check blaze sees";

const FLOAT_NOTE: &str = "a float `as` an integer drops the fraction, saturates outside the integer's range and turns NaN into 0, which blaze does not follow; round as meant (`trunc`, `round`) and check the value against the integer's `MIN` and `MAX` first";

const CALL_NOTE: &str = "the call's overflow is the profile's (a panic with overflow checks, a wrap without) or a panic in every profile; the `checked_*` form says overflow is a failure, `wrapping_*` and `saturating_*` what it becomes";

/// What one operation is, as the lint judges it.
enum Site {
    /// `+ - * << >>` of integers; `compound` for `+=` and the rest.
    Binary { lhs: ExprId, rhs: ExprId, op: ArithOp, compound: bool },
    Neg { operand: ExprId },
    /// An integer cast that can lose the value, or a float cast to an integer (`float`).
    Cast { operand: ExprId, float: bool },
    /// A call whose overflow is the profile's or a panic; its name.
    Call { name: &'static str },
}

fn find(cx: &Cx<'_, '_>) -> Vec<Found> {
    // alloc: the findings of one body, reported at the edge
    let mut found = Vec::new();
    let Some(analysis) = cx.analysis() else { return found };
    // A `const` or `static` is evaluated at compile time, where an overflow is an error in
    // every profile.
    let Ok(def @ DefWithBodyId::FunctionId(func)) = DefWithBodyId::try_from(analysis.def()) else {
        return found;
    };
    let db = cx.db();
    let body = analysis.body();
    let infer = analysis.infer();
    let file = analysis.tree().1;
    let parents = facts::Parents::of(body);
    let shown = |ty| shown(cx, analysis, ty);
    let mut overflow: Option<&Overflow> = None;
    let fixes = Fixes::of(cx, func);
    for (id, expr) in body.exprs() {
        let Some(site) = site(cx, body, infer, id, expr) else { continue };
        // Inside a `const { .. }`: evaluated at compile time.
        if parents.up(id, usize::MAX).any(|it| matches!(body.expr(it), Expr::Const(_))) {
            continue;
        }
        let Ok(at) = analysis.source_map().expr_syntax(id) else { continue };
        if !matches!(at.file_id, hir::HirFileId::MacroFile(_)) && at.file_id != file {
            continue;
        }
        let Some(range) = cx.written_range(at.file_id, at.value.text_range()) else { continue };
        let outer = analysis.expr_range(id);
        if at.file_id != file && !outer.is_some_and(|it| it.contains_range(range)) {
            continue;
        }
        // A length's subtraction `len_minus` reports, when it is on, is its finding alone, so
        // the one place is found once (the one home of the rule is `len_minus::reported`).
        if let Site::Binary { op: ArithOp::Sub, compound: false, .. } = site
            && cx.on_blaze(Lint::LenMinus)
            && at.file_id == file
            && cx.in_file()
            && let Some(node) = written_bin(cx.root(), range)
            && super::std_mistakes::len_minus_reported(cx, node)
        {
            continue;
        }
        let quote = |expr: ExprId| quoted(cx, analysis, expr);
        // The type the operation is of: a compound assignment's is its place's.
        let ty = match site {
            Site::Binary { lhs, .. } => infer.expr_ty(lhs),
            _ => infer.expr_ty(id),
        };
        let kind = match site {
            Site::Binary { op: ArithOp::Shl | ArithOp::Shr, .. } => OpKind::Shift,
            Site::Binary { .. } => OpKind::Arith,
            Site::Neg { .. } => OpKind::Neg,
            Site::Cast { .. } => OpKind::Cast,
            Site::Call { .. } => {
                let Site::Call { name } = site else { continue };
                let message = match name {
                    "strict" => format!(
                        "{} panics when it overflows, in every profile",
                        quote(id).unwrap_or_else(|| "this `strict_*` call".to_owned()) // alloc: a message's part
                    ),
                    _ => format!(
                        "can't show {} fits in `{}` here: blaze does not follow `{name}`, whose overflow is the profile's",
                        quote(id).unwrap_or_else(|| format!("this `{name}`")), // alloc: a message's part
                        shown(ty)
                    ),
                };
                found.push(Found { range, message, note: CALL_NOTE, fix: None });
                continue;
            }
        };
        if let Site::Cast { float: true, .. } = site {
            // alloc: a message
            let message = format!(
                "{} turns a float into `{}`, which blaze can't show keeps its value",
                quote(id).unwrap_or_else(|| "this `as`".to_owned()), // alloc: a message's part
                shown(ty)
            );
            found.push(Found { range, message, note: FLOAT_NOTE, fix: None });
            continue;
        }
        let overflow = *overflow.get_or_insert_with(|| hir_ty_pure::reach::overflow_of_body(db, def));
        let why = if overflow.analysed(id, kind) {
            match overflow.missing(id, kind) {
                None => continue,
                Some(missing) => said(missing, &site, infer, &quote, &shown),
            }
        } else {
            match overflow.refused.first() {
                // Not lowered: code no path reaches.
                None => continue,
                // alloc: a message's part
                Some(&(_, why)) => format!("{} can't be checked: {why}", super::body_item(db, analysis)),
            }
        };
        let what = quote(id).unwrap_or_else(|| match &site {
            Site::Binary { op, compound, .. } => {
                format!("this `{}{}`", op_text(*op), if *compound { "=" } else { "" }) // alloc: a message's part
            }
            Site::Neg { .. } => "this `-`".to_owned(), // alloc: a message's part
            _ => "this `as`".to_owned(),               // alloc: a message's part
        });
        let message = match (&site, kind) {
            (_, OpKind::Shift) => format!("can't show {what} shifts by less than the bits of `{}` here: {why}", shown(ty)),
            (_, OpKind::Cast) => format!("can't show {what} keeps its value in `{}` here: {why}", shown(ty)),
            _ => format!("can't show {what} fits in `{}` here: {why}", shown(ty)),
        };
        let in_file = at.file_id == file && cx.in_file();
        let fix = if in_file { fixes.fix(cx, analysis, &parents, id, &site, ty, &shown) } else { None };
        let fix = fix.map(|text| (range, text));
        found.push(Found { range, message, note: NOTE, fix });
    }
    found
}

/// The binary expression of `root`'s tree written exactly at `range`.
fn written_bin(root: syntax::view::Node<'_>, range: TextRange) -> Option<syntax::view::Node<'_>> {
    if !root.text_range().contains_range(range) {
        return None;
    }
    let at = match root.covering_element(range) {
        NodeOrToken::Node(node) => node,
        NodeOrToken::Token(token) => token.parent()?,
    };
    at.ancestors()
        .find(|it| it.kind() == syntax::SyntaxKind::BIN_EXPR && it.text_range() == range)
}

/// What `expr` is, when the lint judges it.
fn site(
    cx: &Cx<'_, '_>,
    body: &Body,
    infer: &hir_ty::InferenceResult<'_>,
    id: ExprId,
    expr: Expr<'_>,
) -> Option<Site> {
    let int = |expr: ExprId| is_int(infer.expr_ty(expr));
    Some(match expr {
        Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::ArithOp(op)) } if arith(op) && int(lhs) && int(rhs) => {
            Site::Binary { lhs, rhs, op, compound: false }
        }
        Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::Assignment { op: Some(op) }) }
            if arith(op) && int(lhs) && int(rhs) =>
        {
            Site::Binary { lhs, rhs, op, compound: true }
        }
        Expr::UnaryOp { expr: operand, op: UnaryOp::Neg }
            if matches!(infer.expr_ty(operand).kind(), TyKind::Int(_)) =>
        {
            Site::Neg { operand }
        }
        Expr::Cast { expr: operand, .. } => {
            let (from_ty, to_ty) = (infer.expr_ty(operand), infer.expr_ty(id));
            let (from, to) = (lossy_as::Kind::of(from_ty)?, lossy_as::Kind::of(to_ty)?);
            if from_ty == to_ty || !lossy_as::loss(from, to)?.of_value() {
                return None;
            }
            let Ok(def) = DefWithBodyId::try_from(cx.analysis()?.def()) else { return None };
            if lossy_as::known(cx, def, body, operand, from).is_some_and(|it| lossy_as::fits(it, to)) {
                return None;
            }
            Site::Cast { operand, float: matches!(from, lossy_as::Kind::Float(_)) }
        }
        Expr::MethodCall { .. } => {
            let (func, _) = infer.method_resolution(id)?;
            Site::Call { name: profile_call(cx, func, infer.expr_ty(id))? }
        }
        _ => return None,
    })
}

fn arith(op: ArithOp) -> bool {
    matches!(op, ArithOp::Add | ArithOp::Sub | ArithOp::Mul | ArithOp::Shl | ArithOp::Shr)
}

fn is_int(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Int(_) | TyKind::Uint(_))
}

fn op_text(op: ArithOp) -> &'static str {
    match op {
        ArithOp::Add => "+",
        ArithOp::Sub => "-",
        ArithOp::Mul => "*",
        ArithOp::Shl => "<<",
        ArithOp::Shr => ">>",
        _ => "?",
    }
}

/// The method's name, after "follow", when a call of `func` returning `ty` overflows as the
/// profile says or panics: an integer's `pow`, `abs` or `next_power_of_two`, an integer
/// `Iterator::sum` or `product`; `"strict"` for a `strict_*`. Resolved by what the call calls,
/// never by its text.
fn profile_call(cx: &Cx<'_, '_>, func: FunctionId, ty: Ty<'_>) -> Option<&'static str> {
    let db = cx.db();
    let name = hir_def::signatures::FunctionSignature::of(db, func).name.symbol().as_str();
    match func.loc(db).container {
        ItemContainerId::TraitId(trait_) => {
            let iterator = cx.std_trait(&["core", "iter", "Iterator"])?;
            if hir_def::TraitId::from(iterator) != trait_ || !is_int(ty) {
                return None;
            }
            match name {
                "sum" => Some("Iterator::sum"),
                "product" => Some("Iterator::product"),
                _ => None,
            }
        }
        ItemContainerId::ImplId(imp) => {
            let core = cx.famous().core()?;
            if hir::Crate::from(hir_def::HasModule::krate(&imp, db)) != core {
                return None;
            }
            let self_ty = db.impl_self_ty(imp).skip_binder();
            if !is_int(self_ty) {
                return None;
            }
            match name {
                "pow" => Some("pow"),
                "abs" => Some("abs"),
                "next_power_of_two" => Some("next_power_of_two"),
                _ if name.starts_with("strict_") => Some("strict"),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The source of `expr`, in backticks, when it is short and on one line.
fn quoted(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'_>, expr: ExprId) -> Option<String> {
    let text = source(cx, analysis, expr)?;
    (text.len() <= QUOTED && !text.contains('\n')).then(|| format!("`{text}`")) // alloc: a message's part
}

/// The source of `expr` as written in the walked file.
fn source<'q>(cx: &Cx<'_, 'q>, analysis: &hir::BodyAnalysis<'_>, expr: ExprId) -> Option<&'q str> {
    let at = analysis.source_map().expr_syntax(expr).ok()?;
    let range = cx.written_range(at.file_id, at.value.text_range())?;
    cx.root().text_range().contains_range(range).then(|| cx.snippet(range))
}

/// What `missing` says is missing, after "here: ".
fn said<'db>(
    missing: Missing,
    site: &Site,
    infer: &hir_ty::InferenceResult<'db>,
    quote: &dyn Fn(ExprId) -> Option<String>,
    shown: &dyn Fn(Ty<'db>) -> String,
) -> String {
    let (left, right) = match *site {
        Site::Binary { lhs, rhs, .. } => (
            quote(lhs).unwrap_or_else(|| "the left operand".to_owned()), // alloc: a message's part
            quote(rhs).unwrap_or_else(|| "the right operand".to_owned()), // alloc: a message's part
        ),
        Site::Neg { operand } | Site::Cast { operand, .. } => {
            let it = quote(operand).unwrap_or_else(|| "the operand".to_owned()); // alloc: a message's part
            (it.clone(), it) // clone: a message's part
        }
        Site::Call { .. } => ("it".to_owned(), "it".to_owned()), // alloc: a message's part
    };
    let bare = |it: &str| it.trim_matches('`').to_owned(); // alloc: a message's part
    // alloc: a message's part
    match missing {
        Missing::Unbounded { left: true, right: true } => format!("nothing bounds {left} or {right}"),
        Missing::Unbounded { left: true, .. } => format!("nothing bounds {left}"),
        Missing::Unbounded { .. } => format!("nothing bounds {right}"),
        Missing::Order => format!("nothing shows `{} <= {}`", bare(&right), bare(&left)),
        Missing::Reaches(value) => {
            format!("what bounds {left} and {right} here lets it reach {value}")
        }
        Missing::Amount(bits) => format!("nothing shows `{} < {bits}`", bare(&right)),
        Missing::NotMin => {
            let ty = match *site {
                Site::Neg { operand } => shown(infer.expr_ty(operand)),
                _ => "the type".to_owned(), // alloc: a message's part
            };
            format!("nothing shows `{} != {ty}::MIN`", bare(&left))
        }
        Missing::Below(least) => format!("nothing shows `{least} <= {}`", bare(&left)),
        Missing::Above(most) => format!("nothing shows `{} <= {most}`", bare(&left)),
        Missing::Wide => "blaze's prover does not follow `u128` values".to_owned(),
    }
}

/// What a fix can rely on of the function: whether `?` takes an `Option`'s `None`, and
/// whether it takes a `TryFromIntError`.
struct Fixes {
    option: bool,
    try_from_error: bool,
}

impl Fixes {
    fn of(cx: &Cx<'_, '_>, func: FunctionId) -> Fixes {
        let db = cx.db();
        let ret = hir::Function::from(func).ret_type(db);
        let famous = cx.famous();
        let option = ret.as_adt().is_some_and(|it| Some(it) == famous.core_option_Option().map(hir::Adt::Enum));
        let try_from_error = (|| {
            let (adt, args) = ret.as_adt_with_args()?;
            if Some(adt) != famous.core_result_Result().map(hir::Adt::Enum) {
                return None;
            }
            let error = args.get(1)?.clone()?; // clone: a type handle
            let from = cx.std_trait(&["core", "convert", "From"])?;
            let hir::ModuleDef::Adt(int_error) = cx.std_def(&["core", "num", "TryFromIntError"])? else {
                return None;
            };
            let int_error = error.derived_from_next_solver(int_error.ty(db).to_next_solver());
            Some(error.impls_trait(db, from, &[int_error]))
        })()
        .unwrap_or(false);
        Fixes { option, try_from_error }
    }

    /// The text that says overflow is a failure in place of `id`, when it is clear: in a
    /// function returning `Option`, outside a closure; a cast also in one returning a `Result`
    /// whose error takes a `TryFromIntError`.
    #[allow(clippy::too_many_arguments)]
    fn fix<'db>(
        &self,
        cx: &Cx<'_, '_>,
        analysis: &hir::BodyAnalysis<'db>,
        parents: &facts::Parents,
        id: ExprId,
        site: &Site,
        ty: Ty<'db>,
        shown: &dyn Fn(Ty<'db>) -> String,
    ) -> Option<String> {
        let body = analysis.body();
        if parents.up(id, usize::MAX).any(|it| matches!(body.expr(it), Expr::Closure { .. })) {
            return None;
        }
        let text = |expr: ExprId| source(cx, analysis, expr);
        // alloc: the fix's text
        match *site {
            Site::Binary { lhs, rhs, op, compound } if self.option => {
                let method = match op {
                    ArithOp::Add => "checked_add",
                    ArithOp::Sub => "checked_sub",
                    ArithOp::Mul => "checked_mul",
                    // `checked_shl` takes a `u32`.
                    ArithOp::Shl if is_u32(analysis.infer().expr_ty(rhs)) => "checked_shl",
                    ArithOp::Shr if is_u32(analysis.infer().expr_ty(rhs)) => "checked_shr",
                    _ => return None,
                };
                let right = text(rhs)?;
                match compound {
                    // Only a local: its place is read and written once either way.
                    true => {
                        let Expr::Path(_) = body.expr(lhs) else { return None };
                        let left = text(lhs)?;
                        Some(format!("{left} = {left}.{method}({right})?"))
                    }
                    false => Some(format!("{}.{method}({right})?", receiver(body, lhs, text(lhs)?, &shown(ty)))),
                }
            }
            Site::Neg { operand } if self.option => {
                Some(format!("{}.checked_neg()?", receiver(body, operand, text(operand)?, &shown(ty))))
            }
            Site::Cast { operand, float: false } => {
                let value = text(operand)?;
                let to = shown(ty);
                match (self.option, self.try_from_error) {
                    (true, _) => Some(format!("{to}::try_from({value}).ok()?")),
                    (false, true) => Some(format!("{to}::try_from({value})?")),
                    (false, false) => None,
                }
            }
            _ => None,
        }
    }
}

/// How a message shows `ty`.
fn shown<'db>(cx: &Cx<'_, '_>, analysis: &hir::BodyAnalysis<'db>, ty: Ty<'db>) -> String {
    let db = cx.db();
    analysis.ty(ty).display(db, cx.krate().to_display_target(db)).to_string()
}

fn is_u32(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Uint(rustc_type_ir::UintTy::U32))
}

/// `expr`'s source as the receiver of a method call: in parentheses unless it binds as tight
/// as one, an unsuffixed literal given `ty`'s suffix so the call has a type.
fn receiver(body: &Body, expr: ExprId, text: &str, ty: &str) -> String {
    // alloc: the fix's text
    match body.expr(expr) {
        Expr::Literal(Literal::Int(_, None) | Literal::Uint(_, None)) => format!("{text}{ty}"),
        Expr::UnaryOp { expr: inner, op: UnaryOp::Neg }
            if matches!(body.expr(inner), Expr::Literal(Literal::Int(_, None) | Literal::Uint(_, None))) =>
        {
            format!("({text}{ty})")
        }
        Expr::Literal(_)
        | Expr::Path(_)
        | Expr::Call { .. }
        | Expr::MethodCall { .. }
        | Expr::Field { .. }
        | Expr::Index { .. } => text.to_owned(),
        _ => format!("({text})"),
    }
}
