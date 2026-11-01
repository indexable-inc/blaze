//! Describes items defined or visible (ie, imported) in a certain scope.
//! This is shared between modules and blocks.
//!
//! A scope keeps each namespace as one column of packed entries in the order names were added,
//! which is the order [`ItemScope::types`], [`ItemScope::values`] and [`ItemScope::macros`] list
//! and `frozen` writes. One open-addressed [`Index`] of `u32` positions, keyed by the hash of the
//! name's pointer, finds a name in all of them at once: [`ItemScope::get`] is one hash and one
//! probe run.

use std::{
    alloc::{Allocator, Global},
    fmt,
    hash::{BuildHasher, Hash},
    marker::PhantomData,
    sync::{LazyLock, OnceLock},
};

use base_db::{Crate, Db};
use blaze_hash::FixedState;
use blaze_ids::{bits::Bits, radix};
use blaze_mem::scratch;
use blaze_query::db::{AsId, FromId};
use either::Either;
use hir_expand::{AstId, MacroCallId, attrs::AttrId, name::Name};
use intern::sym;
use la_arena::{Idx, RawIdx};
use smallvec::SmallVec;
use span::Edition;
use stdx::{format_to, impl_from};
use syntax::ast;
use thin_vec::ThinVec;

use crate::{
    AdtId, BuiltinDeriveImplId, BuiltinType, ConstId, ExternBlockId, ExternCrateId, HasModule,
    ImplId, Lookup, MacroCallStyles, MacroId, ModuleDefId, ModuleId, TraitId, UseId,
    builtin_type::{BuiltinFloat, BuiltinInt, BuiltinUint},
    per_ns::{Item, Namespace, PerNs, TypesItem},
    visibility::{Visibility, VisibilityExplicitness},
};

/// The names of a scope being collected that a glob gave their binding, per namespace: a
/// build-time table, in the collector's frame or (for the facts engine's carried state) the heap.
#[derive(Clone)]
pub struct PerNsGlobImports<A: Allocator + Clone = Global> {
    types: hashbrown::HashSet<(ModuleId, Name), FixedState, A>,
    values: hashbrown::HashSet<(ModuleId, Name), FixedState, A>,
    macros: hashbrown::HashSet<(ModuleId, Name), FixedState, A>,
}

/// By the marks alone: `Global` is not `PartialEq`, so a derive would not cover the default.
impl<A: Allocator + Clone> PartialEq for PerNsGlobImports<A> {
    fn eq(&self, other: &Self) -> bool {
        self.types == other.types && self.values == other.values && self.macros == other.macros
    }
}

impl<A: Allocator + Clone> Eq for PerNsGlobImports<A> {}

impl<A: Allocator + Clone> fmt::Debug for PerNsGlobImports<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PerNsGlobImports")
            .field("types", &self.types)
            .field("values", &self.values)
            .field("macros", &self.macros)
            .finish()
    }
}

