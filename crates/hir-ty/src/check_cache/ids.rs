//! The ids a body's inference record names, in a form another process makes again.
//!
//! Inside [`relocating`], every id the inference record and its type tables write goes through
//! [`id_bits`] as its index in a [`Table`] plus one (0 stays "no id"), and every id they read
//! through [`id_of`]; outside one, both are what they were: an id's bits. [`write`] then
//! gives each entry of the table its portable form, its location: a hir-def item as its index in
//! the crate's portable table (`hir_def::deps::PortableIds`, frontend-deps' frozen encoding), a
//! hir-ty interned id as what it is interned by, each id that location names an entry of the same
//! table. [`read`] makes them again in this database.

use std::cell::RefCell;

use base_db::{Crate, Db};
use blaze_format::{
    FormatError,
    cursor::{Put, Reader},
};
use blaze_hash::HashMap;
use blaze_memo::{Gid, Id as _};
use hir_def::{
    AdtId, BuiltinDeriveImplId, BuiltinDeriveImplLoc, ConstId, DefWithBodyId, EnumId,
    EnumVariantId, ExpressionStoreOwnerId, FunctionId, GenericDefId, ImplId, LoweringMode,
    StaticId, StructId, TraitId, TypeAliasId, UnionId, VariantId,
    builtin_derive::BuiltinDeriveImplTrait,
    deps::PortableIds,
    expr_store::ExpressionStore,
    hir::{ClosureKind, CoroutineKind, CoroutineSource, Expr, ExprId, Movability},
};
use hir_expand::attrs::AttrId;
use la_arena::{Idx, RawIdx};
use std::alloc::Global;

use crate::{
    ImplTraitId, InferBodyId,
    db::{
        AnonConstId, AnonConstLoc, InternedClosure, InternedClosureId, InternedCoroutineClosureId,
        InternedCoroutineId, InternedOpaqueTyId,
    },
    next_solver::{
        DbInterner, StoredEarlyBinder,
        persist::{TyTableReader, TyTableWriter},
    },
};

type R<T = ()> = Result<T, String>;

/// Which id type an entry of a [`Table`] is: an entry is read back only as the kind it was
/// written as. The first ten are hir-def's items, which the crate's portable table holds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub(crate) enum IdKind {
    Function,
    Struct,
    Union,
    Enum,
    Const,
    Static,
    Trait,
    TypeAlias,
    Impl,
    EnumVariant,
    BuiltinDeriveImpl,
    OpaqueTy,
    Closure,
    Coroutine,
    CoroutineClosure,
    AnonConst,
}

impl IdKind {
    /// Every kind, each at its number.
    pub(crate) const ALL: [IdKind; 16] = [
        IdKind::Function,
        IdKind::Struct,
        IdKind::Union,
        IdKind::Enum,
        IdKind::Const,
        IdKind::Static,
        IdKind::Trait,
        IdKind::TypeAlias,
        IdKind::Impl,
        IdKind::EnumVariant,
        IdKind::BuiltinDeriveImpl,
        IdKind::OpaqueTy,
        IdKind::Closure,
        IdKind::Coroutine,
        IdKind::CoroutineClosure,
        IdKind::AnonConst,
    ];

    fn of(byte: u8) -> Option<IdKind> {
        IdKind::ALL.get(usize::from(byte)).copied()
    }

    /// A hir-def item, held by the crate's portable table.
    fn is_item(self) -> bool {
        (self as u8) <= (IdKind::EnumVariant as u8)
    }

    /// Made after the `locs` type table is opened, so a type in it may not name one.
    fn is_late(self) -> bool {
        (self as u8) > (IdKind::BuiltinDeriveImpl as u8)
    }
}

const _: () = {
    let mut at = 0;
    while at < IdKind::ALL.len() {
        assert!(IdKind::ALL[at] as usize == at, "IdKind::ALL is not in the order of the kinds");
        at += 1;
    }
};

/// An id type the inference record and its type tables write, by the kind its table
/// entries are.
pub(crate) trait Relocate: blaze_memo::Id {
    const KIND: IdKind;
}

macro_rules! relocate {
    ($($ty:ty => $kind:ident),* $(,)?) => {$(
        impl Relocate for $ty {
            const KIND: IdKind = IdKind::$kind;
        }
    )*};
}

relocate! {
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
    BuiltinDeriveImplId => BuiltinDeriveImpl,
}

macro_rules! relocate_db {
    ($($ty:ident => $kind:ident),* $(,)?) => {$(
        impl<'db> Relocate for $ty<'db> {
            const KIND: IdKind = IdKind::$kind;
        }
    )*};
}

relocate_db! {
    InternedOpaqueTyId => OpaqueTy,
    InternedClosureId => Closure,
    InternedCoroutineId => Coroutine,
    InternedCoroutineClosureId => CoroutineClosure,
    AnonConstId => AnonConst,
}

