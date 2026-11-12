//! The impls of a crate kept as a record and read back instead of found (frozen dependency
//! artifacts, blaze's `docs/v2/plan.md`, item 1; docs/v2/plan/design/restore.md, "Impl tables
//! read in place"), with `hir_def`'s frozen encoding.
//!
//! The record is [`pages`]: a crate's trait impls, inherent impls and its traits' value items
//! as tables a lookup reads one entry of. This file is what has a crate read from one: the
//! per-crate input ([`Tables`], [`ImplTablesSlot`]), the key the record is kept under
//! ([`key`]), the serve and the store ([`serve`], [`store`]), and how a simplified self type is
//! written. As with a frozen `DefMap`, what is read must equal what finding the impls gives:
//! [`Mode::Check`] finds them and [`check`] records a difference, which is a bug.

use std::sync::Arc as StdArc;

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    ModuleDefId,
    deps::{DepImages, ImageStore, PageSource, Paged, Served},
    nameres::frozen::{Dec, Enc, Mode, record_difference},
};
use rustc_type_ir::{FloatTy, IntTy, Mutability, UintTy, fast_reject::SimplifiedType as S};
use std::alloc::Global;

use crate::next_solver::{SimplifiedType, SolverDefId};

pub mod pages;

pub use pages::encode;
pub(super) use pages::{
    all_inherent_into, all_traits_into, all_value_items, blanket, inherent, row, trait_into,
    value_items,
};

type R<T = ()> = Result<T, String>;

/// What a crate's impls are read from: the pages of its record ([`pages`]), read a part at a
/// time through their source, and what is done with them. Equal when the keys are: the store
/// gives the bytes of a key only after checking them, so equal keys are equal bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tables {
    pub pages: Paged,
    pub mode: Mode,
}

/// A record held whole in memory: a test's, and what [`Tables::whole`] makes.
struct Whole(StdArc<[u8]>);

impl PageSource for Whole {
    fn size(&self) -> u64 {
        self.0.len() as u64
    }

    // dyn: as `PageSource::read`
    fn read(&self, at: u64, len: u64, with: &mut dyn FnMut(&[u8])) -> Result<(), String> {
        let range = usize::try_from(at).ok().zip(usize::try_from(len).ok());
        let part = range.and_then(|(at, len)| self.0.get(at..at.checked_add(len)?));
        // alloc: error path
        let part = part.ok_or_else(|| format!("bytes {at}+{len} of {}", self.0.len()))?;
        with(part);
        Ok(())
    }
}

impl Tables {
    /// Tables read from `bytes` ([`encode`]) held whole in memory, keyed by their content:
    /// for a caller with the bytes and no store (a test, a check of the encoding).
    pub fn whole(bytes: &[u8], mode: Mode) -> Tables {
        let mut hasher = Hasher::new(KEY_DOMAIN);
        hasher.part(bytes);
        // alloc: the record held whole, once per crate so given
        let source: StdArc<dyn PageSource> = StdArc::new(Whole(StdArc::from(bytes)));
        Tables { pages: Paged { key: hasher.finish(), source }, mode }
    }
}

/// The crates whose impls are read from tables, set by whoever has the records: read where
/// every crate is gone over ([`serve`], [`withdraw_unimaged`]). A crate's own tables are read
/// from its slot ([`ImplTablesSlot`], `hir_def::deps::given`), so tables given to one crate
/// are no write a reader of another crate's read.
#[blaze_query::input(singleton, debug)]
pub struct FrozenImpls {
    #[returns(ref)]
    pub maps: StdArc<HashMap<Crate, Tables, Global>>,
}

/// Has finding the impls of a crate use `maps`.
pub fn set(db: &mut Db, maps: HashMap<Crate, Tables, Global>) {
    use blaze_query::db::{Durability, Setter};
    // clone: the handle of the map set before, to tell which crates' tables move
    let was = FrozenImpls::try_get(&*db).map(|it| it.maps(&*db).clone());
    hir_def::deps::given::give_changed::<ImplTablesSlot, Tables>(db, was.as_deref(), &maps);
    let maps = StdArc::new(maps);
    match FrozenImpls::try_get(db) {
        Some(it) => {
            it.set_maps(db).with_durability(Durability::HIGH).to(maps);
        }
        None => {
            _ = FrozenImpls::new(db, Durability::HIGH, maps);
        }
    }
}

