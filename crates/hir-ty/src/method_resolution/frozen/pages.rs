//! A crate's impl tables as pages: one record a lookup reads a bounded part of
//! (docs/v2/plan/design/restore.md, "Impl tables read in place"), with the discipline of
//! `hir_def::nameres::frozen::pages`.
//!
//! The record is segments (`hir_def::nameres::frozen::segments`), each found through an
//! open-addressed table of 16-byte slots (a key's hash, where its entry is):
//!
//! | table | key | its entry holds | read by |
//! |---|---|---|---|
//! | rows | a trait and a simplified self type | the impls and built-in derives of the trait for the type | [`row`] |
//! | traits | a trait | its blanket impls, and where its rows are (one run of the record) | [`blanket`], [`trait_into`] |
//! | inherent | a simplified self type | the type's inherent impls | [`inherent`] |
//! | names | a name's text | each trait of the crate with a function or constant of that name, and the item | [`value_items`] |
//! | selfs | a simplified self type | where each row of a trait for the type is | [`for_self_ty`] |
//!
//! | segment | read when |
//! |---|---|
//! | header (100 bytes), files | the crate's tables are first asked |
//! | a window of four slots | a key is looked up |
//! | an entry | its key is looked up |
//!
//! A lookup reads the window its key's hash names and the entry of each slot of that hash:
//! two reads of a few dozen bytes where the table has no collision, whatever the crate's
//! size. A key that is not there ends at an empty slot of its window: a definite "none" from
//! one read, with no scan. A table with no key at all answers from the header alone.
//!
//! What each lookup reads, against its floor (an oracle that reads only what it returns):
//! a row, a type's inherent impls, a trait's blanket impls and a name's items read a window
//! and one entry, two reads (the floor is the one entry). Every trait impl for a self type
//! ([`for_self_ty`]) reads a window, the type's entry and each of its rows: `k + 2` reads for
//! `k` rows, where the floor is `k`. A row is written once, under its trait, and the type's
//! entry names its place: a copy of the impls under the type would make it `2` reads and
//! double the rows' bytes on disk.
//!
//! The whole of a table is read only by a reader of all of it ([`all_traits_into`],
//! [`all_inherent_into`], [`all_value_items`]), which goes over the slots.
//!
//! Every place is from the body's start, which the header gives, and every number
//! little-endian: nothing is fixed up after a read. Nothing is a database's id: an item is
//! written as `hir_def`'s frozen encoding writes it, and a key's hash is of what the key is
//! in every database ([`trait_hash`], [`self_ty_hash`]: the item's syntax id, which is of its
//! kind and name; a name's text). A hash only finds the slot: the entry holds its key, which
//! the reader compares.
//!
//! The bytes come through a `PageSource`, which holds them only while a read runs. Bytes that
//! do not read unwind with `Unreadable` naming the crate; nothing is found in their place.

use base_db::{Crate, Db};
use either::Either;
use hir_def::{
    AdtId, BuiltinDeriveImplId, ImplId, Lookup, ModuleDefId, TraitId,
    deps::PageSource,
    nameres::frozen::{
        self as frozen_def, Dec, Enc, Unreadable,
        segments::{self, Files, Writer},
    },
};
use hir_expand::name::Name;
use rustc_type_ir::fast_reject::SimplifiedType as S;

use super::{R, read_simplified, simplified_number, write_simplified};
use crate::{
    method_resolution::{
        CandidateId, InherentImpls, InherentImplsBuilder, TraitImpls, TraitImplsBuilder,
        trait_value_items,
    },
    next_solver::{SimplifiedType, SolverDefId},
};

#[cfg(test)]
mod tests;

/// What `Unreadable` calls these bytes.
const KIND: &str = "impl table pages";

/// What a record of impl table pages starts with.
const MAGIC: [u8; 8] = *b"blzimpg\0";

/// The layout of a record: it changes with every change to the header, the slots, which
/// tables there are or what an entry holds. What an id is written as is
/// `hir_def::nameres::frozen::ENCODING`'s, which the header names as well. Layout 1 had no
/// table of self types; its header is refused by this number.
pub const LAYOUT: u32 = 2;

/// Bytes of the header.
const HEADER: usize = 100;
/// Bytes of a table's slot.
const SLOT: usize = 16;
/// The slots a lookup reads at once: a key is in the first of them unless that many keys
/// before it share its slot.
const WINDOW: u32 = 4;

const FNV_BASIS: u64 = 0xcbf2_9ce4_8422_2325;

/// FNV-1a of `bytes` from `hash`: the same in every process, which a hash of an interned id
/// is not.
const fn fnv(mut hash: u64, bytes: &[u8]) -> u64 {
    let mut at = 0;
    while at < bytes.len() {
        hash ^= bytes[at] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        at += 1;
    }
    hash
}

/// Never 0, which marks an empty slot.
const fn nonzero(hash: u64) -> u64 {
    if hash == 0 { 1 } else { hash }
}

/// The encodings of the entries, as the header names them: `hir_def`'s of the ids and this
/// crate's of a simplified type.
const ENCODING: u64 =
    fnv(fnv(FNV_BASIS, frozen_def::ENCODING.as_bytes()), super::ENCODING.as_bytes());

