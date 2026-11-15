//! Generic arguments, clauses and ids written in one database and made again in another, for a
//! record that is of no one crate (docs/v2/plan/design/store.md, "Describe records"): the code
//! generator's describe of a function names instances of many crates, each a body with its
//! arguments.
//!
//! The same codec as a bodies part's ([`super::ids`], `next_solver::persist`), one home for it:
//! inside a [`super::ids::relocating`] scope every id is its index in a table, and the table's
//! entries are written by where their ids come from. What differs is how a hir-def item is
//! named. A bodies part names it by its index in its crate's portable table, which only that
//! crate's image reads; here the caller names each item by 32 bytes of its own (the code
//! generator gives `blaze_items::item_key`, which is of the item's crate and path and never of
//! an id) and finds it again from them, so one buffer names items of any crate.
//!
//! Nothing of the writing database is in the bytes: no id, no row of an interner. Two databases
//! of the same sources that write the same values in the same order give the same bytes, but
//! for one case: a `dyn` type's projections and auto traits are written in the order the writing
//! database holds them, which is by its ids ([`super::in_scope`] sorts them again when read).
//! A reader of such a type gets the right type; a key made of the bytes may differ between two
//! databases that numbered those traits in another order, which is a miss, never another value.

use std::mem::offset_of;

use base_db::Db;
use blaze_format::{
    FormatError, Part as SchemaPart, Placed, Record, SchemaHash, Slice, open_as, schema_of,
};
use blaze_hash::HashMap;
use blaze_query::db::{AsId, FromId};
use hir_def::{
    ConstId, EnumId, EnumVariantId, FunctionId, ImplId, StaticId, StructId, TraitId, TypeAliasId,
    UnionId,
};
use std::alloc::Global;

use super::ids::{self, IdKind, ItemSink, Relocate, Table};
use crate::{
    db::{InternedClosureId, InternedCoroutineId},
    next_solver::{
        Clauses, DbInterner, GenericArgs,
        persist::{TABLE_SCHEMA, TyTableReader, TyTableWriter},
    },
};

type R<T = ()> = Result<T, String>;

/// What a caller names an item by: 32 bytes that are the same in every database that has the
/// item, and another item's in none.
pub type Name = [u8; 32];

/// One item in the buffer: its kind's byte, then its name.
const NAMED: usize = 1 + size_of::<Name>();

blaze_format::record! {
    /// A ported buffer: where its type table and the table of its anonymous constants' types
    /// are, its ids as [`super::ids`] writes them, and its items, each [`NAMED`] bytes.
    #[derive(Clone, Copy)]
    struct Root: 1 {
        tys: u64,
        locs: u64,
        ids: Slice<u8>,
        items: Slice<u8>,
    }
}

const _: () = assert!(size_of::<Root>() == 32);

/// The schema of a ported buffer, and the version of everything its bytes are made with.
pub const SCHEMA: SchemaHash = schema_of(&[
    SchemaPart::Str("blaze v2 ported ids and types 1"),
    SchemaPart::Schema(<Root as Record>::SCHEMA),
    SchemaPart::Schema(TABLE_SCHEMA),
    SchemaPart::U64(ids::VERSION),
]);

#[expect(
    clippy::needless_pass_by_value,
    reason = "the `map_err` callback of every read, which hands it the error by value"
)]
fn format_error(error: FormatError) -> String {
    // alloc: the reason of a refusal
    error.to_string()
}

/// A hir-def item a ported value names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Item {
    Function(FunctionId),
    Struct(StructId),
    Union(UnionId),
    Enum(EnumId),
    Const(ConstId),
    Static(StaticId),
    Trait(TraitId),
    TypeAlias(TypeAliasId),
    Impl(ImplId),
    EnumVariant(EnumVariantId),
}

impl Item {
    fn of(kind: IdKind, bits: u64) -> Option<Item> {
        let id = u32::try_from(bits).ok().and_then(blaze_query::db::Id::new)?;
        Some(match kind {
            IdKind::Function => Item::Function(FunctionId::from_id(id)),
            IdKind::Struct => Item::Struct(StructId::from_id(id)),
            IdKind::Union => Item::Union(UnionId::from_id(id)),
            IdKind::Enum => Item::Enum(EnumId::from_id(id)),
            IdKind::Const => Item::Const(ConstId::from_id(id)),
            IdKind::Static => Item::Static(StaticId::from_id(id)),
            IdKind::Trait => Item::Trait(TraitId::from_id(id)),
            IdKind::TypeAlias => Item::TypeAlias(TypeAliasId::from_id(id)),
            IdKind::Impl => Item::Impl(ImplId::from_id(id)),
            IdKind::EnumVariant => Item::EnumVariant(EnumVariantId::from_id(id)),
            IdKind::BuiltinDeriveImpl
            | IdKind::OpaqueTy
            | IdKind::Closure
            | IdKind::Coroutine
            | IdKind::CoroutineClosure
            | IdKind::AnonConst => return None,
        })
    }