/// The kind of a crate's slot holding its entry of [`FrozenImpls`].
pub struct ImplTablesSlot;

/// The tables `krate` was given, from its own slot: the one read of them inside a query.
fn tables(db: &Db, krate: Crate) -> Option<&Tables> {
    krate.given::<ImplTablesSlot, Tables>(db)
}

/// Whether the impls of `krate` are read from its tables rather than found.
pub fn served(db: &Db, krate: Crate) -> bool {
    tables(db, krate).is_some_and(|it| it.mode != Mode::Check)
}

// Name resolution hands the impls maintainer no delta of a crate whose impls are read here.
inventory::submit! { hir_def::nameres::delta::ImplsServed(served) }

/// The version of how a simplified self type is written ([`write_simplified`]), for the key
/// the record is stored under ([`key`]) and the record's header: it changes with every change
/// to it, so bytes of another version are never asked for.
pub const ENCODING: &str = "blaze frozen impls 2";

/// The domain of the key a crate's impl tables are kept under ([`key`]).
pub const KEY_DOMAIN: &str = "blaze v2 impl tables";

/// Where the impl tables of the crate whose image is filed under `filed` are kept: the
/// image's key (`hir_def::deps::Image::key`, which covers every input of the crate and of the
/// crates it depends on, and what it read), the layout of the pages ([`pages::LAYOUT`]), this
/// encoding and the one its ids are written in (`hir_def::nameres::frozen::ENCODING`), so
/// pages of another layout are another key and are never asked for. A crate's impls, their
/// traits and their self types' heads are made from its own items and the items of crates it
/// depends on, all under that key.
pub fn key(filed: ContentId) -> ContentId {
    let mut hasher = Hasher::new(KEY_DOMAIN);
    hasher
        .id(&filed)
        .update(&pages::LAYOUT.to_le_bytes())
        .part(ENCODING.as_bytes())
        .part(hir_def::nameres::frozen::ENCODING.as_bytes());
    hasher.finish()
}

/// Gives `db` the impl tables `images` keeps of each crate `served` keyed that is read from
/// its image in [`Mode::Serve`], and takes away those of a crate `served` keyed that is not
/// (its image went, or its tables were never written): called in the write of the serve
/// (`hir_def::deps::serve_asked`), so the pass of that write finds each crate with tables
/// served and the impls maintainer never keys an impl of it. A crate `served` did not key is
/// left as it is. Gives the crates whose tables came or went, sorted; nothing is written for
/// none. The caller notes a change of the crate graph when one of them was maintained before
/// (the impls maintainer then drops what it held of it).
///
/// A crate with an image and no tables in the store is maintained from its decoded def map,
/// as before tables were kept, and [`store`] then writes them. Tables that are found and do
/// not read stop the query that reads them, naming the crate; nothing is found in their place.
pub fn serve(
    db: &mut Db,
    served: &Served,
    // dyn: the driver's store, one call per crate per serve
    images: &dyn ImageStore,
    mode: Mode,
) -> R<Vec<Crate>> {
    // clone: what was served before, once per serve, to add to
    let mut maps =
        FrozenImpls::try_get(&*db).map(|it| (**it.maps(&*db)).clone()).unwrap_or_default();
    // In the crates' order: the store is asked in the same order by every database.
    let mut asked: Vec<Crate> = served.keys.keys().copied().collect(); // alloc: once per serve
    asked.sort_unstable();
    let mut changed = Vec::new(); // alloc: none until a crate's tables change
    for krate in asked {
        // clone: two buffer handles and a few words, once per crate asked
        let image = DepImages::try_get(&*db).and_then(|it| it.images(&*db).get(&krate).cloned());
        let found = match image {
            Some(image) if mode == Mode::Serve && image.mode() == Mode::Serve => {
                let at = key(image.key());
                // The record's pages, read a part at a time: nothing of it is held here.
                let source = images.pages(&at).map_err(|why| {
                    // alloc: error text
                    format!("reading the impl tables of crate {} (key {at:?}): {why}", krate.named(&*db))
                })?;
                source.map(|source| Tables { pages: Paged { key: at, source }, mode: Mode::Serve })
            }
            _ => None,
        };
        if maps.get(&krate) == found.as_ref() {
            continue;
        }
        changed.push(krate);
        match found {
            Some(it) => {
                maps.insert(krate, it);
            }
            None => {
                maps.remove(&krate);
            }
        }
    }
    if !changed.is_empty() {
        set(db, maps);
    }
    Ok(changed)
}

