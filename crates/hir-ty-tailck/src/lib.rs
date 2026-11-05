//! Explicit tail calls: rustc's `check_tail_calls` (`compiler/rustc_mir_build/src/
//! check_tail_calls.rs`, MIT or Apache-2.0) over a body's expression store and its inference
//! rather than THIR, and the callees the `tail_call_track_caller` lint is about.
//!
//! A body is walked once, each `become` checked where it is, in rustc's order of checks and
//! with its early returns. What rustc's THIR makes of the expression after `become` is read
//! off the store and the inference: a builtin operator (an operator writeback leaves no
//! method on, rustc's `fix_scalar_builtin_expr` and `fix_index_builtin_expr`), an overloaded
//! one (a call the source does not write), a call, or something else. A closure, coroutine or
//! `async` body is checked with the item's (blaze infers them together), as rustc's
//! `is_closure` body is. Each refusal is a [`TailCallError`]: rustc gives the body it is in its
//! error MIR, which [`TailCallCheck::fails_body`] says, so no later pass sees it.
//!
//! rustc gives `tail_call_track_caller` from codegen, of the instance a tail call makes. Here
//! it is of what the body's own environment resolves the callee to: a `#[track_caller]`
//! function, a trait's method whose declaration is one, or the impl's method the environment
//! names. A trait's method that only an instance resolves to a `#[track_caller]` impl method is
//! not found (rustc's `default-trait-method.rs`).

#![forbid(unsafe_code)]

use base_db::Db;
use either::Either;
use hir_def::{
    CallableDefId, DefWithBodyId, FunctionId, HasModule, ItemContainerId, Lookup, TraitId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore},
    hir::{BinaryOp, Expr, ExprId, ExprKind, UnaryOp},
    lang_item::{LangItems, lang_items},
    signatures::FunctionSignature,
};
use hir_ty::{
    InferenceResult, ParamEnvAndCrate,
    db::HirDatabase,
    next_solver::{
        Binder, DbInterner, FnSig, ParamEnv, Region, RegionExt, StoredTy, Ty, TyKind, TypingMode,
        infer::{DbInternerInferExt, InferCtxt, traits::ObligationCause},
    },
};
use rustc_abi::ExternAbi;
use rustc_type_ir::{
    RegionKind, TypeFlags, TypeFoldable, TypeFolder, TypeSuperFoldable, TypeVisitableExt,
    inherent::{IntoKind, Ty as _},
};

/// What rustc's `check_tail_calls` refuses of one `become`, and the nodes its message is at.
/// `N` is the node an expression is named by: an [`ExprId`] here, its syntax in `hir`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TailCallErrorKind<N = ExprId> {
    /// "`become` is not allowed in closures": in a closure, coroutine or `async` body.
    InClosure,
    /// "`become` does not support operators" of a builtin operator, `value` the operation.
    BuiltinOp { value: N },
    /// "`become` requires a function call": `value` is no call (a constructor, a block, a
    /// deref, an overloaded index, a literal).
    NonCall { value: N },
    /// "`become` does not support operators" of an overloaded operator: a call of `method`,
    /// the operator trait's, with the operands, `rhs` the second of a binary one. The method
    /// is `None` for a trait rustc does not suggest calling (`PartialEq`, `PartialOrd`).
    Op { site: CallSite<N>, method: Option<&'static str>, lhs: N, rhs: Option<N> },
    /// "tail calling closures directly is not allowed": `callee` is a closure, called with
    /// `args` arguments, or `None` when its argument type is not a tuple.
    CallingClosure { callee: N, args: Option<u32> },
    /// "tail calling intrinsics is not allowed".
    Intrinsic,
    /// "tail calls can only be performed with function definitions or pointers": the callee
    /// `callee` of the call at `site` is of type `ty`, which `deref` derefs to a function.
    NonFnCallee { site: CallSite<N>, callee: N, ty: StoredTy, deref: Option<DerefToFn> },
    /// "mismatched function ABIs".
    AbiMismatch { caller: ExternAbi, callee: ExternAbi },
    /// "ABI does not support guaranteed tail calls".
    UnsupportedAbi(ExternAbi),
    /// "mismatched signatures": the caller's and the callee's signatures, each as the type of
    /// a function pointer with its late-bound regions free.
    SignatureMismatch { caller: StoredTy, callee: StoredTy },
    /// "a function marked with `#[track_caller]` cannot perform a tail-call".
    TrackCallerCaller,
    /// "tail-calls are not allowed in c-variadic functions".
    CVariadicCaller,
    /// "c-variadic functions can't be tail-called".
    CVariadicCallee,
    /// "unsized arguments cannot be used in a tail call", of an argument of type `ty`.
    UnsizedArgument(StoredTy),
    /// A type of the signatures that does not normalize in the caller's environment, which
    /// rustc's `normalize_erasing_regions` takes to be a bug. blaze's own error, naming it.
    NotNormalized(StoredTy),
}

