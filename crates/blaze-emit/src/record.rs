//! [`emit_record`]: one LIR function as a code record (docs/v2/plan/design/run.md, "End
//! state", "The code record"): position-independent code that holds no address, its outside
//! references as import sites and its address-holding data as a relro part, every target named
//! by (space, symbol). The pass's side is pass/sites.rs; here its rows of reference places
//! become `blaze_code` targets through the caller's [`Names`].
//!
//! The record comes back unsealed, with its frame word and one unwind row a call (pass/unwind.rs);
//! its caller then calls `CodeRecord::seal`, which sorts the targets by (space, symbol) and the sites by offset,
//! so the same function gives the same bytes (rule 25).

use core::fmt;

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_CODE, OWN_RELRO, Site, SiteKind, Space, Symbol, Target, Word};
use blaze_lir::View;
use blaze_own::Alloc;

use crate::{
    BULK_INLINE_MOST, Code, EmitError, Isa, RelocTo,
    pass::{
        self,
        sites::{Form, SiteTo, WordTo},
    },
};

/// What a function's code or data names outside itself, as the LIR function's reference rows
/// say it (a system routine is not among them: the record names it by its C name itself).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outside {
    /// The function of reference place `place` (`RefKind::Func`).
    Func(u32),
    /// The static of reference place `place` (`RefKind::Static`).
    Static(u32),
    /// The vtable of reference place `place` (`RefKind::Vtable`).
    Vtable(u32),
    /// The thread-local of reference place `place` (`RefKind::Tls`): its import word holds its
    /// offset in a thread's block.
    Tls(u32),
    /// The program's thread-local descriptor static (crates/blaze-build/src/tls.rs).
    TlsDesc,
    /// The program's thread-local getter, `fn(desc) -> block`.
    TlsGetter,
}

impl fmt::Display for Outside {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Outside::Func(place) => write!(f, "function reference {place}"),
            Outside::Static(place) => write!(f, "static reference {place}"),
            Outside::Vtable(place) => write!(f, "vtable reference {place}"),
            Outside::Tls(place) => write!(f, "thread-local reference {place}"),
            Outside::TlsDesc => f.write_str("the thread-local descriptor"),
            Outside::TlsGetter => f.write_str("the thread-local getter"),
        }
    }
}

/// How one [`Outside`] is named in the record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Named<'n> {
    /// By its symbol; its space is its kind's (a function's `Space::Func`, a static's and the
    /// descriptor's `Space::Static`, a vtable's `Space::Vtable`, a thread-local's `Space::Tls`).
    Symbol(Symbol),
    /// A C library name the launcher binds with `dlsym` (`Space::Outside`): a function or a
    /// static only, and with no addend.
    C(&'n str),
}

/// The caller's names for what the function references: the run path knows which instance,
/// static or C name each reference place is.
pub trait Names {
    /// `of`'s name, or `None` where the caller has none (refused: [`RecordEmitError::NoSymbol`]).
    fn name(&self, of: Outside) -> Option<Named<'_>>;
}

/// Why a function was not emitted as a code record.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecordEmitError {
    /// The pass refused it.
    Emit(EmitError),
    /// The function names `of`, and [`Names`] has no symbol for it.
    NoSymbol { of: Outside },
    /// `of` was given a C name, and only a function or a static has one.
    CName { of: Outside },
    /// A word of data holds the address of `of`, a C name, plus `addend`: a name is bound by
    /// `dlsym` alone (`blaze_code::image::ImageError::OutsideAddend` would refuse it later).
    CAddend { of: Outside, addend: i64 },
}

impl fmt::Display for RecordEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordEmitError::Emit(why) => why.fmt(f),
            RecordEmitError::NoSymbol { of } => write!(f, "{of} has no symbol: a code record names every target by (space, symbol) (run.md, \"The code record\")"),
            RecordEmitError::CName { of } => write!(f, "{of} was given a C name: only a function or a static is bound by `dlsym`"),
            RecordEmitError::CAddend { of, addend } => write!(f, "data holds the address of {of}, a C name, plus {addend}: a C name is bound by `dlsym` alone, with no addend"),
        }
    }
}

impl core::error::Error for RecordEmitError {}