/// Writes to `images` the impl tables of each of `crates`, a crate with the key its image is
/// filed under, that is not read from tables already: what the next database's [`serve`]
/// finds. Called once the crates are resolved and checked, where their images are written.
/// Gives the crates whose tables do not encode, with why: such a crate has no tables in the
/// store and is maintained by every database, which the caller says.
pub fn store(
    db: &Db,
    crates: &[(Crate, ContentId)],
    // dyn: the driver's store, one call per crate per front end stored
    images: &dyn ImageStore,
) -> R<Vec<(Crate, String)>> {
    let mut refused = Vec::new(); // alloc: none until a crate is refused
    for &(krate, filed) in crates {
        if served(db, krate) {
            continue;
        }
        match encode(db, krate) {
            Ok(bytes) => images.put(key(filed), bytes).map_err(|why| {
                // alloc: error text
                format!("keeping the impl tables of crate {}: {why}", krate.named(db))
            })?,
            Err(why) => refused.push((krate, why)),
        }
    }
    Ok(refused)
}

inventory::submit! {
    base_db::Implied { name: "dependency impl tables withdrawn", write: withdraw_unimaged }
}

#[cfg(test)]
thread_local! {
    /// The bug a test planted: a crate that loses its image keeps its impl tables.
    pub(crate) static KEEPS_TABLES_PAST_IMAGE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The write implied by a change to the files of crates read from images, after
/// `hir_def::deps`' own (implied writes run in the order of their names: "dependency images
/// withdrawn" is before this one): a crate that lost its image in this write loses its impl
/// tables in it too, so the pass that resolves it again maintains its impls again. The tables'
/// key is the image's, so tables kept past their image would be read against files they were
/// not made from.
fn withdraw_unimaged(db: &mut Db, written: &base_db::Written<'_>) {
    #[cfg(test)]
    if KEEPS_TABLES_PAST_IMAGE.get() {
        return;
    }
    if written.set.is_empty()
        && written.added.is_empty()
        && written.removed.is_empty()
        && written.roots.is_empty()
    {
        return;
    }
    let Some(input) = FrozenImpls::try_get(&*db) else { return };
    let maps = input.maps(&*db);
    if maps.is_empty() {
        return;
    }
    let images = DepImages::try_get(&*db).map(|it| it.images(&*db));
    let imaged = |krate: &Crate| images.is_some_and(|it| it.contains_key(krate));
    if maps.keys().all(imaged) {
        return;
    }
    // clone: the tables' handles, once per write that withdraws an image
    let mut kept = (**maps).clone();
    kept.retain(|krate, _| imaged(krate));
    set(db, kept);
}

/// Records a difference between the pages of `krate` and the live impls in [`Mode::Check`],
/// or what was read of them in [`Mode::Verify`]: what [`encode`] writes now against the
/// record, read whole for this.
pub fn check(db: &Db, krate: Crate) {
    let Some(tables) = tables(db, krate) else { return };
    let what = match tables.mode {
        Mode::Serve => return,
        Mode::Check => "the live ones",
        Mode::Verify => "what was read of them",
    };
    let source = &*tables.pages.source;
    // alloc: Check/Verify only, which record differences; never when serving
    let mut kept: Vec<u8> = Vec::new();
    let read = source.read(0, source.size(), &mut |bytes| kept.extend_from_slice(bytes));
    match (read, encode(db, krate)) {
        (Ok(()), Ok(written)) if written == kept => {}
        (Ok(()), Ok(written)) => record_difference(db, krate, "impls", what, &written, &kept),
        (Err(why), _) | (_, Err(why)) => {
            // alloc: Check/Verify only, as above
            let what = format!("{what}, which cannot be compared ({why})");
            record_difference(db, krate, "impls", &what, &[], &[])
        }
    }
}

const INTS: [IntTy; 6] = [IntTy::Isize, IntTy::I8, IntTy::I16, IntTy::I32, IntTy::I64, IntTy::I128];
const UINTS: [UintTy; 6] =
    [UintTy::Usize, UintTy::U8, UintTy::U16, UintTy::U32, UintTy::U64, UintTy::U128];
const FLOATS: [FloatTy; 4] = [FloatTy::F16, FloatTy::F32, FloatTy::F64, FloatTy::F128];

/// The kind [`write_simplified`] writes `it` as, and the number it writes after it where the
/// kind has one (0 where it has none, or names an item instead): what a table's slot hashes
/// ([`pages`]), the same in every database. `None` for a type [`write_simplified`] refuses.
pub(super) fn simplified_number(it: &SimplifiedType<'_>) -> Option<(u8, u64)> {
    Some(match *it {
        S::Bool => (0, 0),
        S::Char => (1, 0),
        // As `write_simplified` numbers them: a standard type by its index, an arbitrary
        // width by its `code()`.
        S::Int(it) => match it.arbitrary_width() {
            Some(_) => (2, u64::from(it.code())),
            None => (2, INTS.iter().position(|x| *x == it).unwrap() as u64),
        },
        S::Uint(it) => match it.arbitrary_width() {
            Some(_) => (3, u64::from(it.code())),
            None => (3, UINTS.iter().position(|x| *x == it).unwrap() as u64),
        },
        S::Float(it) => (4, FLOATS.iter().position(|x| *x == it).unwrap() as u64),
        S::Adt(_) => (5, 0),
        S::Foreign(_) => (6, 0),
        S::Str => (7, 0),
        S::Array => (8, 0),
        S::Slice => (9, 0),
        S::Ref(it) => (10, u64::from(mutability(it))),
        S::Ptr(it) => (11, u64::from(mutability(it))),
        S::Never => (12, 0),
        S::Tuple(n) => (13, n as u64),
        S::MarkerTraitObject => (14, 0),
        S::Trait(_) => (15, 0),
        S::Function(n) => (16, n as u64),
        S::UnsafeBinder => (17, 0),
        S::Placeholder => (18, 0),
        S::Error => (19, 0),
        S::Closure(_) | S::Coroutine(_) | S::CoroutineWitness(_) => return None,
    })
}

fn write_def(enc: &mut Enc<'_>, it: &SolverDefId<'_>) -> R {
    match *it {
        SolverDefId::AdtId(it) => {
            enc.u8(0);
            enc.adt(it)
        }
        SolverDefId::TraitId(it) => {
            enc.u8(1);
            enc.def(it.into())
        }
        SolverDefId::TypeAliasId(it) => {
            enc.u8(2);
            enc.def(it.into())
        }
        // alloc: the error of impls that cannot be written, which keeps them unfrozen
        _ => Err("a type named by neither an ADT, a trait nor a type alias".to_owned()),
    }
}

fn read_def<'db>(dec: &mut Dec<'_>) -> R<SolverDefId<'db>> {
    Ok(match dec.u8()? {
        0 => SolverDefId::AdtId(dec.adt()?),
        _ => match dec.def()? {
            ModuleDefId::TraitId(it) => SolverDefId::TraitId(it),
            ModuleDefId::TypeAliasId(it) => SolverDefId::TypeAliasId(it),
            // alloc: the error on corrupt bytes.
            _ => return Err("a type named by an item that names none".to_owned()),
        },
    })
}