/// The hash a trait's slot is found by: of its syntax id (its kind and name in its file),
/// which every database of the same sources gives it.
fn trait_hash(db: &Db, trait_: TraitId) -> u64 {
    let raw = trait_.lookup(db).id.value.erase().into_raw();
    nonzero(fnv(fnv(FNV_BASIS, b"t"), &raw.to_le_bytes()))
}

/// The syntax id of the item a simplified type names; `None` for one no table is written
/// with ([`write_simplified`] refuses it).
fn def_raw(db: &Db, def: &SolverDefId<'_>) -> Option<u32> {
    Some(match *def {
        SolverDefId::AdtId(AdtId::StructId(it)) => it.lookup(db).id.value.erase().into_raw(),
        SolverDefId::AdtId(AdtId::UnionId(it)) => it.lookup(db).id.value.erase().into_raw(),
        SolverDefId::AdtId(AdtId::EnumId(it)) => it.lookup(db).id.value.erase().into_raw(),
        SolverDefId::TraitId(it) => it.lookup(db).id.value.erase().into_raw(),
        SolverDefId::TypeAliasId(it) => it.lookup(db).id.value.erase().into_raw(),
        _ => return None,
    })
}

/// The hash a simplified self type's slot is found by: of its kind and, as it has one, its
/// number ([`simplified_number`]) or the syntax id of the item it names. `None` for a type
/// no table is written with (a closure, a coroutine): no served crate has an impl for it.
fn self_ty_hash(db: &Db, self_ty: &SimplifiedType<'_>) -> Option<u64> {
    let (kind, number) = simplified_number(self_ty)?;
    let number = match self_ty {
        S::Adt(def) | S::Foreign(def) | S::Trait(def) => u64::from(def_raw(db, def)?),
        _ => number,
    };
    let hash = fnv(fnv(fnv(FNV_BASIS, b"s"), &[kind]), &number.to_le_bytes());
    Some(nonzero(hash))
}

/// The hash a row's slot is found by, of its trait's and its self type's.
fn row_hash(trait_: u64, self_ty: u64) -> u64 {
    nonzero(fnv(fnv(fnv(FNV_BASIS, b"r"), &trait_.to_le_bytes()), &self_ty.to_le_bytes()))
}

/// The hash a name's slot is found by: of its text alone. A keyed name and a plain one of
/// one text share an entry, which holds each name whole.
fn name_hash(name: &Name) -> u64 {
    nonzero(fnv(fnv(FNV_BASIS, b"n"), name.unkeyed().as_str().as_bytes()))
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from(u32_at(bytes, at)) | u64::from(u32_at(bytes, at + 4)) << 32
}

/// One table of slots: where it is in the body, how many slots (a power of two, or 0 for a
/// table with no key) and how many keys.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Table {
    at: u32,
    slots: u32,
    keys: u32,
}

/// The header: where the files, the body and each table are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    files_len: u32,
    /// Where the body starts: every place in a slot or an entry is from here.
    body_at: u32,
    total: u32,
    rows: Table,
    traits: Table,
    inherent: Table,
    names: Table,
    selfs: Table,
    /// Facts of the whole crate, a bit each ([`DECLARES_SEALED`]).
    flags: u32,
}

/// The flag of a crate that declares a `#[blaze::sealed]` trait (`traits::declares_sealed`):
/// asked of every crate of a graph, it is the header's for a served crate, not a walk of its
/// modules and impls.
const DECLARES_SEALED: u32 = 1;

impl Header {
    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&LAYOUT.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&ENCODING.to_le_bytes());
        for it in [self.files_len, self.body_at, self.total] {
            out.extend_from_slice(&it.to_le_bytes());
        }
        for table in [self.rows, self.traits, self.inherent, self.names, self.selfs] {
            for it in [table.at, table.slots, table.keys] {
                out.extend_from_slice(&it.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.flags.to_le_bytes());
    }

    /// The header in `bytes`, or why they are no pages this blaze reads: another record, a
    /// record of another layout, or one whose entries are of another encoding, each said by
    /// name.
    fn parse(bytes: &[u8]) -> R<Header> {
        if bytes.len() != HEADER {
            // alloc: error path
            return Err(format!("a header of {} bytes", bytes.len()));
        }
        if bytes[..8] != MAGIC {
            // alloc: error path
            return Err(format!(
                "no impl table pages: the record starts with {:02x?}",
                &bytes[..8]
            ));
        }
        let layout = u32_at(bytes, 8);
        if layout != LAYOUT {
            // alloc: error path
            return Err(format!(
                "impl table pages of layout {layout}, where this blaze reads layout {LAYOUT}"
            ));
        }
        let encoding = u64_at(bytes, 16);
        if encoding != ENCODING {
            // alloc: error path
            return Err(format!(
                "impl table pages whose entries are of encoding {encoding:#x}, where this \
                 blaze reads `{}` with `{}` ({ENCODING:#x})",
                frozen_def::ENCODING,
                super::ENCODING
            ));
        }
        let table = |at: usize| Table {
            at: u32_at(bytes, at),
            slots: u32_at(bytes, at + 4),
            keys: u32_at(bytes, at + 8),
        };
        let header = Header {
            files_len: u32_at(bytes, 24),
            body_at: u32_at(bytes, 28),
            total: u32_at(bytes, 32),
            rows: table(36),
            traits: table(48),
            inherent: table(60),
            names: table(72),
            selfs: table(84),
            flags: u32_at(bytes, 96),
        };
        if header.flags & !DECLARES_SEALED != 0 {
            // alloc: error path
            return Err(format!(
                "impl table pages with flags {:#x}, of which this blaze knows {DECLARES_SEALED:#x}",
                header.flags
            ));
        }
        for (name, table) in [
            ("rows", header.rows),
            ("traits", header.traits),
            ("inherent", header.inherent),
            ("names", header.names),
            ("self types", header.selfs),
        ] {
            if table.slots != 0 && !table.slots.is_power_of_two() {
                // alloc: error path
                return Err(format!("a table of {name} of {} slots", table.slots));
            }
        }
        Ok(header)
    }
}

