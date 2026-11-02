//! rustc's `InvalidFromUtf8`: calls of `std::str::from_utf8`, `str::from_utf8` and their
//! `_mut` and `_unchecked` forms with bytes known not to be UTF-8 (`invalid_from_utf8`,
//! `invalid_from_utf8_unchecked`): a byte string literal or an array of byte and integer
//! literals, borrowed or not, written in the call or read through `let`s that never change
//! and the crate's `const`s and `static`s.
//!
//! Follows rustc's `compiler/rustc_lint/src/invalid_from_utf8.rs`, `InvalidFromUtf8Diag` of
//! `compiler/rustc_lint/src/lints.rs` and `LateContext::expr_or_init_with_outside_body` of
//! `compiler/rustc_lint/src/context.rs` (MIT or Apache-2.0).

use blaze_items::{Fact, ItemId};
use hir_def::{
    DefWithBodyId, HasModule as _, ItemContainerId, Lookup as _, ModuleDefId,
    hir::{Array, BindingAnnotation, BindingId, Expr, ExprId, Literal, Pat, Statement},
    resolver::ValueNs,
};

use super::FnItems;
use crate::{
    lints::bodies::{BodyCx, ExprKinds, ExprKind, LatePass, LintSet, lint_set, types::util},
    lower::{Found, Phase},
    model::Diag,
};

const ITEMS: &[&str] = &[
    "str_from_utf8",
    "str_from_utf8_mut",
    "str_from_utf8_unchecked",
    "str_from_utf8_unchecked_mut",
    "str_inherent_from_utf8",
    "str_inherent_from_utf8_mut",
    "str_inherent_from_utf8_unchecked",
    "str_inherent_from_utf8_unchecked_mut",
];

pub(crate) struct InvalidFromUtf8 {
    items: FnItems,
}

impl Default for InvalidFromUtf8 {
    fn default() -> InvalidFromUtf8 {
        InvalidFromUtf8 { items: FnItems::new(ITEMS) }
    }
}

impl LatePass for InvalidFromUtf8 {
    const LINTS: LintSet = lint_set(["invalid_from_utf8", "invalid_from_utf8_unchecked"]);
    const EXPRS: ExprKinds = ExprKinds::of(&[ExprKind::Call]);

    fn check_expr(&mut self, b: &BodyCx<'_, '_, '_>, expr: ExprId, out: &mut Vec<Found>) {
        let Some([arg]) = util::call_args(b, expr) else { return };
        let Some((def, _)) = util::called_fn(b, expr) else { return };
        let Some(item) = self.items.of(b.db(), def) else { return };
        // alloc: empty, pushed to only for a const read by the argument of a `from_utf8` call
        let mut init = Init { linted: b, item: None, expr: *arg, read: Vec::new() };
        init.or_init();
        while let Expr::Ref { expr: inner, .. } = init.b().store().expr(init.expr) {
            init.expr = inner;
            init.or_init();
        }
        let Some(error) = invalid_bytes(init.b(), init.expr) else { return };
        let (Some(at), Some(call), Some(literal)) = (b.expr_node(expr), util::span(b, expr), util::span(init.b(), init.expr))
        else {
            return;
        };
        let method = item.strip_prefix("str_").unwrap_or(item);
        let method = match method.strip_prefix("inherent_") {
            // alloc: the lint's diagnostic
            Some(method) => format!("str::{method}"),
            // alloc: the lint's diagnostic
            None => format!("std::str::{method}"),
        };
        let mut diag = match item.contains("unchecked") {
            true => Diag::lint(
                "invalid_from_utf8_unchecked",
                // alloc: the lint's diagnostic
                format!("calls to `{method}` with an invalid literal are undefined behavior"),
                call,
            ),
            false => Diag::lint(
                "invalid_from_utf8",
                // alloc: the lint's diagnostic
                format!("calls to `{method}` with an invalid literal always return an error"),
                call,
            ),
        };
        // alloc: the lint's diagnostic
        diag.span_label(literal, format!("the literal was valid UTF-8 up to the {} bytes", error.valid_up_to()));
        for def in init.read.iter().rev() {
            let (item, def) = *def;
            let name = b.cx.def_path(def.into());
            // alloc: the lint's diagnostic
            diag.fact(Fact::Code(b.cx.key(item)), format!("the code of `{name}`"));
        }
        let called = b.cx.def_path(ModuleDefId::FunctionId(def).into());
        // alloc: the lint's diagnostic
        diag.fact(Fact::Signature(b.cx.key(ItemId::Function(def))), format!("the signature of `{called}`"));
        out.push(b.lint(diag, Phase::Late, at));
    }
}

