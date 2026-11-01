//! `hir_def` crate contains everything between macro expansion and type
//! inference.
//!
//! It defines various items (structs, enums, traits) which comprises Rust code,
//! as well as an algorithm for resolving paths to such entities.
//!
//! Note that `hir_def` is a work in progress, so not all of the above is
//! actually true.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![feature(allocator_api, allocator_ext)]
#![deny(unsafe_code)]

extern crate rustc_parse_format;

pub extern crate rustc_abi;
pub extern crate rustc_abi as layout;

pub mod attrs;
pub mod builtin_type;
pub mod item_scope;
pub mod per_ns;

pub mod signatures;

pub mod dyn_map;

pub mod item_tree;

pub mod builtin_derive;
pub mod diagnostic_item;
pub mod lang_item;
pub mod macro_reach;
pub mod unstable_features;

pub mod expr_store;
pub mod hir;
pub mod item_hash;
pub mod resolver;
pub mod span_names;
pub mod upvars;

pub mod nameres;

pub mod deps;

pub mod src;

pub mod find_path;
pub mod import_map;
pub mod visibility;
pub mod test_items;

#[cfg(test)]
mod alloc_budgets;
#[cfg(test)]
mod macro_expansion_tests;
#[cfg(test)]
mod nameres_budgets;
#[cfg(test)]
mod test_db;

use std::{
    alloc::Allocator,
    fmt,
    hash::{Hash, Hasher},
};

use base_db::{Crate, Db, impl_intern_key};
use hir_expand::{
    AstId, EditionedFileId, ExpandResult, ExpandTo, HirFileId, InFile, MacroCallId, MacroCallKind,
    MacroCallStyles, MacroDefId, MacroDefKind,
    attrs::AttrId,
    builtin::{BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander},
    eager::{EagerCall, expand_eager_macro_input_view},
    impl_intern_lookup,
    mod_path::ModPath,
    name::Name,
    proc_macro::{CustomProcMacroExpander, ProcMacroKind},
};
use intern::sym;
use nameres::DefMap;
use rustc_abi::ExternAbi;
use span::{AstIdNode, Edition, FileAstId, SyntaxContext};
use stdx::impl_from;
use syntax::{AstNode, ast};
use thin_vec::ThinVec;

use crate::{
    attrs::AttrFlags,
    builtin_derive::BuiltinDeriveImplTrait,
    builtin_type::BuiltinType,
    expr_store::ExpressionStoreSourceMap,
    hir::generics::{GenericParams, LocalLifetimeParamId, LocalTypeOrConstParamId},
    nameres::{
        LocalDefMap,
        assoc::{ImplItems, TraitItems},
        block_def_map, crate_def_map, crate_local_def_map,
        diagnostics::DefDiagnostics,
    },
    signatures::{EnumVariants, InactiveEnumVariantCode, VariantFields},
};

pub use crate::{
    find_path::FindPathConfig, hir::type_ref, item_tree::file_item_tree,
    lang_item::crate_notable_traits, signatures::LocalFieldId,
};
pub use hir_expand::{Intern, Lookup, tt};

type FxIndexMap<K, V> = indexmap::IndexMap<K, V, blaze_hash::FixedState>;

#[derive(Debug)]
pub struct ItemLoc<N: AstIdNode> {
    pub container: ModuleId,
    pub id: AstId<N>,
}

impl<N: AstIdNode> Clone for ItemLoc<N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N: AstIdNode> Copy for ItemLoc<N> {}

impl<N: AstIdNode> PartialEq for ItemLoc<N> {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container && self.id == other.id
    }
}

impl<N: AstIdNode> Eq for ItemLoc<N> {}

impl<N: AstIdNode> Hash for ItemLoc<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ItemLoc { container, id } = *self;
        container.hash(state);
        let word = hash_ast_id(id, state);
        state.write_u64(word);
    }
}

/// Hashes the id of `id`'s file alone and returns the rest of `id` as the low 40 bits of a word
/// (the file-local id, then the file's variant), for a loc to fill the high bits with its own
/// small fields and hash the word once.
#[inline]
fn hash_ast_id<N, H: Hasher>(id: AstId<N>, state: &mut H) -> u64 {
    let file: u64 = match id.file_id {
        HirFileId::FileId(it) => {
            it.hash(state);
            0
        }
        HirFileId::MacroFile(it) => {
            it.hash(state);
            1
        }
    };
    u64::from(id.value.erase().into_raw()) | file << 32
}

// An `AstId` is 12 bytes: the file 8 (a tag word and an id), the file-local id 4.
const _: () = assert!(size_of::<AstId<ast::Struct>>() == 12);

// 16 bytes per item (module 4, `AstId` 12), no padding; hashed in 3 writes (module, file,
// file-local id with the file's variant).
const _: () = assert!(size_of::<ItemLoc<ast::Struct>>() == 16);
const _: () = assert!(align_of::<ItemLoc<ast::Struct>>() == 4);

impl<N: AstIdNode> HasModule for ItemLoc<N> {
    #[inline]
    fn module(&self, _db: &Db) -> ModuleId {
        self.container
    }
}

#[derive(Debug)]
pub struct AssocItemLoc<N: AstIdNode> {
    // FIXME: Store this as an erased `blaze_query::db::Id` to save space
    pub container: ItemContainerId,
    pub id: AstId<N>,
}

impl<N: AstIdNode> Clone for AssocItemLoc<N> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<N: AstIdNode> Copy for AssocItemLoc<N> {}

impl<N: AstIdNode> PartialEq for AssocItemLoc<N> {
    fn eq(&self, other: &Self) -> bool {
        self.container == other.container && self.id == other.id
    }
}

impl<N: AstIdNode> Eq for AssocItemLoc<N> {}

impl<N: AstIdNode> Hash for AssocItemLoc<N> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let AssocItemLoc { container, id } = *self;
        let container = container.hash_id(state);
        let word = hash_ast_id(id, state);
        state.write_u64(word | u64::from(container) << 40);
    }
}

// 20 bytes per item (container 8: a tag word and an id, `AstId` 12); hashed in 3 writes
// (container, file, file-local id with both variants).
const _: () = assert!(size_of::<AssocItemLoc<ast::Fn>>() == 20);
const _: () = assert!(align_of::<AssocItemLoc<ast::Fn>>() == 4);

impl<N: AstIdNode> HasModule for AssocItemLoc<N> {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.container.module(db)
    }
}

pub trait AstIdLoc {
    type Container;
    type Ast: AstNode;
    fn ast_id(&self) -> AstId<Self::Ast>;
    fn container(&self) -> Self::Container;
}

impl<N: AstIdNode> AstIdLoc for ItemLoc<N> {
    type Container = ModuleId;
    type Ast = N;
    #[inline]
    fn ast_id(&self) -> AstId<Self::Ast> {
        self.id
    }
    #[inline]
    fn container(&self) -> Self::Container {
        self.container
    }
}

impl<N: AstIdNode> AstIdLoc for AssocItemLoc<N> {
    type Container = ItemContainerId;
    type Ast = N;
    #[inline]
    fn ast_id(&self) -> AstId<Self::Ast> {
        self.id
    }
    #[inline]
    fn container(&self) -> Self::Container {
        self.container
    }
}

macro_rules! impl_intern {
    ($id:ident, $loc:ident) => {
        impl_intern_key!($id, $loc);
        impl_intern_lookup!($id, $loc);
    };
}

macro_rules! impl_loc {
    ($loc:ident, $id:ident: $id_ty:ident, $container:ident: $container_type:ident) => {
        impl AstIdLoc for $loc {
            type Container = $container_type;
            type Ast = ast::$id_ty;
            fn ast_id(&self) -> AstId<Self::Ast> {
                self.$id
            }
            fn container(&self) -> Self::Container {
                self.$container
            }
        }

        impl HasModule for $loc {
            #[inline]
            fn module(&self, db: &Db) -> ModuleId {
                self.$container.module(db)
            }
        }
    };
}

type FunctionLoc = AssocItemLoc<ast::Fn>;
impl_intern!(FunctionId, FunctionLoc);

type StructLoc = ItemLoc<ast::Struct>;
impl_intern!(StructId, StructLoc);

impl StructId {
    pub fn fields(self, db: &Db) -> &VariantFields {
        VariantFields::of(db, self.into())
    }

