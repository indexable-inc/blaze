//! Defines the basics of attributes lowering.
//!
//! The heart and soul of this module is [`expand_cfg_attr_view()`], alongside its sibling
//! [`expand_cfg_attr_with_doc_comments_view()`]. It is used to implement all attribute lowering.
//! Its basic job is to list attributes; however, attributes do not necessarily map
//! into [`view::Attr`], because `cfg_attr` can map to zero, one, or more attributes
//! (`#[cfg_attr(predicate, attr1, attr2, ...)]`). [`expand_cfg_attr_view()`] expands `cfg_attr`s
//! as it goes (as its name implies), to list all attributes.
//!
//! Another thing to note is that we need to be able to map an attribute back to a range
//! (for diagnostic purposes etc.). This is only ever needed for attributes that participate
//! in name resolution. An attribute is mapped back by its [`AttrId`], which is just an
//! index into the item tree attributes list. To minimize the risk of bugs, we have one
//! place (here) and one function ([`is_item_tree_filtered_attr()`]) that decides whether
//! an attribute participate in name resolution.

use std::{borrow::Cow, cell::OnceCell, convert::Infallible, fmt, ops::ControlFlow};

use ::tt::TextRange;
use base_db::{Db, Crate};
use blaze_mem::{Tag, frame};
use cfg::{CfgExpr, CfgOptions};
use either::Either;
use itertools::Itertools;
use mbe::{DelimiterKind, Punct};
use smallvec::SmallVec;
use span::{RealSpanMap, Span, SyntaxContext};
use syntax::{
    AstNode, SmolStr, ast, unescape,
    view::{self, AstNode as _},
};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    AstId,
    mod_path::ModPath,
    tt::{self, TopSubtree},
};

pub trait AstPathExt {
    fn is1(&self, segment: &str) -> bool;

    fn as_one_segment(&self) -> Option<SmolStr>;

    fn as_up_to_two_segment(&self) -> Option<(SmolStr, Option<SmolStr>)>;
}

/// `f` of the text of the path's one segment, without `r#`, read in place.
pub(crate) fn with_one_segment_view<R>(
    path: &view::Path<'_>,
    f: impl FnOnce(&str) -> R,
) -> Option<R> {
    let name_ref = path.as_single_name_ref()?;
    Some(f(name_ref.text().trim_start_matches("r#")))
}

impl AstPathExt for view::Path<'_> {
    fn is1(&self, segment: &str) -> bool {
        with_one_segment_view(self, |it| it == segment).unwrap_or(false)
    }

    fn as_one_segment(&self) -> Option<SmolStr> {
        with_one_segment_view(self, |it| SmolStr::new(it))
    }

    fn as_up_to_two_segment(&self) -> Option<(SmolStr, Option<SmolStr>)> {
        let parent = self.qualifier().as_one_segment();
        let this = self.segment()?.name_ref()?.text().trim_start_matches("r#").into();
        if let Some(parent) = parent { Some((parent, Some(this))) } else { Some((this, None)) }
    }
}

impl AstPathExt for Option<view::Path<'_>> {
    fn is1(&self, segment: &str) -> bool {
        self.is_some_and(|it| it.is1(segment))
    }

    fn as_one_segment(&self) -> Option<SmolStr> {
        self.and_then(|it| it.as_one_segment())
    }

    fn as_up_to_two_segment(&self) -> Option<(SmolStr, Option<SmolStr>)> {
        self.and_then(|it| it.as_up_to_two_segment())
    }
}

pub trait AstKeyValueMetaExt {
    fn value_string(&self) -> Option<SmolStr>;
}

impl AstKeyValueMetaExt for view::KeyValueMeta<'_> {
    fn value_string(&self) -> Option<SmolStr> {
        if let Some(view::Expr::Literal(value)) = self.expr()
            && let view::LiteralKind::String(value) = value.kind()
            && let Ok(value) = value.value()
        {
            Some((*value).into())
        } else {
            None
        }
    }
}

