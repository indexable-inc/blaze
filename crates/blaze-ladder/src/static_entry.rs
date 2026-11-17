//! A static's entry (docs/v2/plan/design/run.md, "Statics in the run"): the one allocation a
//! `static` item is, as a datum record (blaze-code datum.rs) the run's child places before any
//! code that names it, and what the datum's words reach, so the parent can send those too.
//!
//! THE VALUE is the constant evaluator's record of the initializer (front2 statics.rs
//! `static_values`, the one home; blaze-build data.rs `DataRecord`): `allocs[0]` is the
//! static's own bytes, each other allocation one its value points into (a `&str`'s bytes, a
//! slice's elements). They are laid out in one datum in their order, each at its own alignment,
//! so the static's address is the datum's first byte and the whole is one allocation of the
//! program: the Reference's "All references and raw pointers to the static refer to the same
//! allocation". A relocation's word holds its addend (data.rs, "The form"); here it becomes an
//! explicit addend and the word's bytes are zero, so the datum's bytes are its form, not its
//! addresses:
//!
//! - `Local(n)`: a word of the datum's own (`OWN_DATUM`), at allocation `n`'s offset plus the
//!   addend;
//! - `Static(k)`: static `k`'s symbol (symbols.rs `static_symbol`) plus the addend, and `k`
//!   DECLARED, so the parent sends it with this one (one batch: a cycle of statics is one);
//! - `Fn` and `Vtable`: refused by name until step 2 of the spec names them as instances.
//!
//! THE PLACE: read-only for a static that is neither `static mut` nor holds an allocation the
//! evaluator marked mutable (interior mutability: data.rs `DataAlloc::mutable`); writable
//! otherwise (the child refuses writable statics by name until step 3). A `#[thread_local]`
//! static is no datum (it is the thread's template, a `Tls` target): refused here by name.
//!
//! THE FORM, little-endian: [`STATIC_ENTRY_MAGIC`], [`super::ENTRY_FORM`] (4), the datum's
//! length (4) and bytes, the declared instances' block as a code entry's (count 4, then rows:
//! none until step 2), and the declared statics' block (count 4, then item keys 16 each,
//! ascending, each once). The KEY is a walk row's (body_ids.rs `record_key`: the static's
//! symbol, its initializer's BodyCheck key and reads, its crate's fact), so an edit of the
//! initializer moves this record's key alone: code names the static's word, not its value.

use core::fmt;
use std::alloc::Global;

use blaze_build::StaticItem;
use blaze_build::data::{DataRecord, DataTarget};
use blaze_code::datum::{Datum, DatumError, DatumView, OWN_DATUM, Place};
use blaze_code::record::{Space, Symbol, Target, Word};
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;

use super::{Cursor, Declared, ENTRY_FORM, EntryError, each_key, put_statics};
use crate::symbols::static_symbol;

/// The first eight bytes of a static's entry (a code record's is [`super::ENTRY_MAGIC`]).
pub const STATIC_ENTRY_MAGIC: [u8; 8] = *b"bzsent01";

/// Why a static has no entry, naming what of its value is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StaticRecordError {
    /// A `#[thread_local]` static: the thread's template, not a datum.
    ThreadLocal,
    /// The evaluator's record has no allocation.
    Empty,
    /// The word at `at` of allocation `alloc` names a fn instance: step 2 of the spec.
    Fn { alloc: u32, at: u32 },
    /// The word at `at` of allocation `alloc` names a vtable: step 2 of the spec.
    Vtable { alloc: u32, at: u32 },
    /// Relocation `reloc` of allocation `alloc` is past its bytes, or names an allocation past
    /// the record's.
    Reloc { alloc: u32, reloc: u32 },
    /// The datum is larger than its form counts (`what`).
    TooLarge { what: &'static str },
    /// The datum did not seal (blaze-code datum.rs names the rule).
    Datum(DatumError),
}

