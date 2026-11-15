//! The signatures part of a dependency's image (docs/v2/plan/design/frontend.md item 5): every
//! signature of every item of the crate's modules, one record per signature query, each found
//! by its item's [`key`] and decoded alone, in place.
//!
//! The part is a [`blaze_format`] buffer of [`SCHEMA`]: the records, each a buffer of its own
//! at an aligned place, then the keys in increasing order and, at the same index, where each
//! record is. Opening the part checks those two plain columns and nothing else; finding a record
//! is a binary search of the keys; no other record is read.
//!
//! A record is the signature's words ([`codes`]), its expression store as a buffer of its own,
//! and the [`table`] of the ids both name. Each id is written as its index in the table
//! ([`crate::hir::persist::relocating`]) and the table as [`crate::nameres::frozen`] writes what
//! it names, so a record means the same in every process.
//!
//! [`Mode::Check`] lowers the signature from its syntax as well and records a difference
//! between the two records; [`Mode::Verify`] writes what was read and records a difference from
//! the record. A difference is a bug in an encoding or the key: the image's signature is served
//! in every mode.

mod codes;
mod ids;
pub(crate) mod key;
pub(crate) mod table;

pub use self::ids::{BODY_KEY_DOMAIN, IdKind, PortableId, PortableIds, body_key};
#[cfg(test)]
mod tests;

use std::mem::offset_of;

use base_db::{Crate, Db};
use blaze_format::{
    ALIGN, FormatError, Part as SchemaPart, Persist, Record, SchemaHash, Slice, Writer, open_as,
};
use blaze_ids::ContentId;
use hir_expand::{AstId, MacroCallId, name::Name};
use syntax::ast;
use thin_vec::ThinVec;

use self::{codes::Sig, key::Item, table::StoreBytes};
use crate::{
    AdtId, AssocItemId, AttrDefId, ConstId, EnumId, FunctionId, ImplId, ItemContainerId, Lookup,
    ModuleDefId, ModuleId, StaticId, StructId, TraitId, TypeAliasId, UnionId, VariantId,
    attrs::AttrFlags,
    deps::Part,
    item_tree::RawVisibility,
    nameres::frozen::entries::items::{decode_flags, flags_bytes},
    expr_store::ExpressionStore,
    hir::persist::{
        Column, Portable, Read, Relocation, SessionBytes, Words, WordsRec, relocating,
    },
    item_scope::ItemScope,
    item_tree::ItemTree,
    nameres::{
        assoc::{ImplItems, TraitItems},
        crate_def_map,
        frozen::{self, Mode},
    },
    signatures::{
        ConstSignature, EnumSignature, EnumVariants, FunctionSignature, ImplSignature,
        InactiveEnumVariantCode, StaticSignature, StructSignature, TraitSignature,
        TypeAliasSignature, UnionSignature, VariantFields,
    },
};

type R<T = ()> = Result<T, String>;

blaze_format::record! {
    /// Where a record is in the part, from the part's start.
    #[derive(Clone, Copy, Debug)]
    struct Place: 1 {
        at: u32,
        len: u32,
    }
}

blaze_format::record! {
    /// The part: each item's key, in increasing order, where its record is, and the attribute
    /// flags of the item the record is of ([`flags_of`]), each at the same index.
    #[derive(Clone, Copy)]
    struct PartRoot: 1 {
        keys: Slice<ContentId>,
        places: Slice<Place>,
        flags: Slice<[u8; 16]>,
    }
}

blaze_format::record! {
    /// One record: the query it answers ([`Item::what`]), its words, its table and its store
    /// (empty for `EnumVariants`, which has none).
    #[derive(Clone, Copy)]
    struct RecordRoot: 1 {
        words: WordsRec,
        table: Slice<u8>,
        store: Slice<u8>,
        what: u32,
    }
}

const _: () = {
    assert!(size_of::<Place>() == 8);
    assert!(size_of::<PartRoot>() == 24);
    assert!(size_of::<RecordRoot>() == 36);
};

/// The schema of the part, and the version of everything its bytes are made with: the words of
/// each signature, the store's buffer, the item tree's codes (visibilities, shapes), the key and
/// the frozen encoding of the table. 8: the assoc items of each impl and trait
/// (`Item::ImplItems`, `Item::TraitItems`) and the flags column (`PartRoot::flags`),
/// so a crate given by its address answers both with no text of its files; a part of 7 is
/// refused by this schema, and its key ([`super::key::PRODUCER`] hashes this) is never asked.
pub(crate) const SCHEMA: SchemaHash = blaze_format::schema_of(&[
    SchemaPart::Str("blaze v2 dependency signatures 8"),
    SchemaPart::Schema(<PartRoot as Record>::SCHEMA),
    SchemaPart::Schema(<RecordRoot as Record>::SCHEMA),
    SchemaPart::Schema(<ExpressionStore as Persist>::SCHEMA),
    SchemaPart::Schema(<ItemTree as Persist>::SCHEMA),
    SchemaPart::Str(key::KEY_DOMAIN),
    SchemaPart::Str(frozen::ENCODING),
]);


/// The domain of the digest of a signature record's bytes: the fact a read of it lists.
const RECORD_FACT_DOMAIN: &str = "blaze v2 dependency signature record fact";

