//! Item signature IR definitions

use std::cell::LazyCell;

use base_db::{Db, EditionedFileId};
use bitflags::bitflags;
use blaze_ids::bits::Bits;
use blaze_mem::frame::Frame;
use cfg::{CfgExpr, CfgOptions};
use hir_expand::{
    AstId, HirFileId, InFile, Intern, Lookup,
    name::{AsName, Name},
    span_map::SpanMap,
};
use intern::{Symbol, sym};
use la_arena::{Arena, Idx};
use rustc_abi::{ExternAbi, IntegerType, ReprOptions};
use smallvec::SmallVec;
use span::{AstIdMap, AstIdNode, ROOT_ERASED_FILE_AST_ID, RealSpanMap};
use syntax::{
    FlatTree, SyntaxKind, SyntaxNodePtr, T, TextRange, TextSize,
    view::{self as ast, AstNode, HasGenericParams, HasName, HasVisibility, IsString},
};
use thin_vec::ThinVec;
use triomphe::Arc;

use crate::{
    AdtId, ConstId, DefWithBodyId, EnumId, EnumVariantId, EnumVariantLoc, ExternBlockId, FunctionId, FxIndexMap,
    GenericDefId, HasModule, ImplId, ItemContainerId, LoweringMode, ModuleId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId, VariantId,
    attrs::AttrFlags,
    expr_store::{
        body::{BodySyntax, body_syntax},
        ExpressionStore, ExpressionStoreSourceMap, Move, MoveTo,
        lower::{
            BodyLowering, ExprCollector, ImplTraitLowering, lower_function,
            lower_generic_params_within, lower_impl, lower_trait, lower_type_alias, lower_type_ref,
        },
    },
    hir::{ExprId, PatId, generics::GenericParams},
    item_tree::{FieldsShape, RawVisibility, visibility_from_ast},
    src::HasSource,
    type_ref::{ConstRef, TraitRef, TypeBound, TypeRefId},
};

#[inline]
fn as_name_opt(name: Option<ast::Name<'_>>) -> Name {
    name.map_or_else(Name::missing, |it| it.as_name())
}

/// The flags of `$Flags` that the attributes `$attrs` set: each a shift and a mask of its one
/// `AttrFlags` bit, OR-ed together, with no branch per flag.
macro_rules! flags_from_attrs {
    ($attrs:expr, $Flags:ident { $($attr:ident => $flag:ident),+ $(,)? }) => {{
        type Bits = <$Flags as bitflags::Flags>::Bits;
        let attrs: AttrFlags = $attrs;
        let attrs = attrs.bits();
        $Flags::from_bits_retain(0 $(| {
            const FROM: u32 = single_bit(AttrFlags::$attr.bits());
            const TO: u32 = single_bit($Flags::$flag.bits() as u128);
            (((attrs >> FROM) & 1) as Bits) << TO
        })+)
    }};
}

/// The position of the one bit of `bits`.
const fn single_bit(bits: u128) -> u32 {
    assert!(bits.count_ones() == 1, "a flag read off an attribute is one bit");
    bits.trailing_zeros()
}

/// Which of `kinds` are among the tokens directly under `node`, read in one pass over its
/// children in the flat tree's kind column: each `*_token()` accessor scans them once more.
fn child_tokens<const N: usize>(node: ast::Node<'_>, kinds: [SyntaxKind; N]) -> [bool; N] {
    let (tree, at) = node.flat();
    let kinds = kinds.map(u16::from);
    let mut found = [false; N];
    for child in tree.children_with_tokens(at) {
        if tree.is_token(child) {
            let kind = tree.kind(child).0;
            for (found, want) in found.iter_mut().zip(kinds) {
                *found |= kind == want;
            }
        }
    }
    found
}

/// The maps a signature is lowered with when it is lowered from a [`SignatureSyntax`], `None`
/// when it is lowered from its file.
type Maps<'a> = Option<(&'a RealSpanMap, &'a AstIdMap)>;

/// What a signature's (or a variant's fields') `of` keeps: the value its crate's image serves,
/// in the `Arc` the image is read into, or the value lowered here without its source map, in
/// the memo itself. Only `with_source_map` builds a map, lowering again.
#[derive(Debug, PartialEq, Eq)]
enum Kept<T> {
    Served(Arc<T>),
    Lowered(T),
}

impl<T> std::ops::Deref for Kept<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        match self {
            Kept::Served(it) => it,
            Kept::Lowered(it) => it,
        }
    }
}

impl<T: PartialEq + std::fmt::Debug> Kept<T> {
    fn new(served: Option<Arc<T>>, lower: impl FnOnce() -> T) -> Kept<T> {
        match served {
            Some(it) => Kept::Served(it),
            None => Kept::Lowered(lower()),
        }
    }

    /// How to lower the value again with its source map, `store` being the kept value's: given
    /// the blocks of the value lowered here, so both lowerings agree on every id. A value an
    /// image serves has the image's blocks, not a lowering's: the map's lowering makes its own,
    /// as the reference lowering from syntax it is.
    fn lowering<'a>(&self, store: &'a ExpressionStore) -> BodyLowering<'a> {
        match self {
            Kept::Lowered(_) => BodyLowering::SourceMap { blocks: store.block_scopes() },
            Kept::Served(_) => BodyLowering::Whole,
        }
    }

    /// `value`, lowered again with its source map as [`Kept::lowering`] says, checked equal to
    /// the value lowered here.
    fn with_map(&self, value: T) -> Arc<T> {
        if let Kept::Lowered(it) = self {
            assert_eq!(value, *it, "lowered again with its source map");
        }
        // alloc: the value with a map, one per `with_source_map`/`relative` memo
        Arc::new(value)
    }
}

/// The queries of the signature `$Sig` of an `$Id`, which `$Sig::lower` lowers and
/// `deps::signatures::$served` reads from its crate's image: `of` reads the signature, lowered
/// without its source map; `relative` and `with_source_map` lower it again with the map.
macro_rules! signature_queries {
    ($Sig:ident, $Id:ident, $served:ident) => {
        #[blaze_query::tracked]
        impl $Sig {
            /// What [`Self::of`] reads: the image's signature, or the one lowered alone.
            #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
            fn kept(db: &Db, id: $Id) -> Kept<$Sig> {
                Kept::new(crate::deps::signatures::$served(db, id), || Self::lower_alone(db, id))
            }

            /// The signature with where each part of it is written in its file.
            #[blaze_query::tracked(returns(ref), lru = 1024, mem_tag = Nameres)]
            pub fn with_source_map(db: &Db, id: $Id) -> (Arc<$Sig>, ExpressionStoreSourceMap) {
                match Self::relative(db, id) {
                    Some((it, map)) => {
                        let map = moved_to_file(db, id.into(), id.lookup(db).id, map);
                        (it.clone(), map) // clone: an `Arc` count, once per memo
                    }
                    None => {
                        let kept = Self::kept(db, id);
                        let source = id.lookup(db).source_view(db);
                        let lowering = kept.lowering(&kept.store);
                        let (it, map) = Self::lower(db, id, source, None, lowering);
                        (kept.with_map(it), map)
                    }
                }
            }

            /// The signature lowered again from its [`signature_syntax`], with the source map
            /// of that tree; `None` for one a macro writes.
            #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
            pub fn relative(db: &Db, id: $Id) -> Option<(Arc<$Sig>, ExpressionStoreSourceMap)> {
                let syntax = signature_syntax(db, id.into()).as_ref()?;
                let kept = Self::kept(db, id);
                let lowering = kept.lowering(&kept.store);
                let (it, map) = Self::lower(db, id, syntax.node(), Some(syntax.maps()), lowering);
                Some((kept.with_map(it), map))
            }
        }

        impl $Sig {
            /// The signature: its crate's image's, or lowered from its [`signature_syntax`] (its
            /// file for one a macro writes) with no source map. A read of it builds no map.
            #[inline]
            pub fn of(db: &Db, id: $Id) -> &$Sig {
                Self::kept(db, id)
            }

            /// The signature lowered from its syntax without its source map, outside any query
            /// of its own: what [`Self::of`] keeps when no image serves it, and what checking an
            /// image records.
            pub(crate) fn lower_alone(db: &Db, id: $Id) -> $Sig {
                let (source, maps) = match signature_syntax(db, id.into()) {
                    Some(syntax) => (syntax.node(), Some(syntax.maps())),
                    None => (id.lookup(db).source_view(db), None),
                };
                Self::lower(db, id, source, maps, BodyLowering::Body).0
            }
        }
    };
}

/// What a signature is lowered from: its item written in a file, as a tree of its own whose
/// text starts at the item, with what no signature reads replaced by an empty node of its kind
/// (the body of a fn, const or static, the items of an impl or trait, and for
/// [`signature_syntax`] the variants of an enum), and the span and ast id maps of that tree.
/// It is compared by that text, so it stays the same while an edit elsewhere in the file moves
/// the item, or one inside what is left out: a signature is lowered again only when it
/// changes, as a body is from its `BodySyntax`. It is read off the file's [`FileOutline`], so
/// a body edit runs no query of it at all.
///
/// An item whose spans are counted from an item around it ([`RealSpanMap::anchor_at`]: an item
/// in a body, or in an `impl`, trait or module that has attributes) is lowered from that whole
/// item instead, less the bodies the outline leaves out of it, so any other edit inside that
/// one lowers it again. One in a body the outline leaves out is read off the file.
pub struct SignatureSyntax {
    pub file_id: HirFileId,
    /// Compared by its content, not a hash of it: the query database compares a new value with the old one
    /// only when the query runs again, and a compare of the columns reads each byte once where
    /// hashing them costs more and can collide.
    tree: std::sync::Arc<FlatTree>,
    /// The item in the tree.
    item: SyntaxNodePtr,
    /// The empty node in place of what is left out, when something is.
    elided: Option<SyntaxNodePtr>,
    ast_id_map: AstIdMap,
    span_map: RealSpanMap,
}

impl PartialEq for SignatureSyntax {
    fn eq(&self, other: &Self) -> bool {
        self.file_id == other.file_id
            && self.item == other.item
            && self.elided == other.elided
            && self.span_map == other.span_map
            && self.ast_id_map == other.ast_id_map
            && *self.tree == *other.tree
    }
}

impl Eq for SignatureSyntax {}

impl std::fmt::Debug for SignatureSyntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignatureSyntax")
            .field("file_id", &self.file_id)
            .field("item", &self.item)
            .field("elided", &self.elided)
            .finish_non_exhaustive()
    }
}

impl SignatureSyntax {
    /// The syntax of `item`, a node of `source`'s tree, with `elide`'s node of it left out when
    /// the item is its own anchor. An item anchored at one around it is that whole one, less
    /// the bodies `source` already left out of it.
    fn new(
        source: Source<'_>,
        item: ast::Node<'_>,
        elide: Elide,
        frame: &Frame,
    ) -> Option<SignatureSyntax> {
        let Source { file_id, span_map, ast_id_map, cuts } = source;
        let item_range = item.text_range();
        let (_, anchor) = span_map.anchor_at(item_range.start());
        let anchor = match anchor == ROOT_ERASED_FILE_AST_ID {
            true => item.ancestors().last()?,
            false => {
                let ptr = ast_id_map.get_erased(anchor);
                item.ancestors().find(|&it| ast::ptr(it) == ptr)?
            }
        };
        let anchor_range = anchor.text_range();
        let own = anchor_range == item_range && anchor.kind() == item.kind();
        // The ast id map walks an empty fn body it is not told is left out as a body it has,
        // so each of the outline's empty bodies in an anchor around the item is left out too.
        // They hold nothing and all come after the item: none moves what the item has.
        let mut elided = Vec::with_capacity_in(1, frame);
        match own {
            true => elided.extend(elide(item)),
            false => bodies_under(anchor, |body| {
                let ptr = ast::ptr(body);
                let start = ptr.text_range().start();
                let at = cuts.partition_point(|it| it.text_range().start() < start);
                if cuts.get(at) == Some(&ptr) {
                    elided.push(body);
                }
            }),
        }
        let base = anchor_range.start();
        let tree = copy_without(anchor, &elided, None);
        let (placeholder, removed, spans_end) = match (own, elided.first()) {
            (true, Some(node)) => {
                let range = node.text_range();
                let placeholder =
                    SyntaxNodePtr::from_parts(node.kind(), TextRange::empty(range.start() - base));
                (Some(placeholder), range.len(), range.start())
            }
            _ => (None, TextSize::new(0), anchor_range.end()),
        };
        let item_ptr = SyntaxNodePtr::from_parts(
            item.kind(),
            TextRange::new(item_range.start() - base, item_range.end() - base - removed),
        );
        let ptrs = frame.alloc_slice_from_iter(elided.iter().map(|&it| ast::ptr(it)));
        let ast_id_map = ast_id_map.relative_without_view(anchor, ast::Node::root(&tree), ptrs);
        Some(SignatureSyntax {
            file_id,
            tree,
            item: item_ptr,
            elided: placeholder,
            ast_id_map,
            span_map: span_map.within(TextRange::new(base, spans_end)),
        })
    }

    fn root(&self) -> ast::Node<'_> {
        ast::Node::root(&self.tree)
    }

    fn maps(&self) -> (&RealSpanMap, &AstIdMap) {
        (&self.span_map, &self.ast_id_map)
    }

    /// The item, in the tree.
    fn node<'s, N: AstNode<'s>>(&'s self) -> InFile<N> {
        let node = ast::to_node(self.item, self.root());
        InFile::new(self.file_id, N::cast(node).expect("the item is a node of its kind"))
    }

    /// `source_map`, of a signature lowered from this tree, moved to where the item is in its
    /// file, at `file` there: what follows the empty node moves on by what it left out.
    fn moved(
        &self,
        file: TextRange,
        source_map: &ExpressionStoreSourceMap,
    ) -> ExpressionStoreSourceMap {
        let relative = self.item.text_range();
        let base = file.start() - relative.start();
        let removed = file.len() - relative.len();
        // Something is left out only of an item that is the tree's root, so the item is the
        // one node around the empty node.
        let to = MoveTo::Signature { item: self.item, elided: self.elided, file, removed, base };
        source_map.moved(&Move { file: self.file_id, to })
    }
}

