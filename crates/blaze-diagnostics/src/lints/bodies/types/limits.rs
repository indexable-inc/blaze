//! `TypeLimits`: literals out of the range of their type (`overflowing_literals`, deny),
//! comparisons a type's limits decide (`unused_comparisons`), comparisons with NaN
//! (`invalid_nan_comparisons`), of wide pointers (`ambiguous_wide_pointer_comparisons`) and of
//! function pointers (`unpredictable_function_pointer_comparisons`).
//!
//! Follows rustc's `compiler/rustc_lint/src/types.rs` (`TypeLimits`, `lint_nan`,
//! `lint_wide_pointer`, `lint_fn_pointer`) and the messages of `compiler/rustc_lint/src/lints.rs`
//! (MIT or Apache-2.0).

mod literal;

use base_db::Crate;
use blaze_hash::HashMap;
use hir_def::{
    AdtId, EnumId, HasModule as _,
    hir::{BinaryOp, CmpOp, Expr, ExprId, Literal, Ordering, Pat, PatId, UnaryOp},
    lang_item::lang_items,
};
use hir_ty::next_solver::{DbInterner, Ty, TyKind};
use intern::Symbol;
use rustc_type_ir::inherent::{GenericArgs as _, IntoKind, Ty as _};
use syntax::view::{self as ast, AstNode as _};

use super::util::{self, Solver};
use crate::{
    lints::bodies::{
        BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, PatKind, PatKinds, lint_set,
    },
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

/// The pass runs at every expression of every body, so each check is gated on what costs least
/// first: the expression's kind (a `u8` tag of the store's columns), then the type inference
/// gave it (an index into `InferenceResult`), then the lowered literal's value. Syntax, spans,
/// paths, the trait solver and strings are reached only by an expression that lints, or that
/// only the source can tell apart (a pattern's literal, whose `-` the lowering folded in).
/// Nothing is allocated on the path that doesn't lint; the per-crate caches are filled once.
pub(crate) struct TypeLimits {
    /// rustc's `negated_expr_id`: the last `-` visited, and its operand. Its span is built only
    /// when the operand overflows.
    last_negation: Option<(ExprId, ExprId)>,
    /// The literals of the pattern last visited: a literal pattern's, or a range's ends. The
    /// walker visits them right after their pattern.
    pat_lits: [Option<ExprId>; 2],
    names: Names,
    /// Per crate: the pointer width `isize` and `usize` have, and `Option`.
    krate: Option<Crate>,
    pointer_bits: Option<u16>,
    option: Option<EnumId>,
    /// Whether an ADT is `NonNull` (its diagnostic item), looked up once per ADT compared as a
    /// pointer: about 16 bytes an entry, one hash probe a lookup.
    non_null: HashMap<AdtId, bool>,
}

/// The diagnostic items the pass compares with, interned once: a comparison is a pointer
/// compare.
struct Names {
    nan: [Symbol; 4],
    non_null: Symbol,
    cmpops: [(Symbol, ComparisonOp); 10],
}

impl Default for TypeLimits {
    fn default() -> TypeLimits {
        let names = Names {
            nan: ["f16_nan", "f32_nan", "f64_nan", "f128_nan"].map(Symbol::intern),
            non_null: Symbol::intern("NonNull"),
            cmpops: [
                ("cmp_ord_max", ComparisonOp::Other),
                ("cmp_ord_min", ComparisonOp::Other),
                ("ord_cmp_method", ComparisonOp::Other),
                ("cmp_partialord_cmp", ComparisonOp::Other),
                ("cmp_partialeq_eq", ComparisonOp::BinOp(Cmp::Eq)),
                ("cmp_partialeq_ne", ComparisonOp::BinOp(Cmp::Ne)),
                ("cmp_partialord_ge", ComparisonOp::BinOp(Cmp::Ge)),
                ("cmp_partialord_gt", ComparisonOp::BinOp(Cmp::Gt)),
                ("cmp_partialord_le", ComparisonOp::BinOp(Cmp::Le)),
                ("cmp_partialord_lt", ComparisonOp::BinOp(Cmp::Lt)),
            ]
            .map(|(name, op)| (Symbol::intern(name), op)),
        };
        TypeLimits {
            last_negation: None,
            pat_lits: [None; 2],
            names,
            krate: None,
            pointer_bits: None,
            option: None,
            non_null: HashMap::default(),
        }
    }
}

impl TypeLimits {
    fn crate_of(&mut self, b: &BodyCx<'_, '_, '_>) {
        let db = b.db();
        let krate = b.owner.krate(db);
        if self.krate != Some(krate) {
            self.krate = Some(krate);
            self.pointer_bits = literal::pointer_width(b);
            self.option = lang_items(db, krate).Option;
            self.non_null.clear();
        }
    }

    /// rustc's `diag_item_cmpop`: the comparison a method of `Ord`, `PartialOrd` or `PartialEq`
    /// is.
    fn cmpop(&self, item: Symbol) -> Option<ComparisonOp> {
        self.names.cmpops.iter().find(|(name, _)| *name == item).map(|&(_, op)| op)
    }

    fn is_non_null(&mut self, b: &BodyCx<'_, '_, '_>, adt: AdtId) -> bool {
        let name = self.names.non_null;
        *self.non_null.entry(adt).or_insert_with(|| util::diagnostic_item(b.db(), adt.into()) == Some(name))
    }
}

/// rustc's `hir::BinOpKind` of a comparison.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// rustc's `ComparisonOp`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ComparisonOp {
    BinOp(Cmp),
    Other,
}