impl Default for PerNsGlobImports {
    fn default() -> Self {
        PerNsGlobImports::new_in(Global)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ImportOrExternCrate {
    Glob(GlobId),
    Import(ImportId),
    ExternCrate(ExternCrateId),
}

impl_from!(ImportOrGlob { Glob, Import } for ImportOrExternCrate);

impl ImportOrExternCrate {
    pub fn import_or_glob(self) -> Option<ImportOrGlob> {
        match self {
            ImportOrExternCrate::Import(it) => Some(ImportOrGlob::Import(it)),
            ImportOrExternCrate::Glob(it) => Some(ImportOrGlob::Glob(it)),
            _ => None,
        }
    }

    pub fn import(self) -> Option<ImportId> {
        match self {
            ImportOrExternCrate::Import(it) => Some(it),
            _ => None,
        }
    }

    pub fn glob(self) -> Option<GlobId> {
        match self {
            ImportOrExternCrate::Glob(id) => Some(id),
            _ => None,
        }
    }

    pub fn use_(self) -> Option<UseId> {
        match self {
            ImportOrExternCrate::Glob(id) => Some(id.use_),
            ImportOrExternCrate::Import(id) => Some(id.use_),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ImportOrGlob {
    Glob(GlobId),
    Import(ImportId),
}

impl ImportOrGlob {
    pub fn into_import(self) -> Option<ImportId> {
        match self {
            ImportOrGlob::Import(it) => Some(it),
            _ => None,
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ImportOrDef {
    Import(ImportId),
    Glob(GlobId),
    ExternCrate(ExternCrateId),
    Def(ModuleDefId),
}

impl_from!(ImportOrExternCrate { Import, Glob, ExternCrate } for ImportOrDef);
impl_from!(ImportOrGlob { Import, Glob } for ImportOrDef);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct ImportId {
    pub use_: UseId,
    pub idx: Idx<ast::UseTree>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub struct GlobId {
    pub use_: UseId,
    pub idx: Idx<ast::UseTree>,
}

impl<A: Allocator + Clone> PerNsGlobImports<A> {
    pub(crate) fn new_in(alloc: A) -> Self {
        PerNsGlobImports {
            // clone: the allocator handle: a frame reference or the zero-sized `Global`
            types: hashbrown::HashSet::with_hasher_in(FixedState, alloc.clone()),
            // clone: the allocator handle: a frame reference or the zero-sized `Global`
            values: hashbrown::HashSet::with_hasher_in(FixedState, alloc.clone()),
            macros: hashbrown::HashSet::with_hasher_in(FixedState, alloc),
        }
    }

    /// The glob marks of `scope`, the scope of `module`, sized to them.
    pub(crate) fn of_scope_in(module: ModuleId, scope: &ItemScope, alloc: A) -> Self {
        let types = || {
            scope.types.iter().filter(|(_, it)| matches!(it.import, Some(ImportOrExternCrate::Glob(_))))
        };
        let values =
            || scope.values.iter().filter(|(_, it)| matches!(it.import, Some(ImportOrGlob::Glob(_))));
        let macros = || {
            scope.macros.iter().filter(|(_, it)| matches!(it.import, Some(ImportOrExternCrate::Glob(_))))
        };
        let set = |len: usize, alloc: A| {
            hashbrown::HashSet::with_capacity_and_hasher_in(len, FixedState, alloc)
        };
        let mut marks = PerNsGlobImports {
            // clone: the allocator handle: a frame reference or the zero-sized `Global`
            types: set(types().count(), alloc.clone()),
            // clone: the allocator handle: a frame reference or the zero-sized `Global`
            values: set(values().count(), alloc.clone()),
            macros: set(macros().count(), alloc),
        };
        marks.types.extend(types().map(|(name, _)| (module, *name)));
        marks.values.extend(values().map(|(name, _)| (module, *name)));
        marks.macros.extend(macros().map(|(name, _)| (module, *name)));
        marks
    }

    pub(crate) fn contains_type(&self, module_id: ModuleId, name: Name) -> bool {
        self.types.contains(&(module_id, name))
    }
    pub(crate) fn contains_value(&self, module_id: ModuleId, name: Name) -> bool {
        self.values.contains(&(module_id, name))
    }
    pub(crate) fn contains_macro(&self, module_id: ModuleId, name: Name) -> bool {
        self.macros.contains(&(module_id, name))
    }
}

impl PerNsGlobImports {
    pub(crate) fn of_scope(module: ModuleId, scope: &ItemScope) -> PerNsGlobImports {
        PerNsGlobImports::of_scope_in(module, scope, Global)
    }
}

/// The namespaces of a scope, as its [`Index`] names them.
const TYPES: u32 = 0;
const VALUES: u32 = 1;
const MACROS: u32 = 2;
const UNRESOLVED: u32 = 3;

/// The position of a name in a namespace it is not in.
const NONE: u32 = u32::MAX;

/// A scope or map of at most this many keys is searched in order and has no [`Index`]: eight
/// pointer compares cost less than a hash and a probe, and most modules' maps are that small.
const LINEAR: usize = 8;

/// An open-addressed table of positions into a scope's columns: linear probing over a power of
/// two of `u32` slots, at most 3/4 full, so a probe run ends at an empty slot. A slot is the
/// namespace (2 bits), 6 bits of the key's hash and the position (24 bits): a probe compares a
/// key only where those 6 bits match, about one slot in 64 of another key. 4 bytes a key over
/// the load (5.3 to 10.7), against 9 for a SwissTable of `usize` indices (8 plus a control byte).
#[derive(Clone, Default)]
pub(crate) struct Index {
    slots: ThinVec<u32>,
}

const POS_BITS: u32 = 24;
const POS_MASK: u32 = (1 << POS_BITS) - 1;
/// The last position a slot holds, so no slot is [`EMPTY`].
const MAX_POS: u32 = POS_MASK - 1;
const EMPTY: u32 = u32::MAX;

#[inline]
fn hash_tag(hash: u64) -> u32 {
    (hash >> 58) as u32
}

/// The hash a name is indexed by: its symbol's pointer, one word, through
/// `blaze_hash::FixedState`, which mixes it into every bit, the low ones that pick the slot too.
#[inline]
fn name_hash(name: &Name) -> u64 {
    FixedState.hash_one(name)
}

impl Index {
    #[inline]
    fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Room for `len` keys.
    fn with_capacity(len: usize) -> Index {
        let slots = (len * 4).div_ceil(3).next_power_of_two().max(16);
        // alloc: the index's slots, once per scope of more than 8 names, when it is sealed
        Index { slots: std::iter::repeat_n(EMPTY, slots).collect() }
    }

    #[inline]
    fn fits(&self, len: usize) -> bool {
        len * 4 <= self.slots.len() * 3
    }

    /// Adds `pos` of namespace `ns`; the caller made sure it [`fits`](Self::fits).
    fn insert(&mut self, hash: u64, ns: u32, pos: usize) {
        assert!(pos <= MAX_POS as usize, "a scope with more than {MAX_POS} keys in one column");
        let mask = self.slots.len() - 1;
        let mut at = hash as usize & mask;
        while self.slots[at] != EMPTY {
            at = (at + 1) & mask;
        }
        self.slots[at] = ns << 30 | hash_tag(hash) << POS_BITS | pos as u32;
    }

    /// The namespace and position of each key whose hash may be `hash`.
    #[inline]
    fn probe(&self, hash: u64) -> Probe<'_> {
        let mask = self.slots.len().wrapping_sub(1);
        Probe { slots: &self.slots, at: hash as usize & mask, mask, tag: hash_tag(hash) }
    }
}

impl fmt::Debug for Index {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Index({} slots)", self.slots.len())
    }
}

pub(crate) struct Probe<'a> {
    slots: &'a [u32],
    at: usize,
    mask: usize,
    tag: u32,
}

impl Iterator for Probe<'_> {
    type Item = (u32, u32);

    #[inline]
    fn next(&mut self) -> Option<(u32, u32)> {
        loop {
            let slot = *self.slots.get(self.at)?;
            if slot == EMPTY {
                return None;
            }
            self.at = (self.at + 1) & self.mask;
            if (slot >> POS_BITS) & 0x3f == self.tag {
                return Some((slot >> 30, slot & POS_MASK));
            }
        }
    }
}

/// One name of one namespace and its [`Item`], packed as tags and ids. Every field the
/// item does not use is `None` or 0, so two entries are equal exactly when their items are.
/// 24 bytes (an `IndexMap` bucket of a `TypesItem` is 48: its hash, the name and 32 bytes of
/// item); reading the item back is a `match` on each tag.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct Entry {
    name: Name,
    def: Option<blaze_query::db::Id>,
    vis: Option<blaze_query::db::Id>,
    import: Option<blaze_query::db::Id>,
    /// The use tree of a glob or an import.
    tree: u32,
    def_tag: u8,
    vis_tag: u8,
    import_tag: u8,
}

const _: () = assert!(size_of::<Option<blaze_query::db::Id>>() == 4);
const _: () = assert!(size_of::<Entry>() == 24);

const DEF_MODULE: u8 = 0;
const DEF_FUNCTION: u8 = 1;
const DEF_STRUCT: u8 = 2;
const DEF_UNION: u8 = 3;
const DEF_ENUM: u8 = 4;
const DEF_VARIANT: u8 = 5;
const DEF_CONST: u8 = 6;
const DEF_STATIC: u8 = 7;
const DEF_TRAIT: u8 = 8;
const DEF_TYPE_ALIAS: u8 = 9;
const DEF_MACRO2: u8 = 10;
const DEF_MACRO_RULES: u8 = 11;
const DEF_PROC_MACRO: u8 = 12;
/// A builtin type is this plus its place in [`BUILTIN_TYPES`], with no id.
const DEF_BUILTIN: u8 = 32;

/// The builtin types in the order of [`BuiltinType::all_builtin_types`].
const BUILTIN_TYPES: [BuiltinType; 19] = [
    BuiltinType::Char,
    BuiltinType::Bool,
    BuiltinType::Str,
    BuiltinType::Int(BuiltinInt::Isize),
    BuiltinType::Int(BuiltinInt::I8),
    BuiltinType::Int(BuiltinInt::I16),
    BuiltinType::Int(BuiltinInt::I32),
    BuiltinType::Int(BuiltinInt::I64),
    BuiltinType::Int(BuiltinInt::I128),
    BuiltinType::Uint(BuiltinUint::Usize),
    BuiltinType::Uint(BuiltinUint::U8),
    BuiltinType::Uint(BuiltinUint::U16),
    BuiltinType::Uint(BuiltinUint::U32),
    BuiltinType::Uint(BuiltinUint::U64),
    BuiltinType::Uint(BuiltinUint::U128),
    BuiltinType::Float(BuiltinFloat::F16),
    BuiltinType::Float(BuiltinFloat::F32),
    BuiltinType::Float(BuiltinFloat::F64),
    BuiltinType::Float(BuiltinFloat::F128),
];

fn builtin_index(ty: BuiltinType) -> u8 {
    match ty {
        BuiltinType::Char => 0,
        BuiltinType::Bool => 1,
        BuiltinType::Str => 2,
        BuiltinType::Int(it) => 3 + it as u8,
        BuiltinType::Uint(it) => 9 + it as u8,
        BuiltinType::Float(it) => 15 + it as u8,
    }
}

#[inline]
fn raw(it: impl AsId) -> Option<blaze_query::db::Id> {
    Some(it.as_id())
}

#[inline]
fn typed<T: FromId>(id: blaze_query::db::Id) -> T {
    T::from_id(id)
}

/// What an entry's definition is: a tag and, but for a builtin type, an id.
pub(crate) trait PackedDef: Copy + PartialEq {
    fn pack(self) -> (u8, Option<blaze_query::db::Id>);
    fn unpack(tag: u8, id: Option<blaze_query::db::Id>) -> Self;
}

impl PackedDef for MacroId {
    #[inline]
    fn pack(self) -> (u8, Option<blaze_query::db::Id>) {
        match self {
            MacroId::Macro2Id(it) => (DEF_MACRO2, raw(it)),
            MacroId::MacroRulesId(it) => (DEF_MACRO_RULES, raw(it)),
            MacroId::ProcMacroId(it) => (DEF_PROC_MACRO, raw(it)),
        }
    }

    #[inline]
    fn unpack(tag: u8, id: Option<blaze_query::db::Id>) -> Self {
        match (tag, id) {
            (DEF_MACRO2, Some(id)) => MacroId::Macro2Id(typed(id)),
            (DEF_MACRO_RULES, Some(id)) => MacroId::MacroRulesId(typed(id)),
            (DEF_PROC_MACRO, Some(id)) => MacroId::ProcMacroId(typed(id)),
            _ => unreachable!("a macro entry with definition tag {tag}"),
        }
    }
}

impl PackedDef for ModuleDefId {
    #[inline]
    fn pack(self) -> (u8, Option<blaze_query::db::Id>) {
        match self {
            ModuleDefId::ModuleId(it) => (DEF_MODULE, raw(it)),
            ModuleDefId::FunctionId(it) => (DEF_FUNCTION, raw(it)),
            ModuleDefId::AdtId(AdtId::StructId(it)) => (DEF_STRUCT, raw(it)),
            ModuleDefId::AdtId(AdtId::UnionId(it)) => (DEF_UNION, raw(it)),
            ModuleDefId::AdtId(AdtId::EnumId(it)) => (DEF_ENUM, raw(it)),
            ModuleDefId::EnumVariantId(it) => (DEF_VARIANT, raw(it)),
            ModuleDefId::ConstId(it) => (DEF_CONST, raw(it)),
            ModuleDefId::StaticId(it) => (DEF_STATIC, raw(it)),
            ModuleDefId::TraitId(it) => (DEF_TRAIT, raw(it)),
            ModuleDefId::TypeAliasId(it) => (DEF_TYPE_ALIAS, raw(it)),
            ModuleDefId::BuiltinType(it) => (DEF_BUILTIN + builtin_index(it), None),
            ModuleDefId::MacroId(it) => it.pack(),
        }
    }

    #[inline]
    fn unpack(tag: u8, id: Option<blaze_query::db::Id>) -> Self {
        let Some(id) = id else {
            return ModuleDefId::BuiltinType(BUILTIN_TYPES[usize::from(tag - DEF_BUILTIN)]);
        };
        match tag {
            DEF_MODULE => ModuleDefId::ModuleId(typed(id)),
            DEF_FUNCTION => ModuleDefId::FunctionId(typed(id)),
            DEF_STRUCT => ModuleDefId::AdtId(AdtId::StructId(typed(id))),
            DEF_UNION => ModuleDefId::AdtId(AdtId::UnionId(typed(id))),
            DEF_ENUM => ModuleDefId::AdtId(AdtId::EnumId(typed(id))),
            DEF_VARIANT => ModuleDefId::EnumVariantId(typed(id)),
            DEF_CONST => ModuleDefId::ConstId(typed(id)),
            DEF_STATIC => ModuleDefId::StaticId(typed(id)),
            DEF_TRAIT => ModuleDefId::TraitId(typed(id)),
            DEF_TYPE_ALIAS => ModuleDefId::TypeAliasId(typed(id)),
            DEF_MACRO2 | DEF_MACRO_RULES | DEF_PROC_MACRO => {
                ModuleDefId::MacroId(MacroId::unpack(tag, Some(id)))
            }
            _ => unreachable!("an entry with definition tag {tag}"),
        }
    }
}

const VIS_EXPLICIT: u8 = 0;
const VIS_IMPLICIT: u8 = 1;
const VIS_CRATE: u8 = 2;
const VIS_PUBLIC: u8 = 3;

#[inline]
fn pack_vis(vis: Visibility) -> (u8, Option<blaze_query::db::Id>) {
    match vis {
        Visibility::Module(it, VisibilityExplicitness::Explicit) => (VIS_EXPLICIT, raw(it)),
        Visibility::Module(it, VisibilityExplicitness::Implicit) => (VIS_IMPLICIT, raw(it)),
        Visibility::PubCrate(it) => (VIS_CRATE, raw(it)),
        Visibility::Public => (VIS_PUBLIC, None),
    }
}

#[inline]
fn unpack_vis(tag: u8, id: Option<blaze_query::db::Id>) -> Visibility {
    match (tag, id) {
        (VIS_EXPLICIT, Some(id)) => Visibility::Module(typed(id), VisibilityExplicitness::Explicit),
        (VIS_IMPLICIT, Some(id)) => Visibility::Module(typed(id), VisibilityExplicitness::Implicit),
        (VIS_CRATE, Some(id)) => Visibility::PubCrate(typed(id)),
        (VIS_PUBLIC, None) => Visibility::Public,
        _ => unreachable!("an entry with visibility tag {tag}"),
    }
}

const IMPORT_NONE: u8 = 0;
const IMPORT_GLOB: u8 = 1;
const IMPORT_USE: u8 = 2;
const IMPORT_EXTERN_CRATE: u8 = 3;

#[inline]
fn tree_index(idx: Idx<ast::UseTree>) -> u32 {
    idx.into_raw().into_u32()
}

#[inline]
fn tree_at(raw: u32) -> Idx<ast::UseTree> {
    Idx::from_raw(RawIdx::from_u32(raw))
}

/// What an entry was imported by: a tag, an id and a use tree.
pub(crate) trait PackedImport: Copy + PartialEq {
    fn pack(import: Option<Self>) -> (u8, Option<blaze_query::db::Id>, u32);
    fn unpack(tag: u8, id: Option<blaze_query::db::Id>, tree: u32) -> Option<Self>;
}

impl PackedImport for ImportOrExternCrate {
    #[inline]
    fn pack(import: Option<Self>) -> (u8, Option<blaze_query::db::Id>, u32) {
        match import {
            None => (IMPORT_NONE, None, 0),
            Some(ImportOrExternCrate::Glob(GlobId { use_, idx })) => {
                (IMPORT_GLOB, raw(use_), tree_index(idx))
            }
            Some(ImportOrExternCrate::Import(ImportId { use_, idx })) => {
                (IMPORT_USE, raw(use_), tree_index(idx))
            }
            Some(ImportOrExternCrate::ExternCrate(it)) => (IMPORT_EXTERN_CRATE, raw(it), 0),
        }
    }

    #[inline]
    fn unpack(tag: u8, id: Option<blaze_query::db::Id>, tree: u32) -> Option<Self> {
        let id = id?;
        Some(match tag {
            IMPORT_GLOB => ImportOrExternCrate::Glob(GlobId { use_: typed(id), idx: tree_at(tree) }),
            IMPORT_USE => {
                ImportOrExternCrate::Import(ImportId { use_: typed(id), idx: tree_at(tree) })
            }
            IMPORT_EXTERN_CRATE => ImportOrExternCrate::ExternCrate(typed(id)),
            _ => unreachable!("an entry with import tag {tag}"),
        })
    }
}

impl PackedImport for ImportOrGlob {
    #[inline]
    fn pack(import: Option<Self>) -> (u8, Option<blaze_query::db::Id>, u32) {
        match import {
            None => (IMPORT_NONE, None, 0),
            Some(ImportOrGlob::Glob(GlobId { use_, idx })) => {
                (IMPORT_GLOB, raw(use_), tree_index(idx))
            }
            Some(ImportOrGlob::Import(ImportId { use_, idx })) => {
                (IMPORT_USE, raw(use_), tree_index(idx))
            }
        }
    }

    #[inline]
    fn unpack(tag: u8, id: Option<blaze_query::db::Id>, tree: u32) -> Option<Self> {
        let id = id?;
        Some(match tag {
            IMPORT_GLOB => ImportOrGlob::Glob(GlobId { use_: typed(id), idx: tree_at(tree) }),
            IMPORT_USE => ImportOrGlob::Import(ImportId { use_: typed(id), idx: tree_at(tree) }),
            _ => unreachable!("a value entry with import tag {tag}"),
        })
    }
}

impl Entry {
    #[inline]
    fn new<D: PackedDef, I: PackedImport>(name: Name, item: Item<D, I>) -> Entry {
        let (def_tag, def) = item.def.pack();
        let (vis_tag, vis) = pack_vis(item.vis);
        let (import_tag, import, tree) = I::pack(item.import);
        Entry { name, def, vis, import, tree, def_tag, vis_tag, import_tag }
    }

    #[inline]
    fn item<D: PackedDef, I: PackedImport>(&self) -> Item<D, I> {
        Item {
            def: D::unpack(self.def_tag, self.def),
            vis: self.vis(),
            import: I::unpack(self.import_tag, self.import, self.tree),
        }
    }

    #[inline]
    fn vis(&self) -> Visibility {
        unpack_vis(self.vis_tag, self.vis)
    }

    fn set_vis(&mut self, vis: Visibility) {
        (self.vis_tag, self.vis) = pack_vis(vis);
    }

    fn set_def<D: PackedDef>(&mut self, def: D) {
        (self.def_tag, self.def) = def.pack();
    }

    #[inline]
    fn is_def(&self, (tag, id): (u8, Option<blaze_query::db::Id>)) -> bool {
        self.def_tag == tag && self.def == id
    }

    #[inline]
    fn declared(&self) -> bool {
        self.import_tag == IMPORT_NONE
    }
}

/// One namespace of a scope, in the order its names were added.
pub(crate) struct Column<D, I> {
    entries: ThinVec<Entry>,
    of: PhantomData<fn() -> Item<D, I>>,
}

impl<D, I> Default for Column<D, I> {
    fn default() -> Self {
        Column { entries: ThinVec::new(), of: PhantomData }
    }
}

impl<D, I> Clone for Column<D, I> {
    fn clone(&self) -> Self {
        // clone: only with its ItemScope, which facts copies once per macro step
        Column { entries: self.entries.clone(), of: PhantomData }
    }
}

impl<D, I> PartialEq for Column<D, I> {
    fn eq(&self, other: &Self) -> bool {
        self.entries == other.entries
    }
}

impl<D, I> Eq for Column<D, I> {}

impl<D: PackedDef + fmt::Debug, I: PackedImport + fmt::Debug> fmt::Debug for Column<D, I> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

impl<D: PackedDef, I: PackedImport> Column<D, I> {
    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    fn at(&self, pos: usize) -> Item<D, I> {
        self.entries[pos].item()
    }

    #[inline]
    fn get(&self, pos: u32) -> Option<Item<D, I>> {
        self.entries.get(pos as usize).map(|it| it.item())
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (&Name, Item<D, I>)> + '_ {
        self.entries.iter().map(|it| (&it.name, it.item()))
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        self.entries.reserve(additional);
    }

    /// Appends a name `frozen` read; [`ItemScope::seal`] indexes it.
    pub(crate) fn push_unindexed(&mut self, name: Name, item: Item<D, I>) {
        self.entries.push(Entry::new(name, item));
    }
}

/// `v` again at exactly its length: one allocation for a non-empty column, none for an empty one
/// or one already exact.
fn exact<T>(v: &mut ThinVec<T>) {
    if v.capacity() == v.len() {
        return;
    }
    // alloc: the memo's column, at its length
    let mut out = ThinVec::with_capacity(v.len());
    out.extend(v.drain(..));
    *v = out;
}

/// Keys and values in the order the keys were first added, with an [`Index`] once there are
/// more than [`LINEAR`]: `size_of::<(K, V)>()` plus 4 bytes of index a key over the load, and
/// none while small. Two are equal when they map the same keys to the same values in any order,
/// as the hash maps they replace were.
#[derive(Clone)]
pub(crate) struct Keyed<K, V> {
    entries: ThinVec<(K, V)>,
    index: Index,
}

impl<K, V> Default for Keyed<K, V> {
    fn default() -> Self {
        Keyed { entries: ThinVec::new(), index: Index::default() }
    }
}

impl<K: fmt::Debug, V: fmt::Debug> fmt::Debug for Keyed<K, V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map().entries(self.entries.iter().map(|(k, v)| (k, v))).finish()
    }
}

impl<K: Copy + Eq + Hash, V> Keyed<K, V> {
    #[inline]
    fn hash(key: &K) -> u64 {
        FixedState.hash_one(key)
    }

    #[inline]
    fn position(&self, key: &K) -> Option<usize> {
        if self.index.is_empty() {
            return self.entries.iter().position(|(it, _)| it == key);
        }
        (self.index.probe(Self::hash(key)))
            .map(|(_, pos)| pos as usize)
            .find(|&pos| self.entries[pos].0 == *key)
    }

    pub(crate) fn get(&self, key: &K) -> Option<&V> {
        self.position(key).map(|pos| &self.entries[pos].1)
    }

    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        let pos = self.position(key)?;
        Some(&mut self.entries[pos].1)
    }

    /// Maps `key` to `value`, in place of what it mapped to.
    fn insert(&mut self, key: K, value: V) {
        match self.position(&key) {
            Some(pos) => self.entries[pos].1 = value,
            None => self.push(key, value),
        }
    }

    fn or_insert_with(&mut self, key: K, value: impl FnOnce() -> V) -> &mut V {
        let pos = match self.position(&key) {
            Some(pos) => pos,
            None => {
                self.push(key, value());
                self.entries.len() - 1
            }
        };
        &mut self.entries[pos].1
    }

    /// Adds `key`, which is not there.
    fn push(&mut self, key: K, value: V) {
        let pos = self.entries.len();
        self.entries.push((key, value));
        let len = pos + 1;
        if len <= LINEAR {
            return;
        }
        if self.index.fits(len) {
            self.index.insert(Self::hash(&key), 0, pos);
        } else {
            self.reindex(2 * len);
        }
    }

    /// Indexes every key with room for `capacity`, and whether no key is there twice.
    fn reindex(&mut self, capacity: usize) -> bool {
        let entries = &self.entries;
        if entries.len() <= LINEAR {
            self.index = Index::default();
            return (1..entries.len()).all(|at| entries[..at].iter().all(|it| it.0 != entries[at].0));
        }
        let mut index = Index::with_capacity(capacity);
        let mut unique = true;
        for (pos, (key, _)) in entries.iter().enumerate() {
            let hash = Self::hash(key);
            unique &= !index.probe(hash).any(|(_, at)| entries[at as usize].0 == *key);
            index.insert(hash, 0, pos);
        }
        self.index = index;
        unique
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn iter(&self) -> impl ExactSizeIterator<Item = (&K, &V)> + '_ {
        self.entries.iter().map(|(k, v)| (k, v))
    }

    fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.entries.iter().map(|(k, _)| *k)
    }

