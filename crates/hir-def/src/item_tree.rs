//! A simplified AST that only contains items.
//!
//! This is the primary IR used throughout `hir_def`. It is the input to the name resolution
//! algorithm, as well as to the queries defined in `adt.rs`, `data.rs`, and most things in
//! `attr.rs`.
//!
//! `ItemTree`s are built per `HirFileId`, from the syntax tree of the parsed file. This means that
//! they are crate-independent: they don't know which `#[cfg]`s are active or which module they
//! belong to, since those concepts don't exist at this level (a single `ItemTree` might be part of
//! multiple crates, or might be included into the same crate twice via `#[path]`).
//!
//! One important purpose of this layer is to provide an "invalidation barrier" for incremental
//! computations: when typing inside an item body, the `ItemTree` of the modified file is typically
//! unaffected, so we don't have to recompute name resolution results or item data (see `data.rs`).
//!
//! The `ItemTree` for the currently open file can be displayed by using the VS Code command
//! "rust-analyzer: Debug ItemTree".
//!
//! Compared to rustc's architecture, `ItemTree` has properties from both rustc's AST and HIR: many
//! syntax-level Rust features are already desugared to simpler forms in the `ItemTree`, but name
//! resolution has not yet been performed. `ItemTree`s are per-file, while rustc's AST and HIR are
//! per-crate, because we are interested in incrementally computing it.
//!
//! The representation of items in the `ItemTree` should generally mirror the surface syntax: it is
//! usually a bad idea to desugar a syntax-level construct to something that is structurally
//! different here. Name resolution needs to be able to process attributes and expand macros
//! (including attribute macros), and having a 1-to-1 mapping between syntax and the `ItemTree`
//! avoids introducing subtle bugs.
//!
//! In general, any item in the `ItemTree` stores its `AstId`, which allows mapping it back to its
//! surface syntax.
//!
//! # Layout (docs/v2/plan/design/data.md)
//!
//! The tree is columns. An item is a [`ModItemId`], a dense index into three per-item columns:
//! its [`ItemKind`] (`u8`), its data word (`u32`: its index in its kind's payload column) and its
//! `FileAstId` (`u32`), [`ITEM_BYTES`] bytes an item. Each kind's payload is a flat array of
//! plain records of `u32`s ([`Function`], [`Struct`], ...), which [`ItemTree::index`] hands out
//! by reference. Lists (an inline module's items, an extern block's, a `use` tree's children, a
//! tuple struct's constructor visibilities) are [`IdRange`]s into one array per element type,
//! and every list's items come before the item holding it. Names and paths are `u32` ids into
//! the tree's tables ([`NameId`], [`PathId`]); visibilities ([`RawVisibilityId`]) and attributes
//! (keyed by item, in [`ItemTree::attrs`]) sit in side tables. [`persist`] writes the columns as
//! they are. Names, paths and visibilities are deduplicated within each top-level item, not
//! across them, so a source file's tree is its items' rows one after the other: an edit keeps
//! the rows of the items it did not change and lowers the others alone ([`items`]). All the
//! columns are one allocation: a header of where each ends, then the columns, placed by offsets
//! (`block.rs`).

mod attrs;
// The raw allocator of a tree's columns: one allocation a tree, not one a column.
#[allow(unsafe_code)]
mod block;
mod items;
mod lower;
pub mod persist;
mod pretty;
#[cfg(test)]
mod tests;

use std::{
    fmt,
    mem::size_of,
    ops::{Index, Range},
    sync::Arc,
};

use base_db::{Db, Crate};
use cfg::CfgExpr;
use hir_expand::{
    ExpandTo, HirFileId,
    attrs::Attr,
    mod_path::{ModPath, PathKind},
    name::Name,
};
use la_arena::{Idx, RawIdx};
use span::{AstIdNode, Edition, FileAstId, SyntaxContext};
use syntax::{
    ast, match_ast,
    view::{self, AstNode as _},
};

use crate::{BlockId, expr_store::body::body_syntax};

pub(crate) use crate::item_tree::{
    attrs::*,
    items::deferred_item_shape,
    lower::{lower_use_tree, visibility_from_ast},
};

blaze_format::record! {
    /// A visibility of an [`ItemTree`]: one of the four every tree has, or an index into its
    /// table of them.
    #[derive(Copy, Clone, Eq, PartialEq)]
    pub(crate) struct RawVisibilityId: 1 {
        raw: u32,
    }
}