impl fmt::Display for StaticRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spec = "run.md, \"Statics in the run\"";
        match *self {
            StaticRecordError::ThreadLocal => write!(f, "a `#[thread_local]` static is the thread's template, made with the thread-locals, not a static's datum ({spec}, step 4)"),
            StaticRecordError::Empty => write!(f, "the constant evaluator's record of its initializer holds no allocation (blaze-build data.rs: `allocs[0]` is the value)"),
            StaticRecordError::Fn { alloc, at } => write!(f, "its value's allocation {alloc} holds a fn's address at byte {at}, which a static's record does not name yet ({spec}, step 2)"),
            StaticRecordError::Vtable { alloc, at } => write!(f, "its value's allocation {alloc} holds a vtable's address at byte {at}, which a static's record does not name yet ({spec}, step 2)"),
            StaticRecordError::Reloc { alloc, reloc } => write!(f, "relocation {reloc} of its value's allocation {alloc} is past its bytes or names no allocation of the record (blaze-build data.rs, \"The form\")"),
            StaticRecordError::TooLarge { what } => write!(f, "its {what} is larger than a static's entry counts"),
            StaticRecordError::Datum(why) => write!(f, "its datum: {why}"),
        }
    }
}

impl core::error::Error for StaticRecordError {}

/// A static's entry from the evaluator's record of its initializer, `value`, and its kind (the
/// module doc).
pub fn make_static_entry<'a>(value: &DataRecord<Global>, kind: StaticItem, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, StaticRecordError> {
    if kind.thread_local {
        return Err(StaticRecordError::ThreadLocal);
    }
    let first = value.allocs.first().ok_or(StaticRecordError::Empty)?;
    // Each allocation's offset in the datum, in order, each at its own alignment.
    let mut starts: AVec<'_, u64> = Vec::with_capacity_in(value.allocs.len(), alloc);
    let mut end = 0u64;
    let mut align = first.align;
    for a in value.allocs.iter() {
        let at = end.next_multiple_of(1u64 << a.align);
        starts.push(at);
        end = at + a.bytes.len() as u64;
        align = align.max(a.align);
    }
    let len = usize::try_from(end).ok().filter(|&n| u32::try_from(n).is_ok()).ok_or(StaticRecordError::TooLarge { what: "value" })?;
    let place = match kind.mutable || value.allocs.iter().any(|a| a.mutable) {
        true => Place::Writable,
        false => Place::ReadOnly,
    };
    let mut d = Datum::new_in(place, align, alloc);
    d.bytes.resize(len, 0);
    for (n, a) in value.allocs.iter().enumerate() {
        let at = starts[n] as usize;
        d.bytes[at..at + a.bytes.len()].copy_from_slice(&a.bytes);
    }
    let mut statics: AVec<'_, ItemKey> = Vec::new_in(alloc);
    for (n, a) in value.allocs.iter().enumerate() {
        for (r, reloc) in a.relocs.iter().enumerate() {
            let refused = StaticRecordError::Reloc { alloc: n as u32, reloc: r as u32 };
            let off = reloc.offset as usize;
            if off.checked_add(8).is_none_or(|e| e > a.bytes.len()) {
                return Err(refused);
            }
            let at = starts[n] as usize + off;
            let mut word = [0u8; 8];
            word.copy_from_slice(&d.bytes[at..at + 8]);
            let addend = i64::from_le_bytes(word);
            d.bytes[at..at + 8].fill(0);
            // `at` is under the datum's length, which fits a u32.
            let at = at as u32;
            let (target, addend) = match reloc.target {
                DataTarget::Local(m) => {
                    let start = *starts.get(m as usize).ok_or(refused)?;
                    (OWN_DATUM, addend.checked_add(start as i64).ok_or(refused)?)
                }
                DataTarget::Static(k) => {
                    statics.push(k);
                    (d.target(Target { space: Space::Static, symbol: static_symbol(k) }), addend)
                }
                DataTarget::Fn(_) => return Err(StaticRecordError::Fn { alloc: n as u32, at: reloc.offset }),
                DataTarget::Vtable(_) => return Err(StaticRecordError::Vtable { alloc: n as u32, at: reloc.offset }),
            };
            d.words.push(Word { at, target, addend });
        }
    }
    let sealed = d.seal().map_err(StaticRecordError::Datum)?;
    let mut datum: AVec<'_, u8> = Vec::new_in(alloc);
    sealed.encode(&mut datum);
    let too_large = |what| StaticRecordError::TooLarge { what };
    let mut out: AVec<'a, u8> = Vec::new_in(alloc);
    out.extend_from_slice(&STATIC_ENTRY_MAGIC);
    out.extend_from_slice(&ENTRY_FORM.to_le_bytes());
    out.extend_from_slice(&u32::try_from(datum.len()).map_err(|_| too_large("datum"))?.to_le_bytes());
    out.extend_from_slice(&datum);
    // No declared instance until step 2 names a fn's address.
    out.extend_from_slice(&0u32.to_le_bytes());
    put_statics(&statics, &mut out, alloc).map_err(too_large)?;
    Ok(out)
}

