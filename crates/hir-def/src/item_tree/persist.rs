//! An [`ItemTree`] in a [`blaze_format`] buffer (docs/v2/plan/lanes/format.md section 5): its
//! columns as they are in memory (docs/v2/plan/design/data.md), and its tables (names, paths,
//! visibilities, attributes, `cfg`s) in the words of [`crate::hir::persist`].
//!
//! The buffer is a [`Root`] of [`Slice`]s, one per column of the tree, each read back by a
//! copy; the per-item columns, the payload columns and the lists are plain records, written
//! byte for byte. What the records name (items, names, paths, visibilities, lists, `cfg`s) is
//! checked on decode, so a tree read back indexes only what it holds, and each list's items are
//! before the item holding it, so no module holds itself. Equal trees are equal bytes.
//!
//! A token tree is written as its flat tokens and built again with the
//! [`tt::TopSubtreeBuilder`] calls that built it, so the encoding inside a [`tt::TopSubtree`],
//! which its `==` compares, comes out the same.

use std::mem::offset_of;

use blaze_format::{FormatError, Part, Persist, Record, Slice, Writer};
use hir_expand::{
    ExpandTo,
    attrs::{Attr, AttrInput},
};
use span::{ErasedFileAstId, FileAstId, Span};

use super::{
    AliasId, AttrOwner, Const, Enum, ExternBlock, ExternCrate, FieldsShape, Function, IdRange,
    Impl, ImportAlias, ItemKind, ItemTree, ItemTreeLens, Macro2, MacroCall, MacroRules, Mod,
    ModItemId, NameId, PathId, RawVisibility, RawVisibilityId, Static, Struct, Trait, TypeAlias,
    Union, Use, UseTree, UseTreeId, VisibilityExplicitness,
};
use crate::hir::persist::{Code, Column, Read, Words, WordsRec, syntax_context};

blaze_format::record! {
    /// A [`MacroCall`] as a record: its context and its expansion kind as words.
    #[derive(Clone, Copy, Debug)]
    struct MacroCallRec: 1 {
        path: PathId,
        ctxt: u32,
        expand_to: u32,
    }
}

blaze_format::record! {
    /// An item tree's columns, and where in its words each of its tables starts.
    #[derive(Clone, Copy)]
    pub(crate) struct Root: 2 {
        top_level: Slice<ModItemId>,
        kinds: Slice<u8>,
        data: Slice<u32>,
        ast_ids: Slice<u32>,
        children: Slice<ModItemId>,
        consts: Slice<Const>,
        enums: Slice<Enum>,
        extern_blocks: Slice<ExternBlock>,
        extern_crates: Slice<ExternCrate>,
        functions: Slice<Function>,
        impls: Slice<Impl>,
        macro2s: Slice<Macro2>,
        macro_calls: Slice<MacroCallRec>,
        macro_rules: Slice<MacroRules>,
        mods: Slice<Mod>,
        statics: Slice<Static>,
        structs: Slice<Struct>,
        traits: Slice<Trait>,
        type_aliases: Slice<TypeAlias>,
        unions: Slice<Union>,
        uses: Slice<Use>,
        use_trees: Slice<UseTree>,
        use_tree_lists: Slice<UseTreeId>,
        ctor_vis: Slice<RawVisibilityId>,
        attr_owners: Slice<AttrOwner>,
        words: WordsRec,
        names_at: u32,
        paths_at: u32,
        vis_at: u32,
        attrs_at: u32,
        cfgs_at: u32,
    }
}

/// An [`ItemTree`] read in place.
#[derive(Clone, Copy)]
pub struct ItemTreeRef<'a> {
    buffer: &'a [u8],
    root: &'a Root,
    // The per-item columns and the lists, resolved once by `read`: an item is then two loads.
    top_level: &'a [ModItemId],
    kinds: &'a [u8],
    ast_ids: &'a [u32],
    children: &'a [ModItemId],
}