/// The ids a body's record names, each once, in the order first written. Written, [`id_bits`]
/// fills it; read, [`read`] makes each entry in this database, in strata.
#[derive(Default, Debug)]
pub(crate) struct Table {
    entries: Vec<(IdKind, u64)>,
    indexes: HashMap<(IdKind, u64), u32, Global>,
    /// Set while the `locs` type table is written: an id of a late kind refuses the body.
    locs: bool,
    refused: Option<&'static str>,
}

impl Table {
    /// The index of the id `id` of `kind`, added when it is new.
    fn index(&mut self, kind: IdKind, id: Gid) -> u32 {
        let bits = gid_bits(id);
        let next = self.entries.len();
        *self.indexes.entry((kind, bits)).or_insert_with(|| {
            self.entries.push((kind, bits));
            u32::try_from(next).expect("more than 2^32 ids in one body")
        })
    }

    /// The bits of the id `index` names: `None` when there is none, it is of another kind, or
    /// it is not made yet (bits 0).
    fn get(&self, kind: IdKind, index: u32) -> Option<u64> {
        let &(found, bits) = self.entries.get(index as usize)?;
        (found == kind && bits != 0).then_some(bits)
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }
}

thread_local! {
    /// The table of the [`relocating`] scope this thread is in, if any.
    static TABLE: RefCell<Option<Table>> = const { RefCell::new(None) };
}

/// Runs `f` with every id the inference record and the persisted type tables write or read
/// going through `table`. `table` holds what was added when `f` returns or unwinds. Scopes do
/// not nest. The database is called inside `f` only to intern values and to read an ADT's
/// flags, which [`outside`] leaves the scope for; it has no path that writes or reads a
/// persisted record of its own there (`evict.rs` drops values and computes them again).
pub(crate) fn relocating<T>(table: &mut Table, f: impl FnOnce() -> T) -> T {
    carry_table();
    stdx::lend(&TABLE, table, f)
}

/// Carries [`TABLE`] with each job on a fiber across its waits ([`blaze_query::carry`]): a
/// scope whose interning waits on a query resumes in it, wherever it resumes.
fn carry_table() {
    static CARRIED: std::sync::Once = std::sync::Once::new();
    CARRIED.call_once(|| {
        blaze_query::carry(blaze_query::Carried {
            take: || stdx::lend::take_carried(&TABLE, Option::is_none),
            put: |it| stdx::lend::put_carried(&TABLE, Option::is_none, it),
            end: |it| stdx::lend::end_carried(&TABLE, it),
        });
    });
}

/// Runs `f` outside the [`relocating`] scope this thread is in, if any: what `f` writes or
/// reads is in this database's own ids.
pub(crate) fn outside<T>(f: impl FnOnce() -> T) -> T {
    struct Restore(Option<Table>);

    impl Drop for Restore {
        fn drop(&mut self) {
            replace_table(self.0.take());
        }
    }

    match replace_table(None) {
        None => f(),
        Some(table) => {
            let _restore = Restore(Some(table));
            f()
        }
    }
}

// Each access to `TABLE` is a call of its own, never inlined: a job that waits between two may
// resume on another thread, and an inlined access may reuse the address of the first thread's
// slot (docs/v2/plan/design/may-suspend.md).

#[inline(never)]
fn replace_table(table: Option<Table>) -> Option<Table> {
    TABLE.with(|slot| std::mem::replace(&mut *slot.borrow_mut(), table))
}

/// Whether this thread is in a [`relocating`] scope: a reader puts back in this database's
/// order what the writer's ids ordered.
#[inline(never)]
pub(crate) fn in_scope() -> bool {
    TABLE.with(|slot| slot.borrow().is_some())
}

/// The bits written for `id`: its index in the scope's table plus one, or its own bits
/// outside a scope.
#[inline(never)]
pub(crate) fn id_bits<T: Relocate>(id: T) -> u64 {
    let id = id.gid();
    TABLE.with(|slot| match slot.borrow_mut().as_mut() {
        None => gid_bits(id),
        Some(table) => {
            if table.locs && T::KIND.is_late() {
                table.refused.get_or_insert(
                    "an anonymous constant's type names a closure, an opaque type or a constant",
                );
            }
            u64::from(table.index(T::KIND, id)) + 1
        }
    })
}

/// Why the bits a record wrote for an id ([`id_of`]) name none, with the bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum IdRefused {
    /// Bits 0, which name no id.
    Zero,
    /// In a scope, an index (the bits less one) past the record's table of `len` entries.
    PastTable { index: u64, len: u64 },
    /// In a scope, an entry of another kind than the reader's, or one not made yet.
    OtherKind,
    /// Outside a scope, bits no id of this session has ([`id_at`]: a zero local or an
    /// all-ones arena half).
    NoId { bits: u64 },
}

impl IdRefused {
    /// The error of the record at byte `at` that held the refused bits.
    pub(crate) fn at(self, at: u64) -> FormatError {
        match self {
            IdRefused::Zero => FormatError::Zero { at, what: "an id's bits" },
            IdRefused::PastTable { index, len } => FormatError::Bound {
                at,
                what: "an id's index in the record's table",
                found: index,
                bound: len,
            },
            IdRefused::OtherKind => FormatError::Invalid {
                at,
                what: "an id whose entry in the record's table is of another kind or not made",
            },
            IdRefused::NoId { .. } => FormatError::Invalid {
                at,
                what: "an id's bits that name no id of this session (a zero local or an all-ones arena)",
            },
        }
    }
}

