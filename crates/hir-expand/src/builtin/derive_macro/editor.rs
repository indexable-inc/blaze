//! `CoercePointee`'s expansion as it was through a `SyntaxEditor`, the reference its token build
//! is checked against (`coerce_pointee_matches_the_editor`): the derive's item read through
//! handles and copied into an editor, whose `where` clause `make` writes from text, every token
//! at the derive's span. The handle parse of the edited copy is here too: the view parse cannot
//! read a tree the editor built.

use blaze_mem::Frame;
use either::Either;
use itertools::Itertools;
use parser::SyntaxKind;
use smallvec::SmallVec;
use span::Span;
use syntax::{
    TextRange, TextSize,
    ast::{
        self, AstNode, FieldList, HasAttrs, HasGenericArgs, HasGenericParams, HasName,
        HasTypeBounds,
    },
    syntax_editor::{GetOrCreateWhereClause, SyntaxEditor},
};
use syntax_bridge::DocCommentDesugarMode;
use tracing::debug;

use super::{
    AdtKind, AdtParam, AdtShape, BasicAdtInfo, FieldsKind, Variant, empty, len_u32,
    tuple_field_symbol, write_coerce_pointee_impls,
};
use crate::{
    BuiltinError, ExpandError, ExpandResult,
    name::{AsName, Name},
    span_map::ExpansionSpanMap,
    tt,
};

/// Whether the words of `attr`'s text (its runs of letters, digits and `_`, leaving out those in
/// `skip`) start with `head` and then have `word`. A word never spans two tokens, so the tokens
/// are split one by one and nothing is copied.
pub(super) fn attr_words_have(attr: &ast::Attr, head: &str, skip: &[&str], word: &str) -> bool {
    let mut first = true;
    for token in attr.syntax().descendants_with_tokens().filter_map(|it| it.into_token()) {
        for it in token.text().split(|c: char| !c.is_alphanumeric() && c != '_') {
            if it.is_empty() || skip.contains(&it) {
                continue;
            }
            if first {
                if it != head {
                    return false;
                }
                first = false;
            } else if it == word {
                return true;
            }
        }
    }
    false
}

