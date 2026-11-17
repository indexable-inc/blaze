//! A record: one function's code, or one static's value, as the launcher takes it
//! (docs/v2/plan/design/launcher.md, "Step 3").
//!
//! A record is position-independent and process-independent: it holds bytes, the places in
//! them where an address goes ([`Fixup`]) and what each address is of, by *place*
//! ([`Place`]): "the function this record names third", never a slot or an address. The
//! daemon makes it from the stored template of the function at its tier (`code(f, tier 0)`,
//! docs/v2/plan/design/tiers.md) and the names of what the template refers to; what a place
//! is in one process (a function's slot, a static's number) comes with the bind
//! (`wire::Bind::refs`). So one record serves every process that runs the function, and a
//! callee's body edit changes no caller's record.
//!
//! What each kind of place is resolved to when the record is placed ([`crate::place`]):
//!
//! | a fixup against | through the GOT (`GotPcRel32`, `GotPage21`, ..) | any other kind |
//! |---|---|---|
//! | a function | its slot word: the call runs what the slot is bound to | its entry thunk: the function's address |
//! | a static, an outside symbol | a cell of the record's own pool that holds the address | the address |
//! | the record's own data | a pool cell that holds the datum's address | the datum's address |
//! | a thread-local static | (`Tlv*`, `TlsDesc*`) the pool's descriptor, whose thunk asks the launcher for this thread's copy | refused |
//! | the code itself (`Own`) | a pool cell | the placed code's address |
//!
//! The bytes are one CBOR item in the core deterministic encoding, written and read by hand
//! as [`crate::wire`]'s messages are; a record's id is the BLAKE3 hash of them. A struct is
//! an array of its fields in the order below (a record is read only by this module, of this
//! [`LAYOUT`], so its fields need no names).

use alloc::vec::Vec;
use core::alloc::Allocator;

use crate::wire::cbor::{Reader, Writer};
use crate::wire::{Arch, List, Listed, WireError};

/// The version of a record's layout: its first field. A record of another is refused by
/// name ([`WireError::Schema`]).
pub const LAYOUT: u32 = 1;

/// How a fixup writes its address: `blaze_codegen::RelocKind`'s native kinds.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[repr(u8)]
pub enum Kind {
    /// The absolute address, 64 bits.
    Abs64 = 0,
    /// x86-64: 32 bits relative to the field.
    PcRel32 = 1,
    /// x86-64: the GOT entry's address, relative.
    GotPcRel32 = 2,
    /// AArch64 `bl`/`b`: 26 bits of words.
    Call26 = 3,
    /// AArch64 `adrp`: the page.
    AdrpPage21 = 4,
    /// AArch64 `add`/`ldr`/`str`: the low 12 bits, scaled as the instruction says.
    PageOffset12 = 5,
    /// AArch64 `adrp` and `ldr` of a GOT entry.
    GotPage21 = 6,
    GotPageOffset12 = 7,
    /// x86-64 Mach-O: `rel32` to the cell that holds a thread-local's descriptor.
    TlvRel32 = 8,
    /// AArch64 Mach-O: that cell's page and low 12 bits.
    TlvPage21 = 9,
    TlvPageOffset12 = 10,
    /// AArch64 ELF TLS descriptor: the pair's page, its `ldr`, its `add`, the call.
    TlsDescPage21 = 11,
    TlsDescLd64Lo12 = 12,
    TlsDescAddLo12 = 13,
    TlsDescCall = 14,
}

impl Kind {
    pub const ALL: [Kind; 15] = [
        Kind::Abs64,
        Kind::PcRel32,
        Kind::GotPcRel32,
        Kind::Call26,
        Kind::AdrpPage21,
        Kind::PageOffset12,
        Kind::GotPage21,
        Kind::GotPageOffset12,
        Kind::TlvRel32,
        Kind::TlvPage21,
        Kind::TlvPageOffset12,
        Kind::TlsDescPage21,
        Kind::TlsDescLd64Lo12,
        Kind::TlsDescAddLo12,
        Kind::TlsDescCall,
    ];

    fn of(tag: u64, at: usize) -> Result<Kind, WireError> {
        usize::try_from(tag)
            .ok()
            .and_then(|tag| Kind::ALL.get(tag).copied())
            .ok_or(WireError::Range { at, what: "a fixup's kind this layout has not" })
    }

    /// The bytes of the field the kind writes: an address, or an instruction or 32 bits.
    pub const fn width(self) -> u64 {
        match self {
            Kind::Abs64 => 8,
            _ => 4,
        }
    }