impl Cmp {
    fn of(op: CmpOp) -> Cmp {
        match op {
            CmpOp::Eq { negated: false } => Cmp::Eq,
            CmpOp::Eq { negated: true } => Cmp::Ne,
            CmpOp::Ord { ordering: Ordering::Less, strict: true } => Cmp::Lt,
            CmpOp::Ord { ordering: Ordering::Less, strict: false } => Cmp::Le,
            CmpOp::Ord { ordering: Ordering::Greater, strict: true } => Cmp::Gt,
            CmpOp::Ord { ordering: Ordering::Greater, strict: false } => Cmp::Ge,
        }
    }

    fn rev(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            it => it,
        }
    }

    fn is_valid<T: PartialOrd>(self, v: T, min: T, max: T) -> bool {
        match self {
            Cmp::Lt => v > min && v <= max,
            Cmp::Le => v >= min && v < max,
            Cmp::Gt => v >= min && v < max,
            Cmp::Ge => v > min && v <= max,
            Cmp::Eq | Cmp::Ne => v >= min && v <= max,
        }
    }
}

/// What a comparison's operand's type, its references peeled, says of the lints that can
/// apply to it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Operand {
    Float,
    /// A raw pointer or an ADT (`NonNull`, `Option<fn()>`): wide and function pointers.
    PtrOrAdt,
    Fn,
    Other,
}

impl Operand {
    fn of(ty: Ty<'_>) -> Operand {
        match peel_refs(ty).0.kind() {
            TyKind::Float(_) => Operand::Float,
            TyKind::RawPtr(..) | TyKind::Adt(..) => Operand::PtrOrAdt,
            TyKind::FnDef(..) | TyKind::FnPtr(..) => Operand::Fn,
            _ => Operand::Other,
        }
    }
}

fn peel_refs(mut ty: Ty<'_>) -> (Ty<'_>, usize) {
    let mut refs = 0;
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        ty = inner;
        refs += 1;
    }
    (ty, refs)
}

impl LatePass for TypeLimits {
    const LINTS: LintSet = lint_set([
        literal::LINT,
        "unused_comparisons",
        "invalid_nan_comparisons",
        "ambiguous_wide_pointer_comparisons",
        "unpredictable_function_pointer_comparisons",
    ]);
    const HOOKS: Hooks = Hooks::BODY;
    const EXPRS: ExprKinds = ExprKinds::of(&[
        ExprKind::Literal,
        ExprKind::UnaryOp,
        ExprKind::BinaryOp,
        ExprKind::Call,
        ExprKind::MethodCall,
    ]);
    const PATS: PatKinds = PatKinds::of(&[PatKind::Lit, PatKind::Range]);

