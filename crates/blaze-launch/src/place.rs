//! A record made code at an address: every fixup written against what its place is in this
//! process ([`Bound`]), with the record's own *pool* after its code.
//!
//! The pool is what a fixup that goes through a word needs and that is not a slot word: a
//! cell that holds the address of a static, an outside symbol or a datum (the GOT entry of
//! code that is never linked), and a thread-local's descriptor. It lies in the body's own
//! range, so it is within reach of the code whatever the process's layout, is written before
//! any word names the body, and is freed with it. Its layout is a function of the record
//! alone ([`pool`]), so the daemon knows a body's placed length before it sends the bind.
//!
//! | pool entry | bytes | holds |
//! |---|---|---|
//! | a cell | 8 | the target's address |
//! | a Mach-O thread-local | 32 | a cell that holds the address of the descriptor after it: the thunk, its argument, a zero |
//! | an ELF thread-local descriptor | 16 | the thunk, its argument |
//!
//! Nothing here touches memory or knows a process: the driver resolves the places
//! (`blaze-launch-rt`), this module computes bytes.

use alloc::vec::Vec;
use core::alloc::Allocator;
use core::fmt;

use crate::record::{Datum, Fixup, Kind, Place, Record, Through, To, What};
use crate::wire::Arch;

/// What a place of a record is in this process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Bound {
    /// A function: the address of its slot word, and of its entry thunk.
    Function { word: u64, thunk: u64 },
    /// A static or an outside symbol: its address (0 for a weak symbol nothing defines).
    At(u64),
    /// A thread-local static: the launcher's thunk, which gives this thread's copy, and the
    /// argument that names the static to it.
    Tls { thunk: u64, arg: u64 },
}

/// Why a record cannot be placed: which fixup (by its offset in the bytes) and what of it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct PlaceError {
    /// The fixup's offset, when one is to blame.
    pub at: Option<u32>,
    pub what: &'static str,
    pub value: u64,
}

impl fmt::Display for PlaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some(at) => write!(f, "the fixup at offset {at}: {} ({:#x})", self.what, self.value),
            None => write!(f, "{} ({:#x})", self.what, self.value),
        }
    }
}

impl core::error::Error for PlaceError {}

fn refuse(fixup: &Fixup, what: &'static str, value: u64) -> PlaceError {
    PlaceError { at: Some(fixup.at), what, value }
}

/// One entry of a record's pool: what it is for, and its offset from the body's start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Entry {
    through: Through,
    to: To,
    at: u64,
}

/// The pool of a record, and so the body's placed length. A function of the record alone.
pub struct Pool<A: Allocator> {
    entries: Vec<Entry, A>,
    /// The body's length, placed: its code, then the pool.
    pub len: u64,
}

impl<A: Allocator> Pool<A> {
    fn entry(&self, through: Through, to: To) -> Option<u64> {
        self.entries.iter().find(|it| it.through == through && it.to == to).map(|it| it.at)
    }
}

/// Lays out the pool of `record`, a function's: one entry for each distinct thing a fixup
/// reaches through a word that is not a function's slot word, in the order of the fixups.
pub fn pool<A: Allocator + Clone>(record: &Record<'_>, alloc: A) -> Result<Pool<A>, PlaceError> {
    if record.what != What::Code {
        return Err(PlaceError { at: None, what: "a static's record placed as code", value: 0 });
    }
    // Which places are functions, read once: a list in a record is read item by item.
    // clone: the allocator's handle, for a second list of the same lifetime
    let mut functions = Vec::with_capacity_in(record.places.len(), alloc.clone());
    functions.extend(record.places.iter().map(|it| it == Place::Function));
    let mut pool = Pool { entries: Vec::new_in(alloc), len: record.size.next_multiple_of(8) };
    for fixup in record.fixups.iter() {
        let through = fixup.kind.through();
        let function = match fixup.to {
            To::Place(place) => functions.get(place as usize).copied().unwrap_or(false),
            To::Data(_) | To::Own => false,
        };
        let bytes = match through {
            Through::Direct => continue,
            // A function's GOT entry is its slot word.
            Through::Got if function => continue,
            Through::Got => 8,
            Through::Tlv => 32,
            Through::TlsDesc => 16,
        };
        if pool.entry(through, fixup.to).is_none() {
            pool.entries.push(Entry { through, to: fixup.to, at: pool.len });
            pool.len += bytes;
        }
    }
    Ok(pool)
}

