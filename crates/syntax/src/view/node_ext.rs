//! The hand-written accessors of the typed views, `ast/node_ext.rs` on views: shallow questions
//! about a node's own syntax. Text is lent from the tree (`&'t str`); nothing here allocates.

use std::iter::{self, successors};

use crate::{
    NodeOrToken, SyntaxKind, T, TextSize, WalkEvent,
    view::{
        self as ast, AstChildren, AstNode, AstToken, ChildrenWithTokens, Element, HasAttrs,
        HasGenericArgs, HasGenericParams, HasLoopBody, HasModuleItem, HasName, HasTypeBounds,
        Node, Preorder, RangeItem, RangeOp, Token, support,
    },
};

pub use crate::ast::{AttrKind, SelfParamKind};

impl<'t> ast::Lifetime<'t> {
    #[inline]
    pub fn text(self) -> &'t str {
        text_of_first_token(self.syntax)
    }
}

impl<'t> ast::Name<'t> {
    #[inline]
    pub fn text(self) -> &'t str {
        text_of_first_token(self.syntax)
    }
}

impl<'t> ast::NameRef<'t> {
    #[inline]
    pub fn text(self) -> &'t str {
        text_of_first_token(self.syntax)
    }

    pub fn as_tuple_field(self) -> Option<usize> {
        self.text().parse().ok()
    }

    #[inline]
    pub fn token_kind(self) -> SyntaxKind {
        self.syntax.first_token().map_or(SyntaxKind::ERROR, |it| it.kind())
    }
}

#[inline]
fn text_of_first_token(node: Node<'_>) -> &str {
    node.first_child_token_text().expect("a name's first child is its token")
}

fn into_comma(it: Element<'_>) -> Option<Token<'_>> {
    let token = match it {
        NodeOrToken::Token(it) => it,
        NodeOrToken::Node(node) if node.kind() == SyntaxKind::ERROR => node.first_token()?,
        NodeOrToken::Node(_) => return None,
    };
    (token.kind() == T![,]).then_some(token)
}

impl<'t> ast::Abi<'t> {
    pub fn abi_string(self) -> Option<ast::String<'t>> {
        support::token(self.syntax, SyntaxKind::STRING).and_then(ast::String::cast)
    }
}

impl<'t> HasModuleItem<'t> for ast::StmtList<'t> {}

impl<'t> ast::Fn<'t> {
    /// Whether an outline (`SourceFile::parse_outline`) left the body unparsed: a lazy node,
    /// whose part is parsed the first time a walk enters it (or was already). Never parses.
    pub fn body_is_lazy(self) -> bool {
        let Some(body) = self.body() else { return false };
        let (tree, at) = body.syntax.flat();
        tree.is_lazy(at)
    }
}

impl<'t> ast::BlockExpr<'t> {
    pub fn statements(self) -> impl Iterator<Item = ast::Stmt<'t>> + use<'t> {
        self.stmt_list().into_iter().flat_map(|it| it.statements())
    }

    pub fn tail_expr(self) -> Option<ast::Expr<'t>> {
        self.stmt_list()?.tail_expr()
    }

    /// Block expressions accept outer and inner attributes, but only when they are the outer
    /// expression of an expression statement or the final expression of another block expression.
    #[inline]
    pub fn may_carry_attributes(self) -> bool {
        matches!(
            self.syntax.parent().map(|it| it.kind()),
            Some(SyntaxKind::BLOCK_EXPR | SyntaxKind::EXPR_STMT)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Macro<'t> {
    MacroRules(ast::MacroRules<'t>),
    MacroDef(ast::MacroDef<'t>),
}

impl<'t> From<ast::MacroRules<'t>> for Macro<'t> {
    fn from(it: ast::MacroRules<'t>) -> Self {
        Macro::MacroRules(it)
    }
}

impl<'t> From<ast::MacroDef<'t>> for Macro<'t> {
    fn from(it: ast::MacroDef<'t>) -> Self {
        Macro::MacroDef(it)
    }
}

impl<'t> AstNode<'t> for Macro<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        let res = match syntax.kind() {
            SyntaxKind::MACRO_RULES => Macro::MacroRules(ast::MacroRules { syntax }),
            SyntaxKind::MACRO_DEF => Macro::MacroDef(ast::MacroDef { syntax }),
            _ => return None,
        };
        Some(res)
    }

    fn syntax(&self) -> Node<'t> {
        match self {
            Macro::MacroRules(it) => it.syntax,
            Macro::MacroDef(it) => it.syntax,
        }
    }
}

impl<'t> HasName<'t> for Macro<'t> {
    fn name(&self) -> Option<ast::Name<'t>> {
        match self {
            Macro::MacroRules(mac) => mac.name(),
            Macro::MacroDef(mac) => mac.name(),
        }
    }
}

impl<'t> HasAttrs<'t> for Macro<'t> {}

impl<'t> From<ast::AssocItem<'t>> for ast::Item<'t> {
    fn from(assoc: ast::AssocItem<'t>) -> Self {
        match assoc {
            ast::AssocItem::Const(it) => ast::Item::Const(it),
            ast::AssocItem::Fn(it) => ast::Item::Fn(it),
            ast::AssocItem::MacroCall(it) => ast::Item::MacroCall(it),
            ast::AssocItem::TypeAlias(it) => ast::Item::TypeAlias(it),
        }
    }
}

