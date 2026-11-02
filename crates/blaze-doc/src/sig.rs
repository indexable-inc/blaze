//! Signatures and declarations as code whose names link: written from the item's signature in
//! the HIR (the types as the source writes them, every path resolved where it is written), in
//! rustdoc's layout: a path shows its last segment, bounds on a parameter stay by it, other
//! predicates go to a `where` clause, a long parameter list breaks one parameter to a line.

use base_db::Db;
use hir::{HasVisibility, ModuleDef};
use hir_def::{
    AdtId, ExpressionStoreOwnerId, FunctionId, GenericDefId, ImplId, ItemContainerId, TraitId,
    TypeAliasId, VariantId,
    expr_store::{
        ExpressionStore,
        path::{GenericArg, GenericArgs, GenericArgsParentheses, Path},
    },
    hir::{
        generics::{
            GenericParams, LocalTypeOrConstParamId, TypeOrConstParamData, TypeParamProvenance,
            WherePredicate,
        },
        type_ref::{
            ConstRef, LifetimeRef, LifetimeRefId, Mutability, TraitBoundModifier, TypeBound,
            TypeRef, TypeRefId, UseArgRef,
        },
    },
    item_tree::FieldsShape,
    lang_item::LangItemTarget,
    layout::ExternAbi,
    resolver::{HasResolver, Resolver, TypeNs},
    signatures::{
        ConstSignature, EnumSignature, FunctionSignature, ImplSignature, StaticFlags,
        StaticSignature, StructSignature, TraitFlags, TraitSignature, TypeAliasSignature,
        UnionSignature, VariantFields,
    },
    visibility::Visibility,
};
use syntax::view::{self as ast, AstNode, HasName};

use crate::{
    model::{Code, Kind, Piece},
    places::{Linking, kind_of},
};

/// rustdoc breaks a signature longer than this onto lines.
const WIDTH: usize = 100;

pub(crate) struct Sig<'a> {
    pub(crate) db: &'a Db,
    pub(crate) places: Linking<'a>,
    store: &'a ExpressionStore,
    resolver: Resolver<'a>,
    owner: ExpressionStoreOwnerId,
}

impl<'a> Sig<'a> {
    pub(crate) fn new(
        db: &'a Db,
        places: Linking<'a>,
        store: &'a ExpressionStore,
        resolver: Resolver<'a>,
        owner: ExpressionStoreOwnerId,
    ) -> Self {
        Sig { db, places, store, resolver, owner }
    }