    fn values(&self) -> impl Iterator<Item = &V> + '_ {
        self.entries.iter().map(|(_, v)| v)
    }

    /// The map again at its length, indexed with no spare room.
    fn settle(&mut self) {
        exact(&mut self.entries);
        let unique = self.reindex(self.entries.len());
        debug_assert!(unique, "a map of a scope with a key twice");
    }

    pub(crate) fn reserve(&mut self, additional: usize) {
        self.entries.reserve(additional);
    }

    /// Appends an entry `frozen` read; [`ItemScope::seal`] indexes it.
    pub(crate) fn push_unindexed(&mut self, key: K, value: V) {
        self.entries.push((key, value));
    }
}

impl<K: Copy + Eq + Hash, V: PartialEq> PartialEq for Keyed<K, V> {
    fn eq(&self, other: &Self) -> bool {
        self.entries.len() == other.entries.len()
            && self.entries.iter().all(|(k, v)| other.get(k) == Some(v))
    }
}

impl<K: Copy + Eq + Hash, V: Eq> Eq for Keyed<K, V> {}

/// A bit per position of a list beside it, set by `set_growing` as the list grows, so it has
/// the list's words and two of the same length are equal when their words are.
pub(crate) type ListBits = Bits<ThinVec<u64>>;

/// The items defined in or imported into a module or block.
///
/// Each namespace is a [`Column`] of 40-byte entries in the order names were added, and
/// `unresolved` a column of names; one [`Index`] finds a name in all four, so a lookup is one
/// hash of the name's pointer and one probe run, which compares a name only where 6 bits of its
/// hash match. About 47 bytes a name per namespace (40 plus the index's 4 over its load) against
/// about 73 for the `FxIndexMap` it replaces (a 60-byte bucket plus 9 bytes of table over the
/// load), and one allocation per column instead of two.
///
/// Two scopes are equal when their namespaces hold the same entries in the same order (the
/// order [`ItemScope::types`] and the others show, so a scope that kept another's order when
/// the query database backdated it would list differently) and the rest the same, `unresolved` and the maps
/// in any order. The index and `sorted` are derived and not compared.
#[derive(Debug, Default, Clone)]
pub struct ItemScope {
    /// Defs visible in this scope. This includes `declarations`, but also
    /// imports. The imports belong to this module and can be resolved by using them on
    /// the `use_imports_*` fields.
    pub(crate) types: Column<ModuleDefId, ImportOrExternCrate>,
    pub(crate) values: Column<ModuleDefId, ImportOrGlob>,
    pub(crate) macros: Column<MacroId, ImportOrExternCrate>,
    pub(crate) unresolved: ThinVec<Name>,
    /// Where each name of the four columns above is; empty while there are at most [`LINEAR`].
    pub(crate) names: Index,
    /// Every name of the scope once, by text, as `namespace << 30 | position`: what
    /// [`ItemScope::entries`] lists. Made by the first `entries` after a name was added, and
    /// written only that once for a scope that is handed over.
    pub(crate) sorted: OnceLock<Box<[u32]>>,
    /// The entries of the three namespaces by def, as `namespace << 30 | position` sorted by
    /// namespace, def and position: what [`ItemScope::name_of`] searches in a scope of more than
    /// [`LINEAR`] names. 4 bytes an entry, made by the first `name_of` after a def changed.
    pub(crate) by_def: OnceLock<Box<[u32]>>,

    /// The defs declared in this scope. Each def has a single scope where it is
    /// declared.
    pub(crate) declarations: ThinVec<ModuleDefId>,

    /// The impls declared here, in declaration order.
    pub(crate) impls: ThinVec<ImplId>,
    /// A bit per impl of `impls`, by position: whether it is a trait impl.
    pub(crate) trait_impl: ListBits,
    /// A bit per impl of `impls`, by position: whether it is `#[rustc_reservation_impl]`.
    pub(crate) reservation_impl: ListBits,
    pub(crate) builtin_derive_impls: ThinVec<BuiltinDeriveImplId>,
    pub(crate) extern_blocks: ThinVec<ExternBlockId>,
    pub(crate) unnamed_consts: ThinVec<ConstId>,
    /// Traits imported via `use Trait as _;`, each with the binding's own import: that `use`,
    /// or the glob that brought another scope's unnamed trait in.
    pub(crate) unnamed_trait_imports: ThinVec<(TraitId, Item<(), ImportOrExternCrate>)>,
    /// Every `use path as _;` of this scope, in the order they resolved.
    pub(crate) underscore_imports: ThinVec<UnderscoreImport>,
    /// The names two globs give different items, in the order the second glob came.
    pub(crate) glob_ambiguities: ThinVec<GlobAmbiguity>,
    /// Single imports that lost their name to another binding, in the order they came.
    pub(crate) shadowed_imports: ThinVec<ShadowedImport>,

    /// The resolutions of the imports of this scope: the binding each import came through, or
    /// the def, walked by [`Self::fully_resolve_import`]. A glob has no entry: it binds many
    /// names, so one link per glob would hold whichever name was bound last, an order the old
    /// collector and the facts engine don't share, and the walk never reads it.
    pub(crate) use_imports_types: Keyed<ImportOrExternCrate, ImportOrDef>,
    pub(crate) use_imports_values: Keyed<ImportOrGlob, ImportOrDef>,
    pub(crate) use_imports_macros: Keyed<ImportOrExternCrate, ImportOrDef>,

    pub(crate) use_decls: ThinVec<UseId>,
    pub(crate) extern_crate_decls: ThinVec<ExternCrateId>,
    /// Macros visible in current module in legacy textual scope
    ///
    /// For macros invoked by an unqualified identifier like `bar!()`, `legacy_macros` will be searched in first.
    /// If it yields no result, then it turns to module scoped `macros`.
    /// It macros with name qualified with a path like `crate::foo::bar!()`, `legacy_macros` will be skipped,
    /// and only normal scoped `macros` will be searched in.
    ///
    /// Note that this automatically inherit macros defined textually before the definition of module itself.
    ///
    /// Module scoped macros will be inserted into `items` instead of here.
    // FIXME: Macro shadowing in one module is not properly handled. Non-item place macros will
    // be all resolved to the last one defined if shadowing happens.
    pub(crate) legacy_macros: Keyed<Name, SmallVec<[MacroId; 1]>>,
    /// The attribute macro invocations in this scope.
    pub(crate) attr_macros: Keyed<AstId<ast::Item>, MacroCallId>,
    /// The macro invocations in this scope.
    pub(crate) macro_invocations: Keyed<AstId<ast::MacroCall>, MacroCallId>,
    /// The derive macro invocations in this scope, keyed by the owner item over the actual derive attributes
    /// paired with the derive macro invocations for the specific attribute.
    pub(crate) derive_macros: Keyed<AstId<ast::Adt>, SmallVec<[DeriveMacroInvocation; 1]>>,
}

impl PartialEq for ItemScope {
    fn eq(&self, other: &Self) -> bool {
        let Self {
            types,
            values,
            macros,
            unresolved,
            names: _,
            sorted: _,
            by_def: _,
            declarations,
            impls,
            trait_impl,
            reservation_impl,
            builtin_derive_impls,
            extern_blocks,
            unnamed_consts,
            unnamed_trait_imports,
            underscore_imports,
            glob_ambiguities,
            shadowed_imports,
            use_imports_types,
            use_imports_values,
            use_imports_macros,
            use_decls,
            extern_crate_decls,
            legacy_macros,
            attr_macros,
            macro_invocations,
            derive_macros,
        } = self;
        *types == other.types
            && *values == other.values
            && *macros == other.macros
            && unresolved.len() == other.unresolved.len()
            && unresolved.iter().all(|it| other.position(UNRESOLVED, it).is_some())
            && *declarations == other.declarations
            && *impls == other.impls
            && *trait_impl == other.trait_impl
            && *reservation_impl == other.reservation_impl
            && *builtin_derive_impls == other.builtin_derive_impls
            && *extern_blocks == other.extern_blocks
            && *unnamed_consts == other.unnamed_consts
            && *unnamed_trait_imports == other.unnamed_trait_imports
            && *underscore_imports == other.underscore_imports
            && *glob_ambiguities == other.glob_ambiguities
            && *shadowed_imports == other.shadowed_imports
            && *use_imports_types == other.use_imports_types
            && *use_imports_values == other.use_imports_values
            && *use_imports_macros == other.use_imports_macros
            && *use_decls == other.use_decls
            && *extern_crate_decls == other.extern_crate_decls
            && *legacy_macros == other.legacy_macros
            && *attr_macros == other.attr_macros
            && *macro_invocations == other.macro_invocations
            && *derive_macros == other.derive_macros
    }
}

impl Eq for ItemScope {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeriveMacroInvocation {
    pub(crate) attr_id: AttrId,
    /// The `#[derive]` call
    pub(crate) attr_call_id: MacroCallId,
    pub(crate) derive_call_ids: SmallVec<[Option<Either<MacroCallId, BuiltinDeriveImplId>>; 4]>,
}

/// A name two glob imports of a scope give different items in one namespace (rustc's ambiguity
/// binding): the scope binds the name to the first glob's item, and this is the other's. Kept
/// only while no declaration or single import shadows the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlobAmbiguity {
    pub name: Name,
    pub other: ItemInNs,
    pub glob: GlobId,
    pub vis: Visibility,
}

/// What a single import or `extern crate` gave a name in one namespace where the scope keeps
/// another binding of that name: a declaration or an earlier import it is defined twice with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShadowedImport {
    pub import: ImportOrExternCrate,
    pub name: Name,
    pub item: ItemInNs,
    pub vis: Visibility,
}

/// A `use path as _;`, which binds no name: what `path` names in the types namespace, if it
/// resolved, and the import's visibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnderscoreImport {
    pub import: ImportId,
    pub def: Option<ModuleDefId>,
    pub vis: Visibility,
}

/// The builtin types by name, which a root `DefMap` resolves a name to when no scope does:
/// asked once per unqualified name, so it is an [`Index`] probe like a scope's, mostly ending at
/// an empty slot.
pub(crate) struct BuiltinScope {
    names: Box<[Name]>,
    defs: Box<[PerNs]>,
    index: Index,
}

impl BuiltinScope {
    #[inline]
    pub(crate) fn get(&self, name: &Name) -> Option<&PerNs> {
        (self.index.probe(name_hash(name)))
            .map(|(_, pos)| pos as usize)
            .find(|&pos| self.names[pos] == *name)
            .map(|pos| &self.defs[pos])
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Name, &PerNs)> + '_ {
        self.names.iter().zip(self.defs.iter())
    }
}

pub(crate) static BUILTIN_SCOPE: LazyLock<BuiltinScope> = LazyLock::new(|| {
    let all = BuiltinType::all_builtin_types();
    // alloc: once per process (LazyLock)
    let names: Box<[Name]> = all.iter().map(|(name, _)| *name).collect();
    let defs = all.iter().map(|&(_, ty)| PerNs::types(ty.into(), Visibility::Public, None));
    let mut index = Index::with_capacity(names.len());
    for (pos, name) in names.iter().enumerate() {
        index.insert(name_hash(name), 0, pos);
    }
    // alloc: once per process (LazyLock)
    BuiltinScope { names, defs: defs.collect(), index }
});

/// Shadow mode for builtin type which can be shadowed by module.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub(crate) enum BuiltinShadowMode {
    /// Prefer user-defined modules (or other types) over builtins.
    Module,
    /// Prefer builtins over user-defined modules (but not other types).
    Other,
}

fn named(ns: u32, entries: &[Entry]) -> impl Iterator<Item = (u32, usize, &Name)> + '_ {
    entries.iter().enumerate().map(move |(pos, it)| (ns, pos, &it.name))
}

/// The columns and their index.
impl ItemScope {
    fn indexed_len(&self) -> usize {
        self.types.len() + self.values.len() + self.macros.len() + self.unresolved.len()
    }