impl<'t> From<ast::ExternItem<'t>> for ast::Item<'t> {
    fn from(extern_item: ast::ExternItem<'t>) -> Self {
        match extern_item {
            ast::ExternItem::Static(it) => ast::Item::Static(it),
            ast::ExternItem::Fn(it) => ast::Item::Fn(it),
            ast::ExternItem::MacroCall(it) => ast::Item::MacroCall(it),
            ast::ExternItem::TypeAlias(it) => ast::Item::TypeAlias(it),
        }
    }
}

impl<'t> ast::Meta<'t> {
    pub fn as_simple_atom(self) -> Option<&'t str> {
        Some(self.as_simple_path()?.as_single_name_ref()?.text())
    }

    pub fn as_simple_call(self) -> Option<(&'t str, ast::TokenTree<'t>)> {
        let ast::Meta::TokenTreeMeta(meta) = self else { return None };
        Some((meta.path()?.as_single_name_ref()?.text(), meta.token_tree()?))
    }

    pub fn as_simple_path(self) -> Option<ast::Path<'t>> {
        let ast::Meta::PathMeta(meta) = self else { return None };
        meta.path()
    }

    pub fn simple_name(self) -> Option<&'t str> {
        match self {
            ast::Meta::CfgAttrMeta(_) => Some("cfg_attr"),
            ast::Meta::CfgMeta(_) => Some("cfg"),
            _ => {
                let path = self.path()?;
                match (path.segment(), path.qualifier()) {
                    (Some(segment), None) => Some(segment.syntax.first_token()?.text()),
                    _ => None,
                }
            }
        }
    }

    pub fn path(self) -> Option<ast::Path<'t>> {
        match self {
            ast::Meta::CfgAttrMeta(_) | ast::Meta::CfgMeta(_) => None,
            ast::Meta::KeyValueMeta(it) => it.path(),
            ast::Meta::PathMeta(it) => it.path(),
            ast::Meta::TokenTreeMeta(it) => it.path(),
            ast::Meta::UnsafeMeta(it) => it.meta()?.path(),
        }
    }

    /// This meta, or for a `cfg_attr` the metas it holds, nested `cfg_attr`s included (without
    /// considering the predicate), in source order.
    pub fn skip_cfg_attrs(self) -> impl Iterator<Item = ast::Meta<'t>> + Clone + use<'t> {
        SkipCfgAttrs { walk: self.syntax().preorder() }
    }

    /// FIXME: Calling this is almost always incorrect, as `cfg_attr` can contains multiple `Meta`s.
    pub fn parent_attr(self) -> Option<ast::Attr<'t>> {
        self.syntax().ancestors().find_map(ast::Attr::cast)
    }
}

/// [`ast::Meta::skip_cfg_attrs`] as a walk instead of a recursion into a buffer: it enters each
/// `cfg_attr` and gives every meta child it meets there, a `cfg_attr` child entered in turn.
#[derive(Clone)]
struct SkipCfgAttrs<'t> {
    walk: Preorder<'t>,
}

impl<'t> Iterator for SkipCfgAttrs<'t> {
    type Item = ast::Meta<'t>;

    fn next(&mut self) -> Option<ast::Meta<'t>> {
        while let Some(event) = self.walk.next() {
            let WalkEvent::Enter(node) = event else { continue };
            if node.kind() == SyntaxKind::CFG_ATTR_META {
                continue;
            }
            self.walk.skip_subtree();
            if let Some(meta) = ast::Meta::cast(node) {
                return Some(meta);
            }
        }
        None
    }
}

impl<'t> ast::Attr<'t> {
    pub fn as_simple_atom(self) -> Option<&'t str> {
        self.meta().and_then(|meta| meta.as_simple_atom())
    }

    pub fn as_simple_call(self) -> Option<(&'t str, ast::TokenTree<'t>)> {
        self.meta().and_then(|meta| meta.as_simple_call())
    }

    pub fn as_simple_path(self) -> Option<ast::Path<'t>> {
        self.meta().and_then(|meta| meta.as_simple_path())
    }

    pub fn simple_name(self) -> Option<&'t str> {
        self.meta().and_then(|meta| meta.simple_name())
    }

    pub fn path(self) -> Option<ast::Path<'t>> {
        self.meta().and_then(|meta| meta.path())
    }

    #[inline]
    pub fn kind(self) -> AttrKind {
        match self.excl_token() {
            Some(_) => AttrKind::Inner,
            None => AttrKind::Outer,
        }
    }

    /// Includes `cfg_attr()` inner metas (without considering the predicate).
    pub fn skip_cfg_attrs(self) -> impl Iterator<Item = ast::Meta<'t>> + Clone + use<'t> {
        self.meta().into_iter().flat_map(ast::Meta::skip_cfg_attrs)
    }
}

impl<'t> ast::DocComment<'t> {
    // `///` or `/**` or `//!` or `/*!`, all are 3 chars.
    pub const PREFIX_LEN: TextSize = TextSize::new(3);

    #[inline]
    pub fn kind(self) -> AttrKind {
        match self.inner_doc_comment_token() {
            Some(_) => AttrKind::Inner,
            None => AttrKind::Outer,
        }
    }

