//! An atom made a record of the form Rust bodies are made in (c.md part 3: "code records, one a
//! function, as Rust bodies make them, so there is no link step"): a function atom a blaze-code
//! [`CodeRecord`], a data atom a [`Datum`].
//!
//! A code record reaches every other thing through a word ([`SiteKind::Load`], [`SiteKind::Call`],
//! record.rs), and its sites are instruction pairs at `at` and `at + 4`. What clang writes, and
//! what each becomes, the kinds no other atom case reaches being a typed refusal
//! ([`LowerError::NotAccepted`], naming the atom, the offset and what the form lacks):
//!
//! | clang writes | the record holds |
//! |---|---|
//! | `bl`/`b` to another atom or name (`Branch26`) | the branch made to reach a VENEER at the code's end: `adrp x16; ldr x16, [x16]` (a `Load` site of the target) then `br x16`. x16 is IP0, which AAPCS64 lets a veneer clobber across a call; a call's return goes to the caller as before |
//! | `bl`/`b` to its own atom | the branch completed in place |
//! | `adrp xN, t@GOTPAGE` + `ldr xN, [xN, t@GOTPAGEOFF]`, adjacent, one register | a `Load` site of `t` |
//! | `adrp xN, t@PAGE` + `add xN, xN, t@PAGEOFF`, adjacent, one register, no addend | the `add` made `ldr xN, [xN]` and a `Load` site of `t`: the word holds `t`'s address, so `xN` is the same value |
//! | an 8-byte pointer in data (`Abs64`) to an atom of the unit | a datum word |
//! | an 8-byte pointer in data to a C name | a datum word of the name ([`blaze_code::datum::Datum::outside`], its text kept) |
//!
//! Refused for now, each by name: a pair not adjacent or across two registers (the scheduler
//! separates them at `-O2`: the form needs a two-offset site, lib.rs "What the launcher must
//! gain"); a load or store THROUGH a direct page offset (`ldr w0, [x8, t@PAGEOFF]`, which a
//! word cannot replace in two instructions); an address with an addend; a difference of two addresses; a thread-local; a weak
//! undefined name; `CondBranch19`/`TestBranch14` out of the atom; C++ exception tables.
//!
//! **Unwind.** The record's frame word says one prologue, blaze-emit's (unwind.rs: instruction 0
//! `stp x29, x30, [sp, #-16]!`, instruction 1 `mov x29, sp`); clang's frame prologue is another
//! (`sub sp; stp x29, x30, [sp, #n]; add x29, sp, #n`). So a C atom's frame word is 0, "cannot
//! be unwound through": a backtrace stops at a C frame, and a Rust panic that would cross one
//! ends the program, as `extern "C"` already has it. An atom with a personality or an LSDA (C++
//! that throws or cleans up) is refused until the form carries DWARF rows (eh.rs makes them for
//! blaze's own prologue only).
//!
//! **Targets.** A name is `Space::Outside` with `Symbol::of_name`, the target Rust's records give
//! a C name; an atom of the unit is `Space::Func` (code) or `Space::Static` (data) with the
//! unit-local symbol ([`crate::key::local_symbol`]), unless it is a global atom, which is reached
//! by its name like any other unit's.

use core::fmt;

use blaze_code::a64::{ADD64, ADRP, LDR64, read32, write32};
use blaze_code::datum::{Datum, DatumError, OWN_DATUM, Place, SealedDatum};
use blaze_code::record::{CodeRecord, LEAST_CODE_ALIGN, MOST_PART_ALIGN, RecordError, Sealed, Site, SiteKind, Space, Symbol, Target, Word};
use blaze_own::Alloc;

use crate::key::{UnitKey, global_symbol, local_symbol};
use crate::split::{Atom, AtomKind, AtomName, Ref, RefKind, RefTo, Shown, Split};

/// `br x16`.
const BR_X16: u32 = 0xD61F_0200;
/// `adrp x16, 0`.
const ADRP_X16: u32 = 0x9000_0010;
/// `ldr x16, [x16]`.
const LDR_X16: u32 = 0xF940_0210;
/// `b`/`bl`: the opcode bits kept, the 26-bit word offset replaced.
const BRANCH_OPCODE: u32 = 0xFC00_0000;

