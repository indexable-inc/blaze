//! The analysis half of [`Semantics`] over views: scopes, path and method resolution, types of
//! expressions and patterns, and descending a token into macro expansions. Each method finds
//! its container as the handle version does (the shared dyn maps), builds a [`SourceAnalyzer`]
//! from the view, and answers from the same inference results.

use either::Either;
use hir_def::{MacroId, expr_store::HygieneId};
use hir_expand::{
    HirFileId, InFile, MacroCallId, MacroCallKind,
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
};
use intern::sym;
use smallvec::SmallVec;
use span::{Span, SyntaxContext};
use syntax::{
    TextRange, ast as tag,
    view::{self as ast, AstNode as _, HasAttrs as _, HasGenericParams as _, Node, Token},
};

use super::{
    Semantics, ToDef,
    source_to_def::{DescentTokens, ViewCtx, macro_call_to_macro_id},
};
use crate::{
    Callable, Field, Function, GenericSubstitution, ItemInNs, Label, LifetimeParam, Local, Macro,
    ModuleDef, TupleField, Type, Variant,
    semantics::{
        PathResolution, PathResolutionPerNs, SemanticsScope, TypeInfo,
        source_to_def::ChildContainer,
    },
    source_analyzer::{SourceAnalyzer, resolve_hir_path},
};

