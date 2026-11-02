//! Span maps for real files and macro expansions.

use std::cell::Cell;

use base_db::Db;
use blaze_ids::radix;
use span::Span;
use syntax::{
    TextRange, ast,
    view::{self, AstNode as _, HasModuleItem as _},
};

pub use span::RealSpanMap;

use crate::{HirFileId, MacroCallId};

use syntax_bridge::{FragmentBounds, MacroFragments};

pub type ExpansionSpanMap = span::SpanMap;

/// Spanmap for a macro file or a real file
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpanMap<'db> {
    /// Spanmap for a macro file
    ExpansionSpanMap(&'db ExpansionSpanMap),
    /// Spanmap for a real file
    RealSpanMap(&'db RealSpanMap),
}

impl syntax_bridge::SpanMapper for SpanMap<'_> {
    fn span_for(&self, range: TextRange) -> Span {
        self.span_for_range(range)
    }
}

/// A [`SpanMap`] read in increasing offset order, as the bridge reads the tokens of a node:
/// each lookup starts from the piece the last one found (two compares while it stays there or
/// moves to the next) instead of a binary search over the map. Any order gives the same spans;
/// one per conversion.
pub(crate) struct InOrder<'db> {
    map: SpanMap<'db>,
    hint: Cell<u32>,
}

impl<'db> InOrder<'db> {
    pub(crate) fn new(map: SpanMap<'db>) -> Self {
        Self { map, hint: Cell::new(0) }
    }

    /// [`SpanMap::span_for_range`], from where the last lookup ended.
    #[inline]
    pub(crate) fn span_for_range(&self, range: TextRange) -> Span {
        let mut hint = self.hint.get();
        let span = match self.map {
            SpanMap::ExpansionSpanMap(map) => map.span_at_from(range.start(), &mut hint),
            SpanMap::RealSpanMap(map) => map.span_for_range_from(range, &mut hint),
        };
        self.hint.set(hint);
        span
    }
}

impl syntax_bridge::SpanMapper for InOrder<'_> {
    #[inline]
    fn span_for(&self, range: TextRange) -> Span {
        self.span_for_range(range)
    }
}

/// The spans for the arguments of a `macro_rules` macro, see [`MacroFragments`].
pub(crate) struct MacroRulesArgs<'db>(pub(crate) InOrder<'db>);

impl syntax_bridge::SpanMapper for MacroRulesArgs<'_> {
    #[inline]
    fn span_for(&self, range: TextRange) -> Span {
        self.0.span_for_range(range)
    }

    fn fragment_bounds(&self, range: TextRange) -> FragmentBounds {
        match self.0.map {
            SpanMap::ExpansionSpanMap(map) => MacroFragments(map).fragment_bounds(range),
            SpanMap::RealSpanMap(_) => FragmentBounds::default(),
        }
    }
}

impl<'db> SpanMap<'db> {
    pub fn span_for_range(&self, range: TextRange) -> Span {
        match self {
            // FIXME: Is it correct for us to only take the span at the start? This feels somewhat
            // wrong. The context will be right, but the range could be considered wrong. See
            // https://github.com/rust-lang/rust/issues/23480, we probably want to fetch the span at
            // the start and end, then merge them like rustc does in `Span::to`
            Self::ExpansionSpanMap(span_map) => span_map.span_at(range.start()),
            Self::RealSpanMap(span_map) => span_map.span_for_range(range),
        }
    }
}

impl HirFileId {
    #[inline]
    pub fn span_map<'db>(self, db: &'db Db) -> SpanMap<'db> {
        match self {
            HirFileId::FileId(file_id) => SpanMap::RealSpanMap(real_span_map(db, file_id)),
            HirFileId::MacroFile(m) => SpanMap::ExpansionSpanMap(m.expansion_span_map(db)),
        }
    }
}

/// This is an implementation detail of [`HirFileId::span_map`]. Outside this crate, use
/// `HirFileId::from(file_id).span_map(db)` instead of `real_span_map(db, file_id)`.
#[blaze_query::tracked(returns(ref), mem_tag = Expand)]
pub(crate) fn real_span_map(
    db: &Db,
    editioned_file_id: base_db::EditionedFileId,
) -> RealSpanMap {
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        real_span_map_in(db, editioned_file_id, frame)
    })
}