/// How a callee that is no function derefs to one, through boxes and references: rustc's
/// suggestion to deref it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DerefToFn {
    /// How many `*` it takes.
    pub count: u32,
    /// It derefs to a function pointer, not a function definition.
    pub pointer: bool,
}

/// Where a call is, as rustc's THIR has it: `fn_span`, the whole call of a call expression or
/// an operator, from the method's name on of a method call; and `fun`'s span, the callee
/// expression or the method's name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallSite<N = ExprId> {
    pub call: N,
    /// `call` is a method call: the spans are of its method's name.
    pub method: bool,
}

impl<N> CallSite<N> {
    pub fn map<M>(self, f: impl FnOnce(N) -> M) -> CallSite<M> {
        CallSite { call: f(self.call), method: self.method }
    }
}

impl<N> TailCallErrorKind<N> {
    /// The same refusal, each expression named by `f`'s node, or the first `Err` it gives.
    pub fn try_map<M, E>(
        self,
        mut f: impl FnMut(N) -> Result<M, E>,
    ) -> Result<TailCallErrorKind<M>, E> {
        use TailCallErrorKind::*;
        Ok(match self {
            InClosure => InClosure,
            BuiltinOp { value } => BuiltinOp { value: f(value)? },
            NonCall { value } => NonCall { value: f(value)? },
            Op { site, method, lhs, rhs } => Op {
                site: CallSite { call: f(site.call)?, method: site.method },
                method,
                lhs: f(lhs)?,
                rhs: rhs.map(&mut f).transpose()?,
            },
            CallingClosure { callee, args } => CallingClosure { callee: f(callee)?, args },
            Intrinsic => Intrinsic,
            NonFnCallee { site, callee, ty, deref } => NonFnCallee {
                site: CallSite { call: f(site.call)?, method: site.method },
                callee: f(callee)?,
                ty,
                deref,
            },
            AbiMismatch { caller, callee } => AbiMismatch { caller, callee },
            UnsupportedAbi(abi) => UnsupportedAbi(abi),
            SignatureMismatch { caller, callee } => SignatureMismatch { caller, callee },
            TrackCallerCaller => TrackCallerCaller,
            CVariadicCaller => CVariadicCaller,
            CVariadicCallee => CVariadicCallee,
            UnsizedArgument(ty) => UnsizedArgument(ty),
            NotNormalized(ty) => NotNormalized(ty),
        })
    }

    /// rustc's message.
    pub fn message(&self) -> &'static str {
        use TailCallErrorKind::*;
        match self {
            InClosure => "`become` is not allowed in closures",
            BuiltinOp { .. } | Op { .. } => "`become` does not support operators",
            NonCall { .. } => "`become` requires a function call",
            CallingClosure { .. } => "tail calling closures directly is not allowed",
            Intrinsic => "tail calling intrinsics is not allowed",
            NonFnCallee { .. } => {
                "tail calls can only be performed with function definitions or pointers"
            }
            AbiMismatch { .. } => "mismatched function ABIs",
            UnsupportedAbi(_) => "ABI does not support guaranteed tail calls",
            SignatureMismatch { .. } => "mismatched signatures",
            TrackCallerCaller => {
                "a function marked with `#[track_caller]` cannot perform a tail-call"
            }
            CVariadicCaller => "tail-calls are not allowed in c-variadic functions",
            CVariadicCallee => "c-variadic functions can't be tail-called",
            UnsizedArgument(_) => "unsized arguments cannot be used in a tail call",
            NotNormalized(_) => "blaze could not normalize a type of a tail call's signatures",
        }
    }
}

