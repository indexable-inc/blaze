//! rustc's `MutableTransmutes` (`mutable_transmutes`): `transmute` of `&T` to `&mut T`, and
//! `CheckTransmutes`: a pointer transmuted to an integer in a `const fn` or an associated const
//! (`ptr_to_integer_transmute_in_consts`), a transmute std has a method for
//! (`unnecessary_transmutes`), and an integer transmuted to a pointer
//! (`integer_to_ptr_transmutes`).
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`MutableTransmutes`),
//! `compiler/rustc_lint/src/transmute.rs` (`CheckTransmutes`, `check_ptr_transmute_in_const`,
//! `check_unnecessary_transmute`, `check_int_to_ptr_transmute`, `UndefinedTransmuteLint`) and
//! `compiler/rustc_lint/src/lints.rs` (`BuiltinMutablesTransmutes`, `IntegerToPtrTransmutes`,
//! `IntegerToPtrTransmutesSuggestion`) (MIT or Apache-2.0).

use blaze_hash::HashMap;
use blaze_items::{Fact, ItemId};
use hir_def::{
    CallableDefId, DefWithBodyId, ItemContainerId, Lookup as _,
    hir::{Expr, ExprId, Literal},
    resolver::ValueNs,
    signatures::FunctionSignature,
};
use hir_ty::next_solver::{CallableIdWrapper, Ty, TyKind};
use rustc_type_ir::{
    IntTy, UintTy,
    inherent::{GenericArgs as _, IntoKind as _},
};

use crate::{
    lints::bodies::{BodyCx, ExprKind, ExprKinds, Hooks, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

#[derive(Default)]
pub(crate) struct MutableTransmutes;

impl LatePass for MutableTransmutes {
    const LINTS: LintSet = lint_set(["mutable_transmutes"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Path]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some((from, to)) = transmute_types(b, expr) else { return };
        let (TyKind::Ref(_, _, from), TyKind::Ref(_, _, to)) = (from.kind(), to.kind()) else { return };
        if from >= to {
            return;
        }
        let (Some(span), Some(node)) = (util::span(b, expr), b.expr_node(expr)) else { return };
        let diag = Diag::lint(
            "mutable_transmutes",
            "transmuting &T to &mut T is undefined behavior, even if the reference is unused, consider instead using an UnsafeCell",
            span,
        );
        out.push(b.lint(diag, Phase::Late, node));
    }
}

/// rustc's `get_transmute_from_to`: the types the path `path` transmutes from and to, when it
/// names the intrinsic `transmute` (rustc's `qpath_res` of it is a `DefKind::Fn`).
fn transmute_types<'db>(b: &BodyCx<'_, '_, 'db>, path: ExprId) -> Option<(Ty<'db>, Ty<'db>)> {
    let Expr::Path(_) = b.store().expr(path) else { return None };
    let TyKind::FnDef(CallableIdWrapper(callable), args) = util::ty(b, path).kind() else { return None };
    // The path's type names the function it can resolve to: test that before resolving it.
    let CallableDefId::FunctionId(named) = callable else { return None };
    if !super::is_transmute(b.db(), named) {
        return None;
    }
    let ValueNs::FunctionId(def) = util::path_value(b, path)? else { return None };
    if !super::is_transmute(b.db(), def) {
        return None;
    }
    let args = args.skip_binder();
    Some((args.type_at(0), args.type_at(1)))
}

/// What is a body of its own to rustc inside a body of hir's: a closure or an inline `const`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Inner {
    Closure,
    Const,
}

/// The expressions of the body that are in a closure or an inline `const`, with the innermost
/// one: rustc's `hir_enclosing_body_owner` of each when it is not the body's owner. Built by a
/// walk of the body the first time a transmute in it is found, which most bodies never have.
#[derive(Default)]
pub(crate) struct CheckTransmutes {
    inner: HashMap<ExprId, Inner>,
    stack: Vec<(ExprId, Option<Inner>)>,
    /// Whether `inner` is of the body being walked.
    built: bool,
}

/// rustc's `hir_body_const_context` of the body an expression is in, and whether that body is
/// an associated const's.
#[derive(Clone, Copy)]
struct ConstContext {
    any: bool,
    const_fn: bool,
    assoc_const: bool,
}