    pub fn token(self) -> ast::AnyComment<'t> {
        self.syntax
            .first_token()
            .and_then(ast::AnyComment::cast)
            .expect("`ast::DocComment` must have a comment token")
    }

    pub fn shape(self) -> ast::CommentShape {
        ast::CommentShape::from_text(self.text_with_markers())
    }

    /// Returns the text with the `/**...*/` or `/*!...*/` or `///...` or `//!...` markers.
    #[inline]
    pub fn text_with_markers(self) -> &'t str {
        text_of_first_token(self.syntax)
    }

    /// Returns the textual content of a doc comment node as a single string with prefix and suffix removed.
    pub fn text(self) -> &'t str {
        let shape = self.shape();
        let text = &self.text_with_markers()[Self::PREFIX_LEN.into()..];
        if shape == ast::CommentShape::Block {
            // The `*/` may not exist because of recovery.
            text.strip_suffix("*/").unwrap_or(text)
        } else {
            text
        }
    }
}

impl<'t> ast::AnyAttr<'t> {
    #[inline]
    pub fn kind(self) -> AttrKind {
        match self {
            ast::AnyAttr::Attr(it) => it.kind(),
            ast::AnyAttr::DocComment(it) => it.kind(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathSegmentKind<'t> {
    Name(ast::NameRef<'t>),
    Type { type_ref: Option<ast::Type<'t>>, trait_ref: Option<ast::PathType<'t>> },
    SelfTypeKw,
    SelfKw,
    SuperKw,
    CrateKw,
}

impl<'t> ast::PathSegment<'t> {
    pub fn parent_path(self) -> ast::Path<'t> {
        self.syntax
            .parent()
            .and_then(ast::Path::cast)
            .expect("segments are always nested in paths")
    }

    pub fn crate_token(self) -> Option<Token<'t>> {
        self.name_ref().and_then(|it| it.crate_token())
    }

    pub fn self_token(self) -> Option<Token<'t>> {
        self.name_ref().and_then(|it| it.self_token())
    }

    pub fn self_type_token(self) -> Option<Token<'t>> {
        self.name_ref().and_then(|it| it.Self_token())
    }

    pub fn super_token(self) -> Option<Token<'t>> {
        self.name_ref().and_then(|it| it.super_token())
    }

    pub fn kind(self) -> Option<PathSegmentKind<'t>> {
        let res = if let Some(name_ref) = self.name_ref() {
            match name_ref.token_kind() {
                T![Self] => PathSegmentKind::SelfTypeKw,
                T![self] => PathSegmentKind::SelfKw,
                T![super] => PathSegmentKind::SuperKw,
                T![crate] => PathSegmentKind::CrateKw,
                _ => PathSegmentKind::Name(name_ref),
            }
        } else {
            let anchor = self.type_anchor()?;
            // <T> or <T as Trait>
            // T is any TypeRef, Trait has to be a PathType
            let mut type_refs =
                anchor.syntax.children().filter(|node| ast::Type::can_cast(node.kind()));
            let type_ref = type_refs.next().and_then(ast::Type::cast);
            let trait_ref = type_refs.next().and_then(ast::PathType::cast);
            PathSegmentKind::Type { type_ref, trait_ref }
        };
        Some(res)
    }

    // [#15778](https://github.com/rust-lang/rust-analyzer/issues/15778)
    pub fn qualifying_trait(self) -> Option<ast::PathType<'t>> {
        let mut path_types = support::children(self.type_anchor()?.syntax);
        let first = path_types.next()?;
        path_types.next().or(Some(first))
    }
}

impl<'t> ast::Path<'t> {
    pub fn parent_path(self) -> Option<ast::Path<'t>> {
        self.syntax.parent().and_then(ast::Path::cast)
    }

    pub fn as_single_segment(self) -> Option<ast::PathSegment<'t>> {
        match self.qualifier() {
            Some(_) => None,
            None => self.segment(),
        }
    }

    pub fn as_single_name_ref(self) -> Option<ast::NameRef<'t>> {
        match self.qualifier() {
            Some(_) => None,
            None => self.segment()?.name_ref(),
        }
    }

    pub fn first_qualifier_or_self(self) -> ast::Path<'t> {
        self.first_qualifier().unwrap_or(self)
    }

    pub fn first_qualifier(self) -> Option<ast::Path<'t>> {
        successors(self.qualifier(), |p| p.qualifier()).last()
    }

    pub fn first_segment(self) -> Option<ast::PathSegment<'t>> {
        self.first_qualifier_or_self().segment()
    }

    pub fn segments(self) -> impl Iterator<Item = ast::PathSegment<'t>> + Clone + use<'t> {
        let path_range = self.syntax.text_range();
        successors(self.first_segment(), move |p| {
            p.parent_path().parent_path().and_then(|p| {
                if path_range.contains_range(p.syntax.text_range()) { p.segment() } else { None }
            })
        })
    }

    pub fn qualifiers(self) -> impl Iterator<Item = ast::Path<'t>> + Clone + use<'t> {
        successors(self.qualifier(), |p| p.qualifier())
    }

    pub fn top_path(self) -> ast::Path<'t> {
        let mut this = self;
        while let Some(path) = this.parent_path() {
            this = path;
        }
        this
    }
}

impl<'t> ast::Use<'t> {
    pub fn is_simple_glob(self) -> bool {
        self.use_tree().is_some_and(|use_tree| {
            use_tree.use_tree_list().is_none() && use_tree.star_token().is_some()
        })
    }
}

impl<'t> ast::UseTree<'t> {
    pub fn is_simple_path(self) -> bool {
        self.use_tree_list().is_none() && self.star_token().is_none()
    }