/// A static's entry read in place.
#[derive(Clone, Copy)]
pub struct StaticEntryView<'b> {
    pub datum: DatumView<'b>,
    declared: &'b [u8],
    count: u32,
    statics: &'b [u8],
}

impl<'b> StaticEntryView<'b> {
    /// Reads `bytes` as a static's entry: the form, the datum (`DatumView::read`), and each
    /// declared row.
    pub fn read(bytes: &'b [u8]) -> Result<StaticEntryView<'b>, EntryError> {
        let mut c = Cursor { bytes, at: 0 };
        if c.take(8, "magic")? != STATIC_ENTRY_MAGIC {
            return Err(EntryError::Magic);
        }
        let form = c.u32("form")?;
        if form != ENTRY_FORM {
            return Err(EntryError::Form { found: form });
        }
        let len = c.u32("datum's length")? as usize;
        let datum = DatumView::read(c.take(len, "datum")?).map_err(EntryError::Datum)?;
        let (declared, count) = c.declared_block()?;
        let statics = c.statics_block()?;
        if c.at != bytes.len() {
            return Err(EntryError::Trailing);
        }
        Ok(StaticEntryView { datum, declared, count, statics })
    }

    /// Each declared instance, in the entry's order, passed to `each`.
    pub fn each_declared<E>(&self, mut each: impl FnMut(Declared<'b>) -> Result<(), E>, malformed: impl Fn(EntryError) -> E) -> Result<(), E> {
        let mut c = Cursor { bytes: self.declared, at: 0 };
        for _ in 0..self.count {
            each(c.declared().map_err(&malformed)?)?;
        }
        Ok(())
    }

    /// Each static the datum's words name, by item key, ascending.
    pub fn each_static(&self, each: impl FnMut(ItemKey)) {
        each_key(self.statics, each);
    }
}

/// Each static an entry's RECORD names, by symbol, in its targets' order: the `Space::Static`
/// targets of the code record or the datum, which is what the run's child binds (run_mode.rs
/// `Binder::static_at`). It reads the record itself, not the entry's declared statics block, so
/// a static target the block does not list is named too: the thread-local descriptor
/// (blaze-emit `RelocTo::TlsDesc`, a `Space::Static` target of every record touching a
/// thread-local, which no `static` item backs). The run's parent sends nothing it has not
/// checked against this (blaze-cli run_parent.rs `Needs::publish`).
pub fn each_static_named(entry: &[u8], mut each: impl FnMut(Symbol)) -> Result<(), EntryError> {
    match entry.get(..8) {
        Some(magic) if magic == STATIC_ENTRY_MAGIC => {
            let datum = StaticEntryView::read(entry)?.datum;
            for i in 0..datum.targets() {
                let target = datum.target(i);
                if target.space == Space::Static {
                    each(target.symbol);
                }
            }
        }
        _ => {
            let record = super::EntryView::read(entry)?.record;
            for i in 0..record.targets() {
                let target = record.target(i);
                if target.space == Space::Static {
                    each(target.symbol);
                }
            }
        }
    }
    Ok(())
}

/// The record an entry of either kind holds: a code record's bytes, or a static's datum.
pub fn entry_record(entry: &[u8]) -> Result<&[u8], EntryError> {
    match entry.get(..8) {
        Some(magic) if magic == STATIC_ENTRY_MAGIC => Ok(StaticEntryView::read(entry)?.datum.encoded()),
        _ => Ok(super::EntryView::read(entry)?.record.bytes()),
    }
}
