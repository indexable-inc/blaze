//! One static's data record, a DATUM (docs/v2/plan/design/run.md, "Statics in the run"): the
//! value blaze-export's `Data` takes and the run's child places before any code names it.
//!
//! - `place`: where it lives once the program runs ([`Place`]): read-only (an immutable static
//!   with no interior mutability), writable (`static mut`, a type with `UnsafeCell`), or
//!   writable and all zero with no bytes kept.
//! - `align`: log2 of its alignment, at most [`MOST_ALIGN`] (a 4 KiB page).
//! - `bytes`: its first bytes; the value's other allocations (a slice or string literal it
//!   points at) are laid out after them by the maker, each a [`OWN_DATUM`] word away.
//! - `targets`: what its words name, by (space, symbol) as a code record's do: a function
//!   ([`Space::Func`]: its entry thunk), a static ([`Space::Static`]: its first byte), a
//!   vtable ([`Space::Vtable`]: its first word) or a C name ([`Space::Outside`]: the address
//!   the run binds the name to, a C table of function pointers in C code's data). A
//!   thread-local is refused by name ([`DatumError::Space`]) until it is built.
//! - `words`: 8-byte words of the bytes the placement fills: the address of `target` plus
//!   `addend`, or of the datum's own byte `addend` ([`OWN_DATUM`]).
//! - `names`: each C name target's name, in target order ([`Datum::outside`] adds one): the
//!   run binds a C name by its text, and a datum is read without the code record that made
//!   it, so the datum keeps the text itself, checked once on read to hash to its symbol
//!   ([`Symbol::of_name`]). A datum naming no C name has no names section: its bytes are those
//!   of the form before C names (`bzdata01`, the fourth count 0).
//!
//! [`Datum`] is the builder; [`Datum::seal`] checks it and puts it in its one canonical form
//! ([`SealedDatum`]), which alone encodes, so two builders write equal bytes (rule 25).
//! [`DatumView`] reads an encoded datum in place, checking the same rules once on bytes from a
//! store (rule 74). [`fill`] makes the placed bytes at an address against the run's words
//! ([`crate::place::Words`], the same seam a code record's placement asks).

use core::fmt;

use blaze_own::{AVec, Alloc, Vec};

use crate::image::ImageError;
use crate::place::Words;
use crate::record::{Space, Symbol, Target, Word};

/// The first eight bytes of every encoded datum.
pub const DATUM_MAGIC: [u8; 8] = *b"bzdata01";

/// A [`Word`]'s target meaning this datum's own byte `addend`.
pub const OWN_DATUM: u32 = u32::MAX;

/// The largest alignment a datum asks, as log2: a 4 KiB page (blaze-build data.rs `MOST_ALIGN`).
pub const MOST_ALIGN: u8 = 12;

/// The bytes before the sections: magic, place, align, six zero, length, four counts.
const HEAD: usize = 40;
const TARGET_ROW: usize = 40;
const WORD_ROW: usize = 16;
/// One C name's row in the names section: its text's start and length in the section's text.
const NAME_ROW: usize = 8;

/// Where a datum lives once the program runs (blaze-export input.rs `DataPlace`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Place {
    /// Never written by the program: an immutable static with no interior mutability.
    ReadOnly,
    /// Written by the program: `static mut`, a static whose type holds an `UnsafeCell`.
    Writable,
    /// Writable and every byte zero at start, `len` bytes: no bytes and no words kept.
    Zeroed { len: u64 },
}

impl Place {
    fn byte(self) -> u8 {
        match self {
            Place::ReadOnly => 0,
            Place::Writable => 1,
            Place::Zeroed { .. } => 2,
        }
    }
}

