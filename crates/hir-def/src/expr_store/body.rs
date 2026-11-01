//! Defines `Body`: a lowered representation of functions, statics and
//! consts.
use std::ops;

use base_db::{Db, EditionedFileId};
use blaze_hash::HashMap;
use hir_expand::{HirFileId, InFile, Lookup, span_map::SpanMap};
use span::{AstIdMap, Edition, ErasedFileAstId, ROOT_ERASED_FILE_AST_ID, RealSpanMap};
use syntax::{
    FlatTree, NodeOrToken, SyntaxKind, SyntaxNodePtr, TextRange, TextSize,
    view::{self, AstNode as _},
};
use triomphe::Arc;

use crate::{
    DefWithBodyId, ExpressionStoreOwnerId, HasModule,
    expr_store::{
        ExpressionStore, ExpressionStoreDiagnostics, ExpressionStoreSourceMap, Move, MoveTo,
        SelfParamPtr,
        lower::{BodyLowering, lower_body},
        pretty,
    },
    hir::{BindingId, ExprId, PatId},
    src::HasSource,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Param<Id> {
    /// The id of the formal parameter. In analysis, you want to use this: it has a special status as a parameter,
    /// while [`user_written`][Self::user_written] is just a local variable.
    pub formal: Id,
    /// The id that corresponds to the pattern the user wrote. In IDE, you usually want to use this.
    ///
    /// It will be different than [`formal`][Self::formal] for coroutine fns (async fn etc.), because we apply
    /// a desugaring for those that copies the parameter into a local variable.
    pub user_written: Id,
}

impl<Id: Copy> Param<Id> {
    pub(crate) fn new(id: Id) -> Self {
        Self { formal: id, user_written: id }
    }
}

/// The body of an item (function, const etc.).
#[derive(Debug, Eq, PartialEq, Hash)]
pub struct Body {
    pub store: ExpressionStore,
    /// The patterns for the function's parameters. While the parameter types are
    /// part of the function signature, the patterns are not (they don't change
    /// the external type of the function).
    ///
    /// If this `Body` is for the body of a constant, this will just be
    /// empty.
    pub params: Box<[Param<PatId>]>,
    pub self_param: Option<Param<BindingId>>,
}

impl ops::Deref for Body {
    type Target = ExpressionStore;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

/// An item body together with the mapping from syntax nodes to HIR expression
/// IDs. This is needed to go from e.g. a position in a file to the HIR
/// expression containing it; but for type inference etc., we want to operate on
/// a structure that is agnostic to the actual positions of expressions in the
/// file, so that we don't recompute types whenever some whitespace is typed.
///
/// One complication here is that, due to macro expansion, a single `Body` might
/// be spread across several files. So, for each ExprId and PatId, we record
/// both the HirFileId and the position inside the file. However, we only store
/// AST -> ExprId mapping for non-macro files, as it is not clear how to handle
/// this properly for macros.
#[derive(Default, Debug, Eq, PartialEq, Hash)]
pub struct BodySourceMap {
    pub self_param: Option<InFile<SelfParamPtr>>,
    pub store: ExpressionStoreSourceMap,
}

impl ops::Deref for BodySourceMap {
    type Target = ExpressionStoreSourceMap;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.store
    }
}

#[blaze_query::tracked]
impl Body {
    /// The body with where each part of it is written. For a body written in a file, this is
    /// [`Body::relative`] with its source map moved to where the body is in the file: the map
    /// is made once, and an edit elsewhere in the file only moves it. The body is equal to
    /// [`Body::of`], with the same ids, but lowered again with the map.
    #[blaze_query::tracked(lru = 512, returns(ref))]
    pub fn with_source_map(
        db: &Db,
        def: DefWithBodyId,
    ) -> (Arc<Body>, BodySourceMap) {
        let _p = crate::body_span!(db, def, "body_with_source_map_query").entered();
        let Some((body, source_map)) = Self::relative(db, def) else {
            let of = Self::of(db, def);
            let (body, source_map) = lower_in_file(db, def, of.lowering_with_source_map());
            assert_eq!(body, *of, "{def:?}: the body lowered with its source map");
            // alloc: the query's value, one per lowered body
            return (Arc::new(body), source_map);
        };
        let syntax = body_syntax(db, def).as_ref().expect("lowered without its syntax");
        let item = def_syntax(db, def).value;
        let start = item.text_range().start() - syntax.item.text_range().start();
        let anchor = item
            .ancestors()
            .find(|&it| {
                let range = it.text_range();
                it.kind() == syntax.kind
                    && range.start() == start
                    && range.len() == syntax.range.len()
                    && kept(it).0 == syntax.tree
            })
            .expect("the anchor is not around the item");
        debug_assert_eq!(Some(start), Self::anchor_offset(db, def));
        let mv = Move { file: syntax.file_id, to: MoveTo::Body { anchor, start } };
        let source_map = BodySourceMap {
            self_param: source_map.self_param.map(|it| mv.ptr(it)),
            store: source_map.store.moved(&mv),
        };
        if check_relative() {
            let lowering = Self::of(db, def).lowering_with_source_map();
            let (whole, in_file) = lower_in_file(db, def, lowering);
            assert_eq!(in_file, source_map, "{def:?}: the source map lowered alone, moved");
            assert_eq!(whole.params.len(), body.params.len());
        }
        // clone: an `Arc`, shared with `Body::relative`'s value
        (body.clone(), source_map)
    }

    /// The body lowered from [`body_syntax`] rather than from the whole file: what it is made of
    /// stays the same while edits elsewhere in the file move it, so it is lowered again only
    /// when its own text changes. The source map is of [`BodySyntax::tree`]. `None`
    /// when the body is not written in a file, but by a macro. Kept for more bodies than
    /// [`Body::with_source_map`] is: what is lowered again after it is let go of has changed
    /// to all that reads it. The body is [`Body::of`] lowered again for its map, with the same
    /// blocks, and checked equal to it.
    #[blaze_query::tracked(lru = 4096, returns(ref))]
    pub fn relative(
        db: &Db,
        def: DefWithBodyId,
    ) -> Option<(Arc<Body>, BodySourceMap)> {
        let _p = crate::body_span!(db, def, "body_relative_query").entered();
        body_syntax(db, def).as_ref()?;
        let of = Self::of(db, def);
        let (body, source_map) = lower_relative(db, def, of.lowering_with_source_map())?;
        assert_eq!(body, *of, "{def:?}: the body lowered with its source map");
        // alloc: the query's value, one per lowered body
        Some((Arc::new(body), source_map))
    }

    /// The body without its source map: lowered as [`Body::relative`] is, or from its file for
    /// a body a macro writes, recording no map. Most bodies a build lowers never show where a
    /// part of them is written, so their maps are built only when read: [`Body::relative`] and
    /// [`Body::with_source_map`] lower the body again, with the map and this body's blocks.
    /// The body is the memo's own value, lent by reference: no `Arc` and no count to write.
    #[blaze_query::tracked(returns(ref))]
    pub fn of(db: &Db, def: DefWithBodyId) -> Body {
        let _p = crate::body_span!(db, def, "body_query").entered();
        match lower_relative(db, def, BodyLowering::Body) {
            Some((body, _)) => body,
            None => lower_in_file(db, def, BodyLowering::Body).0,
        }
    }

    /// Whether lowering the body found an error: a diagnostic of its source map other than
    /// [`ExpressionStoreDiagnostics::InactiveCode`]. What rustc keeps a body out of borrow
    /// checking for, without the positions the map holds, so that a reader of it reads none.
    #[blaze_query::tracked(returns(copy))]
    pub fn has_lowering_errors(db: &Db, def: DefWithBodyId) -> bool {
        Self::unmoved_source_map(db, def)
            .diagnostics()
            .iter()
            .any(|it| !matches!(it, ExpressionStoreDiagnostics::InactiveCode { .. }))
    }
}

impl Body {
    /// How to lower this body again for its source map: with its blocks.
    fn lowering_with_source_map(&self) -> BodyLowering<'_> {
        BodyLowering::SourceMap { blocks: self.store.block_scopes() }
    }

    /// Where in its file the anchor of [`body_syntax`] begins, which the source map of
    /// [`Body::relative`] is relative to. `None` for a body a macro writes.
    pub fn anchor_offset(db: &Db, def: DefWithBodyId) -> Option<TextSize> {
        let syntax = body_syntax(db, def).as_ref()?;
        Some(def_syntax(db, def).value.text_range().start() - syntax.item.text_range().start())
    }

    /// The source map of [`Body::relative`] when there is one, and of
    /// [`Body::with_source_map`] when there is not: for what needs of it only the file or the
    /// expansion each part is written in, and the macro calls, and not where in the file.
    /// It stays the same while edits elsewhere in the file move the body.
    pub fn unmoved_source_map(db: &Db, def: DefWithBodyId) -> &BodySourceMap {
        Self::unmoved(db, def).1
    }

    /// [`Body::unmoved_source_map`] with the body it maps.
    pub fn unmoved(db: &Db, def: DefWithBodyId) -> (&Body, &BodySourceMap) {
        let (body, source_map) = match Self::relative(db, def) {
            Some(it) => it,
            None => Self::with_source_map(db, def),
        };
        (&**body, source_map)
    }
}