impl CheckTransmutes {
    fn build(&mut self, b: &BodyCx<'_, '_, '_>) {
        let store = b.store();
        self.built = true;
        self.inner.clear();
        self.stack.clear();
        self.stack.push((b.body.root_expr(), None));
        while let Some((expr, inner)) = self.stack.pop() {
            if let Some(inner) = inner {
                self.inner.insert(expr, inner);
            }
            let children = match store.expr(expr) {
                Expr::Closure { .. } => Some(Inner::Closure),
                Expr::Const(_) => Some(Inner::Const),
                _ => inner,
            };
            let stack = &mut self.stack;
            store.walk_child_exprs(expr, |it| stack.push((it, children)));
        }
    }

    fn context(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId) -> ConstContext {
        if !self.built {
            self.build(b);
        }
        let db = b.db();
        match self.inner.get(&expr) {
            Some(Inner::Closure) => ConstContext { any: false, const_fn: false, assoc_const: false },
            Some(Inner::Const) => ConstContext { any: true, const_fn: false, assoc_const: false },
            None => match b.owner {
                DefWithBodyId::FunctionId(it) => {
                    let const_fn = FunctionSignature::of(db, it).is_const();
                    ConstContext { any: const_fn, const_fn, assoc_const: false }
                }
                DefWithBodyId::ConstId(it) => {
                    let container = it.lookup(db).container;
                    let assoc_const = matches!(container, ItemContainerId::ImplId(_) | ItemContainerId::TraitId(_));
                    ConstContext { any: true, const_fn: false, assoc_const }
                }
                DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => {
                    ConstContext { any: true, const_fn: false, assoc_const: false }
                }
            },
        }
    }
}

impl LatePass for CheckTransmutes {
    const LINTS: LintSet =
        lint_set(["ptr_to_integer_transmute_in_consts", "unnecessary_transmutes", "integer_to_ptr_transmutes"]);
    const HOOKS: Hooks = Hooks::BODY;
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call]);

    fn check_body(&mut self, _b: &BodyCx<'_, '_, '_>, _out: &mut Vec<Found>) {
        self.built = false;
    }

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Expr::Call { callee, args: &[arg] } = b.store().expr(expr) else { return };
        let Some((src, dst)) = transmute_types(b, callee) else { return };
        let (Some(span), Some(node)) = (util::span(b, expr), b.expr_node(expr)) else { return };
        let context = self.context(b, expr);

        if (context.const_fn || context.assoc_const)
            && src.is_raw_ptr()
            && matches!(dst.kind(), TyKind::Int(_) | TyKind::Uint(_))
        {
            let mut diag = Diag::lint(
                "ptr_to_integer_transmute_in_consts",
                "pointers cannot be transmuted to integers during const eval",
                // clone: a span, for the ptr_to_integer_transmute_in_consts lint
                span.clone(),
            );
            diag.note("at compile-time, pointers do not have an integer value");
            diag.note("avoiding this restriction via `union` or raw pointers leads to compile-time undefined behavior");
            diag.help("for more information, see https://doc.rust-lang.org/std/mem/fn.transmute.html");
            out.push(b.lint(diag, Phase::Late, node));
        }
        if let Some(diag) = unnecessary_transmute(b, &span, callee, arg, context.any, src, dst) {
            out.push(b.lint(diag, Phase::Late, node));
        }
        if let Some(diag) = int_to_ptr_transmute(b, expr, &span, arg, src, dst) {
            out.push(b.lint(diag, Phase::Late, node));
        }
    }
}