/// What a word of a placed body holds that is an address as it is, 8 bytes, and so is
/// another number in another process ([`absolutes`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Abs {
    /// An address in the body itself: its code or its pool.
    Own,
    /// The address of what the record names: a function's is its entry thunk.
    Of(To),
    /// The launcher's thunk for thread-locals.
    TlsThunk,
    /// What names the thread-local static `To` is to that thunk.
    TlsArg(To),
}

/// Every word of the body [`place`] makes of `record` that holds an absolute address: its
/// offset in the body, and what the address is of. Everything else [`place`] writes is
/// relative to the field, and all of it stays in the launcher's one mapping (the body, a
/// slot's word, an entry thunk), so a body moved with that mapping differs in these words
/// alone. A fixup relative to anything outside the mapping (a static, a datum or an outside
/// symbol reached with no word between) is refused: such a body cannot be moved.
pub fn absolutes<A: Allocator>(
    record: &Record<'_>,
    pool: &Pool<A>,
    mut each: impl FnMut(u64, Abs),
) -> Result<(), PlaceError> {
    for entry in pool.entries.iter() {
        match entry.through {
            Through::Direct => {}
            Through::Got => each(
                entry.at,
                match entry.to {
                    To::Own => Abs::Own,
                    to => Abs::Of(to),
                },
            ),
            // The cell holds the address of the descriptor after it.
            Through::Tlv => {
                each(entry.at, Abs::Own);
                each(entry.at + 8, Abs::TlsThunk);
                each(entry.at + 16, Abs::TlsArg(entry.to));
            }
            Through::TlsDesc => {
                each(entry.at, Abs::TlsThunk);
                each(entry.at + 8, Abs::TlsArg(entry.to));
            }
        }
    }
    for fixup in record.fixups.iter() {
        match (fixup.kind, fixup.kind.through()) {
            (Kind::Abs64, _) => each(
                u64::from(fixup.at),
                match fixup.to {
                    To::Own => Abs::Own,
                    to => Abs::Of(to),
                },
            ),
            (_, Through::Direct) => {
                let within = match fixup.to {
                    To::Own => true,
                    To::Data(_) => false,
                    To::Place(place) => {
                        record.places.iter().nth(place as usize) == Some(Place::Function)
                    }
                };
                if !within {
                    let what = "is relative to something outside the launcher's code mapping";
                    return Err(refuse(&fixup, what, fixup.kind as u64));
                }
            }
            // Through a pool entry or a slot's word: in the mapping.
            _ => {}
        }
    }
    Ok(())
}

/// The address a fixup against `to` names when it names the target itself.
fn address(
    fixup: &Fixup,
    to: To,
    at: u64,
    bounds: &[Bound],
    data: &[u64],
) -> Result<u64, PlaceError> {
    match to {
        To::Own => Ok(at),
        To::Data(datum) => data
            .get(datum as usize)
            .copied()
            .ok_or_else(|| refuse(fixup, "against a datum that was not placed", u64::from(datum))),
        To::Place(place) => match bounds.get(place as usize) {
            Some(Bound::Function { thunk, .. }) => Ok(*thunk),
            Some(Bound::At(address)) => Ok(*address),
            Some(Bound::Tls { .. }) => Err(refuse(
                fixup,
                "takes the address of a thread-local, which only its descriptor gives",
                u64::from(place),
            )),
            None => Err(refuse(fixup, "against a place the bind gave nothing for", u64::from(place))),
        },
    }
}

/// The thunk and argument of the thread-local a fixup is against.
fn thread_local(fixup: &Fixup, bounds: &[Bound]) -> Result<(u64, u64), PlaceError> {
    match fixup.to {
        To::Place(place) => match bounds.get(place as usize) {
            Some(Bound::Tls { thunk, arg }) => Ok((*thunk, *arg)),
            _ => Err(refuse(
                fixup,
                "a thread-local access of a place that is no thread-local static",
                u64::from(place),
            )),
        },
        To::Data(_) | To::Own => {
            Err(refuse(fixup, "a thread-local access of the record's own bytes", 0))
        }
    }
}

