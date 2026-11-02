//! Items by content (docs/v2/plan/lanes/items.md section 6), from blaze-next's `blaze-items` (#344): a
//! front-end crate, next to `hir-ty`.
//!
//! An item (fn, struct, enum, union, trait, impl, const, static, type alias, macro) has two
//! kinds of name:
//!
//! - **which** item it is: its [`ItemKey`], like rustc's `DefPathHash` (its crate, its
//!   parent's key, its kind, its name and a disambiguator). Queries are asked of it. It is
//!   never part of a code hash: a rename gives a new key, and every query of the new key
//!   produces the old fingerprints, so nothing is compiled.
//! - **what** it is: its four hashes ([`ItemHashes`]), of its normalized form after expansion,
//!   resolution and inference, with no names, spans or order, each reference the hash of what
//!   it refers to, a cycle hashed as one unit with members by position (Unison's rule). Names
//!   and positions are in a hash only where the program can observe them (`type_name`,
//!   `module_path!`, symbol names, `line!`, `#[track_caller]`): a query that reads one records
//!   the fact [`Fact::Name`] or reads a [`blaze_format::SpanRef`] filled at link time.
//!
//! A query depends on [`Fact`]s, the narrowest thing it read, each a query of its own whose
//! fingerprint is what it stands for. Negative facts ("no impl of Trait for T", "N is not in
//! S") are answered from indexes keyed by [`ImplIndexKey`] and [`ScopeIndexKey`], so an added
//! impl or item touches only the queries that asked a question it now answers differently.

//!
//! In this crate: [`key`] gives each item its [`ItemKey`] and resolves one back; `form` writes
//! an item's normalized form; [`hash`] fills the forms' slots and gives the four hashes
//! ([`hashes`]); [`facts`] gives each [`Fact`] its value; `index` holds the negative-fact
//! indexes; [`table`] holds hashes and facts as columns ([`HashColumns`], [`FactSet`]);
//! [`goals`] gives the persistent goal cache its keys and facts. Each is computed by `#[blaze_query::tracked]` queries over the front end's database, so
//! an edit recomputes only the forms it touched and stops where a hash comes out equal.
//!
//! Memory (docs/v2/plan/design/memory.md): a crate's tables (its key index, its hash columns)
//! sit in reserved regions of `blaze-mem`, which grow in place and never move. A query's
//! temporaries come from its task's frame, but only in a query whose own nesting is bounded (a
//! crate's tables, an index, a leaf that calls no query): a hash fills each ref by the hash of
//! the item it names, so hashing nests as deep as the graph of refs, past the frames a worker
//! keeps warm. Preimages are written into the worker's scratch buffers.

#![feature(allocator_api, allocator_ext)]
#![forbid(unsafe_code)]

mod bound;
mod facts;
mod form;
pub mod goals;
pub mod hash;
mod index;
pub mod key;
pub mod table;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_query::{QueryKey, QueryKind};
use hir_def::{
    AdtId, AssocItemId, ConstId, DefWithBodyId, FunctionId, GenericDefId, ImplId, MacroId,
    StaticId, TraitId, TypeAliasId,
};
use serde::{Deserialize, Serialize};

pub use bound::{BOUND_DOMAIN, bound_hash};
pub use facts::*;
pub use form::observes_names;
pub use hash::{
    Unhashable, closure_code, code_hash, coroutine_code, deep_hash, hashes, interface_hash,
    layout_hash, signature_hash,
};
pub use index::*;
pub use key::{Resolved, item_key, resolve};
pub use table::{FactSet, HashColumns, crate_hashes};

/// Which item, in one database: its dense ids, never hashed, never kept past the database.
/// [`ItemKey`] is what is kept.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, blaze_query::Supertype)]
pub enum ItemId {
    Function(FunctionId),
    Adt(AdtId),
    Trait(TraitId),
    Impl(ImplId),
    Const(ConstId),
    Static(StaticId),
    Alias(TypeAliasId),
    Macro(MacroId),
}