impl<'a> ItemTreeRef<'a> {
    /// The items at the top level of the file, as [`ItemTree::top_level_items`] lists them.
    pub fn top_level(self) -> impl Iterator<Item = ModItemId> + 'a {
        self.top_level.iter().copied()
    }

    /// How many items the tree has, at the top level or not.
    pub fn len(self) -> usize {
        self.kinds.len()
    }

    /// The kind of `item`, read in place; `None` when the buffer has no such item or its kind
    /// is out of range (which [`ItemTreeRef::decode`] refuses).
    pub(crate) fn kind(self, item: ModItemId) -> Option<ItemKind> {
        ItemKind::from_u8(*self.kinds.get(item.index as usize)?)
    }

    /// The ast id of `item`, read in place.
    pub fn ast_id(self, item: ModItemId) -> Option<ErasedFileAstId> {
        self.ast_ids.get(item.index as usize).map(|&raw| ErasedFileAstId::from_raw(raw))
    }

    /// The items of an inline module's or an extern block's list, read in place.
    pub fn children(self, list: IdRange) -> Option<&'a [ModItemId]> {
        if !list.within(self.children.len()) {
            return None;
        }
        Some(&self.children[list.range()])
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// The owned tree, equal to the one written: its one block, sized from the buffer's column
    /// lengths and table counts before a value is read.
    pub fn decode(self) -> Result<ItemTree, FormatError> {
        let (root, buffer) = (self.root, self.buffer);
        let invalid = |what| FormatError::Invalid { at: self.position(root), what };
        macro_rules! follow {
            ($($column:ident),+) => { $( let $column = root.$column.follow(buffer)?; )+ };
        }
        follow!(
            data,
            consts,
            enums,
            extern_blocks,
            extern_crates,
            functions,
            impls,
            macro2s,
            macro_calls,
            macro_rules,
            mods,
            statics,
            structs,
            traits,
            type_aliases,
            unions,
            uses,
            use_trees,
            use_tree_lists,
            ctor_vis,
            attr_owners
        );
        let words = Read::new(&root.words, buffer);
        let mut names = words.from(root.names_at);
        let mut paths = words.from(root.paths_at);
        let mut vis = words.from(root.vis_at);
        let mut attrs = words.from(root.attrs_at);
        let mut cfgs = words.from(root.cfgs_at);
        let lens = ItemTreeLens {
            top_level: self.top_level.len(),
            kinds: self.kinds.len(),
            data: data.len(),
            ast_ids: self.ast_ids.len(),
            children: self.children.len(),
            consts: consts.len(),
            enums: enums.len(),
            extern_blocks: extern_blocks.len(),
            extern_crates: extern_crates.len(),
            functions: functions.len(),
            impls: impls.len(),
            macro2s: macro2s.len(),
            macro_calls: macro_calls.len(),
            macro_rules: macro_rules.len(),
            mods: mods.len(),
            statics: statics.len(),
            structs: structs.len(),
            traits: traits.len(),
            type_aliases: type_aliases.len(),
            unions: unions.len(),
            uses: uses.len(),
            use_trees: use_trees.len(),
            use_tree_lists: use_tree_lists.len(),
            ctor_vis: ctor_vis.len(),
            names: names.len()?,
            paths: paths.len()?,
            vis: vis.len()?,
            attr_owners: attr_owners.len(),
            attr_index: self.kinds.len().div_ceil(64),
            attr_list: attrs.len()?,
            cfgs: cfgs.len()?,
        };
        // alloc: the tree's block, once a store read, the edge
        let tree = ItemTree::try_build(lens, |s| {
            s.top_level.extend_from_slice(self.top_level);
            for &kind in self.kinds {
                let kind =
                    ItemKind::from_u8(kind).ok_or_else(|| invalid("an item kind out of range"))?;
                s.kinds.push(kind);
            }
            s.data.extend_from_slice(data);
            s.ast_ids.extend(
                (self.ast_ids.iter())
                    .map(|&raw| FileAstId::from_erased_unchecked(ErasedFileAstId::from_raw(raw))),
            );
            s.children.extend_from_slice(self.children);
            s.consts.extend_from_slice(consts);
            s.enums.extend_from_slice(enums);
            s.extern_blocks.extend_from_slice(extern_blocks);
            s.extern_crates.extend_from_slice(extern_crates);
            s.functions.extend_from_slice(functions);
            s.impls.extend_from_slice(impls);
            s.macro2s.extend_from_slice(macro2s);
            for call in macro_calls {
                let expand_to = expand_to_of(call.expand_to)
                    .ok_or_else(|| invalid("a macro expansion kind out of range"))?;
                let ctxt = syntax_context(call.ctxt)
                    .ok_or_else(|| invalid("a syntax context of u32::MAX"))?;
                s.macro_calls.push(MacroCall { path: call.path, ctxt, expand_to });
            }
            s.macro_rules.extend_from_slice(macro_rules);
            s.mods.extend_from_slice(mods);
            s.statics.extend_from_slice(statics);
            s.structs.extend_from_slice(structs);
            s.traits.extend_from_slice(traits);
            s.type_aliases.extend_from_slice(type_aliases);
            s.unions.extend_from_slice(unions);
            s.uses.extend_from_slice(uses);
            s.use_trees.extend_from_slice(use_trees);
            s.use_tree_lists.extend_from_slice(use_tree_lists);
            s.ctor_vis.extend_from_slice(ctor_vis);
            for _ in 0..lens.names {
                s.names.push(names.get()?);
            }
            for _ in 0..lens.paths {
                s.paths.push(paths.get()?);
            }
            for _ in 0..lens.vis {
                s.vis.push(vis.get()?);
            }
            s.attr_owners.extend_from_slice(attr_owners);
            s.attr_index.extend(super::attr_blocks(attr_owners, self.kinds.len()));
            for _ in 0..lens.attr_list {
                s.attr_list.push(attrs.get()?);
            }
            for _ in 0..lens.cfgs {
                s.cfgs.push(cfgs.get()?);
            }
            Ok(())
        })?;
        check(&tree).map_err(invalid)?;
        Ok(tree)
    }

    fn position<T>(&self, value: &T) -> u64 {
        (value as *const T as usize).wrapping_sub(self.buffer.as_ptr() as usize) as u64
    }
}

