//! E0392: a type or lifetime parameter of a struct, enum or union that nothing constrains, so
//! its variance is bivariant; or one used only through the type itself ("is only used
//! recursively"). E0091: a type parameter of a type alias its type never names.
//!
//! Follows rustc's `compiler/rustc_hir_analysis/src/check/check.rs`
//! (`check_type_alias_type_params_are_used`), `wfcheck.rs` (`report_bivariance`,
//! `IsProbablyCyclical`, `CollectUsageSpans`) and `diagnostics.rs` (`UnusedGenericParameter`,
//! `RecursiveGenericParameter`, `UnusedGenericParameterHelp`) (MIT or Apache-2.0). Which
//! parameters are unused (`check_variances_for_type_defn`: variance, then
//! `identify_constrained_generic_params` over the lowered predicates, then `HasErrorDeep`) is
//! hir-ty's `adt_unused_params`, through `hir::Adt::unused_params`.

use blaze_hash::HashSet;
use blaze_items::{Fact, ItemId};
use hir::PathResolution;
use smallvec::SmallVec;
use std::alloc::Global;
use syntax::{
    TextRange, WalkEvent,
    view::{
        self as ast, AstNode as _, HasGenericArgs as _, HasGenericParams as _, HasName as _,
        HasTypeBounds as _,
    },
};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Diag, MultiSpan, Span},
};