fn mutability(it: Mutability) -> u8 {
    match it {
        Mutability::Not => 0,
        Mutability::Mut => 1,
    }
}

fn read_mutability(dec: &mut Dec<'_>) -> R<Mutability> {
    Ok(if dec.u8()? == 0 { Mutability::Not } else { Mutability::Mut })
}

pub(super) fn write_simplified(enc: &mut Enc<'_>, it: &SimplifiedType<'_>) -> R {
    fn index<T: PartialEq + Copy>(all: &[T], it: T) -> u32 {
        all.iter().position(|x| *x == it).unwrap() as u32
    }
    match *it {
        S::Bool => enc.u8(0),
        S::Char => enc.u8(1),
        // blaze: an arbitrary width (`#![blaze::arbitrary_ints]`, `impl u24 { .. }` in core) is
        // written as its `code()`, 128 plus its width, past the standard types' indices, so their
        // bytes are unchanged.
        S::Int(it) => {
            enc.u8(2);
            enc.u32(match it.arbitrary_width() {
                Some(_) => u32::from(it.code()),
                None => index(&INTS, it),
            });
        }
        S::Uint(it) => {
            enc.u8(3);
            enc.u32(match it.arbitrary_width() {
                Some(_) => u32::from(it.code()),
                None => index(&UINTS, it),
            });
        }
        S::Float(it) => {
            enc.u8(4);
            enc.u32(index(&FLOATS, it));
        }
        S::Adt(ref def) => {
            enc.u8(5);
            write_def(enc, def)?;
        }
        S::Foreign(ref def) => {
            enc.u8(6);
            write_def(enc, def)?;
        }
        S::Str => enc.u8(7),
        S::Array => enc.u8(8),
        S::Slice => enc.u8(9),
        S::Ref(it) => {
            enc.u8(10);
            enc.u8(mutability(it));
        }
        S::Ptr(it) => {
            enc.u8(11);
            enc.u8(mutability(it));
        }
        S::Never => enc.u8(12),
        S::Tuple(n) => {
            enc.u8(13);
            enc.usize(n)?;
        }
        S::MarkerTraitObject => enc.u8(14),
        S::Trait(ref def) => {
            enc.u8(15);
            write_def(enc, def)?;
        }
        S::Function(n) => {
            enc.u8(16);
            enc.usize(n)?;
        }
        S::UnsafeBinder => enc.u8(17),
        S::Placeholder => enc.u8(18),
        S::Error => enc.u8(19),
        S::Closure(_) | S::Coroutine(_) | S::CoroutineWitness(_) => {
            // alloc: the error of impls that cannot be written, which keeps them unfrozen
            return Err("an impl for a closure or a coroutine".to_owned());
        }
    }
    Ok(())
}