impl RawVisibilityId {
    const PUB: Self = RawVisibilityId { raw: u32::MAX };
    const PRIV_IMPLICIT: Self = RawVisibilityId { raw: u32::MAX - 1 };
    const PRIV_EXPLICIT: Self = RawVisibilityId { raw: u32::MAX - 2 };
    const PUB_CRATE: Self = RawVisibilityId { raw: u32::MAX - 3 };

    /// Whether this is one of the visibilities every tree has, not one of its table.
    fn is_builtin(self) -> bool {
        self.raw >= Self::PUB_CRATE.raw
    }
}

impl fmt::Debug for RawVisibilityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_tuple("RawVisibilityId");
        match *self {
            Self::PUB => f.field(&"pub"),
            Self::PRIV_IMPLICIT | Self::PRIV_EXPLICIT => f.field(&"pub(self)"),
            Self::PUB_CRATE => f.field(&"pub(crate)"),
            _ => f.field(&self.raw),
        };
        f.finish()
    }
}

blaze_format::record! {
    /// A name of an [`ItemTree`]: an index into its table of names, `&tree[id]` the [`Name`].
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct NameId: 1 {
        raw: u32,
    }
}

impl NameId {
    /// No name: the name of `const _`.
    pub const NONE: NameId = NameId { raw: u32::MAX };
}

blaze_format::record! {
    /// A path of an [`ItemTree`]: an index into its table of paths, `&tree[id]` the path.
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct PathId: 1 {
        raw: u32,
    }
}

impl PathId {
    /// No path: the prefix of `use {a, b}` or the path of `use *` in a nested tree.
    pub const NONE: PathId = PathId { raw: u32::MAX };
}

blaze_format::record! {
    /// An [`ImportAlias`] of an [`ItemTree`], or none: [`ItemTree::import_alias`] reads it.
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct AliasId: 1 {
        raw: u32,
    }
}

impl AliasId {
    /// No `as`.
    pub const NONE: AliasId = AliasId { raw: u32::MAX };
    /// `as _`.
    pub const UNDERSCORE: AliasId = AliasId { raw: u32::MAX - 1 };
}

blaze_format::record! {
    /// A `use` tree of an [`ItemTree`]: an index into its array of them.
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct UseTreeId: 1 {
        raw: u32,
    }
}

blaze_format::record! {
    /// A list: `len` elements of one of the tree's arrays from `start`.
    #[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
    pub struct IdRange: 1 {
        pub start: u32,
        pub len: u32,
    }
}

impl IdRange {
    pub const EMPTY: IdRange = IdRange { start: 0, len: 0 };
    /// Not a list: the items of a `mod m;`.
    const NONE: IdRange = IdRange { start: u32::MAX, len: 0 };

    #[inline]
    fn range(self) -> Range<usize> {
        self.start as usize..self.start as usize + self.len as usize
    }

    /// Whether the list is within an array of `len` elements.
    fn within(self, len: usize) -> bool {
        (self.start as usize).checked_add(self.len as usize).is_some_and(|end| end <= len)
    }
}

blaze_format::record! {
    /// The attributes of one item: `attrs` of the tree's attributes and, when one of them is a
    /// `cfg` that is false, that `cfg` (an index into the tree's `cfgs`; the attributes are then
    /// those before it).
    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    pub(crate) struct AttrOwner: 1 {
        pub(crate) owner: ModItemId,
        pub(crate) attrs: IdRange,
        pub(crate) cfg: u32,
    }
}

impl AttrOwner {
    /// No `cfg` disables the item.
    pub(crate) const ENABLED: u32 = u32::MAX;
}

/// Computes an [`ItemTree`] for the given file or macro expansion.
pub fn file_item_tree(db: &Db, file_id: HirFileId, krate: Crate) -> &ItemTree {
    file_item_tree_query(db, file_id, krate)
}

/// A source file's tree is made from its last one where it was edited ([`items`]), so an edit
/// lowers only the items it touches, and one that touches none returns the last tree itself;
/// a macro file's is lowered whole, as each expansion is made anew. The tree is behind an
/// `Arc` so the last one is kept beside the query database without a copy.
#[blaze_query::tracked(returns(deref))]
fn file_item_tree_query(
    db: &Db,
    file_id: HirFileId,
    krate: Crate,
) -> Arc<ItemTree> {
    let _p = crate::span_names::of_file(
        db,
        file_id,
        krate,
        tracing::info_span!(
            "file_item_tree_query",
            krate = tracing::field::Empty,
            function = tracing::field::Empty
        ),
    )
    .entered();
    match file_id {
        HirFileId::FileId(file) => items::file_item_tree(db, file, krate),
        // alloc: the tree's handle, once an expansion's tree
        HirFileId::MacroFile(_) => Arc::new(lower_whole(db, file_id, krate)),
    }
}