/// A table's slot: the key's hash (0: empty) and where its entry is, from the body's start.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Slot {
    hash: u64,
    at: u32,
    len: u32,
}

impl Slot {
    fn parse(bytes: &[u8]) -> Slot {
        Slot { hash: u64_at(bytes, 0), at: u32_at(bytes, 8), len: u32_at(bytes, 12) }
    }
}

/// The slots of a table of `entries` (each a key's hash and where its entry is), as bytes:
/// twice as many slots as keys, each key in the first empty slot from its hash's.
fn table_of(entries: &[(u64, (u32, u32))]) -> (u32, Vec<u8>) {
    if entries.is_empty() {
        // alloc: none, an empty list
        return (0, Vec::new());
    }
    let count = (entries.len() * 2).next_power_of_two();
    let mask = count - 1;
    // alloc: encoding, once per table per write of a crate's pages
    let mut slots = vec![Slot::default(); count];
    for &(hash, (at, len)) in entries {
        let mut slot = hash as u32 as usize & mask;
        while slots[slot].hash != 0 {
            slot = (slot + 1) & mask;
        }
        slots[slot] = Slot { hash, at, len };
    }
    // alloc: encoding, as above
    let mut bytes = Vec::with_capacity(count * SLOT);
    for slot in slots {
        bytes.extend_from_slice(&slot.hash.to_le_bytes());
        bytes.extend_from_slice(&slot.at.to_le_bytes());
        bytes.extend_from_slice(&slot.len.to_le_bytes());
    }
    (count as u32, bytes)
}

/// `read` of the `len` bytes of `source` at `at`.
// dyn: the crate's one source
fn read<T>(
    source: &dyn PageSource,
    at: u64,
    len: u32,
    read: impl FnOnce(&[u8]) -> R<T>,
) -> R<T> {
    let mut read = Some(read);
    let mut out = None;
    source.read(at, u64::from(len), &mut |bytes| {
        let Some(read) = read.take() else { return };
        out = Some(match bytes.len() == len as usize {
            true => read(bytes),
            // alloc: error path
            false => Err(format!("{} bytes read at {at}, {len} asked", bytes.len())),
        });
    })?;
    // alloc: error path
    out.unwrap_or_else(|| Err(format!("nothing read at {at}")))
}

/// What every reader of a crate's impl table pages needs first, read once: where each table
/// is, and the files the entries name.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Head {
    header: Header,
    files: Files,
}

// dyn: the crate's one source
fn read_head(db: &Db, krate: Crate, source: &dyn PageSource) -> R<Head> {
    let header = read(source, 0, HEADER as u32, Header::parse)?;
    if u64::from(header.total) != source.size() {
        // alloc: error path
        return Err(format!(
            "a record of {} bytes whose header says {}",
            source.size(),
            header.total
        ));
    }
    if u64::from(header.body_at) != HEADER as u64 + u64::from(header.files_len)
        || header.body_at > header.total
    {
        // alloc: error path
        return Err(format!(
            "a body at {} after {} bytes of files, in a record of {}",
            header.body_at, header.files_len, header.total
        ));
    }
    let files = read(source, HEADER as u64, header.files_len, |bytes| {
        segments::files(db, krate, bytes)
    })?;
    Ok(Head { header, files })
}

/// The head of the impl table pages of `krate`. Depends on which pages the crate has and on
/// the files they name.
#[blaze_query::tracked(returns(ref), mem_tag = Infer)]
fn impl_pages_head(db: &Db, krate: Crate) -> Head {
    // The value is the record's, found by content: told to the engine (`row::STORED`).
    blaze_query::db::report_stored_read(db);
    let Some(tables) = super::tables(db, krate) else {
        // alloc: error path
        refuse(db, krate, "the crate has no impl table pages".to_owned())
    };
    match read_head(db, krate, &*tables.pages.source) {
        Ok(head) => head,
        Err(why) => refuse(db, krate, why),
    }
}

/// Unwinds with `Unreadable` naming `krate` and `why` its pages do not read.
fn refuse(db: &Db, krate: Crate, why: String) -> ! {
    // alloc: an error's crate name, once
    let name = krate.extra_data(db).display_name.as_ref().map(|it| it.crate_name().to_string());
    Unreadable { krate, name, kind: KIND, why }.throw()
}