/// What [`SignatureSyntax::new`] leaves out of an item: the child of it that no signature reads.
type Elide = for<'t> fn(ast::Node<'t>) -> Option<ast::Node<'t>>;

/// What a [`SignatureSyntax`] is read from: its file's [`FileOutline`], or the file itself for
/// an item in a body the outline leaves out. The maps are of the tree the item is a node of.
struct Source<'a> {
    file_id: HirFileId,
    span_map: &'a RealSpanMap,
    ast_id_map: &'a AstIdMap,
    /// The empty nodes the outline has in place of the bodies it leaves out, in order; none in
    /// a file.
    cuts: &'a [SyntaxNodePtr],
}

/// The body a [`FileOutline`] leaves out of `item` when the item is its own anchor: a fn's
/// block, a const's or static's expression. Each is the child the item's accessor returns
/// (`ast::Fn::body`, `ast::Const::body`): the first child of that kind.
fn left_out(item: ast::Node<'_>) -> Option<ast::Node<'_>> {
    match item.kind() {
        SyntaxKind::FN => item.child_where(|kind| kind == SyntaxKind::BLOCK_EXPR),
        SyntaxKind::CONST | SyntaxKind::STATIC => item.child_where(ast::Expr::can_cast),
        _ => None,
    }
}

/// Whether a fn body's text may start with an inner attribute, as
/// [`syntax::view::block_may_start_with_inner_attr`] reads it (comments skipped; a maybe for
/// any text it can't rule out): such a body holds attributes of its fn, so the outline keeps
/// it. Doc comments aren't attributes the outline is read for (the docs read the file).
fn may_start_with_inner_attr(body: ast::Node<'_>) -> bool {
    let (tree, at) = body.flat();
    ast::block_may_start_with_inner_attr(&tree.source()[tree.range(at)], false)
}

/// The [`left_out`] bodies of the items that can be span anchors under `node` (a file or an
/// item of one), in order: the file's items, and those of each impl, trait and module among
/// them, where `real_span_map` finds anchors. Only a fn's body is ever lazy, and none of these
/// is in one, so nothing here parses a body.
fn bodies_under<'t>(node: ast::Node<'t>, mut body: impl FnMut(ast::Node<'t>)) {
    let mut item = |item: ast::Node<'t>| {
        let list = match item.kind() {
            SyntaxKind::IMPL | SyntaxKind::TRAIT => SyntaxKind::ASSOC_ITEM_LIST,
            SyntaxKind::MODULE => SyntaxKind::ITEM_LIST,
            _ => {
                if let Some(it) = left_out(item) {
                    body(it);
                }
                return;
            }
        };
        if let Some(list) = item.child_where(|kind| kind == list) {
            list.children().filter_map(left_out).for_each(&mut body);
        }
    };
    match node.kind() {
        SyntaxKind::SOURCE_FILE => node.children().for_each(&mut item),
        _ => item(node),
    }
}

/// A copy of `node`'s subtree with `elided` (under it, in order) each left as an empty node of
/// its kind, shared: the file's own tree when `node` is its root and nothing is left out, as
/// `clone_subtree` shared it.
fn copy_without(
    node: ast::Node<'_>,
    elided: &[ast::Node<'_>],
    placeholder: Option<SyntaxKind>,
) -> std::sync::Arc<FlatTree> {
    if elided.is_empty() && node.parent().is_none() && node.text_range().start() == TextSize::new(0)
    {
        // clone: an `Arc` count of the file's tree, which a copy would equal
        return std::sync::Arc::clone(node.shared_file());
    }
    // alloc: the copy, the memo's tree
    std::sync::Arc::new(match placeholder {
        Some(kind) => node.subtree_without_as(elided, kind),
        None => node.subtree_without(elided),
    })
}

/// The kind of every empty node a [`FileOutline`] has in place of a body: one kind, so that an
/// edit turning a body into an expression of another kind leaves the outline the same. A fn's
/// body is one already, and a const's or static's body may be one.
const CUT: SyntaxKind = SyntaxKind::BLOCK_EXPR;

/// A file's tree with the body of every fn, const and static that is its own span anchor left
/// out, each an empty node of its kind, and that tree's ast id and span maps: what every
/// signature of the file is read from. A body edit leaves it the same (nothing after a body
/// moves, as the body has no length here), so it backdates and no signature query of the file
/// runs again; rustc's per-owner HIR hash, in one query per file.
///
/// A body of an item anchored at one around it (an item of an impl, trait or module with
/// attributes, or of a module in a module) stays: cutting it would move its neighbours' spans,
/// which count from that anchor. An edit in it runs the file's signature queries again.
pub struct FileOutline {
    tree: std::sync::Arc<FlatTree>,
    ast_id_map: AstIdMap,
    span_map: RealSpanMap,
    /// The empty node in place of each body left out, in order.
    cuts: Box<[SyntaxNodePtr]>,
}

impl PartialEq for FileOutline {
    fn eq(&self, other: &Self) -> bool {
        self.cuts == other.cuts
            && self.span_map == other.span_map
            && self.ast_id_map == other.ast_id_map
            && *self.tree == *other.tree
    }
}

impl Eq for FileOutline {}

impl std::fmt::Debug for FileOutline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileOutline").field("cuts", &self.cuts).finish_non_exhaustive()
    }
}

impl FileOutline {
    fn root(&self) -> ast::Node<'_> {
        ast::Node::root(&self.tree)
    }
}

#[cfg(test)]
impl FileOutline {
    /// The outline's text, every body cut out.
    pub(crate) fn text(&self) -> String {
        self.root().text().to_string()
    }
}

/// The [`FileOutline`] of `file_id`: one pass over the file's items, and one over what is left
/// of the file for the copy and each map.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn file_outline(db: &Db, file_id: EditionedFileId) -> FileOutline {
    blaze_mem::frame::current(|frame| {
        let file = HirFileId::from(file_id);
        let root = file.root(db);
        let ast_id_map = file.ast_id_map(db);
        let SpanMap::RealSpanMap(span_map) = file.span_map(db) else {
            unreachable!("the spans of a file, not a macro's, are a real span map")
        };
        let mut bodies = Vec::new_in(frame);
        bodies_under(root, |body| {
            let item = body.parent().expect("a body is under its item");
            if item.kind() == SyntaxKind::FN && may_start_with_inner_attr(body) {
                return;
            }
            let (start, anchor) = span_map.anchor_at(item.text_range().start());
            let own = start == item.text_range().start()
                && anchor != ROOT_ERASED_FILE_AST_ID
                && ast_id_map.get_erased(anchor) == ast::ptr(item);
            if own {
                bodies.push(body);
            }
        });
        let outline = copy_without(root, &bodies, Some(CUT));
        let ptrs = frame.alloc_slice_from_iter(bodies.iter().map(|&it| ast::ptr(it)));
        let ranges = frame.alloc_slice_from_iter(ptrs.iter().map(SyntaxNodePtr::text_range));
        let mut cut = TextSize::new(0);
        let cuts = ptrs
            .iter()
            .map(|ptr| {
                let at = ptr.text_range().start() - cut;
                cut += ptr.text_range().len();
                SyntaxNodePtr::from_parts(CUT, TextRange::empty(at))
            })
            // alloc: the memo's cuts, exact: a slice's length
            .collect();
        FileOutline {
            ast_id_map: ast_id_map.relative_without_view(root, ast::Node::root(&outline), ptrs),
            span_map: span_map.without(ranges),
            tree: outline,
            cuts,
        }
    })
}

/// The [`SignatureSyntax`] of the item at `id`, `elide`'s node of it left out when it is its
/// own anchor: read off its file's [`FileOutline`], and so the same after any edit of a body
/// left out of it. An item in such a body is not in the outline and is read off its file, as
/// the body it is in is. `None` for an item a macro writes.
fn item_syntax<N: AstIdNode>(
    db: &Db,
    id: AstId<N>,
    module: ModuleId,
    elide: Elide,
    frame: &Frame,
) -> Option<SignatureSyntax> {
    let HirFileId::FileId(file) = id.file_id else { return None };
    let outline = file_outline(db, file);
    if let Some(ptr) = outline.ast_id_map.try_get_erased(id.value.erase()) {
        let source = Source {
            file_id: id.file_id,
            span_map: &outline.span_map,
            ast_id_map: &outline.ast_id_map,
            cuts: &outline.cuts,
        };
        return SignatureSyntax::new(source, ast::to_node(ptr, outline.root()), elide, frame);
    }
    if let Some((body, tree, ptr)) = owning_body(db, id.file_id, id.value.erase(), module) {
        let source = Source { file_id: id.file_id, span_map: &body.span_map, ast_id_map: &body.ast_id_map, cuts: &[] };
        return SignatureSyntax::new(source, ast::to_node(ptr, ast::Node::root(tree)), elide, frame);
    }
    let SpanMap::RealSpanMap(span_map) = id.file_id.span_map(db) else { return None };
    let ast_id_map = id.file_id.ast_id_map(db);
    let item = ast::to_node(ast_id_map.get_erased(id.value.erase()), id.file_id.root(db));
    let source = Source { file_id: id.file_id, span_map, ast_id_map, cuts: &[] };
    SignatureSyntax::new(source, item, elide, frame)
}

/// The tree of `owner`'s [`BodySyntax`], kept as a query's value so that what is read off it
/// lives as long as the database: the same while the body is.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
fn body_tree(db: &Db, owner: DefWithBodyId) -> Option<std::sync::Arc<FlatTree>> {
    body_syntax(db, owner).as_ref().map(|it| it.tree(db))
}

/// The body an item of `module` at `id` is written in, its tree and the item's node there: the
/// body whose own syntax made `module`'s block (`BlockIdLt::owner`). `None` for an item outside
/// fn bodies, of a block lowered from its file, or written by a macro.
fn owning_body<'db>(
    db: &'db Db,
    file_id: HirFileId,
    id: span::ErasedFileAstId,
    module: ModuleId,
) -> Option<(&'db BodySyntax, &'db std::sync::Arc<FlatTree>, SyntaxNodePtr)> {
    if !id.in_body() {
        return None;
    }
    let owner = module.block(db)?.owner(db)?;
    let syntax = body_syntax(db, owner).as_ref().filter(|it| it.file_id == file_id)?;
    let tree = body_tree(db, owner).as_ref()?;
    Some((syntax, tree, syntax.ast_id_map.try_get_erased(id)?))
}

/// The node at `id`, an item of a block of `module`, in the body that holds it
/// ([`owning_body`]): an edit elsewhere in the file leaves it the same.
pub(crate) fn body_item<'db, N: AstIdNode + syntax::view::Typed>(
    db: &'db Db,
    id: AstId<N>,
    module: ModuleId,
) -> Option<N::Of<'db>> {
    let (_, tree, ptr) = owning_body(db, id.file_id, id.value.erase(), module)?;
    <N::Of<'db> as syntax::view::AstNode<'db>>::cast(syntax::view::to_node(ptr, ast::Node::root(tree)))
}

/// The node at `id` in its file's [`FileOutline`], when the outline has it: the item's
/// attributes read the same off it as off the file, and a body edit leaves it the same.
/// `None` for an item in a body the outline leaves out, or one a macro writes.
pub(crate) fn outline_item<'db, N: AstIdNode + syntax::view::Typed>(
    db: &'db Db,
    id: AstId<N>,
) -> Option<N::Of<'db>> {
    let HirFileId::FileId(file) = id.file_id else { return None };
    let outline = file_outline(db, file);
    let ptr = outline.ast_id_map.try_get_erased(id.value.erase())?;
    let root = syntax::view::Node::root(&outline.tree);
    <N::Of<'db> as syntax::view::AstNode<'db>>::cast(syntax::view::to_node(ptr, root))
}

/// The visibility written on the item at `id`, unresolved: lowered off its file's
/// [`FileOutline`] with the outline's spans, and so the same after any edit of a body the
/// outline leaves out. An item the outline doesn't have (in such a body, or written by a
/// macro) is lowered off its file, as that body is.
pub(crate) fn written_visibility<N: AstIdNode>(db: &Db, id: AstId<N>, module: ModuleId) -> RawVisibility {
    if let HirFileId::FileId(file) = id.file_id {
        let outline = file_outline(db, file);
        if let Some(ptr) = outline.ast_id_map.try_get_erased(id.value.erase()) {
            let node = ast::to_node(ptr, ast::Node::root(&outline.tree));
            return visibility_of(db, node, &mut |range| outline.span_map.span_for_range(range).ctx);
        }
    }
    if let Some((body, tree, ptr)) = owning_body(db, id.file_id, id.value.erase(), module) {
        let node = ast::to_node(ptr, ast::Node::root(tree));
        return visibility_of(db, node, &mut |range| body.span_map.span_for_range(range).ctx);
    }
    let node = ast::to_node(id.file_id.ast_id_map(db).get_erased(id.value.erase()), id.file_id.root(db));
    let span_map = id.file_id.span_map(db);
    visibility_of(db, node, &mut |range| span_map.span_for_range(range).ctx)
}

fn visibility_of(
    db: &Db,
    node: ast::Node<'_>,
    span_for_range: &mut impl FnMut(TextRange) -> span::SyntaxContext,
) -> RawVisibility {
    let item = ast::AnyHasVisibility::cast(node)
        .unwrap_or_else(|| panic!("a {:?} at {:?} has no visibility", node.kind(), node.text_range()));
    visibility_from_ast(db, item.visibility(), span_for_range)
}

/// The root of `file`'s [`FileOutline`]: the file less the bodies it leaves out.
pub(crate) fn outline_root(db: &Db, file: EditionedFileId) -> syntax::view::SourceFile<'_> {
    syntax::view::root(&file_outline(db, file).tree).expect("an outline's root is a file")
}

/// The root of `file`'s [`FileOutline`] and the outline's ast id map, whose ids are the file's.
pub(crate) fn outline_with_ids(
    db: &Db,
    file: EditionedFileId,
) -> (syntax::view::Node<'_>, &AstIdMap) {
    let outline = file_outline(db, file);
    (syntax::view::Node::root(&outline.tree), &outline.ast_id_map)
}

