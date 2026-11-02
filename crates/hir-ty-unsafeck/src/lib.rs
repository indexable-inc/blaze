//! Unsafety checking: rustc's `check_unsafety` (`compiler/rustc_mir_build/src/check_unsafety.rs`)
//! over a body's expression store and its inference rather than THIR.
//!
//! A body is walked once, in THIR's order, with rustc's safety context: safe, the body of an
//! `unsafe fn`, or an `unsafe` block. Closures, coroutines and inline consts are walked with
//! their body, in its context; a repeat count, an anonymous const of its own in rustc, is walked
//! as one, safe. Each operation that needs `unsafe` where it is not is one [`UnsafeOp`]: what it
//! is, and whether it is an error (E0133) or one of the lints `unsafe_op_in_unsafe_fn` and
//! `deprecated_safe_2024`; the lint levels are the caller's to apply. [`unsafety_check`] is a
//! query of the body, so an edit that leaves a body's operations as they were stops there.

#![forbid(unsafe_code)]

extern crate rustc_type_ir;

#[cfg(test)]
mod tests;

use std::mem;

use base_db::{Crate, Db};
use either::Either;
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, ExpressionStoreOwnerId, FieldId, FunctionId,
    GenericDefId, HasModule, ItemContainerId, Lookup, TraitId, VariantId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore, ExpressionStoreSourceMap, path::Path},
    hir::{
        Array, AsmOperand, BinaryOp, BindingId, Expr, ExprId, ExprOrPatId, ExprOrPatIdPacked,
        InlineAsmKind, Pat, PatId, RecordSpread, Statement, UnaryOp, Unsafe,
    },
    lang_item::lang_items,
    resolver::ValueNs,
    signatures::{FunctionSignature, StaticFlags, StaticSignature, VariantFields},
    type_ref::Rawness,
};
use hir_ty::{
    Adjust, AutoBorrow, BindingMode, ByRef, ExplicitDropMethodUseKind, InferenceDiagnostic,
    InferenceResult, OverloadedDeref, TargetFeatureIsSafeInTarget, TargetFeatures, Unsafety,
    db::HirDatabase,
    drop::is_freeze,
    infer::AutoBorrowMutability,
    is_fn_unsafe_to_call,
    next_solver::{
        CallableIdWrapper, DbInterner, Mutability, ParamEnv, Ty, TyKind, TypingMode,
        abi::Safety,
        infer::{DbInternerInferExt, InferCtxt},
    },
    target_feature_is_safe_in_target,
};
use hir_lint_levels::{Level, LintId};
use rustc_type_ir::inherent::IntoKind;
use span::Edition;

/// What an operation that needs `unsafe` does: rustc's `UnsafeOpKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UnsafetyReason {
    /// Reading a union field, or binding in a pattern of a union.
    UnionField,
    /// Calling an `unsafe fn`, an `unsafe` function pointer, a function of an `extern` block not
    /// declared `safe`, or a `#[target_feature]` function the caller does not have the features
    /// of.
    UnsafeFnCall,
    InlineAsm,
    RawPtrDeref,
    MutableStatic,
    ExternStatic,
    /// Reading or matching an `unsafe` field (feature `unsafe_fields`).
    UseOfUnsafeField,
    /// A struct expression of a variant with an `unsafe` field.
    InitializingTypeWithUnsafeField,
    /// Calling `Drop::drop` or `Drop::pin_drop` by path where type checking did not refuse it.
    CallDropExplicitly,
    /// A struct expression of a type with `#[rustc_layout_scalar_valid_range_*]`: rustc's
    /// `InitializingTypeWith`.
    InitializingTypeWith,
    /// Assigning to, or borrowing mutably, a place through a field of a type with
    /// `#[rustc_layout_scalar_valid_range_*]`, or a `ref mut` binding inside its pattern.
    MutationOfLayoutConstrainedField,
    /// Borrowing such a place, or binding it `ref`, where its type is not `Freeze`.
    BorrowOfLayoutConstrainedField,
    /// `wrap_binder!` or `unwrap_binder!`: a cast to or from an `unsafe<..>` binder type.
    UnsafeBinderCast,
}

/// How rustc reports an [`UnsafeOp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum UnsafeLint {
    /// E0133.
    HardError,
    /// The lint `unsafe_op_in_unsafe_fn`: in the body of an `unsafe fn`, outside a block.
    UnsafeOpInUnsafeFn,
    /// The lint `deprecated_safe_2024`: a call of a `#[rustc_deprecated_safe_2024]` function
    /// written before 2024. In the body of an `unsafe fn` ([`UnsafeOp::in_unsafe_fn`]) rustc
    /// gives it only where `unsafe_op_in_unsafe_fn` is not allowed.
    DeprecatedSafe2024,
}