    pub fn parent_use_tree_list(self) -> Option<ast::UseTreeList<'t>> {
        self.syntax.parent().and_then(ast::UseTreeList::cast)
    }

    pub fn top_use_tree(self) -> ast::UseTree<'t> {
        let mut this = self;
        while let Some(use_tree_list) = this.parent_use_tree_list() {
            this = use_tree_list.parent_use_tree();
        }
        this
    }
}

impl<'t> ast::UseTreeList<'t> {
    pub fn parent_use_tree(self) -> ast::UseTree<'t> {
        self.syntax
            .parent()
            .and_then(ast::UseTree::cast)
            .expect("UseTreeLists are always nested in UseTrees")
    }

    pub fn has_inner_comment(self) -> bool {
        self.syntax
            .children_with_tokens()
            .filter_map(|it| it.into_token())
            .any(|it| ast::Comment::can_cast(it.kind()))
    }

    pub fn comma(self) -> impl Iterator<Item = Token<'t>> + use<'t> {
        self.syntax
            .children_with_tokens()
            .filter_map(|it| it.into_token().filter(|it| it.kind() == T![,]))
    }
}

impl<'t> ast::Impl<'t> {
    pub fn self_ty(self) -> Option<ast::Type<'t>> {
        self.target().1
    }

    pub fn trait_(self) -> Option<ast::Type<'t>> {
        self.target().0
    }

    fn target(self) -> (Option<ast::Type<'t>>, Option<ast::Type<'t>>) {
        let mut types = support::children::<ast::Type<'t>>(self.syntax).peekable();
        let for_kw = self.for_token();
        let trait_ = types.next_if(|trait_| {
            for_kw.is_some_and(|for_kw| {
                trait_.syntax().text_range().start() < for_kw.text_range().start()
            })
        });
        let self_ty = types.next();
        (trait_, self_ty)
    }

    pub fn for_trait_name_ref(name_ref: ast::NameRef<'t>) -> Option<ast::Impl<'t>> {
        let this = name_ref.syntax.ancestors().find_map(ast::Impl::cast)?;
        if this.trait_()?.syntax().text_range().start() == name_ref.syntax.text_range().start() {
            Some(this)
        } else {
            None
        }
    }
}

impl<'t> ast::Trait<'t> {
    /// The `const` of `const trait Tr`, which the parser keeps as a modifier of the item.
    #[inline]
    pub fn const_token(self) -> Option<Token<'t>> {
        support::token(self.syntax, T![const])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructKind<'t> {
    Record(ast::RecordFieldList<'t>),
    Tuple(ast::TupleFieldList<'t>),
    Unit,
}

impl<'t> StructKind<'t> {
    fn from_node(node: Node<'t>) -> StructKind<'t> {
        if let Some(nfdl) = support::child::<ast::RecordFieldList<'t>>(node) {
            StructKind::Record(nfdl)
        } else if let Some(pfl) = support::child::<ast::TupleFieldList<'t>>(node) {
            StructKind::Tuple(pfl)
        } else {
            StructKind::Unit
        }
    }
}

impl<'t> ast::Struct<'t> {
    pub fn kind(self) -> StructKind<'t> {
        StructKind::from_node(self.syntax)
    }
}

impl<'t> ast::Union<'t> {
    pub fn kind(self) -> StructKind<'t> {
        StructKind::from_node(self.syntax)
    }
}

impl<'t> ast::RecordExprField<'t> {
    pub fn for_field_name(field_name: ast::NameRef<'t>) -> Option<ast::RecordExprField<'t>> {
        let candidate = Self::for_name_ref(field_name)?;
        if candidate.field_name() == Some(field_name) { Some(candidate) } else { None }
    }

    pub fn for_name_ref(name_ref: ast::NameRef<'t>) -> Option<ast::RecordExprField<'t>> {
        let syn = name_ref.syntax;
        syn.parent()
            .and_then(ast::RecordExprField::cast)
            .or_else(|| syn.ancestors().nth(4).and_then(ast::RecordExprField::cast))
    }

    /// Deals with field init shorthand
    pub fn field_name(self) -> Option<ast::NameRef<'t>> {
        if let Some(name_ref) = self.name_ref() {
            return Some(name_ref);
        }
        if let ast::Expr::PathExpr(expr) = self.expr()? {
            let path = expr.path()?;
            let segment = path.segment()?;
            let name_ref = segment.name_ref()?;
            if path.qualifier().is_none() {
                return Some(name_ref);
            }
        }
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameLike<'t> {
    NameRef(ast::NameRef<'t>),
    Name(ast::Name<'t>),
    Lifetime(ast::Lifetime<'t>),
}

impl<'t> NameLike<'t> {
    pub fn as_name_ref(self) -> Option<ast::NameRef<'t>> {
        match self {
            NameLike::NameRef(name_ref) => Some(name_ref),
            _ => None,
        }
    }

    pub fn as_lifetime(self) -> Option<ast::Lifetime<'t>> {
        match self {
            NameLike::Lifetime(lifetime) => Some(lifetime),
            _ => None,
        }
    }

    pub fn text(self) -> &'t str {
        match self {
            NameLike::NameRef(name_ref) => name_ref.text(),
            NameLike::Name(name) => name.text(),
            NameLike::Lifetime(lifetime) => lifetime.text(),
        }
    }
}

impl<'t> AstNode<'t> for NameLike<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::NAME | SyntaxKind::NAME_REF | SyntaxKind::LIFETIME)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        let res = match syntax.kind() {
            SyntaxKind::NAME => NameLike::Name(ast::Name { syntax }),
            SyntaxKind::NAME_REF => NameLike::NameRef(ast::NameRef { syntax }),
            SyntaxKind::LIFETIME => NameLike::Lifetime(ast::Lifetime { syntax }),
            _ => return None,
        };
        Some(res)
    }

    fn syntax(&self) -> Node<'t> {
        match self {
            NameLike::NameRef(it) => it.syntax,
            NameLike::Name(it) => it.syntax,
            NameLike::Lifetime(it) => it.syntax,
        }
    }
}