/// The [`SignatureSyntax`] of `def`'s signature, `None` for an item a macro writes.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn signature_syntax(db: &Db, def: GenericDefId) -> Option<SignatureSyntax> {
    let nothing: Elide = |_| None;
    let items: Elide = |it| it.child_where(|kind| kind == SyntaxKind::ASSOC_ITEM_LIST);
    let variants: Elide = |it| it.child_where(|kind| kind == SyntaxKind::VARIANT_LIST);
    blaze_mem::frame::current(|frame| match def {
        GenericDefId::AdtId(AdtId::StructId(it)) => {
            item_syntax(db, it.lookup(db).id, it.module(db), nothing, frame)
        }
        GenericDefId::AdtId(AdtId::UnionId(it)) => {
            item_syntax(db, it.lookup(db).id, it.module(db), nothing, frame)
        }
        GenericDefId::AdtId(AdtId::EnumId(it)) => {
            item_syntax(db, it.lookup(db).id, it.module(db), variants, frame)
        }
        GenericDefId::FunctionId(it) => item_syntax(db, it.lookup(db).id, it.module(db), left_out, frame),
        GenericDefId::ConstId(it) => item_syntax(db, it.lookup(db).id, it.module(db), left_out, frame),
        GenericDefId::StaticId(it) => item_syntax(db, it.lookup(db).id, it.module(db), left_out, frame),
        GenericDefId::ImplId(it) => item_syntax(db, it.lookup(db).id, it.module(db), items, frame),
        GenericDefId::TraitId(it) => item_syntax(db, it.lookup(db).id, it.module(db), items, frame),
        GenericDefId::TypeAliasId(it) => item_syntax(db, it.lookup(db).id, it.module(db), nothing, frame),
    })
}

/// The [`SignatureSyntax`] of an enum with its variants, nothing left out: what its
/// [`EnumVariants`] and the [`VariantFields`] of its variants are lowered from. `None` for an
/// enum a macro writes.
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn variants_syntax(db: &Db, def: EnumId) -> Option<SignatureSyntax> {
    blaze_mem::frame::current(|frame| item_syntax(db, def.lookup(db).id, def.module(db), |_| None, frame))
}

/// `source_map`, of `def`'s signature lowered from its [`signature_syntax`], moved to `item`,
/// where `def` is in its file.
fn moved_to_file<N: AstIdNode>(
    db: &Db,
    def: GenericDefId,
    item: AstId<N>,
    source_map: &ExpressionStoreSourceMap,
) -> ExpressionStoreSourceMap {
    let syntax = signature_syntax(db, def).as_ref();
    syntax.expect("a signature lowered from no syntax").moved(file_range(db, item), source_map)
}

/// Where `item` is in its file, read off its ast id: no walk of the tree to its node.
fn file_range<N: AstIdNode>(db: &Db, item: AstId<N>) -> TextRange {
    item.file_id.ast_id_map(db).get(item.value).syntax_node_ptr().text_range()
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct StructSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: StructFlags,
    pub shape: FieldsShape,
}

// Each signature `of` keeps is in its memo (in an `Arc` only when an image serves it), and
// holds its store and generic params (704 and 72 bytes today) and its own fields, whose bytes
// are what these assert: 8 for a struct, union, enum or trait, 40 for a function, 32 for a type
// alias, 16 for a const or static (no generic params) or an impl.
const _: () = assert!(
    size_of::<StructSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 8
);
const _: () = assert!(
    size_of::<UnionSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 8
);
const _: () = assert!(
    size_of::<EnumSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 8
);
const _: () = assert!(size_of::<ConstSignature>() == size_of::<ExpressionStore>() + 16);
const _: () = assert!(size_of::<StaticSignature>() == size_of::<ExpressionStore>() + 16);
const _: () = assert!(
    size_of::<ImplSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16
);
const _: () = assert!(
    size_of::<TraitSignature>() == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 8
);
const _: () = assert!(
    size_of::<FunctionSignature>()
        == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 40
);
const _: () = assert!(
    size_of::<TypeAliasSignature>()
        == size_of::<ExpressionStore>() + size_of::<GenericParams>() + 32
);
// One word of flags per signature, tested with a mask.
const _: () = assert!(size_of::<StructFlags>() == 2);
const _: () = assert!(size_of::<EnumFlags>() == 1);
const _: () = assert!(size_of::<ConstFlags>() == 1);
const _: () = assert!(size_of::<StaticFlags>() == 1);
const _: () = assert!(size_of::<ImplFlags>() == 1);
const _: () = assert!(size_of::<TraitFlags>() == 2);
const _: () = assert!(size_of::<FnFlags>() == 2);
const _: () = assert!(size_of::<TypeAliasFlags>() == 1);

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    pub struct StructFlags: u16 {
        /// Indicates whether this struct has `#[repr]`.
        const HAS_REPR = 1 << 0;
        /// Indicates whether the struct has a `#[rustc_has_incoherent_inherent_impls]` attribute.
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 1;
        /// Indicates whether the struct has a `#[fundamental]` attribute.
        const FUNDAMENTAL      = 1 << 2;
        /// Indicates whether the struct is `PhantomData`.
        const IS_PHANTOM_DATA  = 1 << 3;
        /// Indicates whether this struct is `Box`.
        const IS_BOX           = 1 << 4;
        /// Indicates whether this struct is `ManuallyDrop`.
        const IS_MANUALLY_DROP = 1 << 5;
        /// Indicates whether this struct is `UnsafeCell`.
        const IS_UNSAFE_CELL   = 1 << 6;
        /// Indicates whether this struct is `UnsafePinned`.
        const IS_UNSAFE_PINNED = 1 << 7;
        /// Indicates whether this struct is `CovariantUnsafeCell`.
        const IS_COVARIANT_UNSAFE_CELL = 1 << 8;
        /// Indicates whether this struct is `f16b`, laid out as the backend's bfloat scalar.
        const IS_F16B = 1 << 9;
        /// Indicates whether this struct is `ppcf128`, laid out as the backend's IBM
        /// double-double scalar.
        const IS_PPC_F128 = 1 << 10;
        /// Whether a field of this struct or union is written `unsafe` (`unsafe_fields`).
        const HAS_UNSAFE_FIELDS = 1 << 11;
        /// Indicates whether this struct is `FieldRepresentingType` (`field_projections`).
        const IS_FIELD_REPRESENTING_TYPE = 1 << 12;
    }
}

/// The lang items a struct is laid out or checked as no other struct is, with the flag each
/// sets, found by `STRUCT_LANG_ITEM_NAMES` (member `i` is entry `i`): one load for a struct with
/// a `#[lang]`, none for any other.
const STRUCT_LANG_ITEMS: [(Symbol, StructFlags); 9] = [
    (sym::phantom_data, StructFlags::IS_PHANTOM_DATA),
    (sym::owned_box, StructFlags::IS_BOX),
    (sym::manually_drop, StructFlags::IS_MANUALLY_DROP),
    (sym::unsafe_cell, StructFlags::IS_UNSAFE_CELL),
    (sym::covariant_unsafe_cell, StructFlags::IS_COVARIANT_UNSAFE_CELL),
    (sym::unsafe_pinned, StructFlags::IS_UNSAFE_PINNED),
    (sym::f16b, StructFlags::IS_F16B),
    (sym::ppcf128, StructFlags::IS_PPC_F128),
    (sym::field_representing_type, StructFlags::IS_FIELD_REPRESENTING_TYPE),
];

static STRUCT_LANG_ITEM_NAMES: intern::KnownNames = intern::KnownNames::new(&[
    sym::Declared::phantom_data,
    sym::Declared::owned_box,
    sym::Declared::manually_drop,
    sym::Declared::unsafe_cell,
    sym::Declared::covariant_unsafe_cell,
    sym::Declared::unsafe_pinned,
    sym::Declared::f16b,
    sym::Declared::ppcf128,
    sym::Declared::field_representing_type,
]);

impl StructFlags {
    /// The flags of a struct or union that its attributes set.
    fn from_attrs(attrs: AttrFlags) -> StructFlags {
        flags_from_attrs!(attrs, StructFlags {
            HAS_REPR => HAS_REPR,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            FUNDAMENTAL => FUNDAMENTAL,
        })
    }

    /// The flag of a struct that is the lang item `lang`.
    fn from_lang_item(lang: &Symbol) -> StructFlags {
        STRUCT_LANG_ITEM_NAMES
            .get(lang)
            .map_or(StructFlags::empty(), |at| STRUCT_LANG_ITEMS[usize::from(at)].1)
    }
}

signature_queries!(StructSignature, StructId, served_struct);

impl StructSignature {
    fn lower(
        db: &Db,
        id: StructId,
        InFile { file_id, value: source }: InFile<ast::Struct<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let attrs = AttrFlags::query(db, id.into());

        let mut flags = StructFlags::from_attrs(attrs);
        if let Some(lang) = attrs.lang_item_with_attrs(db, id.into()) {
            flags |= StructFlags::from_lang_item(&lang);
        }
        let cfg_options = loc.container.krate(db).cfg_options(db);
        flags.set(StructFlags::HAS_UNSAFE_FIELDS, has_unsafe_field(source.field_list(), cfg_options));
        let shape = adt_shape(source.kind());

        let (store, generic_params, source_map) = lower_generic_params_within(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
            maps,
            lowering,
        );
        let name = as_name_opt(source.name());
        (StructSignature { generic_params, store, flags, shape, name }, source_map)
    }