    pub(crate) fn ty(&self, out: &mut Code, ty: TypeRefId) {
        let db = self.db;
        match self.store.type_ref(ty) {
            TypeRef::Never => {
                out.text("!");
            }
            TypeRef::Placeholder | TypeRef::Error => {
                out.text("_");
            }
            TypeRef::Tuple(items) => {
                out.text("(");
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        out.text(", ");
                    }
                    self.ty(out, *item);
                }
                if items.len() == 1 {
                    out.text(",");
                }
                out.text(")");
            }
            TypeRef::Path(path) => self.path(out, path),
            TypeRef::RawPtr(inner, mutability) => {
                out.text(match mutability {
                    Mutability::Shared => "*const ",
                    Mutability::Mut => "*mut ",
                });
                self.ty(out, inner);
            }
            TypeRef::Reference(reference) => {
                out.text("&");
                if let Some(lifetime) = reference.lifetime {
                    self.lifetime(out, lifetime);
                    out.text(" ");
                }
                if reference.mutability == Mutability::Mut {
                    out.kw("mut").text(" ");
                }
                self.ty(out, reference.ty);
            }
            TypeRef::Array(array) => {
                out.text("[");
                self.ty(out, array.ty);
                out.text("; ");
                self.const_ref(out, &array.len);
                out.text("]");
            }
            TypeRef::Slice(inner) => {
                out.text("[");
                self.ty(out, inner);
                out.text("]");
            }
            TypeRef::Fn(fn_) => {
                if let Some(binder) = &fn_.binder {
                    out.kw("for").text("<");
                    for (at, name) in binder.iter().enumerate() {
                        if at > 0 {
                            out.text(", ");
                        }
                        out.push(Piece::Lifetime(lifetime_text(name.as_str())));
                    }
                    out.text("> ");
                }
                if fn_.is_unsafe {
                    out.kw("unsafe").text(" ");
                }
                if fn_.abi != ExternAbi::Rust {
                    out.kw("extern").text(format!(" \"{}\" ", fn_.abi.as_str()));
                }
                out.kw("fn").text("(");
                if let Some(((_, ret), params)) = fn_.params.split_last() {
                    for (at, (name, ty)) in params.iter().enumerate() {
                        if at > 0 {
                            out.text(", ");
                        }
                        if let Some(name) = name {
                            out.text(format!("{}: ", name.as_str()));
                        }
                        self.ty(out, *ty);
                    }
                    if fn_.is_varargs {
                        out.text(if params.is_empty() { "..." } else { ", ..." });
                    }
                    out.text(")");
                    if !matches!(self.store.type_ref(*ret), TypeRef::Tuple(it) if it.is_empty()) {
                        out.text(" -> ");
                        self.ty(out, *ret);
                    }
                } else {
                    out.text(")");
                }
            }
            TypeRef::UnsafeBinder { lifetimes, ty } => {
                // rustdoc's `print_higher_ranked_params_with_space`: nothing when no lifetime is bound.
                let names = self.store.names(lifetimes);
                if !names.is_empty() {
                    out.kw("unsafe").text("<");
                    for (at, name) in names.iter().enumerate() {
                        if at > 0 {
                            out.text(", ");
                        }
                        out.push(Piece::Lifetime(lifetime_text(name.as_str())));
                    }
                    out.text("> ");
                }
                self.ty(out, ty);
            }
            TypeRef::ImplTrait(bounds) => {
                out.kw("impl").text(" ");
                self.bounds(out, bounds);
            }
            TypeRef::DynTrait(bounds) => {
                out.kw("dyn").text(" ");
                self.bounds(out, bounds);
            }
            TypeRef::TypeParam(param) => {
                if let Some((_, code)) = self.places.subst.iter().find(|(it, _)| *it == param) {
                    out.extend(code.clone());
                    return;
                }
                let params = GenericParams::of(db, param.parent());
                match params[param.local_id()].name() {
                    Some(name) if params[param.local_id()].is_trait_self() => {
                        let _ = name;
                        out.push(Piece::Plain("Self".to_owned()));
                    }
                    Some(name) => {
                        out.push(Piece::Plain(name.as_str().to_owned()));
                    }
                    None => {
                        // `impl Trait` in argument position: its bounds are predicates on it.
                        let (params, store) = GenericParams::with_store(db, param.parent());
                        let sig = Sig { store, ..self.reborrow() };
                        out.kw("impl").text(" ");
                        let bounds: Vec<&TypeBound> = params
                            .where_predicates()
                            .iter()
                            .filter_map(|pred| match pred {
                                WherePredicate::TypeBound { target, bound, .. }
                                    if matches!(store.type_ref(*target), TypeRef::TypeParam(id) if id == param) =>
                                {
                                    Some(bound)
                                }
                                _ => None,
                            })
                            .collect();
                        for (at, bound) in bounds.iter().enumerate() {
                            if at > 0 {
                                out.text(" + ");
                            }
                            sig.bound(out, bound);
                        }
                    }
                }
            }
            TypeRef::PatternType(inner, _) => self.ty(out, inner),
        }
    }

    fn reborrow(&self) -> Sig<'a> {
        Sig {
            db: self.db,
            places: self.places,
            store: self.store,
            resolver: self.resolver.clone(),
            owner: self.owner,
        }
    }

    pub(crate) fn bounds(&self, out: &mut Code, bounds: &[TypeBound]) {
        let mut first = true;
        for bound in bounds {
            if matches!(bound, TypeBound::Error) {
                continue;
            }
            if !first {
                out.text(" + ");
            }
            first = false;
            self.bound(out, bound);
        }
    }

    pub(crate) fn bound(&self, out: &mut Code, bound: &TypeBound) {
        match bound {
            TypeBound::Path(path, modifier) => {
                match modifier {
                    TraitBoundModifier::None => {}
                    TraitBoundModifier::Maybe => {
                        out.text("?");
                    }
                    TraitBoundModifier::Const => {
                        out.kw("const").text(" ");
                    }
                    TraitBoundModifier::MaybeConst => {
                        out.text("[").kw("const").text("] ");
                    }
                }
                self.path(out, self.store.path(*path));
            }
            TypeBound::ForLifetime(names, path) => {
                out.kw("for").text("<");
                for (at, name) in self.store.names(*names).iter().enumerate() {
                    if at > 0 {
                        out.text(", ");
                    }
                    out.push(Piece::Lifetime(lifetime_text(name.as_str())));
                }
                out.text("> ");
                self.path(out, self.store.path(*path));
            }
            TypeBound::Lifetime(lifetime) => self.lifetime(out, *lifetime),
            TypeBound::Use(args) => {
                out.kw("use").text("<");
                for (at, arg) in self.store.use_args(*args).iter().enumerate() {
                    if at > 0 {
                        out.text(", ");
                    }
                    match arg {
                        UseArgRef::Name(name) => {
                            out.push(Piece::Plain(name.as_str().to_owned()));
                        }
                        UseArgRef::Lifetime(lifetime) => self.lifetime(out, *lifetime),
                    }
                }
                out.text(">");
            }
            TypeBound::Error => {}
        }
    }

    pub(crate) fn lifetime(&self, out: &mut Code, lifetime: LifetimeRefId) {
        let text = match &self.store[lifetime] {
            LifetimeRef::Named(name) => lifetime_text(name.as_str()),
            LifetimeRef::Static => "'static".to_owned(),
            LifetimeRef::Placeholder | LifetimeRef::Error => "'_".to_owned(),
            LifetimeRef::Param(param) => {
                let params = GenericParams::of(self.db, param.parent);
                lifetime_text(params[param.local_id].name.as_str())
            }
        };
        out.push(Piece::Lifetime(text));
    }

    pub(crate) fn const_ref(&self, out: &mut Code, value: &ConstRef) {
        let text = hir_def::expr_store::pretty::print_expr_hir(
            self.db,
            self.store,
            self.owner,
            value.expr,
            span::Edition::CURRENT,
        );
        out.text(text);
    }

    pub(crate) fn path(&self, out: &mut Code, path: Path<'_>) {
        let db = self.db;
        if let Path::LangItem(target, name) = path {
            let def: Option<ModuleDef> = match target {
                LangItemTarget::TraitId(it) => Some(ModuleDef::Trait(it.into())),
                LangItemTarget::StructId(it) => Some(ModuleDef::Adt(hir::Adt::Struct(it.into()))),
                LangItemTarget::EnumId(it) => Some(ModuleDef::Adt(hir::Adt::Enum(it.into()))),
                LangItemTarget::UnionId(it) => Some(ModuleDef::Adt(hir::Adt::Union(it.into()))),
                LangItemTarget::TypeAliasId(it) => Some(ModuleDef::TypeAlias(it.into())),
                LangItemTarget::EnumVariantId(it) => Some(ModuleDef::EnumVariant(it.into())),
                _ => None,
            };
            if let Some(def) = def {
                let text = def.name(db).map(|it| it.as_str().to_owned()).unwrap_or_default();
                self.link(out, text, def);
            }
            if let Some(name) = name {
                out.text("::").push(Piece::Plain(name.as_str().to_owned()));
            }
            return;
        }
        if path.is_self_type() {
            out.push(Piece::Plain("Self".to_owned()));
            return;
        }
        let segments = path.segments();
        let Some(last) = segments.last() else { return };
        match self.resolver.resolve_path_in_type_ns(db, path) {
            Some((ns, None, _)) => {
                self.segment(out, ns, last.name.as_str(), last.args_and_bindings);
            }
            Some((ns, Some(unresolved), _)) if unresolved > 0 => {
                let base = segments.get(unresolved - 1).unwrap();
                let base_trait = matches!(ns, TypeNs::TraitId(_)).then_some(ns);
                match base.args_and_bindings {
                    Some(args) if args.has_self_type => {
                        // `<T as Trait>::Assoc`: the self type is the trait's first argument.
                        out.text("<");
                        if let Some(GenericArg::Type(self_ty)) = args.args.first() {
                            self.ty(out, *self_ty);
                        }
                        out.text(" ").kw("as").text(" ");
                        self.segment(out, ns, base.name.as_str(), Some(args));
                        out.text(">");
                    }
                    args => self.segment(out, ns, base.name.as_str(), args),
                }
                for segment in segments.skip(unresolved).iter() {
                    out.text("::");
                    let assoc = base_trait.and_then(|ns| match ns {
                        TypeNs::TraitId(trait_) => {
                            let trait_: hir::Trait = trait_.into();
                            trait_.items(db).into_iter().find_map(|item| match item {
                                hir::AssocItem::TypeAlias(alias)
                                    if alias.name(db) == *segment.name =>
                                {
                                    Some(alias)
                                }
                                _ => None,
                            })
                        }
                        _ => None,
                    });
                    match assoc.and_then(|it| self.places.target(db, ModuleDef::TypeAlias(it))) {
                        Some(to) => {
                            out.push(Piece::Link {
                                text: segment.name.as_str().to_owned(),
                                kind: Kind::AssocType,
                                to,
                            });
                        }
                        None => {
                            out.push(Piece::Plain(segment.name.as_str().to_owned()));
                        }
                    }
                    if let Some(args) = segment.args_and_bindings {
                        self.generic_args(out, args);
                    }
                }
            }
            _ => {
                for (at, segment) in segments.iter().enumerate() {
                    if at > 0 {
                        out.text("::");
                    }
                    out.push(Piece::Plain(segment.name.as_str().to_owned()));
                    if let Some(args) = segment.args_and_bindings {
                        self.generic_args(out, args);
                    }
                }
            }
        }
    }

    fn segment(&self, out: &mut Code, ns: TypeNs, name: &str, args: Option<GenericArgs<'_>>) {
        let db = self.db;
        let def: Option<ModuleDef> = match ns {
            TypeNs::AdtId(it) => Some(ModuleDef::Adt(it.into())),
            TypeNs::TypeAliasId(it) => Some(ModuleDef::TypeAlias(it.into())),
            TypeNs::BuiltinType(it) => Some(ModuleDef::BuiltinType(it.into())),
            TypeNs::TraitId(it) => Some(ModuleDef::Trait(it.into())),
            TypeNs::EnumVariantId(it) => Some(ModuleDef::EnumVariant(it.into())),
            TypeNs::ModuleId(it) => Some(ModuleDef::Module(it.into())),
            TypeNs::SelfType(_) | TypeNs::AdtSelfType(_) => {
                out.push(Piece::Plain("Self".to_owned()));
                None
            }
            TypeNs::GenericParam(param) => {
                if let Some((_, code)) = self.places.subst.iter().find(|(it, _)| *it == param) {
                    out.extend(code.clone());
                    return;
                }
                let params = GenericParams::of(db, param.parent());
                let data = &params[param.local_id()];
                let text = if data.is_trait_self() {
                    "Self".to_owned()
                } else {
                    data.name().map_or_else(|| name.to_owned(), |it| it.as_str().to_owned())
                };
                out.push(Piece::Plain(text));
                None
            }
        };
        if let Some(def) = def {
            let text = match def {
                ModuleDef::BuiltinType(_) => name.to_owned(),
                _ => def.name(db).map_or_else(|| name.to_owned(), |it| it.as_str().to_owned()),
            };
            self.link(out, text, def);
        }
        if let Some(args) = args {
            self.generic_args(out, args);
        }
    }

    fn link(&self, out: &mut Code, text: String, def: ModuleDef) {
        let db = self.db;
        let kind = match def {
            ModuleDef::EnumVariant(_) => Some(Kind::Variant),
            _ => kind_of(db, def),
        };
        match (kind, self.places.target(db, def)) {
            (Some(kind), Some(to)) => {
                out.push(Piece::Link { text, kind, to });
            }
            _ => {
                out.push(Piece::Plain(text));
            }
        }
    }

    pub(crate) fn generic_args(&self, out: &mut Code, args: GenericArgs<'_>) {
        match args.parenthesized {
            GenericArgsParentheses::ParenSugar => {
                out.text("(");
                if let Some(GenericArg::Type(tuple)) = args.args.first() {
                    match self.store.type_ref(*tuple) {
                        TypeRef::Tuple(items) => {
                            for (at, item) in items.iter().enumerate() {
                                if at > 0 {
                                    out.text(", ");
                                }
                                self.ty(out, *item);
                            }
                        }
                        _ => self.ty(out, *tuple),
                    }
                }
                out.text(")");
                if let Some(ret) = args.bindings.first().and_then(|it| it.type_ref)
                    && !matches!(self.store.type_ref(ret), TypeRef::Tuple(it) if it.is_empty())
                {
                    out.text(" -> ");
                    self.ty(out, ret);
                }
                return;
            }
            GenericArgsParentheses::ReturnTypeNotation => {
                out.text("(..)");
                return;
            }
            GenericArgsParentheses::No => {}
        }
        let skip = usize::from(args.has_self_type);
        let shown: Vec<&GenericArg> = args.args.iter().skip(skip).collect();
        if shown.is_empty() && args.bindings.is_empty() {
            return;
        }
        out.text("<");
        let mut first = true;
        for arg in shown {
            if !first {
                out.text(", ");
            }
            first = false;
            match arg {
                GenericArg::Type(ty) => self.ty(out, *ty),
                GenericArg::Lifetime(lifetime) => self.lifetime(out, *lifetime),
                GenericArg::Const(value) => self.const_ref(out, value),
            }
        }
        for binding in &args.bindings {
            if !first {
                out.text(", ");
            }
            first = false;
            out.push(Piece::Plain(binding.name.as_str().to_owned()));
            if let Some(args) = binding.args {
                self.generic_args(out, args);
            }
            if let Some(ty) = binding.type_ref {
                out.text(" = ");
                self.ty(out, ty);
            } else if !binding.bounds.is_empty() {
                out.text(": ");
                self.bounds(out, &binding.bounds);
            }
        }
        out.text(">");
    }

    /// `<'a, T: Bound, const N: usize>`: the parameters, each with the bounds whose target is
    /// just the parameter.
    pub(crate) fn generics(&self, out: &mut Code, params: &GenericParams) {
        let mut items: Vec<Code> = vec![];
        for (id, lifetime) in params.iter_lt() {
            let mut item = Code::default();
            item.push(Piece::Lifetime(lifetime_text(lifetime.name.as_str())));
            let bounds: Vec<LifetimeRefId> = params
                .where_predicates()
                .iter()
                .filter_map(|pred| match pred {
                    WherePredicate::Lifetime { target, bound }
                        if matches!(self.store[*target], LifetimeRef::Param(p) if p.local_id == id) =>
                    {
                        Some(*bound)
                    }
                    _ => None,
                })
                .collect();
            for (at, bound) in bounds.into_iter().enumerate() {
                item.text(if at == 0 { ": " } else { " + " });
                self.lifetime(&mut item, bound);
            }
            items.push(item);
        }
        for (id, param) in params.iter_type_or_consts() {
            let mut item = Code::default();
            match param {
                TypeOrConstParamData::TypeParamData(ty) => {
                    if ty.provenance != TypeParamProvenance::TypeParamList {
                        continue;
                    }
                    let Some(name) = &ty.name else { continue };
                    item.push(Piece::Plain(name.as_str().to_owned()));
                    let bounds = self.inline_bounds(params, id);
                    if !bounds.is_empty() {
                        item.text(": ");
                        for (at, bound) in bounds.iter().enumerate() {
                            if at > 0 {
                                item.text(" + ");
                            }
                            self.bound(&mut item, bound);
                        }
                    }
                    if let Some(default) = ty.default {
                        item.text(" = ");
                        self.ty(&mut item, default);
                    }
                }
                TypeOrConstParamData::ConstParamData(c) => {
                    item.kw("const").text(" ").push(Piece::Plain(c.name.as_str().to_owned()));
                    item.text(": ");
                    self.ty(&mut item, c.ty);
                    if let Some(default) = &c.default {
                        item.text(" = ");
                        self.const_ref(&mut item, default);
                    }
                }
            }
            items.push(item);
        }
        if items.is_empty() {
            return;
        }
        out.text("<");
        for (at, item) in items.into_iter().enumerate() {
            if at > 0 {
                out.text(", ");
            }
            out.extend(item);
        }
        out.text(">");
    }

    /// The bounds written on a type parameter itself (`T: Clone`), not under `for<..>`.
    fn inline_bounds<'p>(
        &self,
        params: &'p GenericParams,
        id: LocalTypeOrConstParamId,
    ) -> Vec<&'p TypeBound> {
        params
            .where_predicates()
            .iter()
            .filter_map(|pred| match pred {
                WherePredicate::TypeBound { lifetimes: None, target, bound }
                    if self.is_param(*target, id) =>
                {
                    Some(bound)
                }
                _ => None,
            })
            .collect()
    }

    fn is_param(&self, target: TypeRefId, id: LocalTypeOrConstParamId) -> bool {
        matches!(self.store.type_ref(target), TypeRef::TypeParam(p) if p.local_id() == id)
    }

    /// The predicates that do not sit on a parameter, as `where` and one predicate to a line.
    pub(crate) fn where_clause(&self, out: &mut Code, params: &GenericParams) -> bool {
        let db = self.db;
        let mut lines: Vec<Code> = vec![];
        let mut last_target: Option<TypeRefId> = None;
        for pred in params.where_predicates() {
            match pred {
                WherePredicate::TypeBound { lifetimes, target, bound } => {
                    let inline = lifetimes.is_none()
                        && match self.store.type_ref(*target) {
                            TypeRef::TypeParam(p) => {
                                let data = &GenericParams::of(db, p.parent())[p.local_id()];
                                // `impl Trait` arguments, trait `Self` and listed parameters
                                // all show their bounds elsewhere.
                                data.name().is_none()
                                    || data.is_trait_self()
                                    || data.type_param().is_some_and(|it| {
                                        it.provenance == TypeParamProvenance::TypeParamList
                                    })
                            }
                            _ => false,
                        };
                    if inline {
                        continue;
                    }
                    if lifetimes.is_none()
                        && last_target == Some(*target)
                        && let Some(line) = lines.last_mut()
                    {
                        line.text(" + ");
                        self.bound(line, bound);
                        continue;
                    }
                    let mut line = Code::default();
                    if let Some(lifetimes) = lifetimes {
                        line.kw("for").text("<");
                        for (at, name) in self.store.names(*lifetimes).iter().enumerate() {
                            if at > 0 {
                                line.text(", ");
                            }
                            line.push(Piece::Lifetime(lifetime_text(name.as_str())));
                        }
                        line.text("> ");
                    }
                    self.ty(&mut line, *target);
                    line.text(": ");
                    self.bound(&mut line, bound);
                    last_target = if lifetimes.is_none() { Some(*target) } else { None };
                    lines.push(line);
                }
                WherePredicate::Lifetime { target, .. } => {
                    // Lifetime bounds on a listed lifetime are shown by it.
                    if matches!(self.store[*target], LifetimeRef::Param(_)) {
                        continue;
                    }
                }
            }
        }
        if lines.is_empty() {
            return false;
        }
        out.text("\n").kw("where");
        for line in lines {
            out.text("\n    ");
            out.extend(line);
            out.text(",");
        }
        true
    }
}