/// Whether every id the tree's records hold names something the tree has, every list is within
/// its array and holds only what comes before its holder, and the attributes are in the order
/// [`ItemTree::attrs`] searches them in: what indexing the tree relies on.
fn check(tree: &ItemTree) -> Result<(), &'static str> {
    let items = tree.kinds().len();
    if tree.data().len() != items || tree.ast_ids().len() != items {
        return Err("per-item columns of different lengths");
    }
    let vis = |id: RawVisibilityId| id.is_builtin() || (id.raw as usize) < tree.vis().len();
    let name = |id: NameId| (id.raw as usize) < tree.names().len();
    let path = |id: PathId| (id.raw as usize) < tree.paths().len();
    let path_or_none = |id: PathId| id == PathId::NONE || path(id);
    let alias =
        |id: AliasId| matches!(id, AliasId::NONE | AliasId::UNDERSCORE) || name(NameId { raw: id.raw });
    let named = |name_id: NameId, vis_id: RawVisibilityId| name(name_id) && vis(vis_id);
    // A list of items, each before the item `holder` that holds the list.
    let list = |range: IdRange, holder: usize| {
        range.within(tree.children().len())
            && tree.children()[range.range()].iter().all(|child| (child.index as usize) < holder)
    };

    if !tree.top_level().iter().all(|id| (id.index as usize) < items) {
        return Err("a top-level item that is not in the tree");
    }
    for at in 0..items {
        let (kind, data) = (tree.kinds()[at], tree.data()[at] as usize);
        if data >= tree.column_len(kind) {
            return Err("an item past the column of its kind");
        }
        if !ItemTree::ast_id_fits(kind, tree.ast_ids()[at]) {
            return Err("an item whose ast id is of another kind");
        }
        let ok = match kind {
            ItemKind::Const => {
                let it = &tree.consts()[data];
                (it.name == NameId::NONE || name(it.name)) && vis(it.visibility)
            }
            ItemKind::Enum => named(tree.enums()[data].name, tree.enums()[data].visibility),
            ItemKind::ExternBlock => list(tree.extern_blocks()[data].children, at),
            ItemKind::ExternCrate => {
                let it = &tree.extern_crates()[data];
                named(it.name, it.visibility) && alias(it.alias)
            }
            ItemKind::Function => {
                named(tree.functions()[data].name, tree.functions()[data].visibility)
            }
            ItemKind::Impl => true,
            ItemKind::Macro2 => named(tree.macro2s()[data].name, tree.macro2s()[data].visibility),
            ItemKind::MacroCall => path(tree.macro_calls()[data].path),
            ItemKind::MacroRules => name(tree.macro_rules()[data].name),
            ItemKind::Mod => {
                let it = &tree.mods()[data];
                named(it.name, it.visibility) && (it.items == IdRange::NONE || list(it.items, at))
            }
            ItemKind::Static => named(tree.statics()[data].name, tree.statics()[data].visibility),
            ItemKind::Struct => {
                let it = &tree.structs()[data];
                named(it.name, it.visibility) && it.value_ns_ctor.within(tree.ctor_vis().len())
            }
            ItemKind::Trait => named(tree.traits()[data].name, tree.traits()[data].visibility),
            ItemKind::TypeAlias => {
                named(tree.type_aliases()[data].name, tree.type_aliases()[data].visibility)
            }
            ItemKind::Union => named(tree.unions()[data].name, tree.unions()[data].visibility),
            ItemKind::Use => {
                let it = &tree.uses()[data];
                vis(it.visibility) && (it.use_tree.raw as usize) < tree.use_trees().len()
            }
        };
        if !ok {
            return Err("an item naming something that is not in the tree");
        }
    }
    if !tree.ctor_vis().iter().all(|&id| vis(id)) {
        return Err("a constructor visibility that is not in the tree");
    }
    for (at, it) in tree.use_trees().iter().enumerate() {
        let ok = match it.kind {
            UseTree::SINGLE => path(it.path) && alias(it.alias) && it.list == IdRange::EMPTY,
            UseTree::GLOB => {
                path_or_none(it.path) && it.alias == AliasId::NONE && it.list == IdRange::EMPTY
            }
            UseTree::PREFIXED => {
                path_or_none(it.path)
                    && it.alias == AliasId::NONE
                    && it.list.within(tree.use_tree_lists().len())
                    && (tree.use_tree_lists()[it.list.range()].iter())
                        .all(|child| (child.raw as usize) < at)
            }
            _ => false,
        };
        if !ok {
            return Err("a use tree that is not one, or not before the tree holding it");
        }
    }
    for owner in tree.attr_owners() {
        if owner.owner != ModItemId::TOP_OWNER && owner.owner.index as usize >= items {
            return Err("attributes of an item that is not in the tree");
        }
        if !owner.attrs.within(tree.attr_list().len()) {
            return Err("attributes past the tree's");
        }
        if owner.cfg != AttrOwner::ENABLED && owner.cfg as usize >= tree.cfgs().len() {
            return Err("a `cfg` that is not in the tree");
        }
    }
    if !tree.attr_owners().windows(2).all(|pair| pair[0].owner < pair[1].owner) {
        return Err("attributes not in the order of their items");
    }
    Ok(())
}

impl ItemTree {
    /// The warm and cold forms of item tree buffers, codec by column: what
    /// `blaze_cas::Store::encode_columns` is given for [`ItemTree`]'s schema.
    pub fn columns() -> blaze_format::tiers::BufferColumns {
        blaze_format::tiers::BufferColumns::of::<Root>(<ItemTree as Persist>::SCHEMA)
    }
}

impl Persist for ItemTree {
    const SCHEMA: blaze_format::SchemaHash = blaze_format::schema_of(&[
        Part::Str("ItemTree"),
        Part::U64(4),
        Part::Schema(<Root as Record>::SCHEMA),
    ]);
    type Ref<'a> = ItemTreeRef<'a>;