/// The id `bits` name, as [`id_bits`] wrote them, or why they name none: 0, and in a scope an
/// index the table has no id of `T`'s kind at. Outside a scope a word is refused when no id
/// has its bits ([`id_at`]: a damaged word can name none, so this is an error, never a
/// panic); an id it does name is checked where its fields are read, once
/// (`blaze_memo::Id::from_gid`).
#[inline(never)]
pub(crate) fn id_of<T: Relocate>(bits: u64) -> Result<T, IdRefused> {
    if bits == 0 {
        return Err(IdRefused::Zero);
    }
    TABLE.with(|slot| match slot.borrow().as_ref() {
        None => id_at(bits).map(T::from_gid).ok_or(IdRefused::NoId { bits }),
        Some(table) => {
            let index = bits - 1;
            let len = table.len() as u64;
            let index = u32::try_from(index)
                .ok()
                .filter(|&it| u64::from(it) < len)
                .ok_or(IdRefused::PastTable { index, len })?;
            let bits = table.get(T::KIND, index).ok_or(IdRefused::OtherKind)?;
            Ok(T::from_gid(entry_id(bits)))
        }
    })
}

/// The word an id is in this process, inside a [`Table`] and outside a scope: its arena and
/// index ([`Gid::session_bits`]) plus one, so that no id is 0, the word of no id. Never written
/// across processes: [`write`] gives each table entry its portable form (an index in the
/// crate's [`PortableIds`], or what a hir-ty id is interned by), and [`read`] makes the ids
/// again; a `Gid` means nothing in another process (spec B, `Revision`).
pub(crate) fn gid_bits(id: Gid) -> u64 {
    id.session_bits().checked_add(1).expect("an id's bits under the last word (an arena index of 2^20 - 1 at its last generation)")
}

/// The word of the id `id`, as [`gid_bits`] gives it.
pub(crate) fn word_of<T: blaze_memo::Id>(id: T) -> u64 {
    gid_bits(id.gid())
}

/// The id whose word is `bits` ([`gid_bits`]): `None` for 0 and for bits no id has.
pub(crate) fn id_at(bits: u64) -> Option<Gid> {
    bits.checked_sub(1).and_then(Gid::from_session_bits)
}

/// The id a [`Table`] entry holds: [`Table::index`] adds only an id's bits.
fn entry_id(bits: u64) -> Gid {
    id_at(bits).expect("a table entry is an id's bits")
}

/// The crate's items every body of its part names: frontend-deps' portable table, and the kind
/// of each of its entries, in the table's order.
#[derive(Default)]
pub(crate) struct Items {
    pub(crate) ids: PortableIds,
    pub(crate) kinds: Vec<u8>,
}

/// The version of what [`write`] and [`write_with`] write of a body's ids.
pub(crate) const VERSION: u64 = 1;

/// Where a body's items go when its ids are written ([`write_with`]): each item is written as
/// its index there.
pub(crate) trait ItemSink {
    /// Whether every item of the body (`items`, as `(kind, bits)`, in the table's order) has a
    /// form here, asked before any is added: a body refused here adds nothing.
    fn admit(&mut self, db: &Db, items: impl Iterator<Item = (IdKind, u64)>) -> R;

    /// The index of the item `bits` of `kind`, added when it is new.
    fn index(&mut self, kind: IdKind, bits: u64) -> R<u32>;
}

/// A crate's portable table ([`Items`]), as a bodies part writes its bodies' items.
struct CrateItems<'i> {
    krate: Crate,
    items: &'i mut Items,
}

impl ItemSink for CrateItems<'_> {
    fn admit(&mut self, db: &Db, items: impl Iterator<Item = (IdKind, u64)>) -> R {
        // alloc: per body written, at store time
        let mut own = PortableIds::new();
        for (kind, bits) in items {
            index_item(&mut own, kind, bits)?;
        }
        own.encode(db, self.krate).map(drop)
    }

    fn index(&mut self, kind: IdKind, bits: u64) -> R<u32> {
        let index = index_item(&mut self.items.ids, kind, bits)?;
        if index as usize == self.items.kinds.len() {
            self.items.kinds.push(kind as u8);
        }
        Ok(index)
    }
}

/// What an entry is in another process: where its id comes from. Entries name others by index.
#[derive(Clone, Copy, Debug)]
enum Loc {
    /// An index in the crate's portable table.
    Item(u32),
    Derive {
        adt: u32,
        trait_: u8,
        attr: u32,
        index: u32,
    },
    Opaque {
        alias: bool,
        def: u32,
        idx: u32,
    },
    Closure {
        owner: (u8, u32),
        expr: u32,
        kind: [u8; 3],
    },
    AnonConst {
        owner: (u8, u32),
        expr: u32,
        ty: u32,
        allow: bool,
    },
}