/// Why a datum was not sealed or does not read: the first broken rule, naming what it is about.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DatumError {
    /// The bytes do not start with [`DATUM_MAGIC`].
    Magic,
    /// The head's counts need `need` bytes; there are `have`.
    Length { need: usize, have: usize },
    /// The place byte `byte` names no [`Place`].
    PlaceByte { byte: u8 },
    /// The alignment's log2 `align` is past [`MOST_ALIGN`].
    Align { align: u8 },
    /// A `Zeroed` datum keeps bytes or words, or another's length is not its bytes'.
    ZeroedHolds,
    /// A target of a space a datum does not name yet (a thread-local).
    Space { space: Space, symbol: Symbol },
    /// A C name target `symbol` with no name kept: a target added with [`Datum::target`]
    /// instead of [`Datum::outside`], or a names section with fewer rows than C name targets.
    NoName { symbol: Symbol },
    /// The name kept for C name target `symbol` is not UTF-8 or does not hash to it
    /// ([`Symbol::of_name`]).
    NameWrong { symbol: Symbol },
    /// The names section is not its one form (`why`): rows not end to end, text past the rows,
    /// padding not zero, or a section where the datum names no C name.
    Names { why: &'static str },
    /// Target `row` has the space byte `byte`.
    SpaceByte { row: u32, byte: u8 },
    /// A word at `at` names a target index the datum does not list.
    NoSuchTarget { at: u32, target: u32 },
    /// A word at `at` is not 8-aligned, or the datum holding it is aligned below 8.
    WordMisaligned { at: u32 },
    /// A word at `at` is not inside the datum's `len` bytes.
    WordOutside { at: u32, len: u32 },
    /// Two words overlap at `at`.
    WordsOverlap { at: u32 },
    /// An own word at `at` addresses byte `addend`, past the datum's `len` bytes.
    OwnOutside { at: u32, addend: i64, len: u32 },
    /// Targets not sorted and unique on bytes from a store.
    TargetsOrder,
    /// A part (`what`) longer than a `u32` counts.
    TooLarge { what: &'static str },
}

impl fmt::Display for DatumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let form = "a static's data record (blaze-code datum.rs; run.md, \"Statics in the run\")";
        match *self {
            DatumError::Magic => write!(f, "the bytes are not {form}: no `bzdata01` head"),
            DatumError::Length { need, have } => write!(f, "{form}'s head counts {need} bytes; it has {have}"),
            DatumError::PlaceByte { byte } => write!(f, "{form} has the unknown place {byte}"),
            DatumError::Align { align } => write!(f, "{form} asks an alignment of 2^{align}, past a page (2^{MOST_ALIGN})"),
            DatumError::ZeroedHolds => write!(f, "{form}: a zeroed datum keeps no bytes or words, and another's length is its bytes'"),
            DatumError::Space { space, symbol } => write!(f, "{form} names {space:?} {symbol}: a static's value naming a thread-local is not placed yet"),
            DatumError::NoName { symbol } => write!(f, "{form} names C name {symbol} and keeps no name for it (a C name is added with `Datum::outside`)"),
            DatumError::NameWrong { symbol } => write!(f, "{form} keeps a name for C name {symbol} that is not UTF-8 or does not hash to it"),
            DatumError::Names { why } => write!(f, "{form}'s names section is not its one form: {why}"),
            DatumError::SpaceByte { row, byte } => write!(f, "{form}'s target {row} has the unknown space {byte}"),
            DatumError::NoSuchTarget { at, target } => write!(f, "{form}'s word at {at} names target {target}, which it does not list"),
            DatumError::WordMisaligned { at } => write!(f, "{form}'s word at {at} is not 8-aligned in an 8-aligned datum"),
            DatumError::WordOutside { at, len } => write!(f, "{form}'s word at {at} is past its {len} bytes"),
            DatumError::WordsOverlap { at } => write!(f, "{form} has two words at {at}"),
            DatumError::OwnOutside { at, addend, len } => write!(f, "{form}'s word at {at} addresses its own byte {addend}, past its {len} bytes"),
            DatumError::TargetsOrder => write!(f, "{form}'s targets are not sorted, each once"),
            DatumError::TooLarge { what } => write!(f, "{form}'s {what} is longer than a u32 counts"),
        }
    }
}