    pub fn fields_with_source_map(
        self,
        db: &Db,
    ) -> (&VariantFields, &ExpressionStoreSourceMap) {
        let r = VariantFields::with_source_map(db, self.into());
        (&r.0, &r.1)
    }
}

pub type UnionLoc = ItemLoc<ast::Union>;
impl_intern!(UnionId, UnionLoc);

impl UnionId {
    pub fn fields(self, db: &Db) -> &VariantFields {
        VariantFields::of(db, self.into())
    }

    pub fn fields_with_source_map(
        self,
        db: &Db,
    ) -> (&VariantFields, &ExpressionStoreSourceMap) {
        let r = VariantFields::with_source_map(db, self.into());
        (&r.0, &r.1)
    }
}

pub type EnumLoc = ItemLoc<ast::Enum>;
impl_intern!(EnumId, EnumLoc);

impl EnumId {
    #[inline]
    pub fn enum_variants(self, db: &Db) -> &EnumVariants {
        &self.enum_variants_with_diagnostics(db).0
    }

    #[inline]
    pub fn enum_variants_with_diagnostics(
        self,
        db: &Db,
    ) -> &(EnumVariants, ThinVec<InactiveEnumVariantCode>) {
        EnumVariants::of(db, self)
    }
}

type ConstLoc = AssocItemLoc<ast::Const>;
impl_intern!(ConstId, ConstLoc);

pub type StaticLoc = AssocItemLoc<ast::Static>;
impl_intern!(StaticId, StaticLoc);

pub type TraitLoc = ItemLoc<ast::Trait>;
impl_intern!(TraitId, TraitLoc);

impl TraitId {
    #[inline]
    pub fn trait_items(self, db: &Db) -> &TraitItems {
        TraitItems::query(db, self)
    }
}

type TypeAliasLoc = AssocItemLoc<ast::TypeAlias>;
impl_intern!(TypeAliasId, TypeAliasLoc);

type ImplLoc = ItemLoc<ast::Impl>;
impl_intern!(ImplId, ImplLoc);

impl ImplId {
    #[inline]
    pub fn impl_items(self, db: &Db) -> &ImplItems {
        &self.impl_items_with_diagnostics(db).0
    }

    #[inline]
    pub fn impl_items_with_diagnostics(
        self,
        db: &Db,
    ) -> &(ImplItems, DefDiagnostics) {
        ImplItems::of(db, self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuiltinDeriveImplLoc {
    pub adt: AdtId,
    pub trait_: BuiltinDeriveImplTrait,
    pub derive_attr_id: AttrId,
    pub derive_index: u32,
}

impl Hash for BuiltinDeriveImplLoc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let BuiltinDeriveImplLoc { adt, trait_, derive_attr_id, derive_index } = *self;
        let adt = adt.hash_id(state);
        derive_attr_id.hash(state);
        state.write_u64(
            u64::from(derive_index) | u64::from(adt) << 32 | u64::from(trait_ as u8) << 40,
        );
    }
}

// 20 bytes per impl (adt 8, attr id 4, index 4, trait 1, 3 of padding); hashed in 3 writes.
const _: () = assert!(size_of::<BuiltinDeriveImplLoc>() == 20);

#[blaze_query::interned(debug, unsafe(no_lifetime))]
#[derive(PartialOrd, Ord)]
pub struct BuiltinDeriveImplId {
    #[returns(ref)]
    pub loc: BuiltinDeriveImplLoc,
}

type UseLoc = ItemLoc<ast::Use>;
impl_intern!(UseId, UseLoc);

type ExternCrateLoc = ItemLoc<ast::ExternCrate>;
impl_intern!(ExternCrateId, ExternCrateLoc);

type ExternBlockLoc = ItemLoc<ast::ExternBlock>;
impl_intern!(ExternBlockId, ExternBlockLoc);

impl ExternBlockId {
    pub fn abi(self, db: &Db) -> ExternAbi {
        signatures::extern_block_abi(db, self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumVariantLoc {
    pub id: AstId<ast::Variant>,
    pub parent: EnumId,
    pub name: Name,
}
impl_intern!(EnumVariantId, EnumVariantLoc);
impl_loc!(EnumVariantLoc, id: Variant, parent: EnumId);

impl Hash for EnumVariantLoc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let EnumVariantLoc { id, parent, name } = self;
        let word = hash_ast_id(*id, state);
        state.write_u64(word);
        parent.hash(state);
        name.hash(state);
    }
}

// 20 bytes per variant (`AstId` 12, enum 4, name 4), no padding; hashed in 4 writes (file,
// file-local id, enum, name).
const _: () = assert!(size_of::<EnumVariantLoc>() == 20);

impl EnumVariantLoc {
    pub fn index(&self, db: &Db) -> usize {
        self.parent
            .enum_variants(db)
            .variants
            .get_full(&self.name)
            .expect("parent enum should include this variant")
            .0
    }
}

impl EnumVariantId {
    pub fn fields(self, db: &Db) -> &VariantFields {
        VariantFields::of(db, self.into())
    }

    pub fn index(self, db: &Db) -> usize {
        self.loc(db).index(db)
    }

    pub fn fields_with_source_map(
        self,
        db: &Db,
    ) -> (&VariantFields, &ExpressionStoreSourceMap) {
        let r = VariantFields::with_source_map(db, self.into());
        (&r.0, &r.1)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Macro2Loc {
    pub container: ModuleId,
    pub id: AstId<ast::MacroDef>,
    pub expander: MacroExpander,
    pub allow_internal_unsafe: bool,
    pub edition: Edition,
}
impl_intern!(Macro2Id, Macro2Loc);
impl_loc!(Macro2Loc, id: MacroDef, container: ModuleId);

impl Hash for Macro2Loc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let Macro2Loc { container, id, expander, allow_internal_unsafe, edition } = *self;
        container.hash(state);
        let word = hash_ast_id(id, state);
        state.write_u64(
            word | u64::from(expander.hash_word()) << 40
                | (u64::from(allow_internal_unsafe) | u64::from(edition as u8) << 1) << 56,
        );
    }
}

// 20 bytes per macro (module 4, `AstId` 12, expander 2, flag 1, edition 1), no padding; hashed
// in 3 writes.
const _: () = assert!(size_of::<Macro2Loc>() == 20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacroRulesLoc {
    pub container: ModuleId,
    pub id: AstId<ast::MacroRules>,
    pub expander: MacroExpander,
    pub flags: MacroRulesLocFlags,
    pub edition: Edition,
}
impl_intern!(MacroRulesId, MacroRulesLoc);
impl_loc!(MacroRulesLoc, id: MacroRules, container: ModuleId);

impl Hash for MacroRulesLoc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let MacroRulesLoc { container, id, expander, flags, edition } = *self;
        container.hash(state);
        let word = hash_ast_id(id, state);
        state.write_u64(
            word | u64::from(expander.hash_word()) << 40
                | (u64::from(flags.bits()) | u64::from(edition as u8) << 3) << 56,
        );
    }
}

// 20 bytes per macro (module 4, `AstId` 12, expander 2, flags 1, edition 1), no padding; hashed
// in 3 writes.
const _: () = assert!(size_of::<MacroRulesLoc>() == 20);

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct MacroRulesLocFlags: u8 {
        const ALLOW_INTERNAL_UNSAFE = 1 << 0;
        const LOCAL_INNER = 1 << 1;
        const EDITION_PANIC = 1 << 2;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MacroExpander {
    Declarative { styles: MacroCallStyles },
    BuiltIn(BuiltinFnLikeExpander),
    BuiltInAttr(BuiltinAttrExpander),
    BuiltInDerive(BuiltinDeriveExpander),
    BuiltInEager(EagerExpander),
    UnimplementedBuiltIn,
}

const _: () = assert!(size_of::<MacroExpander>() == 2);

impl MacroExpander {
    /// The variant in the high byte and its payload in the low one, so a loc hashes the expander
    /// with its other small fields in one word.
    #[inline]
    fn hash_word(self) -> u16 {
        let (variant, payload): (u8, u8) = match self {
            MacroExpander::Declarative { styles } => (0, styles.bits()),
            MacroExpander::BuiltIn(it) => (1, it as u8),
            MacroExpander::BuiltInAttr(it) => (2, it as u8),
            MacroExpander::BuiltInDerive(it) => (3, it as u8),
            MacroExpander::BuiltInEager(it) => (4, it as u8),
            MacroExpander::UnimplementedBuiltIn => (5, 0),
        };
        u16::from(variant) << 8 | u16::from(payload)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcMacroLoc {
    pub container: ModuleId,
    pub id: AstId<ast::Fn>,
    pub expander: CustomProcMacroExpander,
    pub kind: ProcMacroKind,
    pub edition: Edition,
}
impl_intern!(ProcMacroId, ProcMacroLoc);
impl_loc!(ProcMacroLoc, id: Fn, container: ModuleId);

impl Hash for ProcMacroLoc {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ProcMacroLoc { container, id, expander, kind, edition } = *self;
        container.hash(state);
        let word = hash_ast_id(id, state);
        expander.hash(state);
        state.write_u64(word | u64::from(kind as u8) << 40 | u64::from(edition as u8) << 48);
    }
}

// 24 bytes per proc macro (module 4, `AstId` 12, expander 4, kind 1, edition 1, 2 of padding);
// hashed in 4 writes (module, file, expander, the rest in one word).
const _: () = assert!(size_of::<ProcMacroLoc>() == 24);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub enum LoweringMode {
    Analysis,
    Ide,
}

pub use self::tracked_struct_token::TrackedStructToken;
mod tracked_struct_token {
    use super::LoweringMode;

    /// A token that is required to construct tracked structs.
    /// This exists to prevent one from accidentally creating a tracked struct outside of a query which may happen for some codepaths.
    pub struct TrackedStructToken {
        // #[non_exhaustive] doesn't work for us here, we want it module focused.
        _private: (),
    }

    impl LoweringMode {
        pub fn allow_tracked_structs(self) -> Option<TrackedStructToken> {
            match self {
                LoweringMode::Analysis => Some(TrackedStructToken { _private: () }),
                LoweringMode::Ide => None,
            }
        }
    }
}

#[blaze_query::tracked(constructor = new_)]
#[derive(PartialOrd, Ord)]
pub struct BlockIdLt<'db> {
    #[returns(copy)]
    pub ast_id: AstId<ast::BlockExpr>,
    /// The containing module.
    #[returns(copy)]
    pub module: ModuleIdLt<'db>,
    /// The body whose lowering from its own syntax ([`crate::expr_store::body::body_syntax`])
    /// made the block; `None` for a block of a signature's expression (an array length, a
    /// const argument) or of a body lowered from its whole file. The item tree of a block written in that body's
    /// text is lowered from the body's syntax alone, so an edit elsewhere in the file leaves it
    /// be; one in a macro's expansion, from the expansion.
    #[returns(copy)]
    pub owner: Option<DefWithBodyId>,
}
pub type BlockId = BlockIdLt<'static>;

impl<'db> fmt::Debug for BlockIdLt<'db> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("BlockId").field(&self.0).finish()
    }
}