/// One reader's view of the pages of a crate: its source, its head, and where its body is.
struct Pages<'a> {
    db: &'a Db,
    krate: Crate,
    // dyn: the crate's one source
    source: &'a dyn PageSource,
    head: &'a Head,
}

impl<'a> Pages<'a> {
    /// The pages of `krate`, which must be served ([`super::served`]). Tells the engine the
    /// running query's value is a record's (`row::STORED`): every reader here is one.
    fn of(db: &'a Db, krate: Crate) -> Pages<'a> {
        blaze_query::db::report_stored_read(db);
        let Some(tables) = super::tables(db, krate) else {
            // alloc: error path
            refuse(db, krate, "the crate has no impl table pages".to_owned())
        };
        Pages { db, krate, source: &*tables.pages.source, head: impl_pages_head(db, krate) }
    }

    /// `with` of the `len` bytes at `at` of the body.
    fn body<T>(&self, at: u32, len: u32, with: impl FnOnce(&[u8]) -> R<T>) -> R<T> {
        read(self.source, u64::from(self.head.header.body_at) + u64::from(at), len, with)
    }

    /// `with` of a reader of the entry at `at` of the body, read to its end.
    fn entry<T>(&self, at: u32, len: u32, with: impl FnOnce(&mut Dec<'_>) -> R<T>) -> R<T> {
        self.body(at, len, |bytes| {
            let mut dec = segments::reader(self.db, self.krate, &self.head.files, bytes);
            let found = with(&mut dec)?;
            dec.finish()?;
            Ok(found)
        })
    }

    /// The entry of `table` whose slot has `hash` and of which `entry` gives `Some`: `entry`
    /// reads an entry's key, gives `None` for another key of the same hash, and reads the
    /// rest for its own. `None` when the walk from the hash's slot ends at an empty one: the
    /// key is not in the table.
    fn find<T>(
        &self,
        table: Table,
        hash: u64,
        mut entry: impl FnMut(&mut Dec<'_>) -> R<Option<T>>,
    ) -> R<Option<T>> {
        if table.slots == 0 {
            return Ok(None);
        }
        let mask = table.slots - 1;
        let mut at = hash as u32 & mask;
        let mut probed = 0;
        // The bound: every slot once. A table is at most half full, so a walk ends at an
        // empty slot long before; one that does not is a table this did not write.
        while probed < table.slots {
            let run = WINDOW.min(table.slots - at);
            let slots = self.body(table.at + at * SLOT as u32, run * SLOT as u32, |bytes| {
                let mut slots = [Slot::default(); WINDOW as usize];
                for (slot, bytes) in slots.iter_mut().zip(bytes.chunks_exact(SLOT)) {
                    *slot = Slot::parse(bytes);
                }
                Ok(slots)
            })?;
            for slot in &slots[..run as usize] {
                if slot.hash == 0 {
                    return Ok(None);
                }
                if slot.hash != hash {
                    continue;
                }
                // Read without `Pages::entry`: an entry of another key is not read to its end.
                let found = self.body(slot.at, slot.len, |bytes| {
                    let mut dec = segments::reader(self.db, self.krate, &self.head.files, bytes);
                    let found = entry(&mut dec)?;
                    if found.is_some() {
                        dec.finish()?;
                    }
                    Ok(found)
                })?;
                if found.is_some() {
                    return Ok(found);
                }
            }
            probed += run;
            at = (at + run) & mask;
        }
        // alloc: error path
        Err(format!("a table of {} slots with none empty", table.slots))
    }

    /// Every entry of `table`, in the order of its slots: for a reader of all of it.
    fn each(&self, table: Table, mut entry: impl FnMut(&mut Dec<'_>) -> R) -> R {
        if table.slots == 0 {
            return Ok(());
        }
        let len = table.slots.checked_mul(SLOT as u32).ok_or("a table over 4 GiB")?;
        let slots = self.body(table.at, len, |bytes| {
            // alloc: the places of a whole table, once per reader of all of it
            Ok(bytes.chunks_exact(SLOT).map(Slot::parse).collect::<Vec<Slot>>())
        })?;
        for slot in slots.iter().filter(|it| it.hash != 0) {
            self.entry(slot.at, slot.len, &mut entry)?;
        }
        Ok(())
    }

    /// Unwinds naming the crate and `what` was being read when `why` stopped it.
    fn refuse(&self, what: std::fmt::Arguments<'_>, why: String) -> ! {
        // alloc: error path
        refuse(self.db, self.krate, format!("{what}: {why}"))
    }
}

fn trait_of(dec: &mut Dec<'_>) -> R<TraitId> {
    match dec.def()? {
        ModuleDefId::TraitId(it) => Ok(it),
        // alloc: error path
        _ => Err("impls of no trait".to_owned()),
    }
}

/// The rest of a row's entry after its key: its impls, then its built-in derives.
fn row_rest(dec: &mut Dec<'_>, mut each: impl FnMut(Either<ImplId, BuiltinDeriveImplId>)) -> R {
    for _ in 0..dec.len()? {
        each(Either::Left(dec.impl_id()?));
    }
    for _ in 0..dec.len()? {
        each(Either::Right(dec.builtin_derive_impl()?));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// The bug a test planted: a key that is not in its table is searched for by a scan of
    /// the whole table, as a reader with no definite "none" would.
    pub(crate) static SCANS_FOR_ABSENT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The bug a test planted: rows are written with no slot, so none is ever found.
    pub(crate) static WRITES_NO_ROW_SLOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The bug a test planted: self types are written with no slot, so a type's rows are
    /// never found from it.
    pub(crate) static WRITES_NO_SELF_SLOT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The impls of `trait_` for `self_ty` in the served crate `krate`, each given to `each`:
/// the impls in the order they were found, then the built-in derives. Reads the row's slot
/// and its entry; a trait with no impl for the type is known from the slot alone.
pub(in crate::method_resolution) fn row<'db>(
    db: &'db Db,
    krate: Crate,
    trait_: TraitId,
    self_ty: &SimplifiedType<'db>,
    mut each: impl FnMut(Either<ImplId, BuiltinDeriveImplId>),
) {
    let pages = Pages::of(db, krate);
    let Some(self_hash) = self_ty_hash(db, self_ty) else { return };
    let hash = row_hash(trait_hash(db, trait_), self_hash);
    let found = pages.find(pages.head.header.rows, hash, |dec| {
        if trait_of(dec)? != trait_ || read_simplified(dec)? != *self_ty {
            return Ok(None);
        }
        row_rest(dec, &mut each)?;
        Ok(Some(()))
    });
    #[cfg(test)]
    if SCANS_FOR_ABSENT.get() && matches!(found, Ok(None)) {
        _ = pages.each(pages.head.header.rows, |dec| {
            trait_of(dec)?;
            read_simplified(dec)?;
            row_rest(dec, |_| ())
        });
    }
    if let Err(why) = found {
        pages.refuse(format_args!("the impls of {trait_:?} for {self_ty:?}"), why)
    }
}

/// What a trait's entry holds after its blanket impls: where its rows are (one run of the
/// body) and the length of each.
struct RowsOf {
    at: u32,
    lens: Vec<u32>,
}

/// A trait's entry after its key: its blanket impls, each given to `each`, and its rows.
fn trait_rest(dec: &mut Dec<'_>, mut each: impl FnMut(ImplId)) -> R<RowsOf> {
    for _ in 0..dec.len()? {
        each(dec.impl_id()?);
    }
    let at = dec.u32()?;
    // alloc: the lengths of one trait's rows, once per trait's entry read
    let lens = dec.many(|dec| dec.u32())?;
    Ok(RowsOf { at, lens })
}

/// The blanket impls of `trait_` in the served crate `krate`, each given to `each` in the
/// order they were found. A trait with no impl in the crate is known from its slot alone.
pub(in crate::method_resolution) fn blanket(
    db: &Db,
    krate: Crate,
    trait_: TraitId,
    mut each: impl FnMut(ImplId),
) {
    let pages = Pages::of(db, krate);
    let found = pages.find(pages.head.header.traits, trait_hash(db, trait_), |dec| {
        if trait_of(dec)? != trait_ {
            return Ok(None);
        }
        trait_rest(dec, &mut each).map(Some)
    });
    if let Err(why) = found {
        pages.refuse(format_args!("the blanket impls of {trait_:?}"), why)
    }
}

/// The rows `rows` names, each added to trait `t` of `builder`: one read of their run.
fn rows_into<'db>(
    pages: &Pages<'_>,
    rows: &RowsOf,
    trait_: TraitId,
    t: u32,
    builder: &mut TraitImplsBuilder<'db, '_>,
) -> R {
    let total = rows.lens.iter().try_fold(0u32, |sum, &len| sum.checked_add(len));
    let total = total.ok_or("rows over 4 GiB")?;
    if total == 0 {
        return Ok(());
    }
    pages.body(rows.at, total, |bytes| {
        let mut at = 0usize;
        for &len in &rows.lens {
            let entry = &bytes[at..at + len as usize];
            at += len as usize;
            let mut dec = segments::reader(pages.db, pages.krate, &pages.head.files, entry);
            if trait_of(&mut dec)? != trait_ {
                // alloc: error path
                return Err("a row of another trait among a trait's rows".to_owned());
            }
            let row = builder.row(t, read_simplified(&mut dec)?);
            row_rest(&mut dec, |it| match it {
                Either::Left(it) => builder.push_impl(row, it),
                Either::Right(it) => builder.push_derive(row, it),
            })?;
            dec.finish()?;
        }
        Ok(())
    })
}

/// Every impl of `trait_` in the served crate `krate`, added to `builder`: its blanket
/// impls and each of its rows. Reads the trait's slot, its entry and the one run its rows
/// are in: as much of the record as the trait has impls, and nothing of another trait.
pub(in crate::method_resolution) fn trait_into<'db>(
    db: &'db Db,
    krate: Crate,
    trait_: TraitId,
    builder: &mut TraitImplsBuilder<'db, '_>,
) {
    let pages = Pages::of(db, krate);
    let found = pages.find(pages.head.header.traits, trait_hash(db, trait_), |dec| {
        if trait_of(dec)? != trait_ {
            return Ok(None);
        }
        let t = builder.trait_(trait_);
        let rows = trait_rest(dec, |it| builder.push_blanket(t, it))?;
        Ok(Some((t, rows)))
    });
    let read = found.and_then(|found| match found {
        Some((t, rows)) => rows_into(&pages, &rows, trait_, t, builder),
        None => Ok(()),
    });
    if let Err(why) = read {
        pages.refuse(format_args!("the impls of {trait_:?}"), why)
    }
}

/// Every trait impl of the served crate `krate`, added to `builder`: for a reader of all of
/// them (`TraitImpls::for_crate`). A lookup never asks for this.
pub(in crate::method_resolution) fn all_traits_into<'db>(
    db: &'db Db,
    krate: Crate,
    builder: &mut TraitImplsBuilder<'db, '_>,
) {
    let pages = Pages::of(db, krate);
    // alloc: each trait's rows, once per reader of all of the crate's impls
    let mut traits: Vec<(TraitId, u32, RowsOf)> = Vec::new();
    let read = pages
        .each(pages.head.header.traits, |dec| {
            let trait_ = trait_of(dec)?;
            let t = builder.trait_(trait_);
            let rows = trait_rest(dec, |it| builder.push_blanket(t, it))?;
            traits.push((trait_, t, rows));
            Ok(())
        })
        .and_then(|()| {
            traits.iter().try_for_each(|(trait_, t, rows)| {
                rows_into(&pages, rows, *trait_, *t, builder)
            })
        });
    if let Err(why) = read {
        pages.refuse(format_args!("every trait impl"), why)
    }
}

/// The inherent impls of the served crate `krate` for `self_ty`, each given to `each` in
/// the order they were found. A type with none is known from its slot alone.
pub(in crate::method_resolution) fn inherent<'db>(
    db: &'db Db,
    krate: Crate,
    self_ty: &SimplifiedType<'db>,
    mut each: impl FnMut(ImplId),
) {
    let pages = Pages::of(db, krate);
    let Some(hash) = self_ty_hash(db, self_ty) else { return };
    let found = pages.find(pages.head.header.inherent, hash, |dec| {
        if read_simplified(dec)? != *self_ty {
            return Ok(None);
        }
        for _ in 0..dec.len()? {
            each(dec.impl_id()?);
        }
        Ok(Some(()))
    });
    if let Err(why) = found {
        pages.refuse(format_args!("the inherent impls of {self_ty:?}"), why)
    }
}

/// Every inherent impl of the served crate `krate`, added to `builder`: for a reader of all
/// of them (`InherentImpls::for_crate`).
pub(in crate::method_resolution) fn all_inherent_into<'db>(
    db: &'db Db,
    krate: Crate,
    builder: &mut InherentImplsBuilder<'db, '_>,
) {
    let pages = Pages::of(db, krate);
    let read = pages.each(pages.head.header.inherent, |dec| {
        let group = builder.group(read_simplified(dec)?);
        for _ in 0..dec.len()? {
            builder.push(group, dec.impl_id()?);
        }
        Ok(())
    });
    if let Err(why) = read {
        pages.refuse(format_args!("every inherent impl"), why)
    }
}