impl core::error::Error for DatumError {}

/// A datum being built. Targets may be added in any order and more than once;
/// [`Datum::seal`] sorts, merges and renumbers them.
pub struct Datum<'a> {
    pub place: Place,
    /// log2 of the alignment.
    pub align: u8,
    pub bytes: AVec<'a, u8>,
    pub targets: AVec<'a, Target>,
    pub words: AVec<'a, Word>,
    /// Each C name added ([`Datum::outside`]): its symbol and its text's start and length in
    /// `text`. Sealed: one row per C name target, in target order, end to end.
    names: AVec<'a, (Symbol, u32, u32)>,
    text: AVec<'a, u8>,
    alloc: Alloc<'a>,
}

impl<'a> Datum<'a> {
    pub fn new_in(place: Place, align: u8, alloc: Alloc<'a>) -> Datum<'a> {
        Datum {
            place,
            align,
            bytes: Vec::new_in(alloc),
            targets: Vec::new_in(alloc),
            words: Vec::new_in(alloc),
            names: Vec::new_in(alloc),
            text: Vec::new_in(alloc),
            alloc,
        }
    }

    /// `target`'s index, for a word made before [`Datum::seal`]. A C name is added with
    /// [`Datum::outside`], which keeps its text; one added here is refused at the seal
    /// ([`DatumError::NoName`]).
    pub fn target(&mut self, target: Target) -> u32 {
        self.targets.push(target);
        (self.targets.len() - 1) as u32
    }

    /// C name `name`'s index as a target ([`Space::Outside`], [`Symbol::of_name`]), its text
    /// kept for the run's binding, for a word made before [`Datum::seal`].
    pub fn outside(&mut self, name: &str) -> u32 {
        let symbol = Symbol::of_name(name);
        if !self.names.iter().any(|&(kept, _, _)| kept == symbol) {
            // A length past a u32 is refused at the seal (`TooLarge`); saturated here.
            let start = u32::try_from(self.text.len()).unwrap_or(u32::MAX);
            let len = u32::try_from(name.len()).unwrap_or(u32::MAX);
            self.names.push((symbol, start, len));
            self.text.extend_from_slice(name.as_bytes());
        }
        self.target(Target { space: Space::Outside, symbol })
    }

    /// The datum in its one canonical form: targets sorted and unique (every word renumbered),
    /// words sorted by offset, one name per C name target in target order; every rule checked
    /// once here.
    pub fn seal(self) -> Result<SealedDatum<'a>, DatumError> {
        let Datum { place, align, bytes, targets, mut words, names, text, alloc } = self;
        if u32::try_from(text.len()).is_err() {
            return Err(DatumError::TooLarge { what: "C names' text" });
        }
        if align > MOST_ALIGN {
            return Err(DatumError::Align { align });
        }
        let len = match place {
            Place::Zeroed { len } => {
                if !bytes.is_empty() || !words.is_empty() {
                    return Err(DatumError::ZeroedHolds);
                }
                len
            }
            Place::ReadOnly | Place::Writable => bytes.len() as u64,
        };
        let len = u32::try_from(len).map_err(|_| DatumError::TooLarge { what: "length" })?;
        for t in targets.iter() {
            allowed(*t)?;
        }
        let mut order: AVec<'a, u32> = Vec::with_capacity_in(targets.len(), alloc);
        for i in 0..targets.len() {
            order.push(i as u32);
        }
        order.sort_unstable_by(|&a, &b| targets[a as usize].cmp(&targets[b as usize]));
        let mut renumber: AVec<'a, u32> = Vec::from_elem_in(0, targets.len(), alloc);
        let mut sorted: AVec<'a, Target> = Vec::with_capacity_in(targets.len(), alloc);
        for &old in order.iter() {
            let t = targets[old as usize];
            if sorted.last() != Some(&t) {
                sorted.push(t);
            }
            renumber[old as usize] = (sorted.len() - 1) as u32;
        }
        for word in words.iter_mut() {
            if word.target == OWN_DATUM {
                continue;
            }
            match renumber.get(word.target as usize) {
                Some(&new) => word.target = new,
                None => return Err(DatumError::NoSuchTarget { at: word.at, target: word.target }),
            }
        }
        words.sort_unstable_by_key(|w| w.at);
        check_words(words.iter().copied(), align, len, sorted.len())?;
        // One name per C name target, in target order, end to end: the canonical section
        // (equal data write equal bytes however their names were added).
        let mut kept: AVec<'a, (Symbol, u32, u32)> = Vec::new_in(alloc);
        let mut kept_text: AVec<'a, u8> = Vec::new_in(alloc);
        for t in sorted.iter().filter(|t| t.space == Space::Outside) {
            let Some(&(_, start, name_len)) = names.iter().find(|&&(symbol, _, _)| symbol == t.symbol) else {
                return Err(DatumError::NoName { symbol: t.symbol });
            };
            let (start, name_len) = (start as usize, name_len as usize);
            let at = kept_text.len() as u32;
            kept_text.extend_from_slice(&text[start..start + name_len]);
            kept.push((t.symbol, at, name_len as u32));
        }
        if matches!(place, Place::Zeroed { .. }) && !kept.is_empty() {
            return Err(DatumError::ZeroedHolds);
        }
        Ok(SealedDatum(Datum { place, align, bytes, targets: sorted, words, names: kept, text: kept_text, alloc }))
    }
}

