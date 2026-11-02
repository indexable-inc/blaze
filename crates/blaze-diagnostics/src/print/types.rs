//! Types as rustc prints them: `FmtPrinter::print_type` with its length limit,
//! `pretty_print_type` of every kind of type, closures and coroutines by where they are
//! written, `dyn` and `impl Trait` with their sugar, and `Ty::sort_string`/`prefix_string`
//! and `string_with_limit` that the type error messages are made of. Ported from
//! `compiler/rustc_middle/src/ty/print/pretty.rs`, `compiler/rustc_middle/src/ty/error.rs`
//! and `compiler/rustc_trait_selection/src/error_reporting/infer/mod.rs` (MIT or Apache-2.0).

use hir_ty::db::HirDatabase;
use hir_def::{
    AdtId, GenericDefId, TypeAliasId,
    hir::{ClosureKind as HirClosureKind, CoroutineKind, CoroutineSource, Movability},
    hir::generics::{GenericParams, TypeOrConstParamData},
};
use hir_ty::{
    GenericPredicates, ImplTraitId, InferBodyId, TyDefId,
    db::{InternedClosure, InternedOpaqueTyId},
    next_solver::{
        Binder, BoundExistentialPredicates, CallableIdWrapper, Clause, ClauseKind,
        ExistentialProjection, FnSig, GenericArg, GenericArgs, ParamTy, Region, RegionExt as _, TermId, TermKind,
        Term, TraitClause, TraitIdWrapper, TraitRef, Ty, TyKind, Tys,
    },
};
use rustc_abi::ExternAbi;
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, ClausePolarity, ClosureKind, InferTy, Mutability, Upcast as _,
    elaborate::elaborate,
    fold_regions,
    inherent::{
        BoundExistentialPredicates as _, Clause as _, GenericArgs as _, IntoKind as _,
        Safety as _, Term as _, Ty as _,
    },
};
use smallvec::SmallVec;
use syntax::view::AstNode as _;

use super::{
    paths::{self, Def, Parent},
    printer::{BinderKind, Mode, Printer},
};
use crate::cx::Cx;

/// Where a closure, coroutine or coroutine witness is written, as rustc names it.
enum Site {
    /// `file:line:col: line:col` of the span rustc's `def_span` gives it.
    Span(crate::model::Span),
    /// Its definition path, for one of another crate or one with no syntax.
    Path(Parent),
}

/// What rustc's `pretty_print_opaque_impl_type` gathers of a parenthesized trait.
struct FnTrait<'t> {
    bound: Binder<'t, (Tys<'t>, Ty<'t>)>,
    is_async: bool,
    kind: ClosureKind,
    return_ty: Option<Binder<'t, Term<'t>>>,
}

type Traits<'t> = Vec<(Binder<'t, TraitClause<'t>>, SmallVec<[(TermId, Term<'t>); 2]>)>;

