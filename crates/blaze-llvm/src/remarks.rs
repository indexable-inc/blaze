//! The inliner's remarks, read back (D100 point 7 (c)): one row per decision LLVM's inliner made
//! about a call site, from LLVM's own remark stream (YAML, started by `LLVMBlazeRemarksBegin`
//! for the inliner's passes only). The optimized record's read list is the bodies inlined,
//! transitively, plus the fact records of the callees still called: both come from these rows.
//!
//! An unreadable stream is an error, never a guess: a parse error, a remark of a pass that was
//! not asked for, a decision without its `Callee` or `Caller`, a name that is no symbol: each
//! is `RemarkError` by name. Analysis remarks (the inliner's cost notes) are not decisions and
//! are not rows; that is the one kind passed over, by its type, never by its text.

use core::ffi::c_void;
use core::fmt;

use blaze_code::record::Symbol;
use blaze_own::AVec;

use crate::ffi;
use crate::names::symbol_of;

/// The pass names whose remarks are streamed: a regular expression LLVM matches against each
/// remark's pass name. `inline` is the CGSCC and module inliners' `DEBUG_TYPE`, and the
/// always-inliner's.
pub const INLINER_PASSES: &str = "^(inline|always-inline)$";

/// One decision of the inliner: `callee`'s body was spliced into `caller` (`inlined`), or a call
/// of it was kept (`!inlined`). Sorted and unique in a compile's list (rule 25).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Remark {
    pub caller: Symbol,
    pub callee: Symbol,
    pub inlined: bool,
}

/// Why the remark stream could not be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RemarkError {
    /// LLVM's YAML parser stopped at an error (its message is not kept: it borrows the parser).
    Parse,
    /// A remark of a pass other than the inliner's.
    Pass,
    /// A remark of the inliner of a type that is no decision and no analysis.
    Type { ty: u32 },
    /// A decision without its `Callee` argument.
    NoCallee,
    /// A decision without its `Caller` argument.
    NoCaller,
    /// A `Callee` or `Caller` that names no symbol (not `bz` and hex, and no UTF-8 C name).
    Name { which: &'static str },
}

impl fmt::Display for RemarkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            RemarkError::Parse => f.write_str("LLVM's remark parser refused the stream"),
            RemarkError::Pass => f.write_str("a remark of a pass other than the inliner's"),
            RemarkError::Type { ty } => write!(f, "an inliner remark of type {ty}, neither passed, missed nor analysis"),
            RemarkError::NoCallee => f.write_str("an inliner decision without its Callee"),
            RemarkError::NoCaller => f.write_str("an inliner decision without its Caller"),
            RemarkError::Name { which } => write!(f, "an inliner decision whose {which} names no symbol"),
        }
    }
}

impl core::error::Error for RemarkError {}

/// Reads the YAML stream `yaml` into `out` (appended, then the whole list sorted and made
/// unique). An empty stream is no remark: a module with no call has no decision.
pub fn read(yaml: &[u8], out: &mut AVec<'_, Remark>) -> Result<(), RemarkError> {
    if yaml.is_empty() {
        return Ok(());
    }
    // SAFETY: `yaml` outlives the parser, which borrows it (Remarks.h); the parser is disposed
    // on every path below; each entry is disposed after its strings were last read (a string
    // lives as long as its entry).
    unsafe {
        let parser = ffi::LLVMRemarkParserCreateYAML(yaml.as_ptr().cast::<c_void>(), yaml.len() as u64);
        if parser.is_null() {
            return Err(RemarkError::Parse);
        }
        let result = read_all(parser, out);
        ffi::LLVMRemarkParserDispose(parser);
        result?;
    }
    out.sort_unstable();
    out.dedup();
    Ok(())
}

/// # Safety
///
/// `parser` is a live parser.
unsafe fn read_all(parser: ffi::RemarkParserRef, out: &mut AVec<'_, Remark>) -> Result<(), RemarkError> {
    loop {
        // SAFETY: the caller's contract.
        let entry = unsafe { ffi::LLVMRemarkParserGetNext(parser) };
        if entry.is_null() {
            // SAFETY: as above.
            return match unsafe { ffi::LLVMRemarkParserHasError(parser) } {
                0 => Ok(()),
                _ => Err(RemarkError::Parse),
            };
        }
        // SAFETY: `entry` is live until disposed here, after `one` has read it.
        let row = unsafe { one(entry) };
        // SAFETY: the entry came from the parser and is disposed once.
        unsafe { ffi::LLVMRemarkEntryDispose(entry) };
        if let Some(remark) = row? {
            out.push(remark);
        }
    }
}

/// One entry's decision, or `None` for an analysis remark.
///
/// # Safety
///
/// `entry` is live.
unsafe fn one(entry: ffi::RemarkEntryRef) -> Result<Option<Remark>, RemarkError> {
    // SAFETY: the entry is live (the caller's contract), and so are its strings and arguments.
    unsafe {
        let pass = ffi::remark_bytes(ffi::LLVMRemarkEntryGetPassName(entry));
        if pass != b"inline" && pass != b"always-inline" {
            return Err(RemarkError::Pass);
        }
        let inlined = match ffi::LLVMRemarkEntryGetType(entry) {
            ffi::REMARK_PASSED => true,
            ffi::REMARK_MISSED => false,
            ffi::REMARK_ANALYSIS | ffi::REMARK_ANALYSIS_FP_COMMUTE | ffi::REMARK_ANALYSIS_ALIASING => {
                return Ok(None);
            }
            ty @ (ffi::REMARK_UNKNOWN | ffi::REMARK_FAILURE) => return Err(RemarkError::Type { ty }),
            ty => return Err(RemarkError::Type { ty }),
        };
        let mut callee = None;
        let mut caller = None;
        let mut arg = ffi::LLVMRemarkEntryGetFirstArg(entry);
        while !arg.is_null() {
            let key = ffi::remark_bytes(ffi::LLVMRemarkArgGetKey(arg));
            let value = ffi::remark_bytes(ffi::LLVMRemarkArgGetValue(arg));
            // The first of each: the inliner names the decision's pair first (later arguments
            // name the call site's place and the cost).
            if key == b"Callee" && callee.is_none() {
                callee = Some(symbol_of(value).ok_or(RemarkError::Name { which: "Callee" })?);
            } else if key == b"Caller" && caller.is_none() {
                caller = Some(symbol_of(value).ok_or(RemarkError::Name { which: "Caller" })?);
            }
            arg = ffi::LLVMRemarkEntryGetNextArg(arg, entry);
        }
        let callee = callee.ok_or(RemarkError::NoCallee)?;
        let caller = caller.ok_or(RemarkError::NoCaller)?;
        Ok(Some(Remark { caller, callee, inlined }))
    }
}