    #[inline]
    fn name_at(&self, ns: u32, pos: u32) -> &Name {
        let pos = pos as usize;
        match ns {
            TYPES => &self.types.entries[pos].name,
            VALUES => &self.values.entries[pos].name,
            MACROS => &self.macros.entries[pos].name,
            _ => &self.unresolved[pos],
        }
    }

    #[inline]
    fn sorted_name(&self, at: u32) -> &Name {
        self.name_at(at >> 30, at & POS_MASK)
    }

    fn all_names(&self) -> impl Iterator<Item = (u32, usize, &Name)> + '_ {
        (named(TYPES, &self.types.entries))
            .chain(named(VALUES, &self.values.entries))
            .chain(named(MACROS, &self.macros.entries))
            .chain(self.unresolved.iter().enumerate().map(|(pos, it)| (UNRESOLVED, pos, it)))
    }

    /// Where `name` is in `types`, `values` and `macros`, [`NONE`] where it is not.
    #[inline]
    fn positions(&self, name: &Name) -> [u32; 3] {
        let mut found = [NONE; 3];
        if self.names.is_empty() {
            let columns = [&self.types.entries, &self.values.entries, &self.macros.entries];
            for (at, column) in found.iter_mut().zip(columns) {
                if let Some(pos) = column.iter().position(|it| it.name == *name) {
                    *at = pos as u32;
                }
            }
            return found;
        }
        for (ns, pos) in self.names.probe(name_hash(name)) {
            if ns != UNRESOLVED && *self.name_at(ns, pos) == *name {
                found[ns as usize] = pos;
            }
        }
        found
    }

    /// Where `name` is in namespace `ns`.
    #[inline]
    fn position(&self, ns: u32, name: &Name) -> Option<usize> {
        if self.names.is_empty() {
            let in_column = |entries: &[Entry]| entries.iter().position(|it| it.name == *name);
            return match ns {
                TYPES => in_column(&self.types.entries),
                VALUES => in_column(&self.values.entries),
                MACROS => in_column(&self.macros.entries),
                _ => self.unresolved.iter().position(|it| it == name),
            };
        }
        (self.names.probe(name_hash(name)))
            .find(|&(at, pos)| at == ns && *self.name_at(ns, pos) == *name)
            .map(|(_, pos)| pos as usize)
    }

    /// Adds `entry` to namespace `ns`, which does not have its name.
    fn push_entry(&mut self, ns: u32, entry: Entry) {
        let column = match ns {
            TYPES => &mut self.types.entries,
            VALUES => &mut self.values.entries,
            _ => &mut self.macros.entries,
        };
        let pos = column.len();
        column.push(entry);
        self.added(ns, pos, &entry.name);
    }

    fn push_unresolved(&mut self, name: Name) {
        let pos = self.unresolved.len();
        self.unresolved.push(name);
        self.added(UNRESOLVED, pos, &name);
    }

    fn added(&mut self, ns: u32, pos: usize, name: &Name) {
        self.sorted.take();
        self.by_def.take();
        let len = self.indexed_len();
        if len <= LINEAR {
            return;
        }
        if self.names.fits(len) {
            self.names.insert(name_hash(name), ns, pos);
        } else {
            self.reindex(2 * len);
        }
    }

    /// Indexes every name with room for `capacity`, and whether no namespace has a name twice.
    fn reindex(&mut self, capacity: usize) -> bool {
        let len = self.indexed_len();
        if len <= LINEAR {
            self.names = Index::default();
            let all: SmallVec<[(u32, &Name); LINEAR]> =
                // alloc: inline, at most 8 names; no heap
                self.all_names().map(|(ns, _, name)| (ns, name)).collect();
            return (1..all.len()).all(|at| !all[..at].contains(&all[at]));
        }
        let mut names = Index::with_capacity(capacity);
        let mut unique = true;
        for (ns, pos, name) in self.all_names() {
            let hash = name_hash(name);
            unique &= !names.probe(hash).any(|(at, other)| at == ns && self.name_at(at, other) == name);
            names.insert(hash, ns, pos);
        }
        self.names = names;
        unique
    }

    /// Every name once, by text, in a scope of more than [`LINEAR`] names: made by
    /// [`Self::settle`] or [`Self::seal`] for the memo's scope, and here only for a scope still
    /// being collected, once per change it is read after.
    fn sorted(&self) -> &[u32] {
        // alloc: a scope being collected, once per change; a settled one has its table
        self.sorted.get_or_init(|| self.sorted_table())
    }

    /// Every name once, by text, for a scope of at most [`LINEAR`] names.
    fn sorted_small(&self) -> SmallVec<[u32; LINEAR]> {
        let mut all: SmallVec<[u32; LINEAR]> =
            // alloc: none, at most `LINEAR` positions inline
            self.all_names().map(|(ns, pos, _)| ns << 30 | pos as u32).collect();
        let kept = self.by_name(&mut all);
        all.truncate(kept);
        all
    }

    /// The table [`Self::sorted`] keeps, sorted in scratch and copied out at its length.
    fn sorted_table(&self) -> Box<[u32]> {
        scratch::with_u32s(|all| {
            all.extend(self.all_names().map(|(ns, pos, _)| ns << 30 | pos as u32));
            let kept = self.by_name(all);
            // alloc: the table, at its exact length
            Box::from(&all[..kept])
        })
    }

    /// Sorts `all` by name and moves the first position of each name to the front, returning how
    /// many names there are.
    fn by_name(&self, all: &mut [u32]) -> usize {
        all.sort_unstable_by(|&a, &b| self.sorted_name(a).cmp(self.sorted_name(b)));
        let mut kept = 0;
        for at in 0..all.len() {
            if kept == 0 || self.sorted_name(all[kept - 1]) != self.sorted_name(all[at]) {
                all[kept] = all[at];
                kept += 1;
            }
        }
        kept
    }

    /// Builds the tables a settled scope is read by when it has more than [`LINEAR`] names, and
    /// drops them when it has fewer (a small scope scans its columns).
    fn fill_tables(&mut self) {
        if self.indexed_len() <= LINEAR {
            self.sorted = OnceLock::new();
            self.by_def = OnceLock::new();
            return;
        }
        if self.sorted.get().is_none() {
            let table = self.sorted_table();
            self.sorted = OnceLock::from(table);
        }
        if self.by_def.get().is_none() {
            let table = self.by_def_table();
            self.by_def = OnceLock::from(table);
        }
    }

    /// Indexes a scope `frozen` read column by column, or says what in it is there twice.
    pub(crate) fn seal(&mut self) -> Result<(), &'static str> {
        let tight = self.indexed_len();
        if !self.reindex(tight) {
            return Err("a scope with a name twice in one namespace");
        }
        let unique = [
            self.use_imports_types.reindex(self.use_imports_types.len()),
            self.use_imports_values.reindex(self.use_imports_values.len()),
            self.use_imports_macros.reindex(self.use_imports_macros.len()),
            self.legacy_macros.reindex(self.legacy_macros.len()),
            self.attr_macros.reindex(self.attr_macros.len()),
            self.macro_invocations.reindex(self.macro_invocations.len()),
            self.derive_macros.reindex(self.derive_macros.len()),
        ];
        if unique.contains(&false) {
            return Err("a scope with a key twice in one of its maps");
        }
        self.fill_tables();
        Ok(())
    }
}