/// `#[derive(CoercePointee)]` of the item: a `SyntaxEditor` copy of it, with the new `where`
/// predicates made by `make`.
pub(super) fn coerce_pointee_derive<'f>(
    frame: &'f Frame,
    span: Span,
    adt: &ast::Adt,
    span_map: &span::SpanMap,
) -> ExpandResult<tt::TopSubtree<&'f Frame>> {
    // The editor's copy of the item starts at 0, the item in `span_map` where it was parsed.
    let base = adt.syntax().text_range().start();
    let span_of = |range: TextRange| {
        let first = span_map.span_at(range.start() + base);
        match range.end().checked_sub(TextSize::new(1)).filter(|_| !range.is_empty()) {
            Some(last) => first.cover(span_map.span_at(last + base)),
            None => first,
        }
    };
    let (editor, adt) = SyntaxEditor::with_ast_node(adt);
    let make = editor.make();
    let refuse = |error: BuiltinError, at: Span| {
        ExpandResult::new(empty(frame, span), error.at(at))
    };
    // rustc's `CoercePointee` errors are E0802.
    fn pointee_error(message: impl Into<Box<str>>) -> BuiltinError {
        BuiltinError::derive(message).code(802)
    }
    let ast::Adt::Struct(strukt) = &adt else {
        return refuse(
            pointee_error(
                "`CoercePointee` can only be derived on `struct`s with `#[repr(transparent)]`",
            ),
            span,
        );
    };
    let has_at_least_one_field = strukt
        .field_list()
        .map(|it| match it {
            ast::FieldList::RecordFieldList(it) => it.fields().next().is_some(),
            ast::FieldList::TupleFieldList(it) => it.fields().next().is_some(),
        })
        .unwrap_or(false);
    if !has_at_least_one_field {
        return refuse(
            pointee_error(
                "`CoercePointee` can only be derived on `struct`s with at least one field",
            ),
            span,
        );
    }
    let type_params = strukt
        .generic_param_list()
        .into_iter()
        .flat_map(|generics| {
            generics.generic_params().filter_map(|param| match param {
                ast::GenericParam::TypeParam(param) => Some(param),
                _ => None,
            })
        })
        .collect_vec();
    if type_params.is_empty() {
        return refuse(
            pointee_error(
                "`CoercePointee` can only be derived on `struct`s that are generic over at least one type",
            ),
            span,
        );
    }
    let (pointee_param, pointee_param_idx) = if type_params.len() == 1 {
        // Regardless of the only type param being designed as `#[pointee]` or not, we can just use it as such.
        (type_params[0].clone(), 0) // clone: `CoercePointee` only, rare: node handles and its edit's lists
    } else {
        let mut pointees = type_params.iter().cloned().enumerate().filter(|(_, param)| {
            param.attrs().any(|attr| {
                let is_pointee = attr.as_simple_atom().is_some_and(|name| name == "pointee");
                if is_pointee {
                    // Remove the `#[pointee]` attribute so it won't be present in the generated
                    // impls (where we cannot resolve it).
                    editor.delete(attr.syntax());
                }
                is_pointee
            })
        });
        match (pointees.next(), pointees.next()) {
            (Some((pointee_idx, pointee)), None) => (pointee, pointee_idx),
            (None, _) => {
                return refuse(
                    pointee_error(
                        "exactly one generic type parameter must be marked \
                                as `#[pointee]` to derive `CoercePointee` traits",
                    ),
                    span,
                );
            }
            (Some((_, one)), Some(_)) => {
                return refuse(
                    pointee_error(
                        "only one type parameter can be marked as `#[pointee]` \
                                when deriving `CoercePointee` traits",
                    ),
                    type_param_span(&one).map_or(span, span_of),
                );
            }
        }
    };
    let (Some(struct_name), Some(pointee_param_name)) = (strukt.name(), pointee_param.name())
    else {
        return ExpandResult::new(
            tt::TopSubtree::empty_in(tt::DelimSpan::from_single(span), frame),
            ExpandError::other(span, "invalid item"),
        );
    };

    {
        let mut pointee_has_maybe_sized_bound = false;
        if let Some(bounds) = pointee_param.type_bound_list() {
            pointee_has_maybe_sized_bound |= bounds.bounds().any(is_maybe_sized_bound);
        }
        if let Some(where_clause) = strukt.where_clause() {
            pointee_has_maybe_sized_bound |= where_clause.predicates().any(|pred| {
                let Some(ast::Type::PathType(ty)) = pred.ty() else { return false };
                let is_not_pointee = ty.path().is_none_or(|path| {
                    let is_pointee = path
                        .as_single_name_ref()
                        .is_some_and(|name| name.text() == pointee_param_name.text());
                    !is_pointee
                });
                if is_not_pointee {
                    return false;
                }
                pred.type_bound_list()
                    .is_some_and(|bounds| bounds.bounds().any(is_maybe_sized_bound))
            })
        }
        if !pointee_has_maybe_sized_bound {
            return refuse(
                // alloc: an error's message, on the error path
                pointee_error(format!(
                    "`derive(CoercePointee)` requires `{pointee_param_name}` to be marked `?Sized`"
                )),
                span_of(pointee_param_name.syntax().text_range()),
            );
        }
    }
    // rustc checks this of the impl the derive writes, in coherence
    // (`visit_implementation_of_coerce_pointee_validity`): only once every check above passed.
    let is_repr_transparent = strukt.attrs().any(|attr| {
        attr.as_simple_call().is_some_and(|(name, tt)| {
            name == "repr"
                && tt.syntax().children_with_tokens().any(|it| {
                    it.into_token().is_some_and(|it| {
                        it.kind() == SyntaxKind::IDENT && it.text() == "transparent"
                    })
                })
        })
    });
    if !is_repr_transparent {
        return refuse(
            pointee_error(
                "`derive(CoercePointee)` is only applicable to `struct` with `repr(transparent)` layout",
            ),
            span,
        );
    }

    const ADDED_PARAM: &str = "__S";

    let mut new_predicates: Vec<ast::WherePred> = Vec::new(); // clone: `CoercePointee` only, rare: node handles and its edit's lists

    {
        // # Rewrite generic parameter bounds
        // For each bound `U: ..` in `struct<U: ..>`, make a new bound with `__S` in place of `#[pointee]`
        // Example:
        // ```
        // struct<
        //     U: Trait<T>,
        //     #[pointee] T: Trait<T> + ?Sized,
        //     V: Trait<T>> ...
        // ```
        // ... generates this `impl` generic parameters
        // ```
        // impl<
        //     U: Trait<T>,
        //     T: Trait<T> + ?Sized,
        //     V: Trait<T>
        // >
        // where
        //     U: Trait<__S>,
        //     __S: Trait<__S> + ?Sized,
        //     V: Trait<__S> ...
        // ```
        for param in &type_params {
            let Some(param_name) = param.name() else { continue };
            if let Some(bounds) = param.type_bound_list() {
                // If the target type is the pointee, duplicate the bound as whole.
                // Otherwise, duplicate only bounds that mention the pointee.
                let is_pointee = param_name.text() == pointee_param_name.text();
                let new_bounds = bounds.bounds().filter_map(|bound| {
                    let new_bound = substitute_type_bound(
                        bound.clone(), // clone: `CoercePointee` only, rare: node handles and its edit's lists
                        pointee_param_name.text(),
                        ADDED_PARAM,
                    );

                    if is_pointee {
                        return new_bound.or(Some(bound));
                    }
                    new_bound
                });

                let new_bounds_target = if is_pointee {
                    make.name_ref(ADDED_PARAM)
                } else {
                    make.name_ref(param_name.text())
                };
                new_predicates.push(make.where_pred(
                    Either::Right(
                        make.ty_path_from_segments([make.path_segment(new_bounds_target)], false),
                    ),
                    new_bounds,
                ));
            }
        }

        // # Rewrite `where` clauses
        //
        // Move on to `where` clauses.
        // Example:
        // ```
        // struct MyPointer<#[pointee] T, ..>
        // where
        //   U: Trait<V> + Trait<T>,
        //   Companion<T>: Trait<T>,
        //   T: Trait<T> + ?Sized,
        // { .. }
        // ```
        // ... will have a impl prelude like so
        // ```
        // impl<..> ..
        // where
        //   U: Trait<V> + Trait<T>,
        //   U: Trait<__S>,
        //   Companion<T>: Trait<T>,
        //   Companion<__S>: Trait<__S>,
        //   T: Trait<T> + ?Sized,
        //   __S: Trait<__S> + ?Sized,
        // ```
        //
        // We should also write a few new `where` bounds from `#[pointee] T` to `__S`
        // as well as any bound that indirectly involves the `#[pointee] T` type.
        for predicate in strukt.where_clause().into_iter().flat_map(|wc| wc.predicates()) {
            let Some(pred_target) = predicate.ty() else { continue };

            // If the target type references the pointee, duplicate the bound as whole.
            // Otherwise, duplicate only bounds that mention the pointee.
            if let Some(predicate_with_substituted_target) =
                substitute_where_pred(&predicate, pointee_param_name.text(), ADDED_PARAM)
            {
                new_predicates.push(predicate_with_substituted_target);
            } else if let Some(bounds) = predicate.type_bound_list() {
                let new_bounds = bounds.bounds().filter_map(|bound| {
                    substitute_type_bound(bound, pointee_param_name.text(), ADDED_PARAM)
                });
                new_predicates.push(make.where_pred(Either::Right(pred_target), new_bounds));
            }
        }
    }

    {
        // # Add `Unsize<__S>` bound to `#[pointee]` at the generic parameter location
        //
        // Find the `#[pointee]` parameter and add an `Unsize<__S>` bound to it.
        new_predicates.push(
            make.where_pred(
                Either::Right(make.ty_path_from_segments(
                    [make.path_segment(make.name_ref(pointee_param_name.text()))],
                    false,
                )),
                [make.type_bound(
                    make.ty_path_from_segments(
                        [
                            make.path_segment(make.name_ref("core")),
                            make.path_segment(make.name_ref("marker")),
                            make.generic_ty_path_segment(
                                make.name_ref("Unsize"),
                                [make
                                    .type_arg(make.ty_path_from_segments(
                                        [make.path_segment(make.name_ref(ADDED_PARAM))],
                                        false,
                                    ))
                                    .into()],
                            ),
                        ],
                        true,
                    ),
                )],
            ),
        );
    }

    let self_for_traits = {
        // Replace the `#[pointee]` with `__S`.
        let mut type_param_idx = 0;
        let self_params_for_traits = strukt
            .generic_param_list()
            .into_iter()
            .flat_map(|params| params.generic_params())
            .filter_map(|param| {
                Some(match param {
                    ast::GenericParam::ConstParam(param) => {
                        ast::GenericArg::ConstArg(make.expr_const_value(param.name()?.text()))
                    }
                    ast::GenericParam::LifetimeParam(param) => {
                        make.lifetime_arg(param.lifetime()?).into()
                    }
                    ast::GenericParam::TypeParam(param) => {
                        let name = if pointee_param_idx == type_param_idx {
                            make.name_ref(ADDED_PARAM)
                        } else {
                            make.name_ref(param.name()?.text())
                        };
                        type_param_idx += 1;
                        make.type_arg(make.ty_path_from_segments([make.path_segment(name)], false))
                            .into()
                    }
                })
            });

        make.path_from_segments(
            [make.generic_ty_path_segment(
                make.name_ref(struct_name.text()),
                self_params_for_traits,
            )],
            false,
        )
    };

    strukt.get_or_create_where_clause(&editor, new_predicates.into_iter());
    let edit = editor.finish();
    let strukt = ast::Struct::cast(edit.new_root().clone()).unwrap(); // clone: `CoercePointee` only, rare: node handles and its edit's lists
    let adt = ast::Adt::Struct(strukt.clone()); // clone: `CoercePointee` only, rare: node handles and its edit's lists

    let mut span_map = span::SpanMap::empty();
    // One span for them all.
    span_map.push(adt.syntax().text_range().end(), span);

    let self_for_traits = syntax_bridge::reference::syntax_node_to_token_tree(
        self_for_traits.syntax(),
        &span_map,
        span,
        DocCommentDesugarMode::ProcMacro,
    );
    let info = match parse_adt_from_handle(&adt, &span_map, span) {
        Ok(it) => it,
        Err(err) => {
            return ExpandResult::new(empty(frame, span), err);
        }
    };

    return ExpandResult::ok(write_coerce_pointee_impls(frame, span, &info, &self_for_traits));

    fn is_maybe_sized_bound(bound: ast::TypeBound) -> bool {
        if bound.question_mark_token().is_none() {
            return false;
        }
        let Some(ast::Type::PathType(ty)) = bound.ty() else {
            return false;
        };
        let Some(path) = ty.path() else {
            return false;
        };
        return segments_eq(&path, &["Sized"])
            || segments_eq(&path, &["core", "marker", "Sized"])
            || segments_eq(&path, &["std", "marker", "Sized"]);

        fn segments_eq(path: &ast::Path, expected: &[&str]) -> bool {
            path.segments().zip_longest(expected.iter().copied()).all(|value| {
                value.both().is_some_and(|(segment, expected)| {
                    segment.name_ref().is_some_and(|name| name.text() == expected)
                })
            })
        }
    }

    /// Returns true if any substitution was performed.
    fn substitute_type_bound(
        bound: ast::TypeBound,
        param_name: &str,
        replacement: &str,
    ) -> Option<ast::TypeBound> {
        let (editor, bound) = SyntaxEditor::with_ast_node(&bound);
        let substituted = bound
            .ty()
            .is_some_and(|ty| substitute_type_in_bound(&editor, ty, param_name, replacement));
        if !substituted {
            return None;
        }

        let edit = editor.finish();
        Some(ast::TypeBound::cast(edit.new_root().clone()).unwrap()) // clone: `CoercePointee` only, rare: node handles and its edit's lists
    }

    fn substitute_where_pred(
        predicate: &ast::WherePred,
        param_name: &str,
        replacement: &str,
    ) -> Option<ast::WherePred> {
        let (editor, predicate) = SyntaxEditor::with_ast_node(predicate);
        let substituted = predicate
            .ty()
            .is_some_and(|ty| substitute_type_in_bound(&editor, ty, param_name, replacement));
        if substituted && let Some(bounds) = predicate.type_bound_list() {
            for bound in bounds.bounds() {
                if let Some(ty) = bound.ty() {
                    substitute_type_in_bound(&editor, ty, param_name, replacement);
                }
            }
        }
        if !substituted {
            return None;
        }

        let edit = editor.finish();
        Some(ast::WherePred::cast(edit.new_root().clone()).unwrap()) // clone: `CoercePointee` only, rare: node handles and its edit's lists
    }

    fn substitute_type_in_bound(
        editor: &SyntaxEditor,
        ty: ast::Type,
        param_name: &str,
        replacement: &str,
    ) -> bool {
        let make = editor.make();
        return match ty {
            ast::Type::ArrayType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::DynTraitType(ty) => {
                go_bounds(editor, ty.type_bound_list(), param_name, replacement)
            }
            ast::Type::FnPtrType(ty) => any_long(
                ty.param_list()
                    .into_iter()
                    .flat_map(|params| params.params().filter_map(|param| param.ty()))
                    .chain(ty.ret_type().and_then(|it| it.ty())),
                |ty| substitute_type_in_bound(editor, ty, param_name, replacement),
            ),
            ast::Type::ForType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::ImplTraitType(ty) => {
                go_bounds(editor, ty.type_bound_list(), param_name, replacement)
            }
            ast::Type::ParenType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::PathType(ty) => ty.path().is_some_and(|path| {
                if path.as_single_name_ref().is_some_and(|name| name.text() == param_name) {
                    editor.replace(
                        path.syntax(),
                        make.path_from_segments(
                            [make.path_segment(make.name_ref(replacement))],
                            false,
                        )
                        .syntax(),
                    );
                    return true;
                }

                any_long(
                    path.segments()
                        .filter_map(|segment| segment.generic_arg_list())
                        .flat_map(|it| it.generic_args())
                        .filter_map(|generic_arg| match generic_arg {
                            ast::GenericArg::TypeArg(ty) => ty.ty(),
                            _ => None,
                        }),
                    |ty| substitute_type_in_bound(editor, ty, param_name, replacement),
                )
            }),
            ast::Type::PtrType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::RefType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::SliceType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::TupleType(ty) => any_long(ty.fields(), |ty| {
                substitute_type_in_bound(editor, ty, param_name, replacement)
            }),
            ast::Type::PatternType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::UnsafeBinderType(ty) => ty
                .ty()
                .is_some_and(|ty| substitute_type_in_bound(editor, ty, param_name, replacement)),
            ast::Type::InferType(_) | ast::Type::MacroType(_) | ast::Type::NeverType(_) => false,
        };

        fn go_bounds(
            editor: &SyntaxEditor,
            bounds: Option<ast::TypeBoundList>,
            param_name: &str,
            replacement: &str,
        ) -> bool {
            bounds.is_some_and(|bounds| {
                any_long(bounds.bounds(), |bound| {
                    bound.ty().is_some_and(|ty| {
                        substitute_type_in_bound(editor, ty, param_name, replacement)
                    })
                })
            })
        }

        /// Like [`Iterator::any()`], but not short-circuiting.
        fn any_long<I: Iterator, F: FnMut(I::Item) -> bool>(iter: I, mut f: F) -> bool {
            let mut result = false;
            iter.for_each(|item| result |= f(item));
            result
        }
    }
}