/// The callback is passed each attribute `cfg_attr` leaves, in order, and the outermost
/// [`view::Attr`] it is in. `unsafe(attr)` are passed the inner attribute for now.
#[inline]
pub fn expand_cfg_attr_view<'t, 'a, BreakValue>(
    attrs: impl Iterator<Item = view::Attr<'t>>,
    cfg_options: impl FnMut() -> &'a CfgOptions,
    mut callback: impl FnMut(view::Meta<'t>, view::Attr<'t>) -> ControlFlow<BreakValue>,
) -> Option<BreakValue> {
    expand_cfg_attr_with_doc_comments_view::<Infallible, _>(
        attrs.map(Either::Left),
        cfg_options,
        move |Either::Left((meta, top_attr))| callback(meta, top_attr),
    )
}

/// [`expand_cfg_attr_view`] with the doc comments passed through in order.
pub fn expand_cfg_attr_with_doc_comments_view<'t, 'a, DocComment, BreakValue>(
    mut attrs: impl Iterator<Item = Either<view::Attr<'t>, DocComment>>,
    mut cfg_options: impl FnMut() -> &'a CfgOptions,
    mut callback: impl FnMut(
        Either<(view::Meta<'t>, view::Attr<'t>), DocComment>,
    ) -> ControlFlow<BreakValue>,
) -> Option<BreakValue> {
    // alloc: none until a `cfg_attr` holds more than one attribute
    let mut stack = SmallVec::<[(view::Meta<'t>, view::Attr<'t>); 1]>::new();
    'process_attrs: loop {
        let (mut meta, top_attr) = match stack.pop() {
            Some(it) => it,
            None => match attrs.next()? {
                Either::Left(attr) => {
                    let Some(meta) = attr.meta() else { continue };
                    (meta, attr)
                }
                Either::Right(doc_comment) => {
                    if let ControlFlow::Break(break_value) = callback(Either::Right(doc_comment)) {
                        return Some(break_value);
                    }
                    continue;
                }
            },
        };

        while let view::Meta::UnsafeMeta(unsafe_meta) = meta {
            let Some(inner) = unsafe_meta.meta() else { continue 'process_attrs };
            meta = inner;
        }

        if let view::Meta::CfgAttrMeta(meta) = meta {
            let Some(cfg_predicate) = meta.cfg_predicate() else { continue };
            let cfg_predicate = CfgExpr::parse_from_view(cfg_predicate);
            if cfg_options().check(&cfg_predicate) != Some(false) {
                let prev_stack_len = stack.len();
                stack.extend(meta.metas().map(|meta| (meta, top_attr)));
                stack[prev_stack_len..].reverse();
            }
        } else if let ControlFlow::Break(break_value) = callback(Either::Left((meta, top_attr))) {
            return Some(break_value);
        }
    }
}

/// [`expand_cfg_attr_view`] as an iterator, for a caller with the cfg in hand: each attribute
/// `cfg_attr` leaves, in order, with the outermost [`view::Attr`] it is in.
pub fn cfg_attrs_view<'t, 'a, I: Iterator<Item = view::Attr<'t>>>(
    attrs: I,
    cfg_options: &'a CfgOptions,
) -> CfgAttrsView<'t, 'a, I> {
    CfgAttrsView { attrs, cfg_options, stack: SmallVec::new() }
}

/// What [`cfg_attrs_view`] gives.
pub struct CfgAttrsView<'t, 'a, I> {
    attrs: I,
    cfg_options: &'a CfgOptions,
    // alloc: none until a `cfg_attr` holds more than one attribute
    stack: SmallVec<[(view::Meta<'t>, view::Attr<'t>); 1]>,
}

impl<'t, I: Iterator<Item = view::Attr<'t>>> Iterator for CfgAttrsView<'t, '_, I> {
    type Item = (view::Meta<'t>, view::Attr<'t>);

    fn next(&mut self) -> Option<Self::Item> {
        'process_attrs: loop {
            let (mut meta, top_attr) = match self.stack.pop() {
                Some(it) => it,
                None => {
                    let attr = self.attrs.next()?;
                    let Some(meta) = attr.meta() else { continue };
                    (meta, attr)
                }
            };
            while let view::Meta::UnsafeMeta(unsafe_meta) = meta {
                let Some(inner) = unsafe_meta.meta() else { continue 'process_attrs };
                meta = inner;
            }
            let view::Meta::CfgAttrMeta(meta) = meta else { return Some((meta, top_attr)) };
            let Some(cfg_predicate) = meta.cfg_predicate() else { continue };
            if self.cfg_options.check(&CfgExpr::parse_from_view(cfg_predicate)) != Some(false) {
                let prev_stack_len = self.stack.len();
                self.stack.extend(meta.metas().map(|meta| (meta, top_attr)));
                self.stack[prev_stack_len..].reverse();
            }
        }
    }
}