/// An operation that needs `unsafe` where it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UnsafeOp {
    node: ExprOrPatIdPacked,
    pub reason: UnsafetyReason,
    pub lint: UnsafeLint,
    /// rustc's `suggest_unsafe_block`: the first [`UnsafeLint::UnsafeOpInUnsafeFn`] of its
    /// function, or of a closure or inline const where the function had none before it, that
    /// `unsafe_op_in_unsafe_fn` is not allowed at. One where it is allowed is not linted and
    /// leaves the next one first. rustc notes there that an unsafe function restricts its
    /// caller.
    pub first: bool,
    /// In the body of an `unsafe fn`, outside a block.
    pub in_unsafe_fn: bool,
}

const _: () = assert!(size_of::<UnsafeOp>() == 8);

impl UnsafeOp {
    /// The expression or pattern rustc points at.
    #[inline]
    pub fn node(&self) -> ExprOrPatId {
        self.node.unpack()
    }
}

/// The operations of a body that need `unsafe` where they are not, in rustc's order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct UnsafetyCheck {
    ops: Box<[UnsafeOp]>,
}

impl UnsafetyCheck {
    #[inline]
    pub fn ops(&self) -> &[UnsafeOp] {
        &self.ops
    }
}

/// rustc's `check_unsafety` of `def`, with the closures, inline consts and repeat counts in it.
#[blaze_query::tracked(returns(ref))]
pub fn unsafety_check(db: &Db, def: DefWithBodyId) -> UnsafetyCheck {
    let body = Body::of(db, def);
    let infer = InferenceResult::of(db, def);
    let owner = ExpressionStoreOwnerId::Body(def);
    let (context, target_features) = match def {
        DefWithBodyId::FunctionId(func) => {
            let context = match FunctionSignature::of(db, func).is_unsafe() {
                true => Context::UnsafeFn,
                false => Context::Safe,
            };
            (context, TargetFeatures::from_fn(db, func))
        }
        DefWithBodyId::StaticId(_) | DefWithBodyId::ConstId(_) | DefWithBodyId::VariantId(_) => {
            (Context::Safe, TargetFeatures::default())
        }
    };
    // alloc: only at the first operation, so none for a body without one.
    let mut ops = Vec::new();
    let mut walker = Walker::new(db, infer, body, owner, context, target_features, &mut ops);
    for param in &body.params {
        walker.walk_pat(param.formal);
    }
    walker.walk_expr(body.root_expr());
    for &konst in infer.defined_anon_consts() {
        let loc = konst.loc(db);
        if loc.owner != owner || konst.is_inline_const(db) {
            continue;
        }
        let infer = InferenceResult::of(db, konst);
        let features = TargetFeatures::default();
        Walker::new(db, infer, body, owner, Context::Safe, features, &mut ops).walk_expr(loc.expr);
    }
    UnsafetyCheck { ops: ops.into_boxed_slice() }
}

/// Whether an operation [`unsafe_operations`] gives is inside an `unsafe` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsideUnsafeBlock {
    No,
    Yes,
}

/// Every operation that needs `unsafe` in `current` and what it holds, inside a block or not,
/// starting outside one: what an editor highlights. A repeat count's are not among them.
pub fn unsafe_operations<'db>(
    db: &'db Db,
    infer: &InferenceResult<'_>,
    def: ExpressionStoreOwnerId,
    body: &ExpressionStore,
    current: ExprId,
    callback: &mut dyn FnMut(ExprOrPatId, InsideUnsafeBlock), // dyn: an editor's, at the edge.
) {
    let target_features = match def {
        ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(func))
        | ExpressionStoreOwnerId::Signature(GenericDefId::FunctionId(func)) => {
            TargetFeatures::from_fn(db, func)
        }
        _ => TargetFeatures::default(),
    };
    let mut sink = Callback(callback);
    Walker::new(db, infer, body, def, Context::Safe, target_features, &mut sink).walk_expr(current);
}

/// rustc's `SafetyContext`, less the unused-`unsafe` bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Context {
    Safe,
    UnsafeFn,
    UnsafeBlock,
}

/// An operation that needs `unsafe`, with the context it is in.
struct Event {
    node: ExprOrPatIdPacked,
    reason: UnsafetyReason,
    /// A call that `deprecated_safe_2024` lints rather than an error.
    deprecated: bool,
    context: Context,
    first: bool,
}

trait Sink {
    fn push(&mut self, event: Event);
}

impl Sink for Vec<UnsafeOp> {
    fn push(&mut self, event: Event) {
        let in_unsafe_fn = match event.context {
            Context::UnsafeBlock => return,
            Context::Safe => false,
            Context::UnsafeFn => true,
        };
        let lint = match (event.deprecated, in_unsafe_fn) {
            (true, _) => UnsafeLint::DeprecatedSafe2024,
            (false, false) => UnsafeLint::HardError,
            (false, true) => UnsafeLint::UnsafeOpInUnsafeFn,
        };
        let op = UnsafeOp {
            node: event.node,
            reason: event.reason,
            lint,
            first: event.first,
            in_unsafe_fn,
        };
        Vec::push(self, op);
    }
}