impl<'db> BlockIdLt<'db> {
    pub fn new(
        db: &'db Db,
        ast_id: AstId<ast::BlockExpr>,
        module: ModuleIdLt<'db>,
        owner: Option<DefWithBodyId>,
        token: TrackedStructToken,
    ) -> Self {
        _ = token;
        BlockIdLt::new_(db, ast_id, module, owner)
    }

    /// The same id with its lifetime erased.
    ///
    /// # Contract
    ///
    /// The id is only meaningful in the revision it was read in. Read after a revision that
    /// deleted it, it gives the fields the `BlockId` last had (the query database frees no id);
    /// it is never UB (`blaze_query::db::FromId` is safe for every lifetime).
    pub fn to_static(self) -> BlockId {
        blaze_query::db::FromId::from_id(blaze_query::db::AsId::as_id(&self))
    }
}
impl BlockId {
    /// The same id bound to `_db`'s borrow.
    ///
    /// # Contract
    ///
    /// The id must come from `_db`, and is only meaningful in the revision it was read in;
    /// otherwise its fields are unspecified, never UB, as for [`BlockIdLt::to_static`].
    pub fn to_db<'db>(self, _db: &'db Db) -> BlockIdLt<'db> {
        blaze_query::db::FromId::from_id(blaze_query::db::AsId::as_id(&self))
    }
}

#[blaze_query::tracked(debug)]
#[derive(PartialOrd, Ord)]
pub struct ModuleIdLt<'db> {
    /// The crate this module belongs to.
    #[returns(copy)]
    pub krate: Crate,
    /// If this `ModuleId` was derived from a `DefMap` for a block expression, this stores the
    /// `BlockId` of that block expression. If `None`, this module is part of the crate-level
    /// `DefMap` of `krate`.
    #[returns(copy)]
    pub block: Option<BlockIdLt<'db>>,
    /// The parent module of this module, or `None` if this is the root module inside the def
    /// map (including for block def maps).
    #[returns(copy)]
    pub containing_module_inside_def_map: Option<ModuleIdLt<'db>>,
    /// The name of this module, or [`sym::__empty`] for the root module.
    #[returns(clone)]
    name_or_empty: Name,
}
pub type ModuleId = ModuleIdLt<'static>;

impl<'db> ModuleIdLt<'db> {
    /// The same id with its lifetime erased.
    ///
    /// # Contract
    ///
    /// The id is only meaningful in the revision it was read in. Read after a revision that
    /// deleted it, it gives the fields the `ModuleId` last had (the query database frees no id);
    /// it is never UB (`blaze_query::db::FromId` is safe for every lifetime).
    pub fn to_static(self) -> ModuleId {
        blaze_query::db::FromId::from_id(blaze_query::db::AsId::as_id(&self))
    }