/// The syntax of the item `def` is the body of.
fn def_syntax(db: &Db, def: DefWithBodyId) -> InFile<view::Node<'_>> {
    match def {
        DefWithBodyId::FunctionId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
        DefWithBodyId::ConstId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
        DefWithBodyId::StaticId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
        DefWithBodyId::VariantId(it) => it.lookup(db).source_view(db).map(|it| it.syntax()),
    }
}

/// Lowers the body of `def` from [`body_syntax`], as `lowering` says. `None` when the body is
/// not written in a file, but by a macro.
fn lower_relative(
    db: &Db,
    def: DefWithBodyId,
    lowering: BodyLowering<'_>,
) -> Option<(Body, BodySourceMap)> {
    let syntax = body_syntax(db, def).as_ref()?;
    let tree = syntax.tree(db);
    let node = view::to_node(syntax.item, view::Node::root(&tree));
    let (mut params, mut is_async_fn, mut is_gen_fn) = (None, false, false);
    let body = match def {
        DefWithBodyId::FunctionId(_) => {
            let it = view::Fn::cast(node)?;
            params = it.param_list();
            is_async_fn = it.async_token().is_some();
            is_gen_fn = it.gen_token().is_some();
            it.body().map(view::Expr::from)
        }
        DefWithBodyId::ConstId(_) => view::Const::cast(node)?.body(),
        DefWithBodyId::StaticId(_) => view::Static::cast(node)?.body(),
        DefWithBodyId::VariantId(_) => view::Variant::cast(node)?.const_arg()?.expr(),
    };
    let maps = Some((&*syntax.span_map, &*syntax.ast_id_map));
    Some(lower_body(
        db,
        def,
        syntax.item,
        syntax.file_id,
        def.module(db),
        params,
        body,
        is_async_fn,
        is_gen_fn,
        maps,
        lowering,
    ))
}