    fn write(&self, out: &mut Writer) -> u64 {
        let mut w = Words::new();
        let names_at = w.at();
        w.seq(self.names().iter());
        let paths_at = w.at();
        w.seq(self.paths().iter());
        let vis_at = w.at();
        w.seq(self.vis().iter());
        let attrs_at = w.at();
        w.seq(self.attr_list().iter());
        let cfgs_at = w.at();
        w.seq(self.cfgs().iter());

        // alloc: once a store write, the edge
        let kinds: Vec<u8> = self.kinds().iter().map(|&kind| kind as u8).collect();
        // alloc: once a store write, the edge
        let ast_ids: Vec<u32> = self.ast_ids().iter().map(|id| id.erase().into_raw()).collect();
        let macro_calls: Vec<MacroCallRec> = (self.macro_calls().iter())
            .map(|call| MacroCallRec {
                path: call.path,
                ctxt: call.ctxt.into_u32(),
                expand_to: expand_to_word(call.expand_to),
            })
            .collect(); // alloc: once a store write, the edge

        let placed = w.place(out);
        let top_level = Column::push(out, self.top_level());
        let kinds = Column::push(out, &kinds);
        let data = Column::push(out, self.data());
        let ast_ids = Column::push(out, &ast_ids);
        let children = Column::push(out, self.children());
        let consts = Column::push(out, self.consts());
        let enums = Column::push(out, self.enums());
        let extern_blocks = Column::push(out, self.extern_blocks());
        let extern_crates = Column::push(out, self.extern_crates());
        let functions = Column::push(out, self.functions());
        let impls = Column::push(out, self.impls());
        let macro2s = Column::push(out, self.macro2s());
        let macro_calls = Column::push(out, &macro_calls);
        let macro_rules = Column::push(out, self.macro_rules());
        let mods = Column::push(out, self.mods());
        let statics = Column::push(out, self.statics());
        let structs = Column::push(out, self.structs());
        let traits = Column::push(out, self.traits());
        let type_aliases = Column::push(out, self.type_aliases());
        let unions = Column::push(out, self.unions());
        let uses = Column::push(out, self.uses());
        let use_trees = Column::push(out, self.use_trees());
        let use_tree_lists = Column::push(out, self.use_tree_lists());
        let ctor_vis = Column::push(out, self.ctor_vis());
        let attr_owners = Column::push(out, self.attr_owners());

        let at = out.next::<Root>();
        let field = |offset: usize| at + offset as u64;
        let root = Root {
            top_level: top_level.slice(field(offset_of!(Root, top_level))),
            kinds: kinds.slice(field(offset_of!(Root, kinds))),
            data: data.slice(field(offset_of!(Root, data))),
            ast_ids: ast_ids.slice(field(offset_of!(Root, ast_ids))),
            children: children.slice(field(offset_of!(Root, children))),
            consts: consts.slice(field(offset_of!(Root, consts))),
            enums: enums.slice(field(offset_of!(Root, enums))),
            extern_blocks: extern_blocks.slice(field(offset_of!(Root, extern_blocks))),
            extern_crates: extern_crates.slice(field(offset_of!(Root, extern_crates))),
            functions: functions.slice(field(offset_of!(Root, functions))),
            impls: impls.slice(field(offset_of!(Root, impls))),
            macro2s: macro2s.slice(field(offset_of!(Root, macro2s))),
            macro_calls: macro_calls.slice(field(offset_of!(Root, macro_calls))),
            macro_rules: macro_rules.slice(field(offset_of!(Root, macro_rules))),
            mods: mods.slice(field(offset_of!(Root, mods))),
            statics: statics.slice(field(offset_of!(Root, statics))),
            structs: structs.slice(field(offset_of!(Root, structs))),
            traits: traits.slice(field(offset_of!(Root, traits))),
            type_aliases: type_aliases.slice(field(offset_of!(Root, type_aliases))),
            unions: unions.slice(field(offset_of!(Root, unions))),
            uses: uses.slice(field(offset_of!(Root, uses))),
            use_trees: use_trees.slice(field(offset_of!(Root, use_trees))),
            use_tree_lists: use_tree_lists.slice(field(offset_of!(Root, use_tree_lists))),
            ctor_vis: ctor_vis.slice(field(offset_of!(Root, ctor_vis))),
            attr_owners: attr_owners.slice(field(offset_of!(Root, attr_owners))),
            words: placed.rec(field(offset_of!(Root, words))),
            names_at,
            paths_at,
            vis_at,
            attrs_at,
            cfgs_at,
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
        at
    }

    fn read(buffer: &[u8]) -> Result<ItemTreeRef<'_>, FormatError> {
        let root = blaze_format::open_as::<Root>(buffer, Self::SCHEMA)?;
        Ok(ItemTreeRef {
            buffer,
            root,
            top_level: root.top_level.follow(buffer)?,
            kinds: root.kinds.follow(buffer)?,
            ast_ids: root.ast_ids.follow(buffer)?,
            children: root.children.follow(buffer)?,
        })
    }
}

impl Code for RawVisibility {
    fn put(&self, w: &mut Words) {
        match self {
            RawVisibility::Module(path, explicitness) => {
                w.u32(0);
                path.put(w);
                explicitness.put(w);
            }
            RawVisibility::PubSelf(explicitness) => {
                w.u32(1);
                explicitness.put(w);
            }
            RawVisibility::PubCrate => w.u32(2),
            RawVisibility::Public => w.u32(3),
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(4, "a visibility out of range")? {
            0 => {
                let path = r.get()?;
                RawVisibility::Module(path, r.get()?)
            }
            1 => RawVisibility::PubSelf(r.get()?),
            2 => RawVisibility::PubCrate,
            _ => RawVisibility::Public,
        })
    }
}

/// The word an [`ExpandTo`] is written as.
fn expand_to_word(expand_to: ExpandTo) -> u32 {
    match expand_to {
        ExpandTo::Statements => 0,
        ExpandTo::Items => 1,
        ExpandTo::Pattern => 2,
        ExpandTo::Type => 3,
        ExpandTo::Expr => 4,
    }
}

/// The [`ExpandTo`] written as `word`, if one is.
fn expand_to_of(word: u32) -> Option<ExpandTo> {
    Some(match word {
        0 => ExpandTo::Statements,
        1 => ExpandTo::Items,
        2 => ExpandTo::Pattern,
        3 => ExpandTo::Type,
        4 => ExpandTo::Expr,
        _ => return None,
    })
}

impl Code for ExpandTo {
    fn put(&self, w: &mut Words) {
        w.u32(expand_to_word(*self));
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let word = r.u32()?;
        expand_to_of(word).ok_or_else(|| r.invalid("a macro expansion kind out of range"))
    }
}

impl Code for VisibilityExplicitness {
    fn put(&self, w: &mut Words) {
        w.u32(match self {
            VisibilityExplicitness::Explicit => 0,
            VisibilityExplicitness::Implicit => 1,
        });
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "a visibility explicitness out of range")? {
            0 => VisibilityExplicitness::Explicit,
            _ => VisibilityExplicitness::Implicit,
        })
    }
}

impl Code for FieldsShape {
    fn put(&self, w: &mut Words) {
        w.u32(match self {
            FieldsShape::Record => 0,
            FieldsShape::Tuple => 1,
            FieldsShape::Unit => 2,
        });
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(3, "a fields shape out of range")? {
            0 => FieldsShape::Record,
            1 => FieldsShape::Tuple,
            _ => FieldsShape::Unit,
        })
    }
}

impl Code for ImportAlias {
    fn put(&self, w: &mut Words) {
        match self {
            ImportAlias::Underscore => w.u32(0),
            ImportAlias::Alias(name) => {
                w.u32(1);
                name.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "an import alias out of range")? {
            0 => ImportAlias::Underscore,
            _ => ImportAlias::Alias(r.get()?),
        })
    }
}

impl Code for Attr {
    fn put(&self, w: &mut Words) {
        self.path.put(w);
        self.input.put(w);
        self.ctxt.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let path = r.get()?;
        let input = r.get()?;
        Ok(Attr { path, input, ctxt: r.get()? })
    }
}

impl Code for AttrInput {
    fn put(&self, w: &mut Words) {
        match self {
            AttrInput::Literal(literal) => {
                w.u32(0);
                literal.put(w);
            }
            AttrInput::TokenTree(tree) => {
                w.u32(1);
                tree.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "an attribute input out of range")? {
            0 => AttrInput::Literal(r.get()?),
            _ => AttrInput::TokenTree(r.get()?),
        })
    }
}