    /// How the kind reaches what it is against.
    pub const fn through(self) -> Through {
        match self {
            Kind::Abs64 | Kind::PcRel32 | Kind::Call26 | Kind::AdrpPage21 | Kind::PageOffset12 => {
                Through::Direct
            }
            Kind::GotPcRel32 | Kind::GotPage21 | Kind::GotPageOffset12 => Through::Got,
            Kind::TlvRel32 | Kind::TlvPage21 | Kind::TlvPageOffset12 => Through::Tlv,
            Kind::TlsDescPage21
            | Kind::TlsDescLd64Lo12
            | Kind::TlsDescAddLo12
            | Kind::TlsDescCall => Through::TlsDesc,
        }
    }
}

/// What a fixup's field names: the target itself, or something of the record's pool.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Through {
    /// The target's address.
    Direct,
    /// A word that holds the target's address: a function's slot word, else a pool cell.
    Got,
    /// Mach-O: a pool cell that holds the address of a thread-local's descriptor.
    Tlv,
    /// ELF: a thread-local's descriptor pair.
    TlsDesc,
}

/// What a fixup is against.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum To {
    /// The `n`th of the record's [`Record::places`].
    Place(u32),
    /// The `n`th of the record's [`Record::data`].
    Data(u32),
    /// The record's own bytes, where they are placed.
    Own,
}

/// A place in a record's bytes where an address goes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Fixup {
    /// The offset of the field in the bytes.
    pub at: u32,
    pub kind: Kind,
    pub to: To,
    pub addend: i64,
}

/// Something outside a record that it names.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Place<'a> {
    /// A function of the program: the bind gives its slot.
    Function,
    /// A static of the program: the bind gives its number.
    Static,
    /// A symbol of outside code (libc, a C dependency, the system), by the name the dynamic
    /// loader knows it by: the launcher looks it up, and stops naming it when it is not there.
    Extern(&'a str),
    /// The same, linked weakly: the address is 0 when nothing defines it.
    Weak(&'a str),
}

impl Place<'_> {
    /// Whether the bind gives a number for it ([`crate::wire::Bind::refs`]).
    pub const fn numbered(&self) -> bool {
        matches!(self, Place::Function | Place::Static)
    }
}

/// Bytes a record refers to and that outlive its code: a string literal, a constant, a
/// vtable. The launcher places a datum once a process by its id and never frees it, since
/// the program may keep its address past the body that named it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Datum<'a> {
    /// What the launcher keeps one copy of it by: its content's id, or for a `writable` one
    /// its owner's.
    pub id: [u8; 32],
    /// A power of two.
    pub align: u32,
    pub bytes: &'a [u8],
    /// Each [`Kind::Abs64`], against a place or a datum before this one.
    pub fixups: List<'a, Fixup>,
    /// Written while the program runs.
    pub writable: bool,
}

/// How to unwind through a function: DWARF call frame information, as `.eh_frame` has it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Unwind<'a> {
    pub code_align: u32,
    pub data_align: i32,
    pub return_address: u16,
    /// The CIE's initial instructions.
    pub cie: &'a [u8],
    /// The FDE's instructions.
    pub fde: &'a [u8],
    /// The personality routine: a function place.
    pub personality: Option<u32>,
    /// The language-specific data area: one of the record's data.
    pub lsda: Option<u32>,
}

/// What a record is of.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum What {
    /// A function's code.
    Code,
    /// A static's initial value.
    Static {
        /// Written while the program runs.
        mutable: bool,
        thread_local: bool,
    },
}

/// One function's code or one static's value. See the module's documentation.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Record<'a> {
    pub arch: Arch,
    pub what: What,
    /// A power of two.
    pub align: u32,
    /// The length of the body: of `bytes`, or of a static whose value is all zeroes, for
    /// which `bytes` is empty.
    pub size: u64,
    pub bytes: &'a [u8],
    pub fixups: List<'a, Fixup>,
    pub places: List<'a, Place<'a>>,
    pub data: List<'a, Datum<'a>>,
    pub unwind: Option<Unwind<'a>>,
    /// The item's path, as a backtrace prints it.
    pub name: &'a str,
}

/// A signed number as an unsigned one whose size follows its magnitude.
const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

const fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// An array's head, which must have exactly `want` items.
fn fields(from: &mut Reader<'_>, want: usize, what: &'static str) -> Result<(), WireError> {
    let at = from.at();
    match from.array()? {
        got if got == want => Ok(()),
        _ => Err(WireError::Type { at, want: what }),
    }
}