/// The tree of `file_id` lowered in one pass. A source file's assembled from its items is this
/// tree, to the byte.
pub(crate) fn lower_whole(db: &Db, file_id: HirFileId, krate: Crate) -> ItemTree {
    let mut ctx = lower::Ctx::new(db, file_id, krate);
    let syntax = file_id.root(db);
    match_ast! {
        match syntax {
            view::SourceFile(file) => {
                let crate_attrs = match file_id == krate.root_file_id(db) {
                    true => base_db::applied_crate_attr_files(db, krate),
                    false => &[],
                };
                ctx.lower_top_attrs(file, crate_attrs);
                ctx.lower_module_items(file)
            },
            view::MacroItems(items) => {
                ctx.lower_module_items(items)
            },
            view::MacroStmts(stmts) => {
                // The produced statements can include items, which should be added as top-level
                // items.
                ctx.lower_macro_stmts(stmts)
            },
            _ => {
                panic!("cannot create item tree for file {file_id:?} from {syntax:?} {syntax}");
            },
        }
    }
}

#[blaze_query::tracked(returns(ref))]
pub(crate) fn block_item_tree_query(
    db: &Db,
    block: BlockId,
    krate: Crate,
) -> ItemTree {
    let _p = tracing::info_span!("block_item_tree_query", ?block).entered();
    let ast_id = block.ast_id(db);
    // A block of a body written in a file is lowered from that body's syntax alone, which an
    // edit elsewhere in the file leaves as it is; any other (a signature's, a macro's body's,
    // one a macro call in the body expands to, whose id is of the expansion) from its file.
    let syntax = block
        .owner(db)
        .and_then(|owner| body_syntax(db, owner).as_ref())
        .filter(|syntax| syntax.file_id == ast_id.file_id);
    let Some(syntax) = syntax else {
        return lower::Ctx::new(db, ast_id.file_id, krate).lower_block(ast_id.to_view(db));
    };
    let tree = syntax.tree(db);
    let ptr = syntax.ast_id_map.try_get_erased(ast_id.value.erase()).unwrap_or_else(|| {
        panic!(
            "{block:?}: block {:?} of {:?} is not in its body's syntax, which made it",
            ast_id.value,
            block.owner(db),
        )
    });
    let node = view::to_node(ptr, view::Node::root(&tree));
    let view = view::BlockExpr::cast(node).expect("a block's ast id names a block");
    let ctx = lower::Ctx::with_maps(db, ast_id.file_id, krate, &syntax.ast_id_map, &syntax.span_map);
    let lowered = ctx.lower_block(view);
    if crate::expr_store::body::check_relative() {
        let whole = lower::Ctx::new(db, ast_id.file_id, krate).lower_block(ast_id.to_view(db));
        assert!(lowered == whole, "{block:?}: the block's item tree lowered from its body alone");
    }
    lowered
}

block::columns! {
    /// The item tree of a source file, as columns (the module's docs), all in one block: a query
    /// memo has no lifetime group of its own, so it is one allocation of the global allocator,
    /// made once a tree (none for a tree with no values).
    pub struct ItemTree(ItemTreeColumn, ItemTreeLens, ItemTreeSinks) {
        top_level: ModItemId,
        /// Per item: its kind, which payload column `data` indexes.
        kinds: ItemKind,
        /// Per item: its index in its kind's payload column.
        data: u32,
        /// Per item.
        ast_ids: FileAstId<ast::Item>,
        /// The items of inline modules and extern blocks, each list an [`IdRange`] of this.
        children: ModItemId,

        consts: Const,
        enums: Enum,
        extern_blocks: ExternBlock,
        extern_crates: ExternCrate,
        functions: Function,
        impls: Impl,
        macro2s: Macro2,
        macro_calls: MacroCall,
        macro_rules: MacroRules,
        mods: Mod,
        statics: Static,
        structs: Struct,
        traits: Trait,
        type_aliases: TypeAlias,
        unions: Union,
        uses: Use,

        /// Every `use` tree of the tree's `use` items, each child before its parent.
        use_trees: UseTree,
        /// The children of prefixed `use` trees, each list an [`IdRange`] of this.
        use_tree_lists: UseTreeId,
        /// The visibilities a struct's value-namespace constructor is the least of, each
        /// struct's an [`IdRange`] of this.
        ctor_vis: RawVisibilityId,

        names: Name,
        paths: ModPath,
        vis: RawVisibility,

        /// Sorted by owner. The last, if its owner is [`ModItemId::TOP_OWNER`], is the top
        /// level's.
        attr_owners: AttrOwner,
        /// Which items have an owner in `attr_owners`, and where: made from it ([`attr_blocks`]),
        /// so not written ([`persist`]).
        attr_index: AttrBlock,
        /// Every item's attributes, each item's an [`IdRange`] of this.
        attr_list: Attr,
        /// The `cfg`s that disable items.
        cfgs: CfgExpr,
    }
}