pub(crate) fn lifetime_text(name: &str) -> String {
    if name.starts_with('\'') { name.to_owned() } else { format!("'{name}") }
}

/// `pub`, `pub(crate)`, `pub(super)`, `pub(in path)` or nothing, said from `module`, the module
/// the item is in.
pub(crate) fn visibility(db: &Db, out: &mut Code, vis: Visibility, module: hir::Module) {
    match vis {
        Visibility::Public => {
            out.kw("pub").text(" ");
        }
        Visibility::PubCrate(_) => {
            out.kw("pub").text("(").kw("crate").text(") ");
        }
        Visibility::Module(to, _) => {
            let to: hir::Module = to.into();
            if to == module {
            } else if to.is_crate_root(db) {
                out.kw("pub").text("(").kw("crate").text(") ");
            } else if module.parent(db) == Some(to) {
                out.kw("pub").text("(").kw("super").text(") ");
            } else {
                let mut parts = vec![];
                let mut at = Some(to);
                while let Some(m) = at {
                    if let Some(name) = m.name(db) {
                        parts.push(name.as_str().to_owned());
                    }
                    at = m.parent(db);
                }
                parts.push("crate".to_owned());
                parts.reverse();
                out.kw("pub").text("(").kw("in").text(format!(" {}) ", parts.join("::")));
            }
        }
    }
}