/// One refusal of rustc's `check_tail_calls`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TailCallError {
    /// The `become` expression.
    pub become_expr: ExprId,
    /// The innermost closure, coroutine or `async` block the `become` is in, whose body rustc
    /// gives its error MIR; `None` for the item's own body.
    pub body: Option<ExprId>,
    pub kind: TailCallErrorKind,
}

/// A tail call of a function marked `#[track_caller]`: rustc's `tail_call_track_caller`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TrackCallerCall {
    pub become_expr: ExprId,
    pub site: CallSite,
}

/// What [`tail_call_check`] finds in a body and the closures in it, in rustc's order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct TailCallCheck {
    errors: Box<[TailCallError]>,
    track_caller: Box<[TrackCallerCall]>,
}

impl TailCallCheck {
    #[inline]
    pub fn errors(&self) -> &[TailCallError] {
        &self.errors
    }

    #[inline]
    pub fn track_caller_calls(&self) -> &[TrackCallerCall] {
        &self.track_caller
    }

    /// Whether some tail call of the body or a closure in it is refused.
    #[inline]
    pub fn failed(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Whether rustc gives `body` (a closure's, coroutine's or `async` block's expression, or
    /// `None` for the item's own) its error MIR: a tail call written in it is refused.
    pub fn fails_body(&self, body: Option<ExprId>) -> bool {
        self.errors.iter().any(|it| it.body == body)
    }
}

/// Whether `store` has a `become` anywhere: one bit of the kinds the store has, read before
/// asking [`tail_call_check`] of a body that has none.
#[inline]
pub fn has_become(store: &ExpressionStore) -> bool {
    store.kinds_present() & (1 << ExprKind::Become as u32) != 0
}

/// rustc's `check_tail_calls` of `def` and the closures in it, and its tail calls of
/// `#[track_caller]` functions.
#[blaze_query::tracked(returns(ref))]
pub fn tail_call_check(db: &Db, def: DefWithBodyId) -> TailCallCheck {
    let body = Body::of(db, def);
    if !has_become(body) {
        return TailCallCheck::default();
    }
    // rustc's `thir_body` fails for a body whose type check is tainted, and the check skips
    // it; a `become` outside a function is such an error (E0572).
    let infer = InferenceResult::of(db, def);
    if infer.has_errors || infer.has_type_mismatches() || infer.is_erroneous() {
        return TailCallCheck::default();
    }
    let DefWithBodyId::FunctionId(caller) = def else { return TailCallCheck::default() };
    let krate = def.krate(db);
    let interner = DbInterner::new_with(db, krate);
    let infcx = interner.infer_ctxt().build(TypingMode::non_body_analysis());
    // With what the body's `for const` loops assume of their binders, as inference had it.
    let env =
        hir_ty::comptime_env::body_env(&infcx, infer, db.trait_environment(def.generic_def(db)));
    let mut checker = Checker {
        db,
        interner,
        infcx: &infcx,
        env,
        krate,
        infer,
        store: body,
        caller,
        caller_sig: None,
        lang: lang_items(db, krate),
        // alloc: only at the first refusal, so none for a body without one.
        errors: Vec::new(),
        // alloc: only at the first such call.
        track_caller: Vec::new(),
    };
    checker.walk(body.root_expr());
    TailCallCheck {
        errors: checker.errors.into_boxed_slice(),
        track_caller: checker.track_caller.into_boxed_slice(),
    }
}

/// The callee of a tail call as rustc's THIR has it: `ExprKind::Call`'s `ty`, `fun` and
/// `fn_span`, and its arguments' types.
struct Call<'db> {
    ty: Ty<'db>,
    fun: ExprId,
    site: CallSite,
}

struct Checker<'a, 'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    infcx: &'a InferCtxt<'db>,
    env: ParamEnv<'db>,
    krate: base_db::Crate,
    infer: &'db InferenceResult<'db>,
    store: &'db ExpressionStore,
    caller: FunctionId,
    /// The caller's signature, erased and normalized, made at the first tail call.
    caller_sig: Option<Result<FnSig<'db>, Ty<'db>>>,
    lang: &'db LangItems,
    errors: Vec<TailCallError>,
    track_caller: Vec<TrackCallerCall>,
}