impl<'t> From<ast::NameRef<'t>> for NameLike<'t> {
    fn from(it: ast::NameRef<'t>) -> Self {
        NameLike::NameRef(it)
    }
}

impl<'t> From<ast::Name<'t>> for NameLike<'t> {
    fn from(it: ast::Name<'t>) -> Self {
        NameLike::Name(it)
    }
}

impl<'t> From<ast::Lifetime<'t>> for NameLike<'t> {
    fn from(it: ast::Lifetime<'t>) -> Self {
        NameLike::Lifetime(it)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NameOrNameRef<'t> {
    Name(ast::Name<'t>),
    NameRef(ast::NameRef<'t>),
}

impl std::fmt::Display for NameOrNameRef<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NameOrNameRef::Name(it) => std::fmt::Display::fmt(it, f),
            NameOrNameRef::NameRef(it) => std::fmt::Display::fmt(it, f),
        }
    }
}

impl<'t> AstNode<'t> for NameOrNameRef<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::NAME | SyntaxKind::NAME_REF)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        let res = match syntax.kind() {
            SyntaxKind::NAME => NameOrNameRef::Name(ast::Name { syntax }),
            SyntaxKind::NAME_REF => NameOrNameRef::NameRef(ast::NameRef { syntax }),
            _ => return None,
        };
        Some(res)
    }

    fn syntax(&self) -> Node<'t> {
        match self {
            NameOrNameRef::NameRef(it) => it.syntax,
            NameOrNameRef::Name(it) => it.syntax,
        }
    }
}

impl<'t> NameOrNameRef<'t> {
    pub fn text(self) -> &'t str {
        match self {
            NameOrNameRef::Name(name) => name.text(),
            NameOrNameRef::NameRef(name_ref) => name_ref.text(),
        }
    }
}

impl<'t> ast::RecordPatField<'t> {
    pub fn for_field_name_ref(field_name: ast::NameRef<'t>) -> Option<ast::RecordPatField<'t>> {
        let candidate = field_name.syntax.parent().and_then(ast::RecordPatField::cast)?;
        match candidate.field_name()? {
            NameOrNameRef::NameRef(name_ref) if name_ref == field_name => Some(candidate),
            _ => None,
        }
    }

    pub fn for_field_name(field_name: ast::Name<'t>) -> Option<ast::RecordPatField<'t>> {
        let candidate =
            field_name.syntax.ancestors().nth(2).and_then(ast::RecordPatField::cast)?;
        match candidate.field_name()? {
            NameOrNameRef::Name(name) if name == field_name => Some(candidate),
            _ => None,
        }
    }

    pub fn parent_record_pat(self) -> ast::RecordPat<'t> {
        self.syntax
            .ancestors()
            .find_map(ast::RecordPat::cast)
            .expect("a RecordPatField is always nested in a RecordPat")
    }

    /// Deals with field init shorthand
    pub fn field_name(self) -> Option<NameOrNameRef<'t>> {
        if let Some(name_ref) = self.name_ref() {
            return Some(NameOrNameRef::NameRef(name_ref));
        }
        match self.pat() {
            Some(ast::Pat::IdentPat(pat)) => Some(NameOrNameRef::Name(pat.name()?)),
            Some(ast::Pat::BoxPat(pat)) => match pat.pat() {
                Some(ast::Pat::IdentPat(pat)) => Some(NameOrNameRef::Name(pat.name()?)),
                _ => None,
            },
            _ => None,
        }
    }
}

impl<'t> ast::Variant<'t> {
    pub fn parent_enum(self) -> ast::Enum<'t> {
        self.syntax
            .parent()
            .and_then(|it| it.parent())
            .and_then(ast::Enum::cast)
            .expect("EnumVariants are always nested in Enums")
    }

    pub fn kind(self) -> StructKind<'t> {
        StructKind::from_node(self.syntax)
    }
}

impl<'t> ast::Item<'t> {
    pub fn generic_param_list(self) -> Option<ast::GenericParamList<'t>> {
        ast::AnyHasGenericParams::cast(self.syntax())?.generic_param_list()
    }
}

impl<'t> ast::Type<'t> {
    pub fn generic_arg_list(self) -> Option<ast::GenericArgList<'t>> {
        if let ast::Type::PathType(path_type) = self {
            path_type.path()?.segment()?.generic_arg_list()
        } else {
            None
        }
    }

    pub fn needs_angles_in_path(self) -> bool {
        !matches!(self, ast::Type::PathType(_)) || self.generic_arg_list().is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind<'t> {
    Name(ast::NameRef<'t>),
    Index(Token<'t>),
}

impl<'t> ast::FieldExpr<'t> {
    pub fn index_token(self) -> Option<Token<'t>> {
        self.syntax
            .children_with_tokens()
            // FIXME: Accepting floats here to reject them in validation later
            .find(|c| c.kind() == SyntaxKind::INT_NUMBER || c.kind() == SyntaxKind::FLOAT_NUMBER)
            .and_then(NodeOrToken::into_token)
    }

    pub fn field_access(self) -> Option<FieldKind<'t>> {
        match self.name_ref() {
            Some(nr) => Some(FieldKind::Name(nr)),
            None => self.index_token().map(FieldKind::Index),
        }
    }
}