    pub fn def_map(self, db: &'db Db) -> &'db DefMap {
        match self.block(db) {
            Some(block) => block_def_map(db, block),
            None => crate_def_map(db, self.krate(db)),
        }
    }

    pub(crate) fn local_def_map(
        self,
        db: &'db Db,
    ) -> (&'db DefMap, &'db LocalDefMap) {
        match self.block(db) {
            Some(block) => (block_def_map(db, block), self.only_local_def_map(db)),
            None => {
                let def_map = crate_local_def_map(db, self.krate(db));
                (def_map.def_map(db), def_map.local(db))
            }
        }
    }

    pub(crate) fn only_local_def_map(self, db: &'db Db) -> &'db LocalDefMap {
        crate_local_def_map(db, self.krate(db)).local(db)
    }

    pub fn crate_def_map(self, db: &'db Db) -> &'db DefMap {
        crate_def_map(db, self.krate(db))
    }

    pub fn name(self, db: &Db) -> Option<Name> {
        let name = self.name_or_empty(db);
        if *name.symbol() == sym::__empty { None } else { Some(name) }
    }

    /// Returns the module containing `self`, either the parent `mod`, or the module (or block) containing
    /// the block, if `self` corresponds to a block expression.
    pub fn containing_module(self, db: &'db Db) -> Option<ModuleIdLt<'db>> {
        self.containing_module_inside_def_map(db)
            .or_else(|| self.block(db).map(|block| block.module(db)))
    }

    pub fn is_block_module(self, db: &Db) -> bool {
        self.block(db).is_some() && self.containing_module_inside_def_map(db).is_none()
    }
}

impl ModuleId {
    /// The same id bound to `_db`'s borrow.
    ///
    /// # Contract
    ///
    /// The id must come from `_db`, and is only meaningful in the revision it was read in;
    /// otherwise its fields are unspecified, never UB, as for [`ModuleIdLt::to_static`].
    pub fn to_db<'db>(self, _db: &'db Db) -> ModuleIdLt<'db> {
        blaze_query::db::FromId::from_id(blaze_query::db::AsId::as_id(&self))
    }

    /// The prelude a name not in scope in this module is looked up in, and the
    /// `#[prelude_import]` naming it if one does: its crate's, which the resolver reads for a
    /// block's modules too.
    pub fn prelude(self, db: &Db) -> Option<(ModuleId, Option<UseId>)> {
        crate_def_map(db, self.krate(db)).prelude()
    }

    /// The extern prelude of this module's crate, in the order its crates were added: each
    /// name, the root of the crate it names and the `extern crate` that added it, if one did.
    pub fn extern_prelude<'db>(
        self,
        db: &'db Db,
    ) -> impl DoubleEndedIterator<Item = (&'db Name, (ModuleId, Option<ExternCrateId>))> + 'db
    {
        crate_local_def_map(db, self.krate(db)).local(db).extern_prelude()
    }

    /// The `#[macro_use]` prelude of this module's crate: each macro name, the macro and the
    /// `extern crate` it is imported through (none for one of the standard prelude).
    pub fn macro_use_prelude(
        self,
        db: &Db,
    ) -> &blaze_hash::HashMap<Name, (MacroId, Option<ExternCrateId>)> {
        crate_def_map(db, self.krate(db)).macro_use_prelude()
    }
}

impl HasModule for ModuleId {
    #[inline]
    fn module(&self, _db: &Db) -> ModuleId {
        *self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct FieldId {
    // FIXME: Store this as an erased `blaze_query::db::Id` to save space
    pub parent: VariantId,
    pub local_id: LocalFieldId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct TupleId(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Value)]
pub struct TupleFieldId {
    pub tuple: TupleId,
    pub index: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeOrConstParamId {
    // FIXME: Store this as an erased `blaze_query::db::Id` to save space
    pub parent: GenericDefId,
    pub local_id: LocalTypeOrConstParamId,
}

/// A TypeOrConstParamId with an invariant that it actually belongs to a type
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeParamId(TypeOrConstParamId);

impl TypeParamId {
    #[inline]
    pub fn parent(&self) -> GenericDefId {
        self.0.parent
    }

    #[inline]
    pub fn local_id(&self) -> LocalTypeOrConstParamId {
        self.0.local_id
    }

    #[inline]
    pub fn trait_self(trait_: TraitId) -> TypeParamId {
        TypeParamId::from_unchecked(TypeOrConstParamId {
            parent: trait_.into(),
            local_id: GenericParams::SELF_PARAM_ID_IN_SELF,
        })
    }

    #[inline]
    /// Caller should check if this toc id really belongs to a type
    pub fn from_unchecked(it: TypeOrConstParamId) -> Self {
        Self(it)
    }
}

impl From<TypeParamId> for TypeOrConstParamId {
    fn from(it: TypeParamId) -> Self {
        it.0
    }
}

/// A TypeOrConstParamId with an invariant that it actually belongs to a const
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ConstParamId(TypeOrConstParamId);

impl ConstParamId {
    pub fn parent(&self) -> GenericDefId {
        self.0.parent
    }
    pub fn local_id(&self) -> LocalTypeOrConstParamId {
        self.0.local_id
    }
}

impl ConstParamId {
    /// Caller should check if this toc id really belongs to a const
    pub fn from_unchecked(it: TypeOrConstParamId) -> Self {
        Self(it)
    }
}

impl From<ConstParamId> for TypeOrConstParamId {
    fn from(it: ConstParamId) -> Self {
        it.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LifetimeParamId {
    // FIXME: Store this as an erased `blaze_query::db::Id` to save space
    pub parent: GenericDefId,
    pub local_id: LocalLifetimeParamId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HrtbLifetimeParamId {
    pub scope: GenericDefId,
    pub local_id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum ItemContainerId {
    ExternBlockId(ExternBlockId),
    ModuleId(ModuleId),
    ImplId(ImplId),
    TraitId(TraitId),
}
impl_from!(ModuleId for ItemContainerId);

impl ItemContainerId {
    /// Hashes the container's id alone and returns its variant, for a loc to hash beside its
    /// own small fields in one word.
    #[inline]
    fn hash_id<H: Hasher>(self, state: &mut H) -> u8 {
        match self {
            ItemContainerId::ExternBlockId(it) => {
                it.hash(state);
                0
            }
            ItemContainerId::ModuleId(it) => {
                it.hash(state);
                1
            }
            ItemContainerId::ImplId(it) => {
                it.hash(state);
                2
            }
            ItemContainerId::TraitId(it) => {
                it.hash(state);
                3
            }
        }
    }
}

/// A Data Type
#[derive(Debug, PartialOrd, Ord, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum AdtId {
    StructId(StructId),
    UnionId(UnionId),
    EnumId(EnumId),
}
impl_from!(StructId, UnionId, EnumId for AdtId);

impl AdtId {
    /// Hashes the adt's id alone and returns its variant, as [`ItemContainerId::hash_id`].
    #[inline]
    fn hash_id<H: Hasher>(self, state: &mut H) -> u8 {
        match self {
            AdtId::StructId(it) => {
                it.hash(state);
                0
            }
            AdtId::UnionId(it) => {
                it.hash(state);
                1
            }
            AdtId::EnumId(it) => {
                it.hash(state);
                2
            }
        }
    }

    /// The adt's variants, in order: its one of a struct or union, an enum's as declared.
    pub fn variants(self, db: &Db) -> impl Iterator<Item = VariantId> + '_ {
        let (one, all) = match self {
            AdtId::StructId(it) => (Some(VariantId::StructId(it)), None),
            AdtId::UnionId(it) => (Some(VariantId::UnionId(it)), None),
            AdtId::EnumId(it) => (None, Some(it.enum_variants(db).variants.values())),
        };
        one.into_iter().chain(all.into_iter().flatten().map(|&(it, _)| VariantId::EnumVariantId(it)))
    }
}

/// A macro
#[derive(Debug, PartialOrd, Ord, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum MacroId {
    Macro2Id(Macro2Id),
    MacroRulesId(MacroRulesId),
    ProcMacroId(ProcMacroId),
}
impl_from!(Macro2Id, MacroRulesId, ProcMacroId for MacroId);

impl MacroId {
    pub fn is_attribute(self, db: &Db) -> bool {
        matches!(self, MacroId::ProcMacroId(it) if it.lookup(db).kind == ProcMacroKind::Attr)
    }
}

#[blaze_query::tracked]
impl MacroId {
    /// Turns a MacroId into a MacroDefId, describing the macro's definition post name resolution.
    #[blaze_query::tracked(returns(copy), mem_tag = Nameres)]
    pub fn definition(self, db: &Db) -> MacroDefId {
        let kind = |expander, file_id, m| {
            let in_file = InFile::new(file_id, m);
            match expander {
                MacroExpander::Declarative { styles } => MacroDefKind::Declarative(in_file, styles),
                MacroExpander::BuiltIn(it) => MacroDefKind::BuiltIn(in_file, it),
                MacroExpander::BuiltInAttr(it) => MacroDefKind::BuiltInAttr(in_file, it),
                MacroExpander::BuiltInDerive(it) => MacroDefKind::BuiltInDerive(in_file, it),
                MacroExpander::BuiltInEager(it) => MacroDefKind::BuiltInEager(in_file, it),
                MacroExpander::UnimplementedBuiltIn => MacroDefKind::UnimplementedBuiltIn(in_file),
            }
        };

        match self {
            MacroId::Macro2Id(it) => {
                let loc = it.lookup(db);

                MacroDefId {
                    krate: loc.container.krate(db),
                    kind: kind(loc.expander, loc.id.file_id, loc.id.value.upcast()),
                    local_inner: false,
                    allow_internal_unsafe: loc.allow_internal_unsafe,
                    edition_panic: false,
                    edition: loc.edition,
                }
            }
            MacroId::MacroRulesId(it) => {
                let loc = it.lookup(db);

                MacroDefId {
                    krate: loc.container.krate(db),
                    kind: kind(loc.expander, loc.id.file_id, loc.id.value.upcast()),
                    local_inner: loc.flags.contains(MacroRulesLocFlags::LOCAL_INNER),
                    allow_internal_unsafe: loc
                        .flags
                        .contains(MacroRulesLocFlags::ALLOW_INTERNAL_UNSAFE),
                    edition_panic: loc.flags.contains(MacroRulesLocFlags::EDITION_PANIC),
                    edition: loc.edition,
                }
            }
            MacroId::ProcMacroId(it) => {
                let loc = it.lookup(db);

                MacroDefId {
                    krate: loc.container.krate(db),
                    kind: MacroDefKind::ProcMacro(loc.id, loc.expander, loc.kind),
                    local_inner: false,
                    allow_internal_unsafe: false,
                    edition_panic: false,
                    edition: loc.edition,
                }
            }
        }
    }
}

/// A generic param
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GenericParamId {
    TypeParamId(TypeParamId),
    ConstParamId(ConstParamId),
    LifetimeParamId(LifetimeParamId),
}
impl_from!(TypeParamId, LifetimeParamId, ConstParamId for GenericParamId);

/// The defs which can be visible in the module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleDefId {
    ModuleId(ModuleId),
    FunctionId(FunctionId),
    AdtId(AdtId),
    // Can't be directly declared, but can be imported.
    EnumVariantId(EnumVariantId),
    ConstId(ConstId),
    StaticId(StaticId),
    TraitId(TraitId),
    TypeAliasId(TypeAliasId),
    BuiltinType(BuiltinType),
    MacroId(MacroId),
}
impl_from!(
    MacroId(Macro2Id, MacroRulesId, ProcMacroId),
    ModuleId,
    FunctionId,
    AdtId(StructId, EnumId, UnionId),
    EnumVariantId,
    ConstId,
    StaticId,
    TraitId,
    TypeAliasId,
    BuiltinType
    for ModuleDefId
);

impl From<DefWithBodyId> for ModuleDefId {
    #[inline]
    fn from(value: DefWithBodyId) -> Self {
        match value {
            DefWithBodyId::FunctionId(id) => id.into(),
            DefWithBodyId::StaticId(id) => id.into(),
            DefWithBodyId::ConstId(id) => id.into(),
            DefWithBodyId::VariantId(id) => id.into(),
        }
    }
}

/// The defs which have a body.
#[derive(Debug, PartialOrd, Ord, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum DefWithBodyId {
    /// A function body.
    FunctionId(FunctionId),
    /// A static item initializer.
    StaticId(StaticId),
    /// A const item initializer
    ConstId(ConstId),
    /// An enum variant discrimiant
    VariantId(EnumVariantId),
}
impl_from!(FunctionId, ConstId, StaticId for DefWithBodyId);

impl From<EnumVariantId> for DefWithBodyId {
    fn from(id: EnumVariantId) -> Self {
        DefWithBodyId::VariantId(id)
    }
}

impl DefWithBodyId {
    pub fn generic_def(self, db: &Db) -> GenericDefId {
        match self {
            DefWithBodyId::FunctionId(f) => f.into(),
            DefWithBodyId::StaticId(s) => s.into(),
            DefWithBodyId::ConstId(c) => c.into(),
            DefWithBodyId::VariantId(c) => c.lookup(db).parent.into(),
        }
    }

    /// The file and erased id of the node `self` is written as: its function, constant, static
    /// or enum variant.
    pub fn erased_ast_id(self, db: &Db) -> InFile<span::ErasedFileAstId> {
        fn erased<N: AstIdNode>(id: AstId<N>) -> InFile<span::ErasedFileAstId> {
            InFile::new(id.file_id, id.value.erase())
        }
        match self {
            DefWithBodyId::FunctionId(it) => erased(it.lookup(db).id),
            DefWithBodyId::ConstId(it) => erased(it.lookup(db).id),
            DefWithBodyId::StaticId(it) => erased(it.lookup(db).id),
            DefWithBodyId::VariantId(it) => erased(it.lookup(db).id),
        }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum AssocItemId {
    FunctionId(FunctionId),
    ConstId(ConstId),
    TypeAliasId(TypeAliasId),
}

// FIXME: not every function, ... is actually an assoc item. maybe we should make
// sure that you can only turn actual assoc items into AssocItemIds. This would
// require not implementing From, and instead having some checked way of
// casting them, and somehow making the constructors private, which would be annoying.
impl_from!(FunctionId, ConstId, TypeAliasId for AssocItemId);

impl_from!(
    AssocItemId {
        FunctionId => FunctionId,
        ConstId => ConstId,
        TypeAliasId => TypeAliasId,
    }
    for ModuleDefId
);

#[derive(Debug, PartialOrd, Ord, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum GenericDefId {
    AdtId(AdtId),
    // consts can have type parameters from their parents (i.e. associated consts of traits)
    ConstId(ConstId),
    FunctionId(FunctionId),
    ImplId(ImplId),
    // can't actually have generics currently, but they might in the future
    // More importantly, this completes the set of items that contain type references
    // which is to be used by the signature expression store in the future.
    StaticId(StaticId),
    TraitId(TraitId),
    TypeAliasId(TypeAliasId),
}
impl_from!(
    AdtId(StructId, EnumId, UnionId),
    ConstId,
    FunctionId,
    ImplId,
    StaticId,
    TraitId,
    TypeAliasId
    for GenericDefId
);

/// Owner of an expression store - either a body or a signature.
/// This is used for queries that operate on expression stores generically,
/// such as `expr_scopes`.
// NOTE: This type cannot be `blaze_query::Supertype` as its variants are overlapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord /* !blaze_query::Supertype */)]
pub enum ExpressionStoreOwnerId {
    Signature(GenericDefId),
    /// A body, something with a root expression.
    ///
    /// An enum variant's body is considered its discriminant initializer.
    Body(DefWithBodyId),
    VariantFields(VariantId),
}

/// The item whose store it is, for a query keyed by it (`blaze_query::db::Storage::ids_of`, read by
/// `crates/hir-firewall`).
impl blaze_query::db::NamesIds for ExpressionStoreOwnerId {
    fn names_ids(&self, out: &mut Vec<blaze_query::db::Id>) {
        use blaze_query::db::AsId;
        out.push(match *self {
            ExpressionStoreOwnerId::Signature(it) => it.as_id(),
            ExpressionStoreOwnerId::Body(it) => it.as_id(),
            ExpressionStoreOwnerId::VariantFields(it) => it.as_id(),
        });
    }
}

impl ExpressionStoreOwnerId {
    // FIXME: Check callers of this, this method likely can be removed
    pub fn as_def_with_body(self) -> Option<DefWithBodyId> {
        if let Self::Body(v) = self { Some(v) } else { None }
    }

    pub fn generic_def(self, db: &Db) -> GenericDefId {
        match self {
            ExpressionStoreOwnerId::Signature(generic_def_id) => generic_def_id,
            ExpressionStoreOwnerId::Body(def_with_body_id) => def_with_body_id.generic_def(db),
            ExpressionStoreOwnerId::VariantFields(variant_id) => match variant_id {
                VariantId::EnumVariantId(it) => it.lookup(db).parent.into(),
                VariantId::StructId(it) => it.into(),
                VariantId::UnionId(it) => it.into(),
            },
        }
    }
}

impl From<GenericDefId> for ExpressionStoreOwnerId {
    fn from(id: GenericDefId) -> Self {
        ExpressionStoreOwnerId::Signature(id)
    }
}

impl From<DefWithBodyId> for ExpressionStoreOwnerId {
    fn from(id: DefWithBodyId) -> Self {
        ExpressionStoreOwnerId::Body(id)
    }
}

impl From<VariantId> for ExpressionStoreOwnerId {
    fn from(id: VariantId) -> Self {
        ExpressionStoreOwnerId::VariantFields(id)
    }
}

impl From<ImplId> for ExpressionStoreOwnerId {
    fn from(id: ImplId) -> Self {
        ExpressionStoreOwnerId::Signature(id.into())
    }
}

impl GenericDefId {
    /// The file of the owner and its generic parameter list, as a view of the database's tree.
    pub fn file_id_and_params_of_view(
        self,
        db: &Db,
    ) -> (HirFileId, Option<syntax::view::GenericParamList<'_>>) {
        fn of_item_loc<'db, Loc>(
            db: &'db Db,
            def: impl Lookup<Data = Loc>,
        ) -> (HirFileId, Option<syntax::view::GenericParamList<'db>>)
        where
            Loc: src::HasSource,
            Loc::Value: syntax::view::Typed,
            for<'t> <Loc::Value as syntax::view::Typed>::Of<'t>: syntax::view::HasGenericParams<'t>,
        {
            let src = def.lookup(db).source_view(db);
            (src.file_id, syntax::view::HasGenericParams::generic_param_list(&src.value))
        }

        match self {
            GenericDefId::FunctionId(it) => of_item_loc(db, it),
            GenericDefId::TypeAliasId(it) => of_item_loc(db, it),
            GenericDefId::AdtId(AdtId::StructId(it)) => of_item_loc(db, it),
            GenericDefId::AdtId(AdtId::UnionId(it)) => of_item_loc(db, it),
            GenericDefId::AdtId(AdtId::EnumId(it)) => of_item_loc(db, it),
            GenericDefId::TraitId(it) => of_item_loc(db, it),
            GenericDefId::ImplId(it) => of_item_loc(db, it),
            GenericDefId::ConstId(it) => (it.lookup(db).id.file_id, None),
            GenericDefId::StaticId(it) => (it.lookup(db).id.file_id, None),
        }
    }

    pub fn assoc_trait_container(self, db: &Db) -> Option<TraitId> {
        match match self {
            GenericDefId::FunctionId(f) => f.lookup(db).container,
            GenericDefId::TypeAliasId(t) => t.lookup(db).container,
            GenericDefId::ConstId(c) => c.lookup(db).container,
            _ => return None,
        } {
            ItemContainerId::TraitId(trait_) => Some(trait_),
            _ => None,
        }
    }

    pub fn from_callable(db: &Db, def: CallableDefId) -> GenericDefId {
        match def {
            CallableDefId::FunctionId(f) => f.into(),
            CallableDefId::StructId(s) => s.into(),
            CallableDefId::EnumVariantId(e) => e.lookup(db).parent.into(),
        }
    }
}

impl_from!(
    AssocItemId {
        FunctionId => FunctionId,
        ConstId => ConstId,
        TypeAliasId => TypeAliasId,
    }
    for GenericDefId
);

#[derive(Debug, PartialOrd, Ord, Clone, Copy, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum CallableDefId {
    FunctionId(FunctionId),
    StructId(StructId),
    EnumVariantId(EnumVariantId),
}

impl_from!(FunctionId, StructId, EnumVariantId for CallableDefId);
impl_from!(
    CallableDefId {
        FunctionId => FunctionId,
        StructId => AdtId,
        EnumVariantId => EnumVariantId,
    }
    for ModuleDefId
);

impl CallableDefId {
    pub fn krate(self, db: &Db) -> Crate {
        match self {
            CallableDefId::FunctionId(f) => f.krate(db),
            CallableDefId::StructId(s) => s.krate(db),
            CallableDefId::EnumVariantId(e) => e.krate(db),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, blaze_query::Supertype)]
pub enum AttrDefId {
    ModuleId(ModuleId),
    AdtId(AdtId),
    FunctionId(FunctionId),
    EnumVariantId(EnumVariantId),
    StaticId(StaticId),
    ConstId(ConstId),
    TraitId(TraitId),
    TypeAliasId(TypeAliasId),
    MacroId(MacroId),
    ImplId(ImplId),
    ExternBlockId(ExternBlockId),
    ExternCrateId(ExternCrateId),
    UseId(UseId),
}

impl_from!(
    ModuleId,
    AdtId(StructId, EnumId, UnionId),
    EnumVariantId,
    StaticId,
    ConstId,
    FunctionId,
    TraitId,
    TypeAliasId,
    MacroId(Macro2Id, MacroRulesId, ProcMacroId),
    ImplId,
    ExternCrateId,
    UseId
    for AttrDefId
);

impl_from!(AssocItemId { FunctionId, ConstId, TypeAliasId } for AttrDefId);
impl_from!(
    VariantId { EnumVariantId => EnumVariantId, StructId => AdtId, UnionId => AdtId }
    for AttrDefId
);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, blaze_query::Supertype, blaze_query::Value,
)]
pub enum VariantId {
    EnumVariantId(EnumVariantId),
    StructId(StructId),
    UnionId(UnionId),
}
impl_from!(EnumVariantId, StructId, UnionId for VariantId);

impl VariantId {
    pub fn from_non_enum(adt_id: AdtId) -> Option<Self> {
        Some(match adt_id {
            AdtId::StructId(struct_id) => struct_id.into(),
            AdtId::UnionId(union_id) => union_id.into(),
            AdtId::EnumId(_) => return None,
        })
    }

    pub fn fields(self, db: &Db) -> &VariantFields {
        VariantFields::of(db, self)
    }

    pub fn fields_with_source_map(
        self,
        db: &Db,
    ) -> (&VariantFields, &ExpressionStoreSourceMap) {
        let r = VariantFields::with_source_map(db, self);
        (&r.0, &r.1)
    }

    pub fn file_id(self, db: &Db) -> HirFileId {
        match self {
            VariantId::EnumVariantId(it) => it.lookup(db).id.file_id,
            VariantId::StructId(it) => it.lookup(db).id.file_id,
            VariantId::UnionId(it) => it.lookup(db).id.file_id,
        }
    }

    pub fn adt_id(self, db: &Db) -> AdtId {
        match self {
            VariantId::EnumVariantId(it) => it.lookup(db).parent.into(),
            VariantId::StructId(it) => it.into(),
            VariantId::UnionId(it) => it.into(),
        }
    }
}

pub trait HasModule {
    /// Returns the enclosing module this thing is defined within.
    fn module(&self, db: &Db) -> ModuleId;
    /// Returns the crate this thing is defined within.
    #[inline]
    #[doc(alias = "crate")]
    fn krate(&self, db: &Db) -> Crate {
        self.module(db).krate(db)
    }
}

// In theory this impl should work out for us, but rustc thinks it collides with all the other
// manual impls that do not have a ModuleId container...
// impl<N, ItemId, Data> HasModule for ItemId
// where
//     N: ItemTreeNode,
//     ItemId: for<'db> Lookup<Database<'db> = Db, Data = Data> + Copy,
//     Data: ItemTreeLoc<Id = N, Container = ModuleId>,
// {
//     #[inline]
//     fn module(&self, db: &Db) -> ModuleId {
//         self.lookup(db).container()
//     }
// }

impl<N, ItemId> HasModule for ItemId
where
    N: AstIdNode,
    ItemId: Lookup<Data = ItemLoc<N>> + Copy,
{
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.lookup(db).container
    }
}

// Technically this does not overlap with the above, but rustc currently forbids this, hence why we
// need to write the 3 impls manually instead
// impl<N, ItemId> HasModule for ItemId
// where
//     N: ItemTreeModItemNode,
//     ItemId: for<'db> Lookup<Database<'db> = Db, Data = AssocItemLoc<N>> + Copy,
// {
//     #[inline]
//     fn module(&self, db: &Db) -> ModuleId {
//         self.lookup(db).container.module(db)
//     }
// }

// region: manual-assoc-has-module-impls
#[inline]
fn module_for_assoc_item_loc(
    db: &Db,
    id: impl Lookup<Data = AssocItemLoc<impl AstIdNode>>,
) -> ModuleId {
    id.lookup(db).container.module(db)
}

impl HasModule for BuiltinDeriveImplLoc {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.adt.module(db)
    }
}

impl HasModule for BuiltinDeriveImplId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.loc(db).module(db)
    }
}