/// Lowers the body of `def` from the whole file it is in, as `lowering` says. What a body
/// written by a macro is lowered with, and what [`check_relative`] checks the others against.
fn lower_in_file(
    db: &Db,
    def: DefWithBodyId,
    lowering: BodyLowering<'_>,
) -> (Body, BodySourceMap) {
    let mut params = None;
    let mut is_async_fn = false;
    let mut is_gen_fn = false;
    let (InFile { file_id, value: body }, syntax_node) = match def {
        DefWithBodyId::FunctionId(f) => {
            let src = f.lookup(db).source_view(db);
            params = src.value.param_list();
            is_async_fn = src.value.async_token().is_some();
            is_gen_fn = src.value.gen_token().is_some();
            let syntax_node = view::ptr(src.value.syntax());
            (src.map(|it| it.body().map(view::Expr::from)), syntax_node)
        }
        DefWithBodyId::ConstId(c) => {
            let src = c.lookup(db).source_view(db);
            let syntax_node = view::ptr(src.value.syntax());
            (src.map(|it| it.body()), syntax_node)
        }
        DefWithBodyId::StaticId(s) => {
            let src = s.lookup(db).source_view(db);
            let syntax_node = view::ptr(src.value.syntax());
            (src.map(|it| it.body()), syntax_node)
        }
        DefWithBodyId::VariantId(v) => {
            let src = v.lookup(db).source_view(db);
            let syntax_node = view::ptr(src.value.syntax());
            (src.map(|it| it.const_arg()?.expr()), syntax_node)
        }
    };
    let module = def.module(db);
    lower_body(
        db,
        def,
        syntax_node,
        file_id,
        module,
        params,
        body,
        is_async_fn,
        is_gen_fn,
        None,
        lowering,
    )
}

