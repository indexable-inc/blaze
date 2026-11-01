//! The calls of a const context: rustc's `Checker::visit_terminator` of a `Call` or `TailCall`
//! and `build_error_for_const_call` (`check_consts/{check,ops}.rs`). What rustc reads off the
//! call's `CallSource` and `call_kind` (`rustc_middle/src/util/call_kind.rs`) is read here off
//! the expression the call is lowered from: every call lowering makes is `from_hir_call`.

use hir_def::{
    CallableDefId, FunctionId, GenericDefId, ItemContainerId, Lookup, TraitId,
    attrs::AttrFlags,
    hir::{
        Expr, ExprId, UnaryOp,
        generics::{GenericParams, WherePredicate},
        type_ref::{TraitBoundModifier, TypeBound},
    },
    signatures::{FnFlags, FunctionSignature},
};
use hir_ty::{
    InferBodyId,
    db::HirDatabase,
    effects::{
        body_constness, const_call_error, const_param_env, fn_is_const, impl_is_const,
        trait_is_const,
    },
    intrinsic::Intrinsic,
    next_solver::{GenericArgs, Ty, TyKind, default_types},
};
use intern::Symbol;
use rustc_type_ir::{
    BoundConstness,
    inherent::{GenericArgs as _, IntoKind},
};

use super::{
    Checker, ClosureNote, ConstCheckKind, Desugaring, MirSpan, Operand, Status, described,
    is_const_callable, operand_ty,
    stability::{self, Item},
};
use crate::mir::StoredPlace;

/// How the source makes a call: rustc's `CallSource` and `from_hir_call`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// Written: `f(..)`, `x.f(..)`, a closure's `c(..)`.
    Call,
    /// An overloaded operator, index or deref, `from_hir_call` false.
    Overloaded,
    /// A desugaring's call, `from_hir_call` as the call it is.
    Desugared(Desugaring),
    /// A constant pattern's comparison, `from_hir_call` false.
    MatchCmp,
}