impl Code for tt::LitKind {
    fn put(&self, w: &mut Words) {
        match *self {
            tt::LitKind::Byte => w.u32(0),
            tt::LitKind::Char => w.u32(1),
            tt::LitKind::Integer => w.u32(2),
            tt::LitKind::Float => w.u32(3),
            tt::LitKind::Str => w.u32(4),
            tt::LitKind::StrRaw(hashes) => {
                w.u32(5);
                hashes.put(w);
            }
            tt::LitKind::ByteStr => w.u32(6),
            tt::LitKind::ByteStrRaw(hashes) => {
                w.u32(7);
                hashes.put(w);
            }
            tt::LitKind::CStr => w.u32(8),
            tt::LitKind::CStrRaw(hashes) => {
                w.u32(9);
                hashes.put(w);
            }
            tt::LitKind::Err(()) => w.u32(10),
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(11, "a literal kind out of range")? {
            0 => tt::LitKind::Byte,
            1 => tt::LitKind::Char,
            2 => tt::LitKind::Integer,
            3 => tt::LitKind::Float,
            4 => tt::LitKind::Str,
            5 => tt::LitKind::StrRaw(r.get()?),
            6 => tt::LitKind::ByteStr,
            7 => tt::LitKind::ByteStrRaw(r.get()?),
            8 => tt::LitKind::CStr,
            9 => tt::LitKind::CStrRaw(r.get()?),
            _ => tt::LitKind::Err(()),
        })
    }
}

impl Code for tt::Literal {
    fn put(&self, w: &mut Words) {
        w.symbol(&self.text_and_suffix);
        self.span.put(w);
        self.kind.put(w);
        self.suffix_len.put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let text_and_suffix = r.symbol()?;
        let span = r.get()?;
        let kind = r.get()?;
        let suffix_len: u32 = r.get()?;
        let text = text_and_suffix.as_str();
        let split = text.len().checked_sub(suffix_len as usize);
        match split.is_some_and(|at| text.is_char_boundary(at)) {
            true => Ok(tt::Literal { text_and_suffix, span, kind, suffix_len }),
            false => Err(r.invalid("a literal suffix that is not the end of its text")),
        }
    }
}

impl Code for tt::Spacing {
    fn put(&self, w: &mut Words) {
        w.u32(*self as u32);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(3, "a punctuation spacing out of range")? {
            0 => tt::Spacing::Alone,
            1 => tt::Spacing::Joint,
            _ => tt::Spacing::JointHidden,
        })
    }
}

impl Code for tt::IdentIsRaw {
    fn put(&self, w: &mut Words) {
        w.u32(*self as u32);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(2, "an identifier rawness out of range")? {
            0 => tt::IdentIsRaw::No,
            _ => tt::IdentIsRaw::Yes,
        })
    }
}

impl Code for tt::DelimiterKind {
    fn put(&self, w: &mut Words) {
        w.u32(*self as u32);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(4, "a delimiter kind out of range")? {
            0 => tt::DelimiterKind::Parenthesis,
            1 => tt::DelimiterKind::Brace,
            2 => tt::DelimiterKind::Bracket,
            _ => tt::DelimiterKind::Invisible,
        })
    }
}

impl Code for tt::TokenTree {
    fn put(&self, w: &mut Words) {
        match self {
            tt::TokenTree::Subtree(subtree) => {
                w.u32(0);
                subtree.delimiter.open.put(w);
                subtree.delimiter.close.put(w);
                subtree.delimiter.kind.put(w);
                w.u32(subtree.len);
            }
            tt::TokenTree::Leaf(tt::Leaf::Literal(literal)) => {
                w.u32(1);
                literal.put(w);
            }
            tt::TokenTree::Leaf(tt::Leaf::Punct(punct)) => {
                w.u32(2);
                punct.char.put(w);
                punct.spacing.put(w);
                punct.span.put(w);
            }
            tt::TokenTree::Leaf(tt::Leaf::Ident(ident)) => {
                w.u32(3);
                w.symbol(&ident.sym);
                ident.span.put(w);
                ident.is_raw.put(w);
            }
        }
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        Ok(match r.tag(4, "a token tree out of range")? {
            0 => {
                let open = r.get()?;
                let close = r.get()?;
                let kind = r.get()?;
                let delimiter = tt::Delimiter { open, close, kind };
                tt::TokenTree::Subtree(tt::Subtree { delimiter, len: r.u32()? })
            }
            1 => tt::TokenTree::Leaf(tt::Leaf::Literal(r.get()?)),
            2 => {
                let char = r.get()?;
                let spacing = r.get()?;
                tt::TokenTree::Leaf(tt::Leaf::Punct(tt::Punct { char, spacing, span: r.get()? }))
            }
            _ => {
                let sym = r.symbol()?;
                let span = r.get()?;
                tt::TokenTree::Leaf(tt::Leaf::Ident(tt::Ident { sym, span, is_raw: r.get()? }))
            }
        })
    }
}

/// A token tree is its flat tokens, top subtree first, and whether the builder that made it
/// dropped an invisible top of its own (`build_skip_top_subtree`, as `syntax_bridge` builds)
/// or kept it (`build`): the two fill the tree's tables in different orders.
impl Code for tt::TopSubtree {
    fn put(&self, w: &mut Words) {
        // alloc: once a store write, the edge
        let tokens: Vec<tt::TokenTree> = self.as_token_trees().iter_flat_tokens().collect();
        // A builder's own top is invisible, so a visible top was the first subtree in it.
        let visible = matches!(
            &tokens[0],
            tt::TokenTree::Subtree(top) if top.delimiter.kind != tt::DelimiterKind::Invisible
        );
        let skip = visible || rebuild(&tokens, false) != *self;
        if skip {
            assert!(
                rebuild(&tokens, true) == *self,
                "a token tree no `TopSubtreeBuilder` calls build again: {self:?}"
            );
        }
        w.bool(skip);
        w.seq(tokens.iter());
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        let skip = r.bool()?;
        let tokens: Vec<tt::TokenTree> = r.seq::<tt::TokenTree, _>()?;
        check_tokens(&tokens).map_err(|what| r.invalid(what))?;
        Ok(rebuild(&tokens, skip))
    }
}