    fn kind(self) -> IdKind {
        match self {
            Item::Function(_) => IdKind::Function,
            Item::Struct(_) => IdKind::Struct,
            Item::Union(_) => IdKind::Union,
            Item::Enum(_) => IdKind::Enum,
            Item::Const(_) => IdKind::Const,
            Item::Static(_) => IdKind::Static,
            Item::Trait(_) => IdKind::Trait,
            Item::TypeAlias(_) => IdKind::TypeAlias,
            Item::Impl(_) => IdKind::Impl,
            Item::EnumVariant(_) => IdKind::EnumVariant,
        }
    }

    fn bits(self) -> u64 {
        let id = match self {
            Item::Function(it) => it.as_id(),
            Item::Struct(it) => it.as_id(),
            Item::Union(it) => it.as_id(),
            Item::Enum(it) => it.as_id(),
            Item::Const(it) => it.as_id(),
            Item::Static(it) => it.as_id(),
            Item::Trait(it) => it.as_id(),
            Item::TypeAlias(it) => it.as_id(),
            Item::Impl(it) => it.as_id(),
            Item::EnumVariant(it) => it.as_id(),
        };
        u64::from(id.as_u32())
    }
}

/// Writes values of one database into one buffer: each value is named by the `u32` its method
/// returns (a place in the type table for arguments and clauses, an entry of the id table for
/// an id), which [`Reader`]'s method of the same name takes.
pub struct Writer<'db> {
    db: &'db Db,
    interner: DbInterner<'db>,
    table: Table,
    tys: TyTableWriter<'db>,
}

impl<'db> Writer<'db> {
    pub fn new(db: &'db Db, interner: DbInterner<'db>) -> Writer<'db> {
        Writer { db, interner, table: Table::default(), tys: TyTableWriter::new(interner) }
    }

    pub fn args(&mut self, args: GenericArgs<'db>) -> u32 {
        let Writer { table, tys, .. } = self;
        ids::relocating(table, || tys.args(args))
    }

    pub fn clauses(&mut self, clauses: Clauses<'db>) -> u32 {
        let Writer { table, tys, .. } = self;
        ids::relocating(table, || tys.clauses(clauses))
    }

    pub fn function(&mut self, id: FunctionId) -> u32 {
        self.entry(id)
    }

    pub fn static_item(&mut self, id: StaticId) -> u32 {
        self.entry(id)
    }

    pub fn closure(&mut self, id: InternedClosureId<'db>) -> u32 {
        self.entry(id)
    }

    pub fn coroutine(&mut self, id: InternedCoroutineId<'db>) -> u32 {
        self.entry(id)
    }

    /// The entry of `id` in the id table, added when it is new: what [`ids::id_bits`] writes
    /// for it in a scope, less the one that keeps 0 for "no id".
    fn entry<T: Relocate>(&mut self, id: T) -> u32 {
        let bits = ids::relocating(&mut self.table, || ids::id_bits(id));
        u32::try_from(bits - 1).expect("an id table of fewer than 2^32 entries")
    }

    /// The buffer of everything written. `name` gives each hir-def item the values name its
    /// [`Name`], or why it has none (an item in a block, say): the buffer is refused then, with
    /// that reason, as it is for an id [`ids`] has no portable form of.
    pub fn finish(
        mut self,
        // dyn: the caller's naming of items, once per item of a buffer
        name: &mut dyn FnMut(Item) -> R<Name>,
    ) -> R<Vec<u8>> {
        let mut locs = TyTableWriter::new(self.interner);
        let mut names = Names {
            name,
            // alloc: per buffer written, its items' names
            bytes: Vec::new(),
            indexes: HashMap::new_in(Global),
        };
        let words = ids::write_with(self.db, &mut self.table, &mut locs, &mut names)?;
        let mut out = blaze_format::Writer::new(SCHEMA);
        let words = Placed::push(&mut out, &words);
        let items = Placed::push(&mut out, &names.bytes);
        let tys = self.tys.write(&mut out);
        let locs = locs.write(&mut out);
        let at = out.next::<Root>();
        let field = |offset: usize| at + offset as u64;
        let root = Root {
            tys,
            locs,
            ids: words.slice(field(offset_of!(Root, ids))),
            items: items.slice(field(offset_of!(Root, items))),
        };
        let written = out.push(&root);
        debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
        Ok(out.finish(at))
    }
}

