//! The signatures part's portable ids and keys, for other parts of an image that name a crate's
//! items (`hir_ty::check_cache`): a table of items written as [`crate::nameres::frozen`] writes
//! them, and the key of an item's body.

use base_db::{Crate, Db};
use blaze_ids::ContentId;
use blaze_query::db::{AsId, FromId};

use super::{
    key::{self, Item},
    table,
};
use crate::{
    ConstId, DefWithBodyId, EnumId, EnumVariantId, FunctionId, ImplId, StaticId, StructId, TraitId,
    TypeAliasId, UnionId, VariantId,
    hir::persist::{Kind, Relocation, entry_id},
};

/// [`key::KEY_DOMAIN`]: a part filed by [`body_key`]s puts it in its schema, so a new key
/// encoding gives the part another key.
pub const BODY_KEY_DOMAIN: &str = key::KEY_DOMAIN;

/// Which item type a [`PortableId`] is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct IdKind(Kind);

/// An item id a [`PortableIds`] table holds.
pub trait PortableId: Copy + AsId + FromId {
    const KIND: IdKind;
}

macro_rules! portable {
    ($($ty:ty => $kind:ident),* $(,)?) => {$(
        impl PortableId for $ty {
            const KIND: IdKind = IdKind(Kind::$kind);
        }
    )*};
}

portable! {
    FunctionId => Function,
    StructId => Struct,
    UnionId => Union,
    EnumId => Enum,
    ConstId => Const,
    StaticId => Static,
    TraitId => Trait,
    TypeAliasId => TypeAlias,
    ImplId => Impl,
    EnumVariantId => EnumVariant,
}

/// Items of one crate, each at an index: written as the signatures part's table, read back in
/// another database as that database's ids.
#[derive(Default, Debug)]
pub struct PortableIds(Relocation);

impl PortableIds {
    pub fn new() -> PortableIds {
        PortableIds(Relocation::new())
    }

    /// The index of `id`, added when it is new.
    pub fn index<T: PortableId>(&mut self, id: T) -> u32 {
        self.0.index(T::KIND.0, u64::from(id.as_id().as_u32()))
    }

    /// The item at `index`, `None` when there is none or it is another type.
    pub fn get<T: PortableId>(&self, index: u32) -> Option<T> {
        self.0.get(T::KIND.0, index).map(|bits| T::from_id(entry_id(bits)))
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The table's bytes; an error names an item frozen cannot write (one of another crate's
    /// blocks, say).
    pub fn encode(&self, db: &Db, krate: Crate) -> Result<Vec<u8>, String> {
        table::encode(db, krate, &self.0)
    }

    /// [`Self::encode`]'s table, its items made again in `db`; an error for bytes that do not
    /// decode, never a panic.
    pub fn decode(db: &Db, krate: Crate, bytes: &[u8]) -> Result<PortableIds, String> {
        table::decode(db, krate, bytes).map(PortableIds)
    }
}

/// The key of `def`'s body, the same in every process: its item's key (a variant's, for a
/// variant's discriminant); `None` for a body in a block.
pub fn body_key(db: &Db, def: DefWithBodyId) -> Option<ContentId> {
    let item = match def {
        DefWithBodyId::FunctionId(it) => Item::Function(it),
        DefWithBodyId::ConstId(it) => Item::Const(it),
        DefWithBodyId::StaticId(it) => Item::Static(it),
        DefWithBodyId::VariantId(it) => Item::Fields(VariantId::EnumVariantId(it)),
    };
    key::of(db, item).map(|(_, key)| key)
}
