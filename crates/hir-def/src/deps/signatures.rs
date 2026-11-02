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
mod key;
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
use hir_expand::{MacroCallId, name::Name};
use thin_vec::ThinVec;
use triomphe::Arc;

use self::{codes::Sig, key::Item, table::StoreBytes};
use crate::{
    AdtId, AssocItemId, ConstId, EnumId, FunctionId, ImplId, ModuleDefId, StaticId, StructId,
    TraitId, TypeAliasId, UnionId, VariantId,
    deps::{Part, serve::crate_name},
    expr_store::ExpressionStore,
    hir::persist::{Column, Read, Relocation, Words, WordsRec, relocating},
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
    /// The part: each item's key, in increasing order, and where its record is, at the same
    /// index.
    #[derive(Clone, Copy)]
    struct PartRoot: 1 {
        keys: Slice<ContentId>,
        places: Slice<Place>,
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
    assert!(size_of::<PartRoot>() == 16);
    assert!(size_of::<RecordRoot>() == 36);
};

/// The schema of the part, and the version of everything its bytes are made with: the words of
/// each signature, the store's buffer, the item tree's codes (visibilities, shapes), the key and
/// the frozen encoding of the table.
pub(crate) const SCHEMA: SchemaHash = blaze_format::schema_of(&[
    SchemaPart::Str("blaze v2 dependency signatures 4"),
    SchemaPart::Schema(<PartRoot as Record>::SCHEMA),
    SchemaPart::Schema(<RecordRoot as Record>::SCHEMA),
    SchemaPart::Schema(<ExpressionStore as Persist>::SCHEMA),
    SchemaPart::Schema(<ItemTree as Persist>::SCHEMA),
    SchemaPart::Str(key::KEY_DOMAIN),
    SchemaPart::Str(frozen::ENCODING),
]);

/// The schema of one record's buffer.
const RECORD_SCHEMA: SchemaHash = blaze_format::schema_of(&[
    SchemaPart::Str("blaze v2 dependency signature record 1"),
    SchemaPart::Schema(<RecordRoot as Record>::SCHEMA),
]);

/// The signatures part of the image of `krate`: a record of every signature query of every item
/// of its modules. An item that cannot be written portably fails the whole part, with why.
/// Appends to `calls` each macro call a record names.
pub(crate) fn encode(db: &Db, krate: Crate, calls: &mut Vec<MacroCallId>) -> R<Vec<u8>> {
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
    // alloc: the error that fails the part.
    let place = |it: usize| u32::try_from(it).map_err(|_| "a part of 4 GiB or more".to_owned());
    for &(_, item) in &keyed {
        let record = record_of(db, krate, item, calls)
            // alloc: the error that fails the part.
            .map_err(|why| format!("the signature of {item:?}: {why}"))?;
        let at = out.bytes.len().next_multiple_of(ALIGN);
        out.bytes.resize(at, 0);
        out.bytes.extend_from_slice(&record);
        places.push(Place { at: place(at)?, len: place(record.len())? });
    }
    // alloc: the key column, written once per crate.
    let keys: Vec<ContentId> = keyed.iter().map(|&(key, _)| key).collect();
    let keys = Column::push(&mut out, &keys);
    let places = Column::push(&mut out, &places);
    let at = out.next::<PartRoot>();
    let root = PartRoot {
        keys: keys.slice(at + offset_of!(PartRoot, keys) as u64),
        places: places.slice(at + offset_of!(PartRoot, places) as u64),
    };
    let written = out.push(&root);
    debug_assert_eq!(written, at, "`Writer::next` is not where `push` writes");
    Ok(out.finish(at))
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
            ModuleDefId::TraitId(it) => {
                items.push(Item::Trait(it));
                assoc(items, &TraitItems::query(db, it).items);
            }
            ModuleDefId::TypeAliasId(it) => items.push(Item::TypeAlias(it)),
            ModuleDefId::ModuleId(_)
            | ModuleDefId::EnumVariantId(_)
            | ModuleDefId::BuiltinType(_)
            | ModuleDefId::MacroId(_) => {}
        }
    }
    items.extend(scope.unnamed_consts().map(Item::Const));
    for it in scope.impls() {
        items.push(Item::Impl(it));
        assoc(items, &ImplItems::of(db, it).0.items);
    }
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
        Item::Variants(_) => return None,
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
fn record_of(db: &Db, krate: Crate, item: Item, calls: &mut Vec<MacroCallId>) -> R<Vec<u8>> {
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
    }
}