/// rustc's `GenericParam::span` of a type parameter: its name, to the end of its default if it
/// has one.
fn type_param_span(param: &ast::TypeParam) -> Option<TextRange> {
    let name = param.name()?.syntax().text_range();
    Some(match param.default_type() {
        Some(ty) => name.cover(ty.syntax().text_range()),
        None => name,
    })
}

/// Pushes the fields of `list` to `fields`, and says how they are written.
fn push_fields(
    fields: &mut Vec<tt::Ident>,
    call_site: Span,
    tm: &ExpansionSpanMap,
    list: Option<FieldList>,
) -> Result<FieldsKind, ExpandError> {
    let at_field = |span: Span| Span { ctx: call_site.ctx, ..span };
    Ok(match list {
        None => FieldsKind::Unit,
        Some(FieldList::RecordFieldList(it)) => {
            fields.reserve(it.fields().count());
            for field in it.fields() {
                let name = name_to_token(call_site, tm, field.name())?;
                fields.push(tt::Ident { span: at_field(name.span), ..name });
            }
            FieldsKind::Record
        }
        Some(FieldList::TupleFieldList(it)) => {
            fields.reserve(it.fields().count());
            for (i, field) in it.fields().enumerate() {
                let at = field
                    .ty()
                    .map_or_else(|| field.syntax().text_range(), |ty| ty.syntax().text_range());
                fields.push(tt::Ident {
                    sym: tuple_field_symbol(i),
                    span: at_field(tm.span_at(at.start())),
                    is_raw: tt::IdentIsRaw::No,
                });
            }
            FieldsKind::Tuple
        }
    })
}