/// The builtin attributes the item tree leaves out.
const ITEM_TREE_FILTERED_ATTRS: &[&str] = &[
    "doc",
    "stable",
    "unstable",
    "target_feature",
    "allow",
    "expect",
    "warn",
    "deny",
    "forbid",
    "repr",
    "inline",
    "track_caller",
    "must_use",
];

blaze_phf::phf_table!(static ITEM_TREE_FILTERED = ITEM_TREE_FILTERED_ATTRS;);

/// Whether the item tree leaves out the attribute `name`: one hash of its bytes and one compare.
#[inline]
pub(crate) fn is_item_tree_filtered_attr(name: &str) -> bool {
    ITEM_TREE_FILTERED.get(name).is_some()
}

/// This collects attributes exactly as the item tree needs them, of a view of the owner. This is
/// used for the item tree, as well as for resolving [`AttrId`]s. One concrete owner type, so one
/// instance and no `dyn`.
pub fn collect_item_tree_attrs_view<'t, 'a, BreakValue>(
    owner: view::AnyHasAttrs<'t>,
    cfg_options: impl Fn() -> &'a CfgOptions,
    mut on_attr: impl FnMut(view::Meta<'t>, view::Attr<'t>) -> ControlFlow<BreakValue>,
) -> Option<Either<BreakValue, CfgExpr>> {
    expand_cfg_attr_view(
        view::attrs_including_inner(&owner),
        || cfg_options(),
        |attr, top_attr| {
            // We filter builtin attributes that we don't need for nameres, because this saves memory.
            // For an attribute to be filtered out, it *must* not be shadowable with a macro!
            let filter = match attr {
                view::Meta::CfgMeta(attr) => {
                    let Some(cfg_predicate) = attr.cfg_predicate() else {
                        return ControlFlow::Continue(());
                    };
                    let cfg = CfgExpr::parse_from_view(cfg_predicate);
                    if cfg_options().check(&cfg) == Some(false) {
                        return ControlFlow::Break(Either::Right(cfg));
                    }
                    true
                }
                _ => attr.path().is_some_and(|path| {
                    with_one_segment_view(&path, is_item_tree_filtered_attr) == Some(true)
                }),
            };
            if !filter && let ControlFlow::Break(v) = on_attr(attr, top_attr) {
                return ControlFlow::Break(Either::Left(v));
            }
            ControlFlow::Continue(())
        },
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub path: ModPath,
    /// Shared: an item tree assembled from its items' trees clones every attribute of the file
    /// on each edit, and a clone is then one count.
    pub input: Option<triomphe::Arc<AttrInput>>,
    pub ctxt: SyntaxContext,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub enum AttrInput {
    /// `#[attr = "string"]`
    Literal(tt::Literal),
    /// `#[attr(subtree)]`
    TokenTree(tt::TopSubtree),
}

impl fmt::Display for AttrInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AttrInput::Literal(lit) => write!(f, " = {lit}"),
            AttrInput::TokenTree(tt) => tt.fmt(f),
        }
    }
}

impl Attr {
    /// #[path = "string"]
    pub fn string_value(&self) -> Option<&str> {
        match self.input.as_deref()? {
            AttrInput::Literal(
                lit @ tt::Literal { kind: tt::LitKind::Str | tt::LitKind::StrRaw(_), .. },
            ) => Some(lit.text()),
            _ => None,
        }
    }

    /// #[path = "string"]
    pub fn string_value_with_span(&self) -> Option<(&str, span::Span)> {
        match self.input.as_deref()? {
            AttrInput::Literal(
                lit @ tt::Literal { kind: tt::LitKind::Str | tt::LitKind::StrRaw(_), span, .. },
            ) => Some((lit.text(), *span)),
            _ => None,
        }
    }