/// One atom, lowered.
pub enum Lowered<'a> {
    /// A function: its symbol and its sealed code record.
    Code { symbol: Symbol, record: Sealed<'a> },
    /// A data object: its symbol and its sealed datum.
    Data { symbol: Symbol, datum: SealedDatum<'a> },
}

/// Why an atom is not a record yet: each names the atom and the offset in it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LowerError<'s> {
    /// The reference at `at` is of a form the record does not hold yet; `what` says which and
    /// what it needs.
    NotAccepted { atom: AtomShown<'s>, at: u32, what: &'static str },
    /// A C name that is not UTF-8: no outside name of the record is.
    NameNotUtf8 { name: Shown<'s> },
    /// The atom is aligned past a page.
    Align { atom: AtomShown<'s>, align_log2: u8 },
    /// The record refused what was made (a bug of this lowering: the rule it names).
    Record { atom: AtomShown<'s>, why: RecordError },
    /// The datum refused what was made.
    Datum { atom: AtomShown<'s>, why: DatumError },
}

/// An atom's name as an error shows it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AtomShown<'s>(pub AtomName<'s>);

impl fmt::Display for AtomShown<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            AtomName::Global(name) => write!(f, "{}", Shown(name)),
            AtomName::Local { name, nth } => write!(f, "{} (local, #{nth})", Shown(name)),
            AtomName::Anonymous { section, start } => write!(f, "the bytes at {start} of section {section}"),
        }
    }
}

impl fmt::Display for LowerError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let at = "(blaze-cunit lower.rs)";
        match *self {
            LowerError::NotAccepted { atom, at: offset, what } => write!(f, "C atom {atom}: the reference at {offset} is {what}, which a code record does not hold yet {at}"),
            LowerError::NameNotUtf8 { name } => write!(f, "C name {name} is not UTF-8 {at}"),
            LowerError::Align { atom, align_log2 } => write!(f, "C atom {atom} asks an alignment of 2^{align_log2}, past a page {at}"),
            LowerError::Record { atom, why } => write!(f, "C atom {atom}: {why} {at}"),
            LowerError::Datum { atom, why } => write!(f, "C atom {atom}: {why} {at}"),
        }
    }
}

impl core::error::Error for LowerError<'_> {}

/// The text of a C name.
fn text(name: &[u8]) -> Result<&str, LowerError<'_>> {
    core::str::from_utf8(name).map_err(|_| LowerError::NameNotUtf8 { name: Shown(name) })
}

/// The symbol atom `n` of `split` is placed under.
pub fn symbol_of<'s>(split: &Split<'s>, unit: UnitKey, n: usize) -> Result<Symbol, LowerError<'s>> {
    match split.atoms[n].name {
        AtomName::Global(name) => Ok(global_symbol(text(name)?)),
        name => Ok(local_symbol(unit, &name)),
    }
}

/// What a reference reaches, as a record's target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach<'s> {
    /// The atom's own bytes.
    Own,
    Name(&'s str),
    Atom(Target),
}

fn reach<'s>(split: &Split<'s>, unit: UnitKey, n: usize, r: &Ref<'s>, atom: &Atom<'s>) -> Result<Reach<'s>, LowerError<'s>> {
    match r.to {
        RefTo::Name { weak: true, .. } => Err(LowerError::NotAccepted { atom: AtomShown(atom.name), at: r.at, what: "a weak undefined C name (the launcher binds no weak outside name yet)" }),
        RefTo::Name { name, weak: false } => Ok(Reach::Name(text(name)?)),
        RefTo::Atom(a) if a as usize == n => Ok(Reach::Own),
        RefTo::Atom(a) => {
            let other = &split.atoms[a as usize];
            if let AtomName::Global(name) = other.name {
                return Ok(Reach::Name(text(name)?));
            }
            let space = match other.kind {
                AtomKind::Code => Space::Func,
                AtomKind::ReadOnly | AtomKind::CStrings | AtomKind::Data | AtomKind::Zero | AtomKind::ExceptTable => Space::Static,
                AtomKind::ThreadData | AtomKind::ThreadZero | AtomKind::ThreadVars => {
                    return Err(LowerError::NotAccepted { atom: AtomShown(atom.name), at: r.at, what: "a reference to a thread-local's bytes" });
                }
            };
            Ok(Reach::Atom(Target { space, symbol: local_symbol(unit, &other.name) }))
        }
    }
}