/// The struct, enum or union read through handles: `CoercePointee`'s edited copy.
pub(super) fn parse_adt_from_handle(
    adt: &ast::Adt,
    tm: &span::SpanMap,
    call_site: Span,
) -> Result<BasicAdtInfo, ExpandError> {
    let mut fields = Vec::new(); // alloc: the shape's field list, one per derive (each list reserved in `push_fields`)
    let mut variants = Vec::new(); // alloc: the shape's variant list, one per derive
    // A struct's one variant takes its name, which is read last, as the errors are.
    let mut struct_fields = None;
    let (name, generic_param_list, where_clause, kind) = match &adt {
        ast::Adt::Struct(it) => {
            struct_fields = Some(push_fields(&mut fields, call_site, tm, it.field_list())?);
            (it.name(), it.generic_param_list(), it.where_clause(), AdtKind::Struct)
        }
        ast::Adt::Enum(it) => {
            let mut default_variant = None;
            for (i, variant) in it.variant_list().into_iter().flat_map(|it| it.variants()).enumerate()
            {
                if default_variant.is_none()
                    && variant.attrs().any(|it| it.simple_name() == Some("default".into()))
                {
                    default_variant = Some(i);
                }
                let name = name_to_token(call_site, tm, variant.name())?;
                let start = len_u32(fields.len());
                let kind = push_fields(&mut fields, call_site, tm, variant.field_list())?;
                variants.push(Variant { name, kind, start, end: len_u32(fields.len()) });
            }
            (
                it.name(),
                it.generic_param_list(),
                it.where_clause(),
                AdtKind::Enum { default_variant },
            )
        }
        ast::Adt::Union(it) => {
            (it.name(), it.generic_param_list(), it.where_clause(), AdtKind::Union)
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
        .collect(); // alloc: one tree per lifetime parameter; none without generics
    let mut param_names: SmallVec<[Name; 4]> = SmallVec::new();
    let param_types = generic_param_list
        .into_iter()
        .flat_map(|param_list| param_list.type_or_const_params())
        .map(|param| {
            let name = match param.name() {
                Some(it) => {
                    let name = it.as_name();
                    if !param_names.contains(&name) {
                        param_names.push(name);
                    }
                    to_tt(it.syntax())
                }
                None => tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site }),
            };
            let bounds = match &param {
                ast::TypeOrConstParam::Type(it) => it.type_bound_list().map(|it| to_tt(it.syntax())),
                ast::TypeOrConstParam::Const(_) => None,
            };
            let const_ty = if let ast::TypeOrConstParam::Const(param) = param {
                let ty = param.ty().map(|ty| to_tt(ty.syntax())).unwrap_or_else(|| {
                    tt::TopSubtree::empty(::tt::DelimSpan { open: call_site, close: call_site })
                });
                Some(ty)
            } else {
                None
            };
            AdtParam { name, const_ty, bounds }
        })
        .collect(); // alloc: one entry per type or const parameter; none without generics

    let where_clause = match where_clause {
        Some(w) => w.predicates().map(|it| to_tt(it.syntax())).collect(), // alloc: one tree per where predicate
        None => Vec::new(), // alloc: none, an empty `Vec`
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
    // we should not inspect `ast::PathType`s in parameter bounds and where clauses. A type
    // without type parameters has none, and its fields aren't walked.
    let associated_types = if param_names.is_empty() {
        Vec::new() // alloc: none, an empty `Vec`
    } else {
        let field_list = match adt {
            ast::Adt::Enum(it) => it.variant_list().map(|list| list.syntax().clone()), // clone: a node handle, a non-atomic count
            ast::Adt::Struct(it) => it.field_list().map(|list| list.syntax().clone()), // clone: a node handle, a non-atomic count
            ast::Adt::Union(it) => it.record_field_list().map(|list| list.syntax().clone()), // clone: a node handle, a non-atomic count
        };
        field_list
            .into_iter()
            .flat_map(|it| it.descendants())
            .filter_map(ast::PathType::cast)
            .filter_map(|p| {
                let name = p.path()?.qualifier()?.as_single_name_ref()?.as_name();
                param_names.contains(&name).then_some(p)
            })
            .map(|it| to_tt(it.syntax()))
            .collect() // alloc: one tree per `T::Assoc` path, only with type parameters
    };
    let name_token = name_to_token(call_site, tm, name)?;
    if let Some(kind) = struct_fields {
        variants.push(Variant { name: name_token.clone(), kind, start: 0, end: len_u32(fields.len()) }); // clone: a copy (symbol and span)
    }
    let packed = adt.attrs().any(|attr| attr_words_have(&attr, "repr", &["r"], "packed"));
    Ok(BasicAdtInfo {
        name: name_token,
        shape: AdtShape { kind, variants, fields },
        param_types,
        lifetimes,
        where_clause,
        associated_types,
        packed,
        copy_bounds: false,
    })
}

fn name_to_token(
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