    #[inline]
    pub fn repr(&self, db: &Db, id: StructId) -> Option<ReprOptions> {
        if self.flags.contains(StructFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}

#[inline]
fn adt_shape(adt_kind: ast::StructKind<'_>) -> FieldsShape {
    match adt_kind {
        ast::StructKind::Record(_) => FieldsShape::Record,
        ast::StructKind::Tuple(_) => FieldsShape::Tuple,
        ast::StructKind::Unit => FieldsShape::Unit,
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct UnionSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: StructFlags,
}

signature_queries!(UnionSignature, UnionId, served_union);

impl UnionSignature {
    fn lower(
        db: &Db,
        id: UnionId,
        InFile { file_id, value: source }: InFile<ast::Union<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let mut flags = StructFlags::from_attrs(AttrFlags::query(db, id.into()));
        let cfg_options = loc.container.krate(db).cfg_options(db);
        let fields = source.record_field_list().map(ast::FieldList::RecordFieldList);
        flags.set(StructFlags::HAS_UNSAFE_FIELDS, has_unsafe_field(fields, cfg_options));

        let (store, generic_params, source_map) = lower_generic_params_within(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
            maps,
            lowering,
        );
        let name = as_name_opt(source.name());
        (UnionSignature { generic_params, store, flags, name }, source_map)
    }

    #[inline]
    pub fn repr(&self, db: &Db, id: UnionId) -> Option<ReprOptions> {
        if self.flags.contains(StructFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}

bitflags! {
    #[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
    pub struct EnumFlags: u8 {
        /// Indicates whether this enum has `#[repr]`.
        const HAS_REPR = 1 << 0;
        /// Indicates whether the enum has a `#[rustc_has_incoherent_inherent_impls]` attribute.
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS  = 1 << 1;
        /// Whether this enum has `#[fundamental]`.
        const FUNDAMENTAL = 1 << 2;
        /// Whether a field of a variant of this enum is written `unsafe` (`unsafe_fields`).
        const HAS_UNSAFE_FIELDS = 1 << 3;
    }
}

impl EnumFlags {
    fn from_attrs(attrs: AttrFlags) -> EnumFlags {
        flags_from_attrs!(attrs, EnumFlags {
            HAS_REPR => HAS_REPR,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            FUNDAMENTAL => FUNDAMENTAL,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct EnumSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: EnumFlags,
}

signature_queries!(EnumSignature, EnumId, served_enum);

impl EnumSignature {
    /// Lowered from its [`signature_syntax`], its variants left out.
    fn lower(
        db: &Db,
        id: EnumId,
        InFile { file_id, value: source }: InFile<ast::Enum<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let mut flags = EnumFlags::from_attrs(AttrFlags::query(db, id.into()));
        flags.set(EnumFlags::HAS_UNSAFE_FIELDS, enum_has_unsafe_fields(db, id));

        let (store, generic_params, source_map) = lower_generic_params_within(
            db,
            loc.container,
            id.into(),
            file_id,
            source.generic_param_list(),
            source.where_clause(),
            LoweringMode::Analysis,
            maps,
            lowering,
        );
        let name = as_name_opt(source.name());
        (EnumSignature { generic_params, store, flags, name }, source_map)
    }
}

/// Whether a field of `fields` that `cfg_options` keeps is written `unsafe`: rustc's
/// `VariantDef::has_unsafe_fields`, read off the syntax so a signature doesn't read the
/// variant's [`VariantFields`].
fn has_unsafe_field(fields: Option<ast::FieldList<'_>>, cfg_options: &CfgOptions) -> bool {
    fn any<'t, Field: ast::HasAttrs<'t>>(
        mut fields: impl Iterator<Item = Field>,
        cfg_options: &CfgOptions,
    ) -> bool {
        fields.any(|field| {
            let [is_unsafe] = child_tokens(field.syntax(), [T![unsafe]]);
            is_unsafe && AttrFlags::is_cfg_enabled_for(field, cfg_options).is_ok()
        })
    }
    match fields {
        Some(ast::FieldList::RecordFieldList(it)) => any(it.fields(), cfg_options),
        Some(ast::FieldList::TupleFieldList(it)) => any(it.fields(), cfg_options),
        None => false,
    }
}

/// Whether a field of a variant of `id` that its crate's cfg keeps is written `unsafe`. It
/// reads the variants, which the enum's [`signature_syntax`] leaves out, so it is a query of
/// its own: a variant edit runs it again, and the signature only when the answer changes.
#[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
fn enum_has_unsafe_fields(db: &Db, id: EnumId) -> bool {
    let loc = id.lookup(db);
    let source = match variants_syntax(db, id) {
        Some(syntax) => syntax.node::<ast::Enum<'_>>().value,
        None => loc.source_view(db).value,
    };
    let cfg_options = loc.container.krate(db).cfg_options(db);
    source.variant_list().is_some_and(|list| {
        list.variants().any(|variant| {
            AttrFlags::is_cfg_enabled_for(variant, cfg_options).is_ok()
                && has_unsafe_field(variant.field_list(), cfg_options)
        })
    })
}

impl EnumSignature {
    pub fn variant_body_type(db: &Db, id: EnumId) -> IntegerType {
        match AttrFlags::repr(db, id.into()) {
            Some(ReprOptions { int: Some(builtin), .. }) => builtin,
            _ => IntegerType::Pointer(true),
        }
    }

    #[inline]
    pub fn repr(&self, db: &Db, id: EnumId) -> Option<ReprOptions> {
        if self.flags.contains(EnumFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, id.into())
        } else {
            None
        }
    }
}
bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Hash)]
    pub struct ConstFlags: u8 {
        const HAS_BODY = 1 << 1;
        /// Written `default const` (`specialization`).
        const IS_DEFAULT = 1 << 2;
        /// Written `type const` (`min_generic_const_args`): rustc's `is_type_const`.
        const TYPE_CONST = 1 << 3;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct ConstSignature {
    pub name: Option<Name>,
    // generic_params: GenericParams,
    pub store: ExpressionStore,
    pub type_ref: TypeRefId,
    pub flags: ConstFlags,
}

signature_queries!(ConstSignature, ConstId, served_const);

impl ConstSignature {
    /// Lowered from its [`signature_syntax`], its body left out.
    fn lower(
        db: &Db,
        id: ConstId,
        source: InFile<ast::Const<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let module = loc.container.module(db);
        let attrs = AttrFlags::query(db, id.into());
        let mut flags = flags_from_attrs!(attrs, ConstFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        });
        flags.set(ConstFlags::HAS_BODY, source.value.body().is_some());
        let [default, type_const] =
            child_tokens(source.value.syntax(), [T![default], T![type]]);
        flags.set(ConstFlags::IS_DEFAULT, default);
        flags.set(ConstFlags::TYPE_CONST, type_const);

        let (store, source_map, type_ref) =
            lower_type_ref(db, module, source.map(|it| it.ty()), maps, lowering);
        let name = source.value.name().map(|it| it.as_name());
        (ConstSignature { store, type_ref, flags, name }, source_map)
    }
}

impl ConstSignature {
    pub fn has_body(&self) -> bool {
        self.flags.contains(ConstFlags::HAS_BODY)
    }

    pub fn is_type_const(&self) -> bool {
        self.flags.contains(ConstFlags::TYPE_CONST)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Hash)]
    pub struct StaticFlags: u8 {
        const HAS_BODY = 1 << 1;
        const MUTABLE    = 1 << 3;
        const UNSAFE     = 1 << 4;
        const EXPLICIT_SAFE = 1 << 5;
        const EXTERN     = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct StaticSignature {
    pub name: Name,

    // generic_params: GenericParams,
    pub store: ExpressionStore,
    pub type_ref: TypeRefId,
    pub flags: StaticFlags,
}

signature_queries!(StaticSignature, StaticId, served_static);

impl StaticSignature {
    /// Lowered from its [`signature_syntax`], its body left out.
    fn lower(
        db: &Db,
        id: StaticId,
        source: InFile<ast::Static<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let module = loc.container.module(db);
        let attrs = AttrFlags::query(db, id.into());
        let mut flags = flags_from_attrs!(attrs, StaticFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        });
        flags.set(StaticFlags::EXTERN, matches!(loc.container, ItemContainerId::ExternBlockId(_)));

        let [mut_, unsafe_, safe] =
            child_tokens(source.value.syntax(), [T![mut], T![unsafe], T![safe]]);
        flags.set(StaticFlags::HAS_BODY, source.value.body().is_some());
        flags.set(StaticFlags::MUTABLE, mut_);
        flags.set(StaticFlags::UNSAFE, unsafe_);
        flags.set(StaticFlags::EXPLICIT_SAFE, safe);

        let (store, source_map, type_ref) =
            lower_type_ref(db, module, source.map(|it| it.ty()), maps, lowering);
        let name = as_name_opt(source.value.name());
        (StaticSignature { store, type_ref, flags, name }, source_map)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Hash)]
    pub struct ImplFlags: u8 {
        const NEGATIVE = 1 << 1;
        const DEFAULT = 1 << 2;
        const UNSAFE = 1 << 3;
        /// `const impl Tr for T` (or the older `impl const Tr for T`).
        const CONST = 1 << 4;
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct ImplSignature {
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub self_ty: TypeRefId,
    pub target_trait: Option<TraitRef>,
    pub flags: ImplFlags,
}

signature_queries!(ImplSignature, ImplId, served_impl);

impl ImplSignature {
    /// Lowered from its [`signature_syntax`], its items left out.
    fn lower(
        db: &Db,
        id: ImplId,
        src: InFile<ast::Impl<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let [unsafe_, negative, default, const_] =
            child_tokens(src.value.syntax(), [T![unsafe], T![!], T![default], T![const]]);
        let mut flags = ImplFlags::empty();
        flags.set(ImplFlags::UNSAFE, unsafe_);
        flags.set(ImplFlags::NEGATIVE, negative);
        flags.set(ImplFlags::DEFAULT, default);
        flags.set(ImplFlags::CONST, const_);

        let (store, source_map, self_ty, target_trait, generic_params) =
            lower_impl(db, loc.container, src, id, maps, lowering);
        (ImplSignature { store, generic_params, self_ty, target_trait, flags }, source_map)
    }
}

impl ImplSignature {
    #[inline]
    pub fn is_negative(&self) -> bool {
        self.flags.contains(ImplFlags::NEGATIVE)
    }

    #[inline]
    pub fn is_default(&self) -> bool {
        self.flags.contains(ImplFlags::DEFAULT)
    }

    #[inline]
    pub fn is_const(&self) -> bool {
        self.flags.contains(ImplFlags::CONST)
    }
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Hash)]
    pub struct TraitFlags: u16 {
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPLS = 1 << 1;
        const FUNDAMENTAL = 1 << 2;
        const UNSAFE = 1 << 3;
        const AUTO = 1 << 4;
        const SKIP_ARRAY_DURING_METHOD_DISPATCH = 1 << 5;
        const SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH = 1 << 6;
        const RUSTC_PAREN_SUGAR = 1 << 7;
        const COINDUCTIVE = 1 << 8;
        const ALIAS = 1 << 9;
        /// `const trait Tr`.
        const CONST = 1 << 10;
        /// `#[rustc_deny_explicit_impl]`: no impl of the trait may be written.
        const DENY_EXPLICIT_IMPL = 1 << 11;
    }
}

impl TraitFlags {
    fn from_attrs(attrs: AttrFlags) -> TraitFlags {
        flags_from_attrs!(attrs, TraitFlags {
            FUNDAMENTAL => FUNDAMENTAL,
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPLS,
            RUSTC_PAREN_SUGAR => RUSTC_PAREN_SUGAR,
            RUSTC_COINDUCTIVE => COINDUCTIVE,
            RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH => SKIP_ARRAY_DURING_METHOD_DISPATCH,
            RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH => SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH,
            RUSTC_DENY_EXPLICIT_IMPL => DENY_EXPLICIT_IMPL,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TraitSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub flags: TraitFlags,
}

signature_queries!(TraitSignature, TraitId, served_trait);

impl TraitSignature {
    /// Lowered from its [`signature_syntax`], its items left out.
    fn lower(
        db: &Db,
        id: TraitId,
        source: InFile<ast::Trait<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let mut flags = TraitFlags::from_attrs(AttrFlags::query(db, id.into()));
        let [auto, unsafe_, alias, const_] = child_tokens(
            source.value.syntax(),
            [T![auto], T![unsafe], T![=], T![const]],
        );
        if auto {
            flags |= TraitFlags::AUTO | TraitFlags::COINDUCTIVE;
        }
        flags.set(TraitFlags::UNSAFE, unsafe_);
        flags.set(TraitFlags::ALIAS, alias);
        flags.set(TraitFlags::CONST, const_);

        let name = as_name_opt(source.value.name());
        let (store, source_map, generic_params) =
            lower_trait(db, loc.container, source, id, maps, lowering);
        (TraitSignature { store, generic_params, flags, name }, source_map)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Hash)]
    pub struct FnFlags: u16 {
        const HAS_BODY = 1 << 1;
        const DEFAULT = 1 << 2;
        const CONST = 1 << 3;
        const ASYNC = 1 << 4;
        const GEN = 1 << 5;
        const UNSAFE = 1 << 6;
        const HAS_VARARGS = 1 << 7;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 8;
        const HAS_SELF_PARAM = 1 << 9;
        /// The `#[target_feature]` attribute is necessary to check safety (with RFC 2396),
        /// but keeping it for all functions will consume a lot of memory when there are
        /// only very few functions with it. So we only encode its existence here, and lookup
        /// it if needed.
        const HAS_TARGET_FEATURE = 1 << 10;
        /// `#[rustc_deprecated_safe_2024]` on an `unsafe fn`, which keeps `UNSAFE`: its body
        /// is an unsafe function's, and a call of it before edition 2024 needs no `unsafe`
        /// block, only the `deprecated_safe_2024` lint (rustc's `check_unsafety`).
        const DEPRECATED_SAFE_2024 = 1 << 11;
        const EXPLICIT_SAFE = 1 << 12;
        const HAS_LEGACY_CONST_GENERICS = 1 << 13;
        const RUSTC_INTRINSIC = 1 << 14;
    }
}

impl FnFlags {
    /// The flags of a function its attributes and its `unsafe` keyword set.
    fn from_attrs(attrs: AttrFlags, unsafe_: bool) -> FnFlags {
        let mut flags = flags_from_attrs!(attrs, FnFlags {
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            HAS_TARGET_FEATURE => HAS_TARGET_FEATURE,
            RUSTC_INTRINSIC => RUSTC_INTRINSIC,
            HAS_LEGACY_CONST_GENERICS => HAS_LEGACY_CONST_GENERICS,
            RUSTC_DEPRECATED_SAFE_2024 => DEPRECATED_SAFE_2024,
        });
        if !unsafe_ {
            flags.remove(FnFlags::DEPRECATED_SAFE_2024);
        }
        flags.set(FnFlags::UNSAFE, unsafe_);
        flags
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct FunctionSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub params: Box<[TypeRefId]>,
    pub ret_type: Option<TypeRefId>,
    pub abi: ExternAbi,
    pub flags: FnFlags,
    /// rustc's `FnDecl::splatted`: the index into `params` (`self` first) of the first parameter
    /// with `#[rustc_splat]`, below `u8::MAX`; `None` for a C-variadic function.
    pub splatted: Option<u8>,
}

signature_queries!(FunctionSignature, FunctionId, served_function);

impl FunctionSignature {
    /// Lowered from its [`signature_syntax`], its body left out.
    fn lower(
        db: &Db,
        id: FunctionId,
        source: InFile<ast::Fn<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);
        let module = loc.container.module(db);

        let attrs = AttrFlags::query(db, id.into());
        let [unsafe_, async_, gen_, const_, default, safe] = child_tokens(
            source.value.syntax(),
            [T![unsafe], T![async], T![gen], T![const], T![default], T![safe]],
        );
        let mut flags = FnFlags::from_attrs(attrs, unsafe_);
        flags.set(FnFlags::ASYNC, async_);
        flags.set(FnFlags::GEN, gen_);
        flags.set(FnFlags::CONST, const_);
        flags.set(FnFlags::DEFAULT, default);
        flags.set(FnFlags::EXPLICIT_SAFE, safe);
        flags.set(FnFlags::HAS_BODY, source.value.body().is_some());

        let name = as_name_opt(source.value.name());
        let abi = source
            .value
            .abi()
            .map(|abi| {
                abi.abi_string()
                    .and_then(|abi| abi.text_without_quotes().parse().ok())
                    .unwrap_or(ExternAbi::FALLBACK)
            })
            .or_else(|| match loc.container {
                ItemContainerId::ExternBlockId(extern_block) => {
                    Some(extern_block_abi(db, extern_block))
                }
                ItemContainerId::ModuleId(_)
                | ItemContainerId::ImplId(_)
                | ItemContainerId::TraitId(_) => None,
            })
            .unwrap_or(ExternAbi::Rust);
        let (store, source_map, generic_params, params, ret_type, self_param, variadic, splatted) =
            lower_function(db, module, source, id, maps, lowering);
        flags.set(FnFlags::HAS_SELF_PARAM, self_param);
        flags.set(FnFlags::HAS_VARARGS, variadic);
        (
            FunctionSignature { generic_params, store, params, ret_type, abi, flags, name, splatted },
            source_map,
        )
    }
}

impl FunctionSignature {
    pub fn has_body(&self) -> bool {
        self.flags.contains(FnFlags::HAS_BODY)
    }

    /// True if the first param is `self`. This is relevant to decide whether this
    /// can be called as a method.
    pub fn has_self_param(&self) -> bool {
        self.flags.contains(FnFlags::HAS_SELF_PARAM)
    }

    pub fn is_default(&self) -> bool {
        self.flags.contains(FnFlags::DEFAULT)
    }

    pub fn is_const(&self) -> bool {
        self.flags.contains(FnFlags::CONST)
    }

    pub fn is_async(&self) -> bool {
        self.flags.contains(FnFlags::ASYNC)
    }

    pub fn is_gen(&self) -> bool {
        self.flags.contains(FnFlags::GEN)
    }

    pub fn is_unsafe(&self) -> bool {
        self.flags.contains(FnFlags::UNSAFE)
    }

    pub fn is_deprecated_safe_2024(&self) -> bool {
        self.flags.contains(FnFlags::DEPRECATED_SAFE_2024)
    }

    pub fn is_safe(&self) -> bool {
        self.flags.contains(FnFlags::EXPLICIT_SAFE)
    }

    pub fn is_varargs(&self) -> bool {
        self.flags.contains(FnFlags::HAS_VARARGS)
    }

    pub fn has_target_feature(&self) -> bool {
        self.flags.contains(FnFlags::HAS_TARGET_FEATURE)
    }

    #[inline]
    pub fn legacy_const_generics_indices<'db>(
        &self,
        db: &'db Db,
        id: FunctionId,
    ) -> Option<&'db [u32]> {
        if !self.flags.contains(FnFlags::HAS_LEGACY_CONST_GENERICS) {
            return None;
        }

        AttrFlags::legacy_const_generic_indices(db, id).as_deref()
    }

    pub fn is_intrinsic(db: &Db, id: FunctionId) -> bool {
        let data = FunctionSignature::of(db, id);
        data.flags.contains(FnFlags::RUSTC_INTRINSIC)
    }
}

bitflags! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq, Default, Hash)]
    pub struct TypeAliasFlags: u8 {
        const RUSTC_HAS_INCOHERENT_INHERENT_IMPL = 1 << 1;
        /// Written `default type` (`specialization`).
        const IS_DEFAULT = 1 << 2;
        const IS_EXTERN = 1 << 6;
        const RUSTC_ALLOW_INCOHERENT_IMPL = 1 << 7;
    }
}

impl TypeAliasFlags {
    fn from_attrs(attrs: AttrFlags) -> TypeAliasFlags {
        flags_from_attrs!(attrs, TypeAliasFlags {
            RUSTC_HAS_INCOHERENT_INHERENT_IMPLS => RUSTC_HAS_INCOHERENT_INHERENT_IMPL,
            RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
        })
    }
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TypeAliasSignature {
    pub name: Name,
    pub generic_params: GenericParams,
    pub store: ExpressionStore,
    pub bounds: Box<[TypeBound]>,
    pub ty: Option<TypeRefId>,
    pub flags: TypeAliasFlags,
}

signature_queries!(TypeAliasSignature, TypeAliasId, served_type_alias);

impl TypeAliasSignature {
    fn lower(
        db: &Db,
        id: TypeAliasId,
        source: InFile<ast::TypeAlias<'_>>,
        maps: Maps<'_>,
        lowering: BodyLowering<'_>,
    ) -> (Self, ExpressionStoreSourceMap) {
        let loc = id.lookup(db);

        let mut flags = TypeAliasFlags::from_attrs(AttrFlags::query(db, id.into()));
        flags.set(
            TypeAliasFlags::IS_EXTERN,
            matches!(loc.container, ItemContainerId::ExternBlockId(_)),
        );
        let [default] = child_tokens(source.value.syntax(), [T![default]]);
        flags.set(TypeAliasFlags::IS_DEFAULT, default);
        let name = as_name_opt(source.value.name());
        let (store, source_map, generic_params, bounds, ty) =
            lower_type_alias(db, loc.container, source, id, maps, lowering);
        (TypeAliasSignature { store, generic_params, flags, bounds, name, ty }, source_map)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct FunctionBody {
    pub store: ExpressionStore,
    pub parameters: Box<[PatId]>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct VariantFieldsBody {
    pub store: ExpressionStore,
    pub fields: Box<[Option<ExprId>]>,
}

/// A single field of an enum variant or struct
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct FieldData {
    pub name: Name,
    pub type_ref: TypeRefId,
    pub visibility: RawVisibility,
    pub is_unsafe: bool,
    pub default_value: Option<ConstRef>,
}

// 40 bytes per field with today's 16-byte `RawVisibility`, in one array per variant.
const _: () = assert!(size_of::<FieldData>() == size_of::<RawVisibility>() + 24);

pub type LocalFieldId = Idx<FieldData>;

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct VariantFields {
    fields: Arena<FieldData>,
    pub store: ExpressionStore,
    pub shape: FieldsShape,
}

// The field array (24 bytes), the shape and the store (72 bytes today): 104 bytes in its memo.
const _: () = assert!(size_of::<VariantFields>() == size_of::<ExpressionStore>() + 32);

/// What the fields of `id` are lowered from: the [`variants_syntax`] of an enum variant's
/// enum, the [`signature_syntax`] of a struct or union. `None` for one a macro writes.
fn variant_syntax(db: &Db, id: VariantId) -> Option<&SignatureSyntax> {
    match id {
        VariantId::EnumVariantId(it) => variants_syntax(db, it.lookup(db).parent).as_ref(),
        VariantId::StructId(it) => signature_syntax(db, it.into()).as_ref(),
        VariantId::UnionId(it) => signature_syntax(db, it.into()).as_ref(),
    }
}

#[blaze_query::tracked]
impl VariantFields {
    /// What [`Self::of`] reads: the image's fields, or those lowered alone.
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    fn kept(db: &Db, id: VariantId) -> Kept<VariantFields> {
        Kept::new(crate::deps::signatures::served_variant_fields(db, id), || {
            Self::lower_alone(db, id)
        })
    }

    /// The fields with where each part of them is written in their file.
    #[blaze_query::tracked(returns(ref), lru = 1024, mem_tag = Nameres)]
    pub fn with_source_map(
        db: &Db,
        id: VariantId,
    ) -> (Arc<Self>, ExpressionStoreSourceMap) {
        let Some((it, map)) = Self::relative(db, id) else {
            let kept = Self::kept(db, id);
            let lowering = kept.lowering(&kept.store);
            let (it, map) =
                blaze_mem::frame::current(|frame| Self::lower(db, id, None, lowering, frame));
            return (kept.with_map(it), map);
        };
        let file = match id {
            VariantId::EnumVariantId(it) => file_range(db, it.lookup(db).parent.lookup(db).id),
            VariantId::StructId(it) => file_range(db, it.lookup(db).id),
            VariantId::UnionId(it) => file_range(db, it.lookup(db).id),
        };
        let syntax = variant_syntax(db, id).expect("fields lowered from no syntax");
        (it.clone(), syntax.moved(file, map)) // clone: an `Arc` count, once per memo
    }

    /// The fields lowered from their [`variant_syntax`], with the source map of that tree;
    /// `None` for a variant, struct or union a macro writes.
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub fn relative(
        db: &Db,
        id: VariantId,
    ) -> Option<(Arc<Self>, ExpressionStoreSourceMap)> {
        let syntax = variant_syntax(db, id)?;
        let kept = Self::kept(db, id);
        let lowering = kept.lowering(&kept.store);
        let (it, map) =
            blaze_mem::frame::current(|frame| Self::lower(db, id, Some(syntax), lowering, frame));
        Some((kept.with_map(it), map))
    }
}

impl VariantFields {
    /// The fields: their crate's image's, or lowered from their [`variant_syntax`] (their
    /// file for those a macro writes) with no source map. A read of them builds no map.
    #[inline]
    pub fn of(db: &Db, id: VariantId) -> &VariantFields {
        Self::kept(db, id)
    }

    /// The fields lowered from their syntax without their source map, outside any query of
    /// their own: what [`Self::of`] keeps when no image serves them, and what checking an
    /// image records.
    pub(crate) fn lower_alone(db: &Db, id: VariantId) -> VariantFields {
        let syntax = variant_syntax(db, id);
        blaze_mem::frame::current(|frame| Self::lower(db, id, syntax, BodyLowering::Body, frame)).0
    }

    fn lower(
        db: &Db,
        id: VariantId,
        syntax: Option<&SignatureSyntax>,
        lowering: BodyLowering<'_>,
        frame: &Frame,
    ) -> (Self, ExpressionStoreSourceMap) {
        let maps = syntax.map(SignatureSyntax::maps);
        let (shape, result) = match id {
            VariantId::EnumVariantId(id) => {
                let loc = id.lookup(db);
                let parent = loc.parent.lookup(db);
                let source = match syntax {
                    Some(syntax) => {
                        let variant = syntax.ast_id_map.get(loc.id.value).to_view(syntax.root());
                        InFile::new(syntax.file_id, variant)
                    }
                    None => loc.source_view(db),
                };
                let shape = adt_shape(source.value.kind());
                let enum_vis = Some(source.value.parent_enum().visibility());
                let fields = lower_field_list(
                    db,
                    parent.container,
                    source.map(|src| src.field_list()),
                    enum_vis,
                    maps,
                    lowering,
                    frame,
                );
                (shape, fields)
            }
            VariantId::StructId(id) => {
                let loc = id.lookup(db);
                let source = syntax.map_or_else(|| loc.source_view(db), SignatureSyntax::node);
                let shape = adt_shape(source.value.kind());
                let fields = lower_field_list(
                    db,
                    loc.container,
                    source.map(|src| src.field_list()),
                    None,
                    maps,
                    lowering,
                    frame,
                );
                (shape, fields)
            }
            VariantId::UnionId(id) => {
                let loc = id.lookup(db);
                let source = syntax.map_or_else(|| loc.source_view(db), SignatureSyntax::node);
                let fields = lower_field_list(
                    db,
                    loc.container,
                    source.map(|src| src.record_field_list().map(ast::FieldList::RecordFieldList)),
                    None,
                    maps,
                    lowering,
                    frame,
                );
                (FieldsShape::Record, fields)
            }
        };
        match result {
            Some((fields, store, source_map)) => (VariantFields { fields, store, shape }, source_map),
            None => (
                VariantFields {
                    fields: Arena::default(),
                    store: ExpressionStore::empty_owned(),
                    shape,
                },
                ExpressionStoreSourceMap::default(),
            ),
        }
    }
}

impl VariantFields {
    /// Fields read back from a dependency image.
    pub(crate) fn from_parts(
        fields: Arena<FieldData>,
        store: ExpressionStore,
        shape: FieldsShape,
    ) -> VariantFields {
        VariantFields { fields, store, shape }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn fields(&self) -> &Arena<FieldData> {
        &self.fields
    }

    pub fn field(&self, name: &Name) -> Option<LocalFieldId> {
        self.fields().iter().find_map(|(id, data)| if &data.name == name { Some(id) } else { None })
    }
}

fn lower_field_list(
    db: &Db,
    module: ModuleId,
    fields: InFile<Option<ast::FieldList<'_>>>,
    override_visibility: Option<Option<ast::Visibility<'_>>>,
    maps: Maps<'_>,
    lowering: BodyLowering<'_>,
    frame: &Frame,
) -> Option<(Arena<FieldData>, ExpressionStore, ExpressionStoreSourceMap)> {
    let file_id = fields.file_id;
    match fields.value? {
        ast::FieldList::RecordFieldList(fields) => lower_fields(
            db,
            module,
            fields.fields().count(),
            InFile::new(file_id, fields.fields().map(|field| (field.ty(), field))),
            |_, field| as_name_opt(field.name()),
            |field| field.eq_token().and_then(|_| field.default_val()),
            override_visibility,
            maps,
            lowering,
            frame,
        ),
        ast::FieldList::TupleFieldList(fields) => lower_fields(
            db,
            module,
            fields.fields().count(),
            InFile::new(file_id, fields.fields().map(|field| (field.ty(), field))),
            |idx, _| Name::new_tuple_field(idx),
            |_| None,
            override_visibility,
            maps,
            lowering,
            frame,
        ),
    }
}

/// The `count` fields of a list, written with their types; `None` when it has none written,
/// before anything is set up to lower them.
fn lower_fields<'t, Field: ast::HasAttrs<'t> + ast::HasVisibility<'t> + Copy>(
    db: &Db,
    module: ModuleId,
    count: usize,
    fields: InFile<impl Iterator<Item = (Option<ast::Type<'t>>, Field)>>,
    mut field_name: impl FnMut(usize, &Field) -> Name,
    mut default_value: impl FnMut(&Field) -> Option<ast::ConstArg<'t>>,
    override_visibility: Option<Option<ast::Visibility<'_>>>,
    maps: Maps<'_>,
    lowering: BodyLowering<'_>,
    frame: &Frame,
) -> Option<(Arena<FieldData>, ExpressionStore, ExpressionStoreSourceMap)> {
    if count == 0 {
        return None;
    }
    let cfg_options = module.krate(db).cfg_options(db);
    let mut col =
        ExprCollector::with_maps(db, module, fields.file_id, crate::LoweringMode::Analysis, maps);
    col.set_lowering(lowering);
    let override_visibility = override_visibility.map(|vis| {
        LazyCell::new(move || {
            let span_map = match maps {
                Some((span_map, _)) => SpanMap::RealSpanMap(span_map),
                None => fields.file_id.span_map(db),
            };
            visibility_from_ast(db, vis, &mut |range| span_map.span_for_range(range).ctx)
        })
    });

    let mut lowered = Vec::with_capacity_in(count, frame);
    let mut idx = 0;
    for (ty, field) in fields.value {
        match AttrFlags::is_cfg_enabled_for(field, cfg_options) {
            Ok(()) => {
                let type_ref =
                    col.lower_type_ref_opt(ty, &mut ImplTraitLowering::Error);
                let visibility = override_visibility.as_ref().map_or_else(
                    || {
                        visibility_from_ast(db, field.visibility(), &mut |range| {
                            col.span_map().span_for_range(range).ctx
                        })
                    },
                    |it| RawVisibility::clone(it),
                );
                let [is_unsafe] = child_tokens(field.syntax(), [T![unsafe]]);
                let name = field_name(idx, &field);
                let default_value = default_value(&field).map(|expr| col.lower_const_arg(expr));

                lowered.push(FieldData { name, type_ref, visibility, is_unsafe, default_value });
                idx += 1;
            }
            Err(cfg) => {
                col.store.diagnostics.push(
                    crate::expr_store::ExpressionStoreDiagnostics::InactiveCode {
                        node: InFile::new(fields.file_id, ast::ptr(field.syntax())),
                        cfg,
                    },
                );
            }
        }
    }
    let (store, source_map) = col.finish_store();
    // alloc: the memo's fields, exact: those the cfg left in
    let mut arena = Arena::with_capacity(lowered.len());
    for field in lowered {
        arena.alloc(field);
    }
    Some((arena, store, source_map))
}

#[derive(Debug, PartialEq, Eq)]
pub struct InactiveEnumVariantCode {
    pub cfg: CfgExpr,
    pub opts: CfgOptions,
    pub ast_id: span::FileAstId<syntax::ast::Variant>,
}

#[derive(Debug)]
pub struct EnumVariants {
    pub variants: FxIndexMap<Name, (EnumVariantId, FieldsShape)>,
    /// For each of [`EnumVariants::variants`], at the same index, whether it is written with
    /// an explicit discriminant (`= expr`): rustc's `VariantDiscr::Explicit`, read off the
    /// syntax, so that nothing a signature asks lowers the discriminant's body.
    explicit_discriminant: VariantBits,
}

/// One bit per variant, by index, kept inline up to 64 variants: no allocation for almost
/// every enum. Bits past the last variant are zero, so equal sets are equal words.
type VariantBits = Bits<SmallVec<[u64; 1]>>;

// The inline word and its length, or a heap pointer and its length, plus the capacity.
const _: () = assert!(size_of::<VariantBits>() == 24);

// The order of the variants is part of the enum: discriminants and layout follow it. Two
// `IndexMap`s with the same entries are equal whatever their order.
impl PartialEq for EnumVariants {
    fn eq(&self, other: &Self) -> bool {
        self.variants.len() == other.variants.len()
            && self.variants.iter().eq(other.variants.iter())
            && self.explicit_discriminant == other.explicit_discriminant
    }
}

impl Eq for EnumVariants {}

#[blaze_query::tracked]
impl EnumVariants {
    #[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
    pub(crate) fn of(
        db: &Db,
        e: EnumId,
    ) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        if let Some(it) = crate::deps::signatures::served_enum_variants(db, e) {
            return it;
        }
        Self::lower(db, e)
    }
}

impl EnumVariants {
    /// The variants lowered from syntax: what [`EnumVariants::of`] gives when no image serves
    /// them, and what checking an image compares its record with.
    pub(crate) fn lower(db: &Db, e: EnumId) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        blaze_mem::frame::current(|frame| {
            let loc = e.lookup(db);
            // The relative tree's ast ids are the file's: what a variant is interned by.
            let syntax = variants_syntax(db, e);
            let (source, ast_id_map) = match &syntax {
                Some(syntax) => (syntax.node::<ast::Enum<'_>>(), &syntax.ast_id_map),
                None => {
                    let source = loc.source_view(db);
                    let ast_id_map = source.file_id.ast_id_map(db);
                    (source, ast_id_map)
                }
            };

            let cfg_options = loc.container.krate(db).cfg_options(db);
            let Some(list) = source.value.variant_list() else {
                let variants = FxIndexMap::default();
                let explicit_discriminant = VariantBits::default();
                return (EnumVariants { variants, explicit_discriminant }, ThinVec::new());
            };
            let count = list.variants().count();
            let mut kept = Vec::with_capacity_in(count, frame);
            let mut inactive = Vec::new_in(frame);
            for variant in list.variants() {
                let ast_id = ast_id_map.ast_id_of_view::<syntax::ast::Variant>(variant);
                match AttrFlags::is_cfg_enabled_for(variant, cfg_options) {
                    Ok(()) => {
                        let name = as_name_opt(variant.name());
                        let id = source.with_value(ast_id);
                        // clone: a `Symbol` count: the loc and the map each keep the name
                        let loc_name = name.clone();
                        let enum_variant =
                            EnumVariantLoc { id, parent: e, name: loc_name }.intern(db);
                        // The expression the variant's body is lowered from, when it has one.
                        let explicit = variant.const_arg().and_then(|it| it.expr()).is_some();
                        kept.push((name, enum_variant, adt_shape(variant.kind()), explicit));
                    }
                    Err(cfg) => inactive.push(InactiveEnumVariantCode {
                        ast_id,
                        cfg,
                        // clone: error path, a cfg'd-out variant's diagnostic
                        opts: cfg_options.clone(),
                    }),
                }
            }
            // alloc: the memo's map, exact unless a name is written twice (an error)
            let mut variants = FxIndexMap::with_capacity_and_hasher(kept.len(), Default::default());
            let kept_len = kept.len();
            // alloc: the memo's bits, inline up to 64 variants
            let mut explicit_discriminant = VariantBits::new(kept_len);
            for (name, enum_variant, shape, explicit) in kept {
                // A name written twice keeps its first place and the last variant's value.
                let (index, _) = variants.insert_full(name, (enum_variant, shape));
                explicit_discriminant.set(index, explicit);
            }
            if variants.len() < kept_len {
                explicit_discriminant.truncate(variants.len());
                explicit_discriminant.shrink_to_fit();
            }
            // alloc: the memo's diagnostics, exact; none unless a variant is cfg'd out
            let mut diagnostics = ThinVec::with_capacity(inactive.len());
            diagnostics.extend(inactive);
            (EnumVariants { variants, explicit_discriminant }, diagnostics)
        })
    }

    /// Variants read back from a dependency image; `explicit[i]` is whether variant `i` is
    /// written with `= expr`.
    pub(crate) fn from_parts(
        variants: FxIndexMap<Name, (EnumVariantId, FieldsShape)>,
        explicit: &[bool],
    ) -> EnumVariants {
        let mut explicit_discriminant = VariantBits::new(variants.len()); // alloc: inline up to 64 variants
        for (index, &it) in explicit.iter().enumerate() {
            explicit_discriminant.set(index, it);
        }
        EnumVariants { variants, explicit_discriminant }
    }
}

impl EnumVariants {
    /// Whether `variant` is written with an explicit discriminant, `= expr`. Read off the
    /// syntax when the variants are lowered; its body is not lowered.
    pub fn has_explicit_discriminant(&self, variant: EnumVariantId) -> bool {
        self.variants
            .values()
            .position(|&(it, _)| it == variant)
            .is_some_and(|index| self.explicit_discriminant.contains(index))
    }

    /// Each variant with whether it is written with an explicit discriminant.
    pub fn with_explicit_discriminant(
        &self,
    ) -> impl Iterator<Item = (EnumVariantId, FieldsShape, bool)> + '_ {
        self.variants
            .values()
            .enumerate()
            .map(|(index, &(id, shape))| (id, shape, self.explicit_discriminant.contains(index)))
    }

    pub fn variant(&self, name: &Name) -> Option<EnumVariantId> {
        self.variants.get(name).map(|&(id, _)| id)
    }

    pub fn variant_name_by_id(&self, variant_id: EnumVariantId) -> Option<Name> {
        self.variants
            .iter()
            // clone: a `Symbol` count, the name the caller asked for
            .find_map(|(name, (id, _))| if *id == variant_id { Some(name.clone()) } else { None })
    }

    // [Adopted from rustc](https://github.com/rust-lang/rust/blob/bd53aa3bf7a24a70d763182303bd75e5fc51a9af/compiler/rustc_middle/src/ty/adt.rs#L446-L448)
    pub fn is_payload_free(&self, db: &Db) -> bool {
        self.with_explicit_discriminant().all(|(v, shape, explicit)| {
            // The condition check order is slightly modified from rustc
            // to improve performance by early returning with relatively fast checks

            // The outer if condition is whether this variant has const ctor or not
            if !matches!(shape, FieldsShape::Unit) {
                let variant = v.fields(db);
                if !variant.fields().is_empty() {
                    return false;
                }

                if explicit {
                    return false;
                }
            }
            true
        })
    }
}

#[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
pub(crate) fn extern_block_abi(db: &Db, extern_block: ExternBlockId) -> ExternAbi {
    let source = extern_block.lookup(db).source_view(db);
    source
        .value
        .abi()
        .and_then(|abi| abi.abi_string()?.text_without_quotes().parse().ok())
        .unwrap_or(ExternAbi::FALLBACK)
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;
    use syntax::{Edition, NodeOrToken, SourceFile, T};

    use super::*;

    /// Every single bit, none, all, and seeded random words.
    fn attr_inputs() -> Vec<AttrFlags> {
        let mut rng = SplitMix64::new(0x5167_5f66_6c61_6773);
        (0..128)
            .map(|bit| 1u128 << bit)
            .chain([0, u128::MAX])
            .chain(
                (0..4096).map(|_| (u128::from(rng.next_u64()) << 64) | u128::from(rng.next_u64())),
            )
            .map(AttrFlags::from_bits_retain)
            .collect()
    }

    /// The flags as they were read before: one `if` per attribute.
    fn struct_reference(attrs: AttrFlags) -> StructFlags {
        let mut flags = StructFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= StructFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= StructFlags::FUNDAMENTAL;
        }
        if attrs.contains(AttrFlags::HAS_REPR) {
            flags |= StructFlags::HAS_REPR;
        }
        flags
    }

    fn enum_reference(attrs: AttrFlags) -> EnumFlags {
        let mut flags = EnumFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= EnumFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::HAS_REPR) {
            flags |= EnumFlags::HAS_REPR;
        }
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= EnumFlags::FUNDAMENTAL;
        }
        flags
    }