const _: () = assert!(size_of::<ItemTree>() == size_of::<usize>());

/// Bytes per item in the per-item columns (`kinds`, `data`, `ast_ids`); its kind's payload
/// record (at most 16 bytes, [`Struct`] and [`Mod`]) is in its own column.
pub const ITEM_BYTES: usize =
    size_of::<ItemKind>() + size_of::<u32>() + size_of::<FileAstId<ast::Item>>();
const _: () = assert!(ITEM_BYTES == 9);

/// 64 items: which of them have attributes (bit `i % 64` of item `i`), and how many items
/// before them do, so an item's [`AttrOwner`] is found with a `popcnt`. 16 bytes per 64 items.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
struct AttrBlock {
    bits: u64,
    before: u32,
}

const _: () = assert!(size_of::<AttrBlock>() == 16);

/// The [`AttrBlock`]s of a tree of `items` items whose attribute owners are `owners`, sorted
/// and in the tree (what `persist`'s `check` holds a decoded tree to), in order: one pass, so
/// they are written straight into the tree's block. Owners out of that order or past `items`
/// (a buffer `check` refuses) make blocks that are wrong, never a panic.
fn attr_blocks(owners: &[AttrOwner], items: usize) -> impl ExactSizeIterator<Item = AttrBlock> {
    let mut owners = (owners.iter())
        .filter(|owner| owner.owner != ModItemId::TOP_OWNER)
        .map(|owner| owner.owner.index as usize)
        .peekable();
    let mut before = 0;
    (0..items.div_ceil(64)).map(move |block| {
        let mut bits = 0u64;
        while let Some(&at) = owners.peek()
            && at / 64 == block
        {
            bits |= 1 << (at % 64);
            owners.next();
        }
        let it = AttrBlock { bits, before };
        before += bits.count_ones();
        it
    })
}

impl ItemTree {
    /// Returns an iterator over all items located at the top level of the `HirFileId` this
    /// `ItemTree` was created from.
    pub(crate) fn top_level_items(&self) -> &[ModItemId] {
        self.top_level()
    }

    /// Returns the inner attributes of the source file.
    #[inline]
    pub(crate) fn top_level_attrs(&self) -> Option<AttrsOrCfgRef<'_>> {
        self.attr_owners()
            .last()
            .filter(|owner| owner.owner == ModItemId::TOP_OWNER)
            .map(|owner| self.attrs_of(owner))
    }

    /// Whether the file's own `#![cfg(...)]` is false. A crate whose root it is is empty, as
    /// rustc leaves one: none of its items and none of its crate attributes.
    pub fn configured_out(&self) -> bool {
        matches!(self.top_level_attrs(), Some(AttrsOrCfgRef::CfgDisabled(..)))
    }

    #[inline]
    pub(crate) fn attrs(&self, of: ModItemId) -> Option<AttrsOrCfgRef<'_>> {
        let block = self.attr_index().get(of.index as usize / 64)?;
        let bit = 1u64 << (of.index % 64);
        if block.bits & bit == 0 {
            return None;
        }
        let at = block.before + (block.bits & (bit - 1)).count_ones();
        Some(self.attrs_of(&self.attr_owners()[at as usize]))
    }

    #[inline]
    fn attrs_of(&self, owner: &AttrOwner) -> AttrsOrCfgRef<'_> {
        let attrs = Attrs::new(&self.attr_list()[owner.attrs.range()]);
        match owner.cfg {
            AttrOwner::ENABLED => AttrsOrCfgRef::Enabled { attrs },
            cfg => AttrsOrCfgRef::CfgDisabled(&self.cfgs()[cfg as usize], attrs),
        }
    }

    /// The items of an inline module's or an extern block's list.
    #[inline]
    pub(crate) fn items(&self, list: IdRange) -> &[ModItemId] {
        &self.children()[list.range()]
    }

    /// Whether `m` is inline, and its items if so.
    #[inline]
    pub(crate) fn mod_kind(&self, m: &Mod) -> ModKind<'_> {
        match m.items {
            IdRange::NONE => ModKind::Outline,
            items => ModKind::Inline { items: self.items(items) },
        }
    }

    /// The visibility of a struct's constructor in the value namespace, if it has one.
    #[inline]
    pub(crate) fn value_ns_ctor(&self, s: &Struct) -> StructValueNsCtor<'_> {
        let all = &self.ctor_vis()[s.value_ns_ctor.range()];
        match all {
            [] => StructValueNsCtor::NoValueNsCtor,
            [vis] => StructValueNsCtor::ValueNsCtorWithVis(vis),
            _ => StructValueNsCtor::ValueNsCtorWithMinVis(all),
        }
    }

    /// The name `id` names, `None` for [`NameId::NONE`].
    #[inline]
    pub fn get_name(&self, id: NameId) -> Option<&Name> {
        match id {
            NameId::NONE => None,
            id => Some(&self[id]),
        }
    }

    #[inline]
    pub fn import_alias(&self, alias: AliasId) -> Option<ImportAlias> {
        match alias {
            AliasId::NONE => None,
            AliasId::UNDERSCORE => Some(ImportAlias::Underscore),
            // clone: allocates nothing, a `Name` is an interned symbol
            AliasId { raw } => Some(ImportAlias::Alias(self[NameId { raw }].clone())),
        }
    }

    #[inline]
    pub fn use_tree(&self, id: UseTreeId) -> &UseTree {
        &self.use_trees()[id.raw as usize]
    }

    /// Returns a count of a few, expensive items.
    ///
    /// For more detail, see [`ItemTreeDataStats`].
    pub fn item_tree_stats(&self) -> ItemTreeDataStats {
        ItemTreeDataStats {
            traits: self.traits().len(),
            impls: self.impls().len(),
            mods: self.mods().len(),
            macro_calls: self.macro_calls().len(),
            macro_rules: self.macro_rules().len(),
        }
    }

    pub fn pretty_print(&self, db: &Db, edition: Edition) -> String {
        pretty::print_item_tree(db, self, edition)
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        // We don't need to check the rest since if those are empty, everything is empty.
        // `attr_owners` might hold the top-level attrs so we need to check it separately.
        self.top_level().is_empty() && self.attr_owners().is_empty()
    }
}