/// Whether [`Body::with_source_map`] checks the source map of each body lowered alone, moved,
/// against that of the same body lowered in its file, and [`BodySyntax::tree`] parses every
/// anchor again and prints the ones that parse differently alone (`RA_CHECK_RELATIVE_BODIES`).
pub(crate) fn check_relative() -> bool {
    static CHECK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CHECK.get_or_init(|| std::env::var_os("RA_CHECK_RELATIVE_BODIES").is_some())
}

/// What lowering a body reads of the file it is written in, as a tree of its own: the anchor
/// the body is written in ([`RealSpanMap::anchor_at`]), which is the item itself unless that
/// is an item in a body, or the item of an `impl` or a module with attributes. Its ranges begin
/// at the anchor, and so stay as they are while edits elsewhere move it in the file. A body
/// lowered from this is lowered again only when this changes, not on every edit of the file.
///
/// It holds no tree, so that a file's tree is let go of once nothing is parsing or lowering
/// it: the tree of [`BodySyntax::tree`] is parsed again from the text of the anchor when it
/// is not among the last few made. It is the same as the one in the file, which it is
/// compared by ([`tree_hash`]).
pub struct BodySyntax {
    pub file_id: HirFileId,
    tree: u128,
    /// The text of the file when this was made, with where the anchor is in it, and what kind
    /// of node it is. Only what is there is read: where it is changes without this changing.
    text: syntax::Text,
    range: TextRange,
    kind: SyntaxKind,
    edition: Edition,
    /// Whether the anchor holds a lazy node of an outline (#1294), and so is parsed again as an
    /// outline: a full parse of its text is a different tree.
    outline: bool,
    /// The item in [`BodySyntax::tree`].
    pub item: SyntaxNodePtr,
    /// The maps of the anchor, the same for every body written in it, and so shared by them.
    pub ast_id_map: Arc<AstIdMap>,
    pub span_map: Arc<RealSpanMap>,
}

/// What every body written in one anchor has the same of: made for the first of them and shared
/// by the rest, so that an edit of a file walks an `impl` of many methods once, not once a method.
#[derive(Clone)]
struct AnchorMaps {
    ast_id_map: Arc<AstIdMap>,
    span_map: Arc<RealSpanMap>,
    outline: bool,
}

impl PartialEq for BodySyntax {
    fn eq(&self, other: &Self) -> bool {
        self.file_id == other.file_id
            && self.tree == other.tree
            && self.item == other.item
            && self.ast_id_map == other.ast_id_map
            && self.span_map == other.span_map
    }
}

impl Eq for BodySyntax {}

impl std::fmt::Debug for BodySyntax {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BodySyntax")
            .field("file_id", &self.file_id)
            .field("range", &self.range)
            .field("item", &self.item)
            .finish_non_exhaustive()
    }
}

impl BodySyntax {
    /// The anchor as a tree of its own: the one kept on this thread, else parsed again. Its
    /// root is [`view::Node::root`] of it.
    pub fn tree(&self, db: &Db) -> std::sync::Arc<FlatTree> {
        let kept = trees::get(self.tree).filter(|_| !check_relative());
        kept.unwrap_or_else(|| {
            let root = self.reparse().unwrap_or_else(|| self.from_file(db));
            // clone: a count of the tree for this thread's tree cache
            trees::put(self.tree, root.clone(), None);
            root
        })
    }