    pub fn string_value_unescape(&self) -> Option<Cow<'_, str>> {
        match self.input.as_deref()? {
            AttrInput::Literal(lit @ tt::Literal { kind: tt::LitKind::StrRaw(_), .. }) => {
                Some(Cow::Borrowed(lit.text()))
            }
            AttrInput::Literal(lit @ tt::Literal { kind: tt::LitKind::Str, .. }) => {
                unescape(lit.text())
            }
            _ => None,
        }
    }

    /// #[path(ident)]
    pub fn single_ident_value(&self) -> Option<tt::Ident> {
        match self.input.as_deref()? {
            AttrInput::TokenTree(tt) => match tt.token_trees().iter().collect_array() {
                Some([tt::TtElement::Leaf(tt::Leaf::Ident(ident))]) => Some(ident),
                _ => None,
            },
            _ => None,
        }
    }

    /// #[path TokenTree]
    pub fn token_tree_value(&self) -> Option<&TopSubtree> {
        match self.input.as_deref()? {
            AttrInput::TokenTree(tt) => Some(tt),
            _ => None,
        }
    }

    /// Parses this attribute as a token tree consisting of comma separated paths.
    pub fn parse_path_comma_token_tree<'a>(
        &'a self,
        db: &'a Db,
    ) -> Option<impl Iterator<Item = (ModPath, Span, tt::TokenTreesView<'a>)> + 'a> {
        let args = self.token_tree_value()?;

        if args.top_subtree().delimiter.kind != DelimiterKind::Parenthesis {
            return None;
        }
        Some(parse_path_comma_token_tree(db, args))
    }
}

fn parse_path_comma_token_tree<'a>(
    db: &'a Db,
    args: &'a tt::Tree,
) -> impl Iterator<Item = (ModPath, Span, tt::TokenTreesView<'a>)> {
    args.token_trees()
        .split(|tt| matches!(tt, tt::TtElement::Leaf(tt::Leaf::Punct(Punct { char: ',', .. }))))
        .filter_map(move |tts| {
            let span = tts.first_span()?;
            Some((ModPath::from_tt(db, tts)?, span, tts))
        })
}

fn unescape(s: &str) -> Option<Cow<'_, str>> {
    let mut buf = String::new(); // alloc: none until the first escape (`reserve_exact` then)
    let mut prev_end = 0;
    let mut has_error = false;
    unescape::unescape_str(s, |char_range, unescaped_char| {
        match (unescaped_char, buf.capacity() == 0) {
            (Ok(c), false) => buf.push(c),
            (Ok(_), true) if char_range.len() == 1 && char_range.start == prev_end => {
                prev_end = char_range.end
            }
            (Ok(c), true) => {
                buf.reserve_exact(s.len());
                buf.push_str(&s[..prev_end]);
                buf.push(c);
            }
            (Err(_), _) => has_error = true,
        }
    });

    match (has_error, buf.capacity() == 0) {
        (true, _) => None,
        (false, false) => Some(Cow::Owned(buf)),
        (false, true) => Some(Cow::Borrowed(s)),
    }
}

/// This is an index of an attribute *that always points to the item tree attributes*.
///
/// Outer attributes are counted first, then inner attributes. This does not support
/// out-of-line modules, which may have attributes spread across 2 files!
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AttrId {
    id: u32,
}

impl AttrId {
    #[inline]
    pub fn from_item_tree_index(id: u32) -> Self {
        Self { id }
    }

    #[inline]
    pub fn item_tree_index(self) -> u32 {
        self.id
    }

    #[cold]
    fn incorrect(self, krate: Crate) -> ! {
        panic!("used an incorrect `AttrId`; crate={krate:?}, attr_id={self:?}");
    }

