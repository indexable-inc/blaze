//! rustc's late lint passes of null, dangling and cast pointers and of references in bodies.
//!
//! - `deref_null`: `DerefNullPtr` (`deref_nullptr`).
//! - `reference_casting`: `InvalidReferenceCasting` (`invalid_reference_casting`).
//! - `autorefs`: `ImplicitAutorefs` (`dangerous_implicit_autorefs`).
//! - `ptr_nulls`: `PtrNullChecks` (`useless_ptr_null_checks`, `invalid_null_arguments`).
//! - `dangling`: `DanglingPointers` (`dangling_pointers_from_temporaries`,
//!   `dangling_pointers_from_locals`).
//!
//! What they share follows rustc's `compiler/rustc_lint/src/utils.rs` (`peel_casts`),
//! `compiler/rustc_lint/src/context.rs` (`LateContext::expr_or_init`) (MIT or Apache-2.0).

pub(crate) mod autorefs;
pub(crate) mod dangling;
pub(crate) mod deref_null;
pub(crate) mod ptr_nulls;
pub(crate) mod reference_casting;

use std::ops::ControlFlow;

use base_db::Db;
use blaze_hash::HashMap;
use hir::{InFile, db::HirDatabase};
use hir_def::{
    DefWithBodyId, FunctionId, HasModule, Lookup,
    hir::{BindingAnnotation, BindingId, Expr, ExprId, Literal, Pat, PatId, Statement},
    resolver::ValueNs,
    src::HasSource,
};
use hir_expand::attrs::{AstPathExt as _, expand_cfg_attr_view};
use hir_ty::{ParamEnvAndCrate, next_solver::Ty};
use intern::Symbol;
use syntax::view::{self as ast, AstNode, HasAttrs};

use super::util;
use crate::{lints::bodies::BodyCx, model::Span};

pub(super) use util::span;

/// rustc's `Span::source_callsite`: the call of the outermost macro `span` came from, or
/// `span`.
pub(super) fn source_callsite(span: Span) -> Span {
    let mut at = span;
    while let Some(expansion) = at.expansion.take() {
        at = expansion.call_site;
    }
    at
}

/// The type `expr` was inferred as, before and after its adjustments, as `hir` gives it to be
/// printed.
pub(super) fn type_info<'db>(b: &BodyCx<'_, '_, 'db>, expr: ExprId) -> Option<hir::TypeInfo<'db>> {
    let node = util::syntax(b, expr)?;
    let expr = ast::Expr::cast(node.value)?;
    b.cx.vsema.type_of_expr(expr)
}