    /// The text of the anchor.
    pub fn text(&self) -> &str {
        &self.text[self.range]
    }

    /// The anchor parsed alone, when that makes the tree it is in the file: as an outline when
    /// it is one, its bodies left to be parsed when they are entered.
    fn reparse(&self) -> Option<std::sync::Arc<FlatTree>> {
        let parse = match self.outline {
            true => syntax::ast::SourceFile::parse_outline(self.text(), self.edition),
            false => syntax::ast::SourceFile::parse(self.text(), self.edition),
        };
        let file = parse.root();
        let root = match self.kind == SyntaxKind::SOURCE_FILE {
            // clone: a count of the parse's tree, kept past the parse
            true => file.shared_file().clone(),
            false => {
                let range = TextRange::up_to(self.range.len());
                let mut items = file.children();
                let item = items.next().filter(|_| items.next().is_none())?;
                (item.kind() == self.kind && item.text_range() == range)
                    // alloc: the anchor's tree of its own, once per parse
                    .then(|| std::sync::Arc::new(item.subtree()))?
            }
        };
        (subtree_hash(&root, 0) == self.tree).then_some(root)
    }

    /// Whether the anchor parsed alone is the tree it is in the file, so that
    /// [`BodySyntax::tree`] needs nothing of the file's tree.
    #[cfg(test)]
    pub(crate) fn reparses_alone(&self) -> bool {
        self.reparse().is_some()
    }

    /// The anchor from the tree of its file, for one that parses differently alone.
    fn from_file(&self, db: &Db) -> std::sync::Arc<FlatTree> {
        if check_relative() {
            eprintln!(
                "{:?}: the anchor at {:?} parses differently alone",
                self.file_id, self.range
            );
        }
        let parse = self.file_id.file_id().expect("a body written in a file").parse(db);
        // Down the nodes around the anchor only: a walk of every node would parse every body
        // of an outline.
        std::iter::successors(Some(parse.root()), |node| {
            node.children().find(|it| it.text_range().contains_range(self.range))
        })
        .filter(|it| it.text_range() == self.range && it.kind() == self.kind)
        .map(kept)
        .find(|it| it.0 == self.tree)
        .expect("the anchor is not in its file")
        .1
    }
}

/// Whether the subtree of `node` holds a lazy node of an outline, parsed or not.
fn holds_lazy(node: view::Node<'_>) -> bool {
    let (tree, at) = node.flat();
    let lazies = tree.lazy_nodes();
    lazies.get(lazies.partition_point(|&it| it < at)).is_some_and(|&it| it < tree.end(at))
}

/// What the subtree of `node` is made of, as the root of a tree of its own, so that two trees
/// made of the same are the same, read in place. Every body of a file runs this on each edit
/// of the file, over its whole anchor (an `impl` or module), so a large anchor's hash is the
/// one its tree keeps, carried across an edit that doesn't change it.
fn tree_hash(node: view::Node<'_>) -> u128 {
    let (tree, at) = node.flat();
    subtree_hash(tree, at)
}

/// [`syntax::FlatTree::subtree_hash`] of `at`: what a body's or a signature's tree is compared
/// by.
pub(crate) fn subtree_hash(tree: &syntax::FlatTree, at: u32) -> u128 {
    tree.subtree_hash(at)
}

/// `anchor` as the root of a tree of its own, with its [`tree_hash`]: the one kept among the
/// trees of this thread when that is of this anchor or made of the same, else a copy, kept.
/// The bodies of an anchor are made one after another, and hashing or copying it again for
/// each is long when it is a whole `impl` or module.
fn kept(anchor: view::Node<'_>) -> (u128, std::sync::Arc<FlatTree>) {
    if let Some(found) = trees::of_anchor(anchor) {
        return found;
    }
    let tree = tree_hash(anchor);
    let root = trees::get(tree).unwrap_or_else(|| match anchor.parent() {
        // The file's root is its whole tree: shared, not copied.
        // clone: a count of the file's tree
        None => anchor.shared_file().clone(),
        // alloc: the anchor's tree of its own, once per anchor kept
        Some(_) => std::sync::Arc::new(anchor.subtree()),
    });
    // clone: a count of the tree for this thread's tree cache
    trees::put(tree, root.clone(), Some(anchor));
    (tree, root)
}