/// The patterns of a slice pattern around its rest pattern: `prefix`, then the `slice` (the
/// first pattern that is or binds a `..`), then `suffix`, each read off the tree as it is
/// iterated.
#[derive(Clone)]
pub struct SlicePatComponents<'t> {
    pub prefix: iter::Take<AstChildren<'t, ast::Pat<'t>>>,
    pub slice: Option<ast::Pat<'t>>,
    pub suffix: iter::Skip<AstChildren<'t, ast::Pat<'t>>>,
}

impl<'t> ast::SlicePat<'t> {
    pub fn components(self) -> SlicePatComponents<'t> {
        let prefix = self
            .pats()
            .take_while(|p| match p {
                ast::Pat::RestPat(_) => false,
                ast::Pat::IdentPat(bp) => !matches!(bp.pat(), Some(ast::Pat::RestPat(_))),
                ast::Pat::RefPat(rp) => match rp.pat() {
                    Some(ast::Pat::RestPat(_)) => false,
                    Some(ast::Pat::IdentPat(bp)) => !matches!(bp.pat(), Some(ast::Pat::RestPat(_))),
                    _ => true,
                },
                _ => true,
            })
            .count();
        SlicePatComponents {
            prefix: self.pats().take(prefix),
            slice: self.pats().nth(prefix),
            suffix: self.pats().skip(prefix + 1),
        }
    }
}

impl<'t> ast::IdentPat<'t> {
    pub fn is_simple_ident(self) -> bool {
        self.at_token().is_none()
            && self.mut_token().is_none()
            && self.ref_token().is_none()
            && self.pat().is_none()
    }
}

impl<'t> ast::SelfParam<'t> {
    pub fn kind(self) -> SelfParamKind {
        if self.amp_token().is_some() {
            if self.mut_token().is_some() { SelfParamKind::MutRef } else { SelfParamKind::Ref }
        } else {
            SelfParamKind::Owned
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeBoundKind<'t> {
    /// Trait
    PathType(Option<ast::ForBinder<'t>>, ast::PathType<'t>),
    /// use
    Use(ast::UseBoundGenericArgs<'t>),
    /// 'a
    Lifetime(ast::Lifetime<'t>),
}

impl<'t> ast::TypeBound<'t> {
    /// # Panics
    ///
    /// On a bound with none of a path type, a `for` type, `use<..>` or a lifetime in it, which
    /// the parser never makes.
    pub fn kind(self) -> Option<TypeBoundKind<'t>> {
        if let Some(path_type) = support::child::<ast::PathType<'t>>(self.syntax) {
            Some(TypeBoundKind::PathType(self.for_binder(), path_type))
        } else if let Some(for_binder) = support::child::<ast::ForType<'t>>(self.syntax) {
            let Some(ast::Type::PathType(path_type)) = for_binder.ty() else { return None };
            Some(TypeBoundKind::PathType(for_binder.for_binder(), path_type))
        } else if let Some(args) = self.use_bound_generic_args() {
            Some(TypeBoundKind::Use(args))
        } else if let Some(lifetime) = self.lifetime() {
            Some(TypeBoundKind::Lifetime(lifetime))
        } else {
            panic!("a type bound at {:?} has no bound in it", self.syntax.text_range())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeOrConstParam<'t> {
    Type(ast::TypeParam<'t>),
    Const(ast::ConstParam<'t>),
}

impl<'t> From<TypeOrConstParam<'t>> for ast::GenericParam<'t> {
    fn from(value: TypeOrConstParam<'t>) -> Self {
        match value {
            TypeOrConstParam::Type(it) => ast::GenericParam::TypeParam(it),
            TypeOrConstParam::Const(it) => ast::GenericParam::ConstParam(it),
        }
    }
}

impl<'t> TypeOrConstParam<'t> {
    pub fn name(self) -> Option<ast::Name<'t>> {
        match self {
            TypeOrConstParam::Type(x) => x.name(),
            TypeOrConstParam::Const(x) => x.name(),
        }
    }
}

impl<'t> AstNode<'t> for TypeOrConstParam<'t> {
    fn can_cast(kind: SyntaxKind) -> bool {
        matches!(kind, SyntaxKind::TYPE_PARAM | SyntaxKind::CONST_PARAM)
    }

    fn cast(syntax: Node<'t>) -> Option<Self> {
        let res = match syntax.kind() {
            SyntaxKind::TYPE_PARAM => TypeOrConstParam::Type(ast::TypeParam { syntax }),
            SyntaxKind::CONST_PARAM => TypeOrConstParam::Const(ast::ConstParam { syntax }),
            _ => return None,
        };
        Some(res)
    }

    fn syntax(&self) -> Node<'t> {
        match self {
            TypeOrConstParam::Type(it) => it.syntax,
            TypeOrConstParam::Const(it) => it.syntax,
        }
    }
}

impl<'t> HasAttrs<'t> for TypeOrConstParam<'t> {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibilityKind<'t> {
    In(ast::Path<'t>),
    PubCrate,
    PubSuper,
    PubSelf,
    Pub,
}

impl<'t> ast::Visibility<'t> {
    pub fn kind(self) -> VisibilityKind<'t> {
        match self.visibility_inner() {
            Some(inner) => inner.kind(),
            None => VisibilityKind::Pub,
        }
    }
}

impl<'t> ast::VisibilityInner<'t> {
    pub fn kind(self) -> VisibilityKind<'t> {
        match self.path() {
            Some(path) => {
                if let Some(segment) =
                    path.as_single_segment().filter(|it| it.coloncolon_token().is_none())
                {
                    if segment.crate_token().is_some() {
                        return VisibilityKind::PubCrate;
                    } else if segment.super_token().is_some() {
                        return VisibilityKind::PubSuper;
                    } else if segment.self_token().is_some() {
                        return VisibilityKind::PubSelf;
                    }
                }
                VisibilityKind::In(path)
            }
            None => VisibilityKind::Pub,
        }
    }
}