// dyn: the editor's callback of `unsafe_operations`.
struct Callback<'c>(&'c mut dyn FnMut(ExprOrPatId, InsideUnsafeBlock));

impl Sink for Callback<'_> {
    fn push(&mut self, event: Event) {
        if event.deprecated {
            return;
        }
        let inside = match event.context {
            Context::UnsafeBlock => InsideUnsafeBlock::Yes,
            Context::Safe | Context::UnsafeFn => InsideUnsafeBlock::No,
        };
        (self.0)(event.node.unpack(), inside);
    }
}

/// rustc's `UnsafetyVisitor`.
struct Walker<'a, 'db, 'i, S> {
    db: &'db Db,
    infer: &'a InferenceResult<'i>,
    store: &'a ExpressionStore,
    owner: ExpressionStoreOwnerId,
    krate: Crate,
    target_features: TargetFeatures<'db>,
    target_feature_is_safe: TargetFeatureIsSafeInTarget,
    /// Of the innermost block a macro wrote, as rustc takes it from a call's span.
    edition: Edition,
    context: Context,
    suggest_unsafe_block: bool,
    /// rustc's `hir_context`, where `unsafe_op_in_unsafe_fn`'s level is taken: the innermost
    /// expression being walked that has syntax, `None` for the item. Kept only while
    /// [`Walker::requires_unsafe`] can read it, in an `unsafe fn` with `suggest_unsafe_block`
    /// set: inside an expression entered without both, neither comes back.
    hir_context: Option<ExprId>,
    /// The store's, once `hir_context` is first kept.
    source_map: Option<&'db ExpressionStoreSourceMap>,
    /// rustc's `assignment_info`: the place being walked is the left side of a `=`.
    assigning: bool,
    in_union_destructure: bool,
    /// rustc's `inside_adt`: in a pattern of a type with `#[rustc_layout_scalar_valid_range_*]`,
    /// not behind a deref.
    inside_adt: bool,
    /// The fields of the variant last asked about: field accesses run on one struct.
    fields: Option<(VariantId, &'db VariantFields)>,
    /// The ADT last asked about and whether it has `#[rustc_layout_scalar_valid_range_*]`.
    layout_constrained: Option<(AdtId, bool)>,
    /// Made at the first shared borrow of a layout constrained place, to ask `Freeze`.
    freeze: Option<(InferCtxt<'db>, ParamEnv<'db>)>,
    drop_trait: Option<Option<TraitId>>,
    sink: &'a mut S,
}

impl<'a, 'db, 'i, S: Sink> Walker<'a, 'db, 'i, S> {
    fn new(
        db: &'db Db,
        infer: &'a InferenceResult<'i>,
        store: &'a ExpressionStore,
        owner: ExpressionStoreOwnerId,
        context: Context,
        target_features: TargetFeatures<'db>,
        sink: &'a mut S,
    ) -> Self {
        let krate = owner.krate(db);
        let target_feature_is_safe = match &krate.workspace_data(db).target {
            Ok(target) => target_feature_is_safe_in_target(target),
            Err(_) => TargetFeatureIsSafeInTarget::No,
        };
        Self {
            db,
            infer,
            store,
            owner,
            krate,
            target_features,
            target_feature_is_safe,
            edition: krate.data(db).edition,
            context,
            suggest_unsafe_block: true,
            hir_context: None,
            source_map: None,
            assigning: false,
            in_union_destructure: false,
            inside_adt: false,
            fields: None,
            layout_constrained: None,
            freeze: None,
            drop_trait: None,
            sink,
        }
    }

    /// rustc's `requires_unsafe` of an operation that is not a deprecated-safe call.
    /// Where `unsafe_op_in_unsafe_fn` is allowed, rustc emits nothing and leaves the flag set.
    fn requires_unsafe(&mut self, node: impl Into<ExprOrPatIdPacked>, reason: UnsafetyReason) {
        let first = self.context == Context::UnsafeFn
            && self.suggest_unsafe_block
            && !self.unsafe_op_in_unsafe_fn_allowed();
        if first {
            self.suggest_unsafe_block = false;
        }
        let event =
            Event { node: node.into(), reason, deprecated: false, context: self.context, first };
        self.sink.push(event);
    }

    fn deprecated_safe_call(&mut self, node: ExprId) {
        let event = Event {
            node: node.into(),
            reason: UnsafetyReason::UnsafeFnCall,
            deprecated: true,
            context: self.context,
            first: false,
        };
        self.sink.push(event);
    }