thread_local! {
    /// The scratch of the anchor sort, kept by the worker.
    static ANCHOR_SORT: std::cell::RefCell<blaze_mem::scratch::Scratch<(syntax::TextSize, span::ErasedFileAstId)>> =
        const { std::cell::RefCell::new(blaze_mem::scratch::Scratch::new()) };
}

/// [`real_span_map`], its anchor list in `frame`.
fn real_span_map_in(
    db: &Db,
    editioned_file_id: base_db::EditionedFileId,
    frame: &blaze_mem::Frame,
) -> RealSpanMap {
    let mut pairs = Vec::new_in(frame);
    pairs.push((syntax::TextSize::new(0), span::ROOT_ERASED_FILE_AST_ID));
    let ast_id_map = HirFileId::from(editioned_file_id).ast_id_map(db);

    let parse = editioned_file_id.parse(db);
    let tree = parse.view();
    // This is an incrementality layer. Basically we can't use absolute ranges for our spans as that
    // would mean we'd invalidate everything whenever we type. So instead we make the text ranges
    // relative to some AstIds reducing the risk of invalidation as typing somewhere no longer
    // affects all following spans in the file.
    // There is some stuff to bear in mind here though, for one, the more "anchors" we create, the
    // easier it gets to invalidate things again as spans are as stable as their anchor's ID.
    // The other problem is proc-macros. Proc-macros have a `Span::join` api that allows them
    // to join two spans that come from the same file. rust-analyzer's proc-macro server
    // can only join two spans if they belong to the same anchor though, as the spans are relative
    // to that anchor. To do cross anchor joining we'd need to access to the ast id map to resolve
    // them again, something we might get access to in the future. But even then, proc-macros doing
    // this kind of joining makes them as stable as the AstIdMap (which is basically changing on
    // every input of the file)…

    let item_to_entry = |item: view::Item<'_>| {
        (item.syntax().text_range().start(), ast_id_map.ast_id_of_view::<ast::Item>(item).erase())
    };
    // Top level items make for great anchors as they are the most stable and a decent boundary
    pairs.extend(tree.items().map(item_to_entry));
    // Unfortunately, assoc items are very common in Rust, so descend into those as well and make
    // them anchors too, but only if they have no attributes attached, as those might be proc-macros
    // and using different anchors inside of them will prevent spans from being joinable.
    tree.items().for_each(|item| match item {
        view::Item::ExternBlock(it) if view::attrs_including_inner(&it).next().is_none() => {
            if let Some(extern_item_list) = it.extern_item_list() {
                pairs.extend(
                    extern_item_list.extern_items().map(view::Item::from).map(item_to_entry),
                );
            }
        }
        view::Item::Impl(it) if view::attrs_including_inner(&it).next().is_none() => {
            if let Some(assoc_item_list) = it.assoc_item_list() {
                pairs.extend(assoc_item_list.assoc_items().map(view::Item::from).map(item_to_entry));
            }
        }
        view::Item::Module(it) if view::attrs_including_inner(&it).next().is_none() => {
            if let Some(item_list) = it.item_list() {
                pairs.extend(item_list.items().map(item_to_entry));
            }
        }
        view::Item::Trait(it) if view::attrs_including_inner(&it).next().is_none() => {
            if let Some(assoc_item_list) = it.assoc_item_list() {
                pairs.extend(assoc_item_list.assoc_items().map(view::Item::from).map(item_to_entry));
            }
        }
        _ => (),
    });
    // The lookup is a binary search, and the nested items were added after the items that
    // hold them.
    ANCHOR_SORT.with_borrow_mut(|scratch| {
        radix::sort_copy_by_key_in(&mut pairs, |&(offset, _)| u32::from(offset), scratch.take())
    });
    // The root's anchor holds the text before the first item. With an item at 0 it holds none
    // and no lookup finds it, but `RealSpanMap::within` would keep it in that item's map, so the
    // item's maps (and the bodies compared by them) would change once an item is written above.
    let zero = syntax::TextSize::new(0);
    if pairs.iter().take_while(|&&(offset, _)| offset == zero).nth(1).is_some() {
        pairs.retain(|&(offset, ast_id)| offset != zero || ast_id != span::ROOT_ERASED_FILE_AST_ID);
    }

    RealSpanMap::from_file(
        editioned_file_id.span_file_id(db),
        &pairs,
        tree.syntax().text_range().end(),
    )
}