/// The words of `table` in portable form, after the record was written into it: the locations
/// of its hir-ty ids (whose own ids are added to it), the types of its anonymous constants into
/// `locs`, and its items into `items`, the crate's portable table. A body whose ids have no
/// portable form is refused with why; nothing of it is added to `items` then.
pub(crate) fn write<'db>(
    db: &'db Db,
    krate: Crate,
    table: &mut Table,
    locs: &mut TyTableWriter<'db>,
    items: &mut Items,
) -> R<Vec<u8>> {
    write_with(db, table, locs, &mut CrateItems { krate, items })
}

/// [`write`], each item of the body written as its index in `items`.
pub(crate) fn write_with<'db>(
    db: &'db Db,
    table: &mut Table,
    locs: &mut TyTableWriter<'db>,
    items: &mut impl ItemSink,
) -> R<Vec<u8>> {
    if let Some(why) = table.refused {
        // alloc: the reason a body is left out
        return Err(why.to_owned());
    }
    // alloc: per body written, at store time
    let mut found = Vec::with_capacity(table.len());
    let mut at = 0;
    while at < table.len() {
        let (kind, bits) = table.entries[at];
        let loc = match kind {
            IdKind::Function
            | IdKind::Struct
            | IdKind::Union
            | IdKind::Enum
            | IdKind::Const
            | IdKind::Static
            | IdKind::Trait
            | IdKind::TypeAlias
            | IdKind::Impl
            | IdKind::EnumVariant => Loc::Item(0),
            IdKind::BuiltinDeriveImpl => {
                let BuiltinDeriveImplLoc { adt, trait_, derive_attr_id, derive_index } =
                    *BuiltinDeriveImplId::from_gid(entry_id(bits)).loc(db);
                let trait_ = BuiltinDeriveImplTrait::ALL
                    .iter()
                    .position(|it| *it == trait_)
                    .expect("a derive's trait is one of ALL");
                Loc::Derive {
                    adt: adt_entry(table, adt),
                    trait_: u8::try_from(trait_).expect("fewer than 256 derivable traits"),
                    attr: derive_attr_id.item_tree_index(),
                    index: derive_index,
                }
            }
            IdKind::OpaqueTy => match InternedOpaqueTyId::from_gid(entry_id(bits)).loc(db) {
                ImplTraitId::ReturnTypeImplTrait(def, idx) => Loc::Opaque {
                    alias: false,
                    def: table.index(IdKind::Function, def.gid()),
                    idx: idx.into_raw().into_u32(),
                },
                ImplTraitId::TypeAliasImplTrait(def, idx) => Loc::Opaque {
                    alias: true,
                    def: table.index(IdKind::TypeAlias, def.gid()),
                    idx: idx.into_raw().into_u32(),
                },
            },
            IdKind::Closure | IdKind::Coroutine | IdKind::CoroutineClosure => {
                let id = entry_id(bits);
                let InternedClosure { owner, expr, kind: closure_kind } = match kind {
                    IdKind::Closure => InternedClosureId::from_gid(id).loc(db),
                    IdKind::Coroutine => InternedCoroutineId::from_gid(id).loc(db),
                    _ => InternedCoroutineClosureId::from_gid(id).loc(db),
                };
                Loc::Closure {
                    owner: infer_owner_entry(table, owner),
                    expr: expr.into_raw().into_u32(),
                    kind: closure_kind_code(closure_kind),
                }
            }
            IdKind::AnonConst => {
                let loc = AnonConstId::from_gid(entry_id(bits)).loc(db);
                let ty = loc.ty.as_ref().skip_binder().as_ref();
                let owner = store_owner_entry(table, loc.owner);
                table.locs = true;
                let ty = relocating(table, || locs.ty(ty));
                table.locs = false;
                if let Some(why) = table.refused {
                    // alloc: the reason a body is left out
                    return Err(why.to_owned());
                }
                Loc::AnonConst {
                    owner,
                    expr: loc.expr.into_raw().into_u32(),
                    ty,
                    allow: loc.allow_using_generic_params,
                }
            }
        };
        found.push(loc);
        at += 1;
    }
    // Checked alone first, so an item frozen's encoding refuses leaves this body out rather than
    // failing the crate's table.
    let body_items =
        table.entries.iter().zip(&found).filter(|(_, loc)| matches!(loc, Loc::Item(_)));
    items.admit(db, body_items.map(|(&entry, _)| entry))?;

    // alloc: the body's words, kept in its part
    let mut out = Vec::with_capacity(table.len() * 8);
    for (&(kind, bits), loc) in table.entries.iter().zip(&found) {
        out.push(kind as u8);
        match *loc {
            Loc::Item(_) => out.put_u32(items.index(kind, bits)?),
            Loc::Derive { adt, trait_, attr, index } => {
                out.put_u32(adt);
                out.push(trait_);
                out.put_u32(attr);
                out.put_u32(index);
            }
            Loc::Opaque { alias, def, idx } => {
                out.push(u8::from(alias));
                out.put_u32(def);
                out.put_u32(idx);
            }
            Loc::Closure { owner: (tag, owner), expr, kind } => {
                out.push(tag);
                out.put_u32(owner);
                out.put_u32(expr);
                out.extend_from_slice(&kind);
            }
            Loc::AnonConst { owner: (tag, owner), expr, ty, allow } => {
                out.push(tag);
                out.put_u32(owner);
                out.put_u32(expr);
                out.put_u32(ty);
                out.push(u8::from(allow));
            }
        }
    }
    Ok(out)
}