    fn trait_reference(attrs: AttrFlags) -> TraitFlags {
        let mut flags = TraitFlags::empty();
        if attrs.contains(AttrFlags::FUNDAMENTAL) {
            flags |= TraitFlags::FUNDAMENTAL;
        }
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags |= TraitFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS;
        }
        if attrs.contains(AttrFlags::RUSTC_PAREN_SUGAR) {
            flags |= TraitFlags::RUSTC_PAREN_SUGAR;
        }
        if attrs.contains(AttrFlags::RUSTC_COINDUCTIVE) {
            flags |= TraitFlags::COINDUCTIVE;
        }
        if attrs.contains(AttrFlags::RUSTC_SKIP_ARRAY_DURING_METHOD_DISPATCH) {
            flags |= TraitFlags::SKIP_ARRAY_DURING_METHOD_DISPATCH;
        }
        if attrs.contains(AttrFlags::RUSTC_SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH) {
            flags |= TraitFlags::SKIP_BOXED_SLICE_DURING_METHOD_DISPATCH;
        }
        if attrs.contains(AttrFlags::RUSTC_DENY_EXPLICIT_IMPL) {
            flags |= TraitFlags::DENY_EXPLICIT_IMPL;
        }
        flags
    }

    /// As before, except that an `unsafe fn` with `#[rustc_deprecated_safe_2024]` keeps
    /// `UNSAFE` (rustc's `fn_sig` of it is unsafe).
    fn fn_reference(attrs: AttrFlags, unsafe_: bool) -> FnFlags {
        let mut flags = FnFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL) {
            flags.insert(FnFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
        }
        if attrs.contains(AttrFlags::HAS_TARGET_FEATURE) {
            flags.insert(FnFlags::HAS_TARGET_FEATURE);
        }
        if attrs.contains(AttrFlags::RUSTC_INTRINSIC) {
            flags.insert(FnFlags::RUSTC_INTRINSIC);
        }
        if attrs.contains(AttrFlags::HAS_LEGACY_CONST_GENERICS) {
            flags.insert(FnFlags::HAS_LEGACY_CONST_GENERICS);
        }
        if unsafe_ {
            flags.insert(FnFlags::UNSAFE);
            if attrs.contains(AttrFlags::RUSTC_DEPRECATED_SAFE_2024) {
                flags.insert(FnFlags::DEPRECATED_SAFE_2024);
            }
        }
        flags
    }