impl HasModule for FunctionId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        module_for_assoc_item_loc(db, *self)
    }
}

impl HasModule for ConstId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        module_for_assoc_item_loc(db, *self)
    }
}

impl HasModule for StaticId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        module_for_assoc_item_loc(db, *self)
    }
}

impl HasModule for TypeAliasId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        module_for_assoc_item_loc(db, *self)
    }
}
// endregion: manual-assoc-has-module-impls

impl HasModule for EnumVariantId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.lookup(db).parent.module(db)
    }
}

impl HasModule for MacroRulesId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.lookup(db).container
    }
}

impl HasModule for Macro2Id {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.lookup(db).container
    }
}

impl HasModule for ProcMacroId {
    #[inline]
    fn module(&self, db: &Db) -> ModuleId {
        self.lookup(db).container
    }
}

impl HasModule for ItemContainerId {
    fn module(&self, db: &Db) -> ModuleId {
        match *self {
            ItemContainerId::ModuleId(it) => it,
            ItemContainerId::ImplId(it) => it.module(db),
            ItemContainerId::TraitId(it) => it.module(db),
            ItemContainerId::ExternBlockId(it) => it.module(db),
        }
    }
}

impl HasModule for AdtId {
    fn module(&self, db: &Db) -> ModuleId {
        match *self {
            AdtId::StructId(it) => it.module(db),
            AdtId::UnionId(it) => it.module(db),
            AdtId::EnumId(it) => it.module(db),
        }
    }
}