/// rustc's printing of the type `expr` was inferred as (`expr_ty`).
pub(super) fn ty_text(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<String> {
    Some(b.cx.ty(&type_info(b, expr)?.original))
}

/// Whether the function has the attribute `#[name]` (`rustc_as_ptr`,
/// `rustc_no_implicit_autorefs`, `rustc_never_returns_null_ptr`), its `cfg_attr`s expanded:
/// rustc's `find_attr!` of it.
pub(super) fn has_attr(db: &Db, def: FunctionId, name: &str) -> bool {
    let krate = def.krate(db);
    let attrs = def.lookup(db).source_view(db).value.attrs();
    let cfg = krate.cfg_options(db);
    expand_cfg_attr_view(attrs, || cfg, |meta, _| match meta {
        ast::Meta::PathMeta(attr) if attr.path().is1(name) => ControlFlow::Break(()),
        _ => ControlFlow::Continue(()),
    })
    .is_some()
}

/// The diagnostic item of the method `expr` calls, if it is a method call of one.
pub(super) fn method_item(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<Symbol> {
    util::fn_item(b.db(), util::method_fn(b, expr)?)
}

/// What rustc's lints read of a type's layout, in the typing environment of the body's item.
#[derive(Clone, Copy)]
pub(super) struct LayoutFacts {
    pub(super) size: u64,
    pub(super) is_zst: bool,
    pub(super) is_1zst: bool,
    pub(super) is_unsized: bool,
}

/// rustc's `layout_of` of `ty`, when it can be computed (not of a type parameter's size).
pub(super) fn layout<'db>(b: &BodyCx<'_, '_, 'db>, ty: Ty<'db>) -> Option<LayoutFacts> {
    let db = b.db();
    let env = ParamEnvAndCrate { param_env: db.trait_environment(b.owner.generic_def(db)), krate: b.owner.krate(db) };
    let layout = db.layout_of_ty(ty.store(), env.store()).ok()?;
    Some(LayoutFacts {
        size: layout.size.bytes(),
        is_zst: layout.is_zst(),
        is_1zst: layout.is_1zst(),
        is_unsized: layout.is_unsized(),
    })
}

/// Whether `expr` is the integer literal `0` (rustc's `LitKind::Int(0, _)`).
pub(super) fn is_zero(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> bool {
    matches!(util::literal(b, expr), Some(Literal::Int(0, _) | Literal::Uint(0, _)))
}

/// The initializers of the body's bindings that rustc's `expr_or_init` follows: of each binding
/// that is the whole pattern of a `let` and is neither `mut` nor `ref` (its value is the
/// initializer's), and of one that is the whole pattern of a `let` expression (which rustc
/// takes for the expression itself). Built once per body, when a pass first asks.
#[derive(Default)]
pub(super) struct Inits {
    owner: Option<DefWithBodyId>,
    map: HashMap<BindingId, ExprId>,
}

impl Inits {
    pub(super) fn of(&mut self, b: &BodyCx<'_, '_, '_>) -> &HashMap<BindingId, ExprId> {
        if self.owner != Some(b.owner) {
            self.owner = Some(b.owner);
            self.map.clear();
            let store = b.store();
            let binding = |pat: PatId| match store.pat(pat) {
                Pat::Bind { id, .. } if store[id].mode == BindingAnnotation::Unannotated => Some(id),
                _ => None,
            };
            for &expr in b.exprs() {
                match store.expr(expr) {
                    Expr::Block { statements, .. } => {
                        for statement in statements {
                            if let Statement::Let { pat, initializer: Some(init), .. } = statement
                                && let Some(id) = binding(pat)
                            {
                                self.map.insert(id, init);
                            }
                        }
                    }
                    Expr::Let { pat, .. } => {
                        if let Some(id) = binding(pat) {
                            self.map.insert(id, expr);
                        }
                    }
                    _ => {}
                }
            }
        }
        &self.map
    }
}

/// rustc's `LateContext::expr_or_init`: through blocks, and from a path to a local binding to
/// what initialized it, while there is one.
pub(super) fn expr_or_init(b: &BodyCx<'_, '_, '_>, inits: &HashMap<BindingId, ExprId>, expr: ExprId) -> ExprId {
    let mut expr = util::peel_blocks(b, expr);
    while let Expr::Path(_) = b.store().expr(expr)
        && let Some(ValueNs::LocalBinding(id)) = util::path_value(b, expr)
        && let Some(&init) = inits.get(&id)
    {
        expr = util::peel_blocks(b, init);
    }
    expr
}

/// rustc's `peel_casts`: through blocks, `as` casts, `.cast()`, `.cast_mut()`, `.cast_const()`,
/// `ptr::from_ref`, `UnsafeCell::raw_get`, `mem::transmute` and the initializers of local
/// bindings, to the expression the pointer comes from.
pub(super) fn peel_casts(b: &BodyCx<'_, '_, '_>, inits: &HashMap<BindingId, ExprId>, expr: ExprId) -> ExprId {
    let mut e = expr;
    loop {
        e = util::peel_blocks(b, e);
        e = match b.store().expr(e) {
            Expr::Cast { expr, .. } => expr,
            Expr::MethodCall { receiver, args, .. }
                if args.is_empty()
                    && method_item(b, e).is_some_and(|it| {
                        let items = util::items();
                        [items.ptr_cast, items.const_ptr_cast, items.ptr_cast_mut, items.ptr_cast_const].contains(&it)
                    }) =>
            {
                receiver
            }
            Expr::Call { callee, args }
                if args.len() == 1
                    && callee_item(b, callee).is_some_and(|it| {
                        let items = util::items();
                        [items.ptr_from_ref, items.unsafe_cell_raw_get, items.transmute].contains(&it)
                    }) =>
            {
                args[0]
            }
            _ => {
                let init = expr_or_init(b, inits, e);
                if init == e {
                    break;
                }
                init
            }
        };
    }
    e
}

/// The diagnostic item the callee `callee` names when it is a path (rustc's `qpath_res` of
/// it, `get_diagnostic_name`).
pub(super) fn callee_item(b: &BodyCx<'_, '_, '_>, callee: ExprId) -> Option<Symbol> {
    match b.store().expr(callee) {
        Expr::Path(_) => util::path_item(b, callee),
        _ => None,
    }
}

/// The function the callee `callee` names when it is a path.
pub(super) fn callee_fn(b: &BodyCx<'_, '_, '_>, callee: ExprId) -> Option<FunctionId> {
    match b.store().expr(callee) {
        Expr::Path(_) => match util::path_value(b, callee)? {
            ValueNs::FunctionId(it) => Some(it),
            _ => None,
        },
        _ => None,
    }
}

/// rustc's `def_span` of a function: its signature, as rustc shows it.
pub(super) fn fn_def_span(b: &BodyCx<'_, '_, '_>, def: FunctionId) -> Span {
    let source = def.lookup(b.db()).source_view(b.db());
    let range = crate::cx::signature_range_view(source.value);
    b.cx.span(InFile::new(source.file_id, range))
}