pub(super) fn check<'db>(cx: &Cx<'_, 'db>, adt: hir::Adt) -> Vec<Found> {
    let db = cx.db;
    // The parameters rustc's `check_variances_for_type_defn` reports: bivariant, constrained by
    // no projection of the lowered predicates (`identify_constrained_generic_params`, which sees
    // `Fn` sugar's `-> T` and associated type bounds as written syntax does not), and none when
    // a field's type has an error (`HasErrorDeep`). Almost always empty.
    let unused = adt.unused_params(db);
    if unused.is_empty() {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(adt) else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(list) = source.value.generic_param_list() else { return Vec::new() };
    let params: Vec<(hir::GenericParam, ast::GenericParam<'db>)> =
        // alloc: only for an ADT with an unconstrained parameter, mostly the error
        list.generic_params().filter_map(|it| Some((cx.vsema.to_def(it)?, it))).collect();

    let fields = field_types(source.value);
    let invalid = Invalid::of(list);
    if invalid.named_in(&fields) {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let phantom_data = hir_def::lang_item::lang_items(db, cx.krate.base())
        .PhantomData
        .map(|it| cx.def_path(hir::ModuleDef::Adt(hir::Adt::Struct(it.into()))));
    // alloc: only for an ADT with a bivariant parameter, mostly the error
    let what = format!("the fields of `{}`", cx.def_path(hir::ModuleDef::Adt(adt)));
    let fact = Fact::Layout(cx.key(ItemId::Adt(adt.into())));
    let mut cyclical = None;
    // alloc: only for an ADT with a bivariant parameter, mostly the error
    let mut found = Vec::new();
    for (param, node) in &params {
        if !unused.contains(param) {
            continue;
        }
        let (name, kind, range) = match node {
            ast::GenericParam::TypeParam(it) => {
                let Some(name) = it.name() else { continue };
                // alloc: only for an ADT with a bivariant parameter, mostly the error
                (name.text().to_string(), "type parameter", name.syntax().text_range())
            }
            ast::GenericParam::LifetimeParam(it) => {
                let Some(lifetime) = it.lifetime() else { continue };
                if invalid.lifetimes.contains(&lifetime.syntax().text_range()) {
                    continue;
                }
                // alloc: only for an ADT with a bivariant parameter, mostly the error
                (lifetime.text().to_string(), "lifetime parameter", lifetime.syntax().text_range())
            }
            ast::GenericParam::ConstParam(_) => continue,
        };
        let span = cx.span(source.with_value(range));
        let usages: Vec<Span> = usage_spans(cx, &fields, *param)
            .into_iter()
            .map(|it| cx.span(source.with_value(it)))
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            .collect();
        let help = match &phantom_data {
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            Some(phantom) => format!(
                "consider removing `{name}`, referring to it in a field, or using a marker such as `{phantom}`"
            ),
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            None => format!("consider removing `{name}` or referring to it in a field"),
        };
        if !usages.is_empty() && *cyclical.get_or_insert_with(|| is_probably_cyclical(cx, adt)) {
            let mut diag = Diag::new(
                crate::model::Level::Error,
                None,
                // alloc: only for an ADT with a bivariant parameter, mostly the error
                format!("{kind} `{name}` is only used recursively"),
            );
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            diag.span = MultiSpan { primary: usages, labels: Vec::new() };
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            diag.span_label(span, format!("{kind} must be used non-recursively in the definition"));
            diag.help(help);
            diag.note("all type parameters must be used in a non-recursive way in order to constrain their variance");
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            diag.fact(fact.clone(), what.clone());
            found.push(Found::new(diag, Phase::Items));
            continue;
        }
        // alloc: only for an ADT with a bivariant parameter, mostly the error
        let mut diag =
            Diag::error(Some("E0392"), format!("{kind} `{name}` is never used"), span.clone());
        // alloc: only for an ADT with a bivariant parameter, mostly the error
        diag.span_label(span, format!("unused {kind}"));
        for usage in usages {
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            diag.span_label(
                usage,
                format!("`{name}` is named here, but is likely unused in the containing type"),
            );
        }
        diag.help(help);
        if let ast::GenericParam::TypeParam(it) = node
            && !is_explicitly_bounded(cx, source.value, *it, *param)
        {
            // alloc: only for an ADT with a bivariant parameter, mostly the error
            diag.help(format!(
                "if you intended `{name}` to be a const parameter, use `const {name}: /* Type */` instead"
            ));
        }
        // alloc: only for an ADT with a bivariant parameter, mostly the error
        diag.fact(fact.clone(), what.clone());
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// E0091: rustc's `check_type_alias_type_params_are_used`, a type parameter of a type alias its
/// type never names. A checked alias (`checked_type_aliases`, or one holding an `impl Trait`) is
/// checked as an item instead.
pub(super) fn check_alias<'db>(cx: &Cx<'_, 'db>, alias: hir::TypeAlias) -> Vec<Found> {
    let db = cx.db;
    // alloc: empty, allocates nothing
    let Some(source) = cx.vsema.source(alias) else { return Vec::new() };
    // alloc: empty, allocates nothing
    let Some(list) = source.value.generic_param_list() else { return Vec::new() };
    let params: SmallVec<[(hir::GenericParam, ast::TypeParam<'db>); 4]> = list
        .type_or_const_params()
        .filter_map(|it| match it {
            ast::TypeOrConstParam::Type(it) => {
                Some((hir::GenericParam::TypeParam(cx.vsema.to_def(it)?), it))
            }
            ast::TypeOrConstParam::Const(_) => None,
        })
        // alloc: inline up to 4 parameters
        .collect();
    // alloc: empty, allocates nothing
    let Some(ty) = source.value.ty() else { return Vec::new() };
    if params.is_empty()
        || ty.syntax().descendants().any(|it| ast::ImplTraitType::can_cast(it.kind()))
        || Invalid::of(list).named_in(std::slice::from_ref(&ty))
    {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let checked = hir_def::unstable_features::UnstableFeatures::query(db, cx.krate.base())
        .iter()
        .any(|it| it.as_str() == "checked_type_aliases");
    if checked {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let mut error = false;
    alias.ty(db).walk(db, |ty| error |= ty.is_unknown());
    if error {
        // alloc: empty, allocates nothing
        return Vec::new();
    }
    let mut used: SmallVec<[hir::GenericParam; 8]> = SmallVec::new();
    params_in(cx, ty.syntax(), &mut used);

    // rustc reads the bounds after `Sized` elaboration: a parameter without `?Sized` has an
    // implicit bound, so none at all means every parameter is `?Sized` and bounded by nothing.
    // Read for an error alone.
    let mut none = None;
    // alloc: the errors' fact, built for an error
    let what = || format!("the type of `{}`", cx.def_path(hir::ModuleDef::TypeAlias(alias)));
    let fact = Fact::Signature(cx.key(ItemId::Alias(alias.into())));
    // alloc: empty unless there is an error
    let mut found = Vec::new();
    for (param, node) in &params {
        if used.contains(param) {
            continue;
        }
        let Some(name) = node.name() else { continue };
        let name_text = name.text();
        let span = cx.span(source.with_value(name.syntax().text_range()));
        let mut diag =
            // alloc: the error's message
            Diag::error(Some("E0091"), format!("type parameter `{name_text}` is never used"), span.clone());
        diag.span_label(span, "unused type parameter");
        // alloc: the error's help
        diag.help(format!(
            "consider removing `{name_text}` or referring to it in the body of the type alias"
        ));
        let none = *none.get_or_insert_with(|| {
            params.iter().all(|&(def, node)| {
                let (relaxed, written) = written_bounds(cx, source.value, node, def);
                relaxed && !written
            })
        });
        if !(none || written_bounds(cx, source.value, *node, *param).1) {
            // alloc: the error's help
            diag.help(format!(
                "if you intended `{name_text}` to be a const parameter, use `const {name_text}: /* Type */` instead"
            ));
        }
        // clone: the error's fact
        diag.fact(fact.clone(), what());
        found.push(Found::new(diag, Phase::Items));
    }
    found
}

/// Whether `param` of an alias is relaxed with `?Sized`, and whether a bound other than a
/// relaxed one is written on it, inline or in the where clause.
fn written_bounds<'db>(
    cx: &Cx<'_, 'db>,
    node: ast::TypeAlias<'db>,
    param: ast::TypeParam<'db>,
    def: hir::GenericParam,
) -> (bool, bool) {
    // alloc: on the error path
    let mut lists: Vec<ast::TypeBoundList<'db>> = param.type_bound_list().into_iter().collect();
    if let (Some(clause), hir::GenericParam::TypeParam(def)) = (node.where_clause(), def) {
        for pred in clause.predicates() {
            let Some(ast::Type::PathType(ty)) = pred.ty() else { continue };
            let Some(path) = ty.path() else { continue };
            if path.qualifier().is_none()
                && cx.vsema.resolve_path(path) == Some(PathResolution::TypeParam(def))
            {
                lists.extend(pred.type_bound_list());
            }
        }
    }
    let (mut relaxed, mut written) = (false, false);
    for bound in lists.iter().flat_map(|it| it.bounds()) {
        if bound.question_mark_token().is_some() {
            relaxed = true;
        } else {
            written = true;
        }
    }
    (relaxed, written)
}

/// The parameters named anywhere in `node`, inside projections and constants too (rustc's
/// `parameters_for` with `include_nonconstraining`, as E0091 walks the alias's type).
fn params_in<'db>(
    cx: &Cx<'_, 'db>,
    node: ast::Node<'db>,
    out: &mut impl Extend<hir::GenericParam>,
) {
    for event in node.preorder() {
        let WalkEvent::Enter(node) = event else { continue };
        if ast::Lifetime::can_cast(node.kind()) {
            let lifetime = ast::Lifetime::cast(node).expect("a lifetime");
            if let Some(param) = cx.vsema.resolve_lifetime_param(lifetime) {
                out.extend([hir::GenericParam::LifetimeParam(param)]);
            }
            continue;
        }
        let Some(path) = ast::Path::cast(node) else { continue };
        if path.qualifier().is_some() {
            continue;
        }
        match cx.vsema.resolve_path(path) {
            Some(PathResolution::TypeParam(it)) => {
                out.extend([hir::GenericParam::TypeParam(it)]);
            }
            Some(PathResolution::ConstParam(it)) => {
                out.extend([hir::GenericParam::ConstParam(it)]);
            }
            _ => {}
        }
    }
}

/// The written types of the fields of every variant.
fn field_types(node: ast::Adt<'_>) -> Vec<ast::Type<'_>> {
    let lists: Vec<ast::FieldList<'_>> = match node {
        // alloc: only for an ADT with a bivariant parameter
        ast::Adt::Struct(it) => it.field_list().into_iter().collect(),
        // alloc: only for an ADT with a bivariant parameter
        ast::Adt::Union(it) => {
            it.record_field_list().map(ast::FieldList::RecordFieldList).into_iter().collect()
        }
        ast::Adt::Enum(it) => it
            .variant_list()
            .into_iter()
            .flat_map(|it| it.variants())
            .filter_map(|it| it.field_list())
            // alloc: only for an ADT with a bivariant parameter
            .collect(),
    };
    // alloc: only for an ADT with a bivariant parameter
    let mut types = Vec::new();
    for list in lists {
        match list {
            ast::FieldList::RecordFieldList(it) => {
                types.extend(it.fields().filter_map(|it| it.ty()))
            }
            ast::FieldList::TupleFieldList(it) => {
                types.extend(it.fields().filter_map(|it| it.ty()))
            }
        }
    }
    types
}

/// rustc's `CollectUsageSpans`: where the fields name `param`, or `Self`.
fn usage_spans<'db>(
    cx: &Cx<'_, 'db>,
    fields: &[ast::Type<'db>],
    param: hir::GenericParam,
) -> Vec<TextRange> {
    // alloc: only for an ADT with a bivariant parameter
    let mut spans = Vec::new();
    for ty in fields {
        for ty in ty.syntax().descendants().filter_map(ast::PathType::cast) {
            let Some(path) = ty.path() else { continue };
            let Some(first) = path.first_segment() else { continue };
            let Some(first_path) = first.syntax().parent().and_then(ast::Path::cast) else {
                continue;
            };
            if first.generic_arg_list().is_some() || first.type_anchor().is_some() {
                continue;
            }
            if first.self_type_token().is_some() {
                spans.push(first_path.syntax().text_range());
                continue;
            }
            if let hir::GenericParam::TypeParam(param) = param
                && cx.vsema.resolve_path(first_path) == Some(PathResolution::TypeParam(param))
            {
                spans.push(first_path.syntax().text_range());
            }
        }
    }
    spans
}

/// Whether a bound is written on the parameter itself, inline or in the where clause.
fn is_explicitly_bounded<'db>(
    cx: &Cx<'_, 'db>,
    node: ast::Adt<'db>,
    param: ast::TypeParam<'db>,
    def: hir::GenericParam,
) -> bool {
    if param.type_bound_list().is_some() {
        return true;
    }
    let Some(clause) = node.where_clause() else { return false };
    let hir::GenericParam::TypeParam(def) = def else { return false };
    clause.predicates().any(|pred| {
        if pred.type_bound_list().is_some()
            && let Some(ast::Type::PathType(ty)) = pred.ty()
            && let Some(path) = ty.path()
            && path.qualifier().is_none()
        {
            cx.vsema.resolve_path(path) == Some(PathResolution::TypeParam(def))
        } else {
            false
        }
    })
}

/// The parameters rustc's resolver turns into errors (E0403, E0262, E0637): a type or const
/// name declared twice, whose every use is then an error, and a lifetime declared again or
/// named `'static` or `'_`, whose name is `ParamName::Error`.
struct Invalid {
    names: HashSet<String, Global>,
    lifetimes: HashSet<TextRange, Global>,
}

impl Invalid {
    fn of<'t>(list: ast::GenericParamList<'t>) -> Invalid {
        // The names declared before, a lifetime's never one of a type or const.
        let mut lifetimes: SmallVec<[ast::Lifetime<'t>; 4]> = SmallVec::new();
        let mut names: SmallVec<[ast::Name<'t>; 4]> = SmallVec::new();
        // alloc: empty unless a parameter is declared twice, an error
        let mut invalid =
            Invalid { names: HashSet::new_in(Global), lifetimes: HashSet::new_in(Global) };
        for param in list.generic_params() {
            match param {
                ast::GenericParam::LifetimeParam(it) => {
                    let Some(lifetime) = it.lifetime() else { continue };
                    let text = lifetime.text();
                    let again = lifetimes.iter().any(|it| it.text() == text);
                    if again || text == "'static" || text == "'_" {
                        invalid.lifetimes.insert(lifetime.syntax().text_range());
                    }
                    if !again {
                        lifetimes.push(lifetime);
                    }
                }
                ast::GenericParam::TypeParam(it) => invalid.name(&mut names, it.name()),
                ast::GenericParam::ConstParam(it) => invalid.name(&mut names, it.name()),
            }
        }
        invalid
    }

    fn name<'t>(&mut self, seen: &mut SmallVec<[ast::Name<'t>; 4]>, name: Option<ast::Name<'t>>) {
        let Some(name) = name else { return };
        if seen.iter().any(|it| it.text() == name.text()) {
            // alloc: a name declared twice, an error
            self.names.insert(name.text().to_owned());
        } else {
            seen.push(name);
        }
    }

    /// Whether `fields` name a type or const declared twice, rustc's `Res::Err`.
    fn named_in(&self, fields: &[ast::Type<'_>]) -> bool {
        !self.names.is_empty()
            && fields
                .iter()
                .flat_map(|ty| ty.syntax().descendants().filter_map(ast::Path::cast))
                .any(|path| {
                    path.qualifier().is_none()
                        && path
                            .segment()
                            .and_then(|it| it.name_ref())
                            .is_some_and(|it| self.names.contains(it.text()))
                })
    }
}

/// rustc's `IsProbablyCyclical`: whether a field, or one of a type it holds, holds `adt`.
fn is_probably_cyclical(cx: &Cx<'_, '_>, adt: hir::Adt) -> bool {
    let mut cyclical = false;
    walk_fields(cx, adt, |_, it| {
        cyclical |= it == Some(adt);
        cyclical
    });
    cyclical
}

/// Every type in the fields of `adt` and, once each, of the ADTs they hold, until `stop` says.
fn walk_fields(
    cx: &Cx<'_, '_>,
    adt: hir::Adt,
    mut stop: impl FnMut(&hir::Type<'_>, Option<hir::Adt>) -> bool,
) {
    let mut seen = HashSet::from_iter_in([adt], Global);
    // alloc: only for an ADT with a bivariant parameter
    let mut queue = vec![adt];
    while let Some(adt) = queue.pop() {
        for field in adt_fields(cx, adt) {
            let mut done = false;
            field.ty(cx.db).walk(cx.db, |ty| {
                if done {
                    return;
                }
                let inner = ty.as_adt();
                done = stop(&ty, inner);
                if let Some(inner) = inner
                    && seen.insert(inner)
                {
                    queue.push(inner);
                }
            });
            if done {
                return;
            }
        }
    }
}

fn adt_fields(cx: &Cx<'_, '_>, adt: hir::Adt) -> Vec<hir::Field> {
    match adt {
        hir::Adt::Struct(it) => it.fields(cx.db),
        hir::Adt::Union(it) => it.fields(cx.db),
        // alloc: only for an ADT with a bivariant parameter
        hir::Adt::Enum(it) => {
            it.variants_iter(cx.db).flat_map(|it| it.fields_iter(cx.db)).collect()
        }
    }
}