    fn check_body(&mut self, b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {
        self.crate_of(b);
        // Expression ids are of the body: one of the body before is no node of this one.
        self.last_negation = None;
        self.pat_lits = [None; 2];
    }

    fn check_pat(&mut self, b: &BodyCx<'_, '_, '_>, pat: PatId, _out: &mut Vec<Found>) {
        match b.store().pat(pat) {
            Pat::Lit(expr) => self.pat_lits = [Some(expr), None],
            Pat::Range { start, end, .. } => self.pat_lits = [start, end],
            _ => {}
        }
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        match b.store().expr(expr) {
            Expr::Literal(lit) => {
                let negation = match self.last_negation {
                    Some((negation, operand)) if operand == expr => Some(negation),
                    _ => None,
                };
                let may_overflow = match self.pat_lits.contains(&Some(expr)) {
                    true => literal::pat_may_overflow(b, expr, lit, self.pointer_bits),
                    false => literal::may_overflow(b, expr, lit, negation.is_some(), self.pointer_bits),
                };
                if may_overflow {
                    literal::lint_literal(b, expr, negation, out);
                }
            }
            Expr::UnaryOp { expr: operand, op: UnaryOp::Neg } => {
                if self.last_negation.is_none_or(|(_, negated)| negated != expr) {
                    self.last_negation = Some((expr, operand));
                }
            }
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::CmpOp(op)) } => {
                let op = Cmp::of(op);
                if !check_limits(b, op, lhs, rhs) {
                    let (Some(at), Some(span)) = (b.expr_node(expr), util::span(b, expr)) else { return };
                    let diag = Diag::lint("unused_comparisons", "comparison is useless due to type limits", span);
                    out.push(b.lint(diag, Phase::Late, at));
                    return;
                }
                let (l, r) = (Operand::of(util::ty(b, lhs)), Operand::of(util::ty(b, rhs)));
                if l == Operand::Other && r == Operand::Other {
                    return;
                }
                let cmpop = ComparisonOp::BinOp(op);
                if l == Operand::Float || r == Operand::Float {
                    out.extend(self.lint_nan(b, expr, op, lhs, rhs));
                }
                self.pointers(b, expr, l, cmpop, lhs, rhs, out);
            }
            Expr::Call { args: &[l, r], .. } => {
                let operand = Operand::of(util::ty(b, l));
                if !matches!(operand, Operand::PtrOrAdt | Operand::Fn) {
                    return;
                }
                let Some((item, _)) = util::called_item(b, expr) else { return };
                let Some(cmpop) = self.cmpop(item) else { return };
                self.pointers(b, expr, operand, cmpop, l, r, out);
            }
            Expr::MethodCall { receiver, args: &[arg], .. } => {
                let operand = Operand::of(util::ty(b, receiver));
                if !matches!(operand, Operand::PtrOrAdt | Operand::Fn) {
                    return;
                }
                let Some(def) = util::method_fn(b, expr) else { return };
                let Some(cmpop) = util::fn_item(b.db(), def).and_then(|it| self.cmpop(it)) else { return };
                self.pointers(b, expr, operand, cmpop, receiver, arg, out);
            }
            _ => {}
        }
    }
}

impl TypeLimits {
    /// The wide and function pointer lints of a comparison whose left operand is `l`.
    #[allow(clippy::too_many_arguments)]
    fn pointers(
        &mut self,
        b: &BodyCx<'_, '_, '_>,
        e: ExprId,
        l: Operand,
        cmpop: ComparisonOp,
        lhs: ExprId,
        rhs: ExprId,
        out: &mut Vec<Found>,
    ) {
        if l == Operand::PtrOrAdt {
            out.extend(self.lint_wide_pointer(b, e, cmpop, lhs, rhs));
        }
        if matches!(l, Operand::PtrOrAdt | Operand::Fn) {
            out.extend(self.lint_fn_pointer(b, e, cmpop, lhs, rhs));
        }
    }
}