    fn type_alias_reference(attrs: AttrFlags) -> TypeAliasFlags {
        let mut flags = TypeAliasFlags::empty();
        if attrs.contains(AttrFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPLS) {
            flags.insert(TypeAliasFlags::RUSTC_HAS_INCOHERENT_INHERENT_IMPL);
        }
        if attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL) {
            flags.insert(TypeAliasFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
        }
        flags
    }

    #[test]
    fn flags_from_attrs_as_one_if_per_flag() {
        for attrs in attr_inputs() {
            assert_eq!(StructFlags::from_attrs(attrs), struct_reference(attrs), "{attrs:?}");
            assert_eq!(EnumFlags::from_attrs(attrs), enum_reference(attrs), "{attrs:?}");
            assert_eq!(TraitFlags::from_attrs(attrs), trait_reference(attrs), "{attrs:?}");
            assert_eq!(TypeAliasFlags::from_attrs(attrs), type_alias_reference(attrs), "{attrs:?}");
            for unsafe_ in [false, true] {
                assert_eq!(
                    FnFlags::from_attrs(attrs, unsafe_),
                    fn_reference(attrs, unsafe_),
                    "{attrs:?} unsafe: {unsafe_}"
                );
            }
            let allow = attrs.contains(AttrFlags::RUSTC_ALLOW_INCOHERENT_IMPL);
            let const_flags = flags_from_attrs!(attrs, ConstFlags {
                RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            });
            assert_eq!(const_flags.contains(ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL), allow);
            assert_eq!(const_flags - ConstFlags::RUSTC_ALLOW_INCOHERENT_IMPL, ConstFlags::empty());
            let static_flags = flags_from_attrs!(attrs, StaticFlags {
                RUSTC_ALLOW_INCOHERENT_IMPL => RUSTC_ALLOW_INCOHERENT_IMPL,
            });
            assert_eq!(static_flags.contains(StaticFlags::RUSTC_ALLOW_INCOHERENT_IMPL), allow);
            assert_eq!(
                static_flags - StaticFlags::RUSTC_ALLOW_INCOHERENT_IMPL,
                StaticFlags::empty()
            );
        }
    }

    #[test]
    fn struct_lang_items_as_a_match() {
        let reference = |lang: &Symbol| match lang {
            _ if *lang == sym::phantom_data => StructFlags::IS_PHANTOM_DATA,
            _ if *lang == sym::owned_box => StructFlags::IS_BOX,
            _ if *lang == sym::manually_drop => StructFlags::IS_MANUALLY_DROP,
            _ if *lang == sym::unsafe_cell => StructFlags::IS_UNSAFE_CELL,
            _ if *lang == sym::covariant_unsafe_cell => StructFlags::IS_COVARIANT_UNSAFE_CELL,
            _ if *lang == sym::unsafe_pinned => StructFlags::IS_UNSAFE_PINNED,
            _ if *lang == sym::f16b => StructFlags::IS_F16B,
            _ if *lang == sym::ppcf128 => StructFlags::IS_PPC_F128,
            _ if *lang == sym::field_representing_type => StructFlags::IS_FIELD_REPRESENTING_TYPE,
            _ => StructFlags::empty(),
        };
        let others = [sym::sized, sym::copy, sym::non_null, sym::va_list, sym::drop];
        let interned = ["phantom_data", "owned_box", "ppcf128", "not_a_lang_item"].map(Symbol::intern);
        for lang in STRUCT_LANG_ITEMS.iter().map(|(it, _)| it.clone()).chain(others).chain(interned) {
            assert_eq!(StructFlags::from_lang_item(&lang), reference(&lang), "{lang:?}");
        }
        for (i, (lang, _)) in STRUCT_LANG_ITEMS.iter().enumerate() {
            assert_eq!(STRUCT_LANG_ITEM_NAMES.get(lang), Some(i as u16), "{lang:?}");
        }
    }