impl<'db> Checker<'_, 'db> {
    pub(super) fn check_call(
        &mut self,
        func: &Operand,
        args: &[Operand],
        destination: Option<&StoredPlace>,
        intrinsic: Option<&Intrinsic>,
        span: MirSpan,
    ) {
        let MirSpan::ExprId(expr) = span else { return };
        let (callee, fn_args) = match operand_ty(self.body, func) {
            Some(TyKind::FnPtr(..)) => {
                if matches!(self.store.expr(expr), Expr::Call { .. }) {
                    self.check_op(ConstCheckKind::FnPointerCall, Status::Forbidden, span);
                }
                return;
            }
            Some(TyKind::FnDef(callee, fn_args)) => match callee.0 {
                CallableDefId::FunctionId(callee) => (callee, fn_args.skip_binder()),
                _ => return,
            },
            _ => return,
        };
        let Some(source) = self.source(expr, callee) else { return };
        let db = self.db;

        if let ItemContainerId::TraitId(trait_) = callee.lookup(db).container {
            self.check_trait_call(trait_, callee, fn_args, destination, source, span);
            return;
        }

        if has_const_conditions(db, callee) {
            if !self.conditions_hold(callee, fn_args) {
                self.tainted = true;
                return;
            }
            let kind = self.call_error(callee, fn_args, destination, source, true);
            self.check_op(kind, Status::conditionally_const(), span);
        }

        let signature = FunctionSignature::of(db, callee);
        if signature.is_varargs() {
            let status = Status::Unstable {
                gate: Symbol::intern("const_c_variadic"),
                already_checked: false,
                safe_to_expose: false,
                is_function_call: true,
            };
            self.check_op(ConstCheckKind::CVariadicCall, status, span);
        }

        // Const eval of `begin_panic` takes its argument to be `&str`, of `panic_display`
        // `&&str`; the call is otherwise allowed.
        let refs_to_str = match () {
            () if self.lang.BeginPanic == Some(callee) => Some(1),
            () if self.lang.PanicDisplay == Some(callee) => Some(2),
            () => None,
        };
        if let Some(refs) = refs_to_str {
            let arg = args.first().and_then(|it| operand_ty(self.body, it));
            if !arg.is_some_and(|ty| refs_to_str_ty(ty, refs)) {
                self.check_op(ConstCheckKind::PanicNonStr, Status::Forbidden, span);
            }
            return;
        }

        if intrinsic.is_some() || signature.flags.contains(FnFlags::RUSTC_INTRINSIC) {
            let name = || {
                intrinsic.map_or_else(
                    // alloc: the text of a diagnostic, made when it is reported
                    || signature.name.as_str().to_owned(),
                    // alloc: the text of a diagnostic, made when it is reported
                    |it| it.to_string(),
                )
            };
            if !is_const_callable(db, callee) {
                let kind = ConstCheckKind::NonConstIntrinsic { name: name() };
                self.check_op(kind, Status::Forbidden, span);
                return;
            }
            self.check_intrinsic_stability(callee, name, span);
            return;
        }

        if !is_const_callable(db, callee) {
            let kind = self.call_error(callee, fn_args, destination, source, false);
            self.check_op(kind, Status::Forbidden, span);
            return;
        }
        self.check_callee_stability(Item::Fn(callee), span);
    }

    /// The call of `callee`, a method of `trait_`: what the impl is is not known here. rustc
    /// calls it conditionally const only when the trait is const and its conditions hold.
    fn check_trait_call(
        &mut self,
        trait_: TraitId,
        callee: FunctionId,
        fn_args: GenericArgs<'db>,
        destination: Option<&StoredPlace>,
        source: Source,
        span: MirSpan,
    ) {
        let holds = trait_is_const(self.db, trait_).then(|| self.conditions_hold(callee, fn_args));
        match holds {
            Some(true) => {
                let kind = self.call_error(callee, fn_args, destination, source, true);
                self.check_op(kind, Status::conditionally_const(), span);
                self.check_callee_stability(Item::Trait(trait_), span);
            }
            // typeck's `E0277` of a `[const]` bound that does not hold; a closure's call is
            // left to this check (`DeferredCallResolution` doesn't enforce constness).
            Some(false) if !is_closure(fn_args.type_at(0)) => self.tainted = true,
            _ => {
                let kind = self.call_error(callee, fn_args, destination, source, false);
                self.check_op(kind, Status::Forbidden, span);
            }
        }
    }

    /// The comparison of a constant pattern of `pattern`, lowered as a `BinOp::Eq` of an operand
    /// of type `ty`. rustc's MIR building compares a scalar with `BinOp::Eq` too, and a `&str`
    /// (the one other type a constant pattern compares) with a call of `<str as
    /// PartialEq>::eq`, `CallSource::MatchCmp` (`string_compare`), which is checked here.
    pub(super) fn check_match_cmp(&mut self, ty: TyKind<'db>, pattern: MirSpan) {
        let TyKind::Ref(_, pointee, _) = ty else { return };
        if !matches!(pointee.kind(), TyKind::Str) {
            return;
        }
        let (Some(trait_), Some(callee)) = (self.lang.PartialEq, self.lang.PartialEq_eq) else {
            return;
        };
        let fn_args = GenericArgs::new_from_slice(&[pointee.into(), pointee.into()]);
        self.check_trait_call(trait_, callee, fn_args, None, Source::MatchCmp, pattern);
    }

    /// How the source makes the call of `callee` lowered from `expr`, or `None` for a call
    /// lowering makes that rustc's MIR does not have as a call (a pattern's comparison, a
    /// `Box`'s allocation, ..).
    fn source(&self, expr: ExprId, callee: FunctionId) -> Option<Source> {
        let resolved = || self.infer.method_resolution(expr).is_some_and(|(it, _)| it == callee);
        match self.store.expr(expr) {
            Expr::Call { callee: path, .. } => {
                if let TyKind::FnDef(it, _) = self.infer.expr_ty(path).kind()
                    && it.0 == CallableDefId::FunctionId(callee)
                {
                    return Some(match self.desugaring(expr, path, callee) {
                        Some(desugaring) => Source::Desugared(desugaring),
                        None => Source::Call,
                    });
                }
                if resolved() {
                    return Some(Source::Call);
                }
            }
            Expr::MethodCall { .. } if resolved() => return Some(Source::Call),
            // rustc's `call_kind`: a call whose span is the `.await` desugaring's, whatever it
            // calls.
            Expr::Await { .. } => return Some(Source::Desugared(Desugaring::Await)),
            Expr::BinaryOp { .. }
            | Expr::UnaryOp { op: UnaryOp::Not | UnaryOp::Neg, .. }
            | Expr::Index { .. }
                if resolved() =>
            {
                return Some(Source::Overloaded);
            }
            _ => {}
        }
        let deref = [self.lang.Deref_deref, self.lang.DerefMut_deref_mut].contains(&Some(callee));
        deref.then_some(Source::Overloaded)
    }

    /// The desugaring the call `expr` of `path` is of: a desugaring's callee is mapped to the
    /// syntax its call is, which a written callee never is.
    fn desugaring(&self, expr: ExprId, path: ExprId, callee: FunctionId) -> Option<Desugaring> {
        let lang = self.lang;
        let desugaring = match Some(callee) {
            it if it == lang.IntoIterIntoIter => Desugaring::ForLoopIntoIter,
            it if it == lang.IteratorNext => Desugaring::ForLoopNext,
            it if it == lang.TryTraitBranch => Desugaring::QuestionBranch,
            it if it == lang.TryTraitFromResidual => Desugaring::QuestionFromResidual,
            it if it == lang.TryTraitFromOutput => Desugaring::TryBlockFromOutput,
            _ => return None,
        };
        let source_map = self.source_map();
        let (Ok(call), Ok(callee)) = (source_map.expr_syntax(expr), source_map.expr_syntax(path))
        else {
            return None;
        };
        (call == callee).then_some(desugaring)
    }

    /// rustc's `revalidate_conditional_constness` of a callee with `[const]` bounds: whether
    /// they hold in this body.
    fn conditions_hold(&self, callee: FunctionId, fn_args: GenericArgs<'db>) -> bool {
        let constness = match self.owner {
            InferBodyId::DefWithBodyId(def) => body_constness(self.db, def),
            InferBodyId::AnonConstId(_) => Some(BoundConstness::Const),
        };
        let Some(constness) = constness else { return true };
        let env = const_param_env(self.db, self.owner.generic_def(self.db));
        const_call_error(self.db, env, constness, callee, fn_args).is_none()
    }

    /// `build_error_for_const_call`: the error of a call of `callee` rustc refuses, by what
    /// `call_kind` makes of it.
    fn call_error(
        &self,
        callee: FunctionId,
        fn_args: GenericArgs<'db>,
        destination: Option<&StoredPlace>,
        source: Source,
        conditionally: bool,
    ) -> ConstCheckKind {
        let (db, lang) = (self.db, self.lang);
        let trait_ = match callee.lookup(db).container {
            ItemContainerId::TraitId(it) => Some(it),
            ItemContainerId::ImplId(impl_) => {
                db.impl_trait(impl_).map(|it| it.skip_binder().def_id.0)
            }
            _ => None,
        };
        let from_hir_call = !matches!(source, Source::Overloaded | Source::MatchCmp);
        let self_ty = || fn_args.type_at(0);
        if let Some(trait_) = trait_
            && [lang.Fn, lang.FnMut, lang.FnOnce].contains(&Some(trait_))
        {
            let note = match self_ty().kind() {
                TyKind::FnDef(it, _) => ClosureNote::FnDef(it.0),
                TyKind::FnPtr(..) => ClosureNote::FnPtr,
                TyKind::Closure(..) => ClosureNote::Closure,
                _ => ClosureNote::None,
            };
            return ConstCheckKind::NonConstClosure { callee, note, conditionally };
        }
        if !from_hir_call
            && let Some(trait_) = trait_
            && self.is_operator(trait_)
        {
            if source == Source::MatchCmp {
                return ConstCheckKind::NonConstMatchEq { ty: self_ty().store(), conditionally };
            }
            let derefs = match lang.PartialEq == Some(trait_) {
                true => consider_dereferencing(fn_args),
                false => None,
            };
            return ConstCheckKind::NonConstOperator { callee, trait_, derefs, conditionally };
        }
        if !from_hir_call && lang.Deref_deref == Some(callee) {
            let target = destination
                .and_then(|it| it.as_ref().as_local())
                .and_then(|it| self.body.locals[it].ty.as_ref().as_reference())
                .map_or(default_types().types.error, |(it, _, _)| it);
            return ConstCheckKind::NonConstDerefCoercion {
                callee,
                ty: self_ty().store(),
                target: target.store(),
                conditionally,
            };
        }
        if let Source::Desugared(desugaring) = source {
            return ConstCheckKind::NonConstDesugaring {
                callee,
                desugaring,
                ty: self_ty().store(),
                conditionally,
            };
        }
        if let ItemContainerId::ImplId(impl_) = callee.lookup(db).container
            && AttrFlags::diagnostic_item(db, impl_.into())
                .is_some_and(|it| it.as_str() == "FmtArgumentsNew")
        {
            return ConstCheckKind::NonConstFmtMacroCall { callee, conditionally };
        }
        let (descr, path) = described(db, self.krate, callee, fn_args);
        let non_const_trait = match callee.lookup(db).container {
            ItemContainerId::TraitId(it) if !trait_is_const(db, it) => Some(it),
            _ => None,
        };
        ConstCheckKind::NonConstFn {
            callee,
            args: fn_args.store(),
            descr,
            path,
            conditionally,
            non_const_trait,
        }
    }

    /// rustc's `lang_items::OPERATORS`.
    fn is_operator(&self, trait_: TraitId) -> bool {
        let l = self.lang;
        [
            l.Add, l.Sub, l.Mul, l.Div, l.Rem, l.Neg, l.Not, l.BitXor, l.BitAnd, l.BitOr, l.Shl,
            l.Shr, l.AddAssign, l.SubAssign, l.MulAssign, l.DivAssign, l.RemAssign,
            l.BitXorAssign, l.BitAndAssign, l.BitOrAssign, l.ShlAssign, l.ShrAssign, l.Index,
            l.IndexMut, l.PartialEq, l.PartialOrd,
        ]
        .contains(&Some(trait_))
    }

    /// The intrinsic part of rustc's `visit_terminator`: a `const` intrinsic is callable when
    /// it is const-stable, or unstable and its feature enabled.
    fn check_intrinsic_stability(
        &mut self,
        callee: FunctionId,
        name: impl Fn() -> String,
        span: MirSpan,
    ) {
        let db = self.db;
        let usable_fallback_body = FunctionSignature::of(db, callee).has_body()
            && !AttrFlags::query(db, callee.into())
                .contains(AttrFlags::RUSTC_INTRINSIC_MUST_BE_OVERRIDDEN)
            && !super::not_const_checked(db, callee);
        let const_stable_indirect = stability::safe_to_expose(db, Item::Fn(callee), self.krate)
            && (usable_fallback_body || stability::intrinsic_const_stable_indirect(db, callee));
        match stability::const_stability(db, Item::Fn(callee)) {
            None => {
                if !const_stable_indirect && self.recursive {
                    let path = described(db, self.krate, callee, GenericArgs::new_from_slice(&[])).1;
                    self.report(ConstCheckKind::UnmarkedIntrinsicExposed { path }, span);
                }
            }
            Some(stability::ConstStability {
                level: stability::Level::Unstable { feature, .. }, ..
            }) => {
                if const_stable_indirect && stability::allows_unstable(db, self.owner, span, feature) {
                    return;
                }
                // clone: a `Symbol`, a count bumped
                let kind = ConstCheckKind::UnstableIntrinsic { name: name(), feature: feature.clone() };
                let status = Status::Unstable {
                    // clone: a `Symbol`, a count bumped
                    gate: feature.clone(),
                    already_checked: false,
                    safe_to_expose: const_stable_indirect,
                    is_function_call: false,
                };
                self.check_op(kind, status, span);
            }
            Some(_) => {}
        }
    }

    /// rustc's `check_callee_stability` of a function or a trait.
    pub(super) fn check_callee_stability(&mut self, item: Item, span: MirSpan) {
        let (db, krate) = (self.db, self.krate);
        let path = || match item {
            Item::Fn(func) => described(db, krate, func, GenericArgs::new_from_slice(&[])).1,
            Item::Trait(trait_) => {
                let name = hir_def::signatures::TraitSignature::of(db, trait_).name.as_str();
                match stability::forces_unstable(db, item.krate(db)) {
                    // A sysroot trait's name is unique, which rustc's trimmed path writes alone.
                    // alloc: the text of a diagnostic, made when it is reported
                    true => name.to_owned(),
                    false => super::def_path(
                        db,
                        krate,
                        hir_def::HasModule::module(&trait_, db),
                        name,
                    ),
                }
            }
        };
        match stability::const_stability(db, item) {
            None => {
                if self.recursive && !stability::safe_to_expose(db, item, self.krate) {
                    self.report(ConstCheckKind::UnmarkedConstItemExposed { path: path() }, span);
                }
            }
            Some(stability::ConstStability {
                level: stability::Level::Unstable { feature, implied_by, issue },
                ..
            }) => {
                let safe_to_expose = stability::safe_to_expose(db, item, self.krate);
                let allows = |feature: &intern::Symbol| {
                    stability::allows_unstable(db, self.owner, span, feature)
                };
                if (allows(feature) || implied_by.as_ref().is_some_and(allows)) && safe_to_expose {
                    return;
                }
                let feature_enabled = item.krate(db) == self.krate
                    || self.features.is_enabled(feature)
                    || implied_by.as_ref().is_some_and(|it| self.features.is_enabled(it))
                    || (feature.as_str() == "rustc_private"
                        && issue.is_some_and(|it| it.get() == 27812)
                        && stability::forces_unstable(db, self.krate));
                if feature_enabled && safe_to_expose {
                    return;
                }
                let kind = match item {
                    Item::Fn(callee) => ConstCheckKind::UnstableConstFn {
                        callee,
                        path: path(),
                        // clone: a `Symbol`, a count bumped
                        feature: feature.clone(),
                    },
                    Item::Trait(trait_) => ConstCheckKind::UnstableConstTrait {
                        trait_,
                        path: path(),
                        // clone: a `Symbol`, a count bumped
                        feature: feature.clone(),
                    },
                };
                let status = Status::Unstable {
                    // clone: a `Symbol`, a count bumped
                    gate: feature.clone(),
                    already_checked: feature_enabled,
                    safe_to_expose,
                    is_function_call: matches!(item, Item::Fn(_)),
                };
                self.check_op(kind, status, span);
            }
            Some(_) => {}
        }
    }
}