impl<'t> ast::LifetimeParam<'t> {
    pub fn lifetime_bounds(self) -> impl Iterator<Item = Token<'t>> + use<'t> {
        self.type_bound_list()
            .into_iter()
            .flat_map(|it| it.bounds())
            .filter_map(|it| it.lifetime()?.lifetime_ident_token())
    }
}

impl<'t> ast::Module<'t> {
    /// Returns the parent ast::Module, this is different than the semantic parent in that this only
    /// considers parent declarations in the AST
    pub fn parent(self) -> Option<ast::Module<'t>> {
        self.syntax.ancestors().nth(2).and_then(ast::Module::cast)
    }
}

/// Whether a token is written between the ends of a range pattern: `..`, `..=`, or the `...`
/// that editions before 2021 take for `..=`.
#[inline]
fn is_range_pat_op(kind: SyntaxKind) -> bool {
    matches!(kind, T![..] | T![..=] | T![...])
}

impl<'t> RangeItem<'t> for ast::RangePat<'t> {
    type Bound = ast::Pat<'t>;

    fn start(&self) -> Option<ast::Pat<'t>> {
        self.syntax
            .children_with_tokens()
            .take_while(|it| !is_range_pat_op(it.kind()))
            .filter_map(|it| it.into_node())
            .find_map(ast::Pat::cast)
    }

    fn end(&self) -> Option<ast::Pat<'t>> {
        self.syntax
            .children_with_tokens()
            .skip_while(|it| !is_range_pat_op(it.kind()))
            .filter_map(|it| it.into_node())
            .find_map(ast::Pat::cast)
    }

    fn op_token(&self) -> Option<Token<'t>> {
        self.syntax
            .children_with_tokens()
            .find_map(|it| it.into_token().filter(|token| is_range_pat_op(token.kind())))
    }

    fn op_kind(&self) -> Option<RangeOp> {
        self.syntax.children_with_tokens().find_map(|it| match it.into_token()?.kind() {
            T![..] => Some(RangeOp::Exclusive),
            // `...` is how an inclusive range pattern was written before `..=`.
            T![..=] | T![...] => Some(RangeOp::Inclusive),
            _ => None,
        })
    }
}

impl<'t> ast::TokenTree<'t> {
    pub fn token_trees_and_tokens(
        self,
    ) -> impl Iterator<Item = NodeOrToken<ast::TokenTree<'t>, Token<'t>>> + use<'t> {
        self.syntax.children_with_tokens().filter_map(|not| match not {
            NodeOrToken::Node(node) => ast::TokenTree::cast(node).map(NodeOrToken::Node),
            NodeOrToken::Token(t) => Some(NodeOrToken::Token(t)),
        })
    }

    pub fn left_delimiter_token(self) -> Option<Token<'t>> {
        self.syntax
            .first_child_or_token()?
            .into_token()
            .filter(|it| matches!(it.kind(), T!['{'] | T!['('] | T!['[']))
    }

    pub fn right_delimiter_token(self) -> Option<Token<'t>> {
        self.syntax
            .last_child_or_token()?
            .into_token()
            .filter(|it| matches!(it.kind(), T!['}'] | T![')'] | T![']']))
    }

    pub fn parent_meta(self) -> Option<ast::Meta<'t>> {
        self.syntax.parent().and_then(ast::Meta::cast)
    }
}

impl<'t> ast::GenericArgList<'t> {
    pub fn lifetime_args(self) -> impl Iterator<Item = ast::LifetimeArg<'t>> + use<'t> {
        self.generic_args().filter_map(|arg| match arg {
            ast::GenericArg::LifetimeArg(it) => Some(it),
            _ => None,
        })
    }
}

impl<'t> ast::GenericParamList<'t> {
    pub fn lifetime_params(self) -> impl Iterator<Item = ast::LifetimeParam<'t>> + use<'t> {
        self.generic_params().filter_map(|param| match param {
            ast::GenericParam::LifetimeParam(it) => Some(it),
            ast::GenericParam::TypeParam(_) | ast::GenericParam::ConstParam(_) => None,
        })
    }

    pub fn type_or_const_params(self) -> impl Iterator<Item = TypeOrConstParam<'t>> + use<'t> {
        self.generic_params().filter_map(|param| match param {
            ast::GenericParam::TypeParam(it) => Some(TypeOrConstParam::Type(it)),
            ast::GenericParam::LifetimeParam(_) => None,
            ast::GenericParam::ConstParam(it) => Some(TypeOrConstParam::Const(it)),
        })
    }
}