impl Listed<'_> for Fixup {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.array(5);
        to.uint(u64::from(self.at));
        to.uint(self.kind as u64);
        let (tag, index) = match self.to {
            To::Place(place) => (0, place),
            To::Data(datum) => (1, datum),
            To::Own => (2, 0),
        };
        to.uint(tag);
        to.uint(u64::from(index));
        to.uint(zigzag(self.addend));
    }

    fn read(from: &mut Reader<'_>) -> Result<Fixup, WireError> {
        fields(from, 5, "a fixup, an array of 5")?;
        let at = from.uint32("a fixup's offset past 32 bits")?;
        let here = from.at();
        let kind = Kind::of(from.uint()?, here)?;
        let here = from.at();
        let tag = from.uint()?;
        let index = from.uint32("a fixup's place past 32 bits")?;
        let to = match (tag, index) {
            (0, place) => To::Place(place),
            (1, datum) => To::Data(datum),
            (2, 0) => To::Own,
            _ => return Err(WireError::Range { at: here, what: "what a fixup is against" }),
        };
        let addend = unzigzag(from.uint()?);
        Ok(Fixup { at, kind, to, addend })
    }
}

impl<'a> Listed<'a> for Place<'a> {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        match *self {
            Place::Function => to.uint(0),
            Place::Static => to.uint(1),
            Place::Extern(name) => to.text(name),
            Place::Weak(name) => {
                to.array(1);
                to.text(name);
            }
        }
    }

    fn read(from: &mut Reader<'a>) -> Result<Place<'a>, WireError> {
        let at = from.at();
        // A number, a text, or an array of one text: told apart by trying each on a copy.
        let mut number = from.clone(); // clone: two words, to try a shape without taking it
        if let Ok(tag) = number.uint() {
            *from = number;
            return match tag {
                0 => Ok(Place::Function),
                1 => Ok(Place::Static),
                _ => Err(WireError::Range { at, what: "a place's kind this layout has not" }),
            };
        }
        let mut text = from.clone(); // clone: as above
        if let Ok(name) = text.text() {
            *from = text;
            return Ok(Place::Extern(name));
        }
        fields(from, 1, "a place: 0, 1, a name, or an array of one name")?;
        Ok(Place::Weak(from.text()?))
    }
}

impl<'a> Listed<'a> for Datum<'a> {
    fn write<A: Allocator>(&self, to: &mut Writer<'_, A>) {
        to.array(5);
        to.bytes(&self.id);
        to.uint(u64::from(self.align));
        to.bytes(self.bytes);
        self.fixups.write(to);
        to.bool(self.writable);
    }

    fn read(from: &mut Reader<'a>) -> Result<Datum<'a>, WireError> {
        fields(from, 5, "a datum, an array of 5")?;
        let at = from.at();
        let id = <[u8; 32]>::try_from(from.bytes()?)
            .map_err(|_| WireError::Range { at, what: "a datum's id is 32 bytes" })?;
        let align = from.uint32("an alignment past 32 bits")?;
        let bytes = from.bytes()?;
        let fixups = List::read(from)?;
        let writable = from.bool()?;
        Ok(Datum { id, align, bytes, fixups, writable })
    }
}

impl<'a> Record<'a> {
    /// Writes the record onto the end of `out`: its bytes, whose BLAKE3 hash is its id.
    pub fn encode<A: Allocator>(&self, out: &mut Vec<u8, A>) {
        let mut to = Writer::new(out);
        to.array(11);
        to.uint(u64::from(LAYOUT));
        to.uint(match self.arch {
            Arch::X86_64 => 0,
            Arch::Aarch64 => 1,
        });
        to.uint(match self.what {
            What::Code => 0,
            What::Static { mutable, thread_local } => {
                1 + u64::from(mutable) + 2 * u64::from(thread_local)
            }
        });
        to.uint(u64::from(self.align));
        to.uint(self.size);
        to.bytes(self.bytes);
        self.fixups.write(&mut to);
        self.places.write(&mut to);
        self.data.write(&mut to);
        match self.unwind {
            None => to.null(),
            Some(unwind) => {
                to.array(7);
                to.uint(u64::from(unwind.code_align));
                to.uint(zigzag(i64::from(unwind.data_align)));
                to.uint(u64::from(unwind.return_address));
                to.bytes(unwind.cie);
                to.bytes(unwind.fde);
                for index in [unwind.personality, unwind.lsda] {
                    match index {
                        Some(index) => to.uint(u64::from(index)),
                        None => to.null(),
                    }
                }
            }
        }
        to.text(self.name);
    }