/// Legacy macros can only be accessed through special methods like `get_legacy_macros`.
/// Other methods will only resolve values, types and module scoped macros only.
impl ItemScope {
    pub fn entries(&self) -> impl Iterator<Item = (&Name, PerNs)> + '_ {
        // FIXME: shadowing
        let order = match self.indexed_len() <= LINEAR {
            true => Either::Left(self.sorted_small().into_iter()),
            false => Either::Right(self.sorted().iter().copied()),
        };
        order.map(move |at| {
            let name = self.sorted_name(at);
            (name, self.get(name))
        })
    }

    pub fn values(&self) -> impl Iterator<Item = (&Name, Item<ModuleDefId, ImportOrGlob>)> + '_ {
        self.values.iter()
    }

    pub fn types(
        &self,
    ) -> impl Iterator<Item = (&Name, Item<ModuleDefId, ImportOrExternCrate>)> + '_ {
        self.types.iter()
    }

    pub fn macros(&self) -> impl Iterator<Item = (&Name, Item<MacroId, ImportOrExternCrate>)> + '_ {
        self.macros.iter()
    }

    pub fn fully_resolve_import(&self, db: &Db, mut import: ImportId) -> PerNs {
        let mut res = PerNs::none();

        let mut scope = self;
        while let Some(&m) = scope.use_imports_macros.get(&ImportOrExternCrate::Import(import)) {
            match m {
                ImportOrDef::Import(i) => {
                    let module_id = i.use_.lookup(db).container;
                    scope = module_id.def_map(db).module(db, module_id).scope(db);
                    import = i;
                }
                ImportOrDef::Def(ModuleDefId::MacroId(def)) => {
                    res.macros = Some(Item { def, vis: Visibility::Public, import: None });
                    break;
                }
                _ => break,
            }
        }
        let mut scope = self;
        while let Some(&m) = scope.use_imports_types.get(&ImportOrExternCrate::Import(import)) {
            match m {
                ImportOrDef::Import(i) => {
                    let module_id = i.use_.lookup(db).container;
                    scope = module_id.def_map(db).module(db, module_id).scope(db);
                    import = i;
                }
                ImportOrDef::Def(def) => {
                    res.types = Some(Item { def, vis: Visibility::Public, import: None });
                    break;
                }
                _ => break,
            }
        }
        let mut scope = self;
        while let Some(&m) = scope.use_imports_values.get(&ImportOrGlob::Import(import)) {
            match m {
                ImportOrDef::Import(i) => {
                    let module_id = i.use_.lookup(db).container;
                    scope = module_id.def_map(db).module(db, module_id).scope(db);
                    import = i;
                }
                ImportOrDef::Def(def) => {
                    res.values = Some(Item { def, vis: Visibility::Public, import: None });
                    break;
                }
                _ => break,
            }
        }
        res
    }

    pub fn declarations(&self) -> impl Iterator<Item = ModuleDefId> + '_ {
        self.declarations.iter().copied()
    }

    pub fn extern_crate_decls(&self) -> impl ExactSizeIterator<Item = ExternCrateId> + '_ {
        self.extern_crate_decls.iter().copied()
    }

    pub fn extern_blocks(&self) -> impl Iterator<Item = ExternBlockId> + '_ {
        self.extern_blocks.iter().copied()
    }

    pub fn use_decls(&self) -> impl ExactSizeIterator<Item = UseId> + '_ {
        self.use_decls.iter().copied()
    }

    /// The impls declared here, in declaration order.
    pub fn impls(&self) -> impl ExactSizeIterator<Item = ImplId> + '_ {
        self.impls.iter().copied()
    }

    /// The impls where `bits` is set (or, with `set` false, clear), in declaration order.
    fn impls_where<'a>(
        &'a self,
        bits: &'a ListBits,
        set: bool,
    ) -> impl Iterator<Item = ImplId> + 'a {
        self.impls
            .iter()
            .enumerate()
            .filter(move |&(at, _)| bits.contains(at) == set)
            .map(|(_, &it)| it)
    }

    pub fn trait_impls(&self) -> impl Iterator<Item = ImplId> + '_ {
        self.impls_where(&self.trait_impl, true)
    }

    pub fn inherent_impls(&self) -> impl Iterator<Item = ImplId> + '_ {
        self.impls_where(&self.trait_impl, false)
    }

    /// The trait impls that are not `#[rustc_reservation_impl]`, in declaration order: what
    /// coherence and trait selection take as impls (rustc's `impl_polarity` makes a reservation
    /// impl `ReservationImpl`, which selection skips).
    pub fn trait_impls_without_reservation(&self) -> impl Iterator<Item = ImplId> + '_ {
        self.impls
            .iter()
            .enumerate()
            .filter(|&(at, _)| self.trait_impl.contains(at) && !self.reservation_impl.contains(at))
            .map(|(_, &it)| it)
    }

    pub fn builtin_derive_impls(&self) -> impl ExactSizeIterator<Item = BuiltinDeriveImplId> + '_ {
        self.builtin_derive_impls.iter().copied()
    }

    pub fn all_macro_calls(&self) -> impl Iterator<Item = MacroCallId> + '_ {
        self.macro_invocations.values().copied().chain(self.attr_macros.values().copied()).chain(
            self.derive_macros.values().flat_map(|it| {
                it.iter().flat_map(|it| {
                    it.derive_call_ids.iter().copied().flatten().flat_map(|it| it.left())
                })
            }),
        )
    }

    pub(crate) fn modules_in_scope(&self) -> impl Iterator<Item = (ModuleId, Visibility)> + '_ {
        self.types.entries.iter().filter(|it| it.def_tag == DEF_MODULE).map(|it| {
            let item: TypesItem = it.item();
            match item.def {
                ModuleDefId::ModuleId(module) => (module, item.vis),
                _ => unreachable!("a module entry of another definition"),
            }
        })
    }

    pub fn unnamed_consts(&self) -> impl Iterator<Item = ConstId> + '_ {
        self.unnamed_consts.iter().copied()
    }

    /// Every const of the module, named or not. An impl in the block of any of them is an impl
    /// of the crate.
    pub fn consts(&self) -> impl Iterator<Item = ConstId> + '_ {
        let named = self.declarations.iter().filter_map(|&def| match def {
            ModuleDefId::ConstId(konst) => Some(konst),
            _ => None,
        });
        self.unnamed_consts().chain(named)
    }

    /// Iterate over all legacy textual scoped macros visible at the end of the module
    pub fn legacy_macros(&self) -> impl Iterator<Item = (&Name, &[MacroId])> + '_ {
        self.legacy_macros.iter().map(|(name, def)| (name, &**def))
    }

    /// Get a name from current module scope, legacy macros are not included
    #[inline]
    pub fn get(&self, name: &Name) -> PerNs {
        let [types, values, macros] = self.positions(name);
        PerNs {
            types: self.types.get(types),
            values: self.values.get(values),
            macros: self.macros.get(macros),
        }
    }

    /// The import that binds `name` in `ns` here, with the binding's visibility: `None` where
    /// the scope declares the name or does not bind it in `ns`. Where in the source the import
    /// is, `src::use_tree_to_view` of its use tree says.
    pub fn import_of(
        &self,
        name: &Name,
        ns: Namespace,
    ) -> Option<(ImportOrExternCrate, Visibility)> {
        let res = self.get(name);
        match ns {
            Namespace::Types => res.types.and_then(|it| Some((it.import?, it.vis))),
            Namespace::Values => res.values.and_then(|it| Some((it.import?.into(), it.vis))),
            Namespace::Macros => res.macros.and_then(|it| Some((it.import?, it.vis))),
        }
    }

    /// Every `use path as _;` of this scope.
    pub fn underscore_imports(&self) -> &[UnderscoreImport] {
        &self.underscore_imports
    }

    /// The names two glob imports give different items, with the item the scope does not bind.
    pub fn glob_ambiguities(&self) -> &[GlobAmbiguity] {
        &self.glob_ambiguities
    }

    /// The other item of `name` in `ns`, where two glob imports give it different ones.
    pub fn glob_ambiguity(&self, name: &Name, ns: Namespace) -> Option<&GlobAmbiguity> {
        (self.glob_ambiguities.iter()).find(|it| it.name == *name && namespace_of(it.other) == ns)
    }

    /// The single imports whose name another binding of the scope kept.
    pub fn shadowed_imports(&self) -> &[ShadowedImport] {
        &self.shadowed_imports
    }

    pub(crate) fn type_(&self, name: &Name) -> Option<(ModuleDefId, Visibility)> {
        let item = self.types.at(self.position(TYPES, name)?);
        Some((item.def, item.vis))
    }

    pub(crate) fn makro(&self, name: &Name) -> Option<MacroId> {
        Some(self.macros.at(self.position(MACROS, name)?).def)
    }

    #[inline]
    fn entry_at(&self, at: u32) -> &Entry {
        let pos = (at & POS_MASK) as usize;
        match at >> 30 {
            TYPES => &self.types.entries[pos],
            VALUES => &self.values.entries[pos],
            _ => &self.macros.entries[pos],
        }
    }

    /// The entries of the three namespaces, by namespace, def and then position, for a scope of
    /// more than [`LINEAR`] names: made by [`Self::settle`] or [`Self::seal`], and here only for a
    /// scope still being collected.
    fn by_def(&self) -> &[u32] {
        // alloc: a scope being collected, once per change; a settled one has its table
        self.by_def.get_or_init(|| self.by_def_table())
    }

    /// The table [`Self::by_def`] keeps, sorted in scratch and copied out at its length.
    fn by_def_table(&self) -> Box<[u32]> {
        scratch::with_u32s(|all| {
            all.extend(
                (named(TYPES, &self.types.entries))
                    .chain(named(VALUES, &self.values.entries))
                    .chain(named(MACROS, &self.macros.entries))
                    .map(|(ns, pos, _)| ns << 30 | pos as u32),
            );
            // The position last keeps a def's names in column order, which is the order
            // `name_of` (the first) and `names_of` (the first `Some`) answer in.
            // `(at >> 30, it.def_tag, it.def, at & POS_MASK)` packed in order: an `Option<Id>`
            // orders `None` first, then by its id's `as_u32`.
            scratch::with_u32s(|spare| {
                radix::sort_copy_by_key_in(
                    all,
                    |&at| {
                        let it = self.entry_at(at);
                        let def = it.def.map_or(0, |id| {
                            1u128 << 64 | u128::from(id.as_u32())
                        });
                        u128::from(at >> 30) << 97
                            | u128::from(it.def_tag) << 89
                            | def << 24
                            | u128::from(at & POS_MASK)
                    },
                    spare,
                )
            });
            // alloc: the table, at its exact length
            Box::from(&all[..])
        })
    }

    /// The entries of `item`'s namespace that name it, in column order, compared packed: a scan
    /// of the column in a scope of at most [`LINEAR`] names, a binary search of `by_def` above.
    fn naming(&self, item: ItemInNs) -> impl Iterator<Item = (&Name, Visibility, bool)> + '_ {
        let (ns, column, def) = match item {
            ItemInNs::Types(def) => (TYPES, &self.types.entries, def.pack()),
            ItemInNs::Values(def) => (VALUES, &self.values.entries, def.pack()),
            ItemInNs::Macros(def) => (MACROS, &self.macros.entries, def.pack()),
        };
        let found = if self.indexed_len() <= LINEAR {
            Either::Left(column.iter().filter(move |it| it.is_def(def)))
        } else {
            let by_def = self.by_def();
            let key = (ns, def.0, def.1);
            let start = by_def.partition_point(|&at| {
                let it = self.entry_at(at);
                (at >> 30, it.def_tag, it.def) < key
            });
            Either::Right(
                by_def[start..]
                    .iter()
                    .map(|&at| (at >> 30, self.entry_at(at)))
                    .take_while(move |&(at_ns, it)| at_ns == ns && it.is_def(def))
                    .map(|(_, it)| it),
            )
        };
        found.map(|it| (&it.name, it.vis(), it.declared()))
    }

    /// A binary search of the scope's entries by def (built at settle, or on the first call after
    /// a change while the scope is being collected).
    pub(crate) fn name_of(&self, item: ItemInNs) -> Option<(&Name, Visibility, /*declared*/ bool)> {
        self.naming(item).next()
    }

    /// Like [`ItemScope::name_of`], over every name of `item` in column order.
    pub(crate) fn names_of<T>(
        &self,
        item: ItemInNs,
        mut cb: impl FnMut(&Name, Visibility, /*declared*/ bool) -> Option<T>,
    ) -> Option<T> {
        self.naming(item).find_map(|(name, vis, declared)| cb(name, vis, declared))
    }

    pub(crate) fn traits(&self) -> impl Iterator<Item = TraitId> + '_ {
        (self.types.entries.iter())
            .filter(|it| it.def_tag == DEF_TRAIT)
            .map(|it| match ModuleDefId::unpack(it.def_tag, it.def) {
                ModuleDefId::TraitId(t) => t,
                _ => unreachable!("a trait entry of another definition"),
            })
            .chain(self.unnamed_trait_imports.iter().map(|&(t, _)| t))
    }

    pub(crate) fn resolutions(&self) -> impl Iterator<Item = (Option<Name>, PerNs)> + '_ {
        self.entries().map(|(name, res)| (Some(*name), res)).chain(
            self.unnamed_trait_imports.iter().map(|(tr, trait_)| {
                (
                    None,
                    PerNs::types(ModuleDefId::TraitId(*tr), trait_.vis, trait_.import),
                )
            }),
        )
    }

    pub fn macro_invoc(&self, call: AstId<ast::MacroCall>) -> Option<MacroCallId> {
        self.macro_invocations.get(&call).copied()
    }

    pub fn iter_macro_invoc(&self) -> impl Iterator<Item = (&AstId<ast::MacroCall>, &MacroCallId)> {
        self.macro_invocations.iter()
    }
}

impl ItemScope {
    pub(crate) fn declare(&mut self, def: ModuleDefId) {
        self.declarations.push(def)
    }

    pub(crate) fn get_legacy_macro(&self, name: &Name) -> Option<&[MacroId]> {
        self.legacy_macros.get(name).map(|it| &**it)
    }

    pub(crate) fn define_impl(&mut self, imp: ImplId, is_trait_impl: bool, is_reservation: bool) {
        let at = self.impls.len();
        self.impls.push(imp);
        self.trait_impl.set_growing(at, is_trait_impl);
        self.reservation_impl.set_growing(at, is_reservation);
    }

    pub(crate) fn define_builtin_derive_impl(&mut self, imp: BuiltinDeriveImplId) {
        self.builtin_derive_impls.push(imp);
    }

    pub(crate) fn define_extern_block(&mut self, extern_block: ExternBlockId) {
        self.extern_blocks.push(extern_block);
    }

    pub(crate) fn define_extern_crate_decl(&mut self, extern_crate: ExternCrateId) {
        self.extern_crate_decls.push(extern_crate);
    }

    pub(crate) fn define_use_decl(&mut self, use_: UseId) {
        self.use_decls.push(use_);
    }

    pub(crate) fn define_unnamed_const(&mut self, konst: ConstId) {
        self.unnamed_consts.push(konst);
    }

    pub(crate) fn define_legacy_macro(&mut self, name: Name, mac: MacroId) {
        self.legacy_macros.or_insert_with(name, SmallVec::new).push(mac);
    }

    pub(crate) fn add_attr_macro_invoc(&mut self, item: AstId<ast::Item>, call: MacroCallId) {
        self.attr_macros.insert(item, call);
    }

    pub(crate) fn add_macro_invoc(&mut self, call: AstId<ast::MacroCall>, call_id: MacroCallId) {
        self.macro_invocations.insert(call, call_id);
    }

