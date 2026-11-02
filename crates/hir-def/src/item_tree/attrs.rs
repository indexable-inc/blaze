//! Defines attribute helpers for name resolution.
//!
//! Notice we don't preserve all attributes for name resolution, to save space:
//! for example, we skip doc comments (desugared to `#[doc = "..."]` attributes)
//! and `#[inline]`. The filtered attributes are listed in [`hir_expand::attrs`].

use std::{
    borrow::Cow,
    convert::Infallible,
    ops::{self, ControlFlow},
};

use base_db::Db;
use cfg::{CfgExpr, CfgOptions};
use either::Either;
use hir_expand::{
    attrs::{Attr, AttrId, AttrInput, collect_item_tree_attrs_view},
    mod_path::ModPath,
    name::Name,
};
use intern::{Symbol, sym};
use syntax::view::{self as ast, AstNode};
use syntax_bridge::DocCommentDesugarMode;
use tt::token_to_literal;

/// The attributes of one item, owned: what lowering one item's attributes alone gives (an
/// [`ItemTree`](super::ItemTree) keeps every item's in one array, read as [`AttrsOrCfgRef`]).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttrsOrCfg {
    Enabled {
        attrs: AttrsOwned,
    },
    /// This only collects the attributes up to the disabled `cfg` (this is what needed for crate-level attributes.)
    CfgDisabled(Box<(CfgExpr, AttrsOwned)>),
}

impl Default for AttrsOrCfg {
    #[inline]
    fn default() -> Self {
        AttrsOrCfg::Enabled { attrs: AttrsOwned(Box::new([])) } // alloc: none, an empty slice
    }
}

impl AttrsOrCfg {
    pub(crate) fn lower<'t, 'a, S>(
        db: &Db,
        owner: impl ast::HasAttrs<'t>,
        cfg_options: &impl Fn() -> &'a CfgOptions,
        span_map: S,
    ) -> AttrsOrCfg
    where
        S: syntax_bridge::SpanMapper + Copy,
    {
        // alloc: the owner's attributes, the value; none for an owner without any
        let mut attrs = Vec::new();
        let cfg = lower_attrs_into(
            db,
            ast::AnyHasAttrs::new(owner),
            cfg_options,
            span_map,
            &mut attrs,
        );
        let attrs = AttrsOwned(attrs.into_boxed_slice());
        match cfg {
            // alloc: once an owner `cfg`d out, which is lowered no further
            Some(cfg) => AttrsOrCfg::CfgDisabled(Box::new((cfg, attrs))),
            None => AttrsOrCfg::Enabled { attrs },
        }
    }
}

/// Appends `owner`'s attributes to `attrs`, and returns the `cfg` that disables it, if one
/// does: the attributes appended are then those before that `cfg` (what crate-level attributes
/// need), so lowering a second owner after a disabled one is never needed. One instance for
/// every kind of owner.
pub(super) fn lower_attrs_into<'t, 'a, S>(
    db: &Db,
    owner: ast::AnyHasAttrs<'t>,
    cfg_options: &impl Fn() -> &'a CfgOptions,
    span_map: S,
    attrs: &mut Vec<Attr>,
) -> Option<CfgExpr>
where
    S: syntax_bridge::SpanMapper + Copy,
{
    let result = collect_item_tree_attrs_view::<Infallible>(owner, cfg_options, |meta, _| {
        // NOTE: We cannot early return from this function, *every* attribute must be pushed, otherwise we'll mess the `AttrId`
        // tracking.
        let path = meta.path();
        let path_range =
            path.map_or_else(|| meta.syntax().text_range(), |path| path.syntax().text_range());
        let (span, input) = match meta {
            ast::Meta::KeyValueMeta(meta) => {
                let span = span_map.span_for(path_range);
                let input = meta.expr().and_then(|value| {
                    if let ast::Expr::Literal(value) = value {
                        Some(
                            AttrInput::Literal(token_to_literal(
                                value.token().text(),
                                span_map.span_for(value.syntax().text_range()),
                            ))
                            .into(),
                        )
                    } else {
                        None
                    }
                });
                (span, input)
            }
            ast::Meta::TokenTreeMeta(meta) => {
                let span = span_map.span_for(path_range);
                let token_tree = meta.token_tree();
                let tt = syntax_bridge::call_input(
                    token_tree.map_or(meta.syntax(), |it| it.syntax()),
                    span_map,
                    span,
                    DocCommentDesugarMode::ProcMacro,
                );
                let input = Some(AttrInput::TokenTree(tt).into());
                (span, input)
            }
            ast::Meta::PathMeta(_) => {
                let span = span_map.span_for(path_range);
                (span, None)
            }
            ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_) | ast::Meta::UnsafeMeta(_) => {
                unreachable!(
                    "`cfg`, `cfg_attr` and `unsafe(...)` are handled in `collect_item_tree_attrs()`"
                )
            }
        };

        let path = path.and_then(|path| {
            ModPath::from_src_view(db, path, &mut |range| span_map.span_for(range).ctx)
        });
        let path = path.unwrap_or_else(|| Name::missing().into());

        attrs.push(Attr { path, input, ctxt: span.ctx });
        ControlFlow::Continue(())
    });
    match result {
        Some(Either::Right(cfg)) => Some(cfg),
        None => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AttrsOwned(Box<[Attr]>);

#[derive(Debug, Clone, Copy)]
pub(crate) struct Attrs<'a>(&'a [Attr]);