/// The UTF-8 error of the bytes `expr` is as rustc reads them: a byte string literal, or an
/// array of byte and integer literals (each integer cut to its low byte).
fn invalid_bytes(b: &BodyCx<'_, '_, '_>, expr: ExprId) -> Option<std::str::Utf8Error> {
    let store = b.store();
    match store.expr(expr) {
        Expr::Literal(Literal::ByteString(bytes)) => std::str::from_utf8(bytes).err(),
        Expr::Array(Array::ElementList { elements }) => {
            let mut bytes = Vec::with_capacity(elements.len());
            for &element in elements {
                bytes.push(match store.expr(element) {
                    Expr::Literal(Literal::Uint(value, _)) => *value as u8,
                    Expr::Literal(Literal::Int(value, _)) => *value as u8,
                    _ => return None,
                });
            }
            std::str::from_utf8(&bytes).err()
        }
        _ => None,
    }
}

/// Where rustc's `expr_or_init_with_outside_body` has got to from an argument: an expression of
/// the body linted, or of the body of a `const` or `static` of the crate it read.
struct Init<'s, 'a, 'db> {
    linted: &'s BodyCx<'s, 'a, 'db>,
    item: Option<BodyCx<'s, 'a, 'db>>,
    expr: ExprId,
    /// The `const`s and `static`s read, in order.
    read: Vec<(ItemId, ModuleDefId)>,
}

/// One step of `expr_or_init_with_outside_body` from a path.
enum Step<'s, 'a, 'db> {
    /// To the initializer of the `let` that binds the local the path names.
    Local(ExprId),
    /// To the value of the `const` or `static` the path names.
    Item(BodyCx<'s, 'a, 'db>, ItemId, ModuleDefId),
}

impl<'s, 'a, 'db> Init<'s, 'a, 'db> {
    fn b(&self) -> &BodyCx<'s, 'a, 'db> {
        self.item.as_ref().unwrap_or(self.linted)
    }

    /// rustc's `expr_or_init_with_outside_body`: through blocks to their tails, from a local
    /// bound by a `let` without `mut` or `ref` to its initializer, and from a `const` or a
    /// `static` of the crate (not of an impl, a trait or an `extern` block) to its value, as
    /// long as that goes.
    fn or_init(&mut self) {
        loop {
            let step = {
                let b = self.b();
                let expr = util::peel_blocks(b, self.expr);
                self.expr = expr;
                self.step(expr)
            };
            match step {
                Some(Step::Local(init)) => self.expr = init,
                Some(Step::Item(body, item, def)) => {
                    self.expr = body.body.root_expr();
                    self.item = Some(body);
                    self.read.push((item, def));
                }
                None => return,
            }
        }
    }

    fn step(&self, expr: ExprId) -> Option<Step<'s, 'a, 'db>> {
        let b = self.b();
        let Expr::Path(_) = b.store().expr(expr) else { return None };
        let db = b.db();
        let local = |container: ItemContainerId, krate: base_db::Crate| {
            matches!(container, ItemContainerId::ModuleId(_)) && krate == b.cx.krate.base()
        };
        let (owner, item, def) = match util::path_value(b, expr)? {
            ValueNs::LocalBinding(binding) => return let_init(b, binding).map(Step::Local),
            ValueNs::ConstId(it) if local(it.lookup(db).container, it.krate(db)) => {
                (DefWithBodyId::ConstId(it), ItemId::Const(it), ModuleDefId::ConstId(it))
            }
            ValueNs::StaticId(it) if local(it.lookup(db).container, it.krate(db)) => {
                (DefWithBodyId::StaticId(it), ItemId::Static(it), ModuleDefId::StaticId(it))
            }
            _ => return None,
        };
        Some(Step::Item(BodyCx::new(self.linted.cx, owner)?, item, def))
    }
}

/// The initializer of the `let` statement whose pattern is `binding` alone, written without
/// `mut` or `ref` so that it is never assigned again (rustc's `LetStmt` parent of the
/// binding's pattern, `BindingMode::NONE`).
fn let_init(b: &BodyCx<'_, '_, '_>, binding: BindingId) -> Option<ExprId> {
    let store = b.store();
    if store[binding].mode != BindingAnnotation::Unannotated {
        return None;
    }
    for &expr in b.exprs() {
        let Expr::Block { statements, .. } = store.expr(expr) else { continue };
        for statement in statements {
            if let Statement::Let { pat, initializer, .. } = statement
                && let Pat::Bind { id, .. } = store.pat(pat)
                && id == binding
            {
                return initializer;
            }
        }
    }
    None
}