impl<'p, 'a, 'db: 't, 't> Printer<'p, 'a, 'db, 't> {
    /// rustc's `FmtPrinter::print_type`: past the type length limit a type prints as `_`,
    /// except `()`, a closure, and a short-named ADT with at most one simple argument, which
    /// prints with paths forced trimmed.
    pub(crate) fn print_type(&mut self, ty: Ty<'t>) {
        match ty.kind() {
            TyKind::Tuple(tys) if tys.as_slice().is_empty() => self.pretty_print_type(ty),
            TyKind::Adt(def, args)
                if self.should_truncate()
                    && args.consts().count() < 2
                    && args.types().count() < 2
                    && args.types().next().is_none_or(|arg| match arg.kind() {
                        TyKind::Adt(_, args) => args.consts().next().is_none() && args.types().next().is_none(),
                        _ => true,
                    })
                    && Def::Adt(def.def_id()).name(self.cx).is_some_and(|it| it.as_str().len() < 7) =>
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

    /// rustc's `pretty_print_type`.
    fn pretty_print_type(&mut self, ty: Ty<'t>) {
        match ty.kind() {
            TyKind::Bool => self.write_str("bool"),
            TyKind::Char => self.write_str("char"),
            TyKind::Int(it) => self.write_str(hir_ty::primitive::int_ty_to_string(it)),
            TyKind::Uint(it) => self.write_str(hir_ty::primitive::uint_ty_to_string(it)),
            TyKind::Float(it) => self.write_str(hir_ty::primitive::float_ty_to_string(it)),
            TyKind::Str => self.write_str("str"),
            TyKind::Never => self.write_str("!"),
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
                self.wrap_binder(&sig_tys.with(hdr), BinderKind::ForAll, |p, sig| p.print_fn_sig(*sig))
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
                let print_r = self.should_print_optional_region(r);
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
                AliasTyKind::Projection { def_id } => {
                    self.print_def_path(Def::TypeAlias(def_id.0), alias.args.as_slice())
                }
                AliasTyKind::Free { def_id } => {
                    self.print_def_path(Def::TypeAlias(def_id.0), alias.args.as_slice())
                }
                AliasTyKind::Inherent { def_id } => self.print_inherent_projection(def_id.0, alias.args),
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
    /// `@` and its path else, `@...` past the type length limit.
    fn print_site(&mut self, loc: InternedClosure<'t>, parent_args: &[GenericArg<'t>]) {
        if self.should_truncate() {
            return self.write_str("@...");
        }
        self.write_str("@");
        match self.site(loc) {
            Site::Span(span) => {
                let file = self.cx.file_of(&span).expect("a span the context made");
                let text = self.cx.file_text(file);
                let (lo_line, lo_col) = line_col(text, span.lo);
                if self.mode.forced {
                    let short = span.file.rsplit(['/', '\\']).next().unwrap_or(&span.file);
                    self.write_fmt(format_args!("{short}:{lo_line}:{lo_col}"));
                } else {
                    let (hi_line, hi_col) = line_col(text, span.hi);
                    self.write_fmt(format_args!("{}:{lo_line}:{lo_col}: {hi_line}:{hi_col}", span.file));
                }
            }
            Site::Path(path) => self.print_parent(&path, parent_args),
        }
    }

    /// rustc's `def_span` of a closure-like definition: from the start of a closure (its
    /// `for<..>`, `move` and `async` included, its attributes not) to the end of its
    /// parameters or return type; all of an `async` or `gen` block. The coroutine of an
    /// `async` closure's body has the closure's: it has no syntax of its own, and is the body
    /// of the closure.
    fn site(&self, loc: InternedClosure<'t>) -> Site {
        let db = self.db;
        let owner = loc.owner.expression_store_owner(db);
        let (store, map) = hir_def::expr_store::ExpressionStore::with_source_map(db, owner);
        let src = map
            .expr_syntax(loc.expr)
            .or_else(|err| match loc.kind {
                HirClosureKind::Coroutine { source: CoroutineSource::Closure, .. } => store
                    .exprs()
                    .find_map(|(id, expr)| match expr {
                        hir_def::hir::Expr::Closure { body, .. } if body == loc.expr => Some(id),
                        _ => None,
                    })
                    .map_or(Err(err), |closure| map.expr_syntax(closure)),
                _ => Err(err),
            })
            .expect("a closure-like expression not desugared from a function has syntax");
        let node = self.cx.node_view(src);
        let syntax = node.value.syntax();
        let local = hir::Crate::from(hir_def::HasModule::krate(&owner, db)) == self.cx.krate;
        if !local {
            let path = paths::closure_path(self.cx, syntax).expect("a closure is written in a body");
            return Site::Path(path);
        }
        Site::Span(self.cx.span(node.with_value(crate::cx::closure_decl_range_view(syntax))))
    }

    /// `[for<'a> ]fn(..) -> T {path}`, with a function's late-bound lifetimes by their names.
    fn print_fn_def(&mut self, def: CallableIdWrapper, args: GenericArgs<'t>) {
        let sig = self.db.callable_item_signature(def.0).instantiate(self.interner, args).skip_normalization();
        let (kind, path) = match def.0 {
            hir_def::CallableDefId::FunctionId(it) => (BinderKind::Signature, Def::Function(it)),
            hir_def::CallableDefId::StructId(it) => (BinderKind::ForAll, Def::Adt(AdtId::StructId(it))),
            hir_def::CallableDefId::EnumVariantId(it) => (BinderKind::ForAll, Def::Variant(it)),
        };
        self.wrap_binder(&sig, kind, |p, sig| p.print_fn_sig(*sig));
        self.write_str(" {");
        self.print_value_path(path, args.as_slice());
        self.write_str("}");
    }

    /// `unsafe extern "C" fn(A, B, ...) -> R`.
    fn print_fn_sig(&mut self, sig: FnSig<'t>) {
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
                    let clauses = GenericPredicates::query_all(self.db, owner)
                        .skip_binder()
                        .filter(|clause| clause_self_ty(*clause).is_some_and(|ty| is_param(ty, param)));
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
        let name = Def::TypeAlias(alias).name(self.cx).expect("an associated type is named");
        self.write_str("::");
        self.write_name(&name);
        self.path_generic_args(&args[1..]);
    }

    /// An opaque type: a type alias's by the alias when it is all the alias is,
    /// `Alias::{opaque#N}` when it is in it; a return position's by its bounds.
    fn print_opaque(&mut self, id: InternedOpaqueTyId<'t>, args: GenericArgs<'t>) {
        match id.loc(self.db) {
            ImplTraitId::TypeAliasImplTrait(alias, idx) => {
                let ty = self.db.ty(TyDefId::TypeAliasId(alias)).instantiate_identity().skip_normalization();
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
                    self.insert_trait(bound.rebind(trait_pred), Some(item), &mut traits, &mut fn_traits);
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
                        let name = Def::Trait(trait_).name(p.cx).expect("a trait is named");
                        p.write_name(&name);
                        p.write_str("(");
                        p.comma_sep_types(args.as_slice());
                        p.write_str(")");
                        if let Some(ty) = return_ty.skip_binder().as_type()
                            && !ty.is_unit()
                        {
                            p.write_str(" -> ");
                            p.wrap_binder(&return_ty, BinderKind::ForAll, |p, term| p.print_term(*term));
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
                            [GenericArg::from(self_ty), Ty::new_tup(interner, args.as_slice()).into()],
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
                            p.write_name(&term_name(p.cx, *item));
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
            let index = match fn_traits.iter().position(|it| it.bound == bound && it.is_async == is_async) {
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
                && term_name(self.cx, item).as_str() == "Output"
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

    /// The `Fn*` or `AsyncFn*` kind of a trait.
    fn fn_trait_kind(&self, trait_: hir_def::TraitId) -> Option<(ClosureKind, bool)> {
        let lang = self.interner.lang_items();
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
    /// auto traits by path.
    fn print_dyn(&mut self, preds: BoundExistentialPredicates<'t>) {
        let mut first = true;
        if let Some(principal) = preds.principal() {
            self.wrap_binder(&principal, BinderKind::ForAll, |p, principal| {
                p.print_principal(preds, principal.def_id.0, principal.args.as_slice())
            });
            first = false;
        }
        let mut auto: SmallVec<[(String, hir_def::TraitId); 2]> = preds
            .auto_traits()
            .into_iter()
            .map(|it| {
                let mut p = Printer::new(self.cx, Mode::UNTRIMMED, false);
                p.print_def_path(Def::Trait(it.0), &[]);
                (p.finish(), it.0)
            })
            .collect(); // alloc: the auto traits' paths, sorted by them; printing a type, the edge
        auto.sort_by(|a, b| a.0.cmp(&b.0));
        for (_, trait_) in auto {
            if !std::mem::replace(&mut first, false) {
                self.write_str(" + ");
            }
            self.print_def_path(Def::Trait(trait_), &[]);
        }
    }

    fn print_principal(&mut self, preds: BoundExistentialPredicates<'t>, trait_: hir_def::TraitId, args: &[GenericArg<'t>]) {
        let interner = self.interner;
        let dummy = interner.default_types().types.dyn_trait_dummy_self;
        let mut with_self: SmallVec<[GenericArg<'t>; 4]> = SmallVec::with_capacity(args.len() + 1);
        with_self.push(dummy.into());
        with_self.extend_from_slice(args);
        self.print_def_path(Def::Trait(trait_), &[]);

        let principal = preds.principal().expect("printing the principal");
        let mut projections: SmallVec<[ExistentialProjection<'t>; 2]> =
            // alloc: none up to 2, inline in the `SmallVec`; printing a type, the edge
            preds.projection_bounds().into_iter().map(|it| it.skip_binder()).collect();

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

        let generics = self.generics_of(Def::Trait(trait_), with_self.len());
        let own = self.own_args_no_defaults(&generics, &with_self);

        let supers: SmallVec<[ExistentialProjection<'t>; 4]> = {
            let clause: Clause<'t> = principal.with_self_ty(interner, dummy).upcast(interner);
            elaborate(interner, [clause])
                .filter_only_self()
                .filter_map(|it| it.as_projection_clause())
                .map(|it| erase(self, ExistentialProjection::erase_self_ty(interner, it.skip_binder())))
                .collect() // alloc: none up to 4, inline in the `SmallVec`; printing a type, the edge
        };
        projections.retain(|it| !supers.contains(&erase(self, *it)));
        projections.sort_by_cached_key(|it| term_name(self.cx, it.def_id.0));

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
        self.write_name(&term_name(self.cx, item));
        let generic: GenericDefId = match item {
            TermId::TypeAliasId(it) => it.into(),
            TermId::ConstId(it) => it.into(),
        };
        let parent_count = hir_ty::generics::generics(self.db, generic).len_parent();
        let args = projection.args.as_slice();
        self.path_generic_args(&args[(parent_count - 1).min(args.len())..]);
        self.write_str(" = ");
        self.print_term(projection.term);
    }

    pub(crate) fn print_term(&mut self, term: Term<'t>) {
        match term.kind() {
            TermKind::Ty(ty) => self.print_type(ty),
            TermKind::Const(c) => self.print_const(c),
        }
    }
}

/// Free regions erased, as rustc's `fold_regions` to `'erased` compares projections.
fn erase<'t>(p: &Printer<'_, '_, '_, 't>, projection: ExistentialProjection<'t>) -> ExistentialProjection<'t> {
    let interner = p.interner;
    fold_regions(interner, projection, |r, _| match r.kind() {
        rustc_type_ir::RegionKind::ReBound(..) => r,
        _ => Region::new_erased(interner),
    })
}

fn term_name(cx: &Cx<'_, '_>, item: TermId) -> hir_expand::name::Name {
    let def = match item {
        TermId::TypeAliasId(it) => Def::TypeAlias(it),
        TermId::ConstId(it) => Def::Const(it),
    };
    def.name(cx).expect("an associated item is named")
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

/// rustc's 1-based line and 1-based column in chars of a byte offset.
fn line_col(text: &str, offset: u32) -> (usize, usize) {
    let offset = (offset as usize).min(text.len());
    let before = &text[..offset];
    let line_start = memchr::memrchr(b'\n', before.as_bytes()).map_or(0, |it| it + 1);
    let line = memchr::memchr_iter(b'\n', before.as_bytes()).count() + 1;
    (line, before[line_start..].chars().count() + 1)
}

/// rustc's `CoroutineDesugaring` prefix: `async `, `` `async` `` with `{:#}`.
fn desugaring(kind: CoroutineKind, ticks: bool) -> &'static str {
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
fn coroutine_desc(kind: HirClosureKind, ticks: bool) -> String {
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

/// `ty` printed by its own printer in `mode`.
pub(super) fn string(cx: &Cx<'_, '_>, ty: Ty<'_>, mode: Mode) -> String {
    let mut p = Printer::new(cx, mode, false);
    p.print_type(ty);
    p.finish()
}

/// `def`'s path, as `def_path_str` prints it, in `mode`.
fn path_string(cx: &Cx<'_, '_>, def: Def, mode: Mode) -> String {
    let mut p = Printer::new(cx, mode, def.is_value());
    p.print_def_path(def, &[]);
    p.finish()
}

/// rustc's `string_with_limit`: the type if it is at most `limit` bytes long, else forced
/// trimmed with ever fewer types until it is.
pub(super) fn string_with_limit(cx: &Cx<'_, '_>, ty: Ty<'_>, limit: usize, mode: Mode) -> String {
    let regular = string(cx, ty, mode);
    if regular.len() <= limit {
        return regular;
    }
    let mut type_limit = 50;
    loop {
        let short = string(cx, ty, mode.forced().limited(type_limit));
        if short.len() <= limit || type_limit == 0 {
            return short;
        }
        type_limit -= 1;
    }
}

/// rustc's `Ty::sort_string`: what kind of type `ty` is, or the type in backticks, at most
/// `max(diagnostic_width / 4, 40)` bytes long with rustc's default width.
pub(super) fn sort_string(cx: &Cx<'_, '_>, ty: Ty<'_>, mode: Mode) -> String {
    let db = cx.db;
    let s = match ty.kind() {
        TyKind::Foreign(it) => {
            // alloc: a type's sort printed, the edge
            return format!("extern type `{}`", path_string(cx, Def::TypeAlias(it.0), mode));
        }
        TyKind::FnDef(def, _) => fn_def_kind(def),
        TyKind::FnPtr(..) => "fn pointer",
        TyKind::Dynamic(preds, _) if let Some(principal) = preds.principal() => {
            // alloc: a type's sort printed, the edge
            return format!("`dyn {}`", path_string(cx, Def::Trait(principal.skip_binder().def_id.0), mode));
        }
        TyKind::Dynamic(..) => "trait object",
        TyKind::Closure(..) => "closure",
        TyKind::Coroutine(id, _) => return coroutine_desc(id.0.loc(db).kind, true),
        TyKind::CoroutineWitness(..) => "coroutine witness",
        TyKind::Infer(InferTy::TyVar(_)) => "inferred type",
        TyKind::Infer(InferTy::IntVar(_)) => "integer",
        TyKind::Infer(InferTy::FloatVar(_)) => "floating-point number",
        TyKind::Placeholder(..) => "placeholder type",
        TyKind::Bound(..) => "bound type",
        TyKind::Infer(InferTy::FreshTy(_)) => "fresh type",
        // hir's types as inference had them at an error hold an undecided literal's as these.
        TyKind::Infer(InferTy::FreshIntTy(_)) => "integer",
        TyKind::Infer(InferTy::FreshFloatTy(_)) => "floating-point number",
        TyKind::Alias(_, alias) if matches!(alias.kind, AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }) => {
            "associated type"
        }
        // alloc: a type's sort printed, the edge
        TyKind::Param(_) => return format!("type parameter `{}`", string(cx, ty, mode)),
        TyKind::Alias(_, alias) if let AliasTyKind::Opaque { def_id } = alias.kind => {
            if is_future(cx, def_id.0) { "future" } else { "opaque type" }
        }
        TyKind::Error(_) => "type error",
        // alloc: a type's sort printed, the edge
        _ => return format!("`{}`", string_with_limit(cx, ty, 40, mode)),
    };
    s.to_owned() // alloc: a type's sort printed, the edge
}

fn fn_def_kind(def: CallableIdWrapper) -> &'static str {
    match def.0 {
        hir_def::CallableDefId::StructId(_) => "struct constructor",
        hir_def::CallableDefId::EnumVariantId(_) => "enum constructor",
        hir_def::CallableDefId::FunctionId(_) => "fn item",
    }
}

/// rustc's `Ty::prefix_string`: the kind of type `ty` is, for `expected X, found Y`.
pub(super) fn prefix_string(cx: &Cx<'_, '_>, ty: Ty<'_>) -> String {
    let db = cx.db;
    let s = match ty.kind() {
        TyKind::Infer(_)
        | TyKind::Error(_)
        | TyKind::Bool
        | TyKind::Char
        | TyKind::Int(_)
        | TyKind::Uint(_)
        | TyKind::Float(_)
        | TyKind::Str
        | TyKind::Never => "type",
        TyKind::Tuple(tys) if tys.as_slice().is_empty() => "unit type",
        TyKind::Adt(def, _) => match def.def_id() {
            AdtId::StructId(_) => "struct",
            AdtId::UnionId(_) => "union",
            AdtId::EnumId(_) => "enum",
        },
        TyKind::Foreign(_) => "extern type",
        TyKind::Array(..) => "array",
        TyKind::Pat(..) => "pattern type",
        TyKind::Slice(_) => "slice",
        TyKind::RawPtr(..) => "raw pointer",
        TyKind::Ref(_, _, Mutability::Mut) => "mutable reference",
        TyKind::Ref(..) => "reference",
        TyKind::FnDef(def, _) => fn_def_kind(def),
        TyKind::FnPtr(..) => "fn pointer",
        TyKind::UnsafeBinder(_) => "unsafe binder",
        TyKind::Dynamic(..) => "trait object",
        TyKind::Closure(..) | TyKind::CoroutineClosure(..) => "closure",
        TyKind::Coroutine(id, _) => return coroutine_desc(id.0.loc(db).kind, true),
        TyKind::CoroutineWitness(..) => "coroutine witness",
        TyKind::Tuple(_) => "tuple",
        TyKind::Placeholder(_) => "higher-ranked type",
        TyKind::Bound(..) => "bound type variable",
        TyKind::Alias(_, alias) => match alias.kind {
            AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. } => "associated type",
            AliasTyKind::Free { .. } => "type alias",
            AliasTyKind::Opaque { .. } => "opaque type",
        },
        TyKind::Param(_) => "type parameter",
    };
    s.to_owned() // alloc: a type's kind printed, the edge
}

/// Whether an opaque type is bounded by `Future`: rustc's `is_impl_trait_future`.
fn is_future(cx: &Cx<'_, '_>, id: InternedOpaqueTyId<'_>) -> bool {
    let db = cx.db;
    let Some(future) = hir_def::lang_item::lang_items(db, cx.krate.into()).Future else { return false };
    id.self_predicates(db).skip_binder().iter().any(|clause| {
        matches!(clause.kind().skip_binder(), ClauseKind::Trait(it) if it.def_id().0 == future)
    })
}

#[cfg(test)]
mod tests {
    use super::line_col;

    #[test]
    fn columns_count_chars_from_one() {
        let text = "fn f() {\n    let é = |x| x;\n}";
        let at = text.find('|').unwrap() as u32;
        assert_eq!(line_col(text, at), (2, 13));
        assert_eq!(line_col(text, 0), (1, 1));
    }
}