impl<'db> Checker<'_, 'db> {
    /// THIR's pre-order: a `become` is checked before what is in it. `body` is the closure the
    /// walk is in.
    fn walk(&mut self, root: ExprId) {
        // alloc: the walk's stack, once per body with a `become`
        let mut stack = vec![(root, None)];
        // alloc: the children of one expression, reused
        let mut children = Vec::new();
        while let Some((expr, body)) = stack.pop() {
            let inner = match self.store.expr(expr) {
                Expr::Become { expr: value } => {
                    self.check(expr, value, body);
                    body
                }
                Expr::Closure { .. } => Some(expr),
                _ => body,
            };
            children.clear();
            self.store.walk_child_exprs(expr, |child| children.push(child));
            stack.extend(children.iter().rev().map(|&child| (child, inner)));
        }
    }

    fn refuse(&mut self, become_expr: ExprId, body: Option<ExprId>, kind: TailCallErrorKind) {
        self.errors.push(TailCallError { become_expr, body, kind });
    }

    /// rustc's `check_tail_call` of `become value`.
    fn check(&mut self, become_expr: ExprId, value: ExprId, body: Option<ExprId>) {
        use TailCallErrorKind::*;
        if body.is_some() {
            self.refuse(become_expr, body, InClosure);
            return;
        }
        let call = match self.call_of(value) {
            Ok(call) => call,
            Err(kind) => {
                self.refuse(become_expr, body, kind);
                return;
            }
        };
        // An overloaded operator is a call the source does not write (`from_hir_call`).
        if let Some(op) = self.overloaded_op(value, &call) {
            self.refuse(become_expr, body, op);
        }

        if let TyKind::FnDef(def, args) = call.ty.kind()
            && let CallableDefId::FunctionId(func) = def.0
        {
            let args = args.skip_binder();
            // A call of a closure, or of anything else through a `Fn*` trait, is a call of the
            // trait's method with the callee as `Self`.
            if let ItemContainerId::TraitId(parent) = func.lookup(self.db).container
                && self.is_fn_trait(parent)
                && let Some(this) = args.as_slice().first().and_then(|it| it.ty())
            {
                let kind = match this.kind() {
                    TyKind::Closure(..) => {
                        let tupled = args.as_slice().get(1).and_then(|it| it.ty());
                        let args = match tupled.map(|it| it.kind()) {
                            Some(TyKind::Tuple(tys)) => Some(tys.len() as u32),
                            _ => None,
                        };
                        CallingClosure { callee: call.fun, args }
                    }
                    _ => non_fn_callee(call.site, call.fun, this),
                };
                self.refuse(become_expr, body, kind);
                return;
            }
            if FunctionSignature::is_intrinsic(self.db, func) {
                self.refuse(become_expr, body, Intrinsic);
            }
        }

        if !matches!(call.ty.kind(), TyKind::FnDef(..) | TyKind::FnPtr(..)) {
            let kind = non_fn_callee(call.site, call.fun, call.ty);
            self.refuse(become_expr, body, kind);
            return;
        }

        let caller_sig = match self.caller_sig() {
            Ok(sig) => sig,
            Err(ty) => {
                self.refuse(become_expr, body, NotNormalized(ty.store()));
                return;
            }
        };
        let Some(callee_sig) = call.ty.callable_sig(self.interner) else {
            unreachable!("a function definition or pointer has a signature: {:?}", call.ty)
        };
        let callee_sig = self.interner.instantiate_bound_regions_with_erased(callee_sig);
        let callee_sig = match self.erase_and_normalize(callee_sig) {
            Ok(sig) => sig,
            Err(ty) => {
                self.refuse(become_expr, body, NotNormalized(ty.store()));
                return;
            }
        };

        if caller_sig.abi() != callee_sig.abi() {
            let kind = AbiMismatch { caller: caller_sig.abi(), callee: callee_sig.abi() };
            self.refuse(become_expr, body, kind);
        }
        if !supports_guaranteed_tail_call(callee_sig.abi()) {
            self.refuse(become_expr, body, UnsupportedAbi(callee_sig.abi()));
        }
        // `extern "tail"` callees need not have the caller's signature.
        if caller_sig.inputs_and_output != callee_sig.inputs_and_output
            && callee_sig.abi() != ExternAbi::RustTail
        {
            let kind = SignatureMismatch {
                caller: self.printed_caller_sig().store(),
                callee: self.printed_callee_sig(call.ty).store(),
            };
            self.refuse(become_expr, body, kind);
        }
        // `#[track_caller]` adds a location argument, which the caller would have to pass on.
        if track_caller(self.db, self.caller) {
            self.refuse(become_expr, body, TrackCallerCaller);
        }
        if caller_sig.c_variadic() {
            self.refuse(become_expr, body, CVariadicCaller);
        }
        if callee_sig.c_variadic() {
            self.refuse(become_expr, body, CVariadicCallee);
        }
        for &arg in callee_sig.inputs().iter() {
            if !self.infcx.type_is_sized_modulo_regions(self.env, arg) {
                self.refuse(become_expr, body, UnsizedArgument(arg.store()));
            }
        }

        if let TyKind::FnDef(def, args) = call.ty.kind()
            && let CallableDefId::FunctionId(func) = def.0
            && self.callee_is_track_caller(func, args.skip_binder())
        {
            self.track_caller.push(TrackCallerCall { become_expr, site: call.site });
        }
    }

    /// What THIR makes of `value`: a call, or the refusal of what is not one.
    fn call_of(&self, value: ExprId) -> Result<Call<'db>, TailCallErrorKind> {
        use TailCallErrorKind::*;
        let builtin = BuiltinOp { value };
        let non_call = NonCall { value };
        let overloaded = |it: ExprId| self.infer.method_resolution(it);
        let as_call = |(func, args)| Call {
            ty: Ty::new_fn_def(self.interner, CallableDefId::FunctionId(func).into(), args),
            fun: value,
            site: CallSite { call: value, method: false },
        };
        match self.store.expr(value) {
            Expr::Call { callee, .. } => {
                if let Some(it) = overloaded(value) {
                    return Ok(Call { fun: callee, ..as_call(it) });
                }
                // THIR's `ty` is the callee's own type, before the autoderef that makes a call
                // of `&fn()` builtin: rustc refuses it as no function.
                let ty = self.infer.expr_ty(callee);
                match ty.kind() {
                    // A tuple struct's or variant's constructor is THIR's `Adt`.
                    TyKind::FnDef(def, _) if !matches!(def.0, CallableDefId::FunctionId(_)) => {
                        Err(non_call)
                    }
                    _ => {
                        Ok(Call { ty, fun: callee, site: CallSite { call: value, method: false } })
                    }
                }
            }
            Expr::MethodCall { .. } => match overloaded(value) {
                Some(it) => Ok(Call {
                    fun: value,
                    site: CallSite { call: value, method: true },
                    ..as_call(it)
                }),
                None => unreachable!("a method call inference found no error in is resolved"),
            },
            Expr::UnaryOp { expr, op: UnaryOp::Not | UnaryOp::Neg } => match overloaded(value) {
                Some(it) if !self.infer.expr_ty(expr).is_scalar() => Ok(as_call(it)),
                _ => Err(builtin),
            },
            Expr::BinaryOp { lhs, rhs, op: Some(op) } => match op {
                // `&&` and `||` are THIR's `LogicalOp`, never a call.
                BinaryOp::LogicOp(_) => Err(non_call),
                BinaryOp::ArithOp(_)
                | BinaryOp::CmpOp(_)
                | BinaryOp::Assignment { op: Some(_) } => {
                    let scalar =
                        self.infer.expr_ty(lhs).is_scalar() && self.infer.expr_ty(rhs).is_scalar();
                    match overloaded(value) {
                        Some(it) if !scalar => Ok(as_call(it)),
                        _ => Err(builtin),
                    }
                }
                BinaryOp::Assignment { op: None } => Err(non_call),
            },
            // An overloaded index is THIR's `*Index::index(..)`, a deref.
            Expr::Index { base, index } => match overloaded(value) {
                Some(_) if !is_builtin_index(self.infer, base, index) => Err(non_call),
                _ => Err(builtin),
            },
            _ => Err(non_call),
        }
    }

    /// rustc's `report_op` of a call `value` the source writes as an operator.
    fn overloaded_op(&self, value: ExprId, call: &Call<'db>) -> Option<TailCallErrorKind> {
        let (lhs, rhs) = match self.store.expr(value) {
            Expr::UnaryOp { expr, op: UnaryOp::Not | UnaryOp::Neg } => (expr, None),
            Expr::BinaryOp { lhs, rhs, .. } => (lhs, Some(rhs)),
            _ => return None,
        };
        let method = match call.ty.kind() {
            TyKind::FnDef(def, _) => match def.0 {
                CallableDefId::FunctionId(func) => match func.lookup(self.db).container {
                    ItemContainerId::TraitId(it) => self.op_method(it),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        };
        Some(TailCallErrorKind::Op { site: call.site, method, lhs, rhs })
    }

    /// rustc's `op_trait_as_method_name`.
    fn op_method(&self, it: TraitId) -> Option<&'static str> {
        let lang = self.lang;
        let table = [
            (lang.Add, "add"),
            (lang.Sub, "sub"),
            (lang.Mul, "mul"),
            (lang.Div, "div"),
            (lang.Rem, "rem"),
            (lang.Neg, "neg"),
            (lang.Not, "not"),
            (lang.BitXor, "bitxor"),
            (lang.BitAnd, "bitand"),
            (lang.BitOr, "bitor"),
            (lang.Shl, "shl"),
            (lang.Shr, "shr"),
            (lang.AddAssign, "add_assign"),
            (lang.SubAssign, "sub_assign"),
            (lang.MulAssign, "mul_assign"),
            (lang.DivAssign, "div_assign"),
            (lang.RemAssign, "rem_assign"),
            (lang.BitXorAssign, "bitxor_assign"),
            (lang.BitAndAssign, "bitand_assign"),
            (lang.BitOrAssign, "bitor_assign"),
            (lang.ShlAssign, "shl_assign"),
            (lang.ShrAssign, "shr_assign"),
            (lang.Index, "index"),
            (lang.IndexMut, "index_mut"),
        ];
        table.into_iter().find(|(trait_, _)| *trait_ == Some(it)).map(|(_, name)| name)
    }

    /// rustc's `fn_trait_kind_from_def_id`: `Fn`, `FnMut` or `FnOnce`, not the `async` ones.
    fn is_fn_trait(&self, it: TraitId) -> bool {
        [self.lang.Fn, self.lang.FnMut, self.lang.FnOnce].contains(&Some(it))
    }

    /// THIR's `body_type` of the caller, regions erased and binders anonymized, normalized.
    fn caller_sig(&mut self) -> Result<FnSig<'db>, Ty<'db>> {
        if let Some(sig) = self.caller_sig {
            return sig;
        }
        let sig = self
            .db
            .callable_item_signature(self.caller.into())
            .instantiate_identity()
            .skip_norm_wip();
        let sig = self.interner.instantiate_bound_regions_with_erased(sig);
        let sig = self.erase_and_normalize(sig);
        self.caller_sig = Some(sig);
        sig
    }

    /// rustc's `normalize_erasing_regions` after `erase_and_anonymize_regions`, in the caller's
    /// environment; `Err` names the type that does not normalize.
    fn erase_and_normalize(&self, sig: FnSig<'db>) -> Result<FnSig<'db>, Ty<'db>> {
        let sig = sig.fold_with(&mut EraseRegions { interner: self.interner, anonymize: true });
        let cause = ObligationCause::dummy();
        let at = || self.infcx.at(&cause, self.env);
        let normalized = at().deeply_normalize(sig).ok().filter(|it| !it.has_infer());
        let Some(normalized) = normalized else {
            let ty = sig
                .inputs_and_output
                .iter()
                .find(|&ty| at().deeply_normalize(ty).ok().is_none_or(|it| it.has_infer()));
            return Err(ty.unwrap_or(sig.output()));
        };
        // What normalizing gives can name regions of an impl.
        Ok(normalized.fold_with(&mut EraseRegions { interner: self.interner, anonymize: true }))
    }

    /// The caller's signature as rustc's note prints it: of `type_of(caller)`, its late-bound
    /// regions free.
    fn printed_caller_sig(&self) -> Ty<'db> {
        let sig = self
            .db
            .callable_item_signature(self.caller.into())
            .instantiate_identity()
            .skip_norm_wip();
        let sig = self.interner.liberate_late_bound_regions(self.caller.into(), sig);
        Ty::new_fn_ptr(self.interner, Binder::dummy(sig))
    }

    /// The callee's signature as rustc's note prints it: of the callee's type, its late-bound
    /// regions free.
    fn printed_callee_sig(&self, callee: Ty<'db>) -> Ty<'db> {
        let callee =
            callee.fold_with(&mut EraseRegions { interner: self.interner, anonymize: false });
        let Some(sig) = callee.callable_sig(self.interner) else {
            unreachable!("a function definition or pointer has a signature: {callee:?}")
        };
        let sig = self.interner.liberate_late_bound_regions(self.caller.into(), sig);
        Ty::new_fn_ptr(self.interner, Binder::dummy(sig))
    }

    /// Whether the function a tail call of `func` with `args` makes is `#[track_caller]`, as
    /// far as the caller's environment resolves it: rustc's `requires_caller_location` of the
    /// instance codegen makes.
    fn callee_is_track_caller(
        &self,
        func: FunctionId,
        args: hir_ty::next_solver::GenericArgs<'db>,
    ) -> bool {
        if track_caller(self.db, func) {
            return true;
        }
        if !matches!(func.lookup(self.db).container, ItemContainerId::TraitId(_)) {
            return false;
        }
        let env = ParamEnvAndCrate { param_env: self.env, krate: self.krate };
        match self.db.lookup_impl_method(env, func, args) {
            (Either::Left(resolved), _) if resolved != func => track_caller(self.db, resolved),
            _ => false,
        }
    }
}