    const KEYWORDS: &str = r#"
unsafe fn a() {}
const async unsafe extern "C" fn b() {}
impl Z { default fn c() {} }
pub(crate) const fn d() {}
fn e() -> i32 { 0 }
async gen fn f() {}
unsafe extern "C" {
    safe fn g();
    unsafe fn h();
    safe static S: u8;
    unsafe static mut T: u8;
    static U: u8;
}
static mut V: u8 = 0;
static W: u8 = 0;
unsafe impl Send for X {}
impl !Sync for X {}
default impl<T> Tr for T {}
impl const Tr for X {}
impl Y {}
auto trait Au {}
unsafe trait Un {}
trait Al = Clone;
const trait Co {}
trait Pl {}
struct R { unsafe a: u8, b: u8, pub c: u8 }
struct Q(pub u8, u8);
impl<T> Tr for T { default type A = u8; default const C: u8 = 0; type B = u8; const D: u8 = 0; }
type const E: u8 = 0;
"#;

    #[test]
    fn child_tokens_as_the_accessors() {
        let parse = SourceFile::parse(KEYWORDS, Edition::CURRENT);
        let mut seen_fn = [false; 6];
        let mut seen_static = [false; 3];
        let mut seen_impl = [false; 4];
        let mut seen_trait = [false; 3];
        let mut seen_unsafe_field = false;
        let mut seen_default = [false; 2];
        let mut seen_type_const = false;
        fn or(seen: &mut [bool], found: &[bool]) {
            seen.iter_mut().zip(found).for_each(|(seen, found)| *seen |= *found)
        }
        for node in parse.root().descendants() {
            if let Some(it) = ast::Fn::cast(node) {
                let found = child_tokens(
                    it.syntax(),
                    [T![unsafe], T![async], T![gen], T![const], T![default], T![safe]],
                );
                let accessors = [
                    it.unsafe_token().is_some(),
                    it.async_token().is_some(),
                    it.gen_token().is_some(),
                    it.const_token().is_some(),
                    it.default_token().is_some(),
                    it.safe_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_fn, &found);
            } else if let Some(it) = ast::Static::cast(node) {
                let found = child_tokens(it.syntax(), [T![mut], T![unsafe], T![safe]]);
                let accessors = [
                    it.mut_token().is_some(),
                    it.unsafe_token().is_some(),
                    it.safe_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_static, &found);
            } else if let Some(it) = ast::Impl::cast(node) {
                let found =
                    child_tokens(it.syntax(), [T![unsafe], T![!], T![default], T![const]]);
                let accessors = [
                    it.unsafe_token().is_some(),
                    it.excl_token().is_some(),
                    it.default_token().is_some(),
                    it.const_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_impl, &found);
            } else if let Some(it) = ast::Trait::cast(node) {
                let found = child_tokens(it.syntax(), [T![auto], T![unsafe], T![=], T![const]]);
                let accessors = [
                    it.auto_token().is_some(),
                    it.unsafe_token().is_some(),
                    it.eq_token().is_some(),
                    it.const_token().is_some(),
                ];
                assert_eq!(found, accessors, "{it}");
                or(&mut seen_trait, &found[..3]);
            } else if let Some(it) = ast::RecordField::cast(node) {
                let [found] = child_tokens(it.syntax(), [T![unsafe]]);
                let scanned = it
                    .syntax()
                    .children_with_tokens()
                    .filter_map(NodeOrToken::into_token)
                    .any(|token| token.kind() == T![unsafe]);
                assert_eq!(found, scanned, "{it}");
                seen_unsafe_field |= found;
            } else if let Some(it) = ast::Const::cast(node) {
                let found = child_tokens(it.syntax(), [T![default], T![type]]);
                assert_eq!(
                    found,
                    [it.default_token().is_some(), it.type_token().is_some()],
                    "{it}"
                );
                seen_default[0] |= found[0];
                seen_type_const |= found[1];
            } else if let Some(it) = ast::TypeAlias::cast(node) {
                let found = child_tokens(it.syntax(), [T![default]]);
                assert_eq!(found, [it.default_token().is_some()], "{it}");
                seen_default[1] |= found[0];
            } else if let Some(it) = ast::TupleField::cast(node) {
                let [found] = child_tokens(it.syntax(), [T![unsafe]]);
                assert!(!found, "{it}");
            }
        }
        assert_eq!(seen_default, [true; 2]);
        assert!(seen_type_const);
        // `gen` is left out: whether `async gen fn` parses is the parser's.
        assert_eq!([seen_fn[0], seen_fn[1], seen_fn[3], seen_fn[4], seen_fn[5]], [true; 5]);
        assert_eq!(seen_static, [true; 3]);
        assert_eq!(seen_impl, [true; 4]);
        assert_eq!(seen_trait, [true; 3]);
        assert!(seen_unsafe_field);
    }

    #[test]
    fn variant_bits_as_bools() {
        let mut rng = SplitMix64::new(0x7661_7269_616e_7473);
        for len in [0, 1, 2, 63, 64, 65, 127, 128, 129, 200, 1000] {
            // Only the first `kept` are ever set, as only the variants kept get a bit.
            let kept = if len == 0 { 0 } else { rng.index(len + 1) };
            let mut model = vec![false; kept];
            let mut bits = VariantBits::new(len);
            for _ in 0..3 * len {
                if kept == 0 {
                    break;
                }
                let index = rng.index(kept);
                let value = rng.coin();
                model[index] = value;
                bits.set(index, value);
                assert_eq!(bits.contains(index), value);
            }
            for (index, &value) in model.iter().enumerate() {
                assert_eq!(bits.contains(index), value, "len {len} index {index}");
            }
            for index in kept..len {
                assert!(!bits.contains(index), "len {len} index {index}");
            }

            bits.truncate(kept);
            let mut fresh = VariantBits::new(kept);
            for (index, &value) in model.iter().enumerate() {
                fresh.set(index, value);
            }
            assert_eq!(bits, fresh, "len {len} kept {kept}");
            for (index, &value) in model.iter().enumerate() {
                assert_eq!(bits.contains(index), value, "len {len} kept {kept} index {index}");
            }
        }
        assert!(!VariantBits::new(64).into_words().spilled());
        assert!(VariantBits::new(65).into_words().spilled());
    }
}

#[cfg(test)]
mod budgets;

#[cfg(test)]
mod alone_tests;

/// What an edit of a file lowers again of the signatures: each is lowered from its own
/// [`SignatureSyntax`], so an edit of a body, or of another item, lowers none of them.
#[cfg(test)]
mod relative_tests {
    use hir_expand::fixture;

    use super::*;
    use crate::{
        AssocItemId, ModuleDefId,
        nameres::{
            assoc::{ImplItems, TraitItems},
            crate_def_map,
        },
        test_db::{TestDB, TestDBExt},
    };

    pub(super) const CODE: &str = r#"
pub struct S<T> { a: T, b: u32 }
pub union U { a: u32, b: f32 }
pub enum E<T> { A = 1, B(T), C { c: u8 } }
macro_rules! ty { () => { u32 } }
pub trait Tr { const C: u32; fn d(&self) -> u32 { 41 } }
impl Tr for S<u32> { const C: u32 = 2; fn d(&self) -> u32 { self.b + 5 } }
pub type A = S<ty!()>;
pub const K: u32 = 3;
pub static L: u32 = K + 1;
pub fn f(x: u32) -> ty!() { x + 11 }
pub fn g<T: Tr>(t: &T) -> u32 where T: Copy { t.d() }
#[cfg(all())]
impl S<u8> { fn h(&self) -> u8 { 0 } }
"#;

    /// Each body of `CODE` edited, the text around it moved.
    const BODY_EDITS: [(&str, &str); 7] = [
        ("{ 41 }", "{ 41 + 1 }"),
        ("= 2;", "= 2 * 2;"),
        ("{ self.b + 5 }", "{ self.b * 5 + 1 }"),
        ("= 3;", "= 3 + 4;"),
        ("= K + 1;", "= K + 100;"),
        ("{ x + 11 }", "{ (x + 11) * 3 }"),
        ("{ t.d() }", "{ t.d() + t.d() }"),
    ];

    /// `code` as the fixture writes it, without its first newline, with `edits` made.
    fn edited(code: &str, edits: &[(&str, &str)]) -> String {
        let code = code.strip_prefix('\n').unwrap_or(code);
        edits.iter().fold(code.to_owned(), |code, (from, to)| {
            assert_eq!(code.matches(from).count(), 1, "`{from}` is not written once");
            code.replace(from, to)
        })
    }

    fn assoc(item: AssocItemId) -> GenericDefId {
        match item {
            AssocItemId::FunctionId(it) => it.into(),
            AssocItemId::ConstId(it) => it.into(),
            AssocItemId::TypeAliasId(it) => it.into(),
        }
    }