/// One item of a name's entry: the name as its trait binds it (keyed, where a `macro` names
/// it), the trait, and the function or constant.
fn value_item(dec: &mut Dec<'_>) -> R<(Name, TraitId, CandidateId)> {
    let name = dec.name()?;
    let trait_ = trait_of(dec)?;
    let item = match dec.def()? {
        ModuleDefId::FunctionId(it) => CandidateId::FunctionId(it),
        ModuleDefId::ConstId(it) => CandidateId::ConstId(it),
        // alloc: error path
        _ => return Err("a trait's value item that is neither a function nor a constant".to_owned()),
    };
    Ok((name, trait_, item))
}

/// For each trait of the served crate `krate` with a function or constant `name` looks up,
/// the trait and the item, each given to `each` (in the record's order, which is no
/// database's). A name no trait of the crate binds is known from its slot alone.
pub(in crate::method_resolution) fn value_items(
    db: &Db,
    krate: Crate,
    name: Name,
    mut each: impl FnMut(TraitId, CandidateId),
) {
    let pages = Pages::of(db, krate);
    let text = name.unkeyed();
    let found = pages.find(pages.head.header.names, name_hash(&name), |dec| {
        let count = dec.len()?;
        for at in 0..count {
            let (of, trait_, item) = value_item(dec)?;
            if at == 0 && of.unkeyed() != text {
                return Ok(None);
            }
            if of == name {
                each(trait_, item);
            }
        }
        Ok(Some(()))
    });
    if let Err(why) = found {
        pages.refuse(format_args!("the traits' items named `{}`", name.as_str()), why)
    }
}