/// The schema of one record's buffer.
const RECORD_SCHEMA: SchemaHash = blaze_format::schema_of(&[
    SchemaPart::Str("blaze v2 dependency signature record 3"),
    SchemaPart::Schema(<RecordRoot as Record>::SCHEMA),
]);

/// The signatures part of the image of `krate`: a record of every signature query of every item
/// of its modules. An item that cannot be written portably fails the whole part, with why.
/// Appends to `calls` each macro call a record names.
pub(crate) fn encode(db: &Db, krate: Crate, calls: &mut Vec<MacroCallId>) -> R<Portable> {
    encode_with(db, krate, calls, |item, calls| record_of(db, krate, item, calls))
}

/// [`encode`], each item's record written by `record_of`: the one layout of the part, which a
/// test hands a record that differs from the item's own.
fn encode_with(
    db: &Db,
    krate: Crate,
    calls: &mut Vec<MacroCallId>,
    mut record_of: impl FnMut(Item, &mut Vec<MacroCallId>) -> R<Portable>,
) -> R<Portable> {
    let items = items(db, krate);
    let mut keyed = Vec::with_capacity(items.len());
    for item in items {
        // alloc: the error that fails the part.
        let (_, key) = key::of(db, item).ok_or_else(|| format!("{item:?} is in a block"))?;
        keyed.push((key, item));
    }
    keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    keyed.dedup();
    if let Some(pair) = keyed.windows(2).find(|pair| pair[0].0 == pair[1].0) {
        // alloc: the error that fails the part.
        return Err(format!("{:?} and {:?} have one key", pair[0].1, pair[1].1));
    }
    let mut out = Writer::new(SCHEMA);
    let mut places = Vec::with_capacity(keyed.len());
    // The flags column, 16 bytes a record, written once per crate.
    let mut flags = Vec::with_capacity_in(keyed.len(), std::alloc::Global);
    // alloc: the error that fails the part.
    let place = |it: usize| u32::try_from(it).map_err(|_| "a part of 4 GiB or more".to_owned());
    for &(_, item) in &keyed {
        let record = record_of(item, calls)
            // alloc: the error that fails the part.
            .map_err(|why| format!("the signature of {item:?}: {why}"))?;
        let at = out.bytes.len().next_multiple_of(ALIGN);
        out.bytes.resize(at, 0);
        out.bytes.extend_from_slice(record.as_bytes());
        places.push(Place { at: place(at)?, len: place(record.len())? });
        let of = flags_owner(item).map_or(AttrFlags::empty(), |owner| AttrFlags::query(db, owner));
        flags.push(flags_bytes(of));
    }
    // alloc: the key column, written once per crate.
    let keys: Vec<ContentId> = keyed.iter().map(|&(key, _)| key).collect();
    let keys = Column::push(&mut out, &keys);
    let places = Column::push(&mut out, &places);
    let flags = Column::push(&mut out, &flags);
    let at = out.next::<PartRoot>();
    let root = PartRoot {
        keys: keys.slice(at + offset_of!(PartRoot, keys) as u64),
        places: places.slice(at + offset_of!(PartRoot, places) as u64),
        flags: flags.slice(at + offset_of!(PartRoot, flags) as u64),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    // Keys, places and the records, each [`Portable`] already.
    Ok(Portable::written(out.finish(at)))
}

/// Each signature query of the items of `krate`'s modules: what the front end asks of a
/// dependency's items. Items in blocks are left out: their blocks are lowered with the body
/// they are in.
fn items(db: &Db, krate: Crate) -> Vec<Item> {
    // alloc: the items of one crate, listed once at store time.
    let mut items = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        scope_items(db, module.scope(db), &mut items);
    }
    items
}

/// Appends to `items` each signature query of the items `scope` declares, its impls' and its
/// traits' too.
pub(super) fn scope_items(db: &Db, scope: &ItemScope, items: &mut Vec<Item>) {
    for def in scope.declarations() {
        match def {
            ModuleDefId::FunctionId(it) => items.push(Item::Function(it)),
            ModuleDefId::AdtId(AdtId::StructId(it)) => {
                items.extend([Item::Struct(it), Item::Fields(VariantId::StructId(it))]);
            }
            ModuleDefId::AdtId(AdtId::UnionId(it)) => {
                items.extend([Item::Union(it), Item::Fields(VariantId::UnionId(it))]);
            }
            ModuleDefId::AdtId(AdtId::EnumId(it)) => {
                items.extend([Item::Enum(it), Item::Variants(it)]);
                for &(variant, _) in EnumVariants::of(db, it).0.variants.values() {
                    items.push(Item::Fields(VariantId::EnumVariantId(variant)));
                }
            }
            ModuleDefId::ConstId(it) => items.push(Item::Const(it)),
            ModuleDefId::StaticId(it) => items.push(Item::Static(it)),
            // With its items, below.
            ModuleDefId::TraitId(_) => {}
            ModuleDefId::TypeAliasId(it) => items.push(Item::TypeAlias(it)),
            ModuleDefId::ModuleId(_)
            | ModuleDefId::EnumVariantId(_)
            | ModuleDefId::BuiltinType(_)
            | ModuleDefId::MacroId(_) => {}
        }
    }
    items.extend(scope.unnamed_consts().map(Item::Const));
    for owner in assoc_owners(db, scope) {
        items.push(owner.item);
        items.push(match owner.item {
            Item::Trait(it) => Item::TraitItems(it),
            Item::Impl(it) => Item::ImplItems(it),
            other => unreachable!("an assoc owner is a trait or an impl, and {other:?} is not"),
        });
        assoc(items, owner.items);
    }
}