/// An integer type [`write_simplified`] wrote: an index into `standard`, or the `code()` of an
/// arbitrary width.
fn read_int<T: Copy, W>(
    dec: &mut Dec<'_>,
    standard: &[T],
    from_code: fn(u8) -> Option<T>,
    arbitrary_width: fn(T) -> Option<W>,
) -> R<T> {
    let at = dec.u32()?;
    if let Some(&it) = standard.get(at as usize) {
        return Ok(it);
    }
    u8::try_from(at)
        .ok()
        .and_then(from_code)
        .filter(|&it| arbitrary_width(it).is_some())
        // alloc: the error on corrupt bytes.
        .ok_or_else(|| format!("integer type {at}"))
}

pub(super) fn read_simplified<'db>(dec: &mut Dec<'_>) -> R<SimplifiedType<'db>> {
    Ok(match dec.u8()? {
        0 => S::Bool,
        1 => S::Char,
        2 => S::Int(read_int(dec, &INTS, IntTy::from_code, IntTy::arbitrary_width)?),
        3 => S::Uint(read_int(dec, &UINTS, UintTy::from_code, UintTy::arbitrary_width)?),
        4 => S::Float(dec.index(&FLOATS)?),
        5 => S::Adt(read_def(dec)?),
        6 => S::Foreign(read_def(dec)?),
        7 => S::Str,
        8 => S::Array,
        9 => S::Slice,
        10 => S::Ref(read_mutability(dec)?),
        11 => S::Ptr(read_mutability(dec)?),
        12 => S::Never,
        13 => S::Tuple(dec.len()?),
        14 => S::MarkerTraitObject,
        15 => S::Trait(read_def(dec)?),
        16 => S::Function(dec.len()?),
        17 => S::UnsafeBinder,
        18 => S::Placeholder,
        19 => S::Error,
        it => return Err(format!("simplified type {it}")), // alloc: the error on corrupt bytes.
    })
}