/// Every function and constant of every trait of the served crate `krate` with the name it
/// is looked up by, each given to `each`: for a reader of all of them.
pub(in crate::method_resolution) fn all_value_items(
    db: &Db,
    krate: Crate,
    mut each: impl FnMut(Name, TraitId, CandidateId),
) {
    let pages = Pages::of(db, krate);
    let read = pages.each(pages.head.header.names, |dec| {
        for _ in 0..dec.len()? {
            let (name, trait_, item) = value_item(dec)?;
            each(name, trait_, item);
        }
        Ok(())
    });
    if let Err(why) = read {
        pages.refuse(format_args!("every trait's value items"), why)
    }
}

/// Every trait impl of the served crate `krate` for `self_ty`, each given to `each`: per
/// row, its impls and then its built-in derives, the rows in the order of their traits'
/// fingerprints (no database's). What `TraitImpls::for_self_ty` gives of the crate's whole
/// table, from the type's entry and its rows alone: `k + 2` reads for `k` rows (the module
/// doc). A type with no trait impl is known from its slot alone.
pub(in crate::method_resolution) fn for_self_ty<'db>(
    db: &'db Db,
    krate: Crate,
    self_ty: &SimplifiedType<'db>,
    mut each: impl FnMut(Either<ImplId, BuiltinDeriveImplId>),
) {
    let pages = Pages::of(db, krate);
    let Some(hash) = self_ty_hash(db, self_ty) else { return };
    let found = pages.find(pages.head.header.selfs, hash, |dec| {
        if read_simplified(dec)? != *self_ty {
            return Ok(None);
        }
        // alloc: the places of one type's rows, once per lookup of the type
        dec.many(|dec| Ok((dec.u32()?, dec.u32()?))).map(Some)
    });
    let read = found.and_then(|rows| {
        rows.unwrap_or_default().into_iter().try_for_each(|(at, len)| {
            pages.entry(at, len, |dec| {
                trait_of(dec)?;
                if read_simplified(dec)? != *self_ty {
                    // alloc: error path
                    return Err("a row of another self type among a type's rows".to_owned());
                }
                row_rest(dec, &mut each)
            })
        })
    });
    if let Err(why) = read {
        pages.refuse(format_args!("the trait impls for {self_ty:?}"), why)
    }
}