/// Every macro call `scope` makes whose output its image holds: those of its items
/// ([`ItemScope::all_macro_calls`]) and those at assoc-item position in its traits and impls
/// (`impl S { #[attr] fn m() {} }`, `impl S { m!(); }`), whose items [`scope_items`] lists.
/// The calls of an image's scopes ([`super::reads::image_calls`]) and of a check's
/// (`filed::checked_reads`), so what an assoc-position proc macro or `include!` read is never
/// left out of either.
pub(super) fn scope_calls<'db>(
    db: &'db Db,
    scope: &'db ItemScope,
) -> impl Iterator<Item = MacroCallId> + 'db {
    let assoc = assoc_owners(db, scope).flat_map(|owner| owner.calls.iter().map(|&(_, call)| call));
    scope.all_macro_calls().chain(assoc)
}

/// A trait or an impl of a scope, with its assoc items and the macro calls at assoc-item
/// position some of them came from.
struct AssocOwner<'db> {
    item: Item,
    items: &'db [(Name, AssocItemId)],
    calls: &'db [(AstId<ast::Item>, MacroCallId)],
}

/// The traits `scope` declares, then its impls: one list for the assoc items [`scope_items`]
/// images and the calls [`scope_calls`] walks.
fn assoc_owners<'db>(
    db: &'db Db,
    scope: &'db ItemScope,
) -> impl Iterator<Item = AssocOwner<'db>> + 'db {
    let traits = scope.declarations().filter_map(move |def| {
        let ModuleDefId::TraitId(it) = def else { return None };
        let assoc = TraitItems::query(db, it);
        Some(AssocOwner { item: Item::Trait(it), items: &assoc.items, calls: &assoc.macro_calls })
    });
    let impls = scope.impls().map(move |it| {
        let assoc = &ImplItems::of(db, it).0;
        AssocOwner { item: Item::Impl(it), items: &assoc.items, calls: &assoc.macro_calls }
    });
    traits.chain(impls)
}

/// The store of the signature `item` asks for: `None` for [`Item::Variants`], which has none.
pub(super) fn store_of(db: &Db, item: Item) -> Option<&ExpressionStore> {
    Some(match item {
        Item::Struct(id) => &StructSignature::of(db, id).store,
        Item::Union(id) => &UnionSignature::of(db, id).store,
        Item::Enum(id) => &EnumSignature::of(db, id).store,
        Item::Const(id) => &ConstSignature::of(db, id).store,
        Item::Static(id) => &StaticSignature::of(db, id).store,
        Item::Impl(id) => &ImplSignature::of(db, id).store,
        Item::Trait(id) => &TraitSignature::of(db, id).store,
        Item::Function(id) => &FunctionSignature::of(db, id).store,
        Item::TypeAlias(id) => &TypeAliasSignature::of(db, id).store,
        Item::Fields(id) => &VariantFields::of(db, id).store,
        Item::Variants(_) | Item::ImplItems(_) | Item::TraitItems(_) => return None,
    })
}

fn assoc(items: &mut Vec<Item>, assoc: &[(Name, AssocItemId)]) {
    items.extend(assoc.iter().map(|&(_, it)| match it {
        AssocItemId::FunctionId(it) => Item::Function(it),
        AssocItemId::ConstId(it) => Item::Const(it),
        AssocItemId::TypeAliasId(it) => Item::TypeAlias(it),
    }));
}

/// The record of `item` of `krate`, from its signature as this database has it.
fn record_of(db: &Db, krate: Crate, item: Item, calls: &mut Vec<MacroCallId>) -> R<Portable> {
    let what = item.what();
    match item {
        Item::Struct(id) => {
            record_naming::<codes::Struct>(db, krate, what, StructSignature::of(db, id), calls)
        }
        Item::Union(id) => {
            record_naming::<codes::Union>(db, krate, what, UnionSignature::of(db, id), calls)
        }
        Item::Enum(id) => {
            record_naming::<codes::Enum>(db, krate, what, EnumSignature::of(db, id), calls)
        }
        Item::Const(id) => {
            record_naming::<codes::Const>(db, krate, what, ConstSignature::of(db, id), calls)
        }
        Item::Static(id) => {
            record_naming::<codes::Static>(db, krate, what, StaticSignature::of(db, id), calls)
        }
        Item::Impl(id) => {
            record_naming::<codes::Impl>(db, krate, what, ImplSignature::of(db, id), calls)
        }
        Item::Trait(id) => {
            record_naming::<codes::Trait>(db, krate, what, TraitSignature::of(db, id), calls)
        }
        Item::Function(id) => {
            record_naming::<codes::Function>(db, krate, what, FunctionSignature::of(db, id), calls)
        }
        Item::TypeAlias(id) => record_naming::<codes::TypeAlias>(
            db,
            krate,
            what,
            TypeAliasSignature::of(db, id),
            calls,
        ),
        Item::Fields(id) => {
            record_naming::<codes::Fields>(db, krate, what, VariantFields::of(db, id), calls)
        }
        Item::Variants(id) => {
            record_naming::<codes::Variants>(db, krate, what, EnumVariants::of(db, id), calls)
        }
        Item::ImplItems(id) => {
            let of = assoc_record(db, ItemContainerId::ImplId(id));
            record_naming::<codes::Assoc>(db, krate, what, &of, calls)
        }
        Item::TraitItems(id) => {
            let of = assoc_record(db, ItemContainerId::TraitId(id));
            record_naming::<codes::Assoc>(db, krate, what, &of, calls)
        }
    }
}

