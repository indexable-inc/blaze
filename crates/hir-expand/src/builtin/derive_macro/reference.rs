//! The derives as they were written before they wrote into one builder: nested `quote!`s, a
//! `format!` per name. Kept for the tests only, which check the new code gives the same trees.
//! [`handle`] keeps `From` and `CoerceShared`'s target as they read the derive's own item
//! through handles.

use blaze_hash::HashSet;
use itertools::izip;
use span::Span;
use stdx::never;
use syntax_bridge::DocCommentDesugarMode;
use tracing::debug;

use crate::{
    ExpandError, ExpandErrorKind, ExpandResult,
    builtin::quote::dollar_crate,
    name::{AsName, Name},
    span_map::ExpansionSpanMap,
    tt,
};
use syntax::ast::{self, AstNode, FieldList, HasAttrs, HasGenericParams, HasName, HasTypeBounds};

/// Where the old derives read the item from, in the database's place: the item and its span
/// map.
pub(super) struct ItemSource<'a>(pub(super) &'a ast::Adt, pub(super) &'a span::SpanMap);

impl ItemSource<'_> {
    fn adt(&self) -> (&ast::Adt, &span::SpanMap) {
        (self.0, self.1)
    }
}

fn parse_adt(
    db: &ItemSource<'_>,
    _: &tt::TopSubtree,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let (adt, tm) = db.adt();
    parse_adt_from_syntax(adt, tm, call_site)
}

#[derive(Clone)]
pub(super) enum VariantShape {
    Struct(Vec<tt::Ident>),
    /// The span of each field, which what is said about the field (as that moving it out of a
    /// packed struct is E0507) points at, as rustc's do.
    Tuple(Vec<Span>),
    Unit,
}

pub(super) fn tuple_field_iterator(spans: &[Span]) -> impl Iterator<Item = tt::Ident> + '_ {
    spans.iter().enumerate().map(|(it, &span)| tt::Ident::new(&format!("f{it}"), span))
}

/// What a pattern over `self` binds a field to. The name of the field may be that of a tuple
/// struct or a constant in scope, which a binding can't have.
pub(super) fn bound_of_self(field: &tt::Ident) -> tt::Ident {
    tt::Ident::new(&format!("{}_self", field.sym), field.span)
}