/// Lowers atom `n` of `split`, a unit of key `unit`, into `alloc`.
pub fn lower<'a, 's>(split: &Split<'s>, unit: UnitKey, n: usize, alloc: Alloc<'a>) -> Result<Lowered<'a>, LowerError<'s>> {
    let atom = &split.atoms[n];
    let symbol = symbol_of(split, unit, n)?;
    match atom.kind {
        AtomKind::Code => Ok(Lowered::Code { symbol, record: code(split, unit, n, alloc)? }),
        AtomKind::ReadOnly | AtomKind::CStrings | AtomKind::Data | AtomKind::Zero | AtomKind::ExceptTable => {
            Ok(Lowered::Data { symbol, datum: data(split, unit, n, alloc)? })
        }
        AtomKind::ThreadData | AtomKind::ThreadZero | AtomKind::ThreadVars => {
            Err(LowerError::NotAccepted { atom: AtomShown(atom.name), at: 0, what: "a thread-local (blaze-code tls.rs's index rows are not made for C yet)" })
        }
    }
}

/// The record's index of `target`, kept once a target.
fn target_index(record: &mut CodeRecord<'_>, reach: &Reach<'_>) -> u32 {
    match *reach {
        Reach::Name(name) => record.outside(name),
        Reach::Atom(target) => record.target(target),
        Reach::Own => unreachable!("an atom's own bytes are no target"),
    }
}

/// Whether `inst` is `(mask, value)`.
fn is(inst: u32, (mask, value): (u32, u32)) -> bool {
    inst & mask == value
}

fn rd(inst: u32) -> u32 {
    inst & 0x1f
}

fn rn(inst: u32) -> u32 {
    (inst >> 5) & 0x1f
}