impl Index<NameId> for ItemTree {
    type Output = Name;
    #[inline]
    fn index(&self, id: NameId) -> &Name {
        &self.names()[id.raw as usize]
    }
}

impl Index<PathId> for ItemTree {
    type Output = ModPath;
    #[inline]
    fn index(&self, id: PathId) -> &ModPath {
        &self.paths()[id.raw as usize]
    }
}

#[derive(Default, Debug, Eq, PartialEq)]
pub struct ItemTreeDataStats {
    pub traits: usize,
    pub impls: usize,
    pub mods: usize,
    pub macro_calls: usize,
    pub macro_rules: usize,
}

/// Trait implemented by all nodes in the item tree.
pub(crate) trait ItemTreeNode: Clone {
    type Source: AstIdNode;
}

#[allow(type_alias_bounds)]
pub(crate) type ItemTreeAstId<T: ItemTreeNode> = FileAstId<T::Source>;

/// Identifies a particular [`ItemTree`].
#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub struct TreeId {
    file: HirFileId,
    block: Option<BlockId>,
}

impl TreeId {
    pub(crate) fn new(file: HirFileId, block: Option<BlockId>) -> Self {
        Self { file, block }
    }

    pub(crate) fn item_tree<'db>(
        &self,
        db: &'db Db,
        krate: Crate,
    ) -> &'db ItemTree {
        match self.block {
            Some(block) => block_item_tree_query(db, block, krate),
            None => file_item_tree(db, self.file, krate),
        }
    }

    #[inline]
    pub fn file_id(self) -> HirFileId {
        self.file
    }

    pub(crate) fn is_block(self) -> bool {
        self.block.is_some()
    }
}

blaze_format::record! {
    /// An item of an [`ItemTree`]: its index in the per-item columns.
    #[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, PartialOrd, Ord)]
    pub struct ModItemId: 1 {
        index: u32,
    }
}

impl ModItemId {
    const TOP_OWNER: ModItemId = ModItemId { index: u32::MAX };

    #[inline]
    fn new(index: u32) -> Self {
        debug_assert!(index != ModItemId::TOP_OWNER.index, "too big index");
        Self { index }
    }

    #[inline]
    pub(crate) fn ast_id(self, item_tree: &ItemTree) -> FileAstId<ast::Item> {
        item_tree.ast_ids()[self.index as usize]
    }
}

