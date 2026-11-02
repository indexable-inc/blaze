//! rustc's `InvalidValue` (`invalid_value`): a value of a type made of zeroed or uninitialized
//! memory, which the type does not permit.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`InvalidValue`, `ty_find_init_error`,
//! `variant_find_init_error`) and `compiler/rustc_lint/src/lints.rs`
//! (`BuiltinUnpermittedTypeInit`) (MIT or Apache-2.0).

use hir_ty::db::HirDatabase;
use blaze_items::{Fact, ItemId};
use hir::InFile;
use hir_def::{
    AdtId, HasModule as _, Lookup, VariantId,
    hir::{Expr, ExprId, Literal},
    src::{HasChildSourceView as _, HasSource as _},
};
use hir_ty::{
    ParamEnvAndCrate,
    consteval::try_const_usize,
    next_solver::{GenericArgs, Ty, TyKind},
};
use rustc_abi::BackendRepr;
use rustc_type_ir::inherent::IntoKind as _;
use syntax::view::AstNode as _;

use super::inhabited::{Pred, Preds};
use crate::{
    cx::def_range_view,
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::{Diag, Span},
};

#[derive(Default)]
pub(crate) struct InvalidValue;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Init {
    Zeroed,
    Uninit,
}

/// rustc's `InitError`, outermost first: why the type cannot be made this way, each part a
/// note at the field or type it is of, if any.
struct InitError(Vec<(String, Option<Span>)>);

impl InitError {
    fn new(message: impl Into<String>) -> InitError {
        // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
        InitError(vec![(message.into(), None)])
    }

    fn spanned(mut self, span: Span) -> InitError {
        self.0[0].1 = Some(span);
        self
    }

    fn nested(mut self, nested: Option<InitError>) -> InitError {
        if let Some(nested) = nested {
            self.0.extend(nested.0);
        }
        self
    }
}

impl LatePass for InvalidValue {
    const LINTS: LintSet = lint_set(["invalid_value"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call, ExprKind::MethodCall]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some(init) = dangerous_init(b, expr) else { return };
        let solver = util::Solver::of(b);
        let interner = solver.interner();
        let conjured = super::erased(interner, util::ty(b, expr));
        let mut cx = InitCx {
            b,
            solver: &solver,
            near: expr,
            preds: Preds::new(&solver, b.cx.krate.base()),
            // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
            layouts: Vec::new(),
        };
        let Some(err) = cx.ty_find_init_error(conjured, init) else { return };
        let (Some(span), Some(node), Some(ty)) = (util::span(b, expr), b.expr_node(expr), super::print(b, expr, conjured))
        else {
            return;
        };
        let message = match init {
            // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
            Init::Zeroed => format!("the type `{ty}` does not permit zero-initialization"),
            // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
            Init::Uninit => format!("the type `{ty}` does not permit being left uninitialized"),
        };
        // clone: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
        let mut diag = Diag::lint("invalid_value", message, span.clone());
        // clone: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
        diag.span_label(span.clone(), "this code causes undefined behavior when executed");
        if cx.preds.ty(conjured) == Pred::True {
            diag.span_label(
                span,
                "help: use `MaybeUninit<T>` instead, and only call `assume_init` after initialization is done",
            );
        }
        for (message, span) in err.0 {
            match span {
                Some(span) => diag.span_note(span, message),
                None => diag.note(message),
            };
        }
        for (adt, what) in cx.layouts {
            diag.fact(Fact::Layout(b.cx.key(ItemId::Adt(adt))), what);
        }
        out.push(b.lint(diag, Phase::Late, node));
    }
}