/// A target a datum may name (the module doc).
fn allowed(t: Target) -> Result<(), DatumError> {
    match t.space {
        Space::Func | Space::Static | Space::Vtable | Space::Outside => Ok(()),
        Space::Tls => Err(DatumError::Space { space: t.space, symbol: t.symbol }),
    }
}

/// The names section's length: a row per C name, then their texts end to end.
fn names_len(rows: usize, text: usize) -> usize {
    rows * NAME_ROW + text
}

/// The words' rules over words sorted by offset: each 8-aligned in an 8-aligned datum, inside
/// it, none overlapping, each target listed, each own address inside.
fn check_words(words: impl Iterator<Item = Word>, align: u8, len: u32, targets: usize) -> Result<(), DatumError> {
    let mut next = 0u64;
    for word in words {
        if word.at % 8 != 0 || align < 3 {
            return Err(DatumError::WordMisaligned { at: word.at });
        }
        if u64::from(word.at) < next {
            return Err(DatumError::WordsOverlap { at: word.at });
        }
        next = u64::from(word.at) + 8;
        if next > u64::from(len) {
            return Err(DatumError::WordOutside { at: word.at, len });
        }
        match word.target {
            OWN_DATUM if word.addend < 0 || word.addend as u64 > u64::from(len) => {
                return Err(DatumError::OwnOutside { at: word.at, addend: word.addend, len });
            }
            OWN_DATUM => {}
            index if index as usize >= targets => return Err(DatumError::NoSuchTarget { at: word.at, target: index }),
            _ => {}
        }
    }
    Ok(())
}

/// A datum in its canonical form ([`Datum::seal`]): the only form that encodes.
pub struct SealedDatum<'a>(Datum<'a>);