impl ops::Deref for Attrs<'_> {
    type Target = [Attr];

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.0
    }
}

/// The attributes of an item of an [`ItemTree`](super::ItemTree), borrowed from its columns.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AttrsOrCfgRef<'a> {
    Enabled { attrs: Attrs<'a> },
    /// The `cfg` that is false, and the attributes before it.
    CfgDisabled(&'a CfgExpr, Attrs<'a>),
}

impl AttrsOwned {
    #[inline]
    pub(crate) fn as_ref(&self) -> Attrs<'_> {
        Attrs(&self.0)
    }
}

impl<'a> Attrs<'a> {
    pub(crate) const EMPTY: Self = Attrs(&[]);

    #[inline]
    pub(super) fn new(attrs: &'a [Attr]) -> Attrs<'a> {
        Attrs(attrs)
    }

    #[inline]
    pub(crate) fn by_key(self, key: Symbol) -> AttrQuery<'a> {
        AttrQuery { attrs: self, key }
    }

    #[inline]
    pub(crate) fn iter(self) -> impl Iterator<Item = (AttrId, &'a Attr)> {
        self.0.iter().enumerate().map(|(id, attr)| (AttrId::from_item_tree_index(id as u32), attr))
    }

    #[inline]
    pub(crate) fn iter_after(
        self,
        after: Option<AttrId>,
    ) -> impl Iterator<Item = (AttrId, &'a Attr)> {
        let skip = after.map_or(0, |after| after.item_tree_index() + 1);
        self.0[skip as usize..]
            .iter()
            .enumerate()
            .map(move |(id, attr)| (AttrId::from_item_tree_index(id as u32 + skip), attr))
    }

    #[inline]
    pub(crate) fn is_proc_macro(&self) -> bool {
        self.by_key(sym::proc_macro).exists()
    }

    #[inline]
    pub(crate) fn is_proc_macro_attribute(&self) -> bool {
        self.by_key(sym::proc_macro_attribute).exists()
    }
}
#[derive(Debug, Clone)]
pub(crate) struct AttrQuery<'attr> {
    attrs: Attrs<'attr>,
    key: Symbol,
}

impl<'attr> AttrQuery<'attr> {
    #[inline]
    pub(crate) fn tt_values(self) -> impl Iterator<Item = &'attr crate::tt::TopSubtree> {
        self.attrs().filter_map(|attr| attr.token_tree_value())
    }

    #[inline]
    pub(crate) fn string_value_with_span(self) -> Option<(&'attr str, span::Span)> {
        self.attrs().find_map(|attr| attr.string_value_with_span())
    }

    #[inline]
    pub(crate) fn string_value_unescape(self) -> Option<Cow<'attr, str>> {
        self.attrs().find_map(|attr| attr.string_value_unescape())
    }

    #[inline]
    pub(crate) fn exists(self) -> bool {
        self.attrs().next().is_some()
    }

    #[inline]
    pub(crate) fn attrs(self) -> impl Iterator<Item = &'attr Attr> + Clone {
        let key = self.key;
        self.attrs.0.iter().filter(move |attr| attr.path.as_ident().is_some_and(|s| *s == key))
    }
}