impl MacroCallId {
    pub fn expansion_span_map(self, db: &Db) -> &ExpansionSpanMap {
        &self.parse_macro_expansion(db).value.1
    }
}

#[cfg(test)]
mod tests {
    use span::{Edition, EditionedFileId, ErasedFileAstId, FileId, SpanAnchor, SyntaxContext};
    use syntax::TextSize;

    use blaze_ids::rng::SplitMix64;

    use super::*;

    /// Ranges of tokens of 1 to 4 bytes over `len` bytes, in order.
    fn tokens(rng: &mut SplitMix64, len: u32) -> Vec<TextRange> {
        let mut at = 0;
        let mut out = Vec::new();
        while at < len {
            let end = (at + 1 + rng.below(4) as u32).min(len);
            out.push(TextRange::new(at.into(), end.into()));
            at = end;
        }
        out
    }

    /// An item at the start of a file has the map of its own (`RealSpanMap::within`) it has
    /// once an item is written above it: the root's empty anchor before it is not kept, so its
    /// bodies stay equal and their readers are not run again (blaze-lints'
    /// `an_edit_of_one_body_lints_that_body_and_no_other`).
    #[test]
    fn an_item_at_the_start_keeps_its_map_when_an_item_is_written_above() {
        let item = "fn a() {\n    let _ = 1;\n}\n";
        let own = [item.to_owned(), format!("struct S;\n{item}")].map(|text| {
            let fixture = crate::fixture::db(&format!("//- /main.rs crate:main\n{text}"));
            let map = real_span_map(fixture.db(), fixture.only_file());
            let (ids, starts) = map.anchors();
            let at_zero = starts.iter().take_while(|&&start| start == 0).count();
            assert_eq!(at_zero, 1, "one anchor begins at 0, the first item's");
            assert_ne!(ids[0][0], span::ROOT_ERASED_FILE_AST_ID.into_raw(), "an item is at 0");
            let (start, len) = (*starts.last().expect("`a`"), ids.last().expect("`a`")[1]);
            let within = map.within(TextRange::at(start.into(), len.into()));
            let (ids, starts) = within.anchors();
            (ids.iter().map(|&[_, len]| len).collect::<Vec<_>>(), starts.to_vec())
        });
        assert_eq!(own[0], own[1]);
    }

    /// The in-order mapper gives the spans the plain lookup gives, walking the tokens in order
    /// (as the bridge does) and then in a scattered order, over both kinds of map.
    #[test]
    fn in_order_gives_the_spans_a_lookup_does() {
        let file_id = EditionedFileId::new(FileId::from_raw(3), Edition::CURRENT);
        let ctx = SyntaxContext::root(Edition::CURRENT);
        let mut rng = SplitMix64::new(0x9e37_79b9_7f4a_7c15);
        for len in [1u32, 7, 64, 500] {
            let mut anchors = vec![(TextSize::new(0), ErasedFileAstId::from_raw(0))];
            let mut at = 0;
            while at < len {
                at += 1 + rng.below(30) as u32;
                if at < len {
                    anchors.push((at.into(), ErasedFileAstId::from_raw(anchors.len() as u32)));
                }
            }
            let real = RealSpanMap::from_file(file_id, &anchors, len.into());

            let mut expansion = ExpansionSpanMap::empty();
            for (i, range) in tokens(&mut rng, len).into_iter().enumerate() {
                let anchor =
                    SpanAnchor { file_id, ast_id: ErasedFileAstId::from_raw(i as u32 % 5) };
                expansion.push(range.end(), Span { range, anchor, ctx });
            }
            expansion.finish();

            for map in [SpanMap::RealSpanMap(&real), SpanMap::ExpansionSpanMap(&expansion)] {
                let walk = tokens(&mut rng, len);
                let in_order = InOrder::new(map);
                for &range in &walk {
                    assert_eq!(in_order.span_for_range(range), map.span_for_range(range));
                }
                let scattered = InOrder::new(map);
                for _ in 0..walk.len() {
                    let range = walk[rng.index(walk.len())];
                    assert_eq!(scattered.span_for_range(range), map.span_for_range(range));
                }
            }
        }
    }
}