/// rustc's `check_limits`: whether a comparison of an integer with a literal is not decided by
/// the integer's type alone. It reads the lowered literal; only a comparison it would lint
/// reads the source, to tell a byte literal (`b'a'`, lowered as `97u8`), which rustc's
/// `LitKind::Byte` leaves alone, from an integer.
fn check_limits(b: &BodyCx<'_, '_, '_>, op: Cmp, l: ExprId, r: ExprId) -> bool {
    let store = b.store();
    let (lit_expr, lit, expr, swap) = match (store.expr(l), store.expr(r)) {
        (Expr::Literal(lit), _) => (l, lit, r, true),
        (_, Expr::Literal(lit)) => (r, lit, l, false),
        _ => return true,
    };
    let norm = if swap { op.rev() } else { op };
    let valid = match util::ty(b, expr).kind() {
        TyKind::Int(t) => {
            let &Literal::Int(value, _) = lit else { return true };
            let (min, max) = literal::int_ty_range(t);
            norm.is_valid(value, min, max)
        }
        TyKind::Uint(t) => {
            let value = match *lit {
                Literal::Int(value, _) => value as u128,
                Literal::Uint(value, _) => value,
                _ => return true,
            };
            let (min, max) = literal::uint_ty_range(t);
            norm.is_valid(value, min, max)
        }
        _ => return true,
    };
    valid || !is_int_number(b, lit_expr)
}

/// Whether the literal `expr` is written as an integer (rustc's `LitKind::Int`).
fn is_int_number(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    let Some(node) = util::syntax(b, expr) else { return false };
    let Some(lit) = ast::Literal::cast(node.value) else { return false };
    matches!(lit.kind(), ast::LiteralKind::IntNumber(_))
}

/// rustc's `Span::until`: from the start of `span` to the start of `end`.
fn until(span: &Span, end: &Span) -> Span {
    span.with_hi(end.lo.max(span.lo))
}

/// Which side of a comparison is NaN, for `lint_nan`'s cases.
enum Nan {
    EqLeft,
    EqRight,
    Ord,
}

impl TypeLimits {
    /// rustc's `is_nan` of `lint_nan`: a path to `f32::NAN` and the rest, through blocks and
    /// borrows. A path is resolved only when it is one.
    fn is_nan(&self, b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
        let expr = util::peel_borrows(b, util::peel_blocks(b, expr));
        if !matches!(b.store().expr(expr), Expr::Path(_)) {
            return false;
        }
        util::path_item(b, expr).is_some_and(|it| self.names.nan.contains(&it))
    }

    /// rustc's `lint_nan`.
    fn lint_nan(&self, b: &BodyCx<'_, '_, '_>, e: ExprId, op: Cmp, l: ExprId, r: ExprId) -> Option<Found> {
        let nan = match op {
            Cmp::Eq | Cmp::Ne if self.is_nan(b, l) => Nan::EqLeft,
            Cmp::Eq | Cmp::Ne if self.is_nan(b, r) => Nan::EqRight,
            Cmp::Lt | Cmp::Le | Cmp::Gt | Cmp::Ge if self.is_nan(b, l) || self.is_nan(b, r) => Nan::Ord,
            _ => return None,
        };
        const LINT: &str = "invalid_nan_comparisons";
        const HELP: &str = "use `f32::is_nan()` or `f64::is_nan()` instead";
        let at = b.expr_node(e)?;
        let e_span = util::span(b, e)?;
        if let Nan::Ord = nan {
            let diag = Diag::lint(LINT, "incorrect NaN comparison, NaN is not orderable", e_span);
            return Some(b.lint(diag, Phase::Late, at));
        }
        let mut diag =
            // clone: a span, for the invalid_nan_comparisons lint
            Diag::lint(LINT, "incorrect NaN comparison, NaN cannot be directly compared to itself", e_span.clone());
        let spans = util::span(b, l)
            .zip(util::span(b, r))
            .and_then(|(l, r)| Some((util::ancestor_inside(&l, &e_span)?, util::ancestor_inside(&r, &e_span)?)));
        let Some((l_span, r_span)) = spans else {
            diag.help(HELP);
            return Some(b.lint(diag, Phase::Late, at));
        };
        let (operand, removed) = match nan {
            // clone: a span, for the invalid_nan_comparisons lint
            Nan::EqLeft => (r_span.clone(), until(&l_span, &r_span)),
            // clone: a span, for the invalid_nan_comparisons lint
            _ => (l_span.clone(), l_span.shrink_to_hi().to(&r_span)),
        };
        let mut parts = Vec::with_capacity(3);
        if op == Cmp::Ne {
            // alloc: the invalid_nan_comparisons lint's text
            parts.push((operand.shrink_to_lo(), "!".to_owned()));
        }
        // alloc: the invalid_nan_comparisons lint's text
        parts.push((operand.shrink_to_hi(), ".is_nan()".to_owned()));
        // alloc: the invalid_nan_comparisons lint's text
        parts.push((removed, String::new()));
        diag.suggestion_with_style(parts, HELP, Applicability::MachineApplicable, SuggestionStyle::ShowAlways);
        Some(b.lint(diag, Phase::Late, at))
    }
}