/// rustc's name of a primitive type.
fn primitive(ty: Ty<'_>) -> Option<&'static str> {
    match ty.kind() {
        TyKind::Int(it) => Some(it.name_str()),
        TyKind::Uint(it) => Some(it.name_str()),
        TyKind::Float(it) => Some(it.name_str()),
        _ => None,
    }
}

/// rustc's `check_unnecessary_transmute`: a transmute std has a method or a cast for, with it
/// suggested. `u8` to `bool` is left out, as rustc leaves it out.
fn unnecessary_transmute(
    b: &BodyCx<'_, '_, '_>,
    span: &Span,
    callee: ExprId,
    arg: ExprId,
    const_context: bool,
    src: Ty<'_>,
    dst: Ty<'_>,
) -> Option<Diag> {
    let callee_span = util::span(b, callee)?;
    let callee_span = util::ancestor_inside(&callee_span, span).unwrap_or(callee_span);
    let end = span.shrink_to_hi();
    let byte = |ty: Ty<'_>| matches!(ty.kind(), TyKind::Uint(UintTy::U8));
    let (parts, help): (Vec<(Span, String)>, Option<&str>) = match (src.kind(), dst.kind()) {
        (TyKind::Array(elem, _), TyKind::Uint(_) | TyKind::Float(_) | TyKind::Int(_)) if byte(elem) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![(callee_span, format!("{}::from_ne_bytes", primitive(dst)?))],
            Some("there's also `from_le_bytes` and `from_be_bytes` if you expect a particular byte order"),
        ),
        (TyKind::Uint(_) | TyKind::Float(_) | TyKind::Int(_), TyKind::Array(elem, _)) if byte(elem) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![(callee_span, format!("{}::to_ne_bytes", primitive(src)?))],
            Some("there's also `to_le_bytes` and `to_be_bytes` if you expect a particular byte order"),
        ),
        // alloc: the unnecessary_transmutes lint's suggestion
        (TyKind::Char, TyKind::Uint(UintTy::U32)) => (vec![(callee_span, "u32::from".to_owned())], None),
        (TyKind::Char, TyKind::Int(IntTy::I32)) => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, "u32::from".to_owned()), (end, ".cast_signed()".to_owned())], None)
        }
        (TyKind::Uint(UintTy::U32), TyKind::Char) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![(callee_span, "char::from_u32_unchecked".to_owned())],
            Some("consider using `char::from_u32(…).unwrap()`"),
        ),
        (TyKind::Int(IntTy::I32), TyKind::Char) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![(callee_span, "char::from_u32_unchecked(i32::cast_unsigned".to_owned()), (end, ")".to_owned())],
            Some("consider using `char::from_u32(i32::cast_unsigned(…)).unwrap()`"),
        ),
        (TyKind::Uint(_), TyKind::Int(_)) => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, format!("{}::cast_signed", primitive(src)?))], None)
        }
        (TyKind::Int(_), TyKind::Uint(_)) => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, format!("{}::cast_unsigned", primitive(src)?))], None)
        }
        (TyKind::Float(_), TyKind::Uint(UintTy::Usize) | TyKind::Int(IntTy::Isize)) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![
                // alloc: the unnecessary_transmutes lint's suggestion
                (callee_span, format!("{}::to_bits", primitive(src)?)),
                // alloc: the unnecessary_transmutes lint's suggestion
                (end, format!(" as {}", primitive(dst)?)),
            ],
            None,
        ),
        (TyKind::Float(_), TyKind::Int(_)) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![(callee_span, format!("{}::to_bits", primitive(src)?)), (end, ".cast_signed()".to_owned())],
            None,
        ),
        (TyKind::Float(_), TyKind::Uint(_)) => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, format!("{}::to_bits", primitive(src)?))], None)
        }
        (TyKind::Uint(UintTy::Usize) | TyKind::Int(IntTy::Isize), TyKind::Float(_)) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![
                // alloc: the unnecessary_transmutes lint's suggestion
                (callee_span, format!("{}::from_bits", primitive(dst)?)),
                // alloc: the unnecessary_transmutes lint's suggestion
                (util::span(b, arg)?.shrink_to_hi(), " as _".to_owned()),
            ],
            None,
        ),
        (TyKind::Int(_), TyKind::Float(_)) => (
            // alloc: the unnecessary_transmutes lint's suggestion
            vec![
                // alloc: the unnecessary_transmutes lint's suggestion
                (callee_span, format!("{}::from_bits({}::cast_unsigned", primitive(dst)?, primitive(src)?)),
                // alloc: the unnecessary_transmutes lint's suggestion
                (end, ")".to_owned()),
            ],
            None,
        ),
        (TyKind::Uint(_), TyKind::Float(_)) => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, format!("{}::from_bits", primitive(dst)?))], None)
        }
        (TyKind::Bool, TyKind::Int(_) | TyKind::Uint(_)) if const_context => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, String::new()), (end, format!(" as {}", primitive(dst)?))], None)
        }
        (TyKind::Bool, TyKind::Int(_) | TyKind::Uint(_)) => {
            // alloc: the unnecessary_transmutes lint's suggestion
            (vec![(callee_span, format!("{}::from", primitive(dst)?))], None)
        }
        _ => return None,
    };
    // clone: a span, for the unnecessary_transmutes lint
    let mut diag = Diag::lint("unnecessary_transmutes", "unnecessary transmute", span.clone());
    diag.multipart_suggestion("replace this with", parts, Applicability::MachineApplicable);
    if let Some(help) = help {
        diag.help(help);
    }
    Some(diag)
}