impl VariantShape {
    fn as_pattern(&self, path: tt::TopSubtree, span: Span) -> tt::TopSubtree {
        self.as_pattern_map(path, span, |it| {
            let bound = bound_of_self(it);
            quote!(span => #bound)
        })
    }

    fn field_names(&self, span: Span) -> Vec<tt::Ident> {
        match self {
            VariantShape::Struct(s) => s.clone(),
            VariantShape::Tuple(spans) => tuple_field_iterator(spans).collect(),
            VariantShape::Unit => vec![],
        }
    }

    fn as_pattern_map(
        &self,
        path: tt::TopSubtree,
        span: Span,
        field_map: impl Fn(&tt::Ident) -> tt::TopSubtree,
    ) -> tt::TopSubtree {
        match self {
            VariantShape::Struct(fields) => {
                let fields = fields.iter().map(|it| {
                    let mapped = field_map(it);
                    quote! {span => #it : #mapped , }
                });
                quote! {span =>
                    #path { # #fields }
                }
            }
            VariantShape::Tuple(spans) => {
                let fields = tuple_field_iterator(spans).map(|it| {
                    let mapped = field_map(&it);
                    quote! {span =>
                        #mapped ,
                    }
                });
                quote! {span =>
                    #path ( # #fields )
                }
            }
            VariantShape::Unit => path,
        }
    }

    fn from(
        call_site: Span,
        tm: &ExpansionSpanMap,
        value: Option<FieldList>,
    ) -> Result<Self, ExpandError> {
        // Where a field is written, in the derive's hygiene: what the expansion says of the
        // field, as copying it out of a packed struct, is reported there, as rustc's is.
        let at_field = |span: Span| Span { ctx: call_site.ctx, ..span };
        let r = match value {
            None => VariantShape::Unit,
            Some(FieldList::RecordFieldList(it)) => VariantShape::Struct(
                it.fields()
                    .map(|it| it.name())
                    .map(|it| {
                        name_to_token(call_site, tm, it)
                            .map(|it| tt::Ident { span: at_field(it.span), ..it })
                    })
                    .collect::<Result<_, _>>()?,
            ),
            Some(FieldList::TupleFieldList(it)) => VariantShape::Tuple(
                it.fields()
                    .map(|field| {
                        let at = field
                            .ty()
                            .map_or_else(|| field.syntax().clone(), |ty| ty.syntax().clone());
                        at_field(tm.span_at(at.text_range().start()))
                    })
                    .collect(),
            ),
        };
        Ok(r)
    }
}

#[derive(Clone)]
pub(super) enum AdtShape {
    Struct(VariantShape),
    Enum { variants: Vec<(tt::Ident, VariantShape)>, default_variant: Option<usize> },
    Union,
}

impl AdtShape {
    fn as_pattern(&self, span: Span, name: &tt::Ident) -> Vec<tt::TopSubtree> {
        self.as_pattern_map(
            name,
            |it| {
                let bound = bound_of_self(it);
                quote!(span =>#bound)
            },
            span,
        )
    }

    fn field_names(&self, span: Span) -> Vec<Vec<tt::Ident>> {
        match self {
            AdtShape::Struct(s) => {
                vec![s.field_names(span)]
            }
            AdtShape::Enum { variants, .. } => {
                variants.iter().map(|(_, fields)| fields.field_names(span)).collect()
            }
            AdtShape::Union => {
                never!("using fields of union in derive is always wrong");
                vec![]
            }
        }
    }

    fn as_pattern_map(
        &self,
        name: &tt::Ident,
        field_map: impl Fn(&tt::Ident) -> tt::TopSubtree,
        span: Span,
    ) -> Vec<tt::TopSubtree> {
        match self {
            AdtShape::Struct(s) => {
                vec![s.as_pattern_map(quote! {span => #name }, span, field_map)]
            }
            AdtShape::Enum { variants, .. } => variants
                .iter()
                .map(|(v, fields)| {
                    fields.as_pattern_map(quote! {span => #name :: #v }, span, &field_map)
                })
                .collect(),
            AdtShape::Union => {
                never!("pattern matching on union is always wrong");
                vec![quote! {span => un }]
            }
        }
    }
}

pub(super) struct BasicAdtInfo {
    name: tt::Ident,
    shape: AdtShape,
    /// first field is the name, and
    /// second field is `Some(ty)` if it's a const param of type `ty`, `None` if it's a type param.
    /// third fields is where bounds, if any
    param_types: Vec<AdtParam>,
    /// Each lifetime parameter as declared, bounds and all, and its name: the impl has them
    /// all, as rustc's does, or its self type would elide them (E0726).
    lifetimes: Vec<(tt::TopSubtree, tt::TopSubtree)>,
    where_clause: Vec<tt::TopSubtree>,
    associated_types: Vec<tt::TopSubtree>,
    /// Whether the fields can be misaligned, and so have to be copied out to be used.
    packed: bool,
}

impl BasicAdtInfo {
    /// A `match` of `self` against each arm's pattern. A packed struct's fields are copied out
    /// of it instead, as rustc's derives do (`{ self.x }`): a field may be misaligned, so isn't
    /// borrowed, and moving one that isn't `Copy` is E0507, where the field is.
    fn match_self(
        &self,
        span: Span,
        arms: Vec<(tt::TopSubtree, tt::TopSubtree)>,
    ) -> tt::TopSubtree {
        if let Some(lets) = self.packed_lets("self")
            && arms.len() == 1
        {
            let (_, body) = arms.into_iter().next().unwrap();
            return quote! {span => { # #lets #body } };
        }
        let arms = arms.into_iter().map(|(pat, body)| {
            let fat_arrow = fat_arrow(span);
            quote! {span => #pat #fat_arrow #body , }
        });
        quote! {span => match self { # #arms } }
    }

    /// For a packed struct, a `let` of each field of `base` (`self` or `other`) to a copy out
    /// of it, to the name the patterns of the other derives bind it to.
    fn packed_lets(&self, base: &str) -> Option<Vec<tt::TopSubtree>> {
        let AdtShape::Struct(shape) = &self.shape else { return None };
        if !self.packed {
            return None;
        }
        let fields = match shape {
            VariantShape::Struct(fields) => fields
                .iter()
                .map(|it| {
                    let at = it.span;
                    (it.clone(), quote! {at => #it })
                })
                .collect(),
            VariantShape::Tuple(spans) => tuple_field_iterator(spans)
                .enumerate()
                .map(|(i, it)| {
                    let at = it.span;
                    let index = quote! {at => #i };
                    (it, index)
                })
                .collect(),
            VariantShape::Unit => vec![],
        };
        let lets = fields.into_iter().map(|(name, field)| {
            let at = name.span;
            let bound = tt::Ident::new(&format!("{}_{base}", name.sym), at);
            let base = tt::Ident::new(base, at);
            quote! {at => let #bound = { #base . #field }; }
        });
        Some(lets.collect())
    }

    /// A `match` of `self` and `other` against each arm's two patterns, and `fallback` if none
    /// matches. A packed struct's fields are copied out of both, as by [`Self::match_self`].
    fn match_both(
        &self,
        span: Span,
        arms: Vec<(tt::TopSubtree, tt::TopSubtree, tt::TopSubtree)>,
        fallback: tt::TopSubtree,
    ) -> tt::TopSubtree {
        if let (Some(lets), Some(other_lets)) =
            (self.packed_lets("self"), self.packed_lets("other"))
            && arms.len() == 1
        {
            let (_, _, body) = arms.into_iter().next().unwrap();
            return quote! {span => { # #lets # #other_lets #body } };
        }
        // Of one variant whose arm is written, nothing could reach a fallback arm.
        let variants = match &self.shape {
            AdtShape::Struct(_) => 1,
            AdtShape::Enum { variants, .. } => variants.len(),
            AdtShape::Union => 0,
        };
        let covered = variants == 1 && arms.len() == 1;
        let arms = arms.into_iter().map(|(pat1, pat2, body)| {
            let fat_arrow = fat_arrow(span);
            quote! {span => ( #pat1 , #pat2 ) #fat_arrow #body , }
        });
        if covered {
            return quote! {span => match (self, other) { # #arms } };
        }
        let fat_arrow = fat_arrow(span);
        quote! {span => match (self, other) { # #arms _unused #fat_arrow #fallback } }
    }
}

pub(super) struct AdtParam {
    name: tt::TopSubtree,
    /// `None` if this is a type parameter.
    const_ty: Option<tt::TopSubtree>,
    bounds: Option<tt::TopSubtree>,
}
pub(super) fn parse_adt_from_syntax(
    adt: &ast::Adt,
    tm: &span::SpanMap,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let (name, generic_param_list, where_clause, shape) = match &adt {
        ast::Adt::Struct(it) => (
            it.name(),
            it.generic_param_list(),
            it.where_clause(),
            AdtShape::Struct(VariantShape::from(call_site, tm, it.field_list())?),
        ),
        ast::Adt::Enum(it) => {
            let default_variant = it
                .variant_list()
                .into_iter()
                .flat_map(|it| it.variants())
                .position(|it| it.attrs().any(|it| it.simple_name() == Some("default".into())));
            (
                it.name(),
                it.generic_param_list(),
                it.where_clause(),
                AdtShape::Enum {
                    default_variant,
                    variants: it
                        .variant_list()
                        .into_iter()
                        .flat_map(|it| it.variants())
                        .map(|it| {
                            Ok((
                                name_to_token(call_site, tm, it.name())?,
                                VariantShape::from(call_site, tm, it.field_list())?,
                            ))
                        })
                        .collect::<Result<_, ExpandError>>()?,
                },
            )
        }
        ast::Adt::Union(it) => {
            (it.name(), it.generic_param_list(), it.where_clause(), AdtShape::Union)
        }
    };

    let to_tt = |node: &syntax::SyntaxNode| {
        syntax_bridge::reference::syntax_node_to_token_tree(
            node,
            tm,
            call_site,
            DocCommentDesugarMode::ProcMacro,
        )
    };
    let lifetimes = generic_param_list
        .iter()
        .flat_map(|param_list| param_list.lifetime_params())
        .filter_map(|param| Some((to_tt(param.syntax()), to_tt(param.lifetime()?.syntax()))))
        .collect();
    let mut param_type_set: HashSet<Name> = HashSet::default();
    let param_types = generic_param_list
        .into_iter()
        .flat_map(|param_list| param_list.type_or_const_params())
        .map(|param| {
            let name = {
                let this = param.name();
                match this {
                    Some(it) => {
                        param_type_set.insert(it.as_name());
                        syntax_bridge::reference::syntax_node_to_token_tree(
                            it.syntax(),
                            tm,
                            call_site,
                            DocCommentDesugarMode::ProcMacro,
                        )
                    }
                    None => {
                        tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site })
                    }
                }
            };
            let bounds = match &param {
                ast::TypeOrConstParam::Type(it) => it.type_bound_list().map(|it| {
                    syntax_bridge::reference::syntax_node_to_token_tree(
                        it.syntax(),
                        tm,
                        call_site,
                        DocCommentDesugarMode::ProcMacro,
                    )
                }),
                ast::TypeOrConstParam::Const(_) => None,
            };
            let const_ty = if let ast::TypeOrConstParam::Const(param) = param {
                let ty = param
                    .ty()
                    .map(|ty| {
                        syntax_bridge::reference::syntax_node_to_token_tree(
                            ty.syntax(),
                            tm,
                            call_site,
                            DocCommentDesugarMode::ProcMacro,
                        )
                    })
                    .unwrap_or_else(|| {
                        tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site })
                    });
                Some(ty)
            } else {
                None
            };
            AdtParam { name, const_ty, bounds }
        })
        .collect();

    let where_clause = if let Some(w) = where_clause {
        w.predicates()
            .map(|it| {
                syntax_bridge::reference::syntax_node_to_token_tree(
                    it.syntax(),
                    tm,
                    call_site,
                    DocCommentDesugarMode::ProcMacro,
                )
            })
            .collect()
    } else {
        vec![]
    };

    // For a generic parameter `T`, when shorthand associated type `T::Assoc` appears in field
    // types (of any variant for enums), we generate trait bound for it. It sounds reasonable to
    // also generate trait bound for qualified associated type `<T as Trait>::Assoc`, but rustc
    // does not do that for some unknown reason.
    //
    // See the analogous function in rustc [find_type_parameters()] and rust-lang/rust#50730.
    // [find_type_parameters()]: https://github.com/rust-lang/rust/blob/1.70.0/compiler/rustc_builtin_macros/src/deriving/generic/mod.rs#L378

    // It's cumbersome to deal with the distinct structures of ADTs, so let's just get untyped
    // `SyntaxNode` that contains fields and look for descendant `ast::PathType`s. Of note is that
    // we should not inspect `ast::PathType`s in parameter bounds and where clauses.
    let field_list = match adt {
        ast::Adt::Enum(it) => it.variant_list().map(|list| list.syntax().clone()),
        ast::Adt::Struct(it) => it.field_list().map(|list| list.syntax().clone()),
        ast::Adt::Union(it) => it.record_field_list().map(|list| list.syntax().clone()),
    };
    let associated_types = field_list
        .into_iter()
        .flat_map(|it| it.descendants())
        .filter_map(ast::PathType::cast)
        .filter_map(|p| {
            let name = p.path()?.qualifier()?.as_single_name_ref()?.as_name();
            param_type_set.contains(&name).then_some(p)
        })
        .map(|it| {
            syntax_bridge::reference::syntax_node_to_token_tree(
                it.syntax(),
                tm,
                call_site,
                DocCommentDesugarMode::ProcMacro,
            )
        })
        .collect();
    let name_token = name_to_token(call_site, tm, name)?;
    let packed = adt.attrs().any(|attr| {
        let text = attr.syntax().text().to_string();
        let mut words = text
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|word| !word.is_empty() && *word != "r");
        words.next() == Some("repr") && words.any(|word| word == "packed")
    });
    Ok(BasicAdtInfo {
        name: name_token,
        shape,
        param_types,
        lifetimes,
        where_clause,
        associated_types,
        packed,
    })
}
pub(super) fn name_to_token(
    call_site: Span,
    token_map: &ExpansionSpanMap,
    name: Option<ast::Name>,
) -> Result<tt::Ident, ExpandError> {
    let name = name.ok_or_else(|| {
        debug!("parsed item has no name");
        ExpandError::other(call_site, "missing name")
    })?;
    let span = token_map.span_at(name.syntax().text_range().start());

    let name_token = tt::Ident::new(name.text(), span);
    Ok(name_token)
}
pub(super) fn expand_simple_derive(
    db: &ItemSource<'_>,
    invoc_span: Span,
    tt: &tt::TopSubtree,
    trait_path: tt::TopSubtree,
    allow_unions: bool,
    copy_if_packed: bool,
    make_trait_body: impl FnOnce(&BasicAdtInfo) -> tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let mut info = match parse_adt(db, tt, invoc_span) {
        Ok(info) => info,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: invoc_span, close: invoc_span }),
                e,
            );
        }
    };
    if !allow_unions && matches!(info.shape, AdtShape::Union) {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(invoc_span)),
            ExpandError::builtin_derive(invoc_span, "this trait cannot be derived for unions"),
        );
    }
    if info.packed && copy_if_packed {
        // As rustc's (`needs_copy_as_bound_if_packed`): the fields are copied out, so what
        // their types are made of is `Copy` too.
        let krate = &dollar_crate(invoc_span);
        let types = info.param_types.iter().filter(|it| it.const_ty.is_none()).map(|it| &it.name);
        let copy: Vec<_> = types
            .chain(&info.associated_types)
            .map(|it| quote! {invoc_span => #it : #krate::marker::Copy })
            .collect();
        info.where_clause.extend(copy);
    }
    ExpandResult::ok(expand_simple_derive_with_parsed(
        invoc_span,
        info,
        trait_path,
        make_trait_body,
        true,
        tt::TopSubtree::empty(tt::DelimSpan::from_single(invoc_span)),
    ))
}

pub(super) fn expand_simple_derive_with_parsed(
    invoc_span: Span,
    info: BasicAdtInfo,
    trait_path: tt::TopSubtree,
    make_trait_body: impl FnOnce(&BasicAdtInfo) -> tt::TopSubtree,
    constrain_to_trait: bool,
    extra_impl_params: tt::TopSubtree,
) -> tt::TopSubtree {
    let trait_body = make_trait_body(&info);
    let mut where_block: Vec<_> =
        info.where_clause.into_iter().map(|w| quote! {invoc_span => #w , }).collect();
    let (params, args): (Vec<_>, Vec<_>) = info
        .param_types
        .into_iter()
        .map(|param| {
            let ident = param.name;
            if let Some(b) = param.bounds {
                let ident2 = ident.copy();
                where_block.push(quote! {invoc_span => #ident2 : #b , });
            }
            if let Some(ty) = param.const_ty {
                let ident2 = ident.copy();
                (quote! {invoc_span => const #ident : #ty , }, quote! {invoc_span => #ident2 , })
            } else {
                let bound = trait_path.copy();
                let ident2 = ident.copy();
                let param = if constrain_to_trait {
                    quote! {invoc_span => #ident : #bound , }
                } else {
                    quote! {invoc_span => #ident , }
                };
                (param, quote! {invoc_span => #ident2 , })
            }
        })
        .unzip();

    if constrain_to_trait {
        where_block.extend(info.associated_types.iter().map(|it| {
            let it = it.copy();
            let bound = trait_path.copy();
            quote! {invoc_span => #it : #bound , }
        }));
    }

    let (lifetime_params, lifetime_args): (Vec<_>, Vec<_>) = info
        .lifetimes
        .into_iter()
        .map(|(param, name)| (quote! {invoc_span => #param , }, quote! {invoc_span => #name , }))
        .unzip();
    let name = info.name;
    quote! {invoc_span =>
        impl < # #lifetime_params # #params #extra_impl_params > #trait_path for #name < # #lifetime_args # #args > where # #where_block { #trait_body }
    }
}

pub(super) fn copy_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = dollar_crate(span);
    expand_simple_derive(
        db,
        span,
        tt,
        quote! {span => #krate::marker::Copy },
        true,
        false,
        |_| quote! {span =>},
    )
}

/// `#[derive(ConstParamTy)]` (`adt_const_params`): the marker trait, named apart from the
/// macro in `core::marker`.
pub(super) fn const_param_ty_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = dollar_crate(span);
    expand_simple_derive(
        db,
        span,
        tt,
        quote! {span => #krate::marker::ConstParamTy_ },
        true,
        false,
        |_| quote! {span =>},
    )
}
pub(super) fn reborrow_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = dollar_crate(span);
    expand_reborrow_marker_derive(
        db,
        span,
        tt,
        quote! {span => #krate::marker::Reborrow },
        "Reborrow",
    )
}
pub(super) fn expand_reborrow_marker_derive(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
    trait_path: tt::TopSubtree,
    trait_name: &'static str,
) -> ExpandResult<tt::TopSubtree> {
    let info = match parse_adt(db, tt, span) {
        Ok(info) => info,
        Err(err) => {
            return ExpandResult::new(tt::TopSubtree::empty(tt::DelimSpan::from_single(span)), err);
        }
    };
    if !matches!(info.shape, AdtShape::Struct(_)) {
        return ExpandResult::new(
            tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
            ExpandError::builtin_derive(
                span,
                format!("`{trait_name}` can only be derived on `struct`s"),
            ),
        );
    }

    ExpandResult::ok(expand_simple_derive_with_parsed(
        span,
        info,
        trait_path,
        |_| quote! {span => },
        false,
        tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
    ))
}
pub(super) fn clone_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    clone_expand_with(db, span, tt, false)
}

/// `copy` is whether `Copy` is derived next to `Clone`, which makes cloning a copy for a type
/// that has no type parameters.
pub(super) fn clone_expand_with(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
    copy: bool,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    expand_simple_derive(db, span, tt, quote! {span => #krate::clone::Clone }, true, true, |adt| {
        let copy = copy && adt.param_types.iter().all(|param| param.const_ty.is_some());
        if copy || matches!(adt.shape, AdtShape::Union) {
            let star = tt::Punct { char: '*', spacing: ::tt::Spacing::Alone, span };
            return quote! {span =>
                fn clone(&self) -> Self {
                    #star self
                }
            };
        }
        if matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.is_empty()) {
            let star = tt::Punct { char: '*', spacing: ::tt::Spacing::Alone, span };
            return quote! {span =>
                fn clone(&self) -> Self {
                    match #star self {}
                }
            };
        }
        let name = &adt.name;
        let patterns = adt.shape.as_pattern(span, name);
        let exprs = adt.shape.as_pattern_map(
            name,
            |it| {
                let bound = bound_of_self(it);
                // A packed struct's field is a copy out of it, which is cloned by reference.
                if adt.packed {
                    quote! {span => #krate::clone::Clone::clone(&#bound) }
                } else {
                    quote! {span => #krate::clone::Clone::clone(#bound) }
                }
            },
            span,
        );
        let body = adt.match_self(span, patterns.into_iter().zip(exprs).collect());
        quote! {span =>
            fn clone(&self) -> Self {
                #body
            }
        }
    })
}

/// This function exists since `quote! {span => => }` doesn't work.
pub(super) fn fat_arrow(span: Span) -> tt::TopSubtree {
    let eq = tt::Punct { char: '=', spacing: ::tt::Spacing::Joint, span };
    quote! {span => #eq> }
}

/// This function exists since `quote! {span => && }` doesn't work.
pub(super) fn and_and(span: Span) -> tt::TopSubtree {
    let and = tt::Punct { char: '&', spacing: ::tt::Spacing::Joint, span };
    quote! {span => #and& }
}

/// As [`and_and`], for `==`.
pub(super) fn eq_eq(span: Span) -> tt::TopSubtree {
    let eq = tt::Punct { char: '=', spacing: ::tt::Spacing::Joint, span };
    quote! {span => #eq= }
}

pub(super) fn default_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    let adt = match parse_adt(db, tt, span) {
        Ok(info) => info,
        Err(e) => {
            return ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan { open: span, close: span }),
                e,
            );
        }
    };
    let (body, constrain_to_trait) = match &adt.shape {
        AdtShape::Struct(fields) => {
            let name = &adt.name;
            let body = fields.as_pattern_map(
                quote!(span =>#name),
                span,
                |_| quote!(span =>#krate::default::Default::default()),
            );
            (body, true)
        }
        AdtShape::Enum { default_variant, variants } => {
            if let Some(d) = default_variant {
                let (name, fields) = &variants[*d];
                let adt_name = &adt.name;
                let body = fields.as_pattern_map(
                    quote!(span =>#adt_name :: #name),
                    span,
                    |_| quote!(span =>#krate::default::Default::default()),
                );
                (body, false)
            } else {
                return ExpandResult::new(
                    tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
                    ExpandError::new(span, ExpandErrorKind::DefaultNoVariant),
                );
            }
        }
        AdtShape::Union => {
            return ExpandResult::new(
                tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
                ExpandError::builtin_derive(span, "this trait cannot be derived for unions"),
            );
        }
    };
    ExpandResult::ok(expand_simple_derive_with_parsed(
        span,
        adt,
        quote! {span => #krate::default::Default },
        |_adt| {
            quote! {span =>
                fn default() -> Self {
                    #body
                }
            }
        },
        constrain_to_trait,
        tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
    ))
}

pub(super) fn debug_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    expand_simple_derive(db, span, tt, quote! {span => #krate::fmt::Debug }, false, true, |adt| {
        let for_variant = |name: String, v: &VariantShape| match v {
            VariantShape::Struct(fields) => {
                // Spelled out in full, as rustc does: a method of the same name on some other
                // trait must not be picked up.
                let for_fields = fields.iter().enumerate().map(|(i, it)| {
                    let x_string = it.sym.as_str().to_owned();
                    let bound = bound_of_self(it);
                    let by = last_by_ref(adt, span, i + 1 == fields.len());
                    quote! {span =>
                        #krate::fmt::DebugStruct::field(&mut ra_expand_builder, #x_string, & #by #bound);
                    }
                });
                quote! {span => {
                    let mut ra_expand_builder = #krate::fmt::Formatter::debug_struct(f, #name);
                    # #for_fields
                    #krate::fmt::DebugStruct::finish(&mut ra_expand_builder)
                } }
            }
            VariantShape::Tuple(spans) => {
                let for_fields = tuple_field_iterator(spans).enumerate().map(|(i, it)| {
                    let bound = bound_of_self(&it);
                    let by = last_by_ref(adt, span, i + 1 == spans.len());
                    quote! {span =>
                        #krate::fmt::DebugTuple::field(&mut ra_expand_builder, & #by #bound);
                    }
                });
                quote! {span => {
                    let mut ra_expand_builder = #krate::fmt::Formatter::debug_tuple(f, #name);
                    # #for_fields
                    #krate::fmt::DebugTuple::finish(&mut ra_expand_builder)
                } }
            }
            VariantShape::Unit => quote! {span =>
                #krate::fmt::Formatter::write_str(f, #name)
            },
        };
        if matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.is_empty()) {
            let star = tt::Punct { char: '*', spacing: ::tt::Spacing::Alone, span };
            return quote! {span =>
                fn fmt(&self, f: &mut #krate::fmt::Formatter) -> #krate::fmt::Result {
                    match #star self {}
                }
            };
        }
        let arms = match &adt.shape {
            AdtShape::Struct(fields) => {
                let name = &adt.name;
                let pat = fields.as_pattern(quote!(span =>#name), span);
                let expr = for_variant(name.sym.as_str().to_owned(), fields);
                vec![(pat, expr)]
            }
            AdtShape::Enum { variants, .. } => variants
                .iter()
                .map(|(name, v)| {
                    let adt_name = &adt.name;
                    let pat = v.as_pattern(quote!(span =>#adt_name :: #name), span);
                    let expr = for_variant(name.sym.as_str().to_owned(), v);
                    (pat, expr)
                })
                .collect(),
            AdtShape::Union => unreachable!(),
        };
        let body = adt.match_self(span, arms);
        quote! {span =>
            fn fmt(&self, f: &mut #krate::fmt::Formatter) -> #krate::fmt::Result {
                #body
            }
        }
    })
}

/// The `&` that rustc's `Debug` puts before the last field of a packed struct, which may be
/// unsized: a `&dyn Debug` is made of a reference to the copy out of it.
pub(super) fn last_by_ref(adt: &BasicAdtInfo, span: Span, last: bool) -> tt::TopSubtree {
    if adt.packed && last {
        let and = tt::Punct { char: '&', spacing: ::tt::Spacing::Alone, span };
        quote! {span => #and }
    } else {
        quote! {span =>}
    }
}

pub(super) fn hash_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    expand_simple_derive(db, span, tt, quote! {span => #krate::hash::Hash }, false, true, |adt| {
        if matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.is_empty()) {
            let star = tt::Punct { char: '*', spacing: ::tt::Spacing::Alone, span };
            return quote! {span =>
                fn hash<H: #krate::hash::Hasher>(&self, ra_expand_state: &mut H) {
                    match #star self {}
                }
            };
        }
        let arms =
            adt.shape.as_pattern(span, &adt.name).into_iter().zip(adt.shape.field_names(span)).map(
                |(pat, names)| {
                    let expr = {
                        let it = names.iter().map(|it| {
                            let bound = bound_of_self(it);
                            quote! {span => #krate::hash::Hash::hash(&#bound, ra_expand_state); }
                        });
                        quote! {span => {
                            # #it
                        } }
                    };
                    (pat, expr)
                },
            );
        let body = adt.match_self(span, arms.collect());
        // rustc leaves the discriminant out when there is only one variant.
        let check_discriminant = if matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.len() > 1)
        {
            quote! {span => #krate::hash::Hash::hash(&#krate::mem::discriminant(self), ra_expand_state); }
        } else {
            quote! {span =>}
        };
        quote! {span =>
            fn hash<H: #krate::hash::Hasher>(&self, ra_expand_state: &mut H) {
                #check_discriminant
                #body
            }
        }
    })
}

pub(super) fn eq_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = dollar_crate(span);
    expand_simple_derive(
        db,
        span,
        tt,
        quote! {span => #krate::cmp::Eq },
        true,
        true,
        |_| quote! {span =>},
    )
}

/// rustc's `expand_deriving_partial_eq`: `StructuralPartialEq`, bounding no parameter and
/// written for a union too, then `PartialEq`.
pub(super) fn partial_eq_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let partial_eq = partial_eq_impl_expand(db, span, tt);
    let Ok(info) = parse_adt(db, tt, span) else { return partial_eq };
    let krate = &dollar_crate(span);
    let structural = expand_simple_derive_with_parsed(
        span,
        info,
        quote! {span => #krate::marker::StructuralPartialEq },
        |_| quote! {span =>},
        false,
        tt::TopSubtree::empty(tt::DelimSpan::from_single(span)),
    );
    let ExpandResult { value, err } = partial_eq;
    let partial_eq = value.token_trees();
    ExpandResult { value: quote! {span => #structural #partial_eq }, err }
}

fn partial_eq_impl_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    expand_simple_derive(
        db,
        span,
        tt,
        quote! {span => #krate::cmp::PartialEq },
        false,
        true,
        |adt| {
            let name = &adt.name;

            // As rustc's: an enum of several variants compares the discriminants, and then the
            // fields of the variants that have any. A match of each variant against itself is a
            // comparison for each, which LLVM leaves as a chain.
            let discriminants =
                matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.len() > 1);
            let (self_patterns, other_patterns) = self_and_other_patterns(adt, name, span);
            let arms = izip!(self_patterns, other_patterns, adt.shape.field_names(span));
            let arms = arms.filter(|(_, _, names)| !discriminants || !names.is_empty());
            let arms: Vec<_> = arms.collect();
            if discriminants {
                let fielded = !arms.is_empty();
                let arms = arms
                    .into_iter()
                    .map(|(pat1, pat2, names)| (pat1, pat2, fields_eq(krate, &names, span)))
                    .collect();
                let and_and = and_and(span);
                let fields = match fielded {
                    true => {
                        let fields = adt.match_both(span, arms, quote! {span => true });
                        quote! {span => #and_and #fields }
                    }
                    false => quote! {span =>},
                };
                let eq_eq = eq_eq(span);
                return quote! {span =>
                    fn eq(&self, other: &Self) -> bool {
                        let __self_discr = #krate::intrinsics::discriminant_value(self);
                        let __arg1_discr = #krate::intrinsics::discriminant_value(other);
                        __self_discr #eq_eq __arg1_discr #fields
                    }
                };
            }
            let arms = arms
                .into_iter()
                .map(|(pat1, pat2, names)| (pat1, pat2, fields_eq(krate, &names, span)))
                .collect();
            let body = adt.match_both(span, arms, quote! {span => false });
            quote! {span =>
                fn eq(&self, other: &Self) -> bool {
                    #body
                }
            }
        },
    )
}

/// Whether the fields `names` of `self`'s and `other`'s variant are equal, each with the next.
pub(super) fn fields_eq(krate: &tt::Ident, names: &[tt::Ident], span: Span) -> tt::TopSubtree {
    let Some((first, rest)) = names.split_first() else { return quote!(span =>true) };
    let eq = |it: &tt::Ident| {
        let t1 = tt::Ident::new(&format!("{}_self", it.sym), it.span);
        let t2 = tt::Ident::new(&format!("{}_other", it.sym), it.span);
        // By path, as rustc does: a type's own `eq` would be found first.
        quote!(span =>#krate::cmp::PartialEq::eq(&#t1, &#t2))
    };
    let first = eq(first);
    let rest = rest.iter().map(|it| {
        let and_and = and_and(span);
        let eq = eq(it);
        quote!(span =>#and_and #eq)
    });
    quote!(span =>#first # #rest)
}

pub(super) fn self_and_other_patterns(
    adt: &BasicAdtInfo,
    name: &tt::Ident,
    span: Span,
) -> (Vec<tt::TopSubtree>, Vec<tt::TopSubtree>) {
    let self_patterns = adt.shape.as_pattern_map(
        name,
        |it| {
            let t = bound_of_self(it);
            quote!(span =>#t)
        },
        span,
    );
    let other_patterns = adt.shape.as_pattern_map(
        name,
        |it| {
            let t = tt::Ident::new(&format!("{}_other", it.sym), it.span);
            quote!(span =>#t)
        },
        span,
    );
    (self_patterns, other_patterns)
}

pub(super) fn ord_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    expand_simple_derive(db, span, tt, quote! {span => #krate::cmp::Ord }, false, true, |adt| {
        fn compare(
            krate: &tt::Ident,
            left: tt::TopSubtree,
            right: tt::TopSubtree,
            rest: tt::TopSubtree,
            span: Span,
        ) -> tt::TopSubtree {
            let fat_arrow1 = fat_arrow(span);
            let fat_arrow2 = fat_arrow(span);
            quote! {span =>
                match #krate::cmp::Ord::cmp(&#left, &#right) {
                    #krate::cmp::Ordering::Equal #fat_arrow1 {
                        #rest
                    }
                    c #fat_arrow2 return c,
                }
            }
        }
        let (self_patterns, other_patterns) = self_and_other_patterns(adt, &adt.name, span);
        // As rustc's: an enum of several variants compares the discriminants first, and a
        // variant without fields is then equal.
        let discriminants =
            matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.len() > 1);
        let arms = izip!(self_patterns, other_patterns, adt.shape.field_names(span));
        let arms = arms.filter(|(_, _, fields)| !discriminants || !fields.is_empty()).map(
            |(pat1, pat2, fields)| {
                let mut body = quote!(span =>#krate::cmp::Ordering::Equal);
                for f in fields.into_iter().rev() {
                    let t1 = tt::Ident::new(&format!("{}_self", f.sym), f.span);
                    let t2 = tt::Ident::new(&format!("{}_other", f.sym), f.span);
                    body = compare(krate, quote!(span =>#t1), quote!(span =>#t2), body, span);
                }
                (pat1, pat2, body)
            },
        );
        let mut body =
            adt.match_both(span, arms.collect(), quote! {span => #krate::cmp::Ordering::Equal });
        if discriminants {
            let left = quote!(span =>#krate::intrinsics::discriminant_value(self));
            let right = quote!(span =>#krate::intrinsics::discriminant_value(other));
            body = compare(krate, left, right, body, span);
        }
        quote! {span =>
            fn cmp(&self, other: &Self) -> #krate::cmp::Ordering {
                #body
            }
        }
    })
}

pub(super) fn partial_ord_expand(
    db: &ItemSource<'_>,
    span: Span,
    tt: &tt::TopSubtree,
) -> ExpandResult<tt::TopSubtree> {
    let krate = &dollar_crate(span);
    expand_simple_derive(
        db,
        span,
        tt,
        quote! {span => #krate::cmp::PartialOrd },
        false,
        true,
        |adt| {
            fn compare(
                krate: &tt::Ident,
                left: tt::TopSubtree,
                right: tt::TopSubtree,
                rest: tt::TopSubtree,
                span: Span,
            ) -> tt::TopSubtree {
                let fat_arrow1 = fat_arrow(span);
                let fat_arrow2 = fat_arrow(span);
                quote! {span =>
                    match #krate::cmp::PartialOrd::partial_cmp(&#left, &#right) {
                        #krate::option::Option::Some(#krate::cmp::Ordering::Equal) #fat_arrow1 {
                            #rest
                        }
                        c #fat_arrow2 return c,
                    }
                }
            }
            let left = quote!(span =>#krate::intrinsics::discriminant_value(self));
            let right = quote!(span =>#krate::intrinsics::discriminant_value(other));

            let (self_patterns, other_patterns) = self_and_other_patterns(adt, &adt.name, span);
            // As rustc's: an enum of several variants compares the discriminants first, and a
            // variant without fields is then equal. A struct compares its fields alone.
            let discriminants =
                matches!(&adt.shape, AdtShape::Enum { variants, .. } if variants.len() > 1);
            let arms = izip!(self_patterns, other_patterns, adt.shape.field_names(span));
            let arms = arms.filter(|(_, _, fields)| !discriminants || !fields.is_empty()).map(
                |(pat1, pat2, fields)| {
                    let mut body =
                        quote!(span =>#krate::option::Option::Some(#krate::cmp::Ordering::Equal));
                    for f in fields.into_iter().rev() {
                        let t1 = tt::Ident::new(&format!("{}_self", f.sym), f.span);
                        let t2 = tt::Ident::new(&format!("{}_other", f.sym), f.span);
                        body = compare(krate, quote!(span =>#t1), quote!(span =>#t2), body, span);
                    }
                    (pat1, pat2, body)
                },
            );
            let equal =
                quote! {span => #krate::option::Option::Some(#krate::cmp::Ordering::Equal) };
            let matched = adt.match_both(span, arms.collect(), equal);
            let body = match discriminants {
                true => compare(krate, left, right, matched, span),
                false => matched,
            };
            quote! {span =>
                fn partial_cmp(&self, other: &Self) -> #krate::option::Option<#krate::cmp::Ordering> {
                    #body
                }
            }
        },
    )
}

/// `From` and `CoerceShared`'s target reading the derive's own item through handles, as before
/// they read it in place. Their parse is `editor.rs`' handle parse.
pub(super) mod handle {
    use blaze_mem::Frame;
    use smallvec::SmallVec;
    use span::Span;
    use syntax::ast::{self, AstNode, FieldList, HasAttrs, HasName};
    use syntax_bridge::DocCommentDesugarMode;

    use crate::{
        BuiltinError, ExpandError, ExpandResult,
        builtin::{
            derive_macro::{
                FieldsKind, Variant, editor::parse_adt_from_handle, empty, in_group, write_derive,
                write_impl,
            },
            quote::{dollar_crate, group},
        },
        tt,
    };

    /// `#[derive(From)]` of the item read through handles.
    pub(in crate::builtin::derive_macro) fn from_derive<'f>(
        frame: &'f Frame,
        span: Span,
        adt: &ast::Adt,
        span_map: &span::SpanMap,
    ) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
        let refuse = |message: &str| {
            let name = adt.name().map(|it| span_map.span_at(it.syntax().text_range().start()));
            let error = BuiltinError::derive(message)
                .note("`#[derive(From)]` can only be used on structs with exactly one field")
                .also_at(name.into_iter().collect::<Box<[_]>>());
            ExpandResult::new(empty(frame, span), error.at(span))
        };
        let strukt = match adt {
            ast::Adt::Struct(it) => it,
            ast::Adt::Enum(_) => return refuse("`#[derive(From)]` used on an enum"),
            ast::Adt::Union(_) => return refuse("`#[derive(From)]` used on a union"),
        };
        let mut field_types: SmallVec<[Option<ast::Type>; 2]> = SmallVec::new();
        match strukt.field_list() {
            Some(FieldList::RecordFieldList(it)) => {
                field_types.extend(it.fields().map(|it| it.ty()).take(2))
            }
            Some(FieldList::TupleFieldList(it)) => {
                field_types.extend(it.fields().map(|it| it.ty()).take(2))
            }
            None => {}
        }
        let field_ty = match &field_types[..] {
            [Some(ty)] => syntax_bridge::reference::syntax_node_to_token_tree(
                ty.syntax(),
                span_map,
                span,
                DocCommentDesugarMode::ProcMacro,
            ),
            [None] => {
                let err = ExpandError::other(span, "missing field type");
                return ExpandResult::new(empty(frame, span), err);
            }
            [] => return refuse("`#[derive(From)]` used on a struct with no fields"),
            _ => return refuse("`#[derive(From)]` used on a struct with multiple fields"),
        };
        let info = match parse_adt_from_handle(adt, span_map, span) {
            Ok(it) => it,
            Err(err) => return ExpandResult::new(empty(frame, span), err),
        };
        let krate = &dollar_crate(span);
        let field_ty = &field_ty;
        let record_field = match info.shape.variants.first() {
            Some(Variant { kind: FieldsKind::Record, .. }) => info.shape.fields.first(),
            _ => None,
        };
        let body = group(span, |b| match record_field {
            Some(field) => quote_into!(b, span => Self { #field : value }),
            None => quote_into!(b, span => Self ( value )),
        });
        let trait_path =
            group(span, |b| quote_into!(b, span => #krate::convert::From<#field_ty>));
        let no_extra_params = group(span, |_| {});
        ExpandResult::ok(write_derive(frame, span, |b| {
            write_impl(b, span, &info, &trait_path, false, &no_extra_params, |b| {
                in_group(b, span, |b| {
                    quote_into!(b, span =>
                        #[inline]
                        fn from(value : #field_ty) -> Self { #body }
                    )
                })
            })
        }))
    }

    /// The tokens of the struct's one `#[coerce_shared(Target)]`, read through handles.
    pub(in crate::builtin::derive_macro) fn coerce_shared_target_tokens(
        span: Span,
        strukt: &ast::Struct,
    ) -> Option<tt::TopSubtree> {
        let mut attrs = strukt
            .attrs()
            .filter(|attr| attr.as_simple_call().is_some_and(|(name, _)| name == "coerce_shared"));
        let attr = attrs.next()?;
        if attrs.next().is_some() {
            return None;
        }
        let (_, target_tokens) = attr.as_simple_call()?;
        let l_paren = target_tokens.l_paren_token()?;
        let r_paren = target_tokens.r_paren_token()?;

        let mut span_map = span::SpanMap::empty();
        span_map.push(target_tokens.syntax().text_range().start(), span);
        span_map.push(target_tokens.syntax().text_range().end(), span);

        let remove = syntax_bridge::Removed::<_>::from_iter([l_paren.into(), r_paren.into()]);
        Some(syntax_bridge::reference::syntax_node_to_token_tree_modified(
            target_tokens.syntax(),
            &span_map,
            syntax_bridge::Appended::<_, std::alloc::Global>::default(),
            remove,
            span,
            DocCommentDesugarMode::ProcMacro,
            |_, _| (true, syntax_bridge::Inserted::new()),
        ))
    }
}