/// Emits `func` (verified by `blaze_lir::verify`) as a code record in `alloc`, for the core
/// and system `isa` says: the record form of [`crate::emit`] (lib.rs, "The record form").
/// Every reference place the function names is named through `names`; no `Via` is asked, since
/// every call goes through its callee's import word.
///
/// The code is written by the pass into its own vector and copied once into the record's.
pub fn emit_record<'a>(func: View<'_>, names: &dyn Names, isa: Isa, alloc: Alloc<'a>) -> Result<CodeRecord<'a>, RecordEmitError> {
    emit_record_up_to(func, names, isa, BULK_INLINE_MOST, alloc).map(|(record, _)| record)
}

/// [`emit_record`], inlining a bulk operation of a constant length up to `inline_most` bytes;
/// also the bytes of instructions at the code's start (what follows is the function's data
/// rows holding no address: the tests check nothing else does).
pub(crate) fn emit_record_up_to<'a>(
    func: View<'_>,
    names: &dyn Names,
    isa: Isa,
    inline_most: u64,
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32), RecordEmitError> {
    let mut out = Code::new_in(alloc);
    let mut form = Form::new_in(alloc);
    pass::run(func, &[], isa, inline_most, &mut out.bytes, &mut out.fixups, &mut out.relocs, Some(&mut form))
        .map_err(RecordEmitError::Emit)?;
    if !out.fixups.is_empty() || !out.relocs.is_empty() {
        let why = "an address word asked of placement (the record form places none)";
        return Err(RecordEmitError::Emit(EmitError::Record { why }));
    }
    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(&out.bytes);
    record.relro.extend_from_slice(&form.relro);
    for row in form.sites.iter() {
        let site = match row.to {
            SiteTo::Relro(index) => {
                Site { at: row.at, kind: SiteKind::Relro, target: NO_TARGET, addend: form.data_at[index as usize] }
            }
            SiteTo::Import(to) => {
                let target = target_of(&mut record, names, to, 0)?;
                let kind = match row.call {
                    true => SiteKind::Call,
                    false => SiteKind::Load,
                };
                Site { at: row.at, kind, target, addend: 0 }
            }
        };
        record.sites.push(site);
    }
    for row in form.words.iter() {
        let target = match row.to {
            WordTo::OwnCode => OWN_CODE,
            WordTo::OwnRelro => OWN_RELRO,
            WordTo::Import(to) => target_of(&mut record, names, to, row.addend)?,
        };
        record.words.push(Word { at: row.at, target, addend: row.addend });
    }
    pass::unwind::into_record(&form.unwind, &mut record).map_err(RecordEmitError::Emit)?;
    Ok((record, form.insts))
}

/// The index of `to` among `record`'s targets (added again each time: `seal` merges equal
/// targets), for a site (`addend` 0) or a word holding its address plus `addend`.
fn target_of(record: &mut CodeRecord<'_>, names: &dyn Names, to: RelocTo, addend: i64) -> Result<u32, RecordEmitError> {
    let (of, space) = match to {
        // The C library's or the compiler runtime's: the record keeps the name for `dlsym`.
        RelocTo::Routine(routine) => return Ok(record.outside(routine.symbol())),
        // A function's address is its body's: its import word holds it, and the image rewrites
        // that word when an edit moves the body, so no slot stub stands between.
        RelocTo::Func(place) | RelocTo::SlotStub(place) => (Outside::Func(place), Space::Func),
        RelocTo::Static(place) => (Outside::Static(place), Space::Static),
        RelocTo::Vtable(place) => (Outside::Vtable(place), Space::Vtable),
        RelocTo::TlsOffset(place) => (Outside::Tls(place), Space::Tls),
        RelocTo::TlsDesc => (Outside::TlsDesc, Space::Static),
        RelocTo::TlsGetter => (Outside::TlsGetter, Space::Func),
        RelocTo::Code(_) => {
            let why = "the code's own address as an import (own data is reached PC-relatively or through the relro part)";
            return Err(RecordEmitError::Emit(EmitError::Record { why }));
        }
    };
    match names.name(of) {
        None => Err(RecordEmitError::NoSymbol { of }),
        Some(Named::Symbol(symbol)) => Ok(record.target(Target { space, symbol })),
        Some(Named::C(name)) => match space {
            Space::Func | Space::Static if addend == 0 => Ok(record.outside(name)),
            Space::Func | Space::Static => Err(RecordEmitError::CAddend { of, addend }),
            Space::Vtable | Space::Tls | Space::Outside => Err(RecordEmitError::CName { of }),
        },
    }
}