/// The assoc items of an impl or a trait as an image holds them: what `ImplItems::of` and
/// `TraitItems::query_with_diagnostics` answer of it (their diagnostics left out: a crate read
/// from an image reports none), and the visibility written on each of an impl's items, or on a
/// trait (which each of its items has).
///
/// Floor (rule 67): one record found by one binary search of the part's keys, its words read in
/// place and each id made again through its table; nothing of the crate's files. Cost, an
/// estimate not measured: the record's head and table (about 100 bytes) plus about 16 bytes an
/// item (its name's index, its kind and table index, its visibility's code) and each item's
/// location in the table (about 12 bytes), so about 30 bytes an assoc item and 100 an owner.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AssocRecord {
    pub(crate) is_trait: bool,
    pub(crate) items: Box<[(Name, AssocItemId)]>,
    pub(crate) macro_calls: ThinVec<(AstId<ast::Item>, MacroCallId)>,
    /// An impl's: the visibility written on each item, at its index in `items`. A trait's: one,
    /// the trait's.
    pub(crate) visibilities: Box<[RawVisibility]>,
}

/// The [`AssocRecord`] of `container`, an impl or a trait, from its syntax.
fn assoc_record(db: &Db, container: ItemContainerId) -> AssocRecord {
    match container {
        ItemContainerId::ImplId(id) => {
            let (items, _) = ImplItems::of(db, id);
            assoc_record_of(db, container, &items.items, &items.macro_calls)
        }
        ItemContainerId::TraitId(id) => {
            let items = TraitItems::query(db, id);
            assoc_record_of(db, container, &items.items, &items.macro_calls)
        }
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
            unreachable!("assoc items are of an impl or a trait")
        }
    }
}

/// The [`AssocRecord`] of `container` with its items and macro calls as given, its
/// visibilities read off its syntax: [`assoc_record`]'s, and a test's of items it collected
/// from syntax itself.
pub(crate) fn assoc_record_of(
    db: &Db,
    container: ItemContainerId,
    items: &[(Name, AssocItemId)],
    macro_calls: &ThinVec<(AstId<ast::Item>, MacroCallId)>,
) -> AssocRecord {
    match container {
        ItemContainerId::ImplId(id) => {
            let module = id.lookup(db).container;
            let visibilities =
                items.iter().map(|&(_, item)| assoc_written_visibility(db, item, module));
            AssocRecord {
                is_trait: false,
                // clone: the image's copy, made once a crate at store time
                items: items.into(),
                // clone: as above
                macro_calls: macro_calls.clone(),
                visibilities: visibilities.collect(),
            }
        }
        ItemContainerId::TraitId(id) => {
            let loc = id.lookup(db);
            let visibility = crate::signatures::written_visibility(db, loc.id, loc.container);
            AssocRecord {
                is_trait: true,
                // clone: the image's copy, made once a crate at store time
                items: items.into(),
                // clone: as above
                macro_calls: macro_calls.clone(),
                visibilities: Box::new([visibility]),
            }
        }
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
            unreachable!("assoc items are of an impl or a trait")
        }
    }
}

/// The visibility written on `item`, an assoc item of a module `module`'s impl, unresolved.
fn assoc_written_visibility(db: &Db, item: AssocItemId, module: ModuleId) -> RawVisibility {
    match item {
        AssocItemId::FunctionId(it) => {
            crate::signatures::written_visibility(db, it.lookup(db).id, module)
        }
        AssocItemId::ConstId(it) => {
            crate::signatures::written_visibility(db, it.lookup(db).id, module)
        }
        AssocItemId::TypeAliasId(it) => {
            crate::signatures::written_visibility(db, it.lookup(db).id, module)
        }
    }
}

/// The container `item` was made in.
fn assoc_container(db: &Db, item: AssocItemId) -> ItemContainerId {
    match item {
        AssocItemId::FunctionId(it) => it.lookup(db).container,
        AssocItemId::ConstId(it) => it.lookup(db).container,
        AssocItemId::TypeAliasId(it) => it.lookup(db).container,
    }
}

/// The owner of the attribute flags the part holds at the record of `item` (its flags column):
/// the item a signature is of (a variant's fields record is the variant's). `None` for a
/// record of no attribute owner of its own (a struct's or union's fields, an enum's variants,
/// an impl's or trait's assoc items), whose column holds no flags.
fn flags_owner(item: Item) -> Option<AttrDefId> {
    Some(match item {
        Item::Struct(it) => AttrDefId::AdtId(AdtId::StructId(it)),
        Item::Union(it) => AttrDefId::AdtId(AdtId::UnionId(it)),
        Item::Enum(it) => AttrDefId::AdtId(AdtId::EnumId(it)),
        Item::Const(it) => AttrDefId::ConstId(it),
        Item::Static(it) => AttrDefId::StaticId(it),
        Item::Impl(it) => AttrDefId::ImplId(it),
        Item::Trait(it) => AttrDefId::TraitId(it),
        Item::Function(it) => AttrDefId::FunctionId(it),
        Item::TypeAlias(it) => AttrDefId::TypeAliasId(it),
        Item::Fields(VariantId::EnumVariantId(it)) => AttrDefId::EnumVariantId(it),
        Item::Fields(VariantId::StructId(_) | VariantId::UnionId(_))
        | Item::Variants(_)
        | Item::ImplItems(_)
        | Item::TraitItems(_) => return None,
    })
}