/// A function atom's code record.
fn code<'a, 's>(split: &Split<'s>, unit: UnitKey, n: usize, alloc: Alloc<'a>) -> Result<Sealed<'a>, LowerError<'s>> {
    let atom = &split.atoms[n];
    let shown = AtomShown(atom.name);
    if let Some(unwind) = atom.unwind {
        if unwind.personality.is_some() || unwind.lsda.is_some() {
            return Err(LowerError::NotAccepted { atom: shown, at: 0, what: "a function with a personality or an LSDA (C++ exception tables)" });
        }
    }
    if atom.align_log2 > MOST_PART_ALIGN {
        return Err(LowerError::Align { atom: shown, align_log2: atom.align_log2 });
    }
    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(atom.bytes);
    record.code_align = atom.align_log2.max(LEAST_CODE_ALIGN);
    // One veneer a target, at the code's end, in the order first branched to.
    let mut veneers: blaze_own::AVec<'a, (Reach<'s>, u32)> = blaze_own::Vec::new_in(alloc);
    let refs = split.refs_of(n);
    let not = |at: u32, what: &'static str| LowerError::NotAccepted { atom: shown, at, what };
    let mut i = 0;
    while i < refs.len() {
        let r = refs[i];
        let at = u64::from(r.at);
        if r.minus.is_some() {
            return Err(not(r.at, "a difference of two addresses"));
        }
        match r.kind {
            RefKind::Branch26 => {
                let inst = read32(&record.code, at);
                let to = match reach(split, unit, n, &r, atom)? {
                    Reach::Own => {
                        // An atom's own offset: the addend counts from its first byte (split.rs).
                        let to = r.addend;
                        if to < 0 || to as u64 >= atom.size || to % 4 != 0 {
                            return Err(not(r.at, "a branch outside its own atom"));
                        }
                        to as u64
                    }
                    target => {
                        if r.addend != 0 {
                            return Err(not(r.at, "a branch to a target plus an addend"));
                        }
                        match veneers.iter().find(|v| v.0 == target) {
                            Some(v) => u64::from(v.1),
                            None => {
                                let index = target_index(&mut record, &target);
                                let v = record.code.len() as u32;
                                for word in [ADRP_X16, LDR_X16, BR_X16] {
                                    record.code.extend_from_slice(&word.to_le_bytes());
                                }
                                record.sites.push(Site { at: v, kind: SiteKind::Load, target: index, addend: 0 });
                                veneers.push((target, v));
                                u64::from(v)
                            }
                        }
                    }
                };
                let words = ((to as i64 - at as i64) / 4) as u32 & 0x03FF_FFFF;
                write32(&mut record.code, at, (inst & BRANCH_OPCODE) | words);
                i += 1;
            }
            RefKind::GotPage21 | RefKind::Page21 => {
                let got = r.kind == RefKind::GotPage21;
                let Some(low) = refs.get(i + 1).copied().filter(|l| l.at == r.at + 4 && l.to == r.to && l.addend == r.addend && l.minus.is_none()) else {
                    return Err(not(r.at, "an adrp whose low half is not the next instruction (a two-offset site is needed)"));
                };
                let first = read32(&record.code, at);
                let second = read32(&record.code, at + 4);
                if !is(first, ADRP) {
                    return Err(not(r.at, "a page relocation on an instruction that is not adrp"));
                }
                let reg = rd(first);
                let target = reach(split, unit, n, &r, atom)?;
                if matches!(target, Reach::Own) {
                    return Err(not(r.at, "the address of its own atom's bytes (a relro site of the code is needed)"));
                }
                if r.addend != 0 {
                    return Err(not(r.at, "an address plus an addend (a Load site has none)"));
                }
                match (got, low.kind) {
                    (true, RefKind::GotPageOff12) if is(second, LDR64) && rd(second) == reg && rn(second) == reg => {}
                    (false, RefKind::PageOff12 { scale_log2: 0 }) if is(second, ADD64) && rd(second) == reg && rn(second) == reg => {
                        write32(&mut record.code, at + 4, LDR64.1 | (reg << 5) | reg);
                    }
                    (false, RefKind::PageOff12 { .. }) => return Err(not(low.at, "a load or store through a direct page offset (a word cannot stand in two instructions)")),
                    _ => return Err(not(low.at, "an adrp pair across two registers or of two kinds")),
                }
                let index = target_index(&mut record, &target);
                record.sites.push(Site { at: r.at, kind: SiteKind::Load, target: index, addend: 0 });
                i += 2;
            }
            RefKind::PageOff12 { .. } | RefKind::GotPageOff12 => return Err(not(r.at, "a low-12 relocation with no adrp before it")),
            RefKind::CondBranch19 | RefKind::TestBranch14 => return Err(not(r.at, "a short branch out of its atom")),
            RefKind::Abs64 | RefKind::Abs32 | RefKind::Rel32 | RefKind::Rel64 => return Err(not(r.at, "an address written into code")),
            RefKind::Delta32 | RefKind::Delta64 => return Err(not(r.at, "a difference of two addresses")),
            RefKind::GotRel32 => return Err(not(r.at, "a pointer to a GOT entry")),
            RefKind::TlvPage21
            | RefKind::TlvPageOff12
            | RefKind::TlsDescPage21
            | RefKind::TlsDescLo12
            | RefKind::TlsDescAdd
            | RefKind::TlsDescCall
            | RefKind::TlsIePage21
            | RefKind::TlsIeLo12
            | RefKind::TlsLeHi12
            | RefKind::TlsLeLo12 => return Err(not(r.at, "a thread-local access")),
        }
    }
    record.seal().map_err(|why| LowerError::Record { atom: shown, why })
}

/// A data atom's datum.
fn data<'a, 's>(split: &Split<'s>, unit: UnitKey, n: usize, alloc: Alloc<'a>) -> Result<SealedDatum<'a>, LowerError<'s>> {
    let atom = &split.atoms[n];
    let shown = AtomShown(atom.name);
    let place = match atom.kind {
        AtomKind::Zero => Place::Zeroed { len: atom.size },
        AtomKind::Data => Place::Writable,
        _ => Place::ReadOnly,
    };
    let mut datum = Datum::new_in(place, atom.align_log2, alloc);
    datum.bytes.extend_from_slice(atom.bytes);
    for r in split.refs_of(n) {
        if r.kind != RefKind::Abs64 || r.minus.is_some() {
            return Err(LowerError::NotAccepted { atom: shown, at: r.at, what: "a relocation in data other than an 8-byte address" });
        }
        let target = match reach(split, unit, n, r, atom)? {
            Reach::Own => OWN_DATUM,
            // The datum keeps the name's text: the run binds it by text (datum.rs `names`).
            Reach::Name(name) => datum.outside(name),
            Reach::Atom(target) => datum.target(target),
        };
        datum.words.push(Word { at: r.at, target, addend: r.addend });
    }
    datum.seal().map_err(|why| LowerError::Datum { atom: shown, why })
}