/// Whether `ty` is a closure.
fn is_closure(ty: Ty<'_>) -> bool {
    matches!(ty.kind(), TyKind::Closure(..))
}

/// Whether `ty` is `refs` references to `str`.
fn refs_to_str_ty(mut ty: TyKind<'_>, refs: u32) -> bool {
    for _ in 0..refs {
        let TyKind::Ref(_, inner, _) = ty else { return false };
        ty = inner.kind();
    }
    matches!(ty, TyKind::Str)
}

/// rustc's `ConsiderDereferencing` of an `==`: the references to peel when both sides are the
/// same references to a primitive.
fn consider_dereferencing(fn_args: GenericArgs<'_>) -> Option<u32> {
    let (lhs, rhs) = (fn_args.type_at(0), fn_args.type_at(1));
    if lhs != rhs || !matches!(lhs.kind(), TyKind::Ref(..)) {
        return None;
    }
    let mut refs = 0;
    let mut ty = lhs;
    while let TyKind::Ref(_, inner, _) = ty.kind() {
        refs += 1;
        ty = inner;
    }
    let primitive = matches!(
        ty.kind(),
        TyKind::Bool | TyKind::Char | TyKind::Int(_) | TyKind::Uint(_) | TyKind::Float(_)
    );
    primitive.then_some(refs)
}

/// Whether `callee` is conditionally const with `[const]` bounds: what `const_conditions`
/// gives it is not empty (hir-ty's `lower::const_conditions`, which is crate-private: the
/// callee's own `[const]` where-clauses and its `const impl`'s).
fn has_const_conditions(db: &base_db::Db, callee: FunctionId) -> bool {
    if !fn_is_const(db, callee) {
        return false;
    }
    let maybe_const = |def: GenericDefId| {
        GenericParams::of(db, def).where_predicates().iter().any(|it| {
            matches!(
                it,
                WherePredicate::TypeBound {
                    lifetimes: None,
                    bound: TypeBound::Path(_, TraitBoundModifier::MaybeConst),
                    ..
                }
            )
        })
    };
    if maybe_const(callee.into()) {
        return true;
    }
    match callee.lookup(db).container {
        ItemContainerId::TraitId(trait_) => trait_is_const(db, trait_),
        ItemContainerId::ImplId(impl_) => impl_is_const(db, impl_.into()) && maybe_const(impl_.into()),
        _ => false,
    }
}