    /// The containing `Attr` (note that it may contain other attributes as well due to
    /// `cfg_attr`) and its `Meta`.
    pub fn find_attr_range_view<'db, N: AstNode + view::Typed>(
        self,
        db: &'db Db,
        krate: Crate,
        owner: AstId<N>,
    ) -> (view::Attr<'db>, view::Meta<'db>)
    where
        N::Of<'db>: view::HasAttrs<'db>,
    {
        self.find_attr_range_with_source_view(db, krate, view::AnyHasAttrs::new(owner.to_view(db)))
    }

    /// [`Self::find_attr_range_view`] of a view of the owner, which must be in the original file
    /// (not speculatively expanded macro output).
    pub fn find_attr_range_with_source_view<'t>(
        self,
        db: &Db,
        krate: Crate,
        owner: view::AnyHasAttrs<'t>,
    ) -> (view::Attr<'t>, view::Meta<'t>) {
        self.find_attr_range_with_source_opt_view(db, krate, owner)
            .unwrap_or_else(|| self.incorrect(krate))
    }

    /// The containing `Attr` (it may hold other attributes too, through `cfg_attr`) and its
    /// `Meta`, of a view of the owner; `None` for an `AttrId` the owner has no attribute of.
    pub(crate) fn find_attr_range_with_source_opt_view<'t>(
        self,
        db: &Db,
        krate: Crate,
        owner: view::AnyHasAttrs<'t>,
    ) -> Option<(view::Attr<'t>, view::Meta<'t>)> {
        let cfg_options = OnceCell::new();
        let mut index = 0;
        let result = collect_item_tree_attrs_view(
            owner,
            || cfg_options.get_or_init(|| krate.cfg_options(db)),
            |meta, top_attr| {
                if index == self.id {
                    return ControlFlow::Break((top_attr, meta));
                }
                index += 1;
                ControlFlow::Continue(())
            },
        );
        match result {
            Some(Either::Left(it)) => Some(it),
            _ => None,
        }
    }

    pub fn find_derive_range(
        self,
        db: &Db,
        krate: Crate,
        owner: AstId<ast::Adt>,
        derive_index: u32,
    ) -> TextRange {
        let (_, derive_attr) = self.find_attr_range_view(db, krate, owner);
        let view::Meta::TokenTreeMeta(derive_attr) = derive_attr else {
            return derive_attr.syntax().text_range();
        };
        let Some(tt) = derive_attr.token_tree() else {
            return derive_attr.syntax().text_range();
        };
        // Only the offsets in the file are read, so the spans are absolute: no map is made.
        let spans = AbsoluteSpans(span::EditionedFileId::current_edition(span::FileId::from_raw(0)));
        frame::task(Tag::Expand, |frame| {
            let tt = syntax_bridge::call_input_in(
                tt.syntax(),
                spans,
                RealSpanMap::absolute_span(spans.0, tt.syntax().text_range()),
                DocCommentDesugarMode::ProcMacro,
                frame,
            );
            let Some((_, _, derive_tts)) =
                parse_path_comma_token_tree(db, &tt).nth(derive_index as usize)
            else {
                return derive_attr.syntax().text_range();
            };
            let (Some(first_span), Some(last_span)) =
                (derive_tts.first_span(), derive_tts.last_span())
            else {
                return derive_attr.syntax().text_range();
            };
            TextRange::new(first_span.range.start(), last_span.range.end())
        })
    }
}

/// [`RealSpanMap::absolute`]'s spans, relative to the file's root, without its two boxes.
#[derive(Clone, Copy)]
struct AbsoluteSpans(span::EditionedFileId);

impl syntax_bridge::SpanMapper for AbsoluteSpans {
    #[inline]
    fn span_for(&self, range: TextRange) -> Span {
        RealSpanMap::absolute_span(self.0, range)
    }
}

#[cfg(test)]
mod tests {
    use span::Edition;
    use syntax::SourceFile;

    use super::*;

    const ITEM: &str = r#"
#[doc = "x"] #[r#inline] #[foo::doc] #[repr(C)] #[cfg_attr(all(), allow(x), derive(Y))]
#[serde] #[r#must_use] #[unsafe(no_mangle)] #[must_used] #[r#mod_attr]
struct S;
"#;

    /// The segment is read in place and filtered as when it was copied out first.
    #[test]
    fn segments_as_before() {
        let file = SourceFile::parse(ITEM, Edition::CURRENT);
        let item = file.root().descendants().find_map(view::Struct::cast).unwrap();
        let item = view::AnyHasAttrs::new(item);
        let cfg = CfgOptions::default();
        let old_text = |path: &view::Path<'_>| -> Option<SmolStr> {
            Some(path.as_single_name_ref()?.text().trim_start_matches("r#").into())
        };