/// The record whose flags are `owner`'s ([`flags_owner`]'s inverse): `None` for an owner the
/// part holds no flags of (a module, a macro, an extern block, an `extern crate`, a `use`).
fn flags_item(owner: AttrDefId) -> Option<Item> {
    Some(match owner {
        AttrDefId::AdtId(AdtId::StructId(it)) => Item::Struct(it),
        AttrDefId::AdtId(AdtId::UnionId(it)) => Item::Union(it),
        AttrDefId::AdtId(AdtId::EnumId(it)) => Item::Enum(it),
        AttrDefId::ConstId(it) => Item::Const(it),
        AttrDefId::StaticId(it) => Item::Static(it),
        AttrDefId::ImplId(it) => Item::Impl(it),
        AttrDefId::TraitId(it) => Item::Trait(it),
        AttrDefId::FunctionId(it) => Item::Function(it),
        AttrDefId::TypeAliasId(it) => Item::TypeAlias(it),
        AttrDefId::EnumVariantId(it) => Item::Fields(VariantId::EnumVariantId(it)),
        AttrDefId::ModuleId(_)
        | AttrDefId::MacroId(_)
        | AttrDefId::ExternBlockId(_)
        | AttrDefId::ExternCrateId(_)
        | AttrDefId::UseId(_) => return None,
    })
}

/// The record of `of`, the value of query `what` of an item of `krate`. The same value is the
/// same bytes in every process: the table lists the ids in the order they are first written,
/// the store first.
fn record<S: Sig>(db: &Db, krate: Crate, what: u8, of: &S::Of) -> R<Portable> {
    // alloc: none: an empty list, which nothing reads
    record_naming::<S>(db, krate, what, of, &mut Vec::new())
}

/// [`record`], appending to `calls` the macro calls the record names: the image's reads
/// cover what their expansions read ([`super::Reads`]).
fn record_naming<S: Sig>(
    db: &Db,
    krate: Crate,
    what: u8,
    of: &S::Of,
    calls: &mut Vec<MacroCallId>,
) -> R<Portable> {
    let mut table = Relocation::new();
    let store = S::store(of).map(|store| {
        let written = relocating(&mut table, || SessionBytes::of(store));
        table::relocate_store(written, &mut table)
    });
    let mut words = Words::new();
    relocating(&mut table, || S::put(of, &mut words));
    table::calls(&table, calls);
    let table = table::encode(db, krate, &table)?;

    let mut out = Writer::new(RECORD_SCHEMA);
    let placed = words.place(&mut out);
    let table = Column::push(&mut out, &table);
    let store = store.map(|store| {
        // A store is a buffer of its own, read in place: at an aligned address.
        let at = out.bytes.len().next_multiple_of(ALIGN);
        out.bytes.resize(at, 0);
        Column::push(&mut out, store.as_bytes())
    });
    let at = out.next::<RecordRoot>();
    let root = RecordRoot {
        words: placed.rec(at + offset_of!(RecordRoot, words) as u64),
        table: table.slice(at + offset_of!(RecordRoot, table) as u64),
        store: store.map_or(Slice::EMPTY, |it| it.slice(at + offset_of!(RecordRoot, store) as u64)),
        what: u32::from(what),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    // The words were relocated, the table is keyed and the store went through
    // `relocate_store`: every id in the record is portable.
    Ok(Portable::written(out.finish(at)))
}

/// Why bytes do not read, for the panic of a corrupt image.
#[expect(
    clippy::needless_pass_by_value,
    reason = "the `map_err` callback of every read here: `Result::map_err` hands the error over by value"
)]
fn format_error(error: FormatError) -> String {
    // alloc: the reason of a panic.
    error.to_string()
}

/// The key of the record of `id`'s signature, the same in every process: what a value made
/// from the constant is filed by beside its crate's key (`hir_ty::consteval::deps`). `None`
/// for a constant in a block.
pub(crate) fn const_key(db: &Db, id: ConstId) -> Option<ContentId> {
    key::of(db, Item::Const(id)).map(|(_, key)| key)
}

/// The record of `item` in `part`, the signatures part of its crate's image: two plain columns
/// checked in place, a key hashed and a binary search, nothing allocated.
fn find<'a>(db: &Db, part: &'a [u8], item: Item) -> R<&'a [u8]> {
    let (_, key) = key::of(db, item).expect("an item outside a block has a key");
    let (at, places, _) = find_at(part, key)?;
    let Place { at, len } = places[at];
    let end = (at as usize).checked_add(len as usize);
    end.and_then(|end| part.get(at as usize..end))
        // alloc: the reason of a panic.
        .ok_or_else(|| "a record past the end of its signatures part".to_owned())
}