/// Clears `out` and fills it with the body of `record` as it must be at address `at`: its
/// code with every fixup written, then its pool (`pool`, [`pool`] of the same record).
/// `bounds` gives what each place is, `data` the address each datum was placed at.
pub fn place<A: Allocator, B: Allocator>(
    record: &Record<'_>,
    pool: &Pool<A>,
    at: u64,
    bounds: &[Bound],
    data: &[u64],
    out: &mut Vec<u8, B>,
) -> Result<(), PlaceError> {
    if bounds.len() != record.places.len() {
        let what = "the record's places and what the bind gave for them differ in number";
        return Err(PlaceError { at: None, what, value: bounds.len() as u64 });
    }
    if data.len() != record.data.len() {
        let what = "the record's data and the addresses they were placed at differ in number";
        return Err(PlaceError { at: None, what, value: data.len() as u64 });
    }
    out.clear();
    out.extend_from_slice(record.bytes);
    out.resize(pool.len as usize, 0);
    // The pool first: a fixup's field then names an entry that is already whole.
    for fixup in record.fixups.iter() {
        let through = fixup.kind.through();
        let Some(entry) = pool.entry(through, fixup.to) else { continue };
        let words: [u64; 3] = match through {
            Through::Direct => continue,
            Through::Got => [address(&fixup, fixup.to, at, bounds, data)?, 0, 0],
            Through::Tlv | Through::TlsDesc => {
                let (thunk, arg) = thread_local(&fixup, bounds)?;
                [thunk, arg, 0]
            }
        };
        let mut put = |offset: u64, word: u64| {
            let offset = offset as usize;
            out[offset..offset + 8].copy_from_slice(&word.to_le_bytes());
        };
        match through {
            Through::Direct => {}
            Through::Got => put(entry, words[0]),
            // The cell, which holds the address of the descriptor after it.
            Through::Tlv => {
                put(entry, at + entry + 8);
                put(entry + 8, words[0]);
                put(entry + 16, words[1]);
                put(entry + 24, 0);
            }
            Through::TlsDesc => {
                put(entry, words[0]);
                put(entry + 8, words[1]);
            }
        }
    }
    for fixup in record.fixups.iter() {
        let through = fixup.kind.through();
        let target = match (through, pool.entry(through, fixup.to)) {
            (Through::Direct, _) => address(&fixup, fixup.to, at, bounds, data)?,
            (_, Some(entry)) => at + entry,
            // Through the GOT with no pool entry: a function's slot word.
            (_, None) => match fixup.to {
                To::Place(place) => match bounds.get(place as usize) {
                    Some(Bound::Function { word, .. }) => *word,
                    _ => {
                        let what = "goes through a word and the pool has none for it";
                        return Err(refuse(&fixup, what, u64::from(place)));
                    }
                },
                _ => return Err(refuse(&fixup, "goes through a word and the pool has none", 0)),
            },
        };
        let field = &mut out[fixup.at as usize..];
        let value = target.wrapping_add(fixup.addend as u64);
        patch(record.arch, &fixup, field, value, at + u64::from(fixup.at))?;
    }
    Ok(())
}

/// Clears `out` and fills it with the bytes of `datum` (or of a static's `record`, through
/// [`place_static`]) as they must be anywhere: each fixup is an absolute address.
pub fn place_datum<B: Allocator>(
    datum: &Datum<'_>,
    bounds: &[Bound],
    data: &[u64],
    out: &mut Vec<u8, B>,
) -> Result<(), PlaceError> {
    out.clear();
    out.extend_from_slice(datum.bytes);
    absolute(datum.fixups.iter(), bounds, data, out)
}

/// Clears `out` and fills it with the initial value of the static `record` is of: its bytes,
/// or its size in zeroes, with each fixup (an absolute address) written.
pub fn place_static<B: Allocator>(
    record: &Record<'_>,
    bounds: &[Bound],
    data: &[u64],
    out: &mut Vec<u8, B>,
) -> Result<(), PlaceError> {
    if record.what == What::Code {
        return Err(PlaceError { at: None, what: "a function's record placed as a static", value: 0 });
    }
    if bounds.len() != record.places.len() || data.len() != record.data.len() {
        let what = "the static's places or data and what was given for them differ in number";
        return Err(PlaceError { at: None, what, value: bounds.len() as u64 });
    }
    let Ok(size) = usize::try_from(record.size) else {
        return Err(PlaceError { at: None, what: "a static past the address space", value: record.size });
    };
    out.clear();
    out.extend_from_slice(record.bytes);
    out.resize(size, 0);
    absolute(record.fixups.iter(), bounds, data, out)
}