impl HasModule for VariantId {
    fn module(&self, db: &Db) -> ModuleId {
        match *self {
            VariantId::EnumVariantId(it) => it.module(db),
            VariantId::StructId(it) => it.module(db),
            VariantId::UnionId(it) => it.module(db),
        }
    }
}

impl HasModule for MacroId {
    fn module(&self, db: &Db) -> ModuleId {
        match *self {
            MacroId::MacroRulesId(it) => it.module(db),
            MacroId::Macro2Id(it) => it.module(db),
            MacroId::ProcMacroId(it) => it.module(db),
        }
    }
}

impl HasModule for DefWithBodyId {
    fn module(&self, db: &Db) -> ModuleId {
        match self {
            DefWithBodyId::FunctionId(it) => it.module(db),
            DefWithBodyId::StaticId(it) => it.module(db),
            DefWithBodyId::ConstId(it) => it.module(db),
            DefWithBodyId::VariantId(it) => it.module(db),
        }
    }
}

impl HasModule for ExpressionStoreOwnerId {
    fn module(&self, db: &Db) -> ModuleId {
        match self {
            ExpressionStoreOwnerId::Signature(def) => def.module(db),
            ExpressionStoreOwnerId::Body(def) => def.module(db),
            ExpressionStoreOwnerId::VariantFields(variant_id) => variant_id.module(db),
        }
    }
}

