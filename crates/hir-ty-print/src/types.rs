//! Types as rustc prints them: `FmtPrinter::print_type` with its length limit,
//! `pretty_print_type` of every kind of type, closures and coroutines by where they are
//! written, `dyn` and `impl Trait` with their sugar; and the same types as `type_name`, a
//! symbol and debug info spell them. Ported from `compiler/rustc_middle/src/ty/print/pretty.rs`,
//! `compiler/rustc_const_eval/src/util/type_name.rs`, `compiler/rustc_symbol_mangling/src/v0.rs`
//! and `compiler/rustc_codegen_ssa/src/debuginfo/type_names.rs` (MIT or Apache-2.0).

use hir_def::{
    AdtId, GenericDefId, HasModule as _, Lookup as _, TypeAliasId,
    hir::generics::{GenericParams, TypeOrConstParamData},
    hir::{ClosureKind as HirClosureKind, CoroutineKind, CoroutineSource, Movability},
    lang_item::lang_items,
};
use hir_ty::{
    GenericPredicates, ImplTraitId, InferBodyId, TyDefId,
    db::{HirDatabase as _, InternedClosure, InternedOpaqueTyId},
    next_solver::{
        AssocTyId, Binder, BoundExistentialPredicates, CallableIdWrapper, Clause, ClauseKind,
        DbInterner, ExistentialProjection, FnSig, GenericArg, GenericArgs, ParamTy, Region,
        RegionExt as _, Term, TermId, TermKind, TraitClause, TraitIdWrapper, TraitRef, Ty, TyKind,
        Tys,
    },
};
use rustc_abi::ExternAbi;
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, ClausePolarity, ClosureKind, InferTy, Mutability, Upcast as _,
    elaborate::elaborate,
    fold_regions,
    inherent::{
        BoundExistentialPredicates as _, Clause as _, GenericArgs as _, IntoKind as _, Safety as _,
        Term as _, Ty as _,
    },
};
use smallvec::SmallVec;

use crate::{
    def::{self, Def, Parent},
    printer::{BinderKind, Mode, NOT_LOOKED_UP, Printer, Spelling, kind_label},
};

/// What rustc's `pretty_print_opaque_impl_type` gathers of a parenthesized trait.
struct FnTrait<'t> {
    bound: Binder<'t, (Tys<'t>, Ty<'t>)>,
    is_async: bool,
    kind: ClosureKind,
    return_ty: Option<Binder<'t, Term<'t>>>,
}

type Traits<'t> = Vec<(Binder<'t, TraitClause<'t>>, SmallVec<[(TermId, Term<'t>); 2]>)>;