/// The record of `of`, the value of query `what` of an item of `krate`. The same value is the
/// same bytes in every process: the table lists the ids in the order they are first written,
/// the store first.
fn record<S: Sig>(db: &Db, krate: Crate, what: u8, of: &S::Of) -> R<Vec<u8>> {
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
) -> R<Vec<u8>> {
    let mut table = Relocation::new();
    let mut store =
        S::store(of).map(|store| relocating(&mut table, || blaze_format::to_bytes(store)));
    if let Some(store) = &mut store {
        table::relocate_store(store, &mut table);
    }
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
        Column::push(&mut out, &store)
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
    Ok(out.finish(at))
}

/// Why bytes do not read, for the panic of a corrupt image.
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
    let root = open_as::<PartRoot>(part, SCHEMA).map_err(format_error)?;
    let keys = root.keys.follow(part).map_err(format_error)?;
    let places = root.places.follow(part).map_err(format_error)?;
    if keys.len() != places.len() {
        // alloc: the reason of a panic.
        return Err("its keys and records are not one each".to_owned());
    }
    let (_, key) = key::of(db, item).expect("an item outside a block has a key");
    let Ok(at) = keys.binary_search(&key) else {
        // alloc: the reason of a panic.
        return Err("its signatures part has no record of the item".to_owned());
    };
    let Place { at, len } = places[at];
    let end = (at as usize).checked_add(len as usize);
    end.and_then(|end| part.get(at as usize..end))
        // alloc: the reason of a panic.
        .ok_or_else(|| "a record past the end of its signatures part".to_owned())
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
         checked against their key, so an encoding wrote what it does not read",
        crate_name(db, krate)
    )
}

/// The value of `item`'s query read from its crate's image, `None` when its crate is read from
/// none or the item is in a block. `live` writes the record of the value lowered from syntax,
/// for [`Mode::Check`].
///
/// An image with no record of the item, or one that does not read, is a corrupt image: a panic
/// naming the crate, the item and why. Serving only gives a database images whose bytes the
/// store checked against their key, so lowering the item instead would hide a bug.
fn served<S: Sig>(db: &Db, item: Item, live: impl FnOnce(Crate) -> R<Vec<u8>>) -> Option<S::Value> {
    let module = item.module(db);
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    let image = super::image_of(db, krate).as_ref()?;
    let what = item.what();
    let part = image.view().part(Part::Signatures);
    let stored = find(db, part, item).unwrap_or_else(|why| corrupt(db, krate, item, &why));
    let value = read::<S>(db, krate, what, image.bytes(), stored)
        .unwrap_or_else(|why| corrupt(db, krate, item, &why));
    match image.mode() {
        Mode::Serve => {}
        Mode::Check => {
            let written = live(krate);
            compare(db, krate, item, "the signature lowered from syntax", written, stored);
        }
        Mode::Verify => {
            let written = record::<S>(db, krate, what, S::of(&value));
            compare(db, krate, item, "what was read of it", written, stored);
        }
    }
    Some(value)
}

/// Records a difference between `written`, the record of `what` of `item`, and `record`, the
/// image's.
fn compare(db: &Db, krate: Crate, item: Item, what: &str, written: R<Vec<u8>>, record: &[u8]) {
    match written {
        Ok(written) if written == record => {}
        Ok(written) => {
            // alloc: a difference, which is a bug.
            let what = format!("{what}, of {item:?}");
            frozen::record_difference(db, krate, "signatures", &what, &written, record);
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
        pub(crate) fn $name(db: &Db, id: $id) -> Option<Arc<$sig>> {
            let item = Item::$code(id);
            served::<codes::$code>(db, item, |krate| {
                record::<codes::$code>(db, krate, item.what(), &$sig::lower_alone(db, id))
            })
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

/// [`EnumVariants::of`] from the image of its crate, if it has one.
pub(crate) fn served_enum_variants(
    db: &Db,
    id: EnumId,
) -> Option<(EnumVariants, ThinVec<InactiveEnumVariantCode>)> {
    let item = Item::Variants(id);
    served::<codes::Variants>(db, item, |krate| {
        record::<codes::Variants>(db, krate, item.what(), &EnumVariants::lower(db, id))
    })
}