impl HasModule for GenericDefId {
    fn module(&self, db: &Db) -> ModuleId {
        match self {
            GenericDefId::FunctionId(it) => it.module(db),
            GenericDefId::AdtId(it) => it.module(db),
            GenericDefId::TraitId(it) => it.module(db),
            GenericDefId::TypeAliasId(it) => it.module(db),
            GenericDefId::ImplId(it) => it.module(db),
            GenericDefId::ConstId(it) => it.module(db),
            GenericDefId::StaticId(it) => it.module(db),
        }
    }
}

impl HasModule for AttrDefId {
    fn module(&self, db: &Db) -> ModuleId {
        match self {
            AttrDefId::ModuleId(it) => *it,
            AttrDefId::AdtId(it) => it.module(db),
            AttrDefId::FunctionId(it) => it.module(db),
            AttrDefId::EnumVariantId(it) => it.module(db),
            AttrDefId::StaticId(it) => it.module(db),
            AttrDefId::ConstId(it) => it.module(db),
            AttrDefId::TraitId(it) => it.module(db),
            AttrDefId::TypeAliasId(it) => it.module(db),
            AttrDefId::ImplId(it) => it.module(db),
            AttrDefId::ExternBlockId(it) => it.module(db),
            AttrDefId::MacroId(it) => it.module(db),
            AttrDefId::ExternCrateId(it) => it.module(db),
            AttrDefId::UseId(it) => it.module(db),
        }
    }
}

impl ModuleDefId {
    /// Returns the module containing `self` (or `self`, if `self` is itself a module).
    ///
    /// Returns `None` if `self` refers to a primitive type.
    pub fn module(&self, db: &Db) -> Option<ModuleId> {
        Some(match self {
            ModuleDefId::ModuleId(id) => *id,
            ModuleDefId::FunctionId(id) => id.module(db),
            ModuleDefId::AdtId(id) => id.module(db),
            ModuleDefId::EnumVariantId(id) => id.module(db),
            ModuleDefId::ConstId(id) => id.module(db),
            ModuleDefId::StaticId(id) => id.module(db),
            ModuleDefId::TraitId(id) => id.module(db),
            ModuleDefId::TypeAliasId(id) => id.module(db),
            ModuleDefId::MacroId(id) => id.module(db),
            ModuleDefId::BuiltinType(_) => return None,
        })
    }
}
/// Helper wrapper for `AstId` with `ModPath`
#[derive(Clone, Debug, Eq, PartialEq)]
struct AstIdWithPath<T: AstIdNode> {
    ast_id: AstId<T>,
    path: ModPath,
}

impl<T: AstIdNode> AstIdWithPath<T> {
    fn new(file_id: HirFileId, ast_id: FileAstId<T>, path: ModPath) -> AstIdWithPath<T> {
        AstIdWithPath { ast_id: AstId::new(file_id, ast_id), path }
    }
}

pub fn macro_call_as_call_id(
    db: &Db,
    ast_id: AstId<ast::MacroCall>,
    path: &ModPath,
    call_site: SyntaxContext,
    expand_to: ExpandTo,
    krate: Crate,
    macro_depth: u32,
    recursion_limit: u32,
    resolver: impl Fn(&ModPath) -> Option<MacroDefId> + Copy,
    eager_calls: &mut Vec<EagerCall, impl Allocator>,
) -> Result<ExpandResult<Option<MacroCallId>>, UnresolvedMacro> {
    let def = resolver(path).ok_or(UnresolvedMacro { path: *path })?;

    let res = match def.kind {
        MacroDefKind::BuiltInEager(..) => expand_eager_macro_input_view(
            db,
            krate,
            ast_id.to_view(db),
            ast_id,
            def,
            call_site,
            macro_depth,
            recursion_limit,
            &|path| resolver(path).filter(MacroDefId::is_fn_like),
            eager_calls,
        ),
        _ if def.is_fn_like() => ExpandResult {
            value: Some(def.make_call(
                db,
                krate,
                MacroCallKind::FnLike { ast_id, expand_to, eager: None },
                call_site,
                macro_depth,
            )),
            err: None,
        },
        _ => return Err(UnresolvedMacro { path: *path }),
    };
    Ok(res)
}

#[derive(Debug)]
pub struct UnresolvedMacro {
    pub path: ModPath,
}

#[derive(Default, Debug, Eq, PartialEq, Clone, Copy)]
pub struct SyntheticSyntax;

// Feature: Completions Attribute
// Crate authors can opt their type out of completions in some cases.
// This is done with the `#[rust_analyzer::completions(...)]` attribute.
//
// All completable things support `#[rust_analyzer::completions(ignore_flyimport)]`,
// which causes the thing to get excluded from flyimport completion. It will still
// be completed when in scope. This is analogous to the setting `rust-analyzer.completion.autoimport.exclude`
// with `"type": "always"`.
//
// In addition, traits support two more modes: `#[rust_analyzer::completions(ignore_flyimport_methods)]`,
// which means the trait itself may still be flyimported but its methods won't, and
// `#[rust_analyzer::completions(ignore_methods)]`, which means the methods won't be completed even when
// the trait is in scope (but the trait itself may still be completed). The methods will still be completed
// on `dyn Trait`, `impl Trait` or where the trait is specified in bounds. These modes correspond to
// the settings `rust-analyzer.completion.autoimport.exclude` with `"type": "methods"` and
// `rust-analyzer.completion.excludeTraits`, respectively.
//
// Malformed attributes will be ignored without warnings.
//
// Note that users have no way to override this attribute, so be careful and only include things
// users definitely do not want to be completed!

/// `#[rust_analyzer::completions(...)]` options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Complete {
    /// No `#[rust_analyzer::completions(...)]`.
    Yes,
    /// `#[rust_analyzer::completions(ignore_flyimport)]`.
    IgnoreFlyimport,
    /// `#[rust_analyzer::completions(ignore_flyimport_methods)]` (on a trait only).
    IgnoreFlyimportMethods,
    /// `#[rust_analyzer::completions(ignore_methods)]` (on a trait only).
    IgnoreMethods,
}

impl Complete {
    #[inline]
    pub fn extract(is_trait: bool, attrs: AttrFlags) -> Complete {
        if attrs.contains(AttrFlags::COMPLETE_IGNORE_FLYIMPORT) {
            return Complete::IgnoreFlyimport;
        } else if is_trait {
            if attrs.contains(AttrFlags::COMPLETE_IGNORE_METHODS) {
                return Complete::IgnoreMethods;
            } else if attrs.contains(AttrFlags::COMPLETE_IGNORE_FLYIMPORT_METHODS) {
                return Complete::IgnoreFlyimportMethods;
            }
        }
        Complete::Yes
    }

    #[inline]
    pub fn for_trait_item(trait_attr: Complete, item_attr: Complete) -> Complete {
        match (trait_attr, item_attr) {
            (
                Complete::IgnoreFlyimportMethods
                | Complete::IgnoreFlyimport
                | Complete::IgnoreMethods,
                _,
            ) => Complete::IgnoreFlyimport,
            _ => item_attr,
        }
    }
}