/// The index of the record under `key` in `part`, with the part's places and flags columns:
/// the part's three plain columns checked in place and one binary search of its keys.
fn find_at(part: &[u8], key: ContentId) -> R<(usize, &[Place], &[[u8; 16]])> {
    let root = open_as::<PartRoot>(part, SCHEMA).map_err(format_error)?;
    let keys = root.keys.follow(part).map_err(format_error)?;
    let places = root.places.follow(part).map_err(format_error)?;
    let flags = root.flags.follow(part).map_err(format_error)?;
    if keys.len() != places.len() || keys.len() != flags.len() {
        // alloc: the reason of a panic.
        return Err("its keys, records and flags are not one each".to_owned());
    }
    let Ok(at) = keys.binary_search(&key) else {
        // alloc: the reason of a panic.
        return Err("its signatures part has no record of the item".to_owned());
    };
    Ok((at, places, flags))
}

/// The value of `bytes`, the record of query `what` of an item of `krate`, with each id it
/// names made in this database. `bytes` are inside `image`, which a store read in place
/// borrows its columns from.
fn read<S: Sig>(
    db: &Db,
    krate: Crate,
    what: u8,
    image: &crate::deps::ImageBuf,
    bytes: &[u8],
) -> R<S::Value> {
    let rec = open_as::<RecordRoot>(bytes, RECORD_SCHEMA).map_err(format_error)?;
    if rec.what != u32::from(what) {
        // alloc: the reason of a panic.
        return Err(format!("a record of query {} where {what} was asked", rec.what));
    }
    let mut table = table::decode(db, krate, rec.table.follow(bytes).map_err(format_error)?)?;
    let store = match rec.store.is_empty() {
        true => None,
        false => {
            let store = rec.store.follow(bytes).map_err(format_error)?;
            Some(table::unrelocate_store(store, &table).map_err(format_error)?)
        }
    };
    let cfg = krate.cfg_options(db);
    relocating(&mut table, || {
        let store = match &store {
            Some(StoreBytes::InPlace(store)) => {
                Some(ExpressionStore::read(store)?.decode_in(image)?)
            }
            Some(StoreBytes::Moved(store)) => Some(ExpressionStore::read(store)?.decode()?),
            None => None,
        };
        S::get(&mut Read::new(&rec.words, bytes), store, cfg)
    })
    .map_err(format_error)
}

#[cold]
#[inline(never)]
fn corrupt(db: &Db, krate: Crate, item: Item, why: &str) -> ! {
    panic!(
        "the image of crate {} is corrupt: the signature of {item:?}: {why}; its bytes were \
         checked against their key, so an encoding wrote what it does not read or the key left \
         out an input the bytes depend on",
        krate.named(db)
    )
}

/// The value of `item`'s query read from its crate's image, `None` when its crate is read from
/// none or the item is in a block. `live` writes the record of the value lowered from syntax,
/// for [`Mode::Check`].
///
/// An image with no record of the item, or one that does not read, is a corrupt image: a panic
/// naming the crate, the item and why. Serving only gives a database images whose bytes the
/// store checked against their key, so lowering the item instead would hide a bug.
fn served<S: Sig>(db: &Db, item: Item, live: impl FnOnce(Crate) -> R<Portable>) -> Option<S::Value> {
    let module = item.module(db);
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    let image = super::image_of(db, krate)?;
    let what = item.what();
    let part = image.view().part(Part::Signatures);
    let stored = find(db, part, item).unwrap_or_else(|why| corrupt(db, krate, item, &why));
    let value = read::<S>(db, krate, what, image.bytes(), stored)
        .unwrap_or_else(|why| corrupt(db, krate, item, &why));
    // The value is the record's: the read is of the item's fact (its stable key and the
    // digest of its record), not of the image ([`super::ImageSlot`]'s fact says only the mode).
    let note = || {
        let (_, key) = key::of(db, item).expect("an item outside a block has a key");
        super::note_item_read(db, RECORD_FACT_DOMAIN, key, stored);
    };
    match image.mode() {
        Mode::Serve => note(),
        Mode::Check => {
            let written = live(krate);
            compare(db, krate, item, "the signature lowered from syntax", written, stored);
        }
        Mode::Verify => {
            note();
            let written = record::<S>(db, krate, what, S::of(&value));
            compare(db, krate, item, "what was read of it", written, stored);
        }
    }
    Some(value)
}

/// Records a difference between `written`, the record of `what` of `item`, and `record`, the
/// image's.
fn compare(db: &Db, krate: Crate, item: Item, what: &str, written: R<Portable>, record: &[u8]) {
    match written {
        Ok(written) if written.as_bytes() == record => {}
        Ok(written) => {
            // alloc: a difference, which is a bug.
            let what = format!("{what}, of {item:?}");
            frozen::record_difference(db, krate, "signatures", &what, written.as_bytes(), record);
        }
        Err(why) => {
            // alloc: a difference, which is a bug.
            let what = format!("{what}, of {item:?}, which does not write ({why})");
            frozen::record_difference(db, krate, "signatures", &what, &[], &[]);
        }
    }
}