impl<'a> SealedDatum<'a> {
    pub fn datum(&self) -> &Datum<'a> {
        &self.0
    }

    /// The datum's bytes, appended to `out` ([`DatumView`] says the layout). Equal sealed data
    /// write equal bytes.
    pub fn encode(&self, out: &mut AVec<'_, u8>) {
        let d = &self.0;
        let start = out.len();
        out.extend_from_slice(&DATUM_MAGIC);
        out.push(d.place.byte());
        out.push(d.align);
        out.extend_from_slice(&[0; 6]);
        let len = match d.place {
            Place::Zeroed { len } => len,
            Place::ReadOnly | Place::Writable => d.bytes.len() as u64,
        };
        out.extend_from_slice(&len.to_le_bytes());
        let names = if d.names.is_empty() { 0 } else { names_len(d.names.len(), d.text.len()) };
        for count in [d.bytes.len(), d.targets.len(), d.words.len(), names] {
            out.extend_from_slice(&(count as u32).to_le_bytes());
        }
        out.extend_from_slice(&d.bytes);
        while (out.len() - start) % 8 != 0 {
            out.push(0);
        }
        for t in d.targets.iter() {
            out.push(t.space as u8);
            out.extend_from_slice(&[0; 7]);
            out.extend_from_slice(&t.symbol.0);
        }
        for w in d.words.iter() {
            out.extend_from_slice(&w.at.to_le_bytes());
            out.extend_from_slice(&w.target.to_le_bytes());
            out.extend_from_slice(&w.addend.to_le_bytes());
        }
        // The names section (sealed: one row per C name target, in target order, end to end),
        // padded to 8; absent when the datum names no C name.
        for &(_, start, name_len) in d.names.iter() {
            out.extend_from_slice(&start.to_le_bytes());
            out.extend_from_slice(&name_len.to_le_bytes());
        }
        out.extend_from_slice(&d.text);
        while (out.len() - start) % 8 != 0 {
            out.push(0);
        }
    }
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn get64(b: &[u8], at: usize) -> u64 {
    u64::from(get32(b, at)) | (u64::from(get32(b, at + 4)) << 32)
}

/// An encoded datum read in place. Layout (little-endian): `DATUM_MAGIC`; place `u8` (0
/// read-only, 1 writable, 2 zeroed), align `u8`, six zero bytes; `len: u64` (its bytes'
/// length, or a zeroed datum's); four `u32` counts (bytes, targets, words, 0); the bytes padded
/// to 8; targets (space `u8`, 7 zero bytes, symbol 32); words (`at: u32`, `target: u32`,
/// `addend: i64`); then, when the fourth count `n` is not 0, the names section of `n` bytes
/// padded to 8: one row (`start: u32`, `len: u32`) per C name target in target order, then
/// their texts end to end (row `k`'s text at `start` of the texts).
#[derive(Clone, Copy)]
pub struct DatumView<'b> {
    bytes: &'b [u8],
    place: Place,
    align: u8,
    data: &'b [u8],
    targets: &'b [u8],
    words: &'b [u8],
    /// The names section's rows and texts (empty when the datum names no C name).
    name_rows: &'b [u8],
    name_text: &'b [u8],
    /// The index of the first C name target: they sort last ([`Space::Outside`] is the last
    /// space), so target `first_outside + k` is row `k`'s.
    first_outside: usize,
}