    /// rustc's `unsafe_op_in_unsafe_fn_allowed`: the lint's level at `hir_context` is `allow`
    /// (`expect` is not).
    fn unsafe_op_in_unsafe_fn_allowed(&self) -> bool {
        const UNSAFE_OP_IN_UNSAFE_FN: LintId = LintId::named("unsafe_op_in_unsafe_fn");
        let level =
            hir_lint_levels::lint_level_at(self.db, self.owner, self.hir_context, UNSAFE_OP_IN_UNSAFE_FN)
                .expect("`hir_context` is only ever an expression with syntax");
        level == Level::Allow
    }

    fn in_context(&mut self, context: Context, f: impl FnOnce(&mut Self)) {
        let outer = mem::replace(&mut self.context, context);
        f(self);
        self.context = outer;
    }

    /// rustc's `visit_inner_body`: a closure, coroutine or inline const, in the context it is
    /// in, with its own `suggest_unsafe_block` from where the outer body's stands.
    fn walk_inner_body(&mut self, params: &[PatId], body: ExprId) {
        let suggest = self.suggest_unsafe_block;
        let in_union_destructure = mem::replace(&mut self.in_union_destructure, false);
        let inside_adt = mem::replace(&mut self.inside_adt, false);
        for &param in params {
            self.walk_pat(param);
        }
        self.walk_expr(body);
        self.suggest_unsafe_block = suggest;
        self.in_union_destructure = in_union_destructure;
        self.inside_adt = inside_adt;
    }

    fn walk_exprs(&mut self, exprs: &[ExprId]) {
        for &expr in exprs {
            self.walk_expr(expr);
        }
    }

    /// rustc's `ExprKind::Scope`: an expression with syntax is the `hir_context` of what is in
    /// it. A desugared one has none, and leaves the context of what it is in.
    fn walk_expr(&mut self, current: ExprId) {
        if self.context != Context::UnsafeFn || !self.suggest_unsafe_block {
            return self.walk_expr_in(current);
        }
        let (db, owner) = (self.db, self.owner);
        let source_map = *self
            .source_map
            .get_or_insert_with(|| ExpressionStore::with_source_map(db, owner).1);
        let outer = self.hir_context;
        if source_map.expr_syntax(current).is_ok() {
            self.hir_context = Some(current);
        }
        self.walk_expr_in(current);
        self.hir_context = outer;
    }