/// Whether `tokens` are a token tree: a top subtree holding all the rest, each subtree within
/// the one holding it.
fn check_tokens(tokens: &[tt::TokenTree]) -> Result<(), &'static str> {
    let Some(tt::TokenTree::Subtree(top)) = tokens.first() else {
        return Err("a token tree that does not start with a subtree");
    };
    if top.usize_len() != tokens.len() - 1 {
        return Err("a top subtree that does not hold every token after it");
    }
    let mut ends = vec![tokens.len()]; // alloc: once a token tree read from the store, the edge
    for (at, token) in tokens.iter().enumerate().skip(1) {
        while ends.last() == Some(&at) {
            ends.pop();
        }
        if let tt::TokenTree::Subtree(subtree) = token {
            let end = at + 1 + subtree.usize_len();
            if ends.last().is_none_or(|&outer| end > outer) {
                return Err("a subtree that ends past the one holding it");
            }
            ends.push(end);
        }
    }
    Ok(())
}

/// The token tree of `tokens` (checked by [`check_tokens`]), built with the calls that made it.
fn rebuild(tokens: &[tt::TokenTree], skip: bool) -> tt::TopSubtree {
    let tt::TokenTree::Subtree(top) = &tokens[0] else {
        unreachable!("a checked token tree starts with a subtree");
    };
    let mut builder = match skip {
        true => {
            let mut builder =
                tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(top.delimiter.open));
            builder.open(top.delimiter.kind, top.delimiter.open);
            builder
        }
        false => tt::TopSubtreeBuilder::new(top.delimiter),
    };
    // alloc: once a token tree read from the store, the edge
    let mut open: Vec<(usize, Span)> = Vec::new();
    for (at, token) in tokens.iter().enumerate().skip(1) {
        while let Some(&(end, close)) = open.last()
            && end == at
        {
            builder.close(close);
            open.pop();
        }
        match token {
            tt::TokenTree::Subtree(subtree) => {
                builder.open(subtree.delimiter.kind, subtree.delimiter.open);
                open.push((at + 1 + subtree.usize_len(), subtree.delimiter.close));
            }
            // clone: allocates nothing, a leaf's text is an interned symbol
            tt::TokenTree::Leaf(leaf) => builder.push(leaf.clone()),
        }
    }
    while let Some((_, close)) = open.pop() {
        builder.close(close);
    }
    match skip {
        true => {
            builder.close(top.delimiter.close);
            builder.build_skip_top_subtree()
        }
        false => builder.build(),
    }
}

/// Writes `tree`, reads it back in place and decodes it: the same tree, the same top level,
/// and the same bytes when written again.
#[cfg(test)]
pub(crate) fn assert_round_trip(tree: &ItemTree) {
    let bytes = blaze_format::to_bytes(tree);
    let buffer = blaze_format::Aligned::copy_from(&bytes);
    let read = ItemTree::read(&buffer).expect("an item tree just written");
    assert_eq!(read.top_level().collect::<Vec<_>>(), tree.top_level().to_vec());
    assert_eq!(read.len(), tree.kinds().len());
    for index in 0..tree.kinds().len() as u32 {
        let item = ModItemId { index };
        assert_eq!(read.kind(item), Some(tree.kinds()[index as usize]));
        assert_eq!(read.ast_id(item), Some(item.ast_id(tree).erase()));
    }
    let past = ModItemId { index: tree.kinds().len() as u32 };
    assert_eq!((read.kind(past), read.ast_id(past)), (None, None));
    for (&kind, &data) in tree.kinds().iter().zip(tree.data()) {
        let list = match kind {
            ItemKind::Mod if tree.mods()[data as usize].items != IdRange::NONE => {
                tree.mods()[data as usize].items
            }
            ItemKind::ExternBlock => tree.extern_blocks()[data as usize].children,
            _ => continue,
        };
        assert_eq!(read.children(list), Some(tree.items(list)));
    }
    assert_eq!(read.children(IdRange { start: 0, len: tree.children().len() as u32 + 1 }), None);
    let decoded = read.decode().expect("an item tree just written");
    assert_eq!(decoded, *tree);
    assert_eq!(blaze_format::to_bytes(&decoded), bytes, "an item tree written twice differs");
}

#[cfg(test)]
mod tests {
    use span::{
        Edition, EditionedFileId, ErasedFileAstId, FileId, ROOT_ERASED_FILE_AST_ID, SpanAnchor,
        SyntaxContext, TextRange, TextSize,
    };
    use hir_expand::fixture;

    use super::*;

    fn round_trip_fixture(fixture: &str) {
        let fx = fixture::db(fixture);
        let file_id = fx.only_file();
        let db = fx.into_db();
        let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
        assert_round_trip(tree);
    }

    #[test]
    fn every_item_kind_round_trips() {
        round_trip_fixture(
            r##"
//! crate docs
#![no_std]
#![feature(decl_macro)]

extern crate self as renamed;
pub(crate) extern crate alloc as _;
pub(super) extern crate core;

pub use crate::path::{nested, items as renamed2, Trait as _, self};
use globs::*;
use ::*;
use a::{c, d::{e, f::*}, {g}};
pub(in crate::m) use self::m::inner;
pub(self) use super::up;

#[on_extern_block]
extern "C" {
    #[on_extern_type]
    pub type ExType;
    pub static EX_STATIC: u8;
    fn ex_fn();
    m!();
}

#[derive(Debug, Clone)]
pub struct Record { pub a: u8, b: u8 }
pub struct Tuple(pub u8, pub(crate) u16);
pub struct MinVis(pub(in crate::a) u8, pub(in crate::b) u8);
pub struct OneVis(pub(in crate::a) u8, pub(in crate::a) u16);
pub(crate) struct Unit;
pub(self) struct PrivateTuple(u8);
union U { a: u8, b: u16 }
pub enum E { A, B(u8), C { c: u8 } }
const C: u8 = 0;
const _: () = ();
pub static mut S: u8 = 0;
pub trait Tr { fn method(&self); }
impl Tr for () { fn method(&self) {} }
impl Record {}
pub(super) type Alias = u8;

/// outer docs
mod inline {
    //! inner docs
    use super::*;
    fn in_module() {}
    mod nested { pub(super) fn deeper() {} }
}
pub mod outline;

macro_rules! m { () => {}; }
pub macro m2() {}
m!();
self::m!();

#[custom(a, "b", 1u8, 1.5f32, 'c', b'd', b"e", c"f", r#"g"#, br"h", r#type, 'lt, -1, a::b, (x) [y] {z}, é)]
#[custom = "literal"]
#[custom = 42]
#[cfg_attr(all(), cfg_attr_custom(x))]
#[cfg_attr(any(), never)]
fn attributed() {}

#[cfg(false)]
fn disabled() {}

#[cfg(all())]
#[before]
#[cfg(any(not(all()), target_os = "linux"))]
#[after]
struct DisabledLater;
"##,
        );
    }