/// A signature renderer for the generic parameters of `def`.
fn sig_of<'a>(
    db: &'a Db,
    places: Linking<'a>,
    store: &'a ExpressionStore,
    def: GenericDefId,
) -> Sig<'a> {
    Sig::new(db, places, store, def.resolver(db), ExpressionStoreOwnerId::Signature(def))
}

/// How a function's declaration reads where it is listed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FnContext {
    /// A free function or an inherent method: with its visibility.
    Free,
    /// An item of a trait or a trait impl: no visibility.
    Trait,
}

pub(crate) fn function(
    db: &Db,
    places: Linking<'_>,
    func: hir::Function,
    context: FnContext,
    name_kind: Kind,
) -> Code {
    let mut out = Code::default();
    let Ok(id) = FunctionId::try_from(func) else {
        // A method of a built-in derive's impl has no signature of its own to show.
        out.kw("fn").text(" ").push(Piece::Name(func.name(db).as_str().to_owned(), name_kind));
        out.text("(&self)");
        return out;
    };
    let data = FunctionSignature::of(db, id);
    let sig = sig_of(db, places, &data.store, id.into());
    if context == FnContext::Free {
        let module = func.module(db);
        visibility(db, &mut out, func.visibility(db), module);
    }
    if data.is_default() {
        out.kw("default").text(" ");
    }
    if data.is_const() {
        out.kw("const").text(" ");
    }
    if data.is_async() {
        out.kw("async").text(" ");
    }
    let is_extern_block = matches!(id.loc(db).container, ItemContainerId::ExternBlockId(_));
    if data.is_unsafe() || (is_extern_block && !data.is_safe()) {
        out.kw("unsafe").text(" ");
    }
    if data.abi != ExternAbi::Rust && !is_extern_block {
        out.kw("extern").text(format!(" \"{}\" ", data.abi.as_str()));
    }
    out.kw("fn").text(" ").push(Piece::Name(data.name.as_str().to_owned(), name_kind));
    sig.generics(&mut out, &data.generic_params);

    let names = param_names(db, func);
    let mut params: Vec<Code> = vec![];
    for (at, ty) in data.params.iter().enumerate() {
        let mut param = Code::default();
        if at == 0 && data.has_self_param() {
            self_param(&sig, &mut param, *ty, &data.store);
        } else {
            let name = names.get(at).cloned().flatten().unwrap_or_else(|| "_".to_owned());
            param.text(format!("{name}: "));
            sig.ty(&mut param, *ty);
        }
        params.push(param);
    }
    if data.is_varargs() {
        let mut param = Code::default();
        param.text("...");
        params.push(param);
    }
    let mut ret = Code::default();
    if let Some(ret_ty) = return_type(data)
        && !matches!(data.store.type_ref(ret_ty), TypeRef::Tuple(it) if it.is_empty())
    {
        ret.text(" -> ");
        sig.ty(&mut ret, ret_ty);
    }
    let one_line: usize = out.plain().len()
        + params.iter().map(|it| it.plain().len() + 2).sum::<usize>()
        + ret.plain().len()
        + 2;
    out.text("(");
    if one_line > WIDTH && !params.is_empty() {
        for param in params {
            out.text("\n    ");
            out.extend(param);
            out.text(",");
        }
        out.text("\n");
    } else {
        for (at, param) in params.into_iter().enumerate() {
            if at > 0 {
                out.text(", ");
            }
            out.extend(param);
        }
    }
    out.text(")");
    out.extend(ret);
    sig.where_clause(&mut out, &data.generic_params);
    out
}

