//! The records: an instance's [`Plan`] (by its plan key), its [`InstanceCode`] (by its instance
//! key), a static's [`StaticValue`] and a test's base key ([`TestKey`]).
//!
//! **The code record** is position-independent: no address is in its bytes. Every place an
//! address goes is a [`Reloc`] naming its target by stable identity ([`Target`]: a function or a
//! static by symbol, one of the record's own panic locations, an extern by name), never by an
//! instance key, so a callee's edit leaves every caller's record valid. Panic locations are
//! item-relative rows ([`LocRow`]: the item, a line counted from the item's first line, a
//! column), so moving a function in its file moves no key: the program fills each location's
//! line from the span table when it places it. Unwind rows and line rows are item-relative too
//! (offsets into the code). The head of the record ([`CodeHead`]: the functions and statics the
//! code refers to, after inlining) comes first, so the walk reads only that.
//!
//! **Facts for callers** ([`CallerFacts`]: noreturn, nounwind, size) are kept with the code. At
//! the edit and release tiers no caller's code reads them (an inline decision reads the callee's
//! *own* facts, from its plan, see [`crate::inline`]); a tier whose callers read them must put
//! them into the caller's key.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;
use std::fmt;

use crate::inline::{InlineAttr, OwnFacts, SizeClass};
use crate::key::{ItemId, Symbol};
use crate::codec::{CodecError, Digest, In, Out, Payload};

/// An instance's plan: its calls in MIR order (each callee once, at its first call) and its own
/// facts. Made from the body before it is lowered; found by the plan key.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Plan {
    pub symbol: Symbol,
    pub calls: Vec<Symbol, Global>,
    pub facts: OwnFacts,
}

impl Payload for Plan {
    const NAME: &'static str = "blaze program plan";

    fn encode_body(&self, out: &mut Out) {
        self.symbol.encode(out);
        out.count(self.calls.len());
        for call in &self.calls {
            call.encode(out);
        }
        encode_facts(&self.facts, out);
    }

    fn decode_body(input: &mut In<'_>) -> Result<Plan, CodecError> {
        let symbol = Symbol::decode(input)?;
        let count = input.count(64)?;
        let mut calls = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            calls.push(Symbol::decode(input)?);
        }
        let facts = decode_facts(input)?;
        Ok(Plan { symbol, calls, facts })
    }
}

fn encode_facts(facts: &OwnFacts, out: &mut Out) {
    out.u8(facts.size as u8);
    out.u8(facts.attr as u8);
    out.bool(facts.cold);
}

fn decode_facts(input: &mut In<'_>) -> Result<OwnFacts, CodecError> {
    let size = match input.u8()? {
        0 => SizeClass::Tiny,
        1 => SizeClass::Small,
        2 => SizeClass::Medium,
        3 => SizeClass::Large,
        _ => return Err(input.error("a size class no class has")),
    };
    let attr = match input.u8()? {
        0 => InlineAttr::None,
        1 => InlineAttr::Hint,
        2 => InlineAttr::Always,
        3 => InlineAttr::Never,
        _ => return Err(input.error("an inline attribute no attribute has")),
    };
    let cold = input.bool()?;
    Ok(OwnFacts { size, attr, cold })
}

/// How a relocation's field holds its target's address `S`, with addend `A`, at the field's
/// address `P`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum RelocKind {
    /// 8 bytes, `S + A`.
    Abs64,
    /// 4 bytes, `S + A - P`, signed (x86_64 `call rel32`, `lea rip+rel32`).
    Rel32,
    /// The low 26 bits of an arm64 `b`/`bl`: `(S + A - P) >> 2`, signed, a multiple of 4,
    /// within 128 MiB.
    Arm64Branch26,
}

/// Why a relocation's field cannot hold its target.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelocError {
    /// The field runs past the bytes.
    OutOfBounds,
    /// The distance does not fit the field (`Rel32`, `Arm64Branch26`), or is not a multiple of
    /// 4 (`Arm64Branch26`).
    OutOfRange { distance: i64 },
}