macro_rules! mod_items {
    ($mod_item:ident -> $( $typ:ident in $column:ident -> $ast:ty ),+ $(,)? ) => {
        /// Which kind an item is: which payload column its data word indexes.
        #[derive(Debug, Copy, Clone, PartialEq, Eq)]
        #[repr(u8)]
        pub(crate) enum ItemKind {
            $( $typ, )+
        }

        impl ItemKind {
            const ALL: &'static [ItemKind] = &[$( ItemKind::$typ, )+];

            /// The kind numbered `kind`, as `ItemKind::X as u8` numbers them.
            pub(crate) fn from_u8(kind: u8) -> Option<ItemKind> {
                Self::ALL.get(kind as usize).copied()
            }
        }

        #[derive(Debug, Copy, Clone)]
        pub(crate) enum $mod_item<'a> {
            $(
                $typ(FileAstId<$ast>, &'a $typ),
            )+
        }

        impl ItemTree {
            #[inline]
            pub(crate) fn index(&self, id: ModItemId) -> $mod_item<'_> {
                let at = id.index as usize;
                let (ast_id, data) = (self.ast_ids()[at], self.data()[at] as usize);
                match self.kinds()[at] {
                    $( ItemKind::$typ => $mod_item::$typ(ast_id.downcast_unchecked(), &self.$column()[data]), )+
                }
            }

            /// How long the payload column of `kind` is.
            fn column_len(&self, kind: ItemKind) -> usize {
                match kind {
                    $( ItemKind::$typ => self.$column().len(), )+
                }
            }

            /// Whether `ast_id` can be the id of an item of `kind`, which [`Self::index`]
            /// downcasts it to.
            fn ast_id_fits(kind: ItemKind, ast_id: FileAstId<ast::Item>) -> bool {
                match kind {
                    $( ItemKind::$typ => ast_id.erase().may_be::<$ast>(), )+
                }
            }
        }

        $(
            impl ItemTreeNode for $typ {
                type Source = $ast;
            }
        )+
    };
}

mod_items! {
ModItemKind ->
    Const in consts -> ast::Const,
    Enum in enums -> ast::Enum,
    ExternBlock in extern_blocks -> ast::ExternBlock,
    ExternCrate in extern_crates -> ast::ExternCrate,
    Function in functions -> ast::Fn,
    Impl in impls -> ast::Impl,
    Macro2 in macro2s -> ast::MacroDef,
    MacroCall in macro_calls -> ast::MacroCall,
    MacroRules in macro_rules -> ast::MacroRules,
    Mod in mods -> ast::Module,
    Static in statics -> ast::Static,
    Struct in structs -> ast::Struct,
    Trait in traits -> ast::Trait,
    TypeAlias in type_aliases -> ast::TypeAlias,
    Union in unions -> ast::Union,
    Use in uses -> ast::Use,
}

const _: () = assert!(size_of::<ItemKind>() == 1);

impl Index<RawVisibilityId> for ItemTree {
    type Output = RawVisibility;
    fn index(&self, index: RawVisibilityId) -> &Self::Output {
        static VIS_PUB: RawVisibility = RawVisibility::Public;
        static VIS_PRIV_IMPLICIT: RawVisibility =
            RawVisibility::PubSelf(VisibilityExplicitness::Implicit);
        static VIS_PRIV_EXPLICIT: RawVisibility =
            RawVisibility::PubSelf(VisibilityExplicitness::Explicit);
        static VIS_PUB_CRATE: RawVisibility = RawVisibility::PubCrate;

        match index {
            RawVisibilityId::PRIV_IMPLICIT => &VIS_PRIV_IMPLICIT,
            RawVisibilityId::PRIV_EXPLICIT => &VIS_PRIV_EXPLICIT,
            RawVisibilityId::PUB => &VIS_PUB,
            RawVisibilityId::PUB_CRATE => &VIS_PUB_CRATE,
            _ => &self.vis()[index.raw as usize],
        }
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Use: 1 {
        pub(crate) visibility: RawVisibilityId,
        pub(crate) use_tree: UseTreeId,
    }
}

blaze_format::record! {
    /// One `use` tree: [`UseTree::kind`] reads it.
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct UseTree: 1 {
        /// [`UseTree::SINGLE`], [`UseTree::GLOB`] or [`UseTree::PREFIXED`].
        kind: u32,
        /// The path, the glob's path or the prefix; [`PathId::NONE`] for none.
        path: PathId,
        /// A single import's alias.
        alias: AliasId,
        /// A prefixed tree's children, in `use_tree_lists`.
        list: IdRange,
    }
}

// FIXME: Would be nice to encode `None` into this
// We could just use a `Name` where `_` well means `_` ..
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportAlias {
    /// Unnamed alias, as in `use Foo as _;`
    Underscore,
    /// Named alias
    Alias(Name),
}

impl ImportAlias {
    pub fn display(&self, edition: Edition) -> impl fmt::Display + '_ {
        ImportAliasDisplay { value: self, edition }
    }
}