impl ItemId {
    pub fn kind(self) -> ItemKind {
        match self {
            ItemId::Function(_) => ItemKind::Fn,
            ItemId::Adt(AdtId::StructId(_)) => ItemKind::Struct,
            ItemId::Adt(AdtId::EnumId(_)) => ItemKind::Enum,
            ItemId::Adt(AdtId::UnionId(_)) => ItemKind::Union,
            ItemId::Trait(_) => ItemKind::Trait,
            ItemId::Impl(_) => ItemKind::Impl,
            ItemId::Const(_) => ItemKind::Const,
            ItemId::Static(_) => ItemKind::Static,
            ItemId::Alias(_) => ItemKind::TypeAlias,
            ItemId::Macro(_) => ItemKind::Macro,
        }
    }

    pub fn krate(self, db: &Db) -> base_db::Crate {
        use hir_def::HasModule;
        match self {
            ItemId::Function(it) => it.krate(db),
            ItemId::Adt(it) => it.krate(db),
            ItemId::Trait(it) => it.krate(db),
            ItemId::Impl(it) => it.krate(db),
            ItemId::Const(it) => it.krate(db),
            ItemId::Static(it) => it.krate(db),
            ItemId::Alias(it) => it.krate(db),
            ItemId::Macro(it) => it.krate(db),
        }
    }

    /// The body it has, if it is of a kind that has one.
    pub fn body(self) -> Option<DefWithBodyId> {
        match self {
            ItemId::Function(it) => Some(it.into()),
            ItemId::Const(it) => Some(it.into()),
            ItemId::Static(it) => Some(it.into()),
            _ => None,
        }
    }

    pub fn generic_def(self) -> Option<GenericDefId> {
        match self {
            ItemId::Function(it) => Some(it.into()),
            ItemId::Adt(it) => Some(it.into()),
            ItemId::Trait(it) => Some(it.into()),
            ItemId::Impl(it) => Some(it.into()),
            ItemId::Const(it) => Some(it.into()),
            ItemId::Static(it) => Some(it.into()),
            ItemId::Alias(it) => Some(it.into()),
            ItemId::Macro(_) => None,
        }
    }
}

impl From<AssocItemId> for ItemId {
    fn from(item: AssocItemId) -> ItemId {
        match item {
            AssocItemId::FunctionId(it) => ItemId::Function(it),
            AssocItemId::ConstId(it) => ItemId::Const(it),
            AssocItemId::TypeAliasId(it) => ItemId::Alias(it),
        }
    }
}

impl ItemId {
    /// The item a body is of: an enum variant's discriminant is its enum's.
    pub fn of_body(db: &Db, body: DefWithBodyId) -> ItemId {
        use hir_def::Lookup;
        match body {
            DefWithBodyId::FunctionId(it) => ItemId::Function(it),
            DefWithBodyId::ConstId(it) => ItemId::Const(it),
            DefWithBodyId::StaticId(it) => ItemId::Static(it),
            DefWithBodyId::VariantId(it) => ItemId::Adt(it.lookup(db).parent.into()),
        }
    }
}

/// Which item: stable across edits of what it is, changed by a rename or a move.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ItemKey(pub ContentId);

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum ItemKind {
    Fn,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Const,
    Static,
    TypeAlias,
    Macro,
}

/// What a user of the item can see: a fn's signature and bounds; a type's fields, variants
/// and generics; a trait's items. It includes what a body leaks: an `impl Trait` return's and
/// an `async fn`'s hidden type and its auto traits, a `const fn`'s body, a closure's inferred
/// signature and captures.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct InterfaceHash(pub ContentId);

/// The interface without what a body leaks into it: in place of each leak (a `const fn`'s, a
/// constant's, an immutable static's and an explicit discriminant's body, an opaque type's
/// hidden type and its auto traits, a closure's captures and a coroutine's held locals) a
/// marker, each ref by its signature hash. Read by what a body's inference is keyed by, which
/// may not depend on any body's inference: what a body reads of a leak is a fact of its own.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SignatureHash(pub ContentId);

/// A type's size, alignment, field offsets, niches and ABI, deep: its fields' layouts, `repr`
/// and the target, transitively.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LayoutHash(pub ContentId);

/// The normalized MIR body, each callee by its interface hash: a callee's body edit leaves
/// it as it was.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CodeHash(pub ContentId);

/// The code with every callee's deep hash, for what inlines (release).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct DeepHash(pub ContentId);