/// The index of the item `bits` of `kind` in `items`, added when it is new.
fn index_item(items: &mut PortableIds, kind: IdKind, bits: u64) -> R<u32> {
    let id = entry_id(bits);
    Ok(match kind {
        IdKind::Function => items.index(FunctionId::from_gid(id)),
        IdKind::Struct => items.index(StructId::from_gid(id)),
        IdKind::Union => items.index(UnionId::from_gid(id)),
        IdKind::Enum => items.index(EnumId::from_gid(id)),
        IdKind::Const => items.index(ConstId::from_gid(id)),
        IdKind::Static => items.index(StaticId::from_gid(id)),
        IdKind::Trait => items.index(TraitId::from_gid(id)),
        IdKind::TypeAlias => items.index(TypeAliasId::from_gid(id)),
        IdKind::Impl => items.index(ImplId::from_gid(id)),
        IdKind::EnumVariant => items.index(EnumVariantId::from_gid(id)),
        // alloc: the error that leaves the body out
        _ => return Err(format!("a {kind:?} is no item of a portable table")),
    })
}

/// The bits of the item `index` names in `items` as a `kind`, `None` when it names another.
pub(crate) fn item_bits(items: &PortableIds, kind: IdKind, index: u32) -> Option<u64> {
    fn bits(id: impl blaze_memo::Id) -> u64 {
        word_of(id)
    }
    match kind {
        IdKind::Function => items.get::<FunctionId>(index).map(bits),
        IdKind::Struct => items.get::<StructId>(index).map(bits),
        IdKind::Union => items.get::<UnionId>(index).map(bits),
        IdKind::Enum => items.get::<EnumId>(index).map(bits),
        IdKind::Const => items.get::<ConstId>(index).map(bits),
        IdKind::Static => items.get::<StaticId>(index).map(bits),
        IdKind::Trait => items.get::<TraitId>(index).map(bits),
        IdKind::TypeAlias => items.get::<TypeAliasId>(index).map(bits),
        IdKind::Impl => items.get::<ImplId>(index).map(bits),
        IdKind::EnumVariant => items.get::<EnumVariantId>(index).map(bits),
        _ => None,
    }
}

/// The kind byte of the crate table's entries, for [`super::part`]'s `kinds` column.
pub(crate) fn item_kind_of(byte: u8) -> Option<IdKind> {
    IdKind::of(byte).filter(|it| it.is_item())
}

fn adt_entry(table: &mut Table, adt: AdtId) -> u32 {
    match adt {
        AdtId::StructId(it) => table.index(IdKind::Struct, it.gid()),
        AdtId::UnionId(it) => table.index(IdKind::Union, it.gid()),
        AdtId::EnumId(it) => table.index(IdKind::Enum, it.gid()),
    }
}

/// A body owner: its tag (0 a function, 1 a static, 2 a constant, 3 a variant's discriminant)
/// and its entry.
fn body_entry(table: &mut Table, def: DefWithBodyId) -> (u8, u32) {
    match def {
        DefWithBodyId::FunctionId(it) => (0, table.index(IdKind::Function, it.gid())),
        DefWithBodyId::StaticId(it) => (1, table.index(IdKind::Static, it.gid())),
        DefWithBodyId::ConstId(it) => (2, table.index(IdKind::Const, it.gid())),
        DefWithBodyId::VariantId(it) => (3, table.index(IdKind::EnumVariant, it.gid())),
    }
}

/// A closure's owner: a body ([`body_entry`]'s tags) or an anonymous constant (tag 4).
fn infer_owner_entry(table: &mut Table, owner: InferBodyId<'_>) -> (u8, u32) {
    match owner {
        InferBodyId::DefWithBodyId(def) => body_entry(table, def),
        InferBodyId::AnonConstId(it) => (4, table.index(IdKind::AnonConst, it.gid())),
    }
}

