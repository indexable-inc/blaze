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
//! - `Fn`: a `Space::Func` target, named by its instance symbol, and a declared instance row so
//!   the parent gives its slot before the datum is placed;
//! - `Vtable`: refused by name until the evaluator carries its table datum.
//!
//! THE PLACE: read-only for a static that is neither `static mut` nor holds an allocation the
//! evaluator marked mutable (interior mutability: data.rs `DataAlloc::mutable`); writable
//! otherwise. A `#[thread_local]`
//! static is no static's datum: reached as a static it is refused by name; its initial value is
//! a TEMPLATE in the same form ([`make_template_entry`]), always read-only, under its `Tls`
//! symbol, from which each thread's copy is made after the child fills its words.
//!
//! THE FORM, little-endian: [`STATIC_ENTRY_MAGIC`], [`super::ENTRY_FORM`], the datum's
//! length (4) and bytes, the declared instances' block as a code entry's (count 4, then rows:
//! each tabled fn key, name length and canonical instance name), and the declared statics' block (count 4, then item keys 16 each,
//! ascending, each once). The KEY is a walk row's (body_ids.rs `record_key`: the static's
//! symbol, its initializer's BodyCheck key and reads, its crate's fact), so an edit of the
//! initializer moves this record's key alone: code names the static's word, not its value.

use core::fmt;

use blaze_build::{InstanceKey, StaticItem};
use blaze_build::data::DataTarget;
use blaze_code::datum::{Datum, DatumError, DatumView, OWN_DATUM, Place};
use blaze_code::record::{Space, Symbol, Target, Word};
use blaze_own::{AVec, Alloc, Vec};
use blaze_typeck::ItemKey;

use super::{Cursor, Declared, ENTRY_FORM, EntryError, each_key, put_statics};
use crate::seam::SeamData;
use crate::symbols::{SymbolError, fn_symbol_of_name, static_symbol};

/// The first eight bytes of a static's entry (a code record's is [`super::ENTRY_MAGIC`]).
pub const STATIC_ENTRY_MAGIC: [u8; 8] = *b"bzsent01";

/// Why a static has no entry, naming what of its value is refused.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StaticRecordError {
    /// A `#[thread_local]` static: the thread's template, not a datum.
    ThreadLocal,
    /// The evaluator's record has no allocation.
    Empty,
    /// The word at `at` of allocation `alloc` names a fn instance whose metadata is unavailable.
    Fn { alloc: u32, at: u32 },
    /// The fn instance at this word has no encodable ground name.
    FnName { alloc: u32, at: u32, error: SymbolError },
    /// The word at `at` of allocation `alloc` names a vtable, whose datum is unavailable.
    Vtable { alloc: u32, at: u32 },
    /// Relocation `reloc` of allocation `alloc` is past its bytes, or names an allocation past
    /// the record's.
    Reloc { alloc: u32, reloc: u32 },
    /// The datum is larger than its form counts (`what`).
    TooLarge { what: &'static str },
    /// The datum did not seal (blaze-code datum.rs names the rule).
    Datum(DatumError),
    /// A template asked of a static that is not `#[thread_local]`.
    NotThreadLocal,
    /// A thread-local's value holds, at byte `at` of allocation `alloc`, its own address.
    SelfPointer { alloc: u32, at: u32 },
    /// A thread-local's value points into allocation `alloc`, which the evaluator marked
    /// mutable: every thread's copy would share it.
    SharedMutable { alloc: u32 },
}