/// An item's four hashes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ItemHashes {
    pub kind: ItemKind,
    pub interface: InterfaceHash,
    /// Of a type.
    pub layout: Option<LayoutHash>,
    /// Of an item with a body: a fn, a const, a static.
    pub code: Option<CodeHash>,
    pub deep: DeepHash,
}

/// rustc's simplified self type (`SimplifiedType`), hashed: the key under which impls and
/// method candidates are found.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct SimplifiedType(pub ContentId);

/// A name, hashed: `Hasher::new(NAME_DOMAIN).update(name)`.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct NameHash(pub ContentId);

pub const NAME_DOMAIN: &str = "blaze v2 name 1";

/// A scope names resolve in: a module's or a block's [`ItemKey`].
pub type Scope = ItemKey;

/// The narrowest thing a query read (docs/v2/plan/lanes/items.md section 6.2), and what invalidates it.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Fact {
    /// F's signature hash ([`SignatureHash`]): read by callers' type checking. What a body
    /// leaks is a fact of its own ([`Fact::HiddenTypes`], [`Fact::AutoTraits`],
    /// [`Fact::ConstValue`], [`Fact::Captures`]).
    Signature(ItemKey),
    /// T's layout hash: read by the codegen of everything that moves or stores a T.
    Layout(ItemKey),
    /// The type and offset of T's field `field` (by position).
    Field { ty: ItemKey, field: u32 },
    /// What `x.m()` resolves to on T: changes when an impl or trait adds, removes or changes
    /// an `m` reachable for T.
    Method { self_ty: SimplifiedType, name: NameHash },
    /// Whether T implements Trait: changes when an impl of Trait whose simplified self type
    /// unifies with T's is added, removed or changed.
    Implements { trait_: ItemKey, self_ty: SimplifiedType },
    /// What N in S resolves to (globs included): changes when an item named N appears or
    /// disappears in a scope S sees.
    Resolves { scope: Scope, name: NameHash },
    /// F's code hash: read by F's codegen, and by callers that inlined F (release).
    Code(ItemKey),
    /// The evaluated value, not its expression, of a const, a static's initializer (by the
    /// static's key) or an enum variant's discriminant (by the variant's key).
    ConstValue(ItemKey),
    /// X's name and path, for what observes them: `type_name`, `module_path!`, mangling,
    /// `#[no_mangle]`, `#[export_name]`, `#[link_name]`.
    Name(ItemKey),
    /// A closure's inferred signature and captures (#906).
    Captures(ItemKey),
    /// Whether an opaque type's hidden type is `Send`, `Sync`, `Unpin` (auto-trait leakage).
    AutoTraits(ItemKey),
    /// The rules that reach across items, each a fact of its own: coherence and orphan
    /// checks of an impl, specialization, drop check, vtable layout, a derive's output.
    Coherence(ItemKey),
    Specializes(ItemKey),
    DropCheck(ItemKey),
    Vtable { trait_: ItemKey, self_ty: SimplifiedType },
    Derive(ItemKey),
    /// The target a crate (by its [`key::crate_key`]) is built for: its tuple, data layout and
    /// spec, or why it could not be loaded.
    Target(ContentId),
    /// All of a crate (by its [`key::crate_key`]): the content of it and of every crate it
    /// depends on, their inputs and the files of their source roots. A dependency's items are
    /// lowered in batches of the whole crate, so a read of a batch stands for this.
    Crate(ContentId),
    /// The hidden type of each `impl Trait` of a function's return type or of a type alias
    /// (by the function's or alias's key): what `rpit_hidden_types` and `tait_hidden_types`
    /// give, which the defining bodies' inference decides.
    HiddenTypes(ItemKey),
}

/// The facts' query kinds, in [`blaze_query::kinds::FACTS`].
pub mod kinds {
    use blaze_query::QueryKind;