    fn walk_expr_in(&mut self, current: ExprId) {
        // THIR wraps an adjusted expression in its adjustments, each of which is not a place
        // the assignment passes through.
        let assigning = mem::replace(&mut self.assigning, false)
            && self.infer.expr_adjustment(current).is_none_or(|it| it.is_empty());
        let store = self.store;
        let expr = store.expr(current);
        if matches!(expr, Expr::Field { .. } | Expr::Index { .. }) {
            self.check_adjustment_borrow(current);
        }
        match expr {
            Expr::Path(path) => self.check_static(current, path),
            Expr::Field { expr, .. } => {
                if let Some(Either::Left(field)) = self.infer.field_resolution(current) {
                    if self.is_unsafe_field(field) {
                        self.requires_unsafe(current, UnsafetyReason::UseOfUnsafeField);
                    } else if matches!(field.parent, VariantId::UnionId(_)) && !assigning {
                        self.requires_unsafe(current, UnsafetyReason::UnionField);
                    }
                }
                self.assigning = assigning;
                self.walk_expr(expr);
            }
            Expr::Cast { expr, .. } => {
                self.assigning = assigning;
                self.walk_expr(expr);
            }
            Expr::Call { callee, args } => {
                let is_path = matches!(store.expr(callee), Expr::Path(_));
                match self.adjusted_ty(callee).kind() {
                    TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(func)), _) => {
                        let drop_use = ExplicitDropMethodUseKind::Path(callee.into());
                        self.check_call(current, func, drop_use);
                    }
                    TyKind::FnDef(CallableIdWrapper(CallableDefId::StructId(it)), _) if is_path => {
                        self.check_adt(current, VariantId::StructId(it));
                        self.walk_exprs(args);
                        return;
                    }
                    TyKind::FnDef(CallableIdWrapper(CallableDefId::EnumVariantId(it)), _)
                        if is_path =>
                    {
                        self.check_adt(current, VariantId::EnumVariantId(it));
                        self.walk_exprs(args);
                        return;
                    }
                    TyKind::FnPtr(_, hdr) if hdr.safety() == Safety::Unsafe => {
                        self.requires_unsafe(current, UnsafetyReason::UnsafeFnCall);
                    }
                    _ => {}
                }
                self.walk_expr(callee);
                self.walk_exprs(args);
            }
            Expr::MethodCall { receiver, args, .. } => {
                if let Some((func, _)) = self.infer.method_resolution(current) {
                    self.check_call(current, func, ExplicitDropMethodUseKind::MethodCall(current));
                }
                self.walk_expr(receiver);
                self.walk_exprs(args);
            }
            Expr::RecordLit { fields, spread, .. } => {
                if let Some(variant) = self.infer.variant_resolution_for_expr(current) {
                    self.check_adt(current, variant);
                }
                for field in fields {
                    self.walk_expr(field.expr);
                }
                if let RecordSpread::Expr(base) = spread {
                    self.walk_expr(base);
                }
            }
            Expr::Ref { expr, rawness: Rawness::RawPtr, .. } => match store.expr(expr) {
                // `&raw const STATIC`: THIR's `Deref` of the static, which a raw borrow of a
                // deref does not walk.
                Expr::Path(_) => {}
                Expr::UnaryOp { expr: pointer, op: UnaryOp::Deref } => self.walk_expr(pointer),
                _ => {
                    let mut peeled = expr;
                    while let Expr::Field { expr: base, .. } = store.expr(peeled)
                        && let Some(Either::Left(FieldId { parent: VariantId::UnionId(_), .. })) =
                            self.infer.field_resolution(peeled)
                    {
                        peeled = base;
                    }
                    self.walk_expr(peeled);
                }
            },
            Expr::Ref { expr, rawness: Rawness::Ref, mutability } => {
                if self.is_layout_constrained_operand(expr) {
                    let mutability = hir_ty::lower_mutability(mutability);
                    self.borrow_of_layout_constrained(current, mutability, self.adjusted_ty(expr));
                }
                self.walk_expr(expr);
            }
            Expr::UnaryOp { expr, op: UnaryOp::Deref } => {
                if let TyKind::RawPtr(..) = self.adjusted_ty(expr).kind() {
                    self.requires_unsafe(current, UnsafetyReason::RawPtrDeref);
                }
                self.walk_expr(expr);
            }
            // An overloaded one borrows its left side, which its adjustment says.
            Expr::BinaryOp { lhs, rhs, op: Some(BinaryOp::Assignment { op: Some(_) }) } => {
                if self.infer.method_resolution(current).is_none()
                    && self.is_layout_constrained_operand(lhs)
                {
                    self.requires_unsafe(current, UnsafetyReason::MutationOfLayoutConstrainedField);
                }
                self.walk_expr(lhs);
                self.walk_expr(rhs);
            }
            Expr::Assignment { target, value } => match store.pat(target) {
                Pat::Expr(place) => {
                    if self.is_layout_constrained_operand(place) {
                        let reason = UnsafetyReason::MutationOfLayoutConstrainedField;
                        self.requires_unsafe(current, reason);
                    }
                    self.assigning = true;
                    self.walk_expr(place);
                    self.assigning = false;
                    self.walk_expr(value);
                }
                // A destructuring assignment is `{ let <pattern> = value; place = binding; .. }`.
                _ => {
                    self.walk_expr(value);
                    self.walk_pat(target);
                    self.walk_places(target);
                }
            },
            Expr::Block { statements, tail, unsafe_, .. } => {
                let edition =
                    store.block_edition(current).map(|it| mem::replace(&mut self.edition, it));
                let context = match unsafe_ {
                    Unsafe::Yes => mem::replace(&mut self.context, Context::UnsafeBlock),
                    Unsafe::No => self.context,
                };
                for statement in statements {
                    match statement {
                        Statement::Let { pat, initializer, else_branch, .. } => {
                            if let Some(initializer) = initializer {
                                self.walk_expr(initializer);
                            }
                            self.walk_pat(pat);
                            if let Some(else_branch) = else_branch {
                                self.walk_expr(else_branch);
                            }
                        }
                        Statement::Expr { expr, .. } => self.walk_expr(expr),
                        Statement::Item(_) => {}
                    }
                }
                if let Some(tail) = tail {
                    self.walk_expr(tail);
                }
                self.context = context;
                if let Some(edition) = edition {
                    self.edition = edition;
                }
            }
            Expr::Let { pat, expr } => {
                self.walk_expr(expr);
                self.walk_pat(pat);
            }
            Expr::Match { expr, arms } => {
                self.walk_expr(expr);
                for arm in arms {
                    if let Some(guard) = arm.guard {
                        self.walk_expr(guard);
                    }
                    self.walk_pat(arm.pat);
                    self.walk_expr(arm.expr);
                }
            }
            Expr::Closure { args, body, .. } => self.walk_inner_body(args, body),
            Expr::Const(body) => self.walk_inner_body(&[], body),
            Expr::Array(Array::ElementList { elements }) => self.walk_exprs(elements),
            // The count is an anonymous const of its own, walked with its inference.
            Expr::Array(Array::Repeat { initializer, .. }) => self.walk_expr(initializer),
            Expr::InlineAsm(asm) => {
                // `naked_asm!` needs `unsafe` on its function's `#[unsafe(naked)]`;
                // `global_asm!` none.
                if asm.kind == InlineAsmKind::Asm {
                    self.requires_unsafe(current, UnsafetyReason::InlineAsm);
                }
                for (_, operand) in asm.operands.iter() {
                    match *operand {
                        AsmOperand::In { expr, .. }
                        | AsmOperand::Out { expr: Some(expr), .. }
                        | AsmOperand::InOut { expr, .. } => self.walk_expr(expr),
                        AsmOperand::SplitInOut { in_expr, out_expr, .. } => {
                            self.walk_expr(in_expr);
                            if let Some(out_expr) = out_expr {
                                self.walk_expr(out_expr);
                            }
                        }
                        AsmOperand::Out { expr: None, .. } | AsmOperand::Sym(_) => {}
                        // An anonymous const in rustc, checked as a body of its own.
                        AsmOperand::Const(expr) => {
                            let suggest = self.suggest_unsafe_block;
                            self.in_context(Context::Safe, |this| this.walk_expr(expr));
                            self.suggest_unsafe_block = suggest;
                        }
                        // Safe even inside `unsafe`: `asm!` always is, and the label would
                        // otherwise have no way out.
                        AsmOperand::Label(block) => {
                            self.in_context(Context::Safe, |this| this.walk_expr(block));
                        }
                    }
                }
            }
            Expr::UnsafeBinderCast { .. } => {
                self.requires_unsafe(current, UnsafetyReason::UnsafeBinderCast);
                store.walk_child_exprs_without_pats(current, |child| self.walk_expr(child));
            }
            Expr::Missing
            | Expr::If { .. }
            | Expr::Loop { .. }
            | Expr::Continue { .. }
            | Expr::Break { .. }
            | Expr::Return { .. }
            | Expr::Become { .. }
            | Expr::Yield { .. }
            | Expr::Yeet { .. }
            | Expr::Await { .. }
            | Expr::UnaryOp { .. }
            | Expr::BinaryOp { .. }
            | Expr::Index { .. }
            | Expr::Tuple { .. }
            | Expr::Literal(_)
            | Expr::Underscore
            | Expr::OffsetOf(_)
            | Expr::IncludeBytes => {
                store.walk_child_exprs_without_pats(current, |child| self.walk_expr(child));
            }
        }
    }

    fn walk_pat(&mut self, current: PatId) {
        let store = self.store;
        let pat = store.pat(current);
        // Match ergonomics' implicit derefs are deref patterns around this one.
        let derefs = self.infer.pat_adjustment(current).is_some_and(|it| !it.is_empty());
        if self.in_union_destructure {
            match pat {
                // `Missing`, `Rest` and `Wild` read nothing; `Or` holds patterns walked below.
                Pat::Missing | Pat::Rest | Pat::Wild | Pat::Or(_) if !derefs => {}
                _ => {
                    self.requires_unsafe(current, UnsafetyReason::UnionField);
                    return;
                }
            }
        }
        let inside_adt = self.inside_adt;
        self.inside_adt &= !derefs;
        match pat {
            Pat::Record { args, .. } => {
                let variant = self.infer.variant_resolution_for_pat(current);
                if let Some(variant) = variant {
                    let fields = self.variant_fields(variant);
                    if has_unsafe_field(fields) {
                        for arg in args {
                            let field = fields.field(&arg.name);
                            if field.is_some_and(|it| fields.fields()[it].is_unsafe) {
                                self.requires_unsafe(arg.pat, UnsafetyReason::UseOfUnsafeField);
                            }
                        }
                    }
                }
                let outer = self.in_union_destructure;
                match variant {
                    Some(VariantId::UnionId(_)) => self.in_union_destructure = true,
                    Some(VariantId::StructId(it)) => {
                        let constrained = self.is_layout_constrained(AdtId::StructId(it));
                        self.inside_adt |= constrained;
                    }
                    Some(VariantId::EnumVariantId(_)) | None => {}
                }
                for arg in args {
                    self.walk_pat(arg.pat);
                }
                self.in_union_destructure = outer;
            }
            Pat::TupleStruct { args, ellipsis, .. } => {
                let variant = self.infer.variant_resolution_for_pat(current);
                if let Some(VariantId::StructId(it)) = variant {
                    let constrained = self.is_layout_constrained(AdtId::StructId(it));
                    self.inside_adt |= constrained;
                }
                if let Some(variant) = variant {
                    let fields = self.variant_fields(variant);
                    if has_unsafe_field(fields) {
                        let count = fields.len();
                        for (at, &arg) in args.iter().enumerate() {
                            let index = match ellipsis {
                                Some(rest) if at >= rest as usize => {
                                    count.checked_sub(args.len() - at)
                                }
                                _ => Some(at),
                            };
                            let is_unsafe = index
                                .and_then(|it| fields.fields().iter().nth(it))
                                .is_some_and(|(_, field)| field.is_unsafe);
                            if is_unsafe {
                                self.requires_unsafe(arg, UnsafetyReason::UseOfUnsafeField);
                            }
                        }
                    }
                }
                for &arg in args {
                    self.walk_pat(arg);
                }
            }
            Pat::Bind { id, .. } => {
                if self.inside_adt {
                    self.check_ref_binding(current, id);
                }
                pat.walk_child_pats(|child| self.walk_pat(child));
            }
            // THIR's deref patterns: `box` is one too.
            Pat::Ref { .. } | Pat::Box { .. } | Pat::Deref { .. } => {
                self.inside_adt = false;
                pat.walk_child_pats(|child| self.walk_pat(child));
            }
            Pat::Missing
            | Pat::Rest
            | Pat::Wild
            | Pat::Tuple { .. }
            | Pat::Or(_)
            | Pat::Range { .. }
            | Pat::Slice { .. }
            | Pat::Path(_)
            | Pat::Lit(_)
            | Pat::NotNull
            | Pat::Expr(_) => pat.walk_child_pats(|child| self.walk_pat(child)),
        }
        self.inside_adt = inside_adt;
    }

    /// A `ref` or `ref mut` binding inside a pattern of a layout constrained type.
    fn check_ref_binding(&mut self, current: PatId, binding: BindingId) {
        let Some(BindingMode(ByRef::Yes(mutability), _)) = self.infer.binding_mode(current) else {
            return;
        };
        match mutability {
            Mutability::Mut => {
                self.requires_unsafe(current, UnsafetyReason::MutationOfLayoutConstrainedField);
            }
            Mutability::Not => {
                if let Some((pointee, _, _)) = self.infer.binding_ty(binding).as_reference()
                    && !self.is_freeze(pointee)
                {
                    self.requires_unsafe(current, UnsafetyReason::BorrowOfLayoutConstrainedField);
                }
            }
        }
    }

    /// rustc's `ExprKind::Borrow` that an adjustment makes of `current`, a place: an autoref, or
    /// the borrow an overloaded deref or index takes of its operand.
    fn check_adjustment_borrow(&mut self, current: ExprId) {
        let Some(first) = self.infer.expr_adjustment(current).and_then(|it| it.first()) else {
            return;
        };
        let mutability = match first.kind {
            Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Not))
            | Adjust::Deref(Some(OverloadedDeref(Mutability::Not))) => Mutability::Not,
            Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::Mut { .. }))
            | Adjust::Deref(Some(OverloadedDeref(Mutability::Mut))) => Mutability::Mut,
            Adjust::NeverToAny
            | Adjust::Deref(None)
            | Adjust::Borrow(AutoBorrow::RawPtr(_))
            | Adjust::Pointer(_) => return,
        };
        if self.is_layout_constrained_place(current) {
            let ty = self.infer.expr_ty(current);
            self.borrow_of_layout_constrained(current, mutability, ty);
        }
    }

    fn borrow_of_layout_constrained(&mut self, node: ExprId, mutability: Mutability, ty: Ty<'db>) {
        match mutability {
            Mutability::Mut => {
                self.requires_unsafe(node, UnsafetyReason::MutationOfLayoutConstrainedField);
            }
            Mutability::Not => {
                if !self.is_freeze(ty) {
                    self.requires_unsafe(node, UnsafetyReason::BorrowOfLayoutConstrainedField);
                }
            }
        }
    }

    /// rustc's `LayoutConstrainedPlaceVisitor` from an operand: THIR's adjustments around it are
    /// a deref or a value, where the place ends.
    fn is_layout_constrained_operand(&mut self, expr: ExprId) -> bool {
        self.infer.expr_adjustment(expr).is_none_or(|it| it.is_empty())
            && self.is_layout_constrained_place(expr)
    }

    /// Whether the place `expr`, its own adjustments aside, passes through a field of a type with
    /// `#[rustc_layout_scalar_valid_range_*]` before any deref. A builtin index walks its index
    /// too, as rustc's visitor does.
    fn is_layout_constrained_place(&mut self, expr: ExprId) -> bool {
        match self.store.expr(expr) {
            Expr::Field { expr: base, .. } => {
                let constrained = match self.adjusted_ty(base).kind() {
                    TyKind::Adt(def, _) => self.is_layout_constrained(def.def_id()),
                    _ => false,
                };
                constrained || self.is_layout_constrained_operand(base)
            }
            Expr::Index { base, index } if self.infer.method_resolution(expr).is_none() => {
                self.is_layout_constrained_operand(base)
                    || self.is_layout_constrained_operand(index)
            }
            _ => false,
        }
    }

    fn is_layout_constrained(&mut self, adt: AdtId) -> bool {
        match self.layout_constrained {
            Some((id, constrained)) if id == adt => constrained,
            _ => {
                let range = AttrFlags::rustc_layout_scalar_valid_range(self.db, adt);
                let constrained = range.start.is_some() || range.end.is_some();
                self.layout_constrained = Some((adt, constrained));
                constrained
            }
        }
    }

    fn is_freeze(&mut self, ty: Ty<'db>) -> bool {
        let (db, krate, owner) = (self.db, self.krate, self.owner);
        let (infcx, env) = self.freeze.get_or_insert_with(|| {
            let interner = DbInterner::new_with(db, krate);
            let env = db.trait_environment(owner.generic_def(db));
            (interner.infer_ctxt().build(TypingMode::non_body_analysis()), env)
        });
        is_freeze(infcx, ty, *env)
    }

    /// The places of a destructuring assignment, in order, each the left side of a `=`.
    fn walk_places(&mut self, current: PatId) {
        let store = self.store;
        match store.pat(current) {
            Pat::Expr(place) => {
                if self.is_layout_constrained_operand(place) {
                    self.requires_unsafe(place, UnsafetyReason::MutationOfLayoutConstrainedField);
                }
                self.assigning = true;
                self.walk_expr(place);
                self.assigning = false;
            }
            pat => pat.walk_child_pats(|child| self.walk_places(child)),
        }
    }

    fn adjusted_ty<'t>(&self, expr: ExprId) -> Ty<'t> {
        self.infer.type_of_expr_with_adjust(expr).unwrap_or_else(|| self.infer.expr_ty(expr))
    }

    fn check_call(&mut self, node: ExprId, func: FunctionId, drop_use: ExplicitDropMethodUseKind) {
        let unsafety = is_fn_unsafe_to_call(
            self.db,
            func,
            &self.target_features,
            self.edition,
            self.target_feature_is_safe,
        );
        match unsafety {
            Unsafety::Safe => {}
            Unsafety::Unsafe => self.requires_unsafe(node, UnsafetyReason::UnsafeFnCall),
            Unsafety::DeprecatedSafe2024 => self.deprecated_safe_call(node),
        }
        if self.is_drop_method(func) {
            let refused = self.infer.diagnostics().iter().any(|it| match it {
                InferenceDiagnostic::ExplicitDropMethodUse { kind } => *kind == drop_use,
                _ => false,
            });
            if !refused {
                self.requires_unsafe(node, UnsafetyReason::CallDropExplicitly);
            }
        }
    }

    fn is_drop_method(&mut self, func: FunctionId) -> bool {
        let ItemContainerId::TraitId(container) = func.lookup(self.db).container else {
            return false;
        };
        let (db, krate) = (self.db, self.krate);
        *self.drop_trait.get_or_insert_with(|| lang_items(db, krate).Drop) == Some(container)
    }

    fn check_adt(&mut self, node: ExprId, variant: VariantId) {
        if has_unsafe_field(self.variant_fields(variant)) {
            self.requires_unsafe(node, UnsafetyReason::InitializingTypeWithUnsafeField);
        }
        let adt = match variant {
            VariantId::StructId(it) => AdtId::StructId(it),
            VariantId::UnionId(it) => AdtId::UnionId(it),
            VariantId::EnumVariantId(it) => AdtId::EnumId(it.lookup(self.db).parent),
        };
        if self.is_layout_constrained(adt) {
            self.requires_unsafe(node, UnsafetyReason::InitializingTypeWith);
        }
    }

    fn is_unsafe_field(&mut self, field: FieldId) -> bool {
        self.variant_fields(field.parent).fields()[field.local_id].is_unsafe
    }

    fn variant_fields(&mut self, variant: VariantId) -> &'db VariantFields {
        match self.fields {
            Some((id, fields)) if id == variant => fields,
            _ => {
                let fields = VariantFields::of(self.db, variant);
                self.fields = Some((variant, fields));
                fields
            }
        }
    }

    /// A path to a `static mut`, or to a static of an `extern` block not declared `safe`.
    fn check_static(&mut self, current: ExprId, path: Path<'_>) {
        if matches!(path, Path::LangItem(..))
            || self.infer.variant_resolution_for_expr(current).is_some()
            || self.infer.assoc_resolutions_for_expr(current).is_some()
            || matches!(self.infer.expr_ty(current).kind(), TyKind::FnDef(..))
        {
            return;
        }
        let Some(ValueNs::StaticId(id)) = self.infer.value_resolution(current.into()) else {
            return;
        };
        let flags = StaticSignature::of(self.db, id).flags;
        if flags.contains(StaticFlags::MUTABLE) {
            self.requires_unsafe(current, UnsafetyReason::MutableStatic);
        } else if flags.contains(StaticFlags::EXTERN) && !flags.contains(StaticFlags::EXPLICIT_SAFE)
        {
            self.requires_unsafe(current, UnsafetyReason::ExternStatic);
        }
    }
}

fn has_unsafe_field(fields: &VariantFields) -> bool {
    fields.fields().iter().any(|(_, field)| field.is_unsafe)
}