/// An `async fn`'s return type is lowered to `impl Future<Output = T>`; rustdoc shows `T`.
fn return_type(data: &FunctionSignature) -> Option<TypeRefId> {
    let ret = data.ret_type?;
    if !data.is_async() && !data.is_gen() {
        return Some(ret);
    }
    match data.store.type_ref(ret) {
        TypeRef::ImplTrait(bounds) => match bounds.first() {
            Some(TypeBound::Path(path, _)) => data
                .store
                .path(*path)
                .segments()
                .last()
                .and_then(|it| it.args_and_bindings)
                .and_then(|it| it.bindings.first())
                .and_then(|it| it.type_ref),
            _ => None,
        },
        _ => Some(ret),
    }
}

fn self_param(sig: &Sig<'_>, out: &mut Code, ty: TypeRefId, store: &ExpressionStore) {
    match store.type_ref(ty) {
        TypeRef::Path(path) if path.is_self_type() => {
            out.kw("self");
        }
        TypeRef::Reference(reference) if matches!(store.type_ref(reference.ty), TypeRef::Path(p) if p.is_self_type()) =>
        {
            out.text("&");
            if let Some(lifetime) = reference.lifetime {
                sig.lifetime(out, lifetime);
                out.text(" ");
            }
            if reference.mutability == Mutability::Mut {
                out.kw("mut").text(" ");
            }
            out.kw("self");
        }
        _ => {
            out.kw("self").text(": ");
            sig.ty(out, ty);
        }
    }
}

/// The parameter names as rustdoc shows them: a binding's name without `mut` or `ref`, another
/// pattern as written.
fn param_names(db: &Db, func: hir::Function) -> Vec<Option<String>> {
    use hir::view::HasSourceView;
    let Some(source) = func.source_view(db) else { return vec![] };
    let Some(list) = source.value.param_list() else { return vec![] };
    let mut out = vec![];
    if list.self_param().is_some() {
        out.push(None);
    }
    for param in list.params() {
        let name = param.pat().map(|pat| match pat {
            ast::Pat::IdentPat(ident) if ident.pat().is_none() => {
                ident.name().map_or_else(|| "_".to_owned(), |it| it.text().to_owned())
            }
            other => squash(other.syntax().text()),
        });
        out.push(Some(name.unwrap_or_else(|| "_".to_owned())));
    }
    out
}

fn squash(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) struct Fields {
    pub(crate) decl: Code,
    pub(crate) hidden: bool,
}

/// A struct, union or enum variant's fields as the declaration lists them: public ones (all
/// when documenting private items), the rest as `/* private fields */`.
fn fields_decl(
    db: &Db,
    places: Linking<'_>,
    variant: VariantId,
    module: hir::Module,
    show_private: bool,
    with_visibility: bool,
    indent: &str,
) -> Fields {
    let fields = VariantFields::of(db, variant);
    let sig = Sig::new(
        db,
        places,
        &fields.store,
        variant.resolver(db),
        ExpressionStoreOwnerId::VariantFields(variant),
    );
    let hir_variant: hir::Variant = variant.into();
    let hir_fields = hir_variant.fields(db);
    let mut out = Code::default();
    let mut hidden = false;
    let shown: Vec<(usize, &hir_def::signatures::FieldData, hir::Field)> = fields
        .fields()
        .iter()
        .zip(hir_fields)
        .enumerate()
        .map(|(at, ((_, data), field))| (at, data, field))
        .filter(|(_, _, field)| {
            use hir::HasAttrs;
            let visible = show_private || field.visibility(db) == hir::Visibility::Public;
            let visible = visible && !field.attrs(db).is_doc_hidden();
            hidden |= !visible;
            visible
        })
        .collect();
    match fields.shape {
        FieldsShape::Unit => {}
        FieldsShape::Tuple => {
            out.text("(");
            for (at, (_, data, field)) in shown.iter().enumerate() {
                if at > 0 {
                    out.text(", ");
                }
                if with_visibility {
                    visibility(db, &mut out, field.visibility(db), module);
                }
                sig.ty(&mut out, data.type_ref);
            }
            if hidden {
                if !shown.is_empty() {
                    out.text(", ");
                }
                out.push(Piece::Comment("/* private fields */".to_owned()));
            }
            out.text(")");
        }
        FieldsShape::Record => {
            if shown.is_empty() && !hidden {
                out.text(" {}");
            } else {
                out.text(" {\n");
                for (_, data, field) in &shown {
                    out.text(indent).text("    ");
                    if with_visibility {
                        visibility(db, &mut out, field.visibility(db), module);
                    }
                    out.push(Piece::Name(data.name.as_str().to_owned(), Kind::StructField));
                    out.text(": ");
                    sig.ty(&mut out, data.type_ref);
                    out.text(",\n");
                }
                if hidden {
                    out.text(indent).text("    ");
                    out.push(Piece::Comment("/* private fields */".to_owned()));
                    out.text("\n");
                }
                out.text(indent).text("}");
            }
        }
    }
    Fields { decl: out, hidden }
}