    pub const SIGNATURE: QueryKind = QueryKind(0x1000);
    pub const LAYOUT: QueryKind = QueryKind(0x1001);
    pub const FIELD: QueryKind = QueryKind(0x1002);
    pub const METHOD: QueryKind = QueryKind(0x1003);
    pub const IMPLEMENTS: QueryKind = QueryKind(0x1004);
    pub const RESOLVES: QueryKind = QueryKind(0x1005);
    pub const CODE: QueryKind = QueryKind(0x1006);
    pub const CONST_VALUE: QueryKind = QueryKind(0x1007);
    pub const NAME: QueryKind = QueryKind(0x1008);
    pub const CAPTURES: QueryKind = QueryKind(0x1009);
    pub const AUTO_TRAITS: QueryKind = QueryKind(0x100a);
    pub const COHERENCE: QueryKind = QueryKind(0x100b);
    pub const SPECIALIZES: QueryKind = QueryKind(0x100c);
    pub const DROP_CHECK: QueryKind = QueryKind(0x100d);
    pub const VTABLE: QueryKind = QueryKind(0x100e);
    pub const DERIVE: QueryKind = QueryKind(0x100f);
    /// An item's [`crate::ItemHashes`].
    pub const HASHES: QueryKind = QueryKind(0x1010);
    pub const TARGET: QueryKind = QueryKind(0x1011);
    pub const CRATE: QueryKind = QueryKind(0x1012);
    pub const HIDDEN_TYPES: QueryKind = QueryKind(0x1013);
    // 0x1800..0x1810 are lane item-hash's (`blaze_item_hash::values`): facts lane items does
    // not hash, listed in `blaze_item_hash::values::kinds`. None is defined here.
}

pub const FACT_DOMAIN: &str = "blaze v2 fact 1";

impl Fact {
    pub fn kind(&self) -> QueryKind {
        match self {
            Fact::Signature(_) => kinds::SIGNATURE,
            Fact::Layout(_) => kinds::LAYOUT,
            Fact::Field { .. } => kinds::FIELD,
            Fact::Method { .. } => kinds::METHOD,
            Fact::Implements { .. } => kinds::IMPLEMENTS,
            Fact::Resolves { .. } => kinds::RESOLVES,
            Fact::Code(_) => kinds::CODE,
            Fact::ConstValue(_) => kinds::CONST_VALUE,
            Fact::Name(_) => kinds::NAME,
            Fact::Captures(_) => kinds::CAPTURES,
            Fact::AutoTraits(_) => kinds::AUTO_TRAITS,
            Fact::Coherence(_) => kinds::COHERENCE,
            Fact::Specializes(_) => kinds::SPECIALIZES,
            Fact::DropCheck(_) => kinds::DROP_CHECK,
            Fact::Vtable { .. } => kinds::VTABLE,
            Fact::Derive(_) => kinds::DERIVE,
            Fact::Target(_) => kinds::TARGET,
            Fact::Crate(_) => kinds::CRATE,
            Fact::HiddenTypes(_) => kinds::HIDDEN_TYPES,
        }
    }

    /// The fact as the query a dependency edge names.
    pub fn key(&self) -> QueryKey {
        let mut hasher = Hasher::new(FACT_DOMAIN);
        hasher.update(&self.kind().0.to_le_bytes());
        match *self {
            Fact::Signature(it)
            | Fact::Layout(it)
            | Fact::Code(it)
            | Fact::ConstValue(it)
            | Fact::Name(it)
            | Fact::Captures(it)
            | Fact::AutoTraits(it)
            | Fact::Coherence(it)
            | Fact::Specializes(it)
            | Fact::DropCheck(it)
            | Fact::Derive(it)
            | Fact::HiddenTypes(it) => hasher.id(&it.0),
            Fact::Field { ty, field } => hasher.id(&ty.0).update(&field.to_le_bytes()),
            Fact::Method { self_ty, name } => hasher.id(&self_ty.0).id(&name.0),
            Fact::Implements { trait_, self_ty } | Fact::Vtable { trait_, self_ty } => {
                hasher.id(&trait_.0).id(&self_ty.0)
            }
            Fact::Resolves { scope, name } => hasher.id(&scope.0).id(&name.0),
            Fact::Target(krate) | Fact::Crate(krate) => hasher.id(&krate),
        };
        QueryKey { kind: self.kind(), input: hasher.finish() }
    }
}

/// The key of the index of impls of a trait for a simplified self type, which answers
/// [`Fact::Implements`] both ways.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ImplIndexKey {
    pub trait_: ItemKey,
    pub self_ty: SimplifiedType,
}

/// The key of the index of what a name is in a scope, which answers [`Fact::Resolves`] both
/// ways.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct ScopeIndexKey {
    pub scope: Scope,
    pub name: NameHash,
}