    pub fn attr_macro_invocs(&self) -> impl Iterator<Item = (AstId<ast::Item>, MacroCallId)> + '_ {
        self.attr_macros.iter().map(|(k, v)| (*k, *v))
    }

    pub(crate) fn set_derive_macro_invoc(
        &mut self,
        adt: AstId<ast::Adt>,
        call: Either<MacroCallId, BuiltinDeriveImplId>,
        id: AttrId,
        idx: usize,
    ) {
        if let Some(derives) = self.derive_macros.get_mut(&adt)
            && let Some(DeriveMacroInvocation { derive_call_ids, .. }) =
                derives.iter_mut().find(|&&mut DeriveMacroInvocation { attr_id, .. }| id == attr_id)
        {
            derive_call_ids[idx] = Some(call);
        }
    }

    /// We are required to set this up front as derive invocation recording happens out of order
    /// due to the fixed pointer iteration loop being able to record some derives later than others
    /// independent of their indices.
    pub(crate) fn init_derive_attribute(
        &mut self,
        adt: AstId<ast::Adt>,
        attr_id: AttrId,
        attr_call_id: MacroCallId,
        mut derive_call_ids: SmallVec<[Option<Either<MacroCallId, BuiltinDeriveImplId>>; 4]>,
    ) {
        derive_call_ids.shrink_to_fit();
        self.derive_macros.or_insert_with(adt, SmallVec::new).push(DeriveMacroInvocation {
            attr_id,
            attr_call_id,
            derive_call_ids,
        });
    }

    pub fn derive_macro_invocs(
        &self,
    ) -> impl Iterator<
        Item = (
            AstId<ast::Adt>,
            impl Iterator<
                Item = (AttrId, MacroCallId, &[Option<Either<MacroCallId, BuiltinDeriveImplId>>]),
            >,
        ),
    > + '_ {
        self.derive_macros.iter().map(|(k, v)| {
            (
                *k,
                v.iter().map(|DeriveMacroInvocation { attr_id, attr_call_id, derive_call_ids }| {
                    (*attr_id, *attr_call_id, &**derive_call_ids)
                }),
            )
        })
    }

    pub fn derive_macro_invoc(
        &self,
        ast_id: AstId<ast::Adt>,
        attr_id: AttrId,
    ) -> Option<MacroCallId> {
        Some(self.derive_macros.get(&ast_id)?.iter().find(|it| it.attr_id == attr_id)?.attr_call_id)
    }

    // FIXME: This is only used in collection, we should move the relevant parts of it out of ItemScope
    pub(crate) fn unnamed_trait_vis(&self, tr: TraitId) -> Option<Visibility> {
        self.unnamed_trait_imports.iter().find(|&&(t, _)| t == tr).map(|(_, trait_)| trait_.vis)
    }

    /// Records `tr` as in scope unnamed, bound by `import`: a `use Tr as _` of this scope, or
    /// the glob that read it from another scope's unnamed traits.
    pub(crate) fn push_unnamed_trait_through(
        &mut self,
        tr: TraitId,
        vis: Visibility,
        import: Option<ImportOrExternCrate>,
    ) {
        self.unnamed_trait_imports.push((tr, Item { def: (), vis, import }));
    }

    /// How this scope has `tr` in scope, as rustc's `traits_in_module` lists it (named
    /// bindings in column order, then unnamed ones, named `_`): the binding's name and its
    /// import, `None` for a declaration; `None` where `tr` is not in scope here.
    pub fn trait_binding(&self, tr: TraitId) -> Option<(Name, Option<ImportOrExternCrate>)> {
        let item = ItemInNs::Types(ModuleDefId::TraitId(tr));
        let named = self.names_of(item, |name, _, declared| match declared {
            true => Some((*name, None)),
            false => Some((*name, self.get(name).types.and_then(|it| it.import))),
        });
        named.or_else(|| {
            let (_, it) = self.unnamed_trait_imports.iter().find(|&&(t, _)| t == tr)?;
            Some((Name::new_symbol_root(sym::underscore), it.import))
        })
    }

    /// Records `it`, in place of what an earlier resolution of the same import recorded.
    pub(crate) fn push_underscore_import(&mut self, it: UnderscoreImport) {
        match self.underscore_imports.iter_mut().find(|old| old.import == it.import) {
            Some(old) => *old = it,
            None => self.underscore_imports.push(it),
        }
    }

    /// Records that glob `it.glob` gives `it.name` another item than the scope binds it to.
    pub(crate) fn push_glob_ambiguity(&mut self, it: GlobAmbiguity) {
        let ns = namespace_of(it.other);
        let same = |old: &&mut GlobAmbiguity| {
            old.name == it.name && old.glob == it.glob && namespace_of(old.other) == ns
        };
        match self.glob_ambiguities.iter_mut().find(same) {
            Some(old) => *old = it,
            None => self.glob_ambiguities.push(it),
        }
    }

    fn drop_glob_ambiguity(&mut self, name: &Name, ns: Namespace) {
        if !self.glob_ambiguities.is_empty() {
            self.glob_ambiguities.retain(|it| it.name != *name || namespace_of(it.other) != ns);
        }
    }

    /// Drops the ambiguities a later write settled: the name is no longer a glob's, or now
    /// names the other item too. Called once, when collection is done.
    pub(crate) fn settle_glob_ambiguities(&mut self) {
        if self.glob_ambiguities.is_empty() {
            return;
        }
        let mut ambiguities = std::mem::take(&mut self.glob_ambiguities);
        ambiguities.retain(|it| {
            let now = self.get(&it.name);
            match it.other {
                ItemInNs::Types(def) => now.types.is_some_and(|now| {
                    matches!(now.import, Some(ImportOrExternCrate::Glob(_))) && now.def != def
                }),
                ItemInNs::Values(def) => now.values.is_some_and(|now| {
                    matches!(now.import, Some(ImportOrGlob::Glob(_))) && now.def != def
                }),
                ItemInNs::Macros(def) => now.macros.is_some_and(|now| {
                    matches!(now.import, Some(ImportOrExternCrate::Glob(_))) && now.def != def
                }),
            }
        });
        self.glob_ambiguities = ambiguities;
    }

    fn push_shadowed(&mut self, it: ShadowedImport) {
        let ns = namespace_of(it.item);
        match (self.shadowed_imports.iter_mut())
            .find(|old| old.import == it.import && namespace_of(old.item) == ns)
        {
            Some(old) => *old = it,
            None => self.shadowed_imports.push(it),
        }
    }

    pub(crate) fn push_res_with_import<A: Allocator + Clone>(
        &mut self,
        glob_imports: &mut PerNsGlobImports<A>,
        lookup: (ModuleId, Name),
        def: PerNs,
        import: Option<ImportOrExternCrate>,
    ) -> bool {
        let mut changed = false;
        let name = lookup.1;

        // FIXME: Document and simplify this

        if let Some(mut fld) = def.types {
            match self.position(TYPES, &name) {
                None => {
                    match import {
                        Some(ImportOrExternCrate::Glob(_)) => {
                            glob_imports.types.insert(lookup);
                        }
                        _ => _ = glob_imports.types.remove(&lookup),
                    }
                    let prev = std::mem::replace(&mut fld.import, import);
                    if let Some(import) = import
                        && import.glob().is_none()
                    {
                        self.use_imports_types
                            .insert(import, prev.map_or(ImportOrDef::Def(fld.def), Into::into));
                    }
                    self.push_entry(TYPES, Entry::new(name, fld));
                    changed = true;
                }
                Some(pos) => {
                    match import {
                        Some(ImportOrExternCrate::Glob(..)) => {
                            // Multiple globs may import the same item and they may
                            // override visibility from previously resolved globs. This is
                            // currently handled by `DefCollector`, because we need to
                            // compute the max visibility for items and we need `DefMap`
                            // for that.
                        }
                        _ => {
                            // A non-glob import either shadows a glob import of the same
                            // name, or re-resolves a stale binding it recorded earlier.
                            if glob_imports.types.remove(&lookup)
                                || self.types.at(pos).is_reresolved_by(&fld, import)
                            {
                                let prev = std::mem::replace(&mut fld.import, import);
                                if let Some(import) = import
                                    && import.glob().is_none()
                                {
                                    self.use_imports_types.insert(
                                        import,
                                        prev.map_or(ImportOrDef::Def(fld.def), Into::into),
                                    );
                                }
                                cov_mark::hit!(import_shadowed);
                                self.types.entries[pos] = Entry::new(name, fld);
                                self.by_def.take();
                                self.drop_glob_ambiguity(&name, Namespace::Types);
                                changed = true;
                            } else if let Some(import) = import
                                && self.types.at(pos).import != Some(import)
                            {
                                self.push_shadowed(ShadowedImport {
                                    import,
                                    name,
                                    item: ItemInNs::Types(fld.def),
                                    vis: fld.vis,
                                });
                            }
                        }
                    }
                }
            }
        }

        if let Some(mut fld) = def.values {
            match self.position(VALUES, &name) {
                None => {
                    match import {
                        Some(ImportOrExternCrate::Glob(_)) => {
                            glob_imports.values.insert(lookup);
                        }
                        _ => _ = glob_imports.values.remove(&lookup),
                    }
                    let import = import.and_then(ImportOrExternCrate::import_or_glob);
                    let prev = std::mem::replace(&mut fld.import, import);
                    if let Some(import) = import
                        && import.into_import().is_some()
                    {
                        self.use_imports_values
                            .insert(import, prev.map_or(ImportOrDef::Def(fld.def), Into::into));
                    }
                    self.push_entry(VALUES, Entry::new(name, fld));
                    changed = true;
                }
                Some(pos) if !matches!(import, Some(ImportOrExternCrate::Glob(..))) => {
                    let import = import.and_then(ImportOrExternCrate::import_or_glob);
                    if glob_imports.values.remove(&lookup)
                        || self.values.at(pos).is_reresolved_by(&fld, import)
                    {
                        cov_mark::hit!(import_shadowed);

                        let prev = std::mem::replace(&mut fld.import, import);
                        if let Some(import) = import
                            && import.into_import().is_some()
                        {
                            self.use_imports_values
                                .insert(import, prev.map_or(ImportOrDef::Def(fld.def), Into::into));
                        }
                        self.values.entries[pos] = Entry::new(name, fld);
                        self.by_def.take();
                        self.drop_glob_ambiguity(&name, Namespace::Values);
                        changed = true;
                    } else if let Some(import) = import
                        && self.values.at(pos).import != Some(import)
                    {
                        self.push_shadowed(ShadowedImport {
                            import: import.into(),
                            name,
                            item: ItemInNs::Values(fld.def),
                            vis: fld.vis,
                        });
                    }
                }
                Some(_) => {}
            }
        }

        if let Some(mut fld) = def.macros {
            match self.position(MACROS, &name) {
                None => {
                    match import {
                        Some(ImportOrExternCrate::Glob(_)) => {
                            glob_imports.macros.insert(lookup);
                        }
                        _ => _ = glob_imports.macros.remove(&lookup),
                    }
                    let prev = std::mem::replace(&mut fld.import, import);
                    if let Some(import) = import
                        && import.glob().is_none()
                    {
                        self.use_imports_macros.insert(
                            import,
                            prev.map_or_else(|| ImportOrDef::Def(fld.def.into()), Into::into),
                        );
                    }
                    self.push_entry(MACROS, Entry::new(name, fld));
                    changed = true;
                }
                Some(pos)
                    if !matches!(import, Some(ImportOrExternCrate::Glob(..)))
                        && (glob_imports.macros.remove(&lookup)
                            || self.macros.at(pos).is_reresolved_by(&fld, import)) =>
                {
                    cov_mark::hit!(import_shadowed);
                    let prev = std::mem::replace(&mut fld.import, import);
                    if let Some(import) = import
                        && import.glob().is_none()
                    {
                        self.use_imports_macros.insert(
                            import,
                            prev.map_or_else(|| ImportOrDef::Def(fld.def.into()), Into::into),
                        );
                    }
                    self.macros.entries[pos] = Entry::new(name, fld);
                    self.by_def.take();
                    self.drop_glob_ambiguity(&name, Namespace::Macros);
                    changed = true;
                }
                Some(pos) => {
                    if let Some(import) = import
                        && !matches!(import, ImportOrExternCrate::Glob(..))
                        && self.macros.at(pos).import != Some(import)
                    {
                        self.push_shadowed(ShadowedImport {
                            import,
                            name,
                            item: ItemInNs::Macros(fld.def),
                            vis: fld.vis,
                        });
                    }
                }
            }
        }

        if def.is_none() && self.position(UNRESOLVED, &name).is_none() {
            self.push_unresolved(name);
            changed = true;
        }

        changed
    }

    /// Marks everything that is not a procedural macro as private to `this_module`.
    pub(crate) fn censor_non_proc_macros(&mut self, krate: Crate) {
        let vis = Visibility::PubCrate(krate);
        for it in self.types.entries.iter_mut().chain(self.values.entries.iter_mut()) {
            it.set_vis(vis);
        }
        for (_, it) in self.unnamed_trait_imports.iter_mut() {
            it.vis = vis;
        }
        for it in self.macros.entries.iter_mut() {
            if it.def_tag == DEF_PROC_MACRO && it.declared() {
                continue;
            }
            it.set_vis(vis);
        }
    }

    pub(crate) fn dump(&self, db: &Db, buf: &mut String) {
        // alloc: a debug dump (the edge)
        let mut entries: Vec<_> = self.resolutions().collect();
        entries.sort_by_key(|(name, _)| *name);

        let print_macro_sub_ns = |buf: &mut String, macro_id: MacroId| {
            let styles = crate::nameres::macro_styles_from_id(db, macro_id);
            if styles.contains(MacroCallStyles::FN_LIKE) {
                buf.push('!');
            }
            if styles.contains(MacroCallStyles::ATTR) || styles.contains(MacroCallStyles::DERIVE) {
                buf.push('#');
            }
        };

        for (name, def) in entries {
            match &name {
                Some(name) => format_to!(buf, "- {} :", name.display(db, Edition::LATEST)),
                None => buf.push_str("- _ :"),
            }

            if let Some(Item { import, .. }) = def.types {
                buf.push_str(" type");
                match import {
                    Some(ImportOrExternCrate::Import(_)) => buf.push_str(" (import)"),
                    Some(ImportOrExternCrate::Glob(_)) => buf.push_str(" (glob)"),
                    Some(ImportOrExternCrate::ExternCrate(_)) => buf.push_str(" (extern)"),
                    None => (),
                }
            }
            if let Some(Item { import, .. }) = def.values {
                buf.push_str(" value");
                match import {
                    Some(ImportOrGlob::Import(_)) => buf.push_str(" (import)"),
                    Some(ImportOrGlob::Glob(_)) => buf.push_str(" (glob)"),
                    None => (),
                }
            }
            if let Some(Item { def: macro_id, import, .. }) = def.macros {
                buf.push_str(" macro");
                print_macro_sub_ns(buf, macro_id);
                match import {
                    Some(ImportOrExternCrate::Import(_)) => buf.push_str(" (import)"),
                    Some(ImportOrExternCrate::Glob(_)) => buf.push_str(" (glob)"),
                    Some(ImportOrExternCrate::ExternCrate(_)) => buf.push_str(" (extern)"),
                    None => (),
                }
            }
            if def.is_none() {
                buf.push_str(" _");
            }

            buf.push('\n');
        }

        // Also dump legacy-textual-scope macros visible at the _end_ of the scope.
        //
        // For tests involving a cursor position, this might include macros that
        // are _not_ visible at the cursor position.
        // alloc: a debug dump (the edge)
        let mut legacy_macros = self.legacy_macros().collect::<Vec<_>>();
        legacy_macros.sort_by(|(a, _), (b, _)| Ord::cmp(a, b));
        for (name, macros) in legacy_macros {
            format_to!(buf, "- (legacy) {} :", name.display(db, Edition::LATEST));
            for &macro_id in macros {
                buf.push_str(" macro");
                print_macro_sub_ns(buf, macro_id);
            }
            buf.push('\n');
        }
    }

    /// Called once when collection is done, to make the scope the memo keeps: every column and
    /// map made again at its length, the index as small as its load allows, and, for a scope of
    /// more than [`LINEAR`] names, the by-name and by-def tables, so that no read of it
    /// allocates.
    pub(crate) fn settle(&mut self) {
        // Exhaustive match to require handling new fields.
        let Self {
            types,
            values,
            macros,
            unresolved,
            names: _,
            sorted: _,
            by_def: _,
            declarations,
            impls,
            trait_impl,
            reservation_impl,
            builtin_derive_impls,
            unnamed_consts,
            unnamed_trait_imports,
            underscore_imports,
            glob_ambiguities,
            shadowed_imports,
            legacy_macros,
            attr_macros,
            derive_macros,
            extern_crate_decls,
            use_decls,
            use_imports_values,
            use_imports_types,
            use_imports_macros,
            macro_invocations,
            extern_blocks,
        } = self;
        exact(underscore_imports);
        exact(glob_ambiguities);
        exact(shadowed_imports);
        exact(extern_blocks);
        exact(&mut types.entries);
        exact(&mut values.entries);
        exact(&mut macros.entries);
        use_imports_types.settle();
        use_imports_values.settle();
        use_imports_macros.settle();
        exact(unresolved);
        exact(declarations);
        exact(impls);
        trait_impl.shrink_to_fit();
        reservation_impl.shrink_to_fit();
        exact(builtin_derive_impls);
        exact(unnamed_consts);
        exact(unnamed_trait_imports);
        for (_, macs) in legacy_macros.entries.iter_mut() {
            macs.shrink_to_fit();
        }
        for (_, calls) in derive_macros.entries.iter_mut() {
            calls.shrink_to_fit();
        }
        legacy_macros.settle();
        attr_macros.settle();
        derive_macros.settle();
        exact(extern_crate_decls);
        exact(use_decls);
        macro_invocations.settle();
        let unique = self.reindex(self.indexed_len());
        debug_assert!(unique, "a namespace of a scope with a name twice");
        self.fill_tables();
    }
}