struct ImportAliasDisplay<'a> {
    value: &'a ImportAlias,
    edition: Edition,
}

impl fmt::Display for ImportAliasDisplay<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.value {
            ImportAlias::Underscore => f.write_str("_"),
            ImportAlias::Alias(name) => fmt::Display::fmt(&name.display_no_db(self.edition), f),
        }
    }
}

/// A `use` tree, read from the tree's columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseTreeKind<'a> {
    /// ```ignore
    /// use path::to::Item;
    /// use path::to::Item as Renamed;
    /// use path::to::Trait as _;
    /// ```
    Single { path: &'a ModPath, alias: Option<ImportAlias> },

    /// ```ignore
    /// use *;  // (invalid, but can occur in nested tree)
    /// use path::*;
    /// ```
    Glob { path: Option<&'a ModPath> },

    /// ```ignore
    /// use prefix::{self, Item, ...};
    /// ```
    Prefixed { prefix: Option<&'a ModPath>, list: &'a [UseTreeId] },
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct ExternCrate: 1 {
        pub name: NameId,
        pub alias: AliasId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct ExternBlock: 1 {
        /// The block's items: [`ItemTree::items`].
        pub(crate) children: IdRange,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Function: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Struct: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
        /// The constructor's visibilities in `ctor_vis`: [`ItemTree::value_ns_ctor`].
        pub(crate) value_ns_ctor: IdRange,
    }
}

/// The visibility of a struct's constructor in the value namespace, read from the tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StructValueNsCtor<'a> {
    NoValueNsCtor,
    ValueNsCtorWithVis(&'a RawVisibilityId),
    /// The least of these (a tuple struct whose fields are `pub(in ...)` of different paths).
    ValueNsCtorWithMinVis(&'a [RawVisibilityId]),
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Union: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Enum: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum FieldsShape {
    Record,
    Tuple,
    Unit,
}

impl FieldsShape {
    #[inline]
    pub fn has_value_ns_ctor(self) -> bool {
        match self {
            FieldsShape::Record => false,
            FieldsShape::Tuple | FieldsShape::Unit => true,
        }
    }
}

/// Visibility of an item, not yet resolved.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RawVisibility {
    /// `pub(in module)`, `pub(crate)` or `pub(super)`. Also private, which is
    /// equivalent to `pub(self)`.
    Module(ModPath, VisibilityExplicitness),
    /// `pub(self)`.
    PubSelf(VisibilityExplicitness),
    /// `pub(crate)`.
    PubCrate,
    /// `pub`.
    Public,
}

/// Whether the item was imported through an explicit `pub(crate) use` or just a `use` without
/// visibility.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum VisibilityExplicitness {
    Explicit,
    Implicit,
}

impl VisibilityExplicitness {
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit)
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Const: 1 {
        /// [`NameId::NONE`] for `const _: () = ();`: [`ItemTree::get_name`].
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Static: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Trait: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Impl: 1 {
        pub is_trait_impl: bool,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct TypeAlias: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Mod: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
        /// An inline module's items; not a list for `mod m;`: [`ItemTree::mod_kind`].
        pub(crate) items: IdRange,
    }
}

/// Whether a module is inline, read from the tree.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum ModKind<'a> {
    /// `mod m { ... }`
    Inline { items: &'a [ModItemId] },
    /// `mod m;`
    Outline,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct MacroCall {
    /// Path to the called macro.
    pub path: PathId,
    pub ctxt: SyntaxContext,
    pub expand_to: ExpandTo,
}

blaze_format::record! {
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct MacroRules: 1 {
        /// The name of the declared macro.
        pub name: NameId,
    }
}

blaze_format::record! {
    /// "Macros 2.0" macro definition.
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    pub struct Macro2: 1 {
        pub name: NameId,
        pub(crate) visibility: RawVisibilityId,
    }
}

const _: () = {
    assert!(size_of::<Const>() == 8);
    assert!(size_of::<Enum>() == 8);
    assert!(size_of::<ExternBlock>() == 8);
    assert!(size_of::<ExternCrate>() == 12);
    assert!(size_of::<Function>() == 8);
    assert!(size_of::<Impl>() == 1);
    assert!(size_of::<Macro2>() == 8);
    assert!(size_of::<MacroCall>() == 12);
    assert!(size_of::<MacroRules>() == 4);
    assert!(size_of::<Mod>() == 16);
    assert!(size_of::<Static>() == 8);
    assert!(size_of::<Struct>() == 16);
    assert!(size_of::<Trait>() == 8);
    assert!(size_of::<TypeAlias>() == 8);
    assert!(size_of::<Union>() == 8);
    assert!(size_of::<Use>() == 8);
    assert!(size_of::<UseTree>() == 20);
    assert!(size_of::<AttrOwner>() == 16);
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImportKind {
    /// The `ModPath` is imported normally.
    Plain,
    /// This is a glob-import of all names in the `ModPath`.
    Glob,
    /// This is a `some::path::self` import, which imports `some::path` only in type namespace.
    TypeOnly,
}

impl Use {
    /// Expands the `UseTree` into individually imported `ModPath`s.
    pub fn expand(
        &self,
        tree: &ItemTree,
        mut cb: impl FnMut(Idx<ast::UseTree>, ModPath, ImportKind, Option<ImportAlias>),
    ) {
        tree.use_tree(self.use_tree).expand_impl(tree, None, &mut 0, &mut cb)
    }
}

impl UseTree {
    const SINGLE: u32 = 0;
    const GLOB: u32 = 1;
    const PREFIXED: u32 = 2;

    /// The [`UseTreeKind`] of this `UseTree`, in `tree`.
    pub fn kind<'a>(&self, tree: &'a ItemTree) -> UseTreeKind<'a> {
        let path = match self.path {
            PathId::NONE => None,
            path => Some(&tree[path]),
        };
        match self.kind {
            UseTree::SINGLE => UseTreeKind::Single {
                path: path.expect("a single import with no path"),
                alias: tree.import_alias(self.alias),
            },
            UseTree::GLOB => UseTreeKind::Glob { path },
            _ => UseTreeKind::Prefixed {
                prefix: path,
                list: &tree.use_tree_lists()[self.list.range()],
            },
        }
    }

    fn expand_impl(
        &self,
        tree: &ItemTree,
        prefix: Option<ModPath>,
        counting_index: &mut u32,
        cb: &mut impl FnMut(Idx<ast::UseTree>, ModPath, ImportKind, Option<ImportAlias>),
    ) {
        fn concat_mod_paths(
            prefix: Option<ModPath>,
            path: &ModPath,
        ) -> Option<(ModPath, ImportKind)> {
            match (prefix, path.kind) {
                // clone: allocates nothing, a `ModPath` is an interned slice
                (None, _) => Some((path.clone(), ImportKind::Plain)),
                (Some(mut prefix), PathKind::Plain) => {
                    prefix.extend(path.segments().iter().cloned());
                    Some((prefix, ImportKind::Plain))
                }
                (Some(mut prefix), PathKind::Super(n)) if n > 0 && prefix.segments().is_empty() => {
                    // `super::super` + `super::rest`
                    match &mut prefix.kind {
                        PathKind::Super(m) => {
                            cov_mark::hit!(concat_super_mod_paths);
                            *m += n;
                            prefix.extend(path.segments().iter().cloned());
                            Some((prefix, ImportKind::Plain))
                        }
                        _ => None,
                    }
                }
                (Some(prefix), PathKind::SELF) if path.segments().is_empty() => {
                    // `some::path::self` == `some::path`
                    Some((prefix, ImportKind::TypeOnly))
                }
                (Some(_), _) => None,
            }
        }

        match self.kind(tree) {
            UseTreeKind::Single { path, alias } => {
                if let Some((path, kind)) = concat_mod_paths(prefix, path) {
                    cb(Idx::from_raw(RawIdx::from_u32(*counting_index)), path, kind, alias);
                }
            }
            UseTreeKind::Glob { path: Some(path) } => {
                if let Some((path, _)) = concat_mod_paths(prefix, path) {
                    cb(
                        Idx::from_raw(RawIdx::from_u32(*counting_index)),
                        path,
                        ImportKind::Glob,
                        None,
                    );
                }
            }
            UseTreeKind::Glob { path: None } => {
                if let Some(prefix) = prefix {
                    cb(
                        Idx::from_raw(RawIdx::from_u32(*counting_index)),
                        prefix,
                        ImportKind::Glob,
                        None,
                    );
                }
            }
            UseTreeKind::Prefixed { prefix: additional_prefix, list } => {
                let prefix = match additional_prefix {
                    Some(path) => match concat_mod_paths(prefix, path) {
                        Some((path, ImportKind::Plain)) => Some(path),
                        _ => return,
                    },
                    None => prefix,
                };
                for &child in list {
                    *counting_index += 1;
                    // clone: allocates nothing, a `ModPath` is an interned slice
                    tree.use_tree(child).expand_impl(tree, prefix.clone(), counting_index, cb);
                }
            }
        }
    }
}