impl fmt::Display for StaticRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let spec = "run.md, \"Statics in the run\"";
        match *self {
            StaticRecordError::ThreadLocal => write!(f, "a `#[thread_local]` static is the thread's template, made with the thread-locals, not a static's datum ({spec}, step 4)"),
            StaticRecordError::Empty => write!(f, "the constant evaluator's record of its initializer holds no allocation (blaze-build data.rs: `allocs[0]` is the value)"),
            StaticRecordError::Fn { alloc, at } => write!(f, "its value's allocation {alloc} holds a fn's address at byte {at}, but the fn instance's symbol and name are unavailable ({spec}, step 2)"),
            StaticRecordError::FnName { alloc, at, error } => write!(f, "its value's allocation {alloc} holds a fn's address at byte {at}: {error}"),
            StaticRecordError::Vtable { alloc, at } => write!(f, "its value's allocation {alloc} holds a vtable's address at byte {at}, but its datum is unavailable ({spec}, step 2)"),
            StaticRecordError::Reloc { alloc, reloc } => write!(f, "relocation {reloc} of its value's allocation {alloc} is past its bytes or names no allocation of the record (blaze-build data.rs, \"The form\")"),
            StaticRecordError::TooLarge { what } => write!(f, "its {what} is larger than a static's entry counts"),
            StaticRecordError::Datum(why) => write!(f, "its datum: {why}"),
            StaticRecordError::NotThreadLocal => write!(f, "a thread-local's template was asked of a static that is not `#[thread_local]` ({spec}, step 4)"),
            StaticRecordError::SelfPointer { alloc, at } => write!(f, "its initial value's allocation {alloc} holds the thread-local's own address at byte {at}: each thread's copy would point at the template, not at itself ({spec}, step 4)"),
            StaticRecordError::SharedMutable { alloc } => write!(f, "its initial value points into allocation {alloc}, which holds interior mutability: every thread's copy would share it; per-thread copies of inner allocations are not made until a program needs them ({spec}, step 4)"),
        }
    }
}

impl core::error::Error for StaticRecordError {}

/// A static's entry from the evaluator's record of its initializer, `value`, and its kind (the
/// module doc).
pub fn make_static_entry<'a>(value: &SeamData, kind: StaticItem, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, StaticRecordError> {
    make_static_entry_with_targets(value, kind, |_| Ok(None), alloc)
}

/// A static entry with the function-instance metadata needed by `DataTarget::Fn` words.
pub fn make_static_entry_with_targets<'a, F>(
    value: &SeamData,
    kind: StaticItem,
    mut fn_target: F,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, StaticRecordError>
where
    F: FnMut(InstanceKey) -> Result<Option<(ItemKey, AVec<'a, u8>)>, SymbolError>,
{
    if kind.thread_local {
        return Err(StaticRecordError::ThreadLocal);
    }
    let place = match kind.mutable || value.allocs.iter().any(|a| a.mutable) {
        true => Place::Writable,
        false => Place::ReadOnly,
    };
    datum_entry(value, place, false, &mut fn_target, alloc)
}

/// A `#[thread_local]` static's TEMPLATE entry (run.md, "Statics in the run", step 4): its
/// initial value as a read-only datum in a static's entry form, placed once by the run's child
/// under the thread-local's symbol (symbols.rs `tls_symbol`); each thread's copy is made from it
/// on the thread's first touch (blaze-build `tls::index::getter`). The copy is the whole datum,
/// so a word of the value naming one of its other allocations (a `&str`'s bytes) names the
/// template's, shared by every thread, which is right only while that allocation is immutable:
/// a mutable one is refused, and so is a word naming the value's own bytes (each copy would
/// point at the template, not at itself).
pub fn make_template_entry<'a>(value: &SeamData, kind: StaticItem, alloc: Alloc<'a>) -> Result<AVec<'a, u8>, StaticRecordError> {
    make_template_entry_with_targets(value, kind, |_| Ok(None), alloc)
}

/// A thread-local template with the function-instance metadata needed by `DataTarget::Fn` words.
pub fn make_template_entry_with_targets<'a, F>(
    value: &SeamData,
    kind: StaticItem,
    mut fn_target: F,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, StaticRecordError>
