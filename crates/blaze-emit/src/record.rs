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

use blaze_code::record::{CodeRecord, NO_TARGET, OWN_CODE, OWN_RELRO, Site, SiteKind, Space, Target, TrapRow, TrapSource, Word};
use blaze_lir::Verified;
use blaze_lir::legalize::LegalizeError;
use blaze_own::Alloc;

use crate::routines::{Called, RoutinePlaces};

use crate::{
    BULK_INLINE_MOST, Code, EmitError, Isa, RelocTo,
    pass::{
        self,
        sites::{Form, SiteTo, WordTo},
    },
};

/// What a function names outside itself ([`Outside`]), how the caller names it ([`Named`]) and
/// the caller's names ([`Names`]; one it lacks is refused: [`RecordEmitError::NoSymbol`]):
/// piece 0's, moved whole to crates/blaze-tier/src/names.rs (rule 30), so every tier names
/// outside references one way.
pub use blaze_tier::{Named, Names, Outside};

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
    /// blaze-lir's legalize refused the function (a vector, `f16` or `f128` form it does not
    /// make scalar, or an `f128` routine not bound).
    Legalize(LegalizeError),
    /// The build's trap source `ordinal` names an `unreachable` past the function's
    /// `unreachables` (blaze-code record.rs `TrapSource`): the build and the function disagree.
    TrapOrdinal { ordinal: u32, unreachables: u32 },
}

impl fmt::Display for RecordEmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RecordEmitError::Emit(why) => why.fmt(f),
            RecordEmitError::NoSymbol { of } => write!(f, "{of} has no symbol: a code record names every target by (space, symbol) (run.md, \"The code record\")"),
            RecordEmitError::CName { of } => write!(f, "{of} was given a C name: only a function or a static is bound by `dlsym`"),
            RecordEmitError::CAddend { of, addend } => write!(f, "data holds the address of {of}, a C name, plus {addend}: a C name is bound by `dlsym` alone, with no addend"),
            RecordEmitError::Legalize(why) => why.fmt(f),
            RecordEmitError::TrapOrdinal { ordinal, unreachables } => write!(f, "the build's trap source names `unreachable` {ordinal}, and the function has {unreachables}: the build's trap table and its function are out of step"),
        }
    }
}

impl core::error::Error for RecordEmitError {}

/// Emits `func` (the proof `blaze_lir::verify` answered: D163) as a code record in `alloc`, for the core
/// and system `isa` says: the record form of [`crate::emit`] (lib.rs, "The record form").
/// Every reference place the function names is named through `names`; no `Via` is asked, since
/// every call goes through its callee's import word.
///
/// The code is written by the pass into its own vector and copied once into the record's.
///
/// `routines` says where each compiler_builtins soft-float routine is among `names`' function
/// references (crate::routines); the answer's [`Called`] is the routines the record calls,
/// which the caller declares as its callees. [`RoutinePlaces::NONE`]: an `f128` operation is
/// refused naming its routine.
pub fn emit_record<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    isa: Isa,
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, Called), RecordEmitError> {
    emit_record_up_to(func, names, routines, isa, BULK_INLINE_MOST, alloc).map(|(record, _, called)| (record, called))
}

/// [`emit_record`], inlining a bulk operation of a constant length up to `inline_most` bytes;
/// also the bytes of instructions at the code's start (what follows is the function's data
/// rows holding no address: the tests check nothing else does).
pub(crate) fn emit_record_up_to<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    isa: Isa,
    inline_most: u64,
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    emit_record_sourced(func, names, routines, isa, inline_most, &[], alloc)
}

/// [`emit_record_up_to`] with the build's `traps`: each becomes the trap row of its
/// `unreachable`'s `brk` (blaze-code record.rs `TrapRow`); an `unreachable` no source names
/// gets no row.
pub(crate) fn emit_record_sourced<'a>(
    func: Verified<'_>,
    names: &dyn Names,
    routines: &RoutinePlaces,
    isa: Isa,
    inline_most: u64,
    traps: &[TrapSource],
    alloc: Alloc<'a>,
) -> Result<(CodeRecord<'a>, u32, Called), RecordEmitError> {
    // Below the entry the pass reads the view; the proof was the entry's to take (D163).
    let func = func.view();
    // The Reference tier takes no vector, `f16` or `f128` arithmetic (INTRINSICS.md 3.2): a
    // function holding any is made scalar first, on a copy in `alloc`, with a reference row for
    // each routine `routines` binds (the rows legalize's calls name); one holding none is
    // emitted as it is. Which routines it calls is legalize's answer, returned to the caller.
    let (legal, called) = crate::routines::legalized(func, routines, alloc).map_err(RecordEmitError::Legalize)?;
    let func = match &legal {
        None => func,
        Some(owned) => owned.view(),
    };
    let mut out = Code::new_in(alloc);
    let mut form = Form::new_in(alloc);
    pass::run(
        func,
        &[],
        isa,
        inline_most,
        &mut out.bytes,
        &mut out.fixups,
        &mut out.relocs,
        &mut out.positions,
        Some(&mut form),
        pass::Reach::Islands,
        alloc,
    )
    .map_err(RecordEmitError::Emit)?;
    if !out.fixups.is_empty() || !out.relocs.is_empty() {
        let why = "an address word asked of placement (the record form places none)";
        return Err(RecordEmitError::Emit(EmitError::Record { why }));
    }
    let mut record = CodeRecord::new_in(alloc);
    record.code.extend_from_slice(&out.bytes);
    record.relro.extend_from_slice(&form.relro);
    record.positions = form.positions;
    // What each part asks of its placement: the most its data rows' alignments (pass/sites.rs).
    record.code_align = form.code_align;
    record.relro_align = form.relro_align;
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
    for source in traps {
        let Some(&(at, held)) = form.unreachables.get(source.ordinal as usize) else {
            return Err(RecordEmitError::TrapOrdinal { ordinal: source.ordinal, unreachables: form.unreachables.len() as u32 });
        };
        record.traps.push(TrapRow { at, kind: source.kind, site: source.site, held, subject: source.subject });
    }
    Ok((record, form.insts, called))
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
        RelocTo::TlsIndex(place) => (Outside::Tls(place), Space::Tls),
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