// return: macro call id and include file id
#[blaze_query::tracked(returns(ref), mem_tag = Nameres)]
pub fn include_macro_invoc(
    db: &Db,
    krate: Crate,
) -> Box<[(MacroCallId, EditionedFileId)]> {
    blaze_mem::frame::current(|frame| {
        let mut found = Vec::new_in(frame);
        found.extend(
            crate_def_map(db, krate)
                .modules(db)
                .flat_map(|(_, m)| m.scope(db).iter_macro_invoc())
                .filter_map(|(_, &invoc)| {
                    invoc.loc(db).include_file_id(db, invoc).map(|x| (invoc, x))
                }),
        );
        // alloc: the memo's calls, exact, once per crate
        Box::from(&found[..])
    })
}

#[cfg(test)]
mod loc_hash_tests {
    use std::{
        fmt::Debug,
        hash::{BuildHasher, Hash},
    };

    use blaze_hash::{FixedState, HashSet};
    use blaze_query::db::FromId;
    use hir_expand::{
        EditionedFileId, HirFileId, InFile, MacroCallId, MacroCallStyles,
        attrs::AttrId,
        builtin::{
            BuiltinAttrExpander, BuiltinDeriveExpander, BuiltinFnLikeExpander, EagerExpander,
        },
        name::Name,
        proc_macro::{CustomProcMacroExpander, ProcMacroKind},
    };
    use intern::Symbol;
    use span::{Edition, ErasedFileAstId, FileAstId};
    use syntax::ast;

    use super::*;

    fn hash<T: Hash>(it: &T) -> u64 {
        FixedState.hash_one(it)
    }

    /// An id of an interned struct whose data no test reads.
    fn query_id<T: FromId>(index: u32) -> T {
        T::from_id(blaze_query::db::Id::from_u32(index))
    }

    fn ast_ids<N: AstIdNode>() -> [AstId<N>; 4] {
        let raw = |it| FileAstId::from_erased_unchecked(ErasedFileAstId::from_raw(it));
        [
            InFile::new(HirFileId::FileId(query_id::<EditionedFileId>(3)), raw(7)),
            InFile::new(HirFileId::MacroFile(query_id::<MacroCallId>(3)), raw(7)),
            InFile::new(HirFileId::FileId(query_id::<EditionedFileId>(4)), raw(7)),
            InFile::new(HirFileId::FileId(query_id::<EditionedFileId>(3)), raw(8)),
        ]
    }

    fn containers() -> [ItemContainerId; 5] {
        [
            ItemContainerId::ModuleId(query_id(5)),
            ItemContainerId::ImplId(query_id(5)),
            ItemContainerId::TraitId(query_id(5)),
            ItemContainerId::ExternBlockId(query_id(5)),
            ItemContainerId::ModuleId(query_id(6)),
        ]
    }

    fn adts() -> [AdtId; 4] {
        [
            AdtId::StructId(query_id(5)),
            AdtId::UnionId(query_id(5)),
            AdtId::EnumId(query_id(5)),
            AdtId::StructId(query_id(6)),
        ]
    }

    fn expanders() -> Vec<MacroExpander> {
        let mut all: Vec<_> = (0..8)
            .map(|bits| MacroExpander::Declarative { styles: MacroCallStyles::from_bits_retain(bits) })
            .collect();
        all.extend(BuiltinFnLikeExpander::ALL.iter().map(|&it| MacroExpander::BuiltIn(it)));
        all.extend(BuiltinAttrExpander::ALL.iter().map(|&it| MacroExpander::BuiltInAttr(it)));
        all.extend(BuiltinDeriveExpander::ALL.iter().map(|&it| MacroExpander::BuiltInDerive(it)));
        all.extend(EagerExpander::ALL.iter().map(|&it| MacroExpander::BuiltInEager(it)));
        all.push(MacroExpander::UnimplementedBuiltIn);
        all
    }

    /// The values differ from each other in one or a few fields: a packed hash must still tell
    /// each unequal pair apart, as the derived one did, and agree with `Eq`.
    #[track_caller]
    fn check<T: Hash + Eq + Debug>(values: &[T]) {
        for a in values {
            for b in values {
                assert_eq!(a == b, hash(a) == hash(b), "{a:?} and {b:?}");
            }
        }
    }

    #[test]
    fn item_locs() {
        let mut locs = Vec::new();
        for id in ast_ids::<ast::Struct>() {
            locs.push(ItemLoc { container: query_id(5), id });
        }
        locs.push(ItemLoc { container: query_id(6), id: ast_ids()[0] });
        check(&locs);
    }

    #[test]
    fn assoc_item_locs() {
        let mut locs = Vec::new();
        for container in containers() {
            for id in ast_ids::<ast::Fn>() {
                locs.push(AssocItemLoc { container, id });
            }
        }
        check(&locs);
    }

    #[test]
    fn enum_variant_locs() {
        let mut locs = Vec::new();
        for id in ast_ids() {
            for parent in [query_id::<EnumId>(5), query_id(6)] {
                for name in ["A", "B"] {
                    let name = Name::new_symbol_root(Symbol::intern(name));
                    locs.push(EnumVariantLoc { id, parent, name });
                }
            }
        }
        check(&locs);
    }

    #[test]
    fn expander_words_are_distinct() {
        let all = expanders();
        let words: HashSet<u16> = all.iter().map(|it| it.hash_word()).collect();
        assert_eq!(words.len(), all.len());
    }

    #[test]
    fn macro_locs() {
        let base = Macro2Loc {
            container: query_id(5),
            id: ast_ids()[0],
            expander: MacroExpander::UnimplementedBuiltIn,
            allow_internal_unsafe: false,
            edition: Edition::Edition2015,
        };
        let mut locs = vec![base];
        locs.push(Macro2Loc { container: query_id(6), ..base });
        locs.extend(ast_ids().into_iter().skip(1).map(|id| Macro2Loc { id, ..base }));
        locs.extend(expanders().into_iter().skip(1).map(|expander| Macro2Loc { expander, ..base }));
        locs.push(Macro2Loc { allow_internal_unsafe: true, ..base });
        locs.push(Macro2Loc { edition: Edition::Edition2021, ..base });
        locs.push(Macro2Loc { allow_internal_unsafe: true, edition: Edition::Edition2021, ..base });
        check(&locs);

        let base = MacroRulesLoc {
            container: query_id(5),
            id: ast_ids()[0],
            expander: MacroExpander::Declarative { styles: MacroCallStyles::FN_LIKE },
            flags: MacroRulesLocFlags::empty(),
            edition: Edition::Edition2015,
        };
        let mut locs = vec![base];
        locs.push(MacroRulesLoc { container: query_id(6), ..base });
        locs.extend(ast_ids().into_iter().skip(1).map(|id| MacroRulesLoc { id, ..base }));
        locs.extend(expanders().into_iter().map(|expander| MacroRulesLoc { expander, ..base }));
        locs.extend(
            (1..8).map(|bits| MacroRulesLoc {
                flags: MacroRulesLocFlags::from_bits_retain(bits),
                ..base
            }),
        );
        locs.push(MacroRulesLoc { edition: Edition::Edition2021, ..base });
        check(&locs);

        let base = ProcMacroLoc {
            container: query_id(5),
            id: ast_ids()[0],
            expander: CustomProcMacroExpander::new(0),
            kind: ProcMacroKind::Bang,
            edition: Edition::Edition2015,
        };
        let mut locs = vec![base];
        locs.push(ProcMacroLoc { container: query_id(6), ..base });
        locs.extend(ast_ids().into_iter().skip(1).map(|id| ProcMacroLoc { id, ..base }));
        locs.push(ProcMacroLoc { expander: CustomProcMacroExpander::new(1), ..base });
        locs.push(ProcMacroLoc { expander: CustomProcMacroExpander::disabled(), ..base });
        locs.push(ProcMacroLoc { kind: ProcMacroKind::Attr, ..base });
        locs.push(ProcMacroLoc { kind: ProcMacroKind::CustomDerive, ..base });
        locs.push(ProcMacroLoc { edition: Edition::Edition2021, ..base });
        check(&locs);
    }

    #[test]
    fn builtin_derive_impl_locs() {
        let mut locs = Vec::new();
        for adt in adts() {
            for &trait_ in BuiltinDeriveImplTrait::ALL {
                for (attr, index) in [(0, 0), (1, 0), (0, 1)] {
                    locs.push(BuiltinDeriveImplLoc {
                        adt,
                        trait_,
                        derive_attr_id: AttrId::from_item_tree_index(attr),
                        derive_index: index,
                    });
                }
            }
        }
        check(&locs);
    }
}

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