// These methods are a temporary measure only meant to be used by `DefCollector::push_res_and_update_glob_vis()`.
impl ItemScope {
    pub(crate) fn update_def_types(&mut self, name: &Name, def: ModuleDefId, vis: Visibility) {
        let pos = self.position(TYPES, name).expect("tried to update def of non-existent type");
        let entry = &mut self.types.entries[pos];
        entry.set_def(def);
        entry.set_vis(vis);
        self.by_def.take();
    }

    pub(crate) fn update_def_values(&mut self, name: &Name, def: ModuleDefId, vis: Visibility) {
        let pos = self.position(VALUES, name).expect("tried to update def of non-existent value");
        let entry = &mut self.values.entries[pos];
        entry.set_def(def);
        entry.set_vis(vis);
        self.by_def.take();
    }

    pub(crate) fn update_def_macros(&mut self, name: &Name, def: MacroId, vis: Visibility) {
        let pos = self.position(MACROS, name).expect("tried to update def of non-existent macro");
        let entry = &mut self.macros.entries[pos];
        entry.set_def(def);
        entry.set_vis(vis);
        self.by_def.take();
    }

    /// Binds `name` in the type namespace to `item` through glob `glob`, in place of the item
    /// another glob bound it to: of two globs giving one name different items, the first in
    /// position order binds it, whichever arrived first (docs/v2/plan/design/dbsp.md §6); of
    /// two giving it the same item, the more visible one, as rustc's `try_define`.
    pub(crate) fn rebind_glob_types(&mut self, name: &Name, mut item: TypesItem, glob: GlobId) {
        let pos = self.position(TYPES, name).expect("tried to rebind a non-existent type");
        item.import = Some(ImportOrExternCrate::Glob(glob));
        self.types.entries[pos] = Entry::new(*name, item);
        self.by_def.take();
    }

    /// [`Self::rebind_glob_types`] in the value namespace.
    pub(crate) fn rebind_glob_values(&mut self, name: &Name, mut item: crate::per_ns::ValuesItem, glob: GlobId) {
        let pos = self.position(VALUES, name).expect("tried to rebind a non-existent value");
        item.import = Some(ImportOrGlob::Glob(glob));
        self.values.entries[pos] = Entry::new(*name, item);
        self.by_def.take();
    }

    /// [`Self::rebind_glob_types`] in the macro namespace.
    pub(crate) fn rebind_glob_macros(&mut self, name: &Name, mut item: crate::per_ns::MacrosItem, glob: GlobId) {
        let pos = self.position(MACROS, name).expect("tried to rebind a non-existent macro");
        item.import = Some(ImportOrExternCrate::Glob(glob));
        self.macros.entries[pos] = Entry::new(*name, item);
        self.by_def.take();
    }
}