/// Whether the served crate `krate` declares a `#[blaze::sealed]` trait, as its header
/// says: written from `traits::declares_sealed` when the pages were.
pub fn declares_sealed(db: &Db, krate: Crate) -> bool {
    impl_pages_head(db, krate).header.flags & DECLARES_SEALED != 0
}

/// How many keys each table of the pages of the served crate `krate` has: rows, traits,
/// inherent self types, names, self types with a row. From the head alone.
pub fn counts(db: &Db, krate: Crate) -> [u32; 5] {
    let header = &impl_pages_head(db, krate).header;
    [
        header.rows.keys,
        header.traits.keys,
        header.inherent.keys,
        header.names.keys,
        header.selfs.keys,
    ]
}

fn impl_ids(enc: &mut Enc<'_>, impls: &[ImplId]) -> R {
    enc.usize(impls.len())?;
    for &it in impls {
        enc.impl_id(it)?;
    }
    Ok(())
}

/// The pages of the impl tables of `krate` as they are now: found, or read (in
/// `Mode::Verify`, where writing what was read gives the record again). Refuses a crate
/// with an impl the encoding cannot name, with why.
pub fn encode(db: &Db, krate: Crate) -> R<Vec<u8>> {
    let mut writer = Writer::new(db, krate);
    // alloc: encoding, once per table per write of a crate's pages (not on a build's path)
    let mut row_slots: Vec<(u64, (u32, u32))> = Vec::new();
    // alloc: encoding, as above
    let mut trait_slots: Vec<(u64, (u32, u32))> = Vec::new();
    // alloc: encoding, as above
    let mut inherent_slots: Vec<(u64, (u32, u32))> = Vec::new();
    // alloc: encoding, as above
    let mut name_slots: Vec<(u64, (u32, u32))> = Vec::new();
    // alloc: encoding, as above
    let mut self_slots: Vec<(u64, (u32, u32))> = Vec::new();
    // Each row's self type and place, for the table of self types.
    // alloc: encoding, once per write of a crate's pages
    let mut rows_of_selfs: Vec<(SimplifiedType<'_>, (u32, u32))> = Vec::new();
    #[cfg(test)]
    let (no_row_slot, no_self_slot) = (WRITES_NO_ROW_SLOT.get(), WRITES_NO_SELF_SLOT.get());
    #[cfg(not(test))]
    let (no_row_slot, no_self_slot) = (false, false);

    // Each trait in the order of its fingerprint, and its rows in the order of their self
    // types': the order of no database's ids, so two databases write the same bytes.
    let trait_impls = &**TraitImpls::for_crate(db, krate);
    let traits = writer
        .enc()
        .sorted(trait_impls.trait_indices(), |enc, it| enc.def(ModuleDefId::TraitId(*it)))?;
    for (trait_, t) in traits {
        let of_trait = trait_hash(db, trait_);
        let rows = writer.enc().sorted(trait_impls.rows_at(t), |enc, it| write_simplified(enc, it))?;
        let mut rows_at = 0;
        // alloc: encoding, once per trait per write of a crate's pages
        let mut lens: Vec<u32> = Vec::with_capacity(rows.len());
        for (self_ty, (impls, derives)) in rows {
            let place = writer.segment(|enc| {
                enc.def(ModuleDefId::TraitId(trait_))?;
                write_simplified(enc, self_ty)?;
                impl_ids(enc, impls)?;
                enc.usize(derives.len())?;
                for &it in derives {
                    enc.builtin_derive_impl(it)?;
                }
                Ok(())
            })?;
            if lens.is_empty() {
                rows_at = place.0;
            }
            lens.push(place.1);
            let of_self = self_ty_hash(db, self_ty).ok_or("a self type with no hash")?;
            if !no_row_slot {
                row_slots.push((row_hash(of_trait, of_self), place));
            }
            rows_of_selfs.push((*self_ty, place));
        }
        let blanket = trait_impls.blanket_impls(trait_);
        let place = writer.segment(|enc| {
            enc.def(ModuleDefId::TraitId(trait_))?;
            impl_ids(enc, blanket)?;
            enc.u32(rows_at);
            enc.usize(lens.len())?;
            for &len in &lens {
                enc.u32(len);
            }
            Ok(())
        })?;
        trait_slots.push((of_trait, place));
    }

    let inherent_impls = InherentImpls::for_crate(db, krate);
    let inherent = writer.enc().sorted(inherent_impls.iter(), |enc, it| write_simplified(enc, it))?;
    for (self_ty, impls) in inherent {
        let place = writer.segment(|enc| {
            write_simplified(enc, self_ty)?;
            impl_ids(enc, impls)
        })?;
        let hash = self_ty_hash(db, self_ty).ok_or("a self type with no hash")?;
        inherent_slots.push((hash, place));
    }

    // The traits' value items, an entry a text: in the order of the fingerprints of name and
    // trait, the entries in the order their first item comes in it.
    let items = trait_value_items(db, krate);
    let items = writer.enc().sorted(
        items.iter().map(|&(name, trait_, item)| ((name, trait_), item)),
        |enc, (name, trait_)| {
            enc.name(name)?;
            enc.def(ModuleDefId::TraitId(*trait_))
        },
    )?;
    // alloc: encoding, once per write of a crate's pages
    let mut texts: Vec<(Name, Vec<(Name, TraitId, CandidateId)>)> = Vec::new();
    for ((name, trait_), item) in items {
        let text = name.unkeyed();
        match texts.iter_mut().rev().find(|it| it.0 == text) {
            Some(group) => group.1.push((name, trait_, item)),
            // alloc: encoding, once per name's text
            None => texts.push((text, vec![(name, trait_, item)])),
        }
    }
    for (text, items) in &texts {
        let place = writer.segment(|enc| {
            enc.usize(items.len())?;
            for &(name, trait_, item) in items {
                enc.name(&name)?;
                enc.def(ModuleDefId::TraitId(trait_))?;
                enc.def(match item {
                    CandidateId::FunctionId(it) => ModuleDefId::FunctionId(it),
                    CandidateId::ConstId(it) => ModuleDefId::ConstId(it),
                })?;
            }
            Ok(())
        })?;
        name_slots.push((name_hash(text), place));
    }

    // Each self type with a row, in the order of its fingerprint, and the places of its rows
    // in the order of their traits' (the sort is stable, so a type's rows stay in the order
    // the traits were written in): an entry a type, which names the rows and copies none.
    let rows_of_selfs =
        writer.enc().sorted(rows_of_selfs.into_iter(), |enc, it| write_simplified(enc, it))?;
    let mut at = 0;
    while at < rows_of_selfs.len() {
        let self_ty = rows_of_selfs[at].0;
        let run = rows_of_selfs[at..].iter().take_while(|it| it.0 == self_ty).count();
        let places = &rows_of_selfs[at..at + run];
        at += run;
        let place = writer.segment(|enc| {
            write_simplified(enc, &self_ty)?;
            enc.usize(places.len())?;
            for &(_, (row_at, row_len)) in places {
                enc.u32(row_at);
                enc.u32(row_len);
            }
            Ok(())
        })?;
        if !no_self_slot {
            let hash = self_ty_hash(db, &self_ty).ok_or("a self type with no hash")?;
            self_slots.push((hash, place));
        }
    }

    let mut table = |slots: &[(u64, (u32, u32))]| -> R<Table> {
        let (count, bytes) = table_of(slots);
        let at = writer.raw(&bytes)?;
        Ok(Table { at, slots: count, keys: slots.len() as u32 })
    };
    let rows = table(&row_slots)?;
    let traits = table(&trait_slots)?;
    let inherent = table(&inherent_slots)?;
    let names = table(&name_slots)?;
    let selfs = table(&self_slots)?;

    let written = writer.finish()?;
    let body_at = HEADER + written.files.len();
    let total = u32::try_from(body_at + written.body.len()).map_err(|_| "pages over 4 GiB")?;
    let header = Header {
        files_len: written.files.len() as u32,
        body_at: body_at as u32,
        total,
        rows,
        traits,
        inherent,
        names,
        selfs,
        flags: match crate::traits::declares_sealed(db, krate) {
            true => DECLARES_SEALED,
            false => 0,
        },
    };
    // alloc: the record, once per crate per write of its pages
    let mut out = Vec::with_capacity(total as usize);
    header.write(&mut out);
    debug_assert_eq!(out.len(), HEADER);
    out.extend_from_slice(&written.files);
    out.extend_from_slice(&written.body);
    debug_assert_eq!(out.len(), total as usize);
    Ok(out)
}