/// rustc's `is_dangerous_init`: `mem::zeroed()`, `mem::uninitialized()`, a transmute of zero, or
/// `MaybeUninit::zeroed()` or `MaybeUninit::uninit()` with `assume_init` called on it.
fn dangerous_init(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Init> {
    let store = b.store();
    match store.expr(expr) {
        // `mem::zeroed()` and `mem::uninitialized()` take no argument, `transmute` one.
        Expr::Call { callee, args } if args.len() <= 1 => {
            let Expr::Path(_) = store.expr(callee) else { return None };
            let (name, items) = (util::path_item(b, callee)?, util::items());
            if name == items.mem_zeroed {
                Some(Init::Zeroed)
            } else if name == items.mem_uninitialized {
                Some(Init::Uninit)
            } else if name == items.transmute && args.first().is_some_and(|&it| is_zero(b, it)) {
                Some(Init::Zeroed)
            } else {
                None
            }
        }
        Expr::MethodCall { receiver, args: &[], .. } => {
            let Expr::Call { callee, args: &[] } = store.expr(receiver) else { return None };
            let Expr::Path(_) = store.expr(callee) else { return None };
            let def = util::method_fn(b, expr)?;
            let items = util::items();
            if util::fn_item(b.db(), def) != Some(items.assume_init) {
                return None;
            }
            let name = util::path_item(b, callee)?;
            if name == items.maybe_uninit_zeroed {
                Some(Init::Zeroed)
            } else if name == items.maybe_uninit_uninit {
                Some(Init::Uninit)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// rustc's `is_zero`: an integer literal `0`, or a tuple of them.
fn is_zero(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    if let Some(literal) = util::literal(b, expr) {
        return matches!(literal, Literal::Int(0, _) | Literal::Uint(0, _));
    }
    match b.store().expr(expr) {
        Expr::Tuple { exprs } => exprs.iter().all(|&it| is_zero(b, it)),
        _ => false,
    }
}

struct InitCx<'s, 'a, 'db> {
    b: &'s BodyCx<'s, 'a, 'db>,
    solver: &'s util::Solver<'db>,
    /// The expression the types are printed as types of the body near.
    near: ExprId,
    preds: Preds<'s, 'db>,
    /// The ADTs whose layout was read, for the facts.
    layouts: Vec<(AdtId, String)>,
}

impl<'db> InitCx<'_, '_, 'db> {
    /// rustc's `ty_find_init_error`: why `ty` does not permit `init`, only when that is sure.
    fn ty_find_init_error(&mut self, ty: Ty<'db>, init: Init) -> Option<InitError> {
        let ty = super::erased(self.solver.interner(), self.preds.normalize(ty).unwrap_or(ty));
        let uninit = init == Init::Uninit;
        match ty.kind() {
            TyKind::Ref(..) => Some(InitError::new("references must be non-null")),
            TyKind::Adt(def, _) if def.is_box() => Some(InitError::new("`Box` must be non-null")),
            TyKind::FnPtr(..) => Some(InitError::new("function pointers must be non-null")),
            TyKind::Never => Some(InitError::new("the `!` type has no valid value")),
            TyKind::RawPtr(pointee, _) if matches!(pointee.kind(), TyKind::Dynamic(..)) => {
                Some(InitError::new("the vtable of a wide raw pointer must be non-null"))
            }
            TyKind::Bool if uninit => Some(InitError::new("booleans must be either `true` or `false`")),
            TyKind::Char if uninit => Some(InitError::new("characters must be a valid Unicode codepoint")),
            TyKind::Int(_) | TyKind::Uint(_) if uninit => Some(InitError::new("integers must be initialized")),
            TyKind::Float(_) if uninit => Some(InitError::new("floats must be initialized")),
            TyKind::RawPtr(..) if uninit => Some(InitError::new("raw pointers must be initialized")),
            TyKind::Adt(def, args) if !def.is_union() => match def.def_id() {
                AdtId::StructId(it) => {
                    self.variant_find_init_error(ty, VariantId::StructId(it), args, "struct field", init)
                }
                AdtId::EnumId(it) => self.enum_find_init_error(ty, it, args, init),
                AdtId::UnionId(_) => None,
            },
            TyKind::Tuple(tys) => tys.iter().find_map(|field| self.ty_find_init_error(field, init)),
            TyKind::Array(elem, len) => match try_const_usize(self.b.db(), len) {
                Some(1..) => self.ty_find_init_error(elem, init),
                _ => None,
            },
            _ => None,
        }
    }

    /// The enum case of rustc's `ty_find_init_error`.
    fn enum_find_init_error(
        &mut self,
        ty: Ty<'db>,
        def: hir_def::EnumId,
        args: GenericArgs<'db>,
        init: Init,
    ) -> Option<InitError> {
        let db = self.b.db();
        let source = def.lookup(db).source_view(db);
        let span = self.b.cx.span(InFile::new(source.file_id, def_range_view(source.value.syntax())));
        // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
        let mut potential = Vec::new();
        for &(variant, _) in def.enum_variants(db).variants.values() {
            let pred = self.preds.variant(VariantId::EnumVariantId(variant), true);
            let pred = self.preds.instantiate(pred, args);
            match self.preds.apply_any_module(pred) {
                Some(false) => {}
                Some(true) => potential.push((variant, true)),
                None => potential.push((variant, false)),
            }
        }
        match potential.as_slice() {
            [] => Some(InitError::new("enums with no inhabited variants have no valid value").spanned(span)),
            &[(variant, _)] => self.variant_find_init_error(
                ty,
                VariantId::EnumVariantId(variant),
                args,
                "field of the only potentially inhabited enum variant",
                init,
            ),
            _ if init == Init::Uninit && potential.iter().filter(|(_, definitely)| *definitely).count() > 1 => {
                Some(
                    InitError::new("enums with multiple inhabited variants have to be initialized to a variant")
                        .spanned(span),
                )
            }
            _ => None,
        }
    }

    /// rustc's `variant_find_init_error`: the first field that does not permit `init`, then
    /// the layout of `ty` when its valid range leaves zero or uninitialized memory out.
    fn variant_find_init_error(
        &mut self,
        ty: Ty<'db>,
        variant: VariantId,
        args: GenericArgs<'db>,
        descr: &str,
        init: Init,
    ) -> Option<InitError> {
        let b = self.b;
        let db = b.db();
        let interner = self.solver.interner();
        let types = db.field_types(variant);
        let local = variant.krate(db) == b.cx.krate.base();
        let mut field_err = None;
        for (field, _) in variant.fields(db).fields().iter() {
            let field_ty = types[field].ty().instantiate(interner, args).skip_norm_wip();
            let Some(mut err) = self.ty_find_init_error(field_ty, init) else { continue };
            if local {
                let source = variant.child_source_view(db, field);
                let node = source.value.expect("a local variant's field has a node").either(|it| it.syntax(), |it| it.syntax());
                let span = b.cx.span(InFile::new(source.file_id, def_range_view(node)));
                if err.0[0].1.is_none() {
                    err.0[0].1 = Some(span);
                    // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
                    err.0[0].0.push_str(&format!(" (in this {descr})"));
                } else {
                    // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
                    err = InitError::new(format!("in this {descr}")).spanned(span).nested(Some(err));
                }
            }
            field_err = Some(err);
            break;
        }

        let krate = b.owner.krate(db);
        let env = ParamEnvAndCrate { param_env: self.solver.env, krate };
        let Ok(layout) = db.layout_of_ty(ty.store(), env.store()) else { return field_err };
        let Ok(data_layout) = db.target_data_layout(krate) else { return field_err };
        let adt = match ty.kind() {
            TyKind::Adt(def, _) => def.def_id(),
            _ => return field_err,
        };
        let Some(printed) = super::print_untrimmed(b, self.near, ty) else { return field_err };
        // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
        self.layouts.push((adt, format!("the layout of `{printed}`")));
        let scalar = match &layout.backend_repr {
            BackendRepr::Scalar(scalar) | BackendRepr::ScalarPair { a: scalar, .. } => *scalar,
            _ => return field_err,
        };
        let message = if !scalar.valid_range(data_layout).contains(0) {
            "must be non-null"
        } else if init == Init::Uninit && !scalar.is_always_valid(data_layout) {
            "must be initialized inside its custom valid range"
        } else {
            return field_err;
        };
        if let Some(field_err) = &mut field_err
            && field_err.0[0].0.contains(message)
        {
            // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
            field_err.0[0].0 = format!("because {}", field_err.0[0].0);
        }
        // alloc: only for a `mem::zeroed` or `mem::uninitialized` call found, to explain it
        Some(InitError::new(format!("`{printed}` {message}")).nested(field_err))
    }
}