impl<'b> DatumView<'b> {
    /// Reads the datum that is exactly `bytes`, checking every rule [`Datum::seal`] checks, once.
    pub fn read(bytes: &'b [u8]) -> Result<DatumView<'b>, DatumError> {
        if bytes.len() < HEAD {
            return Err(DatumError::Length { need: HEAD, have: bytes.len() });
        }
        if bytes[..8] != DATUM_MAGIC {
            return Err(DatumError::Magic);
        }
        let align = bytes[9];
        if align > MOST_ALIGN {
            return Err(DatumError::Align { align });
        }
        let len = get64(bytes, 16);
        let (count_bytes, count_targets, count_words, count_names) =
            (get32(bytes, 24) as usize, get32(bytes, 28) as usize, get32(bytes, 32) as usize, get32(bytes, 36) as usize);
        let place = match bytes[8] {
            0 => Place::ReadOnly,
            1 => Place::Writable,
            2 => Place::Zeroed { len },
            byte => return Err(DatumError::PlaceByte { byte }),
        };
        let consistent = match place {
            Place::Zeroed { .. } => count_bytes == 0 && count_words == 0 && count_names == 0,
            Place::ReadOnly | Place::Writable => len == count_bytes as u64,
        };
        if !consistent {
            return Err(DatumError::ZeroedHolds);
        }
        let len = u32::try_from(len).map_err(|_| DatumError::TooLarge { what: "length" })?;
        let padded = (count_bytes + 7) & !7;
        let names_at = HEAD + padded + count_targets * TARGET_ROW + count_words * WORD_ROW;
        let need = names_at + ((count_names + 7) & !7);
        if need != bytes.len() {
            return Err(DatumError::Length { need, have: bytes.len() });
        }
        let data = &bytes[HEAD..HEAD + count_bytes];
        let targets = &bytes[HEAD + padded..HEAD + padded + count_targets * TARGET_ROW];
        let words = &bytes[HEAD + padded + count_targets * TARGET_ROW..names_at];
        let names = &bytes[names_at..names_at + count_names];
        if bytes[names_at + count_names..].iter().any(|&byte| byte != 0) {
            return Err(DatumError::Names { why: "its padding is not zero" });
        }
        let mut last: Option<Target> = None;
        let mut first_outside = count_targets;
        for row in 0..count_targets {
            let byte = targets[row * TARGET_ROW];
            let Some(space) = space_of(byte) else {
                return Err(DatumError::SpaceByte { row: row as u32, byte });
            };
            let t = Target { space, symbol: symbol_at(targets, row * TARGET_ROW + 8) };
            allowed(t)?;
            if last.is_some_and(|before| before >= t) {
                return Err(DatumError::TargetsOrder);
            }
            if space == Space::Outside && first_outside == count_targets {
                first_outside = row;
            }
            last = Some(t);
        }
        // The C name targets sort last, so they are rows `first_outside..`; each has its row.
        let outside = count_targets - first_outside;
        if outside == 0 && count_names != 0 {
            return Err(DatumError::Names { why: "the datum names no C name" });
        }
        let Some(name_text) = names.get(outside * NAME_ROW..) else {
            return Err(DatumError::NoName { symbol: symbol_at(targets, (first_outside + names.len() / NAME_ROW) * TARGET_ROW + 8) });
        };
        let name_rows = &names[..outside * NAME_ROW];
        let mut next = 0usize;
        for k in 0..outside {
            let (start, name_len) = (get32(name_rows, k * NAME_ROW) as usize, get32(name_rows, k * NAME_ROW + 4) as usize);
            if start != next || start + name_len > name_text.len() {
                return Err(DatumError::Names { why: "its rows are not end to end over its texts" });
            }
            next = start + name_len;
            let symbol = symbol_at(targets, (first_outside + k) * TARGET_ROW + 8);
            match core::str::from_utf8(&name_text[start..next]) {
                Ok(name) if Symbol::of_name(name) == symbol => {}
                _ => return Err(DatumError::NameWrong { symbol }),
            }
        }
        if next != name_text.len() {
            return Err(DatumError::Names { why: "its texts run past its rows" });
        }
        let view = DatumView { bytes, place, align, data, targets, words, name_rows, name_text, first_outside };
        check_words((0..count_words).map(|i| view.word(i)), align, len, count_targets)?;
        Ok(view)
    }

    /// The name kept for C name target `symbol` (checked on read to hash to it), by halving
    /// over the C name targets; `None` when the datum does not name it.
    pub fn name(&self, symbol: Symbol) -> Option<&'b str> {
        let rows = self.name_rows.len() / NAME_ROW;
        let (mut lo, mut hi) = (0usize, rows);
        while lo < hi {
            let mid = (lo + hi) / 2;
            match symbol_at(self.targets, (self.first_outside + mid) * TARGET_ROW + 8).cmp(&symbol) {
                core::cmp::Ordering::Less => lo = mid + 1,
                core::cmp::Ordering::Greater => hi = mid,
                core::cmp::Ordering::Equal => {
                    let start = get32(self.name_rows, mid * NAME_ROW) as usize;
                    let len = get32(self.name_rows, mid * NAME_ROW + 4) as usize;
                    return core::str::from_utf8(self.name_text.get(start..start + len)?).ok();
                }
            }
        }
        None
    }