impl PerNs {
    pub(crate) fn from_def(
        def: ModuleDefId,
        vis: Visibility,
        value_ns_ctor_vis: Option<Visibility>,
        import: Option<ImportOrExternCrate>,
    ) -> PerNs {
        match def {
            ModuleDefId::ModuleId(_) => PerNs::types(def, vis, import),
            ModuleDefId::FunctionId(_) => {
                PerNs::values(def, vis, import.and_then(ImportOrExternCrate::import_or_glob))
            }
            ModuleDefId::AdtId(adt) => match adt {
                AdtId::UnionId(_) => PerNs::types(def, vis, import),
                AdtId::EnumId(_) => PerNs::types(def, vis, import),
                AdtId::StructId(_) => match value_ns_ctor_vis {
                    Some(value_ns_ctor_vis) => PerNs {
                        types: Some(Item { def, vis, import }),
                        values: Some(Item {
                            def,
                            vis: value_ns_ctor_vis,
                            import: import.and_then(ImportOrExternCrate::import_or_glob),
                        }),
                        macros: None,
                    },
                    None => PerNs::types(def, vis, import),
                },
            },
            ModuleDefId::EnumVariantId(_) => match value_ns_ctor_vis {
                Some(value_ns_ctor_vis) => PerNs {
                    types: Some(Item { def, vis, import }),
                    values: Some(Item {
                        def,
                        vis: value_ns_ctor_vis,
                        import: import.and_then(ImportOrExternCrate::import_or_glob),
                    }),
                    macros: None,
                },
                None => PerNs::types(def, vis, import),
            },
            ModuleDefId::ConstId(_) | ModuleDefId::StaticId(_) => {
                PerNs::values(def, vis, import.and_then(ImportOrExternCrate::import_or_glob))
            }
            ModuleDefId::TraitId(_) => PerNs::types(def, vis, import),
            ModuleDefId::TypeAliasId(_) => PerNs::types(def, vis, import),
            ModuleDefId::BuiltinType(_) => PerNs::types(def, vis, import),
            ModuleDefId::MacroId(mac) => PerNs::macros(mac, vis, import),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum ItemInNs {
    Types(ModuleDefId),
    Values(ModuleDefId),
    Macros(MacroId),
}

pub(crate) fn namespace_of(item: ItemInNs) -> Namespace {
    match item {
        ItemInNs::Types(_) => Namespace::Types,
        ItemInNs::Values(_) => Namespace::Values,
        ItemInNs::Macros(_) => Namespace::Macros,
    }
}

impl ItemInNs {
    pub fn as_module_def_id(self) -> Option<ModuleDefId> {
        match self {
            ItemInNs::Types(id) | ItemInNs::Values(id) => Some(id),
            ItemInNs::Macros(_) => None,
        }
    }

    /// Returns the crate defining this item (or `None` if `self` is built-in).
    pub fn krate(&self, db: &Db) -> Option<Crate> {
        self.module(db).map(|module_id| module_id.krate(db))
    }

    pub fn module(&self, db: &Db) -> Option<ModuleId> {
        match self {
            ItemInNs::Types(id) | ItemInNs::Values(id) => id.module(db),
            ItemInNs::Macros(id) => Some(id.module(db)),
        }
    }
}

#[cfg(test)]
mod tests {
    mod budgets;

    use blaze_hash::{HashMap, HashSet};
    use blaze_ids::rng::SplitMix64;
    use indexmap::map::Entry as MapEntry;
    use intern::Symbol;
    use itertools::Itertools;

    use super::*;
    use crate::{
        FxIndexMap,
        per_ns::{MacrosItem, Namespace, ValuesItem},
    };

    /// An id of no database: the scope only packs and compares it.
    fn id<T: FromId>(n: u64) -> T {
        let n = u32::try_from(n).expect("a test id below 2^32");
        T::from_id(blaze_query::db::Id::from_u32((n % 7 + 1) + ((n / 7) << 20)))
    }

    fn names() -> Vec<Name> {
        (0..40).map(|it| Name::new_symbol_root(Symbol::intern(&format!("scope_test_{it}")))).collect()
    }

    fn def(rng: &mut SplitMix64) -> ModuleDefId {
        let n = rng.below(6);
        match rng.below(8) {
            0 => ModuleDefId::ModuleId(id(n)),
            1 => ModuleDefId::FunctionId(id(n)),
            2 => ModuleDefId::AdtId(AdtId::StructId(id(n))),
            3 => ModuleDefId::TraitId(id(n)),
            4 => ModuleDefId::ConstId(id(n)),
            5 => ModuleDefId::BuiltinType(BUILTIN_TYPES[rng.below(19) as usize]),
            6 => ModuleDefId::EnumVariantId(id(n)),
            _ => ModuleDefId::MacroId(makro(rng)),
        }
    }

    fn makro(rng: &mut SplitMix64) -> MacroId {
        let n = rng.below(6);
        match rng.below(3) {
            0 => MacroId::Macro2Id(id(n)),
            1 => MacroId::MacroRulesId(id(n)),
            _ => MacroId::ProcMacroId(id(n)),
        }
    }

    fn vis(rng: &mut SplitMix64) -> Visibility {
        match rng.below(4) {
            0 => Visibility::Public,
            1 => Visibility::PubCrate(id(rng.below(2))),
            2 => Visibility::Module(id(rng.below(3)), VisibilityExplicitness::Explicit),
            _ => Visibility::Module(id(rng.below(3)), VisibilityExplicitness::Implicit),
        }
    }

    fn import(rng: &mut SplitMix64) -> Option<ImportOrExternCrate> {
        let use_ = id(rng.below(4));
        let idx = tree_at(rng.below(3) as u32);
        match rng.below(4) {
            0 => None,
            1 => Some(ImportOrExternCrate::Glob(GlobId { use_, idx })),
            2 => Some(ImportOrExternCrate::Import(ImportId { use_, idx })),
            _ => Some(ImportOrExternCrate::ExternCrate(id(rng.below(3)))),
        }
    }

    fn per_ns(rng: &mut SplitMix64) -> PerNs {
        let import = import(rng);
        let item = |rng: &mut SplitMix64| (def(rng), vis(rng));
        PerNs {
            types: rng.one_in(2).then(|| {
                let (def, vis) = item(rng);
                Item { def, vis, import }
            }),
            values: rng.one_in(2).then(|| {
                let (def, vis) = item(rng);
                Item { def, vis, import: import.and_then(ImportOrExternCrate::import_or_glob) }
            }),
            macros: rng.one_in(3).then(|| Item { def: makro(rng), vis: vis(rng), import }),
        }
    }

    /// The scope as it was: three `IndexMap`s, a hash set and hash maps, written as they were.
    #[derive(Default)]
    struct Model {
        types: FxIndexMap<Name, TypesItem>,
        values: FxIndexMap<Name, ValuesItem>,
        macros: FxIndexMap<Name, MacrosItem>,
        unresolved: HashSet<Name>,
        use_imports_types: HashMap<ImportOrExternCrate, ImportOrDef>,
        use_imports_values: HashMap<ImportOrGlob, ImportOrDef>,
        use_imports_macros: HashMap<ImportOrExternCrate, ImportOrDef>,
        legacy_macros: HashMap<Name, Vec<MacroId>>,
    }

    impl Model {
        fn get(&self, name: &Name) -> PerNs {
            PerNs {
                types: self.types.get(name).copied(),
                values: self.values.get(name).copied(),
                macros: self.macros.get(name).copied(),
            }
        }

        fn push_res_with_import(
            &mut self,
            glob_imports: &mut PerNsGlobImports,
            lookup: (ModuleId, Name),
            def: PerNs,
            import: Option<ImportOrExternCrate>,
        ) -> bool {
            let mut changed = false;
            if let Some(mut fld) = def.types {
                match self.types.entry(lookup.1) {
                    MapEntry::Vacant(entry) => {
                        match import {
                            Some(ImportOrExternCrate::Glob(_)) => {
                                glob_imports.types.insert(lookup);
                            }
                            _ => _ = glob_imports.types.remove(&lookup),
                        }
                        let prev = std::mem::replace(&mut fld.import, import);
                        if let Some(import) = import
                            && import.glob().is_none()
                        {
                            self.use_imports_types
                                .insert(import, prev.map_or(ImportOrDef::Def(fld.def), Into::into));
                        }
                        entry.insert(fld);
                        changed = true;
                    }
                    MapEntry::Occupied(mut entry) => match import {
                        Some(ImportOrExternCrate::Glob(..)) => {}
                        _ => {
                            if glob_imports.types.remove(&lookup)
                                || entry.get().is_reresolved_by(&fld, import)
                            {
                                let prev = std::mem::replace(&mut fld.import, import);
                                if let Some(import) = import
                                    && import.glob().is_none()
                                {
                                    self.use_imports_types.insert(
                                        import,
                                        prev.map_or(ImportOrDef::Def(fld.def), Into::into),
                                    );
                                }
                                entry.insert(fld);
                                changed = true;
                            }
                        }
                    },
                }
            }
            if let Some(mut fld) = def.values {
                match self.values.entry(lookup.1) {
                    MapEntry::Vacant(entry) => {
                        match import {
                            Some(ImportOrExternCrate::Glob(_)) => {
                                glob_imports.values.insert(lookup);
                            }
                            _ => _ = glob_imports.values.remove(&lookup),
                        }
                        let import = import.and_then(ImportOrExternCrate::import_or_glob);
                        let prev = std::mem::replace(&mut fld.import, import);
                        if let Some(import) = import
                            && import.into_import().is_some()
                        {
                            self.use_imports_values
                                .insert(import, prev.map_or(ImportOrDef::Def(fld.def), Into::into));
                        }
                        entry.insert(fld);
                        changed = true;
                    }
                    MapEntry::Occupied(mut entry)
                        if !matches!(import, Some(ImportOrExternCrate::Glob(..))) =>
                    {
                        let import = import.and_then(ImportOrExternCrate::import_or_glob);
                        if glob_imports.values.remove(&lookup)
                            || entry.get().is_reresolved_by(&fld, import)
                        {
                            let prev = std::mem::replace(&mut fld.import, import);
                            if let Some(import) = import
                                && import.into_import().is_some()
                            {
                                self.use_imports_values.insert(
                                    import,
                                    prev.map_or(ImportOrDef::Def(fld.def), Into::into),
                                );
                            }
                            entry.insert(fld);
                            changed = true;
                        }
                    }
                    _ => {}
                }
            }
            if let Some(mut fld) = def.macros {
                match self.macros.entry(lookup.1) {
                    MapEntry::Vacant(entry) => {
                        match import {
                            Some(ImportOrExternCrate::Glob(_)) => {
                                glob_imports.macros.insert(lookup);
                            }
                            _ => _ = glob_imports.macros.remove(&lookup),
                        }
                        let prev = std::mem::replace(&mut fld.import, import);
                        if let Some(import) = import
                            && import.glob().is_none()
                        {
                            self.use_imports_macros.insert(
                                import,
                                prev.map_or_else(|| ImportOrDef::Def(fld.def.into()), Into::into),
                            );
                        }
                        entry.insert(fld);
                        changed = true;
                    }
                    MapEntry::Occupied(mut entry)
                        if !matches!(import, Some(ImportOrExternCrate::Glob(..)))
                            && (glob_imports.macros.remove(&lookup)
                                || entry.get().is_reresolved_by(&fld, import)) =>
                    {
                        let prev = std::mem::replace(&mut fld.import, import);
                        if let Some(import) = import
                            && import.glob().is_none()
                        {
                            self.use_imports_macros.insert(
                                import,
                                prev.map_or_else(|| ImportOrDef::Def(fld.def.into()), Into::into),
                            );
                        }
                        entry.insert(fld);
                        changed = true;
                    }
                    _ => {}
                }
            }
            if def.is_none() && self.unresolved.insert(lookup.1) {
                changed = true;
            }
            changed
        }

        fn censor_non_proc_macros(&mut self, krate: Crate) {
            let vis = Visibility::PubCrate(krate);
            self.types.values_mut().for_each(|it| it.vis = vis);
            self.values.values_mut().for_each(|it| it.vis = vis);
            for it in self.macros.values_mut() {
                if !matches!(it.def, MacroId::ProcMacroId(_) if it.import.is_none()) {
                    it.vis = vis;
                }
            }
        }
    }

    #[track_caller]
    fn check(scope: &ItemScope, model: &Model, names: &[Name]) {
        for name in names {
            assert_eq!(scope.get(name), model.get(name), "{name:?}");
            assert_eq!(scope.type_(name), model.types.get(name).map(|it| (it.def, it.vis)));
            assert_eq!(scope.makro(name), model.macros.get(name).map(|it| it.def));
            assert_eq!(
                scope.get_legacy_macro(name),
                model.legacy_macros.get(name).map(Vec::as_slice)
            );
        }
        assert!(scope.types().eq(model.types.iter().map(|(n, it)| (n, *it))));
        assert!(scope.values().eq(model.values.iter().map(|(n, it)| (n, *it))));
        assert!(scope.macros().eq(model.macros.iter().map(|(n, it)| (n, *it))));
        let expected = (model.types.keys())
            .chain(model.values.keys())
            .chain(model.macros.keys())
            .chain(model.unresolved.iter())
            .sorted()
            .dedup()
            .map(|name| (*name, model.get(name)));
        assert!(scope.entries().map(|(name, it)| (*name, it)).eq(expected));
        assert_eq!(scope.use_imports_types.len(), model.use_imports_types.len());
        for (k, v) in &model.use_imports_types {
            assert_eq!(scope.use_imports_types.get(k), Some(v));
        }
        assert_eq!(scope.use_imports_values.len(), model.use_imports_values.len());
        for (k, v) in &model.use_imports_values {
            assert_eq!(scope.use_imports_values.get(k), Some(v));
        }
        assert_eq!(scope.use_imports_macros.len(), model.use_imports_macros.len());
        for (k, v) in &model.use_imports_macros {
            assert_eq!(scope.use_imports_macros.get(k), Some(v));
        }
        for (name, item) in &model.types {
            let first = model.types.iter().find(|(_, it)| it.def == item.def).unwrap();
            let expected = (first.0, first.1.vis, first.1.import.is_none());
            assert_eq!(scope.name_of(ItemInNs::Types(item.def)), Some(expected), "{name:?}");
        }
        for item in model.macros.values() {
            let first = model.macros.iter().find(|(_, it)| it.def == item.def).unwrap();
            let expected = (first.0, first.1.vis, first.1.import.is_none());
            assert_eq!(scope.name_of(ItemInNs::Macros(item.def)), Some(expected));
        }
        let names_of = |item: ItemInNs| {
            let mut all = Vec::new();
            scope.names_of(item, |name, vis, declared| {
                all.push((*name, vis, declared));
                None::<()>
            });
            all
        };
        for item in model.values.values() {
            let expected: Vec<_> = (model.values.iter())
                .filter(|(_, it)| it.def == item.def)
                .map(|(name, it)| (*name, it.vis, it.import.is_none()))
                .collect();
            assert_eq!(names_of(ItemInNs::Values(item.def)), expected);
        }
        for item in model.types.values() {
            let expected: Vec<_> = (model.types.iter())
                .filter(|(_, it)| it.def == item.def)
                .map(|(name, it)| (*name, it.vis, it.import.is_none()))
                .collect();
            assert_eq!(names_of(ItemInNs::Types(item.def)), expected);
        }
        let modules = model.types.values().filter_map(|it| match it.def {
            ModuleDefId::ModuleId(module) => Some((module, it.vis)),
            _ => None,
        });
        assert!(scope.modules_in_scope().eq(modules));
        let traits = model.types.values().filter_map(|it| match it.def {
            ModuleDefId::TraitId(it) => Some(it),
            _ => None,
        });
        assert!(scope.traits().eq(traits));
        assert_eq!(scope.clone(), *scope);
    }

    /// The columns give what the maps gave, after every step of a seeded random sequence of
    /// what collection does to a scope.
    #[test]
    fn columns_answer_as_the_maps_did() {
        let names = names();
        for seed in [1u64, 0x9e37_79b9, 0xdead_beef_cafe] {
            let mut rng = SplitMix64::new(seed);
            let (mut scope, mut model) = (ItemScope::default(), Model::default());
            let (mut globs, mut model_globs) =
                (PerNsGlobImports::default(), PerNsGlobImports::default());
            for _ in 0..600 {
                let name = names[rng.below(names.len() as u64) as usize];
                match rng.below(16) {
                    0..=8 => {
                        let lookup = (id(rng.below(2)), name);
                        let def =
                            if rng.one_in(8) { PerNs::none() } else { per_ns(&mut rng) };
                        let import = import(&mut rng);
                        assert_eq!(
                            scope.push_res_with_import(&mut globs, lookup, def, import),
                            model.push_res_with_import(&mut model_globs, lookup, def, import),
                        );
                    }
                    9 => {
                        let vis = vis(&mut rng);
                        let glob = GlobId { use_: id(rng.below(4)), idx: tree_at(rng.below(3) as u32) };
                        let ns = [Namespace::Types, Namespace::Values, Namespace::Macros]
                            [rng.below(3) as usize];
                        match ns {
                            Namespace::Types if model.types.contains_key(&name) => {
                                let it = &mut model.types[&name];
                                it.vis = vis;
                                scope.rebind_glob_types(&name, *it, glob);
                                it.import = Some(ImportOrExternCrate::Glob(glob));
                            }
                            Namespace::Values if model.values.contains_key(&name) => {
                                let it = &mut model.values[&name];
                                it.vis = vis;
                                scope.rebind_glob_values(&name, *it, glob);
                                it.import = Some(ImportOrGlob::Glob(glob));
                            }
                            Namespace::Macros if model.macros.contains_key(&name) => {
                                let it = &mut model.macros[&name];
                                it.vis = vis;
                                scope.rebind_glob_macros(&name, *it, glob);
                                it.import = Some(ImportOrExternCrate::Glob(glob));
                            }
                            _ => {}
                        }
                    }
                    10 => {
                        let (def, vis) = (def(&mut rng), vis(&mut rng));
                        if model.types.contains_key(&name) {
                            scope.update_def_types(&name, def, vis);
                            let it = &mut model.types[&name];
                            (it.def, it.vis) = (def, vis);
                        }
                        if model.values.contains_key(&name) {
                            scope.update_def_values(&name, def, vis);
                            let it = &mut model.values[&name];
                            (it.def, it.vis) = (def, vis);
                        }
                        let mac = makro(&mut rng);
                        if model.macros.contains_key(&name) {
                            scope.update_def_macros(&name, mac, vis);
                            let it = &mut model.macros[&name];
                            (it.def, it.vis) = (mac, vis);
                        }
                    }
                    11 | 12 => {
                        let mac = makro(&mut rng);
                        scope.define_legacy_macro(name, mac);
                        model.legacy_macros.entry(name).or_default().push(mac);
                    }
                    13 if rng.one_in(8) => {
                        let krate = id(rng.below(2));
                        scope.censor_non_proc_macros(krate);
                        model.censor_non_proc_macros(krate);
                    }
                    14 if rng.one_in(4) => scope.settle(),
                    _ => {}
                }
                check(&scope, &model, &names);
            }
            let mut sealed = scope.clone();
            assert_eq!(sealed.seal(), Ok(()));
            assert_eq!(sealed, scope);
            check(&sealed, &model, &names);
        }
    }

    fn item(def: u64) -> TypesItem {
        Item { def: ModuleDefId::FunctionId(id(def)), vis: Visibility::Public, import: None }
    }

    fn scope_of(names: &[Name], unresolved: &[Name]) -> ItemScope {
        let mut scope = ItemScope::default();
        let mut globs = PerNsGlobImports::default();
        for &name in names {
            let def = PerNs { types: Some(item(0)), values: None, macros: None };
            scope.push_res_with_import(&mut globs, (id(0), name), def, None);
        }
        for &name in unresolved {
            scope.push_res_with_import(&mut globs, (id(0), name), PerNs::none(), None);
        }
        scope
    }

    /// Equal as the maps were, but for the namespaces' order, which is what they list.
    #[test]
    fn scopes_are_equal_by_their_columns() {
        let all = names();
        let (a, b, c, d) = (all[0], all[1], all[2], all[3]);
        assert_eq!(scope_of(&[a, b], &[c, d]), scope_of(&[a, b], &[c, d]));
        assert_eq!(scope_of(&[a, b], &[c, d]), scope_of(&[a, b], &[d, c]));
        assert_ne!(scope_of(&[a, b], &[]), scope_of(&[b, a], &[]));
        assert_ne!(scope_of(&[a, b], &[c]), scope_of(&[a, b], &[d]));
        let many: Vec<_> = all[..30].to_vec();
        let mut reversed = all[10..30].to_vec();
        reversed.reverse();
        assert_eq!(scope_of(&many, &reversed), scope_of(&many, &all[10..30]));
        let mut sealed = scope_of(&many, &reversed);
        sealed.settle();
        assert_eq!(sealed, scope_of(&many, &reversed));
    }

    #[test]
    fn a_frozen_scope_with_a_name_twice_does_not_seal() {
        let all = names();
        for len in [2, 20] {
            let mut scope = ItemScope::default();
            for name in &all[..len] {
                scope.types.push_unindexed(*name, item(0));
            }
            assert_eq!(scope.clone().seal(), Ok(()));
            scope.types.push_unindexed(all[len / 2], item(1));
            assert!(scope.seal().is_err());
        }
    }

    /// `Keyed` maps as a hash map does, past the size it starts indexing at.
    #[test]
    fn keyed_maps_as_a_hash_map() {
        let mut rng = SplitMix64::new(7);
        let mut keyed: Keyed<u32, u64> = Keyed::default();
        let mut map: HashMap<u32, u64> = HashMap::default();
        for step in 0..3000u64 {
            let key = rng.below(400) as u32;
            if rng.one_in(3) {
                *keyed.or_insert_with(key, || 0) += step;
                *map.entry(key).or_insert(0) += step;
            } else {
                keyed.insert(key, step);
                map.insert(key, step);
            }
            if rng.one_in(500) {
                keyed.settle();
            }
            assert_eq!(keyed.len(), map.len());
            for probe in 0..400 {
                assert_eq!(keyed.get(&probe), map.get(&probe));
            }
        }
        let mut reordered: Keyed<u32, u64> = Keyed::default();
        let mut sorted: Vec<_> = map.iter().collect();
        sorted.sort();
        for &(&k, &v) in sorted.iter().rev() {
            reordered.insert(k, v);
        }
        assert_eq!(reordered, keyed);
        reordered.insert(sorted[0].0 + 1000, 0);
        assert_ne!(reordered, keyed);
    }

    #[test]
    fn builtins_are_found_by_name() {
        let all = BuiltinType::all_builtin_types();
        assert_eq!(all.map(|(_, it)| it), BUILTIN_TYPES);
        for (index, (name, ty)) in all.iter().enumerate() {
            assert_eq!(builtin_index(*ty) as usize, index);
            let expected = PerNs::types((*ty).into(), Visibility::Public, None);
            assert_eq!(BUILTIN_SCOPE.get(name).copied(), Some(expected));
        }
        let listed = BUILTIN_SCOPE.iter().map(|(name, _)| *name);
        assert!(listed.eq(all.iter().map(|(name, _)| *name)));
        for name in names() {
            assert_eq!(BUILTIN_SCOPE.get(&name), None);
        }
    }

    /// Every definition, visibility and import packs and reads back as itself.
    #[test]
    fn entries_read_back_what_was_packed() {
        let mut rng = SplitMix64::new(11);
        let name = names()[0];
        for _ in 0..2000 {
            let item = Item { def: def(&mut rng), vis: vis(&mut rng), import: import(&mut rng) };
            let entry = Entry::new(name, item);
            assert_eq!(entry.item::<ModuleDefId, ImportOrExternCrate>(), item);
            assert!(entry.is_def(item.def.pack()));
            let import = item.import.and_then(ImportOrExternCrate::import_or_glob);
            let value = Item { def: item.def, vis: item.vis, import };
            assert_eq!(Entry::new(name, value).item::<ModuleDefId, ImportOrGlob>(), value);
        }
    }

    /// The sizes the notes give (`docs/v2/plan/lanes/db/speed-hir-nameres-scope.md`), with
    /// ids of 4 bytes (`blaze_query::db::Id`).
    #[test]
    fn sizes() {
        assert_eq!(size_of::<Entry>(), 24);
        assert_eq!(size_of::<Index>(), 8);
        assert_eq!(size_of::<Visibility>(), 8);
        assert_eq!(size_of::<ModuleDefId>(), 12);
        assert_eq!(size_of::<TypesItem>(), 32);
        assert_eq!(size_of::<ValuesItem>(), 32);
        assert_eq!(size_of::<MacrosItem>(), 28);
        assert_eq!(size_of::<PerNs>(), 92);
    }
}