impl TypeLimits {
    /// rustc's `ptr_unsized` of `lint_wide_pointer`: the references around a raw pointer or a
    /// `NonNull` to an unsized type, what gives its pointer, and whether the type is `dyn`.
    fn ptr_unsized<'db>(
        &mut self,
        b: &BodyCx<'_, '_, 'db>,
        solver: &mut Option<Solver<'db>>,
        ty: Ty<'db>,
    ) -> Option<(usize, &'static str, bool)> {
        let (ty, refs) = peel_refs(ty);
        let (pointee, modifiers) = match (ty.kind(), ty.as_adt()) {
            (TyKind::RawPtr(pointee, _), _) => (pointee, ""),
            (_, Some((adt, args))) if self.is_non_null(b, adt) => (args.type_at(0), ".as_ptr()"),
            _ => return None,
        };
        let solver = solver.get_or_insert_with(|| Solver::of(b));
        (!solver.is_sized(pointee)).then(|| (refs, modifiers, matches!(pointee.kind(), TyKind::Dynamic(..))))
    }

    /// rustc's `lint_wide_pointer`.
    fn lint_wide_pointer<'db>(
        &mut self,
        b: &BodyCx<'_, '_, 'db>,
        e: ExprId,
        cmpop: ComparisonOp,
        l: ExprId,
        r: ExprId,
    ) -> Option<Found> {
        const LINT: &str = "ambiguous_wide_pointer_comparisons";
        const MESSAGE: &str =
            "ambiguous wide pointer comparison, the comparison includes metadata which may not be expected";
        let mut solver = None;
        let l = util::peel_borrows(b, l);
        let r = util::peel_borrows(b, r);
        let (l_refs, l_modifiers, l_dyn) = self.ptr_unsized(b, &mut solver, util::ty(b, l))?;
        let (r_refs, r_modifiers, r_dyn) = self.ptr_unsized(b, &mut solver, util::ty(b, r))?;
        let e_span = &util::span(b, e)?;
        let at = b.expr_node(e)?;
        // clone: a span, for the ambiguous_wide_pointer_comparisons lint
        let mut diag = Diag::lint(LINT, MESSAGE, e_span.clone());
        let spans = util::span(b, l)
            .zip(util::span(b, r))
            .and_then(|(l, r)| Some((util::ancestor_inside(&l, e_span)?, util::ancestor_inside(&r, e_span)?)));
        let Some((l_span, r_span)) = spans else {
            diag.help("use explicit `std::ptr::eq` method to compare metadata and addresses");
            diag.help("use `std::ptr::addr_eq` or untyped pointers to only compare their addresses");
            return Some(b.lint(diag, Phase::Late, at));
        };
        let ne = if cmpop == ComparisonOp::BinOp(Cmp::Ne) { "!" } else { "" };
        let is_eq_ne = matches!(cmpop, ComparisonOp::BinOp(Cmp::Eq | Cmp::Ne));
        let is_dyn_comparison = l_dyn && r_dyn;
        let via_method_call = matches!(b.store().expr(e), Expr::MethodCall { .. } | Expr::Call { .. });
        let left = until(&e_span.shrink_to_lo(), &l_span.shrink_to_lo());
        let middle = until(&l_span.shrink_to_hi(), &r_span.shrink_to_lo());
        let right = until(&r_span.shrink_to_hi(), &e_span.shrink_to_hi());
        let deref_left = "*".repeat(l_refs);
        let deref_right = "*".repeat(r_refs);
        let verbose = |diag: &mut Diag, message: &str, parts: Vec<(Span, String)>| {
            diag.suggestion_with_style(parts, message, Applicability::MaybeIncorrect, SuggestionStyle::ShowAlways);
        };
        let eq_parts = |function: &str| {
            // alloc: the ambiguous_wide_pointer_comparisons lint's text
            vec![
                // alloc: the ambiguous_wide_pointer_comparisons lint's text; clone: a span, for the ambiguous_wide_pointer_comparisons lint
                (left.clone(), format!("{ne}std::ptr::{function}({deref_left}")),
                // alloc: the ambiguous_wide_pointer_comparisons lint's text; clone: a span, for the ambiguous_wide_pointer_comparisons lint
                (middle.clone(), format!("{l_modifiers}, {deref_right}")),
                // alloc: the ambiguous_wide_pointer_comparisons lint's text; clone: a span, for the ambiguous_wide_pointer_comparisons lint
                (right.clone(), format!("{r_modifiers})")),
            ]
        };
        if is_eq_ne {
            verbose(
                &mut diag,
                "use `std::ptr::addr_eq` or untyped pointers to only compare their addresses",
                eq_parts("addr_eq"),
            );
            if !is_dyn_comparison {
                verbose(&mut diag, "use explicit `std::ptr::eq` method to compare metadata and addresses", eq_parts("eq"));
            }
        } else {
            let paren_left = if l_refs != 0 { ")" } else { "" };
            let paren_right = if r_refs != 0 { ")" } else { "" };
            // alloc: the ambiguous_wide_pointer_comparisons lint's text
            let mut parts = Vec::new();
            if l_refs != 0 {
                // alloc: the ambiguous_wide_pointer_comparisons lint's text
                parts.push((l_span.shrink_to_lo(), format!("({deref_left}")));
            }
            // alloc: the ambiguous_wide_pointer_comparisons lint's text
            parts.push((l_span.shrink_to_hi(), format!("{l_modifiers}{paren_left}.cast::<()>()")));
            if r_refs != 0 {
                // alloc: the ambiguous_wide_pointer_comparisons lint's text
                parts.push((r_span.shrink_to_lo(), format!("({deref_right}")));
            }
            // alloc: the ambiguous_wide_pointer_comparisons lint's text
            parts.push((r_span.shrink_to_hi(), format!("{r_modifiers}{paren_right}.cast::<()>()")));
            verbose(&mut diag, "use untyped pointers to only compare their addresses", parts);
            let (open, close) = if via_method_call { ("", "") } else { ("(", ")") };
            verbose(
                &mut diag,
                "or expect the lint to compare the pointers metadata and addresses",
                // alloc: the ambiguous_wide_pointer_comparisons lint's text
                vec![
                    (
                        e_span.shrink_to_lo(),
                        // alloc: the ambiguous_wide_pointer_comparisons lint's text
                        format!(r#"{{ #[expect(ambiguous_wide_pointer_comparisons, reason = "...")] {open}"#),
                    ),
                    // alloc: the ambiguous_wide_pointer_comparisons lint's text
                    (e_span.shrink_to_hi(), format!("{close} }}")),
                ],
            );
        }
        Some(b.lint(diag, Phase::Late, at))
    }

    /// rustc's `lint_fn_pointer`.
    fn lint_fn_pointer<'db>(
        &self,
        b: &BodyCx<'_, '_, 'db>,
        e: ExprId,
        cmpop: ComparisonOp,
        l: ExprId,
        r: ExprId,
    ) -> Option<Found> {
        const LINT: &str = "unpredictable_function_pointer_comparisons";
        let is_fn = |ty: Ty<'db>| matches!(ty.kind(), TyKind::FnDef(..) | TyKind::FnPtr(..));
        let l = util::peel_borrows(b, l);
        let r = util::peel_borrows(b, r);
        let (l_ty, l_refs) = peel_refs(util::ty(b, l));
        let (r_ty, r_refs) = peel_refs(util::ty(b, r));
        if !(is_fn(l_ty) && is_fn(r_ty)) {
            let some_fn = |ty: Ty<'db>| match ty.as_adt() {
                Some((adt, args)) => self.option.is_some_and(|it| AdtId::from(it) == adt) && is_fn(args.type_at(0)),
                None => false,
            };
            if !(some_fn(l_ty) && some_fn(r_ty)) {
                return None;
            }
        }
        let at = b.expr_node(e)?;
        let e_span = &util::span(b, e)?;
        let warn = || {
            let mut diag = Diag::lint(
                LINT,
                "function pointer comparisons do not produce meaningful results since their addresses are not guaranteed to be unique",
                // clone: a span, for the unpredictable_function_pointer_comparisons lint
                e_span.clone(),
            );
            diag.note("the address of the same function can vary between different codegen units");
            diag.note("furthermore, different functions could have the same address after being merged together");
            diag.note("for more information visit <https://doc.rust-lang.org/nightly/core/ptr/fn.fn_addr_eq.html>");
            diag
        };
        if !(is_fn(l_ty) && is_fn(r_ty)) {
            return Some(b.lint(warn(), Phase::Late, at));
        }
        let is_eq_ne = matches!(cmpop, ComparisonOp::BinOp(Cmp::Eq | Cmp::Ne));
        let spans = util::span(b, l)
            .zip(util::span(b, r))
            .and_then(|(l, r)| Some((util::ancestor_inside(&l, e_span)?, util::ancestor_inside(&r, e_span)?)));
        let (true, Some((l_span, r_span))) = (is_eq_ne, spans) else {
            return Some(b.lint(warn(), Phase::Late, at));
        };
        let ne = if cmpop == ComparisonOp::BinOp(Cmp::Ne) { "!" } else { "" };
        let deref_left = "*".repeat(l_refs);
        let deref_right = "*".repeat(r_refs);
        let left = until(&e_span.shrink_to_lo(), &l_span.shrink_to_lo());
        let middle = until(&l_span.shrink_to_hi(), &r_span.shrink_to_lo());
        let right = until(&r_span.shrink_to_hi(), &e_span.shrink_to_hi());
        let close = if matches!(r_ty.kind(), TyKind::FnPtr(..)) {
            // alloc: the unpredictable_function_pointer_comparisons lint's text
            ")".to_owned()
        } else {
            let sig = r_ty.callable_sig(interner(b))?;
            let fn_ptr = Ty::new_fn_ptr(interner(b), sig);
            // alloc: the unpredictable_function_pointer_comparisons lint's text
            format!(" as {})", util::print_ty(b, r, fn_ptr)?)
        };
        let mut diag = warn();
        diag.suggestion_with_style(
            // alloc: the unpredictable_function_pointer_comparisons lint's text
            vec![
                // alloc: the unpredictable_function_pointer_comparisons lint's text
                (left, format!("{ne}std::ptr::fn_addr_eq({deref_left}")),
                // alloc: the unpredictable_function_pointer_comparisons lint's text
                (middle, format!(", {deref_right}")),
                (right, close),
            ],
            "refactor your code, or use `std::ptr::fn_addr_eq` to suppress the lint",
            Applicability::MaybeIncorrect,
            SuggestionStyle::ShowAlways,
        );
        Some(b.lint(diag, Phase::Late, at))
    }
}

fn interner<'db>(b: &BodyCx<'_, '_, 'db>) -> DbInterner<'db> {
    DbInterner::new_with(b.db(), b.owner.krate(b.db()))
}