/// The type of one field, for its entry under "Fields".
pub(crate) fn field_type(db: &Db, places: Linking<'_>, field: hir::Field) -> Code {
    let variant: VariantId = field.parent_def(db).into();
    let fields = VariantFields::of(db, variant);
    let sig = Sig::new(
        db,
        places,
        &fields.store,
        variant.resolver(db),
        ExpressionStoreOwnerId::VariantFields(variant),
    );
    let id: hir_def::FieldId = field.into();
    let mut out = Code::default();
    sig.ty(&mut out, fields.fields()[id.local_id].type_ref);
    out
}

/// A variant as its entry shows it: `Name(u8)`, `Name { .. }` with its fields, `Name = 3`.
pub(crate) fn variant(db: &Db, places: Linking<'_>, variant: hir::EnumVariant) -> Code {
    let mut out = Code::default();
    out.push(Piece::Name(variant.name(db).as_str().to_owned(), Kind::Variant));
    let id: hir_def::EnumVariantId = variant.into();
    let module = variant.module(db);
    let fields = fields_decl(db, places, VariantId::EnumVariantId(id), module, true, false, "");
    if let FieldsShape::Tuple = VariantFields::of(db, VariantId::EnumVariantId(id)).shape {
        out.extend(fields.decl);
    }
    out
}

/// A struct's declaration, or with `alias` the one its alias's page shows.
pub(crate) fn struct_decl(
    db: &Db,
    places: Linking<'_>,
    it: hir::Struct,
    show_private: bool,
    alias: Option<&Aliased>,
) -> (Code, bool) {
    let id: hir_def::StructId = it.into();
    let data = StructSignature::of(db, id);
    let module = it.module(db);
    let mut out = Code::default();
    attributes(db, &mut out, AdtId::StructId(id));
    visibility(db, &mut out, it.visibility(db), module);
    out.kw("struct").text(" ");
    let sig = sig_of(db, places, &data.store, id.into());
    let where_clause =
        head(&sig, &mut out, alias, data.name.as_str(), Kind::Struct, &data.generic_params);
    let has_where = !where_clause.0.is_empty();
    let places = alias.map_or(places, |it| places.substituting(&it.subst));
    let fields = fields_decl(db, places, VariantId::StructId(id), module, show_private, true, "");
    match data.shape {
        FieldsShape::Record => {
            out.extend(where_clause);
            if has_where {
                out.text("\n");
                out.extend(trim_leading_space(fields.decl));
            } else {
                out.extend(fields.decl);
            }
        }
        FieldsShape::Tuple => {
            out.extend(fields.decl);
            out.extend(where_clause);
            out.text(";");
        }
        FieldsShape::Unit => {
            out.extend(where_clause);
            out.text(";");
        }
    }
    (out, fields.hidden)
}

fn trim_leading_space(mut code: Code) -> Code {
    if let Some(Piece::Text(text)) = code.0.first_mut()
        && let Some(rest) = text.strip_prefix(' ')
    {
        *text = rest.to_owned();
    }
    code
}

/// A union's declaration, or with `alias` the one its alias's page shows.
pub(crate) fn union_decl(
    db: &Db,
    places: Linking<'_>,
    it: hir::Union,
    show_private: bool,
    alias: Option<&Aliased>,
) -> (Code, bool) {
    let id: hir_def::UnionId = it.into();
    let data = UnionSignature::of(db, id);
    let module = it.module(db);
    let mut out = Code::default();
    attributes(db, &mut out, AdtId::UnionId(id));
    visibility(db, &mut out, it.visibility(db), module);
    out.kw("union").text(" ");
    let sig = sig_of(db, places, &data.store, id.into());
    let where_clause =
        head(&sig, &mut out, alias, data.name.as_str(), Kind::Union, &data.generic_params);
    let has_where = !where_clause.0.is_empty();
    out.extend(where_clause);
    let places = alias.map_or(places, |it| places.substituting(&it.subst));
    let fields = fields_decl(db, places, VariantId::UnionId(id), module, show_private, true, "");
    if has_where {
        out.text("\n");
        out.extend(trim_leading_space(fields.decl));
    } else {
        out.extend(fields.decl);
    }
    (out, fields.hidden)
}

/// An enum's declaration, or with `alias` the one its alias's page shows.
pub(crate) fn enum_decl(
    db: &Db,
    places: Linking<'_>,
    it: hir::Enum,
    alias: Option<&Aliased>,
) -> Code {
    use hir::HasAttrs;
    let id: hir_def::EnumId = it.into();
    let data = EnumSignature::of(db, id);
    let module = it.module(db);
    let mut out = Code::default();
    attributes(db, &mut out, AdtId::EnumId(id));
    visibility(db, &mut out, it.visibility(db), module);
    out.kw("enum").text(" ");
    let sig = sig_of(db, places, &data.store, id.into());
    let where_clause =
        head(&sig, &mut out, alias, data.name.as_str(), Kind::Enum, &data.generic_params);
    let has_where = !where_clause.0.is_empty();
    out.extend(where_clause);
    let places = alias.map_or(places, |it| places.substituting(&it.subst));
    let variants: Vec<hir::EnumVariant> =
        it.variants(db).into_iter().filter(|v| !v.attrs(db).is_doc_hidden()).collect();
    let some_hidden = variants.len() != it.variants(db).len();
    out.text(if has_where { "\n{" } else { " {" });
    if variants.is_empty() && !some_hidden {
        out.text("}");
        return out;
    }
    out.text("\n");
    for v in variants {
        out.text("    ");
        out.push(Piece::Name(v.name(db).as_str().to_owned(), Kind::Variant));
        let vid: hir_def::EnumVariantId = v.into();
        let fields =
            fields_decl(db, places, VariantId::EnumVariantId(vid), module, true, false, "    ");
        out.extend(fields.decl);
        out.text(",\n");
    }
    if some_hidden {
        out.text("    ").push(Piece::Comment("// some variants omitted".to_owned()));
        out.text("\n");
    }
    out.text("}");
    out
}

/// `#[repr(..)]` and `#[non_exhaustive]`, which rustdoc shows above a type.
fn attributes(db: &Db, out: &mut Code, adt: AdtId) {
    let flags = hir_def::attrs::AttrFlags::query(db, adt.into());
    if flags.contains(hir_def::attrs::AttrFlags::NON_EXHAUSTIVE) {
        out.push(Piece::Attribute("#[non_exhaustive]".to_owned())).text("\n");
    }
}

