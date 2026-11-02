//! Items, with the spans of rustc's `parser/item.rs`: an item runs from its first token after
//! its outer attributes (its visibility, when it has one) to its last, and the inner attributes
//! of its body join its own.

use syntax::view::{
    self as ra, AstNode, HasGenericParams, HasModuleItem as _, HasName as _, HasTypeBounds as _,
    HasVisibility as _,
};
use syntax::{NodeOrToken, T};

use super::{Lower, first_token, last_token, prev_real_token};
use crate::ast::{self, DUMMY_NODE_ID, ThinVec};
use crate::span::{Ident, Span, kw, sym};

impl Lower<'_> {
    /// The crate root: rustc's `parse_mod(Eof)`, whose inner span runs from the file's first
    /// token to its last.
    pub(crate) fn krate(&self, file: ra::SourceFile<'_>) -> ast::Crate {
        let node = file.syntax();
        let attrs = self.inner_attrs(node);
        let items = file.items().filter_map(|i| self.item(i)).collect();
        let spans = self.mod_spans(node, None);
        ast::Crate {
            id: DUMMY_NODE_ID,
            attrs,
            items,
            spans,
            is_placeholder: false,
        }
    }

    /// `inner_span` from the first token after `{` (or of the file) to `}` (or the last token);
    /// `inject_use_span` empty at the first token after the inner attributes.
    fn mod_spans(&self, list: ra::Node<'_>, open: Option<ra::Token<'_>>) -> ast::ModSpans {
        let skip = usize::from(open.is_some());
        let (mut lo, mut hi) = (None, None);
        let tokens = list
            .descendants_with_tokens()
            .filter_map(NodeOrToken::into_token)
            .filter(|t| !super::is_trivia(t.kind()));
        for (n, t) in tokens.enumerate() {
            if n == skip {
                lo = Some(t);
            }
            hi = Some(t);
        }
        let empty = self.sp(syntax::TextRange::empty(list.text_range().end()));
        let lo = lo.map(|t| self.tsp(t)).unwrap_or(empty);
        let hi = hi.map(|t| self.tsp(t)).unwrap_or(empty);
        let inner_span = lo.to(hi);
        let mut post_attr = None;
        for child in list.children_with_tokens() {
            let kind = child.kind();
            if super::is_trivia(kind) || (skip == 1 && kind == T!['{']) {
                continue;
            }
            if let NodeOrToken::Node(n) = child
                && ra::AnyAttr::cast(n).is_some_and(|a| {
                    self.attr(a)
                        .is_some_and(|a| a.style == ast::AttrStyle::Inner)
                })
            {
                continue;
            }
            post_attr = match child {
                NodeOrToken::Token(t) => Some(self.tsp(t)),
                NodeOrToken::Node(n) => first_token(n).map(|t| self.tsp(t)),
            };
            break;
        }
        let post_attr = post_attr.unwrap_or(hi.shrink_to_hi());
        ast::ModSpans {
            inner_span,
            inject_use_span: post_attr.shrink_to_lo(),
        }
    }

    fn mk_item<K>(
        &self,
        node: ra::Node<'_>,
        vis: Option<ra::Visibility<'_>>,
        mut attrs: ast::AttrVec,
        kind: K,
    ) -> Box<ast::Item<K>> {
        let mut all = self.outer_attrs(node);
        all.append(&mut attrs);
        Box::new(ast::Item {
            attrs: all,
            id: DUMMY_NODE_ID,
            span: self.nsp(node),
            vis: self.visibility(vis, node),
            kind,
            tokens: None,
        })
    }

    pub(crate) fn item(&self, item: ra::Item<'_>) -> Option<Box<ast::Item>> {
        let node = item.syntax();
        let span = self.nsp(node);
        let vis = match item {
            ra::Item::ExternBlock(_) | ra::Item::MacroCall(_) | ra::Item::AsmExpr(_) => None,
            ra::Item::Const(i) => i.visibility(),
            ra::Item::Enum(i) => i.visibility(),
            ra::Item::ExternCrate(i) => i.visibility(),
            ra::Item::Fn(i) => i.visibility(),
            ra::Item::Impl(i) => i.visibility(),
            ra::Item::MacroDef(i) => i.visibility(),
            ra::Item::MacroRules(i) => i.visibility(),
            ra::Item::Module(i) => i.visibility(),
            ra::Item::Static(i) => i.visibility(),
            ra::Item::Struct(i) => i.visibility(),
            ra::Item::Trait(i) => i.visibility(),
            ra::Item::TypeAlias(i) => i.visibility(),
            ra::Item::Union(i) => i.visibility(),
            ra::Item::Use(i) => i.visibility(),
        };
        let mut inner = ThinVec::new();
        let kind = match item {
            ra::Item::Const(c) => ast::ItemKind::Const(self.const_item(c)),
            ra::Item::Enum(e) => {
                let ident = self.item_name(e.name(), span);
                let generics = self.adt_generics(e, ident.span, None);
                let variants = e
                    .variant_list()
                    .map(|l| l.variants().map(|v| self.variant(v)).collect())
                    .unwrap_or_default();
                ast::ItemKind::Enum(ident, generics, ast::EnumDef { variants })
            }
            ra::Item::ExternBlock(b) => {
                let safety = self.safety(b.unsafe_token(), None);
                let abi_node = b.abi();
                let extern_span = abi_node
                    .and_then(|a| a.extern_token())
                    .map(|t| self.tsp(t))
                    .unwrap_or(span);
                let abi = abi_node
                    .and_then(|a| a.string_token())
                    .and_then(|s| self.str_lit(s));
                let mut items = ThinVec::new();
                if let Some(list) = b.extern_item_list() {
                    inner = self.inner_attrs(list.syntax());
                    for it in list.extern_items() {
                        if let Some(it) = self.foreign_item(it) {
                            items.push(it);
                        }
                    }
                }
                ast::ItemKind::ForeignMod(ast::ForeignMod {
                    extern_span,
                    safety,
                    abi,
                    items,
                })
            }
            ra::Item::ExternCrate(e) => {
                let name = match e.name_ref() {
                    Some(n) => self.name_ref(n),
                    None => return self.item_err(span),
                };
                match e.rename() {
                    Some(r) => {
                        let ident = match (r.name(), r.underscore_token()) {
                            (Some(n), _) => self.name(n),
                            (None, Some(u)) => Ident::new(kw::Underscore, self.tsp(u)),
                            _ => return self.item_err(span),
                        };
                        ast::ItemKind::ExternCrate(Some(name.name), ident)
                    }
                    None => ast::ItemKind::ExternCrate(None, name),
                }
            }
            ra::Item::Fn(f) => {
                let (func, body_attrs) = self.func(f);
                inner = body_attrs;
                ast::ItemKind::Fn(func)
            }
            ra::Item::Impl(i) => {
                let (imp, attrs) = self.impl_item(i, span);
                inner = attrs;
                ast::ItemKind::Impl(imp)
            }
            ra::Item::MacroCall(m) => ast::ItemKind::MacCall(self.mac_call(m)),
            ra::Item::MacroRules(m) => {
                let ident = self.item_name(m.name(), span);
                let body = match m.token_tree() {
                    Some(tt) => Box::new(self.delim_args(tt)),
                    None => return self.item_err(span),
                };
                ast::ItemKind::MacroDef(
                    ident,
                    ast::MacroDef {
                        body,
                        macro_rules: true,
                        eii_declaration: None,
                    },
                )
            }
            ra::Item::MacroDef(m) => {
                let ident = self.item_name(m.name(), span);
                if m.args().is_some() {
                    self.unsupported(span, "a `macro` with a parameter list");
                }
                let body = match m.body() {
                    Some(tt) => Box::new(self.delim_args(tt)),
                    None => return self.item_err(span),
                };
                ast::ItemKind::MacroDef(
                    ident,
                    ast::MacroDef {
                        body,
                        macro_rules: false,
                        eii_declaration: None,
                    },
                )
            }
            ra::Item::Module(m) => {
                let ident = self.item_name(m.name(), span);
                let safety = self.safety(m.syntax().child_token(T![unsafe]), None);
                let kind = match m.item_list() {
                    Some(list) => {
                        inner = self.inner_attrs(list.syntax());
                        let items = list.items().filter_map(|i| self.item(i)).collect();
                        let spans = self.mod_spans(list.syntax(), list.l_curly_token());
                        ast::ModKind::Loaded(items, ast::Inline::Yes, spans)
                    }
                    None => ast::ModKind::Unloaded,
                };
                ast::ItemKind::Mod(safety, ident, kind)
            }
            ra::Item::Static(s) => ast::ItemKind::Static(self.static_item(s, span)),
            ra::Item::Struct(s) => {
                let ident = self.item_name(s.name(), span);
                let (data, tuple_end) = self.variant_data(s.field_list());
                let generics = self.adt_generics(s, ident.span, tuple_end);
                ast::ItemKind::Struct(ident, generics, data)
            }
            ra::Item::Trait(t) => {
                let (tr, attrs) = self.trait_item(t, span);
                inner = attrs;
                tr
            }
            ra::Item::TypeAlias(t) => ast::ItemKind::TyAlias(self.ty_alias(t, span)),
            ra::Item::Union(u) => {
                let ident = self.item_name(u.name(), span);
                let (data, _) =
                    self.variant_data(u.record_field_list().map(ra::FieldList::RecordFieldList));
                let generics = self.adt_generics(u, ident.span, None);
                ast::ItemKind::Union(ident, generics, data)
            }
            ra::Item::Use(u) => match u.use_tree() {
                Some(tree) => ast::ItemKind::Use(self.use_tree(tree)),
                None => return self.item_err(span),
            },
            ra::Item::AsmExpr(_) => {
                self.unsupported(span, "a builtin `global_asm`");
                return None;
            }
        };
        Some(self.mk_item(node, vis, inner, kind))
    }

    fn item_err<T>(&self, span: Span) -> Option<T> {
        self.unsupported(span, "an item rust-analyzer did not parse");
        None
    }

    fn item_name(&self, name: Option<ra::Name<'_>>, span: Span) -> Ident {
        match name {
            Some(n) => self.name(n),
            None => {
                self.unsupported(span, "an item without a name");
                Ident::new(sym::empty, span)
            }
        }
    }

    fn safety(
        &self,
        unsafe_: Option<ra::Token<'_>>,
        safe: Option<ra::Token<'_>>,
    ) -> ast::Safety {
        match (unsafe_, safe) {
            (Some(u), _) => ast::Safety::Unsafe(self.tsp(u)),
            (None, Some(s)) => ast::Safety::Safe(self.tsp(s)),
            (None, None) => ast::Safety::Default,
        }
    }

    fn defaultness(&self, default: Option<ra::Token<'_>>) -> ast::Defaultness {
        match default {
            Some(d) => ast::Defaultness::Default(self.tsp(d)),
            None => ast::Defaultness::Implicit,
        }
    }

    /// Generics of a struct, enum or union: without a `where`, rustc's clause is empty right
    /// after the generics (or, for a tuple struct, after its fields).
    fn adt_generics<'t>(
        &self,
        node: impl HasGenericParams<'t>,
        ident: Span,
        tuple_end: Option<Span>,
    ) -> ast::Generics {
        let after = node
            .generic_param_list()
            .map(|g| self.nsp(g.syntax()))
            .unwrap_or(ident);
        self.item_generics(node, ident, tuple_end.unwrap_or(after))
    }

    // ---- functions ----

    /// A function and the inner attributes of its body (rustc's `parse_fn`): the signature runs
    /// from the item's start to the token before the body, or through the `;`.
    fn func(&self, f: ra::Fn<'_>) -> (Box<ast::Fn>, ast::AttrVec) {
        let node = f.syntax();
        let span = self.nsp(node);
        let ident = self.item_name(f.name(), span);
        let params = f.param_list();
        let params_span = params
            .map(|p| self.nsp(p.syntax()))
            .unwrap_or(ident.span);
        let decl = self.fn_decl(params, f.ret_type(), span);
        let decl_end = match &decl.output {
            ast::FnRetTy::Ty(ty) => ty.span,
            ast::FnRetTy::Default(_) => params_span,
        };
        let generics = self.item_generics(f, ident.span, decl_end);
        let header = ast::FnHeader {
            constness: match f.const_token() {
                Some(c) => ast::Const::Yes(self.tsp(c)),
                None => ast::Const::No,
            },
            coroutine_marker: match (f.async_token(), f.gen_token()) {
                (Some(a), Some(g)) => {
                    Some((ast::CoroutineKind::AsyncGen, self.tsp(a).to(self.tsp(g))))
                }
                (Some(a), None) => Some((ast::CoroutineKind::Async, self.tsp(a))),
                (None, Some(g)) => Some((ast::CoroutineKind::Gen, self.tsp(g))),
                (None, None) => None,
            }
            .map(|(kind, span)| ast::CoroutineMarker {
                kind,
                span,
                closure_id: DUMMY_NODE_ID,
                return_impl_trait_id: DUMMY_NODE_ID,
            }),
            safety: self.safety(f.unsafe_token(), f.safe_token()),
            ext: self.abi(f.abi()),
        };
        let lo = super::first_token_after_attrs(node)
            .map(|t| self.tsp(t))
            .unwrap_or(span);
        let (body, inner, sig_hi) = match f.body() {
            Some(b) => {
                let first = b.syntax().first_token();
                let hi = first
                    .and_then(prev_real_token)
                    .map(|t| self.tsp(t))
                    .unwrap_or(span);
                (
                    Some(self.block_expr_inner(b)),
                    self.block_inner_attrs(b),
                    hi,
                )
            }
            None => {
                let hi = f.semicolon_token().map(|t| self.tsp(t)).unwrap_or(span);
                (None, ThinVec::new(), hi)
            }
        };
        let sig = ast::FnSig {
            header,
            decl: Box::new(decl),
            span: lo.to(sig_hi),
        };
        let func = ast::Fn {
            defaultness: self.defaultness(f.default_token()),
            ident,
            generics,
            sig,
            contract: None,
            define_opaque: None,
            body,
            eii_impl: None,
        };
        (Box::new(func), inner)
    }

    // ---- impls and traits ----

    fn impl_item(&self, i: ra::Impl<'_>, span: Span) -> (ast::Impl, ast::AttrVec) {
        let impl_kw = i.impl_token().map(|t| self.tsp(t)).unwrap_or(span);
        let self_ty = match i.self_ty() {
            Some(t) => self.ty(t),
            None => self.ty_err(span),
        };
        let generics = self.item_generics(i, impl_kw, self_ty.span);
        let constness = match i.const_token() {
            Some(c) => ast::Const::Yes(self.tsp(c)),
            None => ast::Const::No,
        };
        let of_trait = i.trait_().map(|t| {
            let path = match t {
                ra::Type::PathType(p) => p.path().map(|p| self.path(p).1),
                _ => None,
            }
            .unwrap_or_else(|| self.err_path(self.nsp(t.syntax())));
            let polarity = match i.excl_token() {
                Some(b) => ast::ImplPolarity::Negative(self.tsp(b)),
                None => ast::ImplPolarity::Positive,
            };
            Box::new(ast::TraitImplHeader {
                defaultness: self.defaultness(i.default_token()),
                safety: self.safety(i.unsafe_token(), None),
                polarity,
                trait_ref: ast::TraitRef {
                    path,
                    ref_id: DUMMY_NODE_ID,
                },
            })
        });
        if of_trait.is_none() && (i.unsafe_token().is_some() || i.default_token().is_some()) {
            self.unsupported(span, "an inherent impl with `unsafe` or `default`");
        }
        let (items, attrs) = match i.assoc_item_list() {
            Some(list) => (
                list.assoc_items()
                    .filter_map(|a| self.assoc_item(a))
                    .collect(),
                self.inner_attrs(list.syntax()),
            ),
            None => (ThinVec::new(), ThinVec::new()),
        };
        (
            ast::Impl {
                generics,
                constness,
                of_trait,
                self_ty,
                items,
            },
            attrs,
        )
    }

    fn trait_item(&self, t: ra::Trait<'_>, span: Span) -> (ast::ItemKind, ast::AttrVec) {
        let ident = self.item_name(t.name(), span);
        let bounds = self.opt_bounds(t);
        let gen_end = t
            .generic_param_list()
            .map(|g| self.nsp(g.syntax()))
            .unwrap_or(ident.span);
        let before_where = t
            .type_bound_list()
            .and_then(|b| last_token(b.syntax()))
            .or_else(|| t.colon_token())
            .map(|tok| self.tsp(tok))
            .unwrap_or(gen_end);
        if t.eq_token().is_some() {
            let generics = self.item_generics(t, ident.span, before_where);
            let constness = match t.syntax().child_token(T![const]) {
                Some(c) => ast::Const::Yes(self.tsp(c)),
                None => ast::Const::No,
            };
            return (
                ast::ItemKind::TraitAlias(Box::new(ast::TraitAlias {
                    constness,
                    ident,
                    generics,
                    bounds,
                })),
                ThinVec::new(),
            );
        }
        let generics = self.item_generics(t, ident.span, before_where);
        let (items, attrs) = match t.assoc_item_list() {
            Some(list) => (
                list.assoc_items()
                    .filter_map(|a| self.assoc_item(a))
                    .collect(),
                self.inner_attrs(list.syntax()),
            ),
            None => (ThinVec::new(), ThinVec::new()),
        };
        let impl_restriction = match t.impl_restriction() {
            Some(r) => {
                self.unsupported(self.nsp(r.syntax()), "an `impl` restriction");
                ast::ImplRestriction {
                    kind: ast::RestrictionKind::Unrestricted,
                    span,
                }
            }
            None => ast::ImplRestriction {
                kind: ast::RestrictionKind::Unrestricted,
                span: super::first_token_after_attrs(t.syntax())
                    .and_then(|tok| {
                        let mut tok = tok;
                        if t.visibility().is_some() {
                            let vis_last = t.visibility().and_then(|v| last_token(v.syntax()))?;
                            tok = super::next_real_token(vis_last)?;
                        }
                        Some(self.tsp(tok).shrink_to_lo())
                    })
                    .unwrap_or(span),
            },
        };
        let tr = ast::Trait {
            impl_restriction,
            constness: match t.syntax().child_token(T![const]) {
                Some(c) => ast::Const::Yes(self.tsp(c)),
                None => ast::Const::No,
            },
            safety: self.safety(t.unsafe_token(), None),
            is_auto: if t.auto_token().is_some() {
                ast::IsAuto::Yes
            } else {
                ast::IsAuto::No
            },
            ident,
            generics,
            bounds,
            items,
        };
        (ast::ItemKind::Trait(Box::new(tr)), attrs)
    }

    fn assoc_item(&self, a: ra::AssocItem<'_>) -> Option<Box<ast::AssocItem>> {
        let node = a.syntax();
        let span = self.nsp(node);
        let (vis, inner, kind) = match a {
            ra::AssocItem::Const(c) => (
                c.visibility(),
                ThinVec::new(),
                ast::AssocItemKind::Const(self.const_item(c)),
            ),
            ra::AssocItem::Fn(f) => {
                let (func, inner) = self.func(f);
                (f.visibility(), inner, ast::AssocItemKind::Fn(func))
            }
            ra::AssocItem::MacroCall(m) => (
                None,
                ThinVec::new(),
                ast::AssocItemKind::MacCall(self.mac_call(m)),
            ),
            ra::AssocItem::TypeAlias(t) => (
                t.visibility(),
                ThinVec::new(),
                ast::AssocItemKind::Type(self.ty_alias(t, span)),
            ),
        };
        Some(self.mk_item(node, vis, inner, kind))
    }

    fn foreign_item(&self, a: ra::ExternItem<'_>) -> Option<Box<ast::ForeignItem>> {
        let node = a.syntax();
        let span = self.nsp(node);
        let (vis, inner, kind) = match a {
            ra::ExternItem::Fn(f) => {
                let (func, inner) = self.func(f);
                (f.visibility(), inner, ast::ForeignItemKind::Fn(func))
            }
            ra::ExternItem::MacroCall(m) => (
                None,
                ThinVec::new(),
                ast::ForeignItemKind::MacCall(self.mac_call(m)),
            ),
            ra::ExternItem::Static(s) => (
                s.visibility(),
                ThinVec::new(),
                ast::ForeignItemKind::Static(self.static_item(s, span)),
            ),
            ra::ExternItem::TypeAlias(t) => (
                t.visibility(),
                ThinVec::new(),
                ast::ForeignItemKind::TyAlias(self.ty_alias(t, span)),
            ),
        };
        Some(self.mk_item(node, vis, inner, kind))
    }

    // ---- consts, statics, type aliases ----

    fn const_item(&self, c: ra::Const<'_>) -> Box<ast::ConstItem> {
        let span = self.nsp(c.syntax());
        let ident = match (c.name(), c.underscore_token()) {
            (Some(n), _) => self.name(n),
            (None, Some(u)) => Ident::new(kw::Underscore, self.tsp(u)),
            _ => self.item_name(None, span),
        };
        let ty = self.opt_ty(c.ty(), span);
        let body = c.body().map(|e| self.expr(e));
        let before_where = body.as_ref().map_or(ty.span, |b| b.span);
        let generics = self.item_generics(c, ident.span, before_where);
        if c.type_token().is_some() {
            self.unsupported(span, "a `type const`");
        }
        Box::new(ast::ConstItem {
            defaultness: self.defaultness(c.default_token()),
            ident,
            generics,
            ty,
            body,
            define_opaque: None,
        })
    }

    fn static_item(&self, s: ra::Static<'_>, span: Span) -> Box<ast::StaticItem> {
        let ident = self.item_name(s.name(), span);
        let ty = self.opt_ty(s.ty(), span);
        Box::new(ast::StaticItem {
            ident,
            ty,
            safety: self.safety(s.unsafe_token(), s.safe_token()),
            mutability: if s.mut_token().is_some() {
                ast::Mutability::Mut
            } else {
                ast::Mutability::Not
            },
            expr: s.body().map(|e| self.expr(e)),
            define_opaque: None,
            eii_impl: None,
        })
    }

    /// rustc's `parse_type_alias`: a `where` before `=` and one after the type.
    fn ty_alias(&self, t: ra::TypeAlias<'_>, span: Span) -> Box<ast::TyAlias> {
        let ident = self.item_name(t.name(), span);
        let bounds = self.opt_bounds(t);
        let gen_end = t
            .generic_param_list()
            .map(|g| self.nsp(g.syntax()))
            .unwrap_or(ident.span);
        let before_where = t
            .type_bound_list()
            .and_then(|b| last_token(b.syntax()))
            .or_else(|| t.colon_token())
            .map(|tok| self.tsp(tok))
            .unwrap_or(gen_end);
        let ty = t.ty().map(|ty| self.ty(ty));
        // Which side of `=` a `where` clause was written on decides which of rustc's two it is.
        let eq = t.eq_token().map(|eq| eq.text_range().start());
        let (mut before, mut after) = (None, None);
        for wc in t.syntax().children().filter_map(ra::WhereClause::cast) {
            match eq {
                Some(eq) if wc.syntax().text_range().start() > eq => after = Some(wc),
                _ => before = Some(wc),
            }
        }
        let after_ty = ty.as_ref().map_or(before_where, |ty| ty.span);
        let generics = self.generics(t.generic_param_list(), before, ident.span, before_where);
        let after_where_clause = self.where_clause(after, after_ty);
        Box::new(ast::TyAlias {
            defaultness: self.defaultness(t.default_token()),
            ident,
            generics,
            after_where_clause,
            bounds,
            ty,
        })
    }

    // ---- structs and enums ----

    /// The fields of a struct, union or variant, and for a tuple struct the span its `where`
    /// clause follows.
    fn variant_data(&self, fields: Option<ra::FieldList<'_>>) -> (ast::VariantData, Option<Span>) {
        match fields {
            Some(ra::FieldList::RecordFieldList(list)) => (
                ast::VariantData::Struct {
                    fields: list.fields().map(|f| self.record_field(f)).collect(),
                    recovered: ast::Recovered::No,
                },
                None,
            ),
            Some(ra::FieldList::TupleFieldList(list)) => (
                ast::VariantData::Tuple(
                    list.fields().map(|f| self.tuple_field(f)).collect(),
                    DUMMY_NODE_ID,
                ),
                Some(self.nsp(list.syntax())),
            ),
            None => (ast::VariantData::Unit(DUMMY_NODE_ID), None),
        }
    }

    /// rustc's `parse_name_and_ty`: from the visibility (after attributes) through the type or
    /// default value.
    fn record_field(&self, f: ra::RecordField<'_>) -> ast::FieldDef {
        let span = self.nsp(f.syntax());
        let ident = self.item_name(f.name(), span);
        let ty = self.opt_ty(f.ty(), span);
        let default = f
            .default_val()
            .and_then(|c| c.expr())
            .map(|e| self.anon_const(e));
        let safety = self.safety(f.unsafe_token(), None);
        if f.mut_restriction().is_some() {
            self.unsupported(span, "a `mut` restriction");
        }
        ast::FieldDef {
            attrs: self.outer_attrs(f.syntax()),
            id: DUMMY_NODE_ID,
            span,
            vis: self.visibility(f.visibility(), f.syntax()),
            extras: self.field_extras(safety, default, f.syntax()),
            ident: Some(ident),
            ty,
            is_placeholder: false,
        }
    }

    /// rustc's `parse_tuple_struct_body`: from the visibility through the type.
    fn tuple_field(&self, f: ra::TupleField<'_>) -> ast::FieldDef {
        let all = self.nsp(f.syntax());
        let ty = self.opt_ty(f.ty(), all);
        if f.mut_restriction().is_some() {
            self.unsupported(all, "a `mut` restriction");
        }
        ast::FieldDef {
            attrs: self.outer_attrs(f.syntax()),
            id: DUMMY_NODE_ID,
            span: all.shrink_to_lo().to(ty.span),
            vis: self.visibility(f.visibility(), f.syntax()),
            extras: None,
            ident: None,
            ty,
            is_placeholder: false,
        }
    }

    fn field_extras(
        &self,
        safety: ast::Safety,
        default: Option<ast::AnonConst>,
        node: ra::Node<'_>,
    ) -> Option<Box<ast::FieldDefExtras>> {
        if matches!(safety, ast::Safety::Default) && default.is_none() {
            return None;
        }
        let at = super::first_token_after_attrs(node).map(|t| self.tsp(t).shrink_to_lo());
        Some(Box::new(ast::FieldDefExtras {
            safety,
            mut_restriction: ast::MutRestriction {
                kind: ast::RestrictionKind::Unrestricted,
                span: at.unwrap_or_else(|| self.nsp(node)),
            },
            default,
        }))
    }

    /// rustc's `parse_enum_variant`: from the visibility or name (after attributes) through the
    /// discriminant.
    fn variant(&self, v: ra::Variant<'_>) -> ast::Variant {
        let span = self.nsp(v.syntax());
        let ident = match (v.name(), v.underscore_token()) {
            (Some(n), _) => self.name(n),
            (None, Some(u)) => Ident::new(kw::Underscore, self.tsp(u)),
            _ => self.item_name(None, span),
        };
        let (data, _) = self.variant_data(v.field_list());
        ast::Variant {
            attrs: self.outer_attrs(v.syntax()),
            id: DUMMY_NODE_ID,
            span,
            vis: self.visibility(v.visibility(), v.syntax()),
            ident,
            data,
            disr_expr: v
                .const_arg()
                .and_then(|c| c.expr())
                .map(|e| self.anon_const(e)),
            is_placeholder: false,
        }
    }

    // ---- use ----

    /// rustc's `parse_use_tree`: the prefix spans the path before any `::{..}` or `::*` (empty
    /// for a bare `{..}`), a nested list spans its braces and a glob its `*`.
    fn use_tree(&self, tree: ra::UseTree<'_>) -> ast::UseTree {
        let span = self.nsp(tree.syntax());
        let mut prefix = match tree.path() {
            Some(p) => self.path(p).1,
            None => ast::Path {
                span: span.shrink_to_lo(),
                segments: ThinVec::new(),
            },
        };
        if tree.path().is_none()
            && let Some(cc) = tree.coloncolon_token()
        {
            prefix
                .segments
                .push(ast::PathSegment::path_root(self.tsp(cc).shrink_to_lo()));
        }
        let kind = if let Some(star) = tree.star_token() {
            ast::UseTreeKind::Glob(self.tsp(star))
        } else if let Some(list) = tree.use_tree_list() {
            ast::UseTreeKind::Nested {
                items: list
                    .use_trees()
                    .map(|t| ast::UseTreeAndId {
                        inner: self.use_tree(t),
                        id: DUMMY_NODE_ID,
                    })
                    .collect(),
                span: self.nsp(list.syntax()),
            }
        } else {
            let rename = tree
                .rename()
                .and_then(|r| match (r.name(), r.underscore_token()) {
                    (Some(n), _) => Some(self.name(n)),
                    (None, Some(u)) => Some(Ident::new(kw::Underscore, self.tsp(u))),
                    _ => None,
                });
            ast::UseTreeKind::Simple(rename)
        };
        ast::UseTree { prefix, kind }
    }
}