/// The trees of the anchors last parsed or lowered on this thread: what a body is lowered from
/// right after its [`BodySyntax`] is made, and where its places are found while it is compiled.
mod trees {
    use std::{
        cell::RefCell,
        collections::VecDeque,
        sync::{Arc, Weak},
    };

    use hir_expand::HirFileId;
    use syntax::{FlatTree, view};

    use super::AnchorMaps;

    const KEPT: usize = 32;
    /// The bodies of an anchor are made one after another, so a few anchors' maps are enough.
    const KEPT_MAPS: usize = 8;

    /// A tree's hash, the tree, and the anchor in a file's tree it was made of: that file's
    /// tree, whose allocation the `Weak` keeps so that its address names it, and the anchor's
    /// place in it.
    type Kept = (u128, Arc<FlatTree>, Option<(Weak<FlatTree>, view::NodeId)>);

    /// The maps of an anchor of a file: the anchor as in [`Kept`], and the file it is of.
    type KeptMaps = (Weak<FlatTree>, view::NodeId, HirFileId, AnchorMaps);

    thread_local! {
        static TREES: RefCell<VecDeque<Kept>> = const { RefCell::new(VecDeque::new()) };
        static MAPS: RefCell<VecDeque<KeptMaps>> = const { RefCell::new(VecDeque::new()) };
    }

    /// The maps of `anchor`, written in `file_id`: the ones kept when they are of this anchor of
    /// this tree, else those `make` makes, kept. What they are made of is the anchor's tree and
    /// the maps of the file that tree is the parse of, so the same anchor has the same maps.
    pub(super) fn maps(
        anchor: view::Node<'_>,
        file_id: HirFileId,
        make: impl FnOnce() -> AnchorMaps,
    ) -> AnchorMaps {
        let (file, at) = (anchor.shared_file(), anchor.id());
        let of = |it: &&KeptMaps| {
            it.1 == at && it.2 == file_id && std::ptr::eq(it.0.as_ptr(), Arc::as_ptr(file))
        };
        // clone: `Arc`s of an anchor's maps, shared by its bodies
        if let Some(found) = MAPS.with_borrow(|maps| maps.iter().find(of).map(|it| it.3.clone())) {
            return found;
        }
        let made = make();
        // clone: `Arc`s of an anchor's maps, shared by its bodies
        let kept = (Arc::downgrade(file), at, file_id, made.clone());
        MAPS.with_borrow_mut(|maps| {
            if maps.len() == KEPT_MAPS {
                maps.pop_front();
            }
            maps.push_back(kept);
        });
        made
    }

    pub(super) fn get(tree: u128) -> Option<Arc<FlatTree>> {
        // clone: a count of a tree from this thread's tree cache
        TREES.with_borrow(|trees| trees.iter().find(|it| it.0 == tree).map(|it| it.1.clone()))
    }

    /// The tree kept of `anchor`, when it is one of these.
    pub(super) fn of_anchor(anchor: view::Node<'_>) -> Option<(u128, Arc<FlatTree>)> {
        let (file, at) = (anchor.shared_file(), anchor.id());
        TREES.with_borrow(|trees| {
            let of = |it: &&Kept| {
                it.2.as_ref().is_some_and(|(of, of_at)| {
                    *of_at == at && std::ptr::eq(of.as_ptr(), Arc::as_ptr(file))
                })
            };
            // clone: a count of a tree from this thread's tree cache
            trees.iter().find(of).map(|it| (it.0, it.1.clone()))
        })
    }

    pub(super) fn put(tree: u128, root: Arc<FlatTree>, anchor: Option<view::Node<'_>>) {
        let anchor = anchor.map(|it| (Arc::downgrade(it.shared_file()), it.id()));
        TREES.with_borrow_mut(|trees| {
            trees.retain(|it| it.0 != tree);
            if trees.len() == KEPT {
                trees.pop_front();
            }
            trees.push_back((tree, root, anchor));
        })
    }
}