pub(crate) fn trait_header(db: &Db, places: Linking<'_>, it: hir::Trait, out: &mut Code) -> Code {
    let id: TraitId = it.into();
    let data = TraitSignature::of(db, id);
    visibility(db, out, it.visibility(db), it.module(db));
    if data.flags.contains(TraitFlags::UNSAFE) {
        out.kw("unsafe").text(" ");
    }
    if data.flags.contains(TraitFlags::AUTO) {
        out.kw("auto").text(" ");
    }
    out.kw("trait").text(" ").push(Piece::Name(data.name.as_str().to_owned(), Kind::Trait));
    let sig = sig_of(db, places, &data.store, id.into());
    sig.generics(out, &data.generic_params);
    // Supertraits are bounds on the trait's `Self`.
    let supers: Vec<&TypeBound> = data
        .generic_params
        .where_predicates()
        .iter()
        .filter_map(|pred| match pred {
            WherePredicate::TypeBound { lifetimes: None, target, bound }
                if matches!(data.store.type_ref(*target), TypeRef::TypeParam(p)
                    if data.generic_params[p.local_id()].is_trait_self()) =>
            {
                Some(bound)
            }
            _ => None,
        })
        .collect();
    if !supers.is_empty() {
        out.text(": ");
        for (at, bound) in supers.into_iter().enumerate() {
            if at > 0 {
                out.text(" + ");
            }
            sig.bound(out, bound);
        }
    }
    let mut where_clause = Code::default();
    sig.where_clause(&mut where_clause, &data.generic_params);
    where_clause
}

pub(crate) fn type_alias(
    db: &Db,
    places: Linking<'_>,
    it: hir::TypeAlias,
    context: FnContext,
    name_kind: Kind,
) -> Code {
    let id: TypeAliasId = it.into();
    let data = TypeAliasSignature::of(db, id);
    let mut out = Code::default();
    if context == FnContext::Free {
        visibility(db, &mut out, it.visibility(db), it.module(db));
    }
    out.kw("type").text(" ").push(Piece::Name(data.name.as_str().to_owned(), name_kind));
    let sig = sig_of(db, places, &data.store, id.into());
    sig.generics(&mut out, &data.generic_params);
    if !data.bounds.is_empty() {
        out.text(": ");
        sig.bounds(&mut out, &data.bounds);
    }
    let mut where_clause = Code::default();
    sig.where_clause(&mut where_clause, &data.generic_params);
    if let Some(ty) = data.ty {
        out.text(" = ");
        sig.ty(&mut out, ty);
    }
    out.extend(where_clause);
    out.text(";");
    out
}

/// What an alias of a struct, union or enum shows under "Aliased Type": the type under the
/// alias's name and generics, its parameters as the alias's arguments (rustdoc's
/// `render_type_alias` at rust 12433c307b5a497388ffe2f4e49dbf1ab8c71fa0).
pub(crate) struct Aliased {
    pub(crate) adt: hir::Adt,
    /// The type's parameters as the alias's arguments, or their defaults.
    pub(crate) subst: Vec<(hir_def::TypeParamId, Code)>,
    name: String,
    generics: Code,
    where_clause: Code,
}