fn absolute<B: Allocator>(
    fixups: impl Iterator<Item = Fixup>,
    bounds: &[Bound],
    data: &[u64],
    out: &mut Vec<u8, B>,
) -> Result<(), PlaceError> {
    for fixup in fixups {
        if fixup.kind != Kind::Abs64 || fixup.to == To::Own {
            return Err(refuse(&fixup, "in data, and not the absolute address of a place or a datum", 0));
        }
        let value = address(&fixup, fixup.to, 0, bounds, data)?.wrapping_add(fixup.addend as u64);
        let Some(field) = out.get_mut(fixup.at as usize..fixup.at as usize + 8) else {
            return Err(refuse(&fixup, "past the end of the bytes", 0));
        };
        field.copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}

fn read32(field: &[u8]) -> u32 {
    u32::from_le_bytes([field[0], field[1], field[2], field[3]])
}

fn write32(field: &mut [u8], word: u32) {
    field[..4].copy_from_slice(&word.to_le_bytes());
}

/// Writes one fixup's field: `value` is the address it names with the addend added, `p` the
/// field's own address. `field` begins at the field and has its width at least
/// (`Record::decode` checked it).
fn patch(arch: Arch, fixup: &Fixup, field: &mut [u8], value: u64, p: u64) -> Result<(), PlaceError> {
    let relative = value.wrapping_sub(p) as i64;
    let x86 = arch == Arch::X86_64;
    match fixup.kind {
        Kind::Abs64 => field[..8].copy_from_slice(&value.to_le_bytes()),
        Kind::PcRel32 | Kind::GotPcRel32 | Kind::TlvRel32 if x86 => match i32::try_from(relative) {
            Ok(relative) => field[..4].copy_from_slice(&relative.to_le_bytes()),
            Err(_) => return Err(refuse(fixup, "its target is past 2 GiB of it", value)),
        },
        Kind::Call26 if !x86 => {
            if relative & 3 != 0 || !(-(1 << 27)..1 << 27).contains(&relative) {
                return Err(refuse(fixup, "a branch whose target is past 128 MiB of it", value));
            }
            let word = read32(field);
            write32(field, (word & !0x03ff_ffff) | (((relative >> 2) as u32) & 0x03ff_ffff));
        }
        Kind::AdrpPage21 | Kind::GotPage21 | Kind::TlvPage21 | Kind::TlsDescPage21 if !x86 => {
            let pages = ((value & !0xfff) as i64).wrapping_sub((p & !0xfff) as i64) >> 12;
            if !(-(1 << 20)..1 << 20).contains(&pages) {
                return Err(refuse(fixup, "an `adrp` whose target is past 4 GiB of it", value));
            }
            let (word, pages) = (read32(field), pages as u32 & 0x1f_ffff);
            let cleared = word & !((3 << 29) | (0x7_ffff << 5));
            write32(field, cleared | ((pages & 3) << 29) | ((pages >> 2) << 5));
        }
        Kind::PageOffset12
        | Kind::GotPageOffset12
        | Kind::TlvPageOffset12
        | Kind::TlsDescLd64Lo12
        | Kind::TlsDescAddLo12
            if !x86 =>
        {
            let word = read32(field);
            // The scale: 0 for `add`, the access size's log for `ldr`/`str` (unsigned offset).
            let shift = if word & 0x1f00_0000 == 0x1100_0000 {
                0
            } else if word & 0x3b00_0000 == 0x3900_0000 {
                let (size, vector, opc) = (word >> 30, word & (1 << 26) != 0, (word >> 22) & 3);
                if vector && size == 0 && opc >= 2 { 4 } else { size }
            } else {
                let what = "a low-12-bits field in an instruction that is no `add`, `ldr` or `str`";
                return Err(refuse(fixup, what, u64::from(word)));
            };
            let low = value & 0xfff;
            if low & ((1 << shift) - 1) != 0 {
                let what = "a load whose target is not aligned to the size it loads";
                return Err(refuse(fixup, what, value));
            }
            write32(field, (word & !(0xfff << 10)) | (((low >> shift) as u32) << 10));
        }
        // The call's marker: nothing is written.
        Kind::TlsDescCall if !x86 => {}
        _ => {
            let what = "a kind the record's instruction set does not have";
            return Err(refuse(fixup, what, fixup.kind as u64));
        }
    }
    Ok(())
}
