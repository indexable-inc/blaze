//! The records the walk reads: an instance's [`Plan`] (by its plan key), the head of its code
//! ([`CodeHead`]: what the code refers to after inlining, read from the code record, which is
//! codegen's), a static's [`StaticValue`] and a test's base key ([`TestKey`]).
//!
//! The code record itself (bytes, relocations, unwind and line rows) is codegen's, filed under
//! the instance key ([`crate::keyed::code_key`]); the linker places it. This crate needs only
//! its head, so it holds no code.
//!
//! **The floor.** A plan is its symbol, its calls and three facts: written once per plan key,
//! read whole by the walk (a few hundred bytes). A head is two lists of symbols. A static's
//! value key is one hash over its bytes and fields: what the walk compares to know it changed.

#![blaze::sans_io]
#![deny(blaze::sans_io_core)]

use std::alloc::Global;

use crate::codec::{CodecError, Digest, In, Out, Payload};
use crate::inline::{InlineAttr, OwnFacts, SizeClass};
use crate::key::Symbol;

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

/// How a field of a static's value holds its target's address `S`, with addend `A`, at the
/// field's address `P`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum RelocKind {
    /// 8 bytes, `S + A`.
    Abs64,
    /// 4 bytes, `S + A - P`, signed.
    Rel32,
    /// The low 26 bits of an arm64 `b`/`bl`: `(S + A - P) >> 2`, signed.
    Arm64Branch26,
}

impl RelocKind {
    pub fn width(self) -> usize {
        match self {
            RelocKind::Abs64 => 8,
            RelocKind::Rel32 | RelocKind::Arm64Branch26 => 4,
        }
    }

    pub(crate) fn tag(self) -> u8 {
        self as u8
    }
}

/// What a field of a static's value points at, by stable identity: never an instance key, so a
/// callee's edit leaves the value's key as it was.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Target {
    Function(Symbol),
    Static(Symbol),
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
        }
    }
}

/// One address in a static's value: at byte `at`, of `kind`, pointing at `target + addend`.
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
}

/// What the walk reads of a code record, or of a static's value: the functions and statics it
/// refers to, each once, in the order of their first field. For code, after inlining: a callee
/// taken in is not in it, and what that callee refers to is.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodeHead {
    pub calls: Vec<Symbol, Global>,
    pub statics: Vec<Symbol, Global>,
}

/// The domain of a static value's key.
pub const STATIC_DOMAIN: &str = "blaze program static 1";

/// A static's initial value, as the front end evaluates it: its bytes and the addresses in them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct StaticValue {
    bytes: Vec<u8, Global>,
    relocs: Vec<Reloc, Global>,
}

/// The key of a static's value: what a program compares to know it changed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ValueKey(pub Digest);

impl StaticValue {
    /// The value, its fields checked (inside the bytes, not overlapping) and sorted; the error
    /// is the first refused field's index.
    pub fn new(bytes: Vec<u8, Global>, mut relocs: Vec<Reloc, Global>) -> Result<StaticValue, u32> {
        relocs.sort_unstable_by_key(|reloc| reloc.at);
        let mut end = 0usize;
        for (index, reloc) in relocs.iter().enumerate() {
            let at = reloc.at as usize;
            if at < end || at + reloc.kind.width() > bytes.len() {
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