/// How a type with no parts is written, the same in every spelling of a type: `type_name`,
/// symbols, debug info, diagnostics and the names `hir-ty-mir`'s users keep across runs.
/// `None` for any other type.
pub fn scalar_name(ty: Ty<'_>) -> Option<&'static str> {
    Some(match ty.kind() {
        TyKind::Bool => "bool",
        TyKind::Char => "char",
        TyKind::Str => "str",
        TyKind::Never => "!",
        TyKind::Int(it) => it.name_str(),
        TyKind::Uint(it) => it.name_str(),
        TyKind::Float(it) => it.name_str(),
        _ => return None,
    })
}

impl<'h, 't> Printer<'h, 't> {
    /// rustc's `FmtPrinter::print_type`: past the type length limit a type prints as `_`,
    /// except `()`, a closure, and a short-named ADT with at most one simple argument, which
    /// prints with paths forced trimmed.
    pub fn print_type(&mut self, ty: Ty<'t>) {
        if self.refused.is_some() {
            return;
        }
        if self.spelling() != Spelling::Diagnostic {
            if self.print_spelled_type(ty) {
                self.pretty_print_type(ty);
            }
            return;
        }
        match ty.kind() {
            TyKind::Tuple(tys) if tys.as_slice().is_empty() => self.pretty_print_type(ty),
            TyKind::Adt(def, args)
                if self.should_truncate()
                    && args.consts().count() < 2
                    && args.types().count() < 2
                    && args.types().next().is_none_or(|arg| match arg.kind() {
                        TyKind::Adt(_, args) => {
                            args.consts().next().is_none() && args.types().next().is_none()
                        }
                        _ => true,
                    })
                    && Def::Adt(def.def_id())
                        .name(self.db)
                        .is_some_and(|it| it.as_str().len() < 7) =>
            {
                self.forced_trimmed(|p| p.pretty_print_type(ty))
            }
            TyKind::Adt(..)
            | TyKind::Foreign(_)
            | TyKind::Pat(..)
            | TyKind::RawPtr(..)
            | TyKind::Ref(..)
            | TyKind::FnDef(..)
            | TyKind::FnPtr(..)
            | TyKind::UnsafeBinder(_)
            | TyKind::Dynamic(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Coroutine(..)
            | TyKind::CoroutineWitness(..)
            | TyKind::Tuple(_)
            | TyKind::Alias(..)
            | TyKind::Param(_)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Error(_)
                if self.should_truncate() =>
            {
                self.write_str("_")
            }
            TyKind::Closure(..) => self.pretty_print_type(ty),
            _ => {
                self.count_type();
                self.pretty_print_type(ty)
            }
        }
    }

    /// The kinds of type `type_name`, a symbol and debug info spell apart from a diagnostic,
    /// or refuse. True for one spelled as a diagnostic spells it (with its regions, paths and
    /// arguments in this spelling).
    fn print_spelled_type(&mut self, ty: Ty<'t>) -> bool {
        let spelling = self.spelling();
        match ty.kind() {
            // `type_name` writes a type parameter of the `impl` of what a body's item is
            // written in `_`, as it does every type that is not one yet.
            TyKind::Param(_) | TyKind::Bound(..) | TyKind::Placeholder(_) | TyKind::Infer(_) | TyKind::Error(_)
                if self.params_as_holes =>
            {
                self.write_str("_")
            }
            TyKind::Param(_) => self.refuse("a type parameter, which only an instance's own types replace"),
            TyKind::Pat(..)
            | TyKind::UnsafeBinder(_)
            | TyKind::Alias(..)
            | TyKind::CoroutineWitness(..)
            | TyKind::Bound(..)
            | TyKind::Placeholder(_)
            | TyKind::Infer(_)
            | TyKind::Error(_) => self.refuse(
                "a type of a kind this does not spell (an alias, a pattern type, an unsafe binder, or a \
                 type not known yet)",
            ),
            TyKind::Foreign(_) if spelling != Spelling::TypeName => {
                self.refuse("an extern type in a symbol or debug info")
            }
            TyKind::Array(elem, len) => {
                self.write_str("[");
                self.print_type(elem);
                self.write_str("; ");
                match hir_ty::consteval::try_const_usize(self.db, len) {
                    Some(len) => self.write_fmt(format_args!("{len}]")),
                    None => self.refuse(NOT_LOOKED_UP),
                }
            }
            TyKind::Dynamic(preds, _) if spelling == Spelling::Debug => self.print_dyn_debug(preds),
            TyKind::FnDef(def, args) => {
                let args = args.skip_binder();
                match def.0 {
                    hir_def::CallableDefId::FunctionId(it) => self.print_def_path(Def::Function(it), args.as_slice()),
                    _ if spelling == Spelling::Symbol => self.refuse("a constructor in a symbol"),
                    hir_def::CallableDefId::StructId(it) => {
                        self.print_type(Ty::new_adt(self.interner, it.into(), args));
                        self.write_str("::{{constructor}}");
                    }
                    hir_def::CallableDefId::EnumVariantId(it) => {
                        let loc = it.lookup(self.db);
                        self.print_type(Ty::new_adt(self.interner, loc.parent.into(), args));
                        self.write_str("::");
                        self.write_name(&loc.name);
                        self.write_str("::{{constructor}}");
                    }
                }
            }
            TyKind::Closure(id, args) => self.print_spelled_closure(id.0.loc(self.db), args.as_closure().parent_args()),
            TyKind::CoroutineClosure(id, args) => {
                self.print_spelled_closure(id.0.loc(self.db), args.as_coroutine_closure().parent_args())
            }
            TyKind::Coroutine(id, args) => {
                let loc = id.0.loc(self.db);
                self.print_spelled_closure(loc, args.as_coroutine().parent_args());
                // rustc's `print_coroutine_with_kind`: the future of an `async` closure is named
                // after how the closure was called when that is not the kind its body was
                // inferred with. The body of a `Fn` or `FnMut` closure is inferred `FnMut`, and
                // called by value it is the by-move body, kind `FnOnce`: `{{call_once}}`. An
                // `FnOnce` closure's body is `FnOnce` however it is called, and no call makes a
                // `FnMut` body of an `FnOnce` one, so rustc's `{{call_mut}}` is never written.
                if spelling == Spelling::TypeName
                    && matches!(loc.kind, HirClosureKind::Coroutine { source: CoroutineSource::Closure, .. })
                    && hir_ty::mir::is_by_move_coroutine(self.db, id.0, args.as_coroutine())
                {
                    self.write_str("::{{call_once}}");
                }
            }
            _ => return true,
        }
        false
    }

    /// A closure, coroutine or `async` block by its path: what it is written in and the
    /// closures around it (`f::{{closure}}`, `<S>::f::{closure#0}`); in debug info, the type of
    /// what it keeps (`m::{impl#0}::f::{closure_env#0}<A>`).
    fn print_spelled_closure(&mut self, loc: InternedClosure<'t>, parent_args: &[GenericArg<'t>]) {
        let InferBodyId::DefWithBodyId(owner) = loc.owner else {
            return self.refuse(
                "a closure in an anonymous constant: rustc names it after the constant's `{constant#n}`, \
                 which is not numbered here",
            );
        };
        let Some(def) = Def::of_body(owner) else {
            return self.refuse(
                "a closure in an enum discriminant's expression: rustc names it after the discriminant's \
                 `{constant#n}`, which is not numbered here",
            );
        };
        let Some(closures) = def::closures_around(self.db, owner, loc.expr) else {
            return self.refuse(NOT_LOOKED_UP);
        };
        match (self.spelling(), def) {
            (Spelling::TypeName, _) | (Spelling::Symbol, Def::Function(_)) => {
                self.print_def_path(def, parent_args);
                self.closure_segments(closures);
            }
            (Spelling::Debug, Def::Function(func)) => {
                self.print_debug_container(func, parent_args, false);
                self.write_str("::");
                self.write_name(&def.segment(self.db).expect("a function is named"));
                let Some((&(place, kind), around)) = closures.split_last() else {
                    return self.refuse(NOT_LOOKED_UP);
                };
                self.closure_segments(around);
                self.write_fmt(format_args!("::{{{}_env#{place}}}", kind_label(kind)));
                self.path_generic_args(parent_args);
            }
            _ => self.refuse("a closure of a constant or a static in a symbol or debug info"),
        }
    }

    /// rustc's `pretty_print_type`.
    fn pretty_print_type(&mut self, ty: Ty<'t>) {
        match ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Str
            | TyKind::Never => self.write_str(scalar_name(ty).expect("a scalar is named")),
            TyKind::Pat(ty, pat) => {
                self.write_str("(");
                self.print_type(ty);
                self.write_fmt(format_args!(") is {pat:?}"));
            }
            TyKind::RawPtr(ty, m) => {
                self.write_str(match m {
                    Mutability::Mut => "*mut ",
                    Mutability::Not => "*const ",
                });
                self.print_type(ty);
            }
            TyKind::Ref(r, ty, m) => {
                self.write_str("&");
                if self.should_print_optional_region(r) {
                    self.print_region(r);
                    self.write_str(" ");
                }
                if m == Mutability::Mut {
                    self.write_str("mut ");
                }
                self.print_type(ty);
            }
            TyKind::Tuple(tys) => {
                self.write_str("(");
                self.comma_sep_types(tys.as_slice());
                if tys.as_slice().len() == 1 {
                    self.write_str(",");
                }
                self.write_str(")");
            }
            TyKind::Array(ty, len) => {
                self.write_str("[");
                self.print_type(ty);
                self.write_str("; ");
                self.print_const(len);
                self.write_str("]");
            }
            TyKind::Slice(ty) => {
                self.write_str("[");
                self.print_type(ty);
                self.write_str("]");
            }
            TyKind::FnDef(def, args) => self.print_fn_def(def, args.skip_binder()),
            TyKind::FnPtr(sig_tys, hdr) => {
                self.wrap_binder(&sig_tys.with(hdr), BinderKind::ForAll, |p, sig| {
                    p.print_fn_sig(*sig)
                })
            }
            TyKind::UnsafeBinder(inner) => {
                let binder: Binder<'t, Ty<'t>> = *inner;
                self.wrap_binder(&binder, BinderKind::Unsafe, |p, ty| p.print_type(*ty))
            }
            // hir's types as inference had them at an error hold an undecided literal's as these.
            TyKind::Infer(InferTy::FreshIntTy(_)) => self.write_str("{integer}"),
            TyKind::Infer(InferTy::FreshFloatTy(_)) => self.write_str("{float}"),
            TyKind::Infer(it) => self.write_fmt(format_args!("{it}")),
            TyKind::Error(_) => self.write_str("_"),
            TyKind::Param(it) => self.print_param(it),
            TyKind::Bound(index, bound) => {
                let mut out = String::new(); // alloc: a bound type printed, the edge
                // Writing into a `String` does not fail.
                let _ = rustc_type_ir::debug_bound_var(&mut out, index, bound.var);
                match bound.kind {
                    BoundTyKind::Anon | BoundTyKind::Param(_) => self.write_str(&out),
                }
            }
            TyKind::Placeholder(it) => self.write_fmt(format_args!("{it:?}")),
            TyKind::Adt(def, args) => self.print_def_path(Def::Adt(def.def_id()), args.as_slice()),
            TyKind::Foreign(it) => self.print_def_path(Def::TypeAlias(it.0), &[]),
            TyKind::Dynamic(preds, r) => {
                let print_r =
                    self.spelling() == Spelling::Diagnostic && self.should_print_optional_region(r);
                if print_r {
                    self.write_str("(");
                }
                self.write_str("dyn ");
                self.print_dyn(preds);
                if print_r {
                    self.write_str(" + ");
                    self.print_region(r);
                    self.write_str(")");
                }
            }
            TyKind::Alias(_, alias) => match alias.kind {
                AliasTyKind::Projection { def_id } => match def_id.0 {
                    AssocTyId::TypeAliasId(it) => {
                        self.print_def_path(Def::TypeAlias(it), alias.args.as_slice())
                    }
                    // rustc's `pretty_print_rpitit`: a trait method's `impl Trait` prints as
                    // its bounds, whose arguments are its opaque type's.
                    AssocTyId::RpititId(it) => self.print_opaque(it.opaque(self.db), alias.args),
                },
                AliasTyKind::Free { def_id } => {
                    self.print_def_path(Def::TypeAlias(def_id.0), alias.args.as_slice())
                }
                AliasTyKind::Inherent { def_id } => {
                    self.print_inherent_projection(def_id.0, alias.args)
                }
                AliasTyKind::Opaque { def_id } => self.print_opaque(def_id.0, alias.args),
            },
            TyKind::Closure(id, args) => {
                let loc = id.0.loc(self.db);
                self.write_str("{closure");
                self.print_site(loc, args.as_closure().parent_args());
                self.write_str("}");
            }
            TyKind::CoroutineClosure(id, args) => {
                let loc = id.0.loc(self.db);
                self.write_str("{");
                if let HirClosureKind::CoroutineClosure(kind) = loc.kind {
                    self.write_str(desugaring(kind, false));
                }
                self.write_str("closure");
                self.print_site(loc, args.as_coroutine_closure().parent_args());
                self.write_str("}");
            }
            TyKind::Coroutine(id, args) => {
                let loc = id.0.loc(self.db);
                let parent_args = args.as_coroutine().parent_args();
                self.write_str("{");
                if matches!(loc.kind, HirClosureKind::OldCoroutine(Movability::Static)) {
                    self.write_str("static ");
                }
                self.write_str(&coroutine_desc(loc.kind, false));
                match (loc.kind, loc.owner) {
                    (
                        HirClosureKind::Coroutine { source: CoroutineSource::Fn, .. },
                        InferBodyId::DefWithBodyId(hir_def::DefWithBodyId::FunctionId(f)),
                    ) => {
                        self.write_str(" of ");
                        self.print_def_path(Def::Function(f), parent_args);
                        self.write_str("()");
                    }
                    _ => self.print_site(loc, parent_args),
                }
                self.write_str("}");
            }
            TyKind::CoroutineWitness(id, args) => {
                let loc = id.0.loc(self.db);
                self.write_str("{coroutine witness");
                self.print_site(loc, args.as_slice());
                self.write_str("}");
            }
        }
    }

    /// `@file:l:c: l:c` of a closure of this crate (`@file:l:c` with paths forced trimmed),
    /// `@` and its path for a dependency's, `@...` past the type length limit. A dependency's
    /// closure in a body with no path (an anonymous constant: rustc's `{constant#N}`, which is
    /// not numbered here) is printed by its span, as a local one.
    fn print_site(&mut self, loc: InternedClosure<'t>, parent_args: &[GenericArg<'t>]) {
        if self.should_truncate() {
            return self.write_str("@...");
        }
        self.write_str("@");
        let db = self.db;
        let host = self.host();
        let owner = loc.owner.expression_store_owner(db);
        let local = owner.krate(db) == host.krate();
        if !local
            && let InferBodyId::DefWithBodyId(body) = loc.owner
            && let Some(path @ Parent::Body { in_anon_const: false, .. }) =
                def::closure_parent(db, body, loc.expr)
        {
            return self.print_parent(&path, parent_args);
        }
        host.closure_span(owner, loc.expr, loc.kind, self.mode.forced, &mut *self.out);
    }

    /// `[for<'a> ]fn(..) -> T {path}`, with a function's late-bound lifetimes by their names.
    fn print_fn_def(&mut self, def: CallableIdWrapper, args: GenericArgs<'t>) {
        let sig = self
            .db
            .callable_item_signature(def.0)
            .instantiate(self.interner, args)
            .skip_normalization();
        let (kind, path) = match def.0 {
            hir_def::CallableDefId::FunctionId(it) => (BinderKind::Signature, Def::Function(it)),
            hir_def::CallableDefId::StructId(it) => {
                (BinderKind::ForAll, Def::Adt(AdtId::StructId(it)))
            }
            hir_def::CallableDefId::EnumVariantId(it) => (BinderKind::ForAll, Def::Variant(it)),
        };
        self.wrap_binder(&sig, kind, |p, sig| p.print_fn_sig(*sig));
        self.write_str(" {");
        self.print_value_path(path, args.as_slice());
        self.write_str("}");
    }

    /// `unsafe extern "C" fn(A, B, ...) -> R`.
    pub fn print_fn_sig(&mut self, sig: FnSig<'t>) {
        self.write_str(sig.safety().prefix_str());
        if sig.abi() != ExternAbi::Rust {
            self.write_fmt(format_args!("extern {} ", sig.abi()));
        }
        self.write_str("fn");
        self.print_fn_sig_parts(sig.inputs(), sig.c_variadic(), sig.output());
    }

    /// rustc's `pretty_print_fn_sig`: `(A, B, ...) -> R`, no `-> ()`.
    fn print_fn_sig_parts(&mut self, inputs: &[Ty<'t>], c_variadic: bool, output: Ty<'t>) {
        self.write_str("(");
        self.comma_sep_types(inputs);
        if c_variadic {
            if !inputs.is_empty() {
                self.write_str(", ");
            }
            self.write_str("...");
        }
        self.write_str(")");
        if !output.is_unit() {
            self.write_str(" -> ");
            self.print_type(output);
        }
    }

    /// A type parameter by its name; an `impl Trait` argument's parameter by its bounds.
    fn print_param(&mut self, param: ParamTy) {
        let params = GenericParams::of(self.db, param.id.parent());
        match &params[param.id.local_id()] {
            TypeOrConstParamData::TypeParamData(data) => match &data.name {
                Some(name) => self.write_str(name.symbol().as_str()),
                None => {
                    let owner = param.id.parent();
                    let clauses =
                        GenericPredicates::query_all(self.db, owner).skip_binder().filter(
                            |clause| clause_self_ty(*clause).is_some_and(|ty| is_param(ty, param)),
                        );
                    self.print_impl_bounds(clauses);
                }
            },
            TypeOrConstParamData::ConstParamData(_) => {
                unreachable!("a type parameter's data is a type parameter's")
            }
        }
    }

    /// rustc's inherent projection: `S::Assoc<..>`.
    fn print_inherent_projection(&mut self, alias: TypeAliasId, args: GenericArgs<'t>) {
        let args = args.as_slice();
        let self_ty = args[0].ty().expect("an inherent projection's first argument is its type");
        self.print_path_with_qualified(self_ty, None);
        let name = Def::TypeAlias(alias).name(self.db).expect("an associated type is named");
        self.write_str("::");
        self.write_name(&name);
        self.path_generic_args(&args[1..]);
    }

    /// An opaque type: a type alias's by the alias when it is all the alias is,
    /// `Alias::{opaque#N}` when it is in it; a return position's by its bounds.
    fn print_opaque(&mut self, id: InternedOpaqueTyId<'t>, args: GenericArgs<'t>) {
        match id.loc(self.db) {
            ImplTraitId::TypeAliasImplTrait(alias, idx) => {
                let ty = self
                    .db
                    .ty(TyDefId::TypeAliasId(alias))
                    .instantiate_identity()
                    .skip_normalization();
                let is_alias = matches!(
                    ty.kind(),
                    TyKind::Alias(_, it) if matches!(it.kind, AliasTyKind::Opaque { def_id } if def_id.0 == id)
                );
                self.print_def_path(Def::TypeAlias(alias), args.as_slice());
                if !is_alias {
                    self.write_fmt(format_args!("::{{opaque#{}}}", idx.into_raw().into_u32()));
                }
            }
            ImplTraitId::ReturnTypeImplTrait(..) => {
                let interner = self.interner;
                let bounds = id
                    .predicates(self.db)
                    .iter_instantiated_copied(interner, args.as_slice())
                    .map(|it| it.skip_normalization());
                self.print_impl_bounds(bounds);
            }
        }
    }

    /// rustc's `pretty_print_opaque_impl_type`: `impl Fn(A) -> R + Trait<X = Y> + ?Sized + 'a`.
    fn print_impl_bounds(&mut self, clauses: impl Iterator<Item = Clause<'t>>) {
        let lang = self.interner.lang_items();
        let mut traits: Traits<'t> = Vec::new(); // alloc: an `impl Trait` printed, the edge
        let mut fn_traits: Vec<FnTrait<'t>> = Vec::new(); // alloc: an `impl Trait` printed, the edge
        let mut lifetimes: SmallVec<[Region<'t>; 1]> = SmallVec::new();
        let mut has_sized = false;
        let mut has_negative_sized = false;
        let mut has_meta_sized = false;
        for clause in clauses {
            let bound = clause.kind();
            match bound.skip_binder() {
                ClauseKind::Trait(pred) => {
                    let trait_ = pred.def_id().0;
                    if Some(trait_) == lang.Sized {
                        match pred.polarity {
                            ClausePolarity::Positive => {
                                has_sized = true;
                                continue;
                            }
                            ClausePolarity::Negative => has_negative_sized = true,
                        }
                    } else if Some(trait_) == lang.MetaSized {
                        has_meta_sized = true;
                        continue;
                    }
                    self.insert_trait(bound.rebind(pred), None, &mut traits, &mut fn_traits);
                }
                ClauseKind::Projection(pred) => {
                    let trait_pred = TraitClause {
                        trait_ref: pred.projection_term.trait_ref(self.interner),
                        polarity: ClausePolarity::Positive,
                    };
                    let item = (pred.def_id().0, bound.rebind(pred.term));
                    self.insert_trait(
                        bound.rebind(trait_pred),
                        Some(item),
                        &mut traits,
                        &mut fn_traits,
                    );
                }
                ClauseKind::TypeOutlives(outlives) => lifetimes.push(outlives.1),
                _ => {}
            }
        }

        self.write_str("impl ");
        let mut first = true;
        let paren_needed = fn_traits.len() > 1 || !traits.is_empty() || !has_sized;
        for entry in fn_traits {
            if !first {
                self.write_str(" + ");
            }
            if paren_needed {
                self.write_str("(");
            }
            let trait_ = self.fn_trait(entry.kind, entry.is_async);
            match entry.return_ty {
                Some(return_ty) => {
                    self.wrap_binder(&entry.bound, BinderKind::ForAll, |p, (args, _)| {
                        let name = Def::Trait(trait_).name(p.db).expect("a trait is named");
                        p.write_name(&name);
                        p.write_str("(");
                        p.comma_sep_types(args.as_slice());
                        p.write_str(")");
                        if let Some(ty) = return_ty.skip_binder().as_type()
                            && !ty.is_unit()
                        {
                            p.write_str(" -> ");
                            p.wrap_binder(&return_ty, BinderKind::ForAll, |p, term| {
                                p.print_term(*term)
                            });
                        }
                        if paren_needed {
                            p.write_str(")");
                        }
                    });
                    first = false;
                }
                None => {
                    let interner = self.interner;
                    let pred = entry.bound.map_bound(|(args, self_ty)| TraitClause {
                        trait_ref: TraitRef::new(
                            interner,
                            TraitIdWrapper(trait_),
                            [
                                GenericArg::from(self_ty),
                                Ty::new_tup(interner, args.as_slice()).into(),
                            ],
                        ),
                        polarity: ClausePolarity::Positive,
                    });
                    match traits.iter_mut().find(|(it, _)| *it == pred) {
                        Some((_, assoc)) => assoc.clear(),
                        None => traits.push((pred, SmallVec::new())),
                    }
                }
            }
        }

        for (pred, assoc) in traits {
            if !first {
                self.write_str(" + ");
            }
            self.wrap_binder(&pred, BinderKind::ForAll, |p, pred| {
                if pred.polarity == ClausePolarity::Negative {
                    p.write_str("!");
                }
                let trait_ = pred.trait_ref.def_id.0;
                p.print_def_path(Def::Trait(trait_), &[]);
                let args = pred.trait_ref.args.as_slice();
                let generics = p.generics_of(Def::Trait(trait_), args.len());
                let own = p.own_args_no_defaults(&generics, args);
                if !own.is_empty() || !assoc.is_empty() {
                    p.generic_delimiters(|p| {
                        let mut sep = false;
                        for arg in own {
                            if std::mem::replace(&mut sep, true) {
                                p.write_str(", ");
                            }
                            p.print_arg(*arg);
                        }
                        for (item, term) in &assoc {
                            if std::mem::replace(&mut sep, true) {
                                p.write_str(", ");
                            }
                            p.write_name(&term_name(p.db, *item));
                            p.write_str(" = ");
                            p.print_term(*term);
                        }
                    });
                }
            });
            first = false;
        }

        let add_sized = has_sized && (first || has_negative_sized);
        let add_maybe_sized = has_meta_sized && !has_negative_sized;
        if add_sized || add_maybe_sized {
            if !first {
                self.write_str(" + ");
            }
            if add_maybe_sized {
                self.write_str("?");
            }
            self.write_str("Sized");
        }

        if !self.mode.forced {
            for r in lifetimes {
                self.write_str(" + ");
                self.print_region(r);
            }
        }
    }

    /// rustc's `insert_trait_and_projection`: a `Fn*` bound with a tuple of arguments joins
    /// the sugared ones, keeping the most specific kind and its `Output`; any other gathers its
    /// associated items.
    fn insert_trait(
        &self,
        pred: Binder<'t, TraitClause<'t>>,
        item: Option<(TermId, Binder<'t, Term<'t>>)>,
        traits: &mut Traits<'t>,
        fn_traits: &mut Vec<FnTrait<'t>>,
    ) {
        let trait_ref = pred.skip_binder().trait_ref;
        let fn_kind = self.fn_trait_kind(trait_ref.def_id.0);
        if pred.skip_binder().polarity == ClausePolarity::Positive
            && let Some((kind, is_async)) = fn_kind
            && let Some(args) = trait_ref.args.as_slice().get(1).and_then(|it| it.ty())
            && let TyKind::Tuple(types) = args.kind()
        {
            let bound = pred.rebind((types, trait_ref.self_ty()));
            let index = match fn_traits
                .iter()
                .position(|it| it.bound == bound && it.is_async == is_async)
            {
                Some(it) => it,
                None => {
                    fn_traits.push(FnTrait { bound, is_async, kind, return_ty: None });
                    fn_traits.len() - 1
                }
            };
            let entry = &mut fn_traits[index];
            if kind.extends(entry.kind) {
                entry.kind = kind;
            }
            if let Some((item, term)) = item
                && term_name(self.db, item).as_str() == "Output"
            {
                entry.return_ty = Some(term);
            }
            return;
        }
        let index = match traits.iter().position(|(it, _)| *it == pred) {
            Some(it) => it,
            None => {
                traits.push((pred, SmallVec::new()));
                traits.len() - 1
            }
        };
        if let Some((item, term)) = item {
            let assoc = &mut traits[index].1;
            match assoc.iter_mut().find(|(it, _)| *it == item) {
                Some(entry) => entry.1 = term.skip_binder(),
                None => assoc.push((item, term.skip_binder())),
            }
        }
    }

    /// The `Fn*` or `AsyncFn*` kind of a trait, by the lang items of the trait's crate.
    fn fn_trait_kind(&self, trait_: hir_def::TraitId) -> Option<(ClosureKind, bool)> {
        let lang = lang_items(self.db, trait_.krate(self.db));
        let id = Some(trait_);
        Some(if id == lang.Fn {
            (ClosureKind::Fn, false)
        } else if id == lang.FnMut {
            (ClosureKind::FnMut, false)
        } else if id == lang.FnOnce {
            (ClosureKind::FnOnce, false)
        } else if id == lang.AsyncFn {
            (ClosureKind::Fn, true)
        } else if id == lang.AsyncFnMut {
            (ClosureKind::FnMut, true)
        } else if id == lang.AsyncFnOnce {
            (ClosureKind::FnOnce, true)
        } else {
            return None;
        })
    }

    /// The trait of a `Fn*` kind, which `fn_trait_kind` read it from.
    fn fn_trait(&self, kind: ClosureKind, is_async: bool) -> hir_def::TraitId {
        let lang = self.interner.lang_items();
        let trait_ = match (kind, is_async) {
            (ClosureKind::Fn, false) => lang.Fn,
            (ClosureKind::FnMut, false) => lang.FnMut,
            (ClosureKind::FnOnce, false) => lang.FnOnce,
            (ClosureKind::Fn, true) => lang.AsyncFn,
            (ClosureKind::FnMut, true) => lang.AsyncFnMut,
            (ClosureKind::FnOnce, true) => lang.AsyncFnOnce,
        };
        trait_.expect("the kind was read from this lang item")
    }

    /// rustc's `pretty_print_dyn_existential`: the principal in its binder (`Fn(A) -> R`
    /// sugared), its projections that its supertraits do not already fix, by name, then the
    /// auto traits by path, sorted by their paths as this spelling prints them.
    fn print_dyn(&mut self, preds: BoundExistentialPredicates<'t>) {
        let mut first = true;
        if let Some(principal) = preds.principal() {
            self.wrap_binder(&principal, BinderKind::ForAll, |p, principal| {
                p.print_principal(preds, principal.def_id.0, principal.args.as_slice())
            });
            first = false;
        }
        let Some(autos) = self.sorted_autos(preds) else { return };
        for (_, trait_) in autos {
            if !std::mem::replace(&mut first, false) {
                self.write_str(" + ");
            }
            self.print_def_path(Def::Trait(trait_), &[]);
        }
    }

    /// The auto traits of a `dyn` type with their paths, sorted by them: printed untrimmed in
    /// a diagnostic, in this spelling else. None when one is refused.
    fn sorted_autos(
        &mut self,
        preds: BoundExistentialPredicates<'t>,
    ) -> Option<SmallVec<[(String, hir_def::TraitId); 2]>> {
        let mut autos: SmallVec<[(String, hir_def::TraitId); 2]> = SmallVec::new();
        for it in preds.auto_traits() {
            let mut p = match self.host {
                Some(host) => Printer::diagnostic(self.db, host, Mode::UNTRIMMED, false),
                None => Printer::spelled(self.db, self.spelling()),
            };
            p.print_def_path(Def::Trait(it.0), &[]);
            match p.finish_spelled() {
                Ok(path) => autos.push((path, it.0)),
                Err(why) => {
                    self.refuse(why);
                    return None;
                }
            }
        }
        autos.sort_by(|a, b| a.0.cmp(&b.0));
        Some(autos)
    }

    fn print_principal(
        &mut self,
        preds: BoundExistentialPredicates<'t>,
        trait_: hir_def::TraitId,
        args: &[GenericArg<'t>],
    ) {
        let interner = self.interner;
        self.print_def_path(Def::Trait(trait_), &[]);

        let principal = preds.principal().expect("printing the principal");
        let mut projections: SmallVec<[ExistentialProjection<'t>; 2]> =
            // alloc: none up to 2, inline in the `SmallVec`; printing a type, the edge
            preds.projection_bounds().into_iter().map(|it| it.skip_binder()).collect();

        // A symbol has every argument of the trait and none of its associated types.
        if self.spelling() == Spelling::Symbol {
            if !projections.is_empty() {
                return self.refuse("a trait object with an associated type in a symbol");
            }
            return self.path_generic_args(args);
        }

        let is_fn = self.fn_trait_kind(trait_).is_some_and(|(_, is_async)| !is_async);
        if is_fn
            && let [projection] = projections.as_slice()
            && let Some(arg) = args.first().and_then(|it| it.ty())
            && let TyKind::Tuple(tys) = arg.kind()
        {
            let output = projection.term.as_type().expect("the `Output` of a `Fn` trait is a type");
            self.print_fn_sig_parts(tys.as_slice(), false, output);
            return;
        }

        let dummy = interner.default_types().types.dyn_trait_dummy_self;
        let mut with_self: SmallVec<[GenericArg<'t>; 4]> = SmallVec::with_capacity(args.len() + 1);
        with_self.push(dummy.into());
        with_self.extend_from_slice(args);
        let generics = self.generics_of(Def::Trait(trait_), with_self.len());
        let own = self.own_args_no_defaults(&generics, &with_self);

        // Elaborating reads lang items (`Sized`, `MetaSized`): a diagnostic's printer has its
        // crate's, a spelled one has no crate and reads those the trait's crate sees.
        let interner = match self.host {
            Some(_) => interner,
            None => DbInterner::new_with(self.db, trait_.krate(self.db)),
        };
        let supers: SmallVec<[ExistentialProjection<'t>; 4]> = {
            let clause: Clause<'t> = principal.with_self_ty(interner, dummy).upcast(interner);
            elaborate(interner, [clause])
                .filter_only_self()
                .filter_map(|it| it.as_projection_clause())
                .map(|it| {
                    erase(self, ExistentialProjection::erase_self_ty(interner, it.skip_binder()))
                })
                .collect() // alloc: none up to 4, inline in the `SmallVec`; printing a type, the edge
        };
        projections.retain(|it| !supers.contains(&erase(self, *it)));
        projections.sort_by_cached_key(|it| term_name(self.db, it.def_id.0));

        if own.is_empty() && projections.is_empty() {
            return;
        }
        self.generic_delimiters(|p| {
            let mut sep = false;
            for arg in own {
                if std::mem::replace(&mut sep, true) {
                    p.write_str(", ");
                }
                p.print_arg(*arg);
            }
            for projection in &projections {
                if std::mem::replace(&mut sep, true) {
                    p.write_str(", ");
                }
                p.print_existential_projection(*projection);
            }
        });
    }

    /// rustc's `ExistentialProjection` printing: `Item<A> = T`.
    fn print_existential_projection(&mut self, projection: ExistentialProjection<'t>) {
        let item = projection.def_id.0;
        self.write_name(&term_name(self.db, item));
        let generic: GenericDefId = match item {
            TermId::TypeAliasId(it) => it.into(),
            TermId::ConstId(it) => it.into(),
            TermId::RpititId(it) => it.function.into(),
        };
        let parent_count = hir_ty::generics::generics(self.db, generic).len_parent();
        let args = projection.args.as_slice();
        self.path_generic_args(&args[(parent_count - 1).min(args.len())..]);
        self.write_str(" = ");
        self.print_term(projection.term);
    }

    /// `dyn Trait<A, Item=B> + Auto` as a debugger is told of it, in brackets after `dyn` when
    /// there is more than the trait: every argument and the associated types in the order of
    /// the bounds.
    fn print_dyn_debug(&mut self, preds: BoundExistentialPredicates<'t>) {
        let Some(autos) = self.sorted_autos(preds) else { return };
        let principal = preds.principal();
        let bracketed = principal.is_some() && !autos.is_empty();
        self.write_str(if bracketed { "dyn (" } else { "dyn " });
        if let Some(principal) = principal {
            let principal = principal.skip_binder();
            self.print_def_path(Def::Trait(principal.def_id.0), &[]);
            let mut sep = false;
            let args = principal.args.as_slice();
            // alloc: none up to 2, inline in the `SmallVec`; printing a type, the edge
            let projections: SmallVec<[_; 2]> = preds.projection_bounds().into_iter().collect();
            if args.iter().any(|it| it.region().is_none()) || !projections.is_empty() {
                self.write_str("<");
                for arg in args.iter().filter(|it| it.region().is_none()) {
                    if std::mem::replace(&mut sep, true) {
                        self.write_str(", ");
                    }
                    self.print_arg(*arg);
                }
                for bound in projections {
                    let bound = bound.skip_binder();
                    let TermId::TypeAliasId(alias) = bound.def_id.0 else {
                        return self.refuse(
                            "a trait object bound by an associated constant in debug info",
                        );
                    };
                    if std::mem::replace(&mut sep, true) {
                        self.write_str(", ");
                    }
                    self.write_name(
                        &Def::TypeAlias(alias).name(self.db).expect("an associated type is named"),
                    );
                    self.write_str("=");
                    match bound.term.as_type() {
                        Some(ty) => self.print_type(ty),
                        None => {
                            return self.refuse(
                                "a trait object bound by an associated constant in debug info",
                            );
                        }
                    }
                }
                self.write_str(">");
            }
            if !autos.is_empty() {
                self.write_str(" + ");
            }
        }
        for (i, (path, _)) in autos.iter().enumerate() {
            if i > 0 {
                self.write_str(" + ");
            }
            self.write_str(path);
        }
        if bracketed {
            self.write_str(")");
        }
    }

    pub fn print_term(&mut self, term: Term<'t>) {
        match term.kind() {
            TermKind::Ty(ty) => self.print_type(ty),
            TermKind::Const(c) => self.print_const(c),
        }
    }
}

/// Free regions erased, as rustc's `fold_regions` to `'erased` compares projections.
fn erase<'t>(
    p: &Printer<'_, 't>,
    projection: ExistentialProjection<'t>,
) -> ExistentialProjection<'t> {
    let interner = p.interner;
    fold_regions(interner, projection, |r, _| match r.kind() {
        rustc_type_ir::RegionKind::ReBound(..) => r,
        _ => Region::new_erased(interner),
    })
}

fn term_name(db: &base_db::Db, item: TermId) -> hir_expand::name::Name {
    let def = match item {
        TermId::TypeAliasId(it) => Def::TypeAlias(it),
        TermId::ConstId(it) => Def::Const(it),
        // A trait method's `impl Trait`, which no `dyn` type binds, by its method.
        TermId::RpititId(it) => Def::Function(it.function),
    };
    def.name(db).expect("an associated item is named")
}

fn clause_self_ty(clause: Clause<'_>) -> Option<Ty<'_>> {
    match clause.kind().skip_binder() {
        ClauseKind::Trait(it) => Some(it.self_ty()),
        ClauseKind::Projection(it) => Some(it.self_ty()),
        ClauseKind::TypeOutlives(it) => Some(it.0),
        _ => None,
    }
}

fn is_param(ty: Ty<'_>, param: ParamTy) -> bool {
    matches!(ty.kind(), TyKind::Param(it) if it.id == param.id)
}

/// rustc's `CoroutineDesugaring` prefix: `async `, `` `async` `` with `{:#}`.
pub fn desugaring(kind: CoroutineKind, ticks: bool) -> &'static str {
    match (kind, ticks) {
        (CoroutineKind::Async, false) => "async ",
        (CoroutineKind::Async, true) => "`async` ",
        (CoroutineKind::Gen, false) => "gen ",
        (CoroutineKind::Gen, true) => "`gen` ",
        (CoroutineKind::AsyncGen, false) => "async gen ",
        (CoroutineKind::AsyncGen, true) => "`async gen` ",
    }
}

/// rustc's `CoroutineKind` display: `async block`, `async fn body`, `coroutine`.
pub fn coroutine_desc(kind: HirClosureKind, ticks: bool) -> String {
    match kind {
        HirClosureKind::Coroutine { kind, source } => {
            let source = match source {
                CoroutineSource::Block => "block",
                CoroutineSource::Closure => "closure body",
                CoroutineSource::Fn => "fn body",
            };
            format!("{}{source}", desugaring(kind, ticks)) // alloc: a coroutine's kind printed, the edge
        }
        _ => "coroutine".to_owned(), // alloc: a coroutine's kind printed, the edge
    }
}