    /// Every signature of the test crate, items of traits and impls too, and every variant.
    fn defs(db: &TestDB) -> (Vec<GenericDefId>, Vec<VariantId>) {
        let (mut signatures, mut variants) = (Vec::new(), Vec::new());
        for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
            let scope = module.scope(db);
            for decl in scope.declarations() {
                match decl {
                    ModuleDefId::FunctionId(it) => signatures.push(it.into()),
                    ModuleDefId::ConstId(it) => signatures.push(it.into()),
                    ModuleDefId::StaticId(it) => signatures.push(it.into()),
                    ModuleDefId::TypeAliasId(it) => signatures.push(it.into()),
                    ModuleDefId::TraitId(it) => {
                        signatures.push(it.into());
                        let items = TraitItems::query(db, it).items.iter();
                        signatures.extend(items.map(|&(_, item)| assoc(item)));
                    }
                    ModuleDefId::AdtId(it) => {
                        signatures.push(it.into());
                        match it {
                            AdtId::StructId(it) => variants.push(it.into()),
                            AdtId::UnionId(it) => variants.push(it.into()),
                            AdtId::EnumId(it) => {
                                let of = EnumVariants::of(db, it).0.variants.values();
                                variants.extend(of.map(|&(v, _)| VariantId::from(v)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            for imp in scope.impls() {
                signatures.push(imp.into());
                let items = ImplItems::of(db, imp).0.items.iter();
                signatures.extend(items.map(|&(_, item)| assoc(item)));
            }
        }
        (signatures, variants)
    }

    /// Where each signature, each variant's fields and each enum's variants are kept: what
    /// lowering them again makes anew.
    fn lowered(db: &TestDB) -> Vec<usize> {
        fn at<T>(it: &T) -> usize {
            std::ptr::from_ref(it) as usize
        }
        let (signatures, variants) = defs(db);
        let signatures = signatures.into_iter().map(|def| match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => at(StructSignature::of(db, it)),
            GenericDefId::AdtId(AdtId::UnionId(it)) => at(UnionSignature::of(db, it)),
            GenericDefId::AdtId(AdtId::EnumId(it)) => {
                at(EnumSignature::of(db, it)) ^ at(EnumVariants::of(db, it))
            }
            GenericDefId::ConstId(it) => at(ConstSignature::of(db, it)),
            GenericDefId::StaticId(it) => at(StaticSignature::of(db, it)),
            GenericDefId::ImplId(it) => at(ImplSignature::of(db, it)),
            GenericDefId::TraitId(it) => at(TraitSignature::of(db, it)),
            GenericDefId::FunctionId(it) => at(FunctionSignature::of(db, it)),
            GenericDefId::TypeAliasId(it) => at(TypeAliasSignature::of(db, it)),
        });
        let variants = variants.into_iter().map(|it| at(VariantFields::of(db, it)));
        signatures.chain(variants).collect()
    }

    /// The signature lowerings in `log`: `kept` lowers what `of` reads, `relative` lowers it
    /// again with its source map.
    fn lowerings(log: &[String]) -> Vec<&str> {
        let lowering = |it: &&String| {
            it.contains("Signature::kept")
                || it.contains("Signature::relative")
                || it.contains("VariantFields::kept")
                || it.contains("VariantFields::relative")
                || it.contains("EnumVariants::of")
        };
        log.iter().filter(lowering).map(String::as_str).collect()
    }

    /// `edits` of `CODE`, with every signature asked for before and after: what they lower.
    fn lowered_by(edits: &[(&str, &str)]) -> (Vec<String>, Vec<usize>, Vec<usize>) {
        let fx = fixture::db(CODE);
        let file = fx.only_file();
        let mut db = fx.into_db();
        let before = lowered(&db);
        assert!(lowerings(&db.executed_in_read_order(|| _ = lowered(&db))).is_empty());
        db.set_file_text(file.file_id(&db), &edited(CODE, edits));
        let mut after = Vec::new();
        let log = db.executed_in_read_order(|| after = lowered(&db));
        (log, before, after)
    }

    #[test]
    fn a_body_edit_lowers_no_signature() {
        let (log, before, after) = lowered_by(&BODY_EDITS);
        assert_eq!(lowerings(&log), Vec::<&str>::new(), "{log:#?}");
        assert_eq!(before, after, "a signature was lowered anew");
    }

    #[test]
    fn an_edit_of_another_item_lowers_none_of_this_one() {
        // `S` gets a field: its signature and fields are lowered again, and every item after
        // it moves, but none of them is lowered again.
        let (log, ..) = lowered_by(&[("a: T, b: u32", "a: T, b: u32, c: u64")]);
        let mut lowerings = lowerings(&log);
        lowerings.sort_unstable();
        assert_eq!(lowerings.len(), 2, "{log:#?}");
        assert!(lowerings[0].contains("StructSignature::kept"), "{lowerings:?}");
        assert!(lowerings[1].contains("VariantFields::kept"), "{lowerings:?}");
    }

    #[test]
    fn a_discriminant_edit_lowers_the_variants_not_the_enum() {
        let (log, ..) = lowered_by(&[("A = 1", "A = 1 + 1")]);
        let lowerings = lowerings(&log);
        assert!(!lowerings.iter().any(|it| it.contains("EnumSignature")), "{log:#?}");
        assert!(lowerings.iter().any(|it| it.contains("EnumVariants::of")), "{log:#?}");
    }

    /// `HAS_UNSAFE_FIELDS` is set for a struct, union or enum with a field written `unsafe`
    /// that cfg keeps (rustc's `all_fields().any(|f| f.safety.is_unsafe())`), and an enum's
    /// follows an edit of its variants.
    #[test]
    fn unsafe_fields_set_the_adt_flag() {
        const UNSAFE: &str = r#"
pub struct S { unsafe a: u32, b: u32 }
pub struct T { a: u32, #[cfg(any())] unsafe b: u32 }
pub union U { unsafe a: u32 }
pub enum E { A, B { unsafe b: u8 } }
pub enum F { A(u8), #[cfg(any())] B { unsafe b: u8 } }
"#;
        fn flags(db: &TestDB) -> Vec<(String, bool)> {
            let mut found = Vec::new();
            for (_, module) in crate_def_map(db, db.fetch_test_crate()).modules(db) {
                for decl in module.scope(db).declarations() {
                    let ModuleDefId::AdtId(adt) = decl else { continue };
                    let (name, has) = match adt {
                        AdtId::StructId(it) => {
                            let sig = StructSignature::of(db, it);
                            (&sig.name, sig.flags.contains(StructFlags::HAS_UNSAFE_FIELDS))
                        }
                        AdtId::UnionId(it) => {
                            let sig = UnionSignature::of(db, it);
                            (&sig.name, sig.flags.contains(StructFlags::HAS_UNSAFE_FIELDS))
                        }
                        AdtId::EnumId(it) => {
                            let sig = EnumSignature::of(db, it);
                            (&sig.name, sig.flags.contains(EnumFlags::HAS_UNSAFE_FIELDS))
                        }
                    };
                    found.push((name.as_str().to_owned(), has));
                }
            }
            found.sort();
            found
        }
        let fx = fixture::db(UNSAFE);
        let file = fx.only_file();
        let mut db = fx.into_db();
        let want = |f| {
            [("E", true), ("F", f), ("S", true), ("T", false), ("U", true)]
                .map(|(name, has)| (name.to_owned(), has))
                .to_vec()
        };
        assert_eq!(flags(&db), want(false));
        db.set_file_text(file.file_id(&db), &edited(UNSAFE, &[("A(u8)", "A(u8), C { unsafe c: u8 }")]));
        assert_eq!(flags(&db), want(true));
    }

    /// The bodies an outline of `CODE` leaves out: those of every fn, const and static that is
    /// its own anchor, which are what `BODY_EDITS` edits, and not `h`'s, whose impl has an
    /// attribute.
    const LEFT_OUT: [(&str, &str); 7] = [
        ("{ 41 }", ""),
        ("= 2;", "= ;"),
        ("{ self.b + 5 }", ""),
        ("= 3;", "= ;"),
        ("= K + 1;", "= ;"),
        ("{ x + 11 }", ""),
        ("{ t.d() }", ""),
    ];

    #[test]
    fn the_outline_leaves_out_each_body_of_its_own_anchor() {
        let fx = fixture::db(CODE);
        let file = fx.only_file();
        let db = fx.into_db();
        let outline = file_outline(&db, file);
        assert_eq!(outline.root().text(), edited(CODE, &LEFT_OUT));
        assert_eq!(outline.cuts.len(), BODY_EDITS.len(), "{outline:?}");
        let root = outline.root();
        for cut in &outline.cuts {
            assert!(cut.text_range().is_empty(), "{cut:?}");
            let item = root
                .descendants()
                .find(|&it| left_out(it).is_some_and(|it| ast::ptr(it) == *cut));
            assert!(item.is_some(), "{cut:?} is not the body of an item in the outline");
        }
    }

    /// A fn body that may start with an inner attribute holds attributes of its fn, so the
    /// outline keeps it; a comment first is skipped, and a const's block is its own.
    #[test]
    fn the_outline_keeps_a_body_an_inner_attribute_may_start() {
        const INNER: &str = r#"
pub fn kept() { #![rustc_nounwind] }
pub fn after_comment() { // why
    #![rustc_nounwind] }
pub fn outer_first() { #[allow(unused)] let x = 1; }
pub fn cut() { /* none */ 1; }
pub const K: u8 = { #![allow(unused)] 1 };
"#;
        const LEFT: &str = r#"
pub fn kept() { #![rustc_nounwind] }
pub fn after_comment() { // why
    #![rustc_nounwind] }
pub fn outer_first() { #[allow(unused)] let x = 1; }
pub fn cut() 
pub const K: u8 = ;
"#;
        let fx = fixture::db(INNER);
        let file = fx.only_file();
        let db = fx.into_db();
        let left = edited(LEFT, &[]);
        assert_eq!(file_outline(&db, file).root().to_string(), left);
        assert_eq!(syntax::view::AstNode::syntax(&outline_root(&db, file)).to_string(), left);
    }

    /// What a body edit leaves the outline the same in, one part at a time.
    #[test]
    fn a_body_edit_keeps_each_part_of_the_outline() {
        let fx = fixture::db(CODE);
        let file = fx.only_file();
        let mut db = fx.into_db();
        let before = file_outline(&db, file);
        // clone: the outline before, kept past the edit
        let (text, cuts) = (before.text(), before.cuts.clone());
        let (span_map, ast_id_map) = (format!("{:?}", before.span_map), format!("{:?}", before.ast_id_map));
        // clone: a count of the tree before
        let tree = before.tree.clone();
        db.set_file_text(file.file_id(&db), &edited(CODE, &BODY_EDITS));
        let after = file_outline(&db, file);
        assert_eq!(after.text(), text, "text");
        assert_eq!(after.cuts, cuts, "cuts");
        assert_eq!(format!("{:?}", after.span_map), span_map, "span map");
        assert_eq!(format!("{:?}", after.ast_id_map), ast_id_map, "ast id map");
        assert!(*after.tree == *tree, "tree");
    }

    #[test]
    fn a_body_edit_runs_no_signature_syntax() {
        let fx = fixture::db(CODE);
        let file = fx.only_file();
        let mut db = fx.into_db();
        _ = lowered(&db);
        db.set_file_text(file.file_id(&db), &edited(CODE, &BODY_EDITS));
        let log = db.executed_in_read_order(|| _ = lowered(&db));
        assert!(log.iter().any(|it| it.contains("file_outline")), "{log:#?}");
        let syntax =
            |it: &&String| it.contains("signature_syntax") || it.contains("variants_syntax");
        assert_eq!(log.iter().filter(syntax).count(), 0, "{log:#?}");
    }

    /// An item in a body the outline leaves out is read off the file, its anchor whole; an
    /// item in the signature of a fn whose body is left out is read off the outline, the
    /// anchor without that body. Both keep the file's ast id for the item.
    #[test]
    fn items_anchored_at_a_fn_are_read_around_its_body() {
        const NESTED: &str = r#"
pub fn outer() -> u32 { struct Inner<T>(T); 1 }
pub fn p(x: [u8; { struct InType; 1 }]) -> u8 { x[0] }
"#;
        let fx = fixture::db(NESTED);
        let file = fx.only_file();
        let db = fx.into_db();
        let file_id = HirFileId::from(file);
        let root = file_id.root(&db);
        let named = |name: &str| {
            root.descendants()
                .filter_map(ast::Struct::cast)
                .find(|it| it.name().is_some_and(|it| it.text() == name))
                .unwrap_or_else(|| panic!("no struct {name}"))
        };
        let nothing: Elide = |_| None;
        let krate = crate::test_db::TestDBExt::fetch_test_crate(&db);
        let root_module = crate::nameres::crate_def_map(&db, krate).root;
        for (name, text) in [
            ("Inner", "pub fn outer() -> u32 { struct Inner<T>(T); 1 }"),
            ("InType", "pub fn p(x: [u8; { struct InType; 1 }]) -> u8 "),
        ] {
            let item = named(name);
            let id = InFile::new(
                file_id,
                file_id.ast_id_map(&db).ast_id_of_view::<syntax::ast::Struct>(item),
            );
            let syntax = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                item_syntax(&db, id, root_module, nothing, frame)
            })
            .unwrap_or_else(|| panic!("{name}"));
            assert_eq!(syntax.root().text(), text, "{name}");
            assert_eq!(syntax.node::<ast::Struct<'_>>().value.syntax().text(), item.syntax().text());
            assert_eq!(syntax.ast_id_map.get_erased(id.value.erase()), syntax.item, "{name}");
            assert_eq!(syntax.elided, None, "{name}");
        }
    }

    /// Allocation budget: the outline's walks on views, `bodies_under` over a file and
    /// `child_tokens` of every node in it, count nothing and allocate nothing once the file's
    /// lazy bodies are parsed.
    #[test]
    fn walking_the_outline_allocates_nothing() {
        let fx = fixture::db(CODE);
        let file = fx.only_file();
        let db = fx.into_db();
        let root = HirFileId::from(file).root(&db);
        let walk = || {
            let mut bodies = 0;
            bodies_under(root, |_| bodies += 1);
            let mut found = 0;
            for node in root.descendants() {
                let [unsafe_, const_] = child_tokens(node, [T![unsafe], T![const]]);
                found += usize::from(unsafe_) + usize::from(const_);
            }
            std::hint::black_box((bodies, found))
        };
        // The bodies `LEFT_OUT` names, and `h`'s, which its impl's attribute keeps.
        assert_eq!(walk().0, LEFT_OUT.len() + 1);
        blaze_alloc_count::assert_at_most(0, "walking a file's outline through views", || {
            walk();
        });
    }

    /// Every read of a lowered signature: the memo hits of its queries, and each field, variant
    /// and flag read off what they hold.
    fn read(db: &TestDB, def: GenericDefId) {
        use std::hint::black_box;
        black_box(signature_syntax(db, def));
        match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => {
                black_box(StructSignature::of(db, it));
                black_box(StructSignature::with_source_map(db, it));
            }
            GenericDefId::AdtId(AdtId::UnionId(it)) => {
                black_box(UnionSignature::of(db, it));
                black_box(UnionSignature::with_source_map(db, it));
            }
            GenericDefId::AdtId(AdtId::EnumId(it)) => {
                black_box(EnumSignature::of(db, it));
                black_box(EnumSignature::with_source_map(db, it));
                black_box(variants_syntax(db, it));
                let (variants, _) = EnumVariants::of(db, it);
                for (name, &(variant, _)) in &variants.variants {
                    black_box(variants.variant(name));
                    black_box(variants.has_explicit_discriminant(variant));
                }
                black_box(variants.is_payload_free(db));
            }
            GenericDefId::ConstId(it) => {
                black_box(ConstSignature::of(db, it).has_body());
                black_box(ConstSignature::with_source_map(db, it));
            }
            GenericDefId::StaticId(it) => {
                black_box(StaticSignature::of(db, it));
                black_box(StaticSignature::with_source_map(db, it));
            }
            GenericDefId::ImplId(it) => {
                black_box(ImplSignature::of(db, it).is_negative());
                black_box(ImplSignature::with_source_map(db, it));
            }
            GenericDefId::TraitId(it) => {
                black_box(TraitSignature::of(db, it));
                black_box(TraitSignature::with_source_map(db, it));
            }
            GenericDefId::FunctionId(it) => {
                black_box(FunctionSignature::of(db, it));
                black_box(FunctionSignature::with_source_map(db, it));
            }
            GenericDefId::TypeAliasId(it) => {
                black_box(TypeAliasSignature::of(db, it));
                black_box(TypeAliasSignature::with_source_map(db, it));
            }
        }
    }

    fn read_fields(db: &TestDB, id: VariantId) {
        use std::hint::black_box;
        let fields = VariantFields::of(db, id);
        black_box(VariantFields::with_source_map(db, id));
        for (field, data) in fields.fields().iter() {
            black_box((field, data));
            black_box(fields.field(&data.name));
        }
    }

    /// Allocation budget: once lowered, reading a signature, its fields or its variants is a
    /// memo hit and reads of what it holds, and allocates nothing.
    #[test]
    fn reading_signatures_allocates_nothing() {
        let fx = fixture::db(CODE);
        let _ = fx.only_file();
        let db = fx.into_db();
        let (signatures, variants) = defs(&db);
        assert!(signatures.len() > 15 && variants.len() > 4, "too small to measure");
        for &def in &signatures {
            read(&db, def);
        }
        for &id in &variants {
            read_fields(&db, id);
        }
        blaze_alloc_count::assert_at_most(0, "reading every signature, field and variant", || {
            for &def in &signatures {
                read(&db, def);
            }
            for &id in &variants {
                read_fields(&db, id);
            }
        });
    }

    /// What `with_source_map` gives, moved from the item's tree to the file, is what lowering
    /// from the whole file gives: the same signature, each part at the same place.
    #[test]
    fn moved_source_maps_are_the_files() {
        macro_rules! check {
            ($db:ident, $Signature:ident, $id:expr) => {{
                let id = $id;
                let (signature, map) = $Signature::with_source_map(&$db, id);
                let source = id.lookup(&$db).source_view(&$db);
                let (whole, whole_map) =
                    $Signature::lower(&$db, id, source, None, BodyLowering::Whole);
                assert_eq!(**signature, whole, "{id:?}");
                assert!(*map == whole_map, "{id:?}: {map:#?}\nfrom the file: {whole_map:#?}");
            }};
        }
        let fx = fixture::db(CODE);
        let file = fx.only_file();
        let mut db = fx.into_db();
        for text in [CODE.to_owned(), edited(CODE, &BODY_EDITS)] {
            db.set_file_text(file.file_id(&db), &text);
            let (signatures, variants) = defs(&db);
            for def in signatures {
                assert!(signature_syntax(&db, def).is_some(), "{def:?} has no syntax of its own");
                match def {
                    GenericDefId::AdtId(AdtId::StructId(it)) => check!(db, StructSignature, it),
                    GenericDefId::AdtId(AdtId::UnionId(it)) => check!(db, UnionSignature, it),
                    GenericDefId::AdtId(AdtId::EnumId(it)) => check!(db, EnumSignature, it),
                    GenericDefId::ConstId(it) => check!(db, ConstSignature, it),
                    GenericDefId::StaticId(it) => check!(db, StaticSignature, it),
                    GenericDefId::ImplId(it) => check!(db, ImplSignature, it),
                    GenericDefId::TraitId(it) => check!(db, TraitSignature, it),
                    GenericDefId::FunctionId(it) => check!(db, FunctionSignature, it),
                    GenericDefId::TypeAliasId(it) => check!(db, TypeAliasSignature, it),
                }
            }
            for id in variants {
                let (fields, map) = VariantFields::with_source_map(&db, id);
                let (whole, whole_map) = blaze_mem::frame::task(blaze_mem::Tag::Nameres, |frame| {
                    VariantFields::lower(&db, id, None, BodyLowering::Whole, frame)
                });
                assert_eq!(**fields, whole, "{id:?}");
                assert!(*map == whole_map, "{id:?}: {map:#?}\nfrom the file: {whole_map:#?}");
            }
        }
    }
}