impl<'db> Semantics<'db> {
    /// The analyzer at `node`, its owner's inference included. `None` when `node`'s file is in
    /// no crate.
    fn analyze(&self, node: Node<'db>) -> Option<SourceAnalyzer<'db>> {
        self.analyze_impl(self.find_file(node), true)
    }

    fn analyze_no_infer(&self, node: Node<'db>) -> Option<SourceAnalyzer<'db>> {
        self.analyze_impl(self.find_file(node), false)
    }

    /// The analyzer for `node`'s innermost owner, with no offset.
    pub(super) fn analyze_impl(
        &self,
        node: InFile<Node<'db>>,
        infer: bool,
    ) -> Option<SourceAnalyzer<'db>> {
        let _p = tracing::info_span!("view::Semantics::analyze_impl").entered();
        let db = self.db;
        let container = self.find_container(node)?;
        let generic_def = |def| SourceAnalyzer::new_generic_def_view(db, &self.imp, def, node, infer);
        let resolver = match container {
            ChildContainer::DefWithBodyId(def) => {
                return Some(SourceAnalyzer::new_for_body_view(db, def, node, infer));
            }
            ChildContainer::VariantId(def) => {
                return Some(SourceAnalyzer::new_variant_body_view(db, &self.imp, def, node, infer));
            }
            ChildContainer::TraitId(it) => return Some(generic_def(it.into())),
            ChildContainer::ImplId(it) => return Some(generic_def(it.into())),
            ChildContainer::EnumId(it) => return Some(generic_def(it.into())),
            ChildContainer::GenericDefId(it) => return Some(generic_def(it)),
            ChildContainer::ModuleId(it) => hir_def::resolver::HasResolver::resolver(it, db),
        };
        Some(SourceAnalyzer::new_for_resolver_view(resolver, node.file_id))
    }

    pub fn scope(&self, node: Node<'db>) -> Option<SemanticsScope<'db>> {
        self.analyze_no_infer(node).map(
            |SourceAnalyzer { file_id, resolver, infer_body, .. }| SemanticsScope {
                db: self.db,
                file_id,
                resolver,
                infer_body,
            },
        )
    }

    pub fn resolve_lifetime_param(&self, lifetime: ast::Lifetime<'db>) -> Option<LifetimeParam> {
        let text = lifetime.text();
        let lifetime_param = lifetime.syntax().ancestors().find_map(|syn| {
            let gpl = ast::AnyHasGenericParams::cast(syn)?.generic_param_list()?;
            gpl.lifetime_params().find(|tp| tp.lifetime().map(|lt| lt.text()) == Some(text))
        })?;
        let src = self.wrap_node_infile(lifetime_param);
        ToDef::to_def(self, src)
    }

    pub fn resolve_label(&self, label: ast::Lifetime<'db>) -> Option<Label> {
        let src = self.wrap_node_infile(label);
        let (parent, label_id) = self.with_ctx(|ctx| ctx.label_ref_to_def(src))?;
        Some(Label { parent, label_id })
    }

    pub fn resolve_type(&self, ty: ast::Type<'db>) -> Option<Type<'db>> {
        self.analyze(ty.syntax())?.type_of_type_view(self.db, ty)
    }

    pub fn type_of_expr(&self, expr: ast::Expr<'db>) -> Option<TypeInfo<'db>> {
        self.analyze(expr.syntax())?
            .type_of_expr_view(self.db, expr)
            .map(|(ty, coerced)| TypeInfo { original: ty, adjusted: coerced })
    }

    /// The adjustments inference applied to `expr`, borrowed as inference stores them.
    pub fn expr_adjustments(&self, expr: ast::Expr<'db>) -> Option<&'db [hir_ty::Adjustment]> {
        self.analyze(expr.syntax())?.expr_adjustments_view(expr)
    }

    pub fn type_of_pat(&self, pat: ast::Pat<'db>) -> Option<TypeInfo<'db>> {
        self.analyze(pat.syntax())?
            .type_of_pat_view(self.db, pat)
            .map(|(ty, coerced)| TypeInfo { original: ty, adjusted: coerced })
    }

    /// `pat`'s type as rustc's "consider giving `x` an explicit type" names it, when inference
    /// left it undecided: [`Self::type_of_pat`]'s `original`, but with each variable made for a
    /// parameter that rustc names (a const parameter, a type parameter outside a macro
    /// expansion) that parameter. `None` for a pattern whose type was decided.
    pub fn named_type_of_pat(&self, pat: ast::Pat<'db>) -> Option<Type<'db>> {
        self.analyze(pat.syntax())?.named_type_of_pat_view(self.db, pat)
    }

    /// The type of the binding, with what its binding mode makes of it: `&mut Option<T>` for
    /// `ref mut x @ Some(_)` where [`Self::type_of_pat`] is `Option<T>`.
    pub fn type_of_binding_in_pat(&self, pat: ast::IdentPat<'db>) -> Option<Type<'db>> {
        self.analyze(pat.syntax())?.type_of_binding_in_pat_view(self.db, pat)
    }

    pub fn resolve_expr_as_callable(&self, call: ast::Expr<'db>) -> Option<Callable<'db>> {
        self.analyze(call.syntax())?.resolve_expr_as_callable_view(self.db, call)
    }

    pub fn resolve_method_call(&self, call: ast::MethodCallExpr<'db>) -> Option<Function> {
        self.analyze(call.syntax())?.resolve_method_call_view(self.db, call)
    }

    pub fn resolve_method_call_as_callable(
        &self,
        call: ast::MethodCallExpr<'db>,
    ) -> Option<Callable<'db>> {
        self.analyze(call.syntax())?.resolve_method_call_as_callable_view(self.db, call)
    }

    pub fn resolve_field(
        &self,
        field: ast::FieldExpr<'db>,
    ) -> Option<Either<Field, TupleField<'db>>> {
        self.analyze(field.syntax())?.resolve_field_view(field)
    }

    pub fn resolve_record_field(
        &self,
        field: ast::RecordExprField<'db>,
    ) -> Option<(Field, Option<Local<'db>>, Type<'db>)> {
        self.resolve_record_field_with_substitution(field)
            .map(|(field, local, ty, _)| (field, local, ty))
    }

    pub fn resolve_record_field_with_substitution(
        &self,
        field: ast::RecordExprField<'db>,
    ) -> Option<(Field, Option<Local<'db>>, Type<'db>, GenericSubstitution<'db>)> {
        self.analyze(field.syntax())?.resolve_record_field_view(self.db, field)
    }

    pub fn resolve_record_pat_field(
        &self,
        field: ast::RecordPatField<'db>,
    ) -> Option<(Field, Type<'db>)> {
        self.resolve_record_pat_field_with_subst(field).map(|(field, ty, _)| (field, ty))
    }

    pub fn resolve_record_pat_field_with_subst(
        &self,
        field: ast::RecordPatField<'db>,
    ) -> Option<(Field, Type<'db>, GenericSubstitution<'db>)> {
        self.analyze(field.syntax())?.resolve_record_pat_field_view(self.db, field)
    }

    pub fn resolve_tuple_struct_pat_fields(
        &self,
        tuple_struct_pat: ast::TupleStructPat<'db>,
    ) -> Option<Vec<(Field, Type<'db>)>> {
        self.analyze(tuple_struct_pat.syntax())?
            .resolve_tuple_struct_pat_fields_view(self.db, tuple_struct_pat)
    }

    pub fn resolve_variant(&self, record_lit: ast::RecordExpr<'db>) -> Option<Variant> {
        self.analyze(record_lit.syntax())?.resolve_variant_view(record_lit).map(Variant::from)
    }

    pub fn resolve_bind_pat_to_const(&self, pat: ast::IdentPat<'db>) -> Option<ModuleDef> {
        self.analyze(pat.syntax())?.resolve_bind_pat_to_const_view(self.db, pat)
    }

    pub fn resolve_offset_of_field(
        &self,
        name_ref: ast::NameRef<'db>,
    ) -> Option<(Either<crate::EnumVariant, Field>, GenericSubstitution<'db>)> {
        self.analyze_no_infer(name_ref.syntax())?.resolve_offset_of_field_view(self.db, name_ref)
    }

    pub fn resolve_path(&self, path: ast::Path<'db>) -> Option<PathResolution<'db>> {
        self.resolve_path_with_subst(path).map(|(it, _)| it)
    }

    pub fn resolve_path_with_subst(
        &self,
        path: ast::Path<'db>,
    ) -> Option<(PathResolution<'db>, Option<GenericSubstitution<'db>>)> {
        self.analyze(path.syntax())?.resolve_path_view(self.db, path)
    }

    pub fn resolve_path_per_ns(&self, path: ast::Path<'db>) -> Option<PathResolutionPerNs<'db>> {
        self.analyze(path.syntax())?.resolve_hir_path_per_ns_view(self.db, path)
    }

    pub fn resolve_mod_path(
        &self,
        scope: Node<'db>,
        path: &ModPath,
    ) -> Option<impl Iterator<Item = ItemInNs>> {
        let analyze = self.analyze(scope)?;
        let items = analyze.resolver.resolve_module_path_in_items(self.db, path);
        Some(items.iter_items().map(|(item, _)| item.into()))
    }

    pub fn resolve_macro_call(&self, macro_call: ast::MacroCall<'db>) -> Option<Macro> {
        self.resolve_macro_call2(self.wrap_node_infile(macro_call))
    }

    pub fn resolve_macro_call2(&self, macro_call: InFile<ast::MacroCall<'db>>) -> Option<Macro> {
        self.to_def2(macro_call)
            .and_then(|call| self.with_ctx(|ctx| macro_call_to_macro_id(ctx, call)))
            .map(Into::into)
    }

    pub fn is_proc_macro_call(&self, macro_call: InFile<ast::MacroCall<'db>>) -> bool {
        self.resolve_macro_call2(macro_call)
            .is_some_and(|m| matches!(m.id, MacroId::ProcMacroId(..)))
    }

    pub fn resolve_macro_call_arm(&self, macro_call: ast::MacroCall<'db>) -> Option<u32> {
        self.to_def(macro_call)?.expansion_span_map(self.db).matched_arm
    }

    pub fn resolve_attr_macro_call(&self, item: ast::Item<'db>) -> Option<Macro> {
        let item_in_file = self.wrap_node_infile(item);
        let id = self.with_ctx(|ctx| {
            let macro_call_id = ctx.item_to_macro_call(item_in_file)?;
            macro_call_to_macro_id(ctx, macro_call_id)
        })?;
        Some(Macro { id })
    }

    /// Descends the token into expansions, returning the tokens that match the input token's
    /// [`syntax::SyntaxKind`] and text: the token itself when none does.
    pub fn descend_into_macros_exact(&self, token: Token<'db>) -> SmallVec<[Token<'db>; 1]> {
        let mut r = SmallVec::new();
        let text = token.text();
        let kind = token.kind();
        self.descend_into_macros_all(self.wrap_token_infile(token), &mut |InFile { value, .. }, ctx| {
            let mapped_kind = value.kind();
            let any_ident_match = kind.is_any_identifier() && mapped_kind.is_any_identifier();
            if (kind == mapped_kind || any_ident_match)
                && text == value.text()
                && !ctx.is_opaque(self.db)
            {
                r.push(value);
            }
        });
        if r.is_empty() {
            r.push(token);
        }
        r
    }

    /// Descent into macros without descending into derives: `f` on each token `token` maps to
    /// that maps no further, through attribute macros, fn-like macros
    /// (eager arguments included), derive attributes and derive helpers.
    fn descend_into_macros_all(
        &self,
        InFile { value: token, file_id }: InFile<Token<'db>>,
        f: &mut impl FnMut(InFile<Token<'db>>, SyntaxContext),
    ) {
        let _p = tracing::info_span!("view::descend_into_macros").entered();
        let db = self.db;
        let span = file_id.span_map(db).span_for_range(token.text_range());

        // The walk's three stacks, inline: the groups of tokens mapped down, the tokens left to
        // descend, and the tokens of the step being taken.
        let mut groups: SmallVec<[(HirFileId, u32); 4]> = SmallVec::new();
        let mut pending = DescentTokens::new();
        let mut current = DescentTokens::new();

        let include = file_id
            .file_id()
            .and_then(|file_id| self.imp.s2d_cache.borrow_mut().get_or_insert_include_for(db, file_id));
        match include {
            Some(include) => {
                // include! inputs are always from real files, so they are handled once upfront.
                let mapped =
                    self.with_ctx(|ctx| map_down(ctx, &mut groups, &mut pending, span, include));
                if mapped.is_none() {
                    return;
                }
            }
            None => {
                pending.push((token, span.ctx));
                groups.push((file_id, 1));
            }
        }

        while let Some((expansion, len)) = groups.pop() {
            current.clear();
            current.extend(pending.drain(pending.len() - len as usize..));
            // Reversed so the first tokens come first off the back.
            current.reverse();
            while let Some((token, ctx)) = current.pop() {
                let remapped =
                    self.descend_step(expansion, token, span, &mut current, &mut groups, &mut pending);
                if remapped.is_none() {
                    f(InFile::new(expansion, token), ctx);
                }
            }
        }
    }

    /// One token of the walk: `Some` when it was mapped down into an expansion (its tokens are
    /// on `pending`, a group on `groups`).
    fn descend_step(
        &self,
        expansion: HirFileId,
        token: Token<'db>,
        span: Span,
        tokens: &mut DescentTokens<'db>,
        groups: &mut SmallVec<[(HirFileId, u32); 4]>,
        pending: &mut DescentTokens<'db>,
    ) -> Option<()> {
        let db = self.db;
        // A token inside `range` (the call just descended into) is mapped by that call already.
        let filter_duplicates = |tokens: &mut DescentTokens<'db>, range: TextRange| {
            tokens.retain(|(t, _)| !range.contains_range(t.text_range()))
        };

        // Attribute invocations first: a macro call inside one resolves only when descended.
        let res = self.with_ctx(|ctx| {
            token
                .parent_ancestors()
                .filter_map(ast::Item::cast)
                .find_map(|item| {
                    // Don't populate the dyn cache for items with no attribute.
                    item.attrs().next()?;
                    ctx.item_to_macro_call(InFile::new(expansion, item)).zip(Some(item))
                })
                .map(|(call_id, item)| {
                    let item_range = item.syntax().text_range();
                    let loc = call_id.loc(db);
                    let text_range = match &loc.kind {
                        MacroCallKind::Attr { censored_attr_ids: attr_ids, .. } => {
                            let (attr, _) = attr_ids.invoc_attr().find_attr_range_with_source_view(
                                db,
                                loc.krate,
                                ast::AnyHasAttrs::new(item),
                            );
                            TextRange::new(attr.syntax().text_range().start(), item_range.end())
                        }
                        _ => item_range,
                    };
                    filter_duplicates(tokens, text_range);
                    map_down(ctx, groups, pending, span, call_id)
                })
        });
        if let Some(res) = res {
            return res;
        }

        // Then token trees: a fn-like macro's input or a secondary attribute input.
        let tt = token
            .parent_ancestors()
            .map_while(Either::<ast::TokenTree<'db>, ast::Meta<'db>>::cast)
            .last()?;
        match tt {
            Either::Left(tt) => {
                let macro_call = tt.syntax().parent().and_then(ast::MacroCall::cast)?;
                if tt.left_delimiter_token() == Some(token)
                    || tt.right_delimiter_token() == Some(token)
                {
                    return None;
                }
                let file_id = self.to_def2(InFile::new(expansion, macro_call))?;
                filter_duplicates(tokens, tt.syntax().text_range());
                self.with_ctx(|ctx| {
                    let res = map_down(ctx, groups, pending, span, file_id);
                    // also descend into eager expansions
                    res.or(file_id
                        .eager_arg(db)
                        .and_then(|arg| map_down(ctx, groups, pending, span, arg)))
                })
            }
            // A derive or a derive helper: the attribute's expansion failed earlier.
            Either::Right(meta) => {
                let attr = meta.parent_attr()?;
                let adt = match attr.syntax().parent().and_then(ast::Adt::cast) {
                    Some(adt) => {
                        // A derive on the ADT: map into the pseudo derive expansion.
                        let res = self.with_ctx(|ctx| {
                            let derive_call = ctx
                                .attr_to_derive_macro_call(
                                    InFile::new(expansion, adt),
                                    InFile::new(expansion, meta),
                                )?
                                .1;
                            filter_duplicates(tokens, attr.syntax().text_range());
                            Some(map_down(ctx, groups, pending, span, derive_call))
                        });
                        if let Some(res) = res {
                            return res;
                        }
                        adt
                    }
                    // Otherwise a derive helper on a variant or field.
                    None => attr.syntax().ancestors().find_map(ast::Item::cast).and_then(
                        |it| match it {
                            ast::Item::Struct(it) => Some(ast::Adt::Struct(it)),
                            ast::Item::Enum(it) => Some(ast::Adt::Enum(it)),
                            ast::Item::Union(it) => Some(ast::Adt::Union(it)),
                            _ => None,
                        },
                    )?,
                };
                let attr_name = attr.path().and_then(|it| it.as_single_name_ref())?.as_name();
                let analyzer =
                    self.analyze_impl(InFile::new(expansion, token.parent()?), false)?;
                let id = expansion.ast_id_map(db).ast_id_of_view::<tag::Adt>(adt);
                let helpers = analyzer
                    .resolver
                    .def_map()
                    .derive_helpers_in_scope(db, InFile::new(expansion, id))?;
                if !helpers.is_empty() {
                    filter_duplicates(tokens, attr.syntax().text_range());
                }
                self.with_ctx(|ctx| {
                    let mut res = None;
                    for (.., derive) in helpers.iter().filter(|(helper, ..)| *helper == attr_name) {
                        let Either::Left(derive) = *derive else { continue };
                        // Several derives may register one helper name: map into each.
                        res = res.or(map_down(ctx, groups, pending, span, derive));
                    }
                    res
                })
            }
        }
    }
}

/// Maps `span` down into `macro_file`'s expansion, its tokens onto `pending` as one group: `Some`
/// when any token was mapped.
fn map_down<'db>(
    ctx: &mut ViewCtx<'db, '_>,
    groups: &mut SmallVec<[(HirFileId, u32); 4]>,
    pending: &mut DescentTokens<'db>,
    span: Span,
    macro_file: MacroCallId,
) -> Option<()> {
    let mapped = ctx.map_range_down(macro_file, span, pending)?;
    groups.push((HirFileId::from(macro_file), mapped));
    (mapped != 0).then_some(())
}

impl<'db> SemanticsScope<'db> {
    /// [`SemanticsScope::speculative_resolve`] of a view of the path.
    pub fn speculative_resolve_view(&self, ast_path: ast::Path<'_>) -> Option<PathResolution<'db>> {
        let mut kind = PathKind::Plain;
        let mut segments = SmallVec::<[Name; 8]>::new();
        let mut first = true;
        for segment in ast_path.segments() {
            if first {
                first = false;
                if segment.coloncolon_token().is_some() {
                    kind = PathKind::Abs;
                }
            }

            let Some(k) = segment.kind() else { continue };
            match k {
                ast::PathSegmentKind::Name(name_ref) => segments.push(name_ref.as_name()),
                ast::PathSegmentKind::Type { .. } => continue,
                ast::PathSegmentKind::SelfTypeKw => {
                    segments.push(Name::new_symbol_root(sym::Self_))
                }
                ast::PathSegmentKind::SelfKw => kind = PathKind::Super(0),
                ast::PathSegmentKind::SuperKw => match kind {
                    PathKind::Super(s) => kind = PathKind::Super(s + 1),
                    PathKind::Plain => kind = PathKind::Super(1),
                    PathKind::Crate | PathKind::Abs | PathKind::DollarCrate(_) => continue,
                },
                ast::PathSegmentKind::CrateKw => kind = PathKind::Crate,
            }
        }

        resolve_hir_path(
            self.db,
            &self.resolver,
            self.infer_body,
            hir_def::expr_store::path::Path::BarePath(&ModPath::from_segments(kind, segments)),
            HygieneId::ROOT,
            None,
        )
    }
}