/// An anonymous constant's store: a signature (tags 0 to 8, `GenericDefId`'s variants in
/// order, an ADT as its three), a body (9 to 12) or a variant's fields (13 to 15).
fn store_owner_entry(table: &mut Table, owner: ExpressionStoreOwnerId) -> (u8, u32) {
    let mut item = |kind, id: Gid| table.index(kind, id);
    match owner {
        ExpressionStoreOwnerId::Signature(def) => match def {
            GenericDefId::AdtId(AdtId::StructId(it)) => (0, item(IdKind::Struct, it.gid())),
            GenericDefId::AdtId(AdtId::EnumId(it)) => (1, item(IdKind::Enum, it.gid())),
            GenericDefId::AdtId(AdtId::UnionId(it)) => (2, item(IdKind::Union, it.gid())),
            GenericDefId::ConstId(it) => (3, item(IdKind::Const, it.gid())),
            GenericDefId::FunctionId(it) => (4, item(IdKind::Function, it.gid())),
            GenericDefId::ImplId(it) => (5, item(IdKind::Impl, it.gid())),
            GenericDefId::StaticId(it) => (6, item(IdKind::Static, it.gid())),
            GenericDefId::TraitId(it) => (7, item(IdKind::Trait, it.gid())),
            GenericDefId::TypeAliasId(it) => (8, item(IdKind::TypeAlias, it.gid())),
        },
        ExpressionStoreOwnerId::Body(def) => {
            let (tag, entry) = body_entry(table, def);
            (9 + tag, entry)
        }
        ExpressionStoreOwnerId::VariantFields(variant) => match variant {
            VariantId::EnumVariantId(it) => (13, item(IdKind::EnumVariant, it.gid())),
            VariantId::StructId(it) => (14, item(IdKind::Struct, it.gid())),
            VariantId::UnionId(it) => (15, item(IdKind::Union, it.gid())),
        },
    }
}

fn coroutine_kind_code(kind: CoroutineKind) -> u8 {
    match kind {
        CoroutineKind::Async => 0,
        CoroutineKind::Gen => 1,
        CoroutineKind::AsyncGen => 2,
    }
}

/// A closure's kind as three bytes, the same in every database: what a stored id of a closure
/// and a body-relative record key (`blaze_item_hash::keys`) carry, so reading one back opens
/// no expression store.
pub fn closure_kind_code(kind: ClosureKind) -> [u8; 3] {
    match kind {
        ClosureKind::Closure => [0, 0, 0],
        ClosureKind::OldCoroutine(movability) => [
            1,
            match movability {
                Movability::Static => 0,
                Movability::Movable => 1,
            },
            0,
        ],
        ClosureKind::Coroutine { kind, source } => [
            2,
            coroutine_kind_code(kind),
            match source {
                CoroutineSource::Block => 0,
                CoroutineSource::Closure => 1,
                CoroutineSource::Fn => 2,
            },
        ],
        ClosureKind::CoroutineClosure(kind) => [3, coroutine_kind_code(kind), 0],
    }
}

fn coroutine_kind_of(code: u8) -> R<CoroutineKind> {
    Ok(match code {
        0 => CoroutineKind::Async,
        1 => CoroutineKind::Gen,
        2 => CoroutineKind::AsyncGen,
        // alloc: the reason of a refusal
        _ => return Err(format!("a coroutine of kind {code}")),
    })
}

/// The kind [`closure_kind_code`] wrote, or why the bytes are no kind.
pub fn closure_kind_of([tag, a, b]: [u8; 3]) -> R<ClosureKind> {
    Ok(match (tag, a, b) {
        (0, 0, 0) => ClosureKind::Closure,
        (1, 0, 0) => ClosureKind::OldCoroutine(Movability::Static),
        (1, 1, 0) => ClosureKind::OldCoroutine(Movability::Movable),
        (2, kind, source) => ClosureKind::Coroutine {
            kind: coroutine_kind_of(kind)?,
            source: match source {
                0 => CoroutineSource::Block,
                1 => CoroutineSource::Closure,
                2 => CoroutineSource::Fn,
                // alloc: the reason of a refusal
                _ => return Err(format!("a coroutine of source {source}")),
            },
        },
        (3, kind, 0) => ClosureKind::CoroutineClosure(coroutine_kind_of(kind)?),
        // alloc: the reason of a refusal
        _ => return Err(format!("a closure of kind {tag} {a} {b}")),
    })
}