/// rustc's `report_nonfn_callee` of a callee `fun` of type `ty`.
fn non_fn_callee(site: CallSite, fun: ExprId, ty: Ty<'_>) -> TailCallErrorKind {
    let mut derefed = ty;
    let mut count = 0;
    while derefed.is_box() || matches!(derefed.kind(), TyKind::Ref(..)) {
        let Some(inner) = derefed.builtin_deref(false) else { break };
        derefed = inner;
        count += 1;
    }
    let deref = match derefed.kind() {
        TyKind::FnDef(..) if count > 0 => Some(DerefToFn { count, pointer: false }),
        TyKind::FnPtr(..) if count > 0 => Some(DerefToFn { count, pointer: true }),
        _ => None,
    };
    TailCallErrorKind::NonFnCallee { site, callee: fun, ty: ty.store(), deref }
}

/// Whether `base[index]` is builtin: writeback leaves it no method (rustc's
/// `fix_index_builtin_expr`), as `hir_ty_mir`'s `is_builtin_index` decides.
fn is_builtin_index(infer: &InferenceResult<'_>, base: ExprId, index: ExprId) -> bool {
    let adjusted = |expr: ExprId| infer.expr_ty_after_adjustments(expr);
    matches!(adjusted(index).kind(), TyKind::Uint(rustc_type_ir::UintTy::Usize))
        && matches!(adjusted(base).strip_reference().kind(), TyKind::Array(..) | TyKind::Slice(..))
}