    /// Reads a record's bytes, all of them: every list is read through once, so iterating
    /// one afterwards cannot fail, and every index a fixup or the unwind entry holds is
    /// checked against what it is an index into.
    pub fn decode(bytes: &'a [u8]) -> Result<Record<'a>, WireError> {
        let mut from = Reader::new(bytes);
        fields(&mut from, 11, "a record, an array of 11")?;
        let layout = from.uint()?;
        if layout != u64::from(LAYOUT) {
            return Err(WireError::Schema { said: layout });
        }
        let at = from.at();
        let arch = match from.uint()? {
            0 => Arch::X86_64,
            1 => Arch::Aarch64,
            _ => return Err(WireError::Range { at, what: "an instruction set this layout has not" }),
        };
        let at = from.at();
        let what = match from.uint()? {
            0 => What::Code,
            tag @ 1..=4 => What::Static { mutable: (tag - 1) & 1 != 0, thread_local: tag > 2 },
            _ => return Err(WireError::Range { at, what: "what a record is of" }),
        };
        let at = from.at();
        let align = from.uint32("an alignment past 32 bits")?;
        if !align.is_power_of_two() {
            return Err(WireError::Range { at, what: "an alignment that is not a power of two" });
        }
        let at = from.at();
        let size = from.uint()?;
        let bytes_ = from.bytes()?;
        if !(bytes_.len() as u64 == size || (bytes_.is_empty() && what != What::Code)) {
            return Err(WireError::Range { at, what: "a size that is not its bytes' length" });
        }
        let fixups_at = from.at();
        let fixups: List<'a, Fixup> = List::read(&mut from)?;
        let places: List<'a, Place<'a>> = List::read(&mut from)?;
        let data_at = from.at();
        let data: List<'a, Datum<'a>> = List::read(&mut from)?;
        let unwind_at = from.at();
        let unwind = match from.null() {
            true => None,
            false => {
                fields(&mut from, 7, "an unwind entry, an array of 7")?;
                let code_align = from.uint32("a code alignment past 32 bits")?;
                let at = from.at();
                let data_align = i32::try_from(unzigzag(from.uint()?))
                    .map_err(|_| WireError::Range { at, what: "a data alignment past 32 bits" })?;
                let at = from.at();
                let return_address = u16::try_from(from.uint()?)
                    .map_err(|_| WireError::Range { at, what: "a register past 16 bits" })?;
                let cie = from.bytes()?;
                let fde = from.bytes()?;
                let mut index = || match from.null() {
                    true => Ok(None),
                    false => from.uint32("an index past 32 bits").map(Some),
                };
                let personality = index()?;
                let lsda = index()?;
                Some(Unwind { code_align, data_align, return_address, cie, fde, personality, lsda })
            }
        };
        let name = from.text()?;
        from.end()?;

        // Every index names something.
        let (place_count, data_count) = (places.len() as u64, data.len() as u64);
        let named = |to: To, before: u64| match to {
            To::Place(place) => u64::from(place) < place_count,
            To::Data(datum) => u64::from(datum) < before,
            To::Own => true,
        };
        let inside = |it: Fixup| u64::from(it.at) + it.kind.width() <= bytes_.len() as u64;
        if fixups.iter().any(|it| !named(it.to, data_count) || !inside(it)) {
            let what = "a fixup past the bytes, or against a place or a datum that is not there";
            return Err(WireError::Range { at: fixups_at, what });
        }
        for (index, datum) in data.iter().enumerate() {
            let inside = |it: Fixup| {
                it.kind == Kind::Abs64
                    && it.to != To::Own
                    && named(it.to, index as u64)
                    && u64::from(it.at) + 8 <= datum.bytes.len() as u64
            };
            if !datum.align.is_power_of_two() || !datum.fixups.iter().all(inside) {
                let what = "a datum whose alignment is no power of two, or with a fixup that \
                            is not an absolute address of a place or an earlier datum inside it";
                return Err(WireError::Range { at: data_at, what });
            }
        }
        if let Some(unwind) = unwind {
            let function = |place: u32| places.iter().nth(place as usize) == Some(Place::Function);
            if !unwind.personality.is_none_or(function)
                || !unwind.lsda.is_none_or(|datum| u64::from(datum) < data_count)
                || what != What::Code
            {
                let what = "an unwind entry of no function, or whose personality is no \
                            function place, or whose data area is no datum";
                return Err(WireError::Range { at: unwind_at, what });
            }
        }
        Ok(Record { arch, what, align, size, bytes: bytes_, fixups, places, data, unwind, name })
    }

    /// How many numbers a bind gives for the record: its function and static places.
    pub fn numbered(&self) -> usize {
        self.places.iter().filter(Place::numbered).count()
    }
}