/// The [`BodySyntax`] of a body written in a file, `None` for one a macro writes.
///
/// A body outside fn bodies reads only its own [`ItemBodySyntax`], which an edit of the file
/// rewrites in place and leaves as it was when the edit left its anchor alone, so an edit of one
/// body reaches no reader of another. A body in a fn body, or one a macro writes, has no ast id
/// that holds without a walk of the bodies, and is placed by [`placed_body_syntax`].
pub fn body_syntax(db: &Db, def: DefWithBodyId) -> &Option<BodySyntax> {
    let InFile { file_id, value: ast_id } = def.erased_ast_id(db);
    let Some(file) = file_id.file_id() else { return placed_body_syntax(db, def) };
    if ast_id.in_body() {
        return placed_body_syntax(db, def);
    }
    let Some(&id) = file_body_syntax(db, file).get(&ast_id) else {
        panic!("{def:?}: {ast_id:?} is outside fn bodies but no item of {file:?} with a body has it")
    };
    <ItemBodySyntax<'_> as blaze_query::db::FromId>::from_id(id).syntax(db)
}

/// The [`BodySyntax`] of one item of a file with a body outside fn bodies, set by
/// [`file_body_syntax`] at each edit of the file.
#[blaze_query::tracked]
struct ItemBodySyntax<'db> {
    ast_id: ErasedFileAstId,
    #[tracked]
    #[returns(ref)]
    syntax: Option<BodySyntax>,
}

/// The [`ItemBodySyntax`] of each fn, const, static and variant of `file` outside fn bodies, by
/// its ast id: changed only when such an item comes or goes, not by where an item is.
#[blaze_query::tracked(returns(ref))]
fn file_body_syntax(db: &Db, file: EditionedFileId) -> HashMap<ErasedFileAstId, blaze_query::db::Id> {
    let file_id = HirFileId::from(file);
    let SpanMap::RealSpanMap(span_map) = file_id.span_map(db) else {
        unreachable!("{file:?} is a real file")
    };
    let ast_id_map = file_id.ast_id_map(db);
    let root = file_id.root(db);
    let mut items = HashMap::default();
    for &[start, end, kind, id] in ast_id_map.entries() {
        let kind = SyntaxKind::from(kind as u16);
        if !matches!(kind, SyntaxKind::FN | SyntaxKind::CONST | SyntaxKind::STATIC | SyntaxKind::VARIANT) {
            continue;
        }
        let ast_id = ErasedFileAstId::from_raw(id);
        let ptr = SyntaxNodePtr::from_parts(kind, TextRange::new(start.into(), end.into()));
        let syntax = anchored_body_syntax(db, file_id, span_map, ast_id_map, view::to_node(ptr, root));
        let item = ItemBodySyntax::new(db, ast_id, syntax);
        items.insert(ast_id, blaze_query::db::AsId::as_id(&item));
    }
    items
}

/// The [`BodySyntax`] of `def` found by where it is in its file: what [`body_syntax`] reads for
/// a body in a fn body or one a macro writes. An item of a block lowered from the syntax of the
/// body it is in is in that body's anchor, and takes its tree and maps from it, so an edit
/// elsewhere in the file leaves it the same.
#[blaze_query::tracked(returns(ref))]
fn placed_body_syntax(db: &Db, def: DefWithBodyId) -> Option<BodySyntax> {
    if let Some(syntax) = in_owner_body_syntax(db, def) {
        return Some(syntax);
    }
    let src = def_syntax(db, def);
    let SpanMap::RealSpanMap(span_map) = src.file_id.span_map(db) else { return None };
    anchored_body_syntax(db, src.file_id, span_map, src.file_id.ast_id_map(db), src.value)
}