impl RelocKind {
    pub fn width(self) -> usize {
        match self {
            RelocKind::Abs64 => 8,
            RelocKind::Rel32 | RelocKind::Arm64Branch26 => 4,
        }
    }

    /// Writes `target + addend` into `bytes[at..]` as this kind, the field being at address
    /// `place`.
    pub fn write(
        self,
        bytes: &mut [u8],
        at: usize,
        place: u64,
        target: u64,
        addend: i64,
    ) -> Result<(), RelocError> {
        let Some(field) = bytes.get_mut(at..at.saturating_add(self.width())) else {
            return Err(RelocError::OutOfBounds);
        };
        if field.len() != self.width() {
            return Err(RelocError::OutOfBounds);
        }
        match self {
            RelocKind::Abs64 => {
                field.copy_from_slice(&target.wrapping_add_signed(addend).to_le_bytes());
            }
            RelocKind::Rel32 => {
                let distance =
                    (target as i64).wrapping_add(addend).wrapping_sub(place as i64);
                let Ok(value) = i32::try_from(distance) else {
                    return Err(RelocError::OutOfRange { distance });
                };
                field.copy_from_slice(&value.to_le_bytes());
            }
            RelocKind::Arm64Branch26 => {
                let distance =
                    (target as i64).wrapping_add(addend).wrapping_sub(place as i64);
                if distance % 4 != 0 || !(-(1i64 << 27)..(1i64 << 27)).contains(&distance) {
                    return Err(RelocError::OutOfRange { distance });
                }
                let mut word = [0u8; 4];
                word.copy_from_slice(field);
                let old = u32::from_le_bytes(word);
                let imm = ((distance >> 2) as u32) & 0x03ff_ffff;
                field.copy_from_slice(&((old & 0xfc00_0000) | imm).to_le_bytes());
            }
        }
        Ok(())
    }

    /// The target address the field at `bytes[at..]` holds, the field being at `place`.
    pub fn read(self, bytes: &[u8], at: usize, place: u64, addend: i64) -> Option<u64> {
        let field = bytes.get(at..at.checked_add(self.width())?)?;
        match self {
            RelocKind::Abs64 => {
                let mut word = [0u8; 8];
                word.copy_from_slice(field);
                Some(u64::from_le_bytes(word).wrapping_add_signed(addend.wrapping_neg()))
            }
            RelocKind::Rel32 => {
                let mut word = [0u8; 4];
                word.copy_from_slice(field);
                let value = i64::from(i32::from_le_bytes(word));
                Some((place as i64).wrapping_add(value).wrapping_sub(addend) as u64)
            }
            RelocKind::Arm64Branch26 => {
                let mut word = [0u8; 4];
                word.copy_from_slice(field);
                let imm = u32::from_le_bytes(word) & 0x03ff_ffff;
                // Sign-extend the 26 bits, then scale by 4.
                let signed = i64::from(((imm << 6) as i32) >> 6);
                Some((place as i64).wrapping_add(signed << 2).wrapping_sub(addend) as u64)
            }
        }
    }

    pub(crate) fn tag(self) -> u8 {
        self as u8
    }

    pub(crate) fn from_tag(tag: u8) -> Option<RelocKind> {
        match tag {
            0 => Some(RelocKind::Abs64),
            1 => Some(RelocKind::Rel32),
            2 => Some(RelocKind::Arm64Branch26),
            _ => None,
        }
    }
}

/// What a relocation's field points at, by stable identity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Target {
    /// A function instance: its entry (in a program of slots, its stub).
    Function(Symbol),
    Static(Symbol),
    /// One of this record's own panic locations, by index into its `locations`: a location
    /// record (file, line, column) the program places and keeps.
    Location(u32),
    /// An extern, by index into this record's `externs`.
    Extern(u32),
}

impl Target {
    fn encode(&self, out: &mut Out) {
        match self {
            Target::Function(symbol) => {
                out.u8(0);
                symbol.encode(out);
            }
            Target::Static(symbol) => {
                out.u8(1);
                symbol.encode(out);
            }
            Target::Location(index) => {
                out.u8(2);
                out.u32(*index);
            }
            Target::Extern(index) => {
                out.u8(3);
                out.u32(*index);
            }
        }
    }