/// The table of a body whose ids are `ids` (what [`write`] gave), each made in this database:
/// items from `items`, the crate's portable table read as `(kind, bits)`; the types of its
/// anonymous constants from the table at `locs` in `buffer`.
pub(crate) fn read<'db>(
    db: &'db Db,
    interner: DbInterner<'db>,
    items: &[(IdKind, u64)],
    ids: &[u8],
    buffer: &[u8],
    locs: u64,
) -> R<Table> {
    // alloc: the table of one body read, as long as its ids
    let mut entries = Vec::new();
    // alloc: as above
    let mut found = Vec::new();
    let mut bytes = Reader::new(ids);
    while !bytes.is_done() {
        let kind = IdKind::of(bytes.u8()?).ok_or("an id of no kind")?;
        let loc = match kind {
            IdKind::Function
            | IdKind::Struct
            | IdKind::Union
            | IdKind::Enum
            | IdKind::Const
            | IdKind::Static
            | IdKind::Trait
            | IdKind::TypeAlias
            | IdKind::Impl
            | IdKind::EnumVariant => Loc::Item(bytes.u32()?),
            IdKind::BuiltinDeriveImpl => Loc::Derive {
                adt: bytes.u32()?,
                trait_: bytes.u8()?,
                attr: bytes.u32()?,
                index: bytes.u32()?,
            },
            IdKind::OpaqueTy => Loc::Opaque {
                alias: match bytes.u8()? {
                    0 => false,
                    1 => true,
                    // alloc: the reason of a refusal
                    _ => return Err("an opaque type of no kind".to_owned()),
                },
                def: bytes.u32()?,
                idx: bytes.u32()?,
            },
            IdKind::Closure | IdKind::Coroutine | IdKind::CoroutineClosure => Loc::Closure {
                owner: (bytes.u8()?, bytes.u32()?),
                expr: bytes.u32()?,
                kind: [bytes.u8()?, bytes.u8()?, bytes.u8()?],
            },
            IdKind::AnonConst => Loc::AnonConst {
                owner: (bytes.u8()?, bytes.u32()?),
                expr: bytes.u32()?,
                ty: bytes.u32()?,
                allow: match bytes.u8()? {
                    0 => false,
                    1 => true,
                    // alloc: the reason of a refusal
                    _ => return Err("a bool that is neither 0 nor 1".to_owned()),
                },
            },
        };
        entries.push((kind, 0));
        found.push(loc);
    }
    let mut table = Table { entries, ..Table::default() };

    // Items and derive impls: what the `locs` table may name.
    for (at, loc) in found.iter().enumerate() {
        if let Loc::Item(index) = *loc {
            let kind = table.entries[at].0;
            match items.get(index as usize) {
                Some(&(found, bits)) if found == kind => table.entries[at].1 = bits,
                // alloc: the reason of a refusal
                _ => {
                    return Err(format!(
                        "an item of index {index} the crate's table has no {kind:?} at"
                    ));
                }
            }
        }
    }
    for (at, loc) in found.iter().enumerate() {
        if let Loc::Derive { adt, trait_, attr, index } = *loc {
            let adt = adt_of(&table, adt)?;
            let trait_ = *BuiltinDeriveImplTrait::ALL
                .get(usize::from(trait_))
                .ok_or("a derive of no trait")?;
            let loc = BuiltinDeriveImplLoc {
                adt,
                trait_,
                derive_attr_id: AttrId::from_item_tree_index(attr),
                derive_index: index,
            };
            table.entries[at].1 = word_of(BuiltinDeriveImplId::new(db, loc));
        }
    }

    let tys = relocating(&mut table, || TyTableReader::open(interner, buffer, locs))
        // alloc: the reason of a refusal
        .map_err(|error| format!("its constants' types: {error}"))?;
    for (at, loc) in found.iter().enumerate() {
        let bits = match *loc {
            Loc::Opaque { alias, def, idx } => {
                let idx = Idx::from_raw(RawIdx::from_u32(idx));
                let loc = match alias {
                    false => ImplTraitId::ReturnTypeImplTrait(entry(&table, def)?, idx),
                    true => ImplTraitId::TypeAliasImplTrait(entry(&table, def)?, idx),
                };
                word_of(InternedOpaqueTyId::new(db, loc))
            }
            Loc::AnonConst { owner, expr, ty, allow } => {
                // alloc: the reason of a refusal
                let ty = tys.ty(ty).map_err(|error| format!("a constant's type: {error}"))?;
                let loc = AnonConstLoc {
                    owner: store_owner_of(&table, owner)?,
                    expr: ExprId::from_raw(RawIdx::from_u32(expr)),
                    ty: StoredEarlyBinder::bind_no_rigid_aliases(ty.store()),
                    allow_using_generic_params: allow,
                };
                let token = LoweringMode::Analysis
                    .allow_tracked_structs()
                    .expect("analysis lowering may make tracked structs");
                word_of(AnonConstId::new(db, loc, &token))
            }
            _ => continue,
        };
        table.entries[at].1 = bits;
    }
    // Closures last: an owner may be an anonymous constant.
    for (at, loc) in found.iter().enumerate() {
        if let Loc::Closure { owner, expr, kind } = *loc {
            let owner = infer_owner_of(&table, owner)?;
            let closure_kind = closure_kind_of(kind)?;
            let id_kind = table.entries[at].0;
            let expr = closure_expr(db, owner, expr, id_kind, closure_kind)?;
            let loc = InternedClosure { owner, expr, kind: closure_kind };
            table.entries[at].1 = gid_bits(match id_kind {
                IdKind::Closure => InternedClosureId::new(db, loc).gid(),
                IdKind::Coroutine => InternedCoroutineId::new(db, loc).gid(),
                _ => InternedCoroutineClosureId::new(db, loc).gid(),
            });
        }
    }
    Ok(table)
}

/// The id entry `index` of `table` is, made already, as a `T`.
fn entry<T: Relocate>(table: &Table, index: u32) -> R<T> {
    table
        .get(T::KIND, index)
        .and_then(id_at)
        .map(T::from_gid)
        // alloc: the reason of a refusal
        .ok_or_else(|| format!("an entry {index} that is no {:?} made before it", T::KIND))
}

