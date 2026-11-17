//! The impls of a crate kept as a record and read back instead of found (frozen dependency
//! artifacts, blaze's `docs/v2/plan.md`, item 1; docs/v2/plan/design/restore.md, "Impl tables
//! read in place"), with `hir_def`'s frozen encoding.
//!
//! The record is [`pages`]: a crate's trait impls, inherent impls and its traits' value items
//! as tables a lookup reads one entry of. This file is what has a crate read from one: the
//! per-crate input ([`Tables`], [`ImplTablesSlot`]), the key the record is kept under
//! ([`key`]), the driver's lookup, slot and store ([`find`], [`give`], [`store`]), and how a
//! simplified self type is written. As with a frozen `DefMap`, what is read must equal what finding the impls gives:
//! [`Mode::Check`] finds them and [`check`] records a difference, which is a bug.

use std::sync::Arc as StdArc;

use base_db::{Crate, Db};
use blaze_hash::HashMap;
use blaze_ids::{ContentId, Hasher};
use hir_def::{
    ModuleDefId,
    deps::{Image, ImageStore, PageSource, Paged},
    nameres::frozen::{Dec, Enc, Mode, record_difference},
};
use rustc_type_ir::{FloatTy, IntTy, Mutability, UintTy, fast_reject::SimplifiedType as S};
use std::alloc::Global;

use crate::next_solver::{SimplifiedType, SolverDefId};

pub mod pages;

pub use pages::encode;
pub(super) use pages::{
    all_inherent_into, all_traits_into, all_value_items, blanket, for_self_ty, inherent, row,
    trait_into, value_items,
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

/// The kind of a crate's slot holding the impl tables it is read from ([`give`],
/// `hir_def::deps::given`): a crate's own slot, so tables given to one crate are no input a
/// reader of another crate's reads.
pub struct ImplTablesSlot;

/// The fact a crate's impl tables slot is read by: the slot's id, of the crate's own stable id
/// `krate_id`, and as its digest the record's key and what is done with it (equal keys are
/// equal bytes; [`served`] reads the mode).
pub fn tables_fact(krate_id: ContentId, tables: &Tables) -> blaze_memo::Fact {
    let mut hasher = Hasher::new("hir-ty impl tables slot digest");
    hasher.id(&tables.pages.key).update(&[tables.mode as u8]);
    blaze_memo::Fact {
        id: ContentId::of("hir-ty impl tables slot", &krate_id.0),
        digest: hasher.finish(),
    }
}

/// Has finding the impls of each crate of `entries` read its tables, from the snapshot `edit`
/// commits: each crate is given its tables in its own slot ([`ImplTablesSlot`]), read by the
/// fact given with them ([`tables_fact`]). Called by the driver in the edit that commits the
/// crates' images (`hir_def::deps::give_image`), so tables are never read against files they
/// were not made from. A crate given equal tables is not given them again. How many crates
/// were given.
pub fn give(
    edit: &mut base_db::Edit<'_>,
    entries: impl IntoIterator<Item = (Crate, blaze_memo::Fact, Tables)>,
) -> usize {
    hir_def::deps::given::give_each::<ImplTablesSlot, Tables>(edit, entries)
}

/// A test's [`give`] of `maps`, committed, each slot named by its record's key in place of the
/// crate's stable id (a test's crates have none). The commit drops each given crate's arena:
/// the caller takes its crates again by key after it.
pub fn give_for_test(db: &mut Db, maps: HashMap<Crate, Tables, Global>) {
    let mut edit = db.edit();
    give(
        &mut edit,
        maps.into_iter().map(|(krate, tables)| {
            (krate, tables_fact(tables.pages.key, &tables), tables)
        }),
    );
    _ = edit.commit();
}

/// The tables `krate` was given, from its own slot: the one read of them inside a query.
fn tables(db: &Db, krate: Crate) -> Option<&Tables> {
    krate.given::<ImplTablesSlot, Tables>(db)
}

/// Whether the impls of `krate` are read from its tables rather than found.
pub fn served(db: &Db, krate: Crate) -> bool {
    tables(db, krate).is_some_and(|it| it.mode != Mode::Check)
}

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

/// The impl tables `images` keeps of `krate`, the crate the driver serves from `image`: the
/// driver's lookup while it builds a snapshot, whose result it gives ([`give`]). `None` where
/// the image is not read in [`Mode::Serve`] or no tables were written of it; such a crate's
/// impls are found from its decoded def map, as before tables were kept, and [`store`] then
/// writes them. `db` names the crate in an error alone.
///
/// Tables that are found and do not read stop the query that reads them, naming the crate;
/// nothing is found in their place.
pub fn find(
    db: &Db,
    krate: Crate,
    image: &Image,
    // dyn: the driver's store, one call per crate per snapshot
    images: &dyn ImageStore,
    mode: Mode,
) -> R<Option<Tables>> {
    if mode != Mode::Serve || image.mode() != Mode::Serve {
        return Ok(None);
    }
    let at = key(image.key());
    // The record's pages, read a part at a time: nothing of it is held here.
    let source = images.pages(&at).map_err(|why| {
        format!("reading the impl tables of crate {} (key {at:?}): {why}", krate.named(db))
    })?;
    Ok(source.map(|source| Tables { pages: Paged { key: at, source }, mode: Mode::Serve }))
}

/// Writes to `images` the impl tables of each of `crates`, a crate with the key its image is
/// filed under, that is not read from tables already: what the next snapshot's [`find`]
/// finds. Called once the crates are resolved and checked, where their images are written.
/// Gives the crates whose tables do not encode, with why: such a crate has no tables in the
/// store and its impls are found by every snapshot, which the caller says.
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