impl<'t> ast::ArgList<'t> {
    /// Comma separated args, argument may be empty
    pub fn args_maybe_empty(self) -> impl Iterator<Item = Option<ast::Expr<'t>>> + use<'t> {
        // (Expr? ','?)*
        let mut after_arg = false;
        self.syntax.children_with_tokens().filter_map(move |it| {
            if into_comma(it).is_some() {
                if std::mem::take(&mut after_arg) { None } else { Some(None) }
            } else {
                Some(ast::Expr::cast(it.into_node()?).inspect(|_| after_arg = true))
            }
        })
    }
}

impl<'t> ast::ForExpr<'t> {
    pub fn iterable(self) -> Option<ast::Expr<'t>> {
        // If the iterable is a BlockExpr, check if the body is missing.
        // If it is assume the iterable is the expression that is missing instead.
        let mut exprs = support::children(self.syntax);
        let first = exprs.next();
        match first {
            Some(ast::Expr::BlockExpr(_)) => exprs.next().and(first),
            first => first,
        }
    }
}

impl<'t> HasLoopBody<'t> for ast::ForExpr<'t> {
    fn loop_body(&self) -> Option<ast::BlockExpr<'t>> {
        let mut exprs = support::children(self.syntax);
        let first = exprs.next();
        let second = exprs.next();
        second.or(first)
    }
}

impl<'t> ast::WhileExpr<'t> {
    pub fn condition(self) -> Option<ast::Expr<'t>> {
        // If the condition is a BlockExpr, check if the body is missing.
        // If it is assume the condition is the expression that is missing instead.
        let mut exprs = support::children(self.syntax);
        let first = exprs.next();
        match first {
            Some(ast::Expr::BlockExpr(_)) => exprs.next().and(first),
            first => first,
        }
    }
}

impl<'t> HasLoopBody<'t> for ast::WhileExpr<'t> {
    fn loop_body(&self) -> Option<ast::BlockExpr<'t>> {
        let mut exprs = support::children(self.syntax);
        let first = exprs.next();
        let second = exprs.next();
        second.or(first)
    }
}

impl<'t> From<ast::Adt<'t>> for ast::Item<'t> {
    fn from(it: ast::Adt<'t>) -> Self {
        match it {
            ast::Adt::Enum(it) => ast::Item::Enum(it),
            ast::Adt::Struct(it) => ast::Item::Struct(it),
            ast::Adt::Union(it) => ast::Item::Union(it),
        }
    }
}

impl<'t> ast::MatchGuard<'t> {
    pub fn condition(self) -> Option<ast::Expr<'t>> {
        support::child(self.syntax)
    }
}

impl<'t> ast::MatchArm<'t> {
    pub fn parent_match(self) -> ast::MatchExpr<'t> {
        self.syntax
            .parent()
            .and_then(|it| it.parent())
            .and_then(ast::MatchExpr::cast)
            .expect("MatchArms are always nested in MatchExprs")
    }
}

impl<'t> From<ast::Item<'t>> for ast::AnyHasAttrs<'t> {
    fn from(node: ast::Item<'t>) -> Self {
        Self::new(node)
    }
}

impl<'t> From<ast::AssocItem<'t>> for ast::AnyHasAttrs<'t> {
    fn from(node: ast::AssocItem<'t>) -> Self {
        Self::new(node)
    }
}

impl<'t> ast::OrPat<'t> {
    pub fn leading_pipe(self) -> Option<Token<'t>> {
        self.syntax
            .children_with_tokens()
            .find(|it| !it.kind().is_trivia())
            .and_then(NodeOrToken::into_token)
            .filter(|it| it.kind() == T![|])
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CfgAtomKey<'t> {
    True,
    False,
    Ident(Token<'t>),
}

impl<'t> ast::CfgAtom<'t> {
    pub fn key(self) -> Option<CfgAtomKey<'t>> {
        if self.true_token().is_some() {
            Some(CfgAtomKey::True)
        } else if self.false_token().is_some() {
            Some(CfgAtomKey::False)
        } else {
            self.ident_token().map(CfgAtomKey::Ident)
        }
    }
}

/// An iterator over the elements in an [`ast::TokenTree`].
///
/// Does not yield trivia or the delimiters.
#[derive(Clone)]
pub struct TokenTreeChildren<'t> {
    iter: ChildrenWithTokens<'t>,
}

impl<'t> TokenTreeChildren<'t> {
    #[inline]
    pub fn new(tt: ast::TokenTree<'t>) -> Self {
        let mut iter = tt.syntax.children_with_tokens();
        iter.next(); // Bump the opening delimiter.
        Self { iter }
    }
}

impl<'t> Iterator for TokenTreeChildren<'t> {
    type Item = NodeOrToken<ast::TokenTree<'t>, Token<'t>>;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        self.iter.find_map(|item| match item {
            NodeOrToken::Node(node) => ast::TokenTree::cast(node).map(NodeOrToken::Node),
            NodeOrToken::Token(token) => {
                let kind = token.kind();
                (!matches!(
                    kind,
                    SyntaxKind::WHITESPACE | SyntaxKind::COMMENT | T![')'] | T![']'] | T!['}']
                ))
                .then_some(NodeOrToken::Token(token))
            }
        })
    }
}