    #[test]
    fn a_configured_out_file_round_trips() {
        round_trip_fixture(
            r#"
//- /lib.rs crate:foo crate-attr:no_std crate-attr:features(f16)
#![cfg(false)]
#![no_core]
fn f() {}
"#,
        );
    }

    #[test]
    fn an_empty_tree_round_trips() {
        assert_round_trip(&ItemTree::default());
    }

    blaze_format::record! {
        #[derive(Clone, Copy)]
        struct TestRoot: 1 {
            words: WordsRec,
            first: u32,
        }
    }

    fn round_trip<T: Code + PartialEq + std::fmt::Debug>(value: &T) {
        let mut w = Words::new();
        let first = w.at();
        value.put(&mut w);
        let mut out = Writer::new(<TestRoot as Record>::SCHEMA);
        let placed = w.place(&mut out);
        let at = out.next::<TestRoot>();
        let root = out.push(&TestRoot {
            words: placed.rec(at + offset_of!(TestRoot, words) as u64),
            first,
        });
        let buffer = blaze_format::Aligned::copy_from(&out.finish(root));
        let root = blaze_format::open::<TestRoot>(&buffer).expect("a buffer just written");
        let mut r = Read::new(&root.words, &buffer).from(root.first);
        assert_eq!(&r.get::<T>().expect("a value just written"), value);
    }