where
    F: FnMut(InstanceKey) -> Result<Option<(ItemKey, AVec<'a, u8>)>, SymbolError>,
{
    if !kind.thread_local {
        return Err(StaticRecordError::NotThreadLocal);
    }
    if let Some(n) = value.allocs.iter().skip(1).position(|a| a.mutable) {
        return Err(StaticRecordError::SharedMutable { alloc: n as u32 + 1 });
    }
    datum_entry(value, Place::ReadOnly, true, &mut fn_target, alloc)
}

/// The entry of `value` as a datum at `place`; `template`: a word naming allocation 0 is refused
/// ([`StaticRecordError::SelfPointer`]).
fn datum_entry<'a, F>(
    value: &SeamData,
    place: Place,
    template: bool,
    fn_target: &mut F,
    alloc: Alloc<'a>,
) -> Result<AVec<'a, u8>, StaticRecordError>
where
    F: FnMut(InstanceKey) -> Result<Option<(ItemKey, AVec<'a, u8>)>, SymbolError>,
{
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
    let mut d = Datum::new_in(place, align, alloc);
    d.bytes.resize(len, 0);
    for (n, a) in value.allocs.iter().enumerate() {
        let at = starts[n] as usize;
        d.bytes[at..at + a.bytes.len()].copy_from_slice(&a.bytes);
    }
    let mut statics: AVec<'_, ItemKey> = Vec::new_in(alloc);
    let mut declared: AVec<'_, (ItemKey, AVec<'_, u8>)> = Vec::new_in(alloc);
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
                DataTarget::Local(0) if template => {
                    return Err(StaticRecordError::SelfPointer { alloc: n as u32, at: reloc.offset });
                }
                DataTarget::Local(m) => {
                    let start = *starts.get(m as usize).ok_or(refused)?;
                    (OWN_DATUM, addend.checked_add(start as i64).ok_or(refused)?)
                }
                DataTarget::Static(k) => {
                    statics.push(k);
                    (d.target(Target { space: Space::Static, symbol: static_symbol(k) }), addend)
                }
                DataTarget::Fn(key) => {
                    let (tabled, name) = fn_target(key)
                        .map_err(|error| StaticRecordError::FnName { alloc: n as u32, at: reloc.offset, error })?
                        .ok_or(StaticRecordError::Fn { alloc: n as u32, at: reloc.offset })?;
                    let symbol = fn_symbol_of_name(&name);
                    declared.push((tabled, name));
                    (d.target(Target { space: Space::Func, symbol }), addend)
                }
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
    declared.sort_unstable_by(|a, b| (a.0, a.1.as_slice()).cmp(&(b.0, b.1.as_slice())));
    declared.dedup_by(|a, b| a.0 == b.0 && a.1.as_slice() == b.1.as_slice());
    out.extend_from_slice(&u32::try_from(declared.len()).map_err(|_| too_large("declared instances"))?.to_le_bytes());
    for (tabled, name) in declared.iter() {
        out.extend_from_slice(&tabled.0.to_le_bytes());
        out.extend_from_slice(&u32::try_from(name.len()).map_err(|_| too_large("declared instance's name"))?.to_le_bytes());
        out.extend_from_slice(name.as_slice());
    }
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
/// thread-local, which no `static` item backs). A `Space::Tls` target is named too: the child
/// numbers a thread-local from its template, the datum the parent sends under the same symbol.
/// The run's parent publishes no record before checking each of these was sent (blaze-cli
/// run_parent.rs `Needs::publish`). A `Space::Vtable` target is named too, as is each target
/// of that kind in the datum of a vtable the code entry carries (a supertrait's table): the
/// child binds a vtable to its datum, sent as a static's is (form 6, [`each_vtable_datum`]).
pub fn each_static_named(entry: &[u8], mut each: impl FnMut(Symbol)) -> Result<(), EntryError> {
    let sent = |space| matches!(space, Space::Static | Space::Tls | Space::Vtable);
    match entry.get(..8) {
        Some(magic) if magic == STATIC_ENTRY_MAGIC => {
            let datum = StaticEntryView::read(entry)?.datum;
            for i in 0..datum.targets() {
                let target = datum.target(i);
                if sent(target.space) {
                    each(target.symbol);
                }
            }
        }
        _ => {
            let view = super::EntryView::read(entry)?;
            let record = view.record;
            for i in 0..record.targets() {
                let target = record.target(i);
                if sent(target.space) {
                    each(target.symbol);
                }
            }
            view.each_vtable(
                |_, datum| {
                    for i in 0..datum.targets() {
                        let target = datum.target(i);
                        if sent(target.space) {
                            each(target.symbol);
                        }
                    }
                    Ok(())
                },
                |why| why,
            )?;
        }
    }
    Ok(())
}

/// Each vtable a code entry carries (form 6): its symbol and its encoded datum, what the run's
/// parent files and sends in the record's `Data` (blaze-cli run_parent.rs `Lazy::resolve`). A
/// static's entry carries none.
pub fn each_vtable_datum<'e, E>(entry: &'e [u8], mut each: impl FnMut(Symbol, &'e [u8]) -> Result<(), E>, malformed: impl Fn(EntryError) -> E) -> Result<(), E> {
    if entry.get(..8).is_some_and(|magic| magic == STATIC_ENTRY_MAGIC) {
        return Ok(());
    }
    let view = super::EntryView::read(entry).map_err(&malformed)?;
    view.each_vtable(|symbol, datum| each(symbol, datum.encoded()), malformed)
}

/// The record an entry of either kind holds: a code record's bytes, or a static's datum.
pub fn entry_record(entry: &[u8]) -> Result<&[u8], EntryError> {
    match entry.get(..8) {
        Some(magic) if magic == STATIC_ENTRY_MAGIC => Ok(StaticEntryView::read(entry)?.datum.encoded()),
        _ => Ok(super::EntryView::read(entry)?.record.bytes()),
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Global;

    use blaze_build::StaticItem;
    use blaze_build::data::{DataAlloc, DataRecord, DataReloc, DataTarget};
    use blaze_code::datum::{OWN_DATUM, Place};
    use blaze_code::record::{Space, Target, Word};
    use blaze_typeck::ItemKey;

    use super::{StaticEntryView, StaticRecordError, make_static_entry, make_static_entry_with_targets, make_template_entry, make_template_entry_with_targets};

    /// `#[thread_local] static V: (&str, u32) = ("hi", 7)`'s evaluated value: allocation 0 the
    /// value (a pointer word to allocation 1, its length, the u32), allocation 1 the bytes "hi";
    /// `inner_mutable` marks allocation 1 mutable, `target` is the pointer's target.
    fn value(inner_mutable: bool, target: DataTarget) -> DataRecord<Global> {
        let mut own = std::vec::Vec::new_in(Global);
        own.extend_from_slice(&0u64.to_le_bytes());
        own.extend_from_slice(&2u64.to_le_bytes());
        own.extend_from_slice(&7u64.to_le_bytes());
        let mut relocs = std::vec::Vec::new_in(Global);
        relocs.push(DataReloc { offset: 0, target });
        let mut hi = std::vec::Vec::new_in(Global);
        hi.extend_from_slice(b"hi");
        let mut allocs = std::vec::Vec::new_in(Global);
        allocs.push(DataAlloc { bytes: own, align: 3, mutable: false, relocs });
        allocs.push(DataAlloc { bytes: hi, align: 0, mutable: inner_mutable, relocs: std::vec::Vec::new_in(Global) });
        DataRecord { allocs }
    }

    /// Asserts a thread-local's template (run.md, "Statics in the run", step 4): its value laid
    /// out as one read-only datum, the pointer to its string an own word at the string's offset
    /// (24), the same form a static's datum has. A thread-local asked as a STATIC stays refused
    /// by name (`ThreadLocal`, whose text says the thread-locals make it). CONTROLS: a mutable
    /// inner allocation (every thread's copy would share it) is `SharedMutable { alloc: 1 }`; a
    /// word naming the value's own bytes (each copy would point at the template) is
    /// `SelfPointer`; a template asked of a plain static is `NotThreadLocal`.
    /// Prints "template: read-only, 26 bytes, one own word at +24; a thread-local as a static, a shared mutable, a self pointer and a plain static refused".
    #[test]
    fn a_thread_local_is_a_read_only_template_and_never_a_static() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let tls = StaticItem { thread_local: true, mutable: false };
        let made = make_template_entry(&value(false, DataTarget::Local(1)), tls, alloc).expect("the template");
        let view = StaticEntryView::read(&made).expect("the template's entry reads as a static's");
        assert_eq!((view.datum.place(), view.datum.bytes().len(), view.datum.words()), (Place::ReadOnly, 26, 1));
        let word = view.datum.word(0);
        assert_eq!((word.at, word.target, word.addend), (0, OWN_DATUM, 24), "the pointer names the string inside the datum");
        assert_eq!(&view.datum.bytes()[24..], b"hi");

        let as_static = make_static_entry(&value(false, DataTarget::Local(1)), tls, alloc).map(|_| ());
        assert_eq!(as_static, Err(StaticRecordError::ThreadLocal));
        let shown = std::format!("{}", StaticRecordError::ThreadLocal);
        assert!(shown.contains("thread_local") && shown.contains("template"), "{shown}");

        let shared = make_template_entry(&value(true, DataTarget::Local(1)), tls, alloc).map(|_| ());
        assert_eq!(shared, Err(StaticRecordError::SharedMutable { alloc: 1 }));
        let selfish = make_template_entry(&value(false, DataTarget::Local(0)), tls, alloc).map(|_| ());
        assert_eq!(selfish, Err(StaticRecordError::SelfPointer { alloc: 0, at: 0 }));
        let plain = make_template_entry(&value(false, DataTarget::Local(1)), StaticItem { thread_local: false, mutable: false }, alloc).map(|_| ());
        assert_eq!(plain, Err(StaticRecordError::NotThreadLocal));
        std::println!("template: read-only, 26 bytes, one own word at +24; a thread-local as a static, a shared mutable, a self pointer and a plain static refused");
    }

    #[test]
    fn a_fn_word_names_its_slot_and_declares_its_instance() {
        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let alloc = erased.handle();
        let key = blaze_build::InstanceKey(blaze_ids::ContentId([7; 32]));
        let value = value(false, DataTarget::Fn(key));
        // The canonical name of fn 11 with no generic arguments.
        let item = ItemKey(11);
        let mut expected = [0; 20];
        expected[..16].copy_from_slice(&item.0.to_le_bytes());
        for thread_local in [false, true] {
            let kind = StaticItem { thread_local, mutable: false };
            let missing = if thread_local {
                make_template_entry(&value, kind, alloc)
            } else {
                make_static_entry(&value, kind, alloc)
            };
            assert_eq!(missing.map(|_| ()), Err(StaticRecordError::Fn { alloc: 0, at: 0 }));
            let target = |asked| {
                assert_eq!(asked, key);
                let mut name = blaze_own::Vec::new_in(alloc);
                name.extend_from_slice(&expected);
                Ok(Some((item, name)))
            };
            let made = if thread_local {
                make_template_entry_with_targets(&value, kind, target, alloc)
            } else {
                make_static_entry_with_targets(&value, kind, target, alloc)
            }.expect("a fn word has a place in both kinds of datum");
            let view = StaticEntryView::read(&made).expect("the entry reads");
            assert_eq!(view.datum.place(), Place::ReadOnly);
            assert_eq!((view.datum.words(), view.datum.targets()), (1, 1));
            assert_eq!(view.datum.word(0), Word { at: 0, target: 0, addend: 0 });
            assert_eq!(view.datum.target(0), Target { space: Space::Func, symbol: crate::symbols::fn_symbol_of_name(&expected) });
            let mut declared = std::vec::Vec::new();
            view.each_declared(|row| {
                declared.push((row.tabled, row.name));
                Ok::<(), ()>(())
            }, |_| ()).expect("the declared instance reads");
            assert_eq!(declared, [(item, expected.as_slice())]);
        }
        std::println!("a static fn word names Space::Func and declares its instance name");
    }

    #[test]
    fn filling_nested_string_slices_preserves_both_interior_addends() {
        use blaze_code::{datum, image::ImageError, place::Words};
        use crate::symbols::static_symbol;

        const OWN: u64 = 0x2000;
        const OTHER: u64 = 0x8000;
        const OTHER_ITEM: ItemKey = ItemKey(17);
        const OTHER_BYTES: &[u8] = b"---cdef!";

        struct OtherStatic;
        impl Words for OtherStatic {
            fn word(&mut self, target: Target, _: bool) -> Result<u64, ImageError> {
                Err(ImageError::Unbound { target })
            }
            fn value(&mut self, target: Target) -> Result<u64, ImageError> {
                assert_eq!(target, Target { space: Space::Static, symbol: static_symbol(OTHER_ITEM) });
                Ok(OTHER)
            }
        }

        let frame = blaze_mem::Frame::new(blaze_mem::tag::Tag::Infer).expect("the test's frame");
        let erased = blaze_own::Erased::new(&frame);
        let words = |values: &[u64]| values.iter().flat_map(|n| n.to_le_bytes()).collect();
        let fill = |second_offset: u64| {
            // The root slice points at two string slices. The first borrows from
            // within this datum, the second from the middle of another static.
            let value = DataRecord { allocs: vec![
                DataAlloc { align: 3, mutable: false, bytes: words(&[0, 2]),
                    relocs: vec![DataReloc { offset: 0, target: DataTarget::Local(1) }] },
                DataAlloc { align: 3, mutable: false, bytes: words(&[1, 2, second_offset, 4]),
                    relocs: vec![DataReloc { offset: 0, target: DataTarget::Local(2) },
                        DataReloc { offset: 16, target: DataTarget::Static(OTHER_ITEM) }] },
                DataAlloc { align: 0, mutable: false, bytes: b"/ab".to_vec(), relocs: vec![] },
            ] };
            let entry = make_static_entry(&value, StaticItem { mutable: false, thread_local: false }, erased.handle()).expect("the slice's entry");
            let datum = StaticEntryView::read(&entry).expect("the slice's datum").datum;
            let mut bytes = vec![0; datum.bytes().len()];
            datum::fill(&datum, static_symbol(ItemKey(23)), OWN, &mut OtherStatic, &mut bytes).expect("place the slice");
            bytes
        };
        let second_is_cdef = |bytes: &[u8]| {
            let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
            assert_eq!((word(0), word(8)), (OWN + 16, 2), "the outer slice names its two elements");
            assert_eq!((word(16), word(24)), (OWN + 48 + 1, 2), "the first string starts inside the local allocation");
            assert_eq!(&bytes[49..51], b"ab");
            assert_eq!(word(40), 4, "placing the second pointer preserves its adjacent length");
            let offset = word(32).checked_sub(OTHER).and_then(|n| usize::try_from(n).ok());
            offset.and_then(|i| OTHER_BYTES.get(i..)?.get(..4)) == Some(&b"cdef"[..])
        };
        assert!(second_is_cdef(&fill(3)), "the second string starts three bytes into its static");
        assert!(!second_is_cdef(&fill(4)), "a one-byte displacement must change the string");
        std::println!("nested slices: local +1 and external +3 preserved; external +4 caught");
    }
}