pub(crate) fn aliased(db: &Db, places: Linking<'_>, it: hir::TypeAlias) -> Option<Aliased> {
    let id: TypeAliasId = it.into();
    let data = TypeAliasSignature::of(db, id);
    let TypeRef::Path(path) = data.store.type_ref(data.ty?) else { return None };
    let sig = sig_of(db, places, &data.store, id.into());
    let TypeNs::AdtId(adt_id) = sig.resolver.resolve_path_in_type_ns_fully(db, path)? else {
        return None;
    };
    let args: Vec<TypeRefId> = path
        .segments()
        .last()
        .and_then(|it| it.args_and_bindings)
        .map(|it| {
            it.args
                .iter()
                .filter_map(|arg| match arg {
                    GenericArg::Type(ty) => Some(*ty),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let adt_def = GenericDefId::AdtId(adt_id);
    let (params, store) = GenericParams::with_store(db, adt_def);
    let adt_sig = sig_of(db, places, store, adt_def);
    let mut subst = vec![];
    let type_params =
        params.iter_type_or_consts().filter_map(|(local, data)| Some((local, data.type_param()?)));
    for (at, (local, param)) in type_params.enumerate() {
        let param_id = hir_def::TypeParamId::from_unchecked(hir_def::TypeOrConstParamId {
            parent: adt_def,
            local_id: local,
        });
        let mut code = Code::default();
        match (args.get(at), param.default) {
            (Some(arg), _) => sig.ty(&mut code, *arg),
            (None, Some(default)) => adt_sig.ty(&mut code, default),
            (None, None) => continue,
        }
        subst.push((param_id, code));
    }
    let mut generics = Code::default();
    sig.generics(&mut generics, &data.generic_params);
    let mut where_clause = Code::default();
    sig.where_clause(&mut where_clause, &data.generic_params);
    Some(Aliased {
        adt: adt_id.into(),
        subst,
        name: data.name.as_str().to_owned(),
        generics,
        where_clause,
    })
}

/// The name, generics and `where` clause a struct, union or enum's declaration starts with:
/// its own, or an alias's.
fn head(
    sig: &Sig<'_>,
    out: &mut Code,
    alias: Option<&Aliased>,
    name: &str,
    kind: Kind,
    params: &GenericParams,
) -> Code {
    let mut where_clause = Code::default();
    match alias {
        Some(alias) => {
            out.push(Piece::Name(alias.name.clone(), kind));
            out.extend(alias.generics.clone());
            where_clause.extend(alias.where_clause.clone());
        }
        None => {
            out.push(Piece::Name(name.to_owned(), kind));
            sig.generics(out, params);
            sig.where_clause(&mut where_clause, params);
        }
    }
    where_clause
}

/// The type an alias stands for: `[u8]` of `type Target = [u8];`.
pub(crate) fn alias_type(db: &Db, places: Linking<'_>, it: hir::TypeAlias) -> Option<Code> {
    let id: TypeAliasId = it.into();
    let data = TypeAliasSignature::of(db, id);
    let mut out = Code::default();
    sig_of(db, places, &data.store, id.into()).ty(&mut out, data.ty?);
    Some(out)
}

pub(crate) fn constant(
    db: &Db,
    places: Linking<'_>,
    it: hir::Const,
    context: FnContext,
    name_kind: Kind,
) -> Code {
    let id: hir_def::ConstId = it.into();
    let data = ConstSignature::of(db, id);
    let mut out = Code::default();
    if context == FnContext::Free {
        visibility(db, &mut out, it.visibility(db), it.module(db));
    }
    let name = data.name.as_ref().map_or("_", |it| it.as_str());
    out.kw("const").text(" ").push(Piece::Name(name.to_owned(), name_kind)).text(": ");
    let sig = sig_of(db, places, &data.store, id.into());
    sig.ty(&mut out, data.type_ref);
    if context == FnContext::Free
        && let Some(value) = short_initializer(db, it)
    {
        out.text(" = ").text(value);
    }
    out.text(";");
    out
}

/// A constant's value as written when it is short and simple, as rustdoc shows it.
fn short_initializer(db: &Db, it: hir::Const) -> Option<String> {
    use hir::view::HasSourceView;
    let source = it.source_view(db)?;
    let body = source.value.body()?;
    let simple = matches!(body, ast::Expr::Literal(_) | ast::Expr::PathExpr(_))
        || matches!(body, ast::Expr::PrefixExpr(prefix) if matches!(prefix.expr(), Some(ast::Expr::Literal(_))));
    let text = squash(body.syntax().text());
    (simple && text.len() <= 60).then_some(text)
}

pub(crate) fn static_decl(db: &Db, places: Linking<'_>, it: hir::Static) -> Code {
    let id: hir_def::StaticId = it.into();
    let data = StaticSignature::of(db, id);
    let mut out = Code::default();
    visibility(db, &mut out, it.visibility(db), it.module(db));
    if data.flags.contains(StaticFlags::UNSAFE) {
        out.kw("unsafe").text(" ");
    }
    out.kw("static").text(" ");
    if data.flags.contains(StaticFlags::MUTABLE) {
        out.kw("mut").text(" ");
    }
    out.push(Piece::Name(data.name.as_str().to_owned(), Kind::Static)).text(": ");
    let sig = sig_of(db, places, &data.store, id.into());
    sig.ty(&mut out, data.type_ref);
    out
}

/// `impl<T: Clone> Trait<A> for Foo<T>` and its `where` clause.
pub(crate) fn impl_header(db: &Db, places: Linking<'_>, it: hir::Impl) -> Code {
    if let (Some(trait_), Some(adt)) = (it.trait_(db), it.self_ty(db).as_adt())
        && is_builtin_derive(db, it)
    {
        return derive_header(db, places, trait_, trait_, adt);
    }
    let mut out = Code::default();
    let Ok(id) = ImplId::try_from(it) else { return out };
    let data = ImplSignature::of(db, id);
    let sig = sig_of(db, places, &data.store, id.into());
    if data.flags.contains(hir_def::signatures::ImplFlags::UNSAFE) {
        out.kw("unsafe").text(" ");
    }
    out.kw("impl");
    sig.generics(&mut out, &data.generic_params);
    out.text(" ");
    if let Some(trait_ref) = &data.target_trait {
        if data.is_negative() {
            out.text("!");
        }
        sig.path(&mut out, data.store.path(trait_ref.path));
        out.text(" ").kw("for").text(" ");
    }
    sig.ty(&mut out, data.self_ty);
    sig.where_clause(&mut out, &data.generic_params);
    out
}

/// Whether `it` is the impl of a built-in derive (`#[derive(Clone)]`), which rustdoc shows as
/// rustc's derive writes it, whatever rust-analyzer's expansion of it says.
pub(crate) fn is_builtin_derive(db: &Db, it: hir::Impl) -> bool {
    use hir::view::HasSourceView;
    if ImplId::try_from(it).is_err() {
        return true;
    }
    let Some(source) = it.source_view(db) else { return false };
    source
        .file_id
        .macro_file()
        .is_some_and(|call| call.kind(db) == hir_expand::MacroKind::DeriveBuiltIn)
}

/// `impl<'a, T: Clone> Clone for Foo<'a, T>`: the type's parameters, each type bounded by the
/// derived trait, as rustc's derive writes the impl; `bound` is that trait when it is not the
/// one implemented, as `impl<T: PartialEq> StructuralPartialEq for Foo<T>`.
pub(crate) fn derive_header(
    db: &Db,
    places: Linking<'_>,
    trait_: hir::Trait,
    bound: hir::Trait,
    adt: hir::Adt,
) -> Code {
    let params = adt_params(db, places, adt);
    let piece = |it: hir::Trait| {
        let name = it.name(db).as_str().to_owned();
        match places.target(db, ModuleDef::Trait(it)) {
            Some(to) => Piece::Link { text: name, kind: Kind::Trait, to },
            None => Piece::Plain(name),
        }
    };
    let trait_piece = piece(trait_);
    let bound_piece = piece(bound);
    let mut out = Code::default();
    out.kw("impl");
    if !params.decls.is_empty() {
        out.text("<");
        for (at, decl) in params.decls.iter().enumerate() {
            if at > 0 {
                out.text(", ");
            }
            out.extend(decl.clone());
            if at >= params.lifetimes && decl.0.len() == 1 && matches!(decl.0[0], Piece::Text(_)) {
                out.text(": ").push(bound_piece.clone());
            }
        }
        out.text(">");
    }
    out.text(" ").push(trait_piece).text(" ").kw("for").text(" ");
    let adt_name = adt.name(db).as_str().to_owned();
    match places.target(db, ModuleDef::Adt(adt)) {
        Some(to) => out.push(Piece::Link { text: adt_name, kind: adt_kind(adt), to }),
        None => out.push(Piece::Plain(adt_name)),
    };
    if !params.args.is_empty() {
        out.text(format!("<{}>", params.args.join(", ")));
    }
    out
}

pub(crate) fn adt_kind(adt: hir::Adt) -> Kind {
    match adt {
        hir::Adt::Struct(_) => Kind::Struct,
        hir::Adt::Enum(_) => Kind::Enum,
        hir::Adt::Union(_) => Kind::Union,
    }
}

/// The type parameters of an ADT, for the impls rustdoc works out.
/// The generic parameters of an ADT as a synthesized impl names them.
pub(crate) struct AdtParams {
    pub(crate) lifetimes: usize,
    /// The type parameters, in order.
    pub(crate) types: Vec<String>,
    /// Each parameter as the impl declares it: `'a`, `T`, `const N: usize`.
    pub(crate) decls: Vec<Code>,
    /// Each parameter as the type takes it: `'a`, `T`, `N`.
    pub(crate) args: Vec<String>,
}

pub(crate) fn adt_params(db: &Db, places: Linking<'_>, adt: hir::Adt) -> AdtParams {
    let def = GenericDefId::AdtId(adt.into());
    let (params, store) = GenericParams::with_store(db, def);
    let sig = sig_of(db, places, store, def);
    let mut out = AdtParams { lifetimes: 0, types: vec![], decls: vec![], args: vec![] };
    for (_, it) in params.iter_lt() {
        let name = lifetime_text(it.name.as_str());
        out.lifetimes += 1;
        out.decls.push(Code(vec![Piece::Lifetime(name.clone())]));
        out.args.push(name);
    }
    for (_, it) in params.iter_type_or_consts() {
        let Some(name) = it.name().map(|n| n.as_str().to_owned()) else { continue };
        let mut decl = Code::default();
        match it {
            TypeOrConstParamData::TypeParamData(_) => {
                decl.text(&name);
                out.types.push(name.clone());
            }
            TypeOrConstParamData::ConstParamData(data) => {
                decl.kw("const").text(format!(" {name}: "));
                sig.ty(&mut decl, data.ty);
            }
        }
        out.decls.push(decl);
        out.args.push(name);
    }
    out
}