/// rustc's `check_int_to_ptr_transmute`: an integer transmuted to a raw pointer or a reference
/// to a type that is not a 1-ZST, other than the literal `0` (`invalid_value` and
/// `invalid_null_arguments` lint that).
fn int_to_ptr_transmute<'db>(
    b: &BodyCx<'_, '_, 'db>,
    expr: ExprId,
    span: &Span,
    arg: ExprId,
    src: Ty<'db>,
    dst: Ty<'db>,
) -> Option<Diag> {
    if !matches!(src.kind(), TyKind::Uint(_) | TyKind::Int(_)) {
        return None;
    }
    let (inner, mutbl, is_ref) = match dst.kind() {
        TyKind::Ref(_, inner, mutbl) => (inner, mutbl, true),
        TyKind::RawPtr(inner, mutbl) => (inner, mutbl, false),
        _ => return None,
    };
    if matches!(util::literal(b, arg), Some(Literal::Int(0, _) | Literal::Uint(0, _))) {
        return None;
    }
    let layout = super::super::pointers::layout(b, inner)?;
    if layout.is_1zst {
        return None;
    }
    let mut diag = Diag::lint(
        "integer_to_ptr_transmutes",
        "transmuting an integer to a pointer creates a pointer without provenance",
        // clone: a span, for the integer_to_ptr_transmutes lint
        span.clone(),
    );
    diag.note("this is dangerous because dereferencing the resulting pointer is undefined behavior");
    diag.note(
        "exposed provenance semantics can be used to create a pointer based on some previously exposed provenance",
    );
    diag.help("if you truly mean to create a pointer without provenance, use `std::ptr::without_provenance_mut`");
    diag.help("for more information about transmute, see <https://doc.rust-lang.org/std/mem/fn.transmute.html#transmutation-between-pointers-and-integers>");
    diag.help("for more information about exposed provenance, see <https://doc.rust-lang.org/std/ptr/index.html#exposed-provenance>");
    let printed = super::print(b, expr, inner)?;
    if let TyKind::Adt(def, _) = inner.kind() {
        // alloc: the integer_to_ptr_transmutes lint's text
        diag.fact(Fact::Layout(b.cx.key(ItemId::Adt(def.def_id()))), format!("the layout of `{printed}`"));
    }
    if !layout.is_unsized {
        let suffix = if mutbl.is_mut() { "_mut" } else { "" };
        let start_call = span.with_hi(util::span(b, arg)?.lo);
        let code = if is_ref {
            // alloc: the integer_to_ptr_transmutes lint's text
            format!("&{}*std::ptr::with_exposed_provenance{suffix}::<{printed}>(", mutbl.prefix_str())
        } else {
            // alloc: the integer_to_ptr_transmutes lint's text
            format!("std::ptr::with_exposed_provenance{suffix}::<{printed}>(")
        };
        diag.multipart_suggestion(
            // alloc: the integer_to_ptr_transmutes lint's text
            format!("use `std::ptr::with_exposed_provenance{suffix}` instead to use a previously exposed provenance"),
            // alloc: the integer_to_ptr_transmutes lint's text
            vec![(start_call, code)],
            Applicability::MachineApplicable,
        );
    }
    Some(diag)
}