macro_rules! served {
    ($($(#[$doc:meta])* $name:ident($id:ty) -> $sig:ident as $code:ident;)*) => {$(
        $(#[$doc])*
        ///
        /// Called inside the `served` query of the signature: the value is kept in that
        /// query's arena and dropped with it ([`blaze_memo::keep_owned`]: a decoded signature
        /// owns its expression store).
        pub(crate) fn $name(db: &Db, id: $id) -> Option<&$sig> {
            let item = Item::$code(id);
            let value = served::<codes::$code>(db, item, |krate| {
                record::<codes::$code>(db, krate, item.what(), &$sig::lower_alone(db, id))
            })?;
            Some(blaze_memo::keep_owned(blaze_memo::Database::memo(db), value))
        }
    )*};
}

served! {
    /// [`StructSignature::of`] from the image of its crate, if it has one.
    served_struct(StructId) -> StructSignature as Struct;
    /// [`UnionSignature::of`] from the image of its crate, if it has one.
    served_union(UnionId) -> UnionSignature as Union;
    /// [`EnumSignature::of`] from the image of its crate, if it has one.
    served_enum(EnumId) -> EnumSignature as Enum;
    /// [`ConstSignature::of`] from the image of its crate, if it has one.
    served_const(ConstId) -> ConstSignature as Const;
    /// [`StaticSignature::of`] from the image of its crate, if it has one.
    served_static(StaticId) -> StaticSignature as Static;
    /// [`ImplSignature::of`] from the image of its crate, if it has one.
    served_impl(ImplId) -> ImplSignature as Impl;
    /// [`TraitSignature::of`] from the image of its crate, if it has one.
    served_trait(TraitId) -> TraitSignature as Trait;
    /// [`FunctionSignature::of`] from the image of its crate, if it has one.
    served_function(FunctionId) -> FunctionSignature as Function;
    /// [`TypeAliasSignature::of`] from the image of its crate, if it has one.
    served_type_alias(TypeAliasId) -> TypeAliasSignature as TypeAlias;
    /// [`VariantFields::of`] from the image of its crate, if it has one.
    served_variant_fields(VariantId) -> VariantFields as Fields;
}

/// [`EnumVariants::of`] from the image of its crate, if it has one and its crate's def map is
/// read from the image's pages too.
///
/// A crate whose def map is resolved here ([`Mode::Check`], or an image without pages) reads
/// its enums' variants from syntax: name resolution asks them while the def map is being made
/// (`use E::*`, a path through a variant), and a record is found by its item's [`key`] and
/// decoded by the positions of modules in that same def map, so serving them would make the
/// crate's resolution ask for itself (`crate_resolution -> scc_state -> EnumVariants::of ->
/// key::module_indexes -> crate_resolution`). The cost: [`Mode::Check`] compares no variants
/// record, and says so by name ([`frozen::uncompared`]), never as an agreement; the variants'
/// ids are the same either way (interned by their syntax).
pub(crate) fn served_enum_variants(
    db: &Db,
    id: EnumId,
) -> Option<(EnumVariants, ThinVec<InactiveEnumVariantCode>)> {
    let item = Item::Variants(id);
    let krate = item.module(db).krate(db);
    if !crate::nameres::frozen::pages::is_paged(db, krate) {
        if super::mode(db, krate) == Some(Mode::Check) {
            frozen::record_uncompared(
                db,
                krate,
                "enum variants",
                "the crate is resolved from source in Check mode, and a variants record is \
                 found by module positions in that same def map, so comparing it while \
                 resolving would make the resolution ask for itself",
            );
        }
        return None;
    }
    served::<codes::Variants>(db, item, |krate| {
        record::<codes::Variants>(db, krate, item.what(), &EnumVariants::lower(db, id))
    })
}

/// A test's planted control: the sections it names read as if the image did not hold them, so
/// the query takes the path of an image written without them (its guarded read of the item's
/// file). Set and cleared under the deps tests' turn lock; nothing outside tests has it.
pub(crate) mod left_out {
    #[cfg(test)]
    use std::sync::atomic::{AtomicU8, Ordering::SeqCst};

    /// The items of traits and impls, and with them their visibilities ([`super::AssocRecord`]).
    pub(crate) const ITEMS: u8 = 1;
    /// The flags column ([`super::served_attr_flags`]).
    pub(crate) const FLAGS: u8 = 2;
    /// The visibilities of an [`super::AssocRecord`] alone, its items still served.
    pub(crate) const VISIBILITIES: u8 = 4;

    #[cfg(test)]
    static LEFT_OUT: AtomicU8 = AtomicU8::new(0);

    /// While held, the sections [`leave_out`] named are left out; dropped, none is.
    #[cfg(test)]
    pub(crate) struct Restore(());

    #[cfg(test)]
    impl Drop for Restore {
        fn drop(&mut self) {
            LEFT_OUT.store(0, SeqCst);
        }
    }

    /// Leaves out `sections` until the guard drops.
    #[cfg(test)]
    #[must_use = "the sections are left out only while the guard is held"]
    pub(crate) fn leave_out(sections: u8) -> Restore {
        LEFT_OUT.store(sections, SeqCst);
        Restore(())
    }

    #[cfg(test)]
    pub(super) fn is(section: u8) -> bool {
        LEFT_OUT.load(SeqCst) & section != 0
    }
}

/// Whether a test left out `section` ([`left_out`]); never outside tests.
#[cfg(test)]
fn left_out(section: u8) -> bool {
    left_out::is(section)
}

#[cfg(not(test))]
fn left_out(_: u8) -> bool {
    false
}

/// The domain of the digest of an item's attribute flags: the fact a read of them lists, its id
/// the item's key under this domain (the item's signature record's own fact has the key itself).
const FLAGS_FACT_DOMAIN: &str = "blaze v2 dependency attr flags fact";

/// `AttrFlags::query` of `owner` from its crate's image: `None` when its crate is read from no
/// image's pages, the owner is in a block, or the part holds no flags of its kind
/// ([`flags_item`]: a module, a macro, an extern block, an `extern crate`, a `use`).
///
/// Served only for a crate read from pages, as [`served_enum_variants`] is: a crate resolved
/// here asks for its items' flags while its def map is being made, and finding a record's key
/// reads that def map's module positions ([`key::module_indexes`]).
///
/// Floor (rule 67): one mapped lookup, a binary search of the part's keys and 16 bytes read in
/// place; no record decoded and nothing of the crate's files. Cost, an estimate not measured:
/// 16 bytes a record of the part (a signature's holds its item's flags, any other 16 zero
/// bytes), so about 16 bytes an item and 16 more a struct, union or enum.
pub(crate) fn served_attr_flags(db: &Db, owner: AttrDefId) -> Option<AttrFlags> {
    let item = flags_item(owner)?;
    if left_out(left_out::FLAGS) {
        return None;
    }
    let module = item.module(db);
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    if !crate::nameres::frozen::pages::is_paged(db, krate) {
        return None;
    }
    let image = super::image_of(db, krate)?;
    let (_, key) = key::of(db, item)?;
    let part = image.view().part(Part::Signatures);
    let (at, _, flags) = find_at(part, key).unwrap_or_else(|why| corrupt(db, krate, item, &why));
    let bytes = &flags[at];
    let value = decode_flags(bytes).unwrap_or_else(|why| corrupt(db, krate, item, &why));
    let id = ContentId::of(FLAGS_FACT_DOMAIN, &key.0);
    super::note_item_read(db, FLAGS_FACT_DOMAIN, id, bytes);
    Some(value)
}

/// The [`AssocRecord`] of `container` (`item` is its [`Item::ImplItems`] or
/// [`Item::TraitItems`]) from its crate's image: `None` when its crate is read from no image's
/// pages or the container is in a block. Served only for a crate read from pages, as
/// [`served_enum_variants`] is (a crate resolved here can ask for a trait's items while its
/// def map is made: `use Trait::item`).
///
/// A record naming an item of another container is a corrupt image (a panic naming the crate,
/// the container and the item): bytes the store checked against their key, which a key that
/// misses an input would let be read for the wrong impl.
fn served_assoc(db: &Db, item: Item, container: ItemContainerId) -> Option<AssocRecord> {
    if left_out(left_out::ITEMS) {
        return None;
    }
    let module = item.module(db);
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    if !crate::nameres::frozen::pages::is_paged(db, krate) {
        if super::mode(db, krate) == Some(Mode::Check) {
            frozen::record_uncompared(
                db,
                krate,
                "assoc items",
                "the crate is resolved from source in Check mode, and an assoc items record is \
                 found by module positions in that same def map, so comparing it while \
                 resolving could make the resolution ask for itself",
            );
        }
        return None;
    }
    let value = served::<codes::Assoc>(db, item, |krate| {
        record::<codes::Assoc>(db, krate, item.what(), &assoc_record(db, container))
    })?;
    if value.is_trait != matches!(container, ItemContainerId::TraitId(_)) {
        corrupt(db, krate, item, "an assoc items record of the other kind of container");
    }
    let foreign = value.items.iter().find(|&&(_, it)| assoc_container(db, it) != container);
    if let Some((name, it)) = foreign {
        // alloc: the reason of a panic.
        let why =
            format!("it names `{}` ({it:?}), an item of another container", name.symbol().as_str());
        corrupt(db, krate, item, &why);
    }
    Some(value)
}

/// `ImplItems::of` of `id` from its crate's image ([`served_assoc`]), its visibilities with it.
pub(crate) fn served_impl_items(db: &Db, id: ImplId) -> Option<AssocRecord> {
    served_assoc(db, Item::ImplItems(id), ItemContainerId::ImplId(id))
}

/// `TraitItems::query_with_diagnostics` of `id` from its crate's image ([`served_assoc`]), the
/// trait's visibility with it.
pub(crate) fn served_trait_items(db: &Db, id: TraitId) -> Option<AssocRecord> {
    served_assoc(db, Item::TraitItems(id), ItemContainerId::TraitId(id))
}

/// The visibility written on each item of `id`, at its index in `ImplItems::of(id)`'s items,
/// from its crate's image: `None` where [`served_impl_items`] is. Read once an impl, by what
/// asks an item's visibility (`AssocItemId::assoc_visibility`).
#[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
pub(crate) fn served_impl_visibilities(db: &Db, id: ImplId) -> Option<Box<[RawVisibility]>> {
    if left_out(left_out::VISIBILITIES) {
        return None;
    }
    served_impl_items(db, id).map(|it| it.visibilities)
}

/// The visibility written on the trait `id` (each of its items has it), from its crate's image:
/// `None` where [`served_trait_items`] is.
#[blaze_memo::tracked(returns(ref), owns, mem_tag = Nameres)]
pub(crate) fn served_trait_visibility(db: &Db, id: TraitId) -> Option<RawVisibility> {
    if left_out(left_out::VISIBILITIES) {
        return None;
    }
    // A trait's record holds one visibility (`codes::Assoc` checks it).
    served_trait_items(db, id).and_then(|it| it.visibilities.into_vec().pop())
}