    /// The whole encoding: equal data are equal bytes.
    pub fn encoded(&self) -> &'b [u8] {
        self.bytes
    }

    pub fn place(&self) -> Place {
        self.place
    }

    /// The alignment in bytes.
    pub fn align(&self) -> u64 {
        1 << self.align
    }

    /// Its length in memory: its bytes', or a zeroed datum's.
    pub fn len(&self) -> u64 {
        match self.place {
            Place::Zeroed { len } => len,
            Place::ReadOnly | Place::Writable => self.data.len() as u64,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Its first bytes before the words are filled (empty for a zeroed datum).
    pub fn bytes(&self) -> &'b [u8] {
        self.data
    }

    pub fn targets(&self) -> usize {
        self.targets.len() / TARGET_ROW
    }

    pub fn target(&self, i: usize) -> Target {
        let row = i * TARGET_ROW;
        let Some(space) = space_of(self.targets[row]) else {
            unreachable!("DatumView::read checked every target's space byte");
        };
        Target { space, symbol: symbol_at(self.targets, row + 8) }
    }

    pub fn words(&self) -> usize {
        self.words.len() / WORD_ROW
    }

    pub fn word(&self, i: usize) -> Word {
        let row = i * WORD_ROW;
        Word { at: get32(self.words, row), target: get32(self.words, row + 4), addend: get64(self.words, row + 8) as i64 }
    }
}

fn space_of(byte: u8) -> Option<Space> {
    [Space::Func, Space::Static, Space::Vtable, Space::Tls, Space::Outside].get(usize::from(byte)).copied()
}

fn symbol_at(b: &[u8], at: usize) -> Symbol {
    let mut s = [0u8; 32];
    s.copy_from_slice(&b[at..at + 32]);
    Symbol(s)
}

/// Why a datum was not filled, naming the static.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FillError {
    /// The buffer is not the datum's bytes' size.
    Size { symbol: Symbol, want: usize, got: usize },
    /// `at` is not on the datum's alignment.
    Misaligned { symbol: Symbol, at: u64, align: u64 },
    /// A word names a target the run's words do not bind.
    Word { symbol: Symbol, why: ImageError },
}

impl fmt::Display for FillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            FillError::Size { symbol, want, got } => write!(f, "static {symbol}: the buffer is {got} bytes, its datum's are {want}"),
            FillError::Misaligned { symbol, at, align } => write!(f, "static {symbol}: placed at {at:#x}, not on its {align}-byte alignment"),
            FillError::Word { symbol, why } => write!(f, "static {symbol}: {why}"),
        }
    }
}

impl core::error::Error for FillError {}

/// Static `symbol`'s datum `datum` made at `at`: its bytes copied into `out` (the write view's
/// bytes at `at`) and every word filled: a target's value from `words` ([`Words::value`]: a
/// fn's entry thunk, a static's or vtable's address, the address a C name is bound to, whose
/// text the run's words read from [`DatumView::name`]) or the datum's own byte, plus the addend.
/// Deterministic: the same datum, `at` and answers give the same bytes.
pub fn fill(datum: &DatumView<'_>, symbol: Symbol, at: u64, words: &mut dyn Words, out: &mut [u8]) -> Result<(), FillError> {
    let want = datum.bytes().len();
    if out.len() != want {
        return Err(FillError::Size { symbol, want, got: out.len() });
    }
    if at % datum.align() != 0 {
        return Err(FillError::Misaligned { symbol, at, align: datum.align() });
    }
    out.copy_from_slice(datum.bytes());
    for w in 0..datum.words() {
        let word = datum.word(w);
        let base = match word.target {
            OWN_DATUM => at,
            index => words.value(datum.target(index as usize)).map_err(|why| FillError::Word { symbol, why })?,
        };
        let value = base.wrapping_add(word.addend as u64);
        let off = word.at as usize;
        out[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }
    Ok(())
}