fn adt_of(table: &Table, index: u32) -> R<AdtId> {
    let kind = table.entries.get(index as usize).ok_or("an ADT past the table")?.0;
    Ok(match kind {
        IdKind::Struct => AdtId::StructId(entry(table, index)?),
        IdKind::Union => AdtId::UnionId(entry(table, index)?),
        IdKind::Enum => AdtId::EnumId(entry(table, index)?),
        // alloc: the reason of a refusal
        _ => return Err(format!("an ADT that is a {kind:?}")),
    })
}

fn body_of(table: &Table, (tag, index): (u8, u32)) -> R<DefWithBodyId> {
    Ok(match tag {
        0 => DefWithBodyId::FunctionId(entry(table, index)?),
        1 => DefWithBodyId::StaticId(entry(table, index)?),
        2 => DefWithBodyId::ConstId(entry(table, index)?),
        3 => DefWithBodyId::VariantId(entry(table, index)?),
        // alloc: the reason of a refusal
        _ => return Err(format!("a body owner of tag {tag}")),
    })
}

fn infer_owner_of<'db>(table: &Table, (tag, index): (u8, u32)) -> R<InferBodyId<'db>> {
    match tag {
        4 => Ok(InferBodyId::AnonConstId(entry(table, index)?)),
        _ => body_of(table, (tag, index)).map(InferBodyId::DefWithBodyId),
    }
}

fn store_owner_of(table: &Table, (tag, index): (u8, u32)) -> R<ExpressionStoreOwnerId> {
    let signature = |def: GenericDefId| ExpressionStoreOwnerId::Signature(def);
    Ok(match tag {
        0 => signature(AdtId::StructId(entry(table, index)?).into()),
        1 => signature(AdtId::EnumId(entry(table, index)?).into()),
        2 => signature(AdtId::UnionId(entry(table, index)?).into()),
        3 => signature(GenericDefId::ConstId(entry(table, index)?)),
        4 => signature(GenericDefId::FunctionId(entry(table, index)?)),
        5 => signature(GenericDefId::ImplId(entry(table, index)?)),
        6 => signature(GenericDefId::StaticId(entry(table, index)?)),
        7 => signature(GenericDefId::TraitId(entry(table, index)?)),
        8 => signature(GenericDefId::TypeAliasId(entry(table, index)?)),
        9..=12 => ExpressionStoreOwnerId::Body(body_of(table, (tag - 9, index))?),
        13 => ExpressionStoreOwnerId::VariantFields(VariantId::EnumVariantId(entry(table, index)?)),
        14 => ExpressionStoreOwnerId::VariantFields(VariantId::StructId(entry(table, index)?)),
        15 => ExpressionStoreOwnerId::VariantFields(VariantId::UnionId(entry(table, index)?)),
        // alloc: the reason of a refusal
        _ => return Err(format!("a store owner of tag {tag}")),
    })
}

/// The expression `expr` of `owner`'s store, checked to be a closure an id of `kind` may name:
/// what `InternedClosureId::new` and its siblings assert in debug builds.
///
/// Where the body is given from its record (`body_store::given`) its store is not there (its
/// text is absent): the expression is checked to be one of the closures the record lists
/// (`db::closure_exprs`), and the closure kind the record wrote (`written`) against `kind`.
fn closure_expr(db: &Db, owner: InferBodyId<'_>, expr: u32, kind: IdKind, written: ClosureKind) -> R<ExprId> {
    let fits_kind = |closure_kind: &ClosureKind| match kind {
        IdKind::Closure => matches!(closure_kind, ClosureKind::Closure | ClosureKind::CoroutineClosure(_)),
        IdKind::Coroutine => matches!(closure_kind, ClosureKind::OldCoroutine(_) | ClosureKind::Coroutine { .. }),
        _ => matches!(closure_kind, ClosureKind::CoroutineClosure(_)),
    };
    if crate::body_store::given(db) {
        let at = ExprId::from_raw(RawIdx::from_u32(expr));
        let listed = crate::db::closure_exprs(db, owner.expression_store_owner(db)).binary_search(&at).is_ok();
        return match (listed, fits_kind(&written)) {
            (true, true) => Ok(at),
            // alloc: the reason of a refusal
            (false, _) => Err(format!("a closure at expression {expr}, which the body's record lists no closure at")),
            // alloc: the reason of a refusal
            (true, false) => Err(format!("a {kind:?} written of a closure kind {written:?}")),
        };
    }
    let store = ExpressionStore::of(db, owner.expression_store_owner(db));
    if expr as usize >= store.expr_count() {
        // alloc: the reason of a refusal
        return Err(format!("a closure at expression {expr}, past its body's"));
    }
    let expr = ExprId::from_raw(RawIdx::from_u32(expr));
    let fits = match store.expr(expr) {
        Expr::Closure { closure_kind, .. } => fits_kind(&closure_kind),
        _ => false,
    };
    match fits {
        true => Ok(expr),
        // alloc: the reason of a refusal
        false => Err(format!("a {kind:?} at an expression that is not one")),
    }
}