/// The [`BodySyntax`] of `def`, an item in a body, off the [`BodySyntax`] of the body whose own
/// syntax made its block (`BlockIdLt::owner`). `None` for an item outside fn bodies, of a block
/// lowered from its file, or written by a macro.
fn in_owner_body_syntax(db: &Db, def: DefWithBodyId) -> Option<BodySyntax> {
    let InFile { file_id, value: ast_id } = def.erased_ast_id(db);
    if !ast_id.in_body() {
        return None;
    }
    let owner = def.module(db).block(db)?.owner(db)?;
    let owner = body_syntax(db, owner).as_ref().filter(|it| it.file_id == file_id)?;
    let item = owner.ast_id_map.try_get_erased(ast_id)?;
    Some(BodySyntax {
        file_id,
        tree: owner.tree,
        // clone: a count of the file's text
        text: owner.text.clone(),
        range: owner.range,
        kind: owner.kind,
        edition: owner.edition,
        outline: owner.outline,
        item,
        // clone: a count of the anchor's maps
        ast_id_map: owner.ast_id_map.clone(),
        // clone: a count of the anchor's maps
        span_map: owner.span_map.clone(),
    })
}

/// The [`BodySyntax`] of `item`, a node of the real file `file_id`.
fn anchored_body_syntax(
    db: &Db,
    file_id: HirFileId,
    span_map: &RealSpanMap,
    ast_id_map: &AstIdMap,
    item: view::Node<'_>,
) -> Option<BodySyntax> {
    let (start, anchor) = span_map.anchor_at(item.text_range().start());
    let anchor = match anchor == ROOT_ERASED_FILE_AST_ID {
        true => item.ancestors().last()?,
        false => {
            let ptr = ast_id_map.get_erased(anchor);
            item.ancestors().find(|&it| view::ptr(it) == ptr)?
        }
    };
    let (tree, kept_tree) = kept(anchor);
    let root = view::Node::root(&kept_tree);
    let range = item.text_range() - start;
    let relative = match root.covering_element(range) {
        NodeOrToken::Node(it) => it,
        NodeOrToken::Token(it) => it.parent()?,
    };
    let relative =
        relative.ancestors().find(|it| it.text_range() == range && it.kind() == item.kind())?;
    let (file, edition) = file_id.file_id()?.unpack(db);
    let AnchorMaps { ast_id_map, span_map, outline } = trees::maps(anchor, file_id, || {
        AnchorMaps {
            // alloc: one per anchor, shared by its bodies
            ast_id_map: Arc::new(ast_id_map.relative_to_view(anchor, root)),
            // alloc: one per anchor, shared by its bodies
            span_map: Arc::new(span_map.within(anchor.text_range())),
            outline: holds_lazy(anchor),
        }
    });
    Some(BodySyntax {
        file_id,
        tree,
        // clone: a count of the file's text
        text: db.file_text(file).text(db).clone(),
        range: anchor.text_range(),
        kind: anchor.kind(),
        edition,
        outline,
        item: view::ptr(relative),
        ast_id_map,
        span_map,
    })
}

impl Body {
    pub fn root_expr(&self) -> ExprId {
        // A `Body` can also contain root expressions that aren't the body (in the param patterns),
        // but the body always come last.
        self.store.expr_roots().next_back().unwrap()
    }

    /// Returns `true` if this is the formal or user-written self param.
    #[inline]
    pub fn is_any_self_param(&self, binding: BindingId) -> bool {
        self.self_param.is_some_and(|self_param| {
            self_param.formal == binding || self_param.user_written == binding
        })
    }

    pub fn pretty_print(
        &self,
        db: &Db,
        owner: DefWithBodyId,
        edition: Edition,
    ) -> String {
        pretty::print_body_hir(db, self, owner, edition)
    }

    pub fn pretty_print_expr(
        &self,
        db: &Db,
        owner: DefWithBodyId,
        expr: ExprId,
        edition: Edition,
    ) -> String {
        pretty::print_expr_hir(db, self, owner.into(), expr, edition)
    }

    pub fn pretty_print_pat(
        &self,
        db: &Db,
        owner: ExpressionStoreOwnerId,
        pat: PatId,
        oneline: bool,
        edition: Edition,
    ) -> String {
        pretty::print_pat_hir(db, self, owner, pat, oneline, edition)
    }
}

impl BodySourceMap {
    pub fn self_param_syntax(&self) -> Option<InFile<SelfParamPtr>> {
        self.self_param
    }
}

#[cfg(test)]
mod tests;