        let mut kept = Vec::new();
        let _ = collect_item_tree_attrs_view::<Infallible>(item, || &cfg, |meta, _| {
            kept.push(meta.syntax().text().to_string());
            ControlFlow::Continue(())
        });
        let mut expected = Vec::new();
        let _ = expand_cfg_attr_view::<Infallible>(
            view::attrs_including_inner(&item),
            || &cfg,
            |meta, _| {
                let filtered = meta
                    .path()
                    .and_then(|it| old_text(&it))
                    .is_some_and(|it| is_item_tree_filtered_attr(&it));
                if !filtered {
                    expected.push(meta.syntax().text().to_string());
                }
                ControlFlow::Continue(())
            },
        );
        assert_eq!(kept, expected);
        assert_eq!(kept, ["foo::doc", "derive(Y)", "serde", "no_mangle", "must_used", "r#mod_attr"]);

        for attr in view::attrs_including_inner(&item) {
            let Some(path) = attr.meta().and_then(|it| it.path()) else { continue };
            for word in ["doc", "inline", "must_use", "foo", "mod_attr", "r#inline", ""] {
                assert_eq!(path.is1(word), old_text(&path).is_some_and(|it| it == word), "{word}");
            }
            assert_eq!(path.as_one_segment(), old_text(&path));
        }
    }

    /// The table finds what the `matches!` it replaced did: every member, near misses of each,
    /// and other attribute names.
    #[test]
    fn filtered_attrs_as_the_string_match() {
        let reference = |name: &str| {
            matches!(
                name,
                "doc"
                    | "stable"
                    | "unstable"
                    | "target_feature"
                    | "allow"
                    | "expect"
                    | "warn"
                    | "deny"
                    | "forbid"
                    | "repr"
                    | "inline"
                    | "track_caller"
                    | "must_use"
            )
        };
        let mut names: Vec<String> = Vec::new();
        for text in ITEM_TREE_FILTERED_ATTRS {
            names.push((*text).to_owned());
            names.push(format!("{text}s"));
            names.push(format!("r#{text}"));
            names.push(text.to_uppercase());
            names.push(text[..text.len() - 1].to_owned());
            names.push(text[1..].to_owned());
        }
        names.extend(["", "cfg", "derive", "must_used", "rustc_const_stable", "test"].map(String::from));
        for name in &names {
            assert_eq!(is_item_tree_filtered_attr(name), reference(name), "{name}");
        }
        assert!(ITEM_TREE_FILTERED_ATTRS.iter().all(|it| is_item_tree_filtered_attr(it)));
    }

    /// `cfg_attrs_view` gives what `expand_cfg_attr_view` gives: the same metas, by kind and
    /// range, with the same outermost attributes, in the same order.
    #[test]
    fn cfg_attrs_view_matches_the_expansion() {
        use syntax::{
            SourceFile,
            view::{self, AstNode as _, HasAttrs as _},
        };

        let text = r#"
#[a]
#[cfg_attr(all(), b, cfg_attr(all(), c, d), e)]
#[cfg_attr(any(), f)]
#[cfg_attr(all(), cfg_attr(any(), g), h)]
#[unsafe(i)]
#[cfg_attr(all(), unsafe(j))]
#[cfg_attr(all())]
fn f() {}
"#;
        let parse = SourceFile::parse(text, span::Edition::CURRENT);
        let cfg = CfgOptions::default();
        let viewed = parse.root().descendants().find_map(view::Fn::cast).unwrap();
        let mut expected = Vec::new();
        expand_cfg_attr_view::<()>(viewed.attrs(), || &cfg, |meta, attr| {
            expected.push((meta.syntax().text_range(), attr.syntax().text_range()));
            ControlFlow::Continue(())
        });
        let got: Vec<_> = cfg_attrs_view(viewed.attrs(), &cfg)
            .map(|(meta, attr)| (meta.syntax().text_range(), attr.syntax().text_range()))
            .collect();
        assert_eq!(got, expected);
        assert_eq!(got.len(), 8, "{got:?}");
    }
}