    fn decode(input: &mut In<'_>) -> Result<Target, CodecError> {
        match input.u8()? {
            0 => Ok(Target::Function(Symbol::decode(input)?)),
            1 => Ok(Target::Static(Symbol::decode(input)?)),
            2 => Ok(Target::Location(input.u32()?)),
            3 => Ok(Target::Extern(input.u32()?)),
            _ => Err(input.error("a relocation target of a tag no target has")),
        }
    }
}

/// One address in a record's bytes: at byte `at`, of `kind`, pointing at `target + addend`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct Reloc {
    pub at: u32,
    pub kind: RelocKind,
    pub target: Target,
    pub addend: i64,
}

impl Reloc {
    fn encode(&self, out: &mut Out) {
        out.u32(self.at);
        out.u8(self.kind.tag());
        self.target.encode(out);
        out.i64(self.addend);
    }

    fn decode(input: &mut In<'_>) -> Result<Reloc, CodecError> {
        let at = input.u32()?;
        let Some(kind) = RelocKind::from_tag(input.u8()?) else {
            return Err(input.error("a relocation of a kind no kind has"));
        };
        let target = Target::decode(input)?;
        let addend = input.i64()?;
        Ok(Reloc { at, kind, target, addend })
    }
}

/// A panic location, item-relative: the item whose text it is in (the instance's own, or an
/// inlinee's), the line counted from that item's first line, and the column.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LocRow {
    pub item: ItemId,
    pub line: u32,
    pub col: u32,
}

/// A line row: the code from `offset` on is from `line` of `item` (item-relative). Kept for
/// debug info; not placed yet (see the crate's report).
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct LineRow {
    pub offset: u32,
    pub item: ItemId,
    pub line: u32,
}

/// What a caller may learn of an instance once it is lowered.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub struct CallerFacts {
    pub noreturn: bool,
    pub nounwind: bool,
    /// The code's size in bytes.
    pub size: u32,
}

/// An instance's code record. Build it with [`InstanceCode::new`], which checks every
/// relocation and derives the head.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct InstanceCode {
    symbol: Symbol,
    calls: Vec<Symbol, Global>,
    statics: Vec<Symbol, Global>,
    facts: CallerFacts,
    code: Vec<u8, Global>,
    unwind: Vec<u8, Global>,
    relocs: Vec<Reloc, Global>,
    locations: Vec<LocRow, Global>,
    lines: Vec<LineRow, Global>,
    externs: Vec<Vec<u8, Global>, Global>,
}

/// What the walk reads of a code record: the functions and statics its code refers to, each
/// once, in the order of their first relocation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodeHead {
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
}

/// Why a code record was refused: the instance, the relocation's index, what was wrong.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CodeError {
    pub symbol: Symbol,
    pub reloc: u32,
    pub what: &'static str,
}

impl fmt::Display for CodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the code of {}, relocation {}: {}", self.symbol, self.reloc, self.what)
    }
}

/// The parts of a code record, in the order [`InstanceCode::new`] takes them.
pub struct CodeParts {
    pub symbol: Symbol,
    pub facts: CallerFacts,
    pub code: Vec<u8, Global>,
    pub unwind: Vec<u8, Global>,
    pub relocs: Vec<Reloc, Global>,
    pub locations: Vec<LocRow, Global>,
    pub lines: Vec<LineRow, Global>,
    pub externs: Vec<Vec<u8, Global>, Global>,
}