/// Whether `func` is `#[track_caller]`, its own attribute or, for an impl's method, that of the
/// trait's declaration of it: rustc's `codegen_fn_attrs` (as `hir_ty_mir`'s `track_caller`).
pub fn track_caller(db: &Db, func: FunctionId) -> bool {
    let has =
        |func: FunctionId| AttrFlags::query(db, func.into()).contains(AttrFlags::TRACK_CALLER);
    if has(func) {
        return true;
    }
    let ItemContainerId::ImplId(impl_id) = func.lookup(db).container else { return false };
    let Some(trait_ref) = db.impl_trait(impl_id) else { return false };
    let name = &FunctionSignature::of(db, func).name;
    trait_ref.skip_binder().def_id.0.trait_items(db).method_by_name(name).is_some_and(has)
}

/// rustc's `ExternAbi::supports_guaranteed_tail_call` (`compiler/rustc_abi/src/
/// extern_abi.rs`), which `rustc_abi` builds only with its `nightly` feature.
pub fn supports_guaranteed_tail_call(abi: ExternAbi) -> bool {
    match abi {
        ExternAbi::CmseNonSecureCall | ExternAbi::CmseNonSecureEntry => false,
        ExternAbi::AvrInterrupt
        | ExternAbi::AvrNonBlockingInterrupt
        | ExternAbi::Msp430Interrupt
        | ExternAbi::RiscvInterruptM
        | ExternAbi::RiscvInterruptS
        | ExternAbi::X86Interrupt => false,
        ExternAbi::GpuKernel | ExternAbi::PtxKernel => false,
        ExternAbi::Custom => false,
        ExternAbi::RustCall => false,
        ExternAbi::C { .. }
        | ExternAbi::System { .. }
        | ExternAbi::Rust
        | ExternAbi::RustCold
        | ExternAbi::RustInvalid
        | ExternAbi::LlvmIntrinsic
        | ExternAbi::EfiApi
        | ExternAbi::Aapcs { .. }
        | ExternAbi::Cdecl { .. }
        | ExternAbi::Stdcall { .. }
        | ExternAbi::Fastcall { .. }
        | ExternAbi::Thiscall { .. }
        | ExternAbi::Vectorcall { .. }
        | ExternAbi::SysV64 { .. }
        | ExternAbi::Win64 { .. }
        | ExternAbi::RustPreserveNone
        | ExternAbi::RustTail
        | ExternAbi::Swift => true,
    }
}

/// rustc's `erase_and_anonymize_regions` (with `anonymize`), or its free regions erased alone.
struct EraseRegions<'db> {
    interner: DbInterner<'db>,
    anonymize: bool,
}

impl<'db> TypeFolder<DbInterner<'db>> for EraseRegions<'db> {
    fn cx(&self) -> DbInterner<'db> {
        self.interner
    }

    fn fold_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if ty.has_type_flags(TypeFlags::HAS_BINDER_VARS | TypeFlags::HAS_FREE_REGIONS) {
            ty.super_fold_with(self)
        } else {
            ty
        }
    }

    fn fold_binder<T: TypeFoldable<DbInterner<'db>>>(
        &mut self,
        t: rustc_type_ir::Binder<DbInterner<'db>, T>,
    ) -> rustc_type_ir::Binder<DbInterner<'db>, T> {
        use rustc_type_ir::Interner as _;
        match self.anonymize {
            true => self.interner.anonymize_bound_vars(t).super_fold_with(self),
            false => t.super_fold_with(self),
        }
    }

    fn fold_region(&mut self, region: Region<'db>) -> Region<'db> {
        match region.kind() {
            RegionKind::ReBound(..) => region,
            _ => Region::new_erased(self.interner),
        }
    }
}