    fn span(start: u32, len: u32, ast_id: u32) -> Span {
        let file_id = EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT);
        Span {
            range: TextRange::at(TextSize::new(start), TextSize::new(len)),
            anchor: SpanAnchor { file_id, ast_id: ErasedFileAstId::from_raw(ast_id) },
            ctx: SyntaxContext::root(Edition::CURRENT),
        }
    }

    #[test]
    fn values_only_the_item_tree_holds_round_trip() {
        round_trip(&FieldsShape::Tuple);
        round_trip(&FieldsShape::Unit);
        for expand_to in [
            ExpandTo::Statements,
            ExpandTo::Items,
            ExpandTo::Pattern,
            ExpandTo::Type,
            ExpandTo::Expr,
        ] {
            round_trip(&expand_to);
            assert_eq!(expand_to_of(expand_to_word(expand_to)), Some(expand_to));
        }
        assert_eq!(expand_to_of(5), None);
        round_trip(&VisibilityExplicitness::Implicit);
        round_trip(&ImportAlias::Underscore);
        round_trip(&RawVisibility::PubCrate);
        round_trip(&RawVisibility::Public);
        round_trip(&RawVisibility::PubSelf(VisibilityExplicitness::Explicit));
        for kind in [
            tt::LitKind::Byte,
            tt::LitKind::StrRaw(3),
            tt::LitKind::ByteStrRaw(0),
            tt::LitKind::CStrRaw(255),
            tt::LitKind::Err(()),
        ] {
            round_trip(&tt::Literal::new("1", span(0, 4, 0), kind, "u8"));
        }
    }

    #[test]
    fn a_parsed_token_tree_rebuilds_equal() {
        let anchor = SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(1), Edition::CURRENT),
            ast_id: ROOT_ERASED_FILE_AST_ID,
        };
        let tree = syntax_bridge::parse_to_token_tree(
            Edition::CURRENT,
            anchor,
            SyntaxContext::root(Edition::CURRENT),
            r#"a (b, c) [d] {e {f}} 'g 1.0 r#raw "s" b'x' => -1u8 ::"#,
        )
        .expect("tokens that lex");
        round_trip(&tree);
    }

    #[test]
    fn a_token_tree_with_an_invisible_top_it_dropped_rebuilds_equal() {
        let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(span(0, 0, 0)));
        builder.open(tt::DelimiterKind::Invisible, span(0, 9, 1));
        builder.push(tt::Leaf::Ident(tt::Ident::new("x", span(0, 1, 1))));
        builder.open(tt::DelimiterKind::Parenthesis, span(2, 1, 2));
        builder.push(tt::Leaf::Punct(tt::Punct {
            char: '+',
            spacing: tt::Spacing::Joint,
            span: span(3, 1, 2),
        }));
        builder.close(span(4, 1, 2));
        builder.close(span(9, 0, 1));
        round_trip(&builder.build_skip_top_subtree());
    }

    #[test]
    fn a_token_tree_of_many_spans_and_symbols_rebuilds_equal() {
        // More span parts than 15 and symbols than 63: the tables sorted by frequency.
        let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(span(0, 0, 0)));
        builder.open(tt::DelimiterKind::Brace, span(0, 1, 0));
        for index in 0..200u32 {
            if index % 17 == 0 {
                builder.open(tt::DelimiterKind::Bracket, span(index * 4, 1, index % 23));
            }
            let name = format!("ident{}", index % 80);
            builder.push(tt::Leaf::Ident(tt::Ident::new(&name, span(index * 4 + 1, 2, index % 29))));
            if index % 17 == 16 {
                builder.close(span(index * 4 + 3, 1, index % 31));
            }
        }
        builder.close(span(1000, 1, 0));
        builder.close(span(1001, 1, 0));
        let tree = builder.build_skip_top_subtree();
        round_trip(&tree);

        let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(span(0, 0, 3)));
        for index in 0..100u32 {
            let name = format!("sym{index}");
            builder.push(tt::Leaf::Ident(tt::Ident::new(&name, span(index, 1, index % 40))));
        }
        round_trip(&builder.build());
    }

    /// `bytes` with the `u32` at byte `at` set to `word`, aligned to be read.
    fn patched(bytes: &[u8], at: usize, word: u32) -> blaze_format::Aligned {
        let mut bytes = bytes.to_vec();
        bytes[at..at + size_of::<u32>()].copy_from_slice(&word.to_le_bytes());
        blaze_format::Aligned::copy_from(&bytes)
    }

    fn offset_in<T>(value: &T, buffer: &[u8]) -> usize {
        value as *const T as usize - buffer.as_ptr() as usize
    }

    /// `bytes` with the `u32` at byte `field` of `record` (which is in `buffer`) set to `word`.
    fn patched_field<T>(
        bytes: &[u8],
        buffer: &[u8],
        record: &T,
        field: usize,
        word: u32,
    ) -> blaze_format::Aligned {
        patched(bytes, offset_in(record, buffer) + field, word)
    }

    /// Whether a buffer that opens is refused on decode.
    fn refused(buffer: &blaze_format::Aligned) -> bool {
        ItemTree::read(buffer).expect("ids are checked on decode").decode().is_err()
    }

    #[test]
    fn a_corrupted_buffer_is_refused() {
        let fx = fixture::db(
            r#"
extern "C" { fn f(); }
pub(in crate::m) fn g() {}
use a::{b, c};
"#,
        );
        let file_id = fx.only_file();
        let db = fx.into_db();
        let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
        assert_round_trip(tree);
        let bytes = blaze_format::to_bytes(tree);
        let buffer = blaze_format::Aligned::copy_from(&bytes);
        let root = blaze_format::open_as::<Root>(&buffer, ItemTree::SCHEMA).unwrap();

        // An item's kind out of range.
        let kinds = root.kinds.get(&buffer);
        let mut bad = bytes.clone();
        bad[offset_in(&kinds[0], &buffer)] = ItemKind::ALL.len() as u8;
        assert!(refused(&blaze_format::Aligned::copy_from(&bad)));

        // An item past the column of its kind.
        let data = root.data.get(&buffer);
        assert!(refused(&patched_field(&bytes, &buffer, &data[0], 0, 1000)));

        // The extern block's list of children past the array.
        let block = &root.extern_blocks.get(&buffer)[0];
        let len = offset_of!(ExternBlock, children) + offset_of!(IdRange, len);
        assert!(refused(&patched_field(&bytes, &buffer, block, len, u32::MAX)));

        // An extern block child that is not an item of the tree, or not before the block.
        let children = root.children.get(&buffer);
        assert!(refused(&patched_field(&bytes, &buffer, &children[0], 0, 1000)));
        let block_item = (0..kinds.len())
            .find(|&at| kinds[at] == ItemKind::ExternBlock as u8)
            .expect("the extern block");
        assert!(refused(&patched_field(&bytes, &buffer, &children[0], 0, block_item as u32)));

        // A function's visibility and name that are not in the tables.
        let functions = root.functions.get(&buffer);
        let g = functions.iter().rfind(|it| !it.visibility.is_builtin()).expect("`g`");
        let vis = offset_of!(Function, visibility);
        assert!(refused(&patched_field(&bytes, &buffer, g, vis, 7)));
        assert!(refused(&patched_field(&bytes, &buffer, g, offset_of!(Function, name), 1000)));

        // A use tree holding itself.
        let use_trees = root.use_trees.get(&buffer);
        let lists = root.use_tree_lists.get(&buffer);
        let parent = use_trees.len() as u32 - 1;
        assert_eq!(use_trees[parent as usize].kind, UseTree::PREFIXED);
        assert!(refused(&patched_field(&bytes, &buffer, &lists[0], 0, parent)));

        // A top-level item past the arrays reads in place, and is refused on decode.
        let top = root.top_level.get(&buffer);
        let bad = patched_field(&bytes, &buffer, &top[0], 0, 999);
        let read = ItemTree::read(&bad).unwrap();
        assert_eq!(read.top_level().next(), Some(ModItemId { index: 999 }));
        assert!(read.decode().is_err());

        // A buffer of another schema.
        let mut other = bytes.clone();
        other[8] ^= 1;
        assert!(ItemTree::read(&blaze_format::Aligned::copy_from(&other)).is_err());
    }

    #[test]
    fn a_corrupted_token_tree_is_refused() {
        // A crate on `std`, so the fixture appends no `no_core` or `no_std` to its attributes.
        let fx = fixture::db(
            "//- /main.rs crate:main deps:std\n#[custom(a, b)] fn f() {}\n//- /std.rs crate:std\n",
        );
        let file_id = fx.file_at("/main.rs");
        let db = fx.into_db();
        let tree = crate::file_item_tree(&db, file_id.into(), fixture::test_crate(&db));
        assert_round_trip(tree);
        let bytes = blaze_format::to_bytes(tree);
        let buffer = blaze_format::Aligned::copy_from(&bytes);
        let root = blaze_format::open_as::<Root>(&buffer, ItemTree::SCHEMA).unwrap();
        let words = root.words.words.get(&buffer);
        let word_at = |index: u32| offset_in(&words[index as usize], &buffer);
        // The words of the tree's attributes: their count, then the one attribute's path (its
        // kind, the count of segments, a segment), the input's option and kind, then the token
        // tree: whether it dropped a top, the count of tokens, the top subtree's tag.
        assert_eq!(words[root.attrs_at as usize], 1, "the one attribute: {:#?}", tree.attr_list());
        let count = root.attrs_at + 1 + 3 + 2 + 1;
        assert_eq!(words[count as usize], 4, "the flat tokens of `(a, b)`: ( a , b");
        // The top subtree: its tag, two spans of 5 words, its kind, then its length.
        let top_len = count + 1 + 1 + 2 * 5 + 1;
        assert_eq!(words[top_len as usize], 3);
        for (at, word) in [(count, u32::MAX), (count + 1, 4), (top_len, 9), (top_len, 2)] {
            let bad = patched(&bytes, word_at(at), word);
            assert!(ItemTree::read(&bad).unwrap().decode().is_err(), "word {at} set to {word}");
        }
    }
}