impl InstanceCode {
    /// The record of `parts`, its relocations checked (each field inside the code, no two
    /// overlapping, each location and extern index in its list) and sorted by place.
    pub fn new(parts: CodeParts) -> Result<InstanceCode, CodeError> {
        let CodeParts { symbol, facts, code, unwind, mut relocs, locations, lines, externs } =
            parts;
        relocs.sort_unstable_by_key(|reloc| reloc.at);
        let mut calls = Vec::new_in(Global);
        let mut statics = Vec::new_in(Global);
        let mut seen_calls = blaze_hash::HashSet::new_in(Global);
        let mut seen_statics = blaze_hash::HashSet::new_in(Global);
        let mut end = 0usize;
        for (index, reloc) in relocs.iter().enumerate() {
            let error = |what| CodeError { symbol, reloc: index as u32, what };
            let at = reloc.at as usize;
            if at < end {
                return Err(error("overlaps the relocation before it"));
            }
            end = at + reloc.kind.width();
            if end > code.len() {
                return Err(error("runs past the end of the code"));
            }
            match reloc.target {
                Target::Function(callee) => {
                    if seen_calls.insert(callee) {
                        calls.push(callee);
                    }
                }
                Target::Static(target) => {
                    if seen_statics.insert(target) {
                        statics.push(target);
                    }
                }
                Target::Location(at) if at as usize >= locations.len() => {
                    return Err(error("names a location past the record's locations"));
                }
                Target::Extern(at) if at as usize >= externs.len() => {
                    return Err(error("names an extern past the record's externs"));
                }
                Target::Location(_) | Target::Extern(_) => {}
            }
        }
        Ok(InstanceCode { symbol, calls, statics, facts, code, unwind, relocs, locations, lines, externs })
    }

    pub fn symbol(&self) -> Symbol {
        self.symbol
    }

    pub fn calls(&self) -> &[Symbol] {
        &self.calls
    }

    pub fn statics(&self) -> &[Symbol] {
        &self.statics
    }

    pub fn facts(&self) -> CallerFacts {
        self.facts
    }

    pub fn code(&self) -> &[u8] {
        &self.code
    }

    pub fn unwind(&self) -> &[u8] {
        &self.unwind
    }

    pub fn relocs(&self) -> &[Reloc] {
        &self.relocs
    }

    pub fn locations(&self) -> &[LocRow] {
        &self.locations
    }

    pub fn lines(&self) -> &[LineRow] {
        &self.lines
    }

    pub fn externs(&self) -> &[Vec<u8, Global>] {
        &self.externs
    }

    /// The bytes a program places for it: the code, then its unwind rows.
    pub fn placed_len(&self) -> u64 {
        (self.code.len() + self.unwind.len()) as u64
    }

    pub fn head(&self) -> CodeHead {
        CodeHead { calls: self.calls.clone(), statics: self.statics.clone() }
    }
}

impl CodeHead {
    /// The head of the code payload `bytes` (the core record's kind, version and key are
    /// checked by [`crate::keyed::code_payload`] first), reading nothing after it.
    pub fn decode_prefix(bytes: &[u8]) -> Result<CodeHead, CodecError> {
        let mut input = In::new(InstanceCode::NAME, bytes);
        let _symbol = Symbol::decode(&mut input)?;
        let calls = decode_symbols(&mut input)?;
        let statics = decode_symbols(&mut input)?;
        Ok(CodeHead { calls, statics })
    }
}

fn encode_symbols(symbols: &[Symbol], out: &mut Out) {
    out.count(symbols.len());
    for symbol in symbols {
        symbol.encode(out);
    }
}

fn decode_symbols(input: &mut In<'_>) -> Result<Vec<Symbol, Global>, CodecError> {
    let count = input.count(64)?;
    let mut out = Vec::with_capacity_in(count, Global);
    for _ in 0..count {
        out.push(Symbol::decode(input)?);
    }
    Ok(out)
}

impl Payload for InstanceCode {
    const NAME: &'static str = "blaze program instance code";

    fn encode_body(&self, out: &mut Out) {
        // The head first: the walk reads only it (`CodeHead::decode_prefix`).
        self.symbol.encode(out);
        encode_symbols(&self.calls, out);
        encode_symbols(&self.statics, out);
        out.bool(self.facts.noreturn);
        out.bool(self.facts.nounwind);
        out.u32(self.facts.size);
        out.bytes(&self.code);
        out.bytes(&self.unwind);
        out.count(self.relocs.len());
        for reloc in &self.relocs {
            reloc.encode(out);
        }
        out.count(self.locations.len());
        for row in &self.locations {
            out.digest(&row.item.0);
            out.u32(row.line);
            out.u32(row.col);
        }
        out.count(self.lines.len());
        for row in &self.lines {
            out.u32(row.offset);
            out.digest(&row.item.0);
            out.u32(row.line);
        }
        out.count(self.externs.len());
        for name in &self.externs {
            out.bytes(name);
        }
    }