/// The items of a buffer as it is written: each named once, in the order first asked for.
struct Names<'n> {
    // dyn: as `Writer::finish`'s
    name: &'n mut dyn FnMut(Item) -> R<Name>,
    bytes: Vec<u8>,
    indexes: HashMap<(IdKind, u64), u32, Global>,
}

impl ItemSink for Names<'_> {
    fn admit(&mut self, _: &Db, items: impl Iterator<Item = (IdKind, u64)>) -> R {
        // Named before any is indexed, so a buffer with an item that has no name is refused
        // whole.
        for (kind, bits) in items {
            self.index(kind, bits)?;
        }
        Ok(())
    }

    fn index(&mut self, kind: IdKind, bits: u64) -> R<u32> {
        if let Some(&index) = self.indexes.get(&(kind, bits)) {
            return Ok(index);
        }
        // alloc: the reason of a refusal
        let item = Item::of(kind, bits).ok_or_else(|| format!("a {kind:?} is no item"))?;
        let name = (self.name)(item)?;
        let index = u32::try_from(self.bytes.len() / NAMED).map_err(|_| "more than 2^32 items")?;
        self.bytes.push(kind as u8);
        self.bytes.extend_from_slice(&name);
        self.indexes.insert((kind, bits), index);
        Ok(index)
    }
}

/// A buffer [`Writer::finish`] gave, its ids made and its types interned in this database.
pub struct Reader<'a, 'db> {
    table: Table,
    tys: TyTableReader<'a, 'db>,
}

impl<'a, 'db> Reader<'a, 'db> {
    /// `bytes` read into `db`. `item` gives the item of this database a [`Name`] names, `None`
    /// when it has none: the buffer is then refused, naming nothing of it, as it is for bytes
    /// that do not read. Never a panic: the bytes are a store's.
    pub fn open(
        db: &'db Db,
        interner: DbInterner<'db>,
        bytes: &'a [u8],
        // dyn: the caller's lookup of items, once per item of a buffer
        item: &mut dyn FnMut(&Name) -> Option<Item>,
    ) -> R<Reader<'a, 'db>> {
        let root = open_as::<Root>(bytes, SCHEMA).map_err(format_error)?;
        let words = root.ids.follow(bytes).map_err(format_error)?;
        let named = root.items.follow(bytes).map_err(format_error)?;
        if !named.len().is_multiple_of(NAMED) {
            // alloc: the reason of a refusal
            return Err("its items are not whole".to_owned());
        }
        // alloc: per buffer read, its items in this database
        let mut items = Vec::with_capacity(named.len() / NAMED);
        for chunk in named.chunks_exact(NAMED) {
            let kind = ids::item_kind_of(chunk[0]).ok_or("an item of no kind")?;
            let name: &Name = chunk[1..].try_into().map_err(|_| "an item's name cut short")?;
            let found = item(name).ok_or("an item this database has none of")?;
            if found.kind() != kind {
                // alloc: the reason of a refusal
                return Err(format!("an item named as a {kind:?} that is a {:?}", found.kind()));
            }
            items.push((kind, found.bits()));
        }
        let mut table = ids::read(db, interner, &items, words, bytes, root.locs)?;
        let tys = ids::relocating(&mut table, || TyTableReader::open(interner, bytes, root.tys))
            .map_err(format_error)?;
        Ok(Reader { table, tys })
    }

    pub fn args(&self, at: u32) -> R<GenericArgs<'db>> {
        self.tys.args(at).map_err(format_error)
    }

    pub fn clauses(&self, at: u32) -> R<Clauses<'db>> {
        self.tys.clauses(at).map_err(format_error)
    }

    pub fn function(&mut self, entry: u32) -> R<FunctionId> {
        self.entry(entry)
    }

    pub fn static_item(&mut self, entry: u32) -> R<StaticId> {
        self.entry(entry)
    }

    pub fn closure(&mut self, entry: u32) -> R<InternedClosureId<'db>> {
        self.entry(entry)
    }

    pub fn coroutine(&mut self, entry: u32) -> R<InternedCoroutineId<'db>> {
        self.entry(entry)
    }

    /// The id at `entry` of the id table, as a `T`: what [`ids::id_of`] reads in a scope.
    fn entry<T: Relocate>(&mut self, entry: u32) -> R<T> {
        ids::relocating(&mut self.table, || ids::id_of::<T>(u64::from(entry) + 1))
            // alloc: the reason of a refusal
            .map_err(|refused| format!("entry {entry} of the id table: {refused:?}"))
    }
}