    fn decode_body(input: &mut In<'_>) -> Result<InstanceCode, CodecError> {
        let symbol = Symbol::decode(input)?;
        let calls = decode_symbols(input)?;
        let statics = decode_symbols(input)?;
        let facts =
            CallerFacts { noreturn: input.bool()?, nounwind: input.bool()?, size: input.u32()? };
        let code = input.bytes_owned()?;
        let unwind = input.bytes_owned()?;
        let count = input.count(14)?;
        let mut relocs = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            relocs.push(Reloc::decode(input)?);
        }
        let count = input.count(40)?;
        let mut locations = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            locations.push(LocRow {
                item: ItemId(input.digest()?),
                line: input.u32()?,
                col: input.u32()?,
            });
        }
        let count = input.count(40)?;
        let mut lines = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            lines.push(LineRow {
                offset: input.u32()?,
                item: ItemId(input.digest()?),
                line: input.u32()?,
            });
        }
        let count = input.count(4)?;
        let mut externs = Vec::with_capacity_in(count, Global);
        for _ in 0..count {
            externs.push(input.bytes_owned()?);
        }
        let record = InstanceCode::new(CodeParts {
            symbol,
            facts,
            code,
            unwind,
            relocs,
            locations,
            lines,
            externs,
        })
        .map_err(|_| input.error("a relocation the record's own checks refuse"))?;
        if record.calls != calls || record.statics != statics {
            return Err(input.error("a head that is not its relocations' targets"));
        }
        Ok(record)
    }
}

/// The domain of a static value's key.
pub const STATIC_DOMAIN: &str = "blaze program static 1";

/// A static's initial value, as the front end evaluates it: its bytes and the addresses in them
/// (functions and statics only).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StaticValue {
    bytes: Vec<u8, Global>,
    relocs: Vec<Reloc, Global>,
}

/// The key of a static's value: what a program compares to know it changed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ValueKey(pub Digest);

impl StaticValue {
    /// The value, its relocations checked (inside the bytes, not overlapping, only functions
    /// and statics) and sorted; the error is the first refused relocation's index.
    pub fn new(bytes: Vec<u8, Global>, mut relocs: Vec<Reloc, Global>) -> Result<StaticValue, u32> {
        relocs.sort_unstable_by_key(|reloc| reloc.at);
        let mut end = 0usize;
        for (index, reloc) in relocs.iter().enumerate() {
            let at = reloc.at as usize;
            let fits = at >= end && at + reloc.kind.width() <= bytes.len();
            let target = matches!(reloc.target, Target::Function(_) | Target::Static(_));
            if !fits || !target {
                return Err(index as u32);
            }
            end = at + reloc.kind.width();
        }
        Ok(StaticValue { bytes, relocs })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn relocs(&self) -> &[Reloc] {
        &self.relocs
    }

    pub fn key(&self) -> ValueKey {
        let mut out = Out::new();
        out.bytes(&self.bytes);
        out.count(self.relocs.len());
        for reloc in &self.relocs {
            reloc.encode(&mut out);
        }
        let bytes = out.finish();
        ValueKey(Digest::of(STATIC_DOMAIN, &bytes))
    }

    /// The functions and statics the value points at, each once, in order of place.
    pub fn head(&self) -> CodeHead {
        let mut calls = Vec::new_in(Global);
        let mut statics = Vec::new_in(Global);
        for reloc in &self.relocs {
            match reloc.target {
                Target::Function(symbol) if !calls.contains(&symbol) => calls.push(symbol),
                Target::Static(symbol) if !statics.contains(&symbol) => statics.push(symbol),
                _ => {}
            }
        }
        CodeHead { calls, statics }
    }
}

/// The base key of a test's result (the core's `TestRun`): see [`crate::reach::test_key`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct TestKey(pub Digest);
