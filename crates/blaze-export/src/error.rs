//! Why an export was refused: each error names the function or datum it is about, where in it,
//! and the rule that refused it. Nothing is guessed and nothing is skipped (rule 3).

use core::fmt;

use blaze_code::image::ImageError;
use blaze_code::record::{RecordError, Symbol, Target};

use crate::input::Panic;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportError {
    /// A site of a placed function could not be completed (its instruction is not the one its
    /// site says, or its target is past the instruction's reach): blaze-code's own error.
    Patch(ImageError),
    /// One function is given twice.
    TwoRecords { symbol: Symbol },
    /// One static or vtable is given twice.
    TwoData { target: Target },
    /// A datum is given under a space that is not a static's or a vtable's.
    DataSpace { target: Target },
    /// A datum's alignment is not a power of two of at most a page.
    DataAlign { target: Target, align: u32 },
    /// A datum's word is not 8-aligned or runs past its bytes.
    DataWordOutside { target: Target, at: u32 },
    /// A zeroed datum is given bytes or words.
    ZeroedWithBytes { target: Target },
    /// The entry's function is not among the functions given.
    NoEntry { symbol: Symbol },
    /// bstd's hosted entry needs the launcher's host table, which an exported file has none of.
    Hosted { start: Symbol },
    /// A function's site or word names a target that no function or datum given places: an
    /// export never leaves a hole (no stub, no lazy binding).
    OutOfReach { from: Symbol, target: Target },
    /// A datum's word names a target that nothing given places.
    DataOutOfReach { from: Target, target: Target },
    /// A thread-local is named: an exported file has no thread-local template yet.
    ThreadLocal { from: Symbol },
    /// A call site names something that is not a function (a static, a vtable).
    CallOfData { from: Symbol, at: u32 },
    /// A record names an outside (C) symbol whose name text it does not hold.
    OutsideNameMissing { from: Symbol, target: Target },
    /// A word names an outside symbol plus an offset: the loader binds a name, not a name plus
    /// an offset.
    OutsideAddend { from: Symbol, target: Target },
    /// A word's address (its target's place plus its addend) is outside the image.
    WordOutside { from: Symbol, at: u64 },
    /// The function has landing pads: an exported file carries no LSDA rows yet.
    LandingPads { symbol: Symbol },
    /// The function calls the unwinder (`name`), and the file carries no unwind tables yet: a
    /// panic would end the program by `abort` where Rust's rule (`panic` given) says otherwise.
    Unwinds { from: Symbol, name: &'static str, panic: Panic },
    /// The image grew past what one of its encodings can say (`what`).
    TooLarge { what: &'static str, len: u64 },
    /// A start record this crate makes broke the record's form: a bug here, named.
    StartRecord(RecordError),
}

impl From<ImageError> for ExportError {
    fn from(error: ImageError) -> ExportError {
        ExportError::Patch(error)
    }
}

const HOLE: &str = "rule: export never leaves a hole (run.md, \"Export\")";

impl fmt::Display for ExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            ExportError::Patch(error) => write!(f, "export: {error}"),
            ExportError::TwoRecords { symbol } => write!(f, "export: function {symbol} is given twice"),
            ExportError::TwoData { target } => write!(f, "export: {:?} {} is given twice", target.space, target.symbol),
            ExportError::DataSpace { target } => {
                write!(f, "export: a datum is given as {:?} {}; data are statics and vtables", target.space, target.symbol)
            }
            ExportError::DataAlign { target, align } => write!(
                f,
                "export: {:?} {} asks for alignment {align}, not a power of two of at most a page",
                target.space, target.symbol
            ),
            ExportError::DataWordOutside { target, at } => write!(
                f,
                "export: {:?} {}: the word at byte {at} is not 8-aligned or runs past its bytes",
                target.space, target.symbol
            ),
            ExportError::ZeroedWithBytes { target } => {
                write!(f, "export: {:?} {} is zeroed and is given bytes or words", target.space, target.symbol)
            }
            ExportError::NoEntry { symbol } => write!(f, "export: the entry {symbol} is not among the functions given"),
            ExportError::Hosted { start } => write!(
                f,
                "export: {start} is bstd's hosted entry, which takes the launcher's host table; an exported file has no launcher and no table yet"
            ),
            ExportError::OutOfReach { from, target } => write!(
                f,
                "export: function {from} names {:?} {}, which no function or datum given places ({HOLE})",
                target.space, target.symbol
            ),
            ExportError::DataOutOfReach { from, target } => write!(
                f,
                "export: {:?} {} names {:?} {}, which no function or datum given places ({HOLE})",
                from.space, from.symbol, target.space, target.symbol
            ),
            ExportError::ThreadLocal { from } => write!(
                f,
                "export: function {from} names a thread-local; an exported file has no thread-local template yet (run.md, \"Export (writer, 2026-11-16)\")"
            ),
            ExportError::CallOfData { from, at } => {
                write!(f, "export: function {from}: the call site at byte {at} names something that is not a function")
            }
            ExportError::OutsideNameMissing { from, target } => {
                write!(f, "export: function {from} names the outside symbol {} without its name text", target.symbol)
            }
            ExportError::OutsideAddend { from, target } => write!(
                f,
                "export: function {from}: a word names the outside symbol {} plus an offset; the loader binds a name alone",
                target.symbol
            ),
            ExportError::WordOutside { from, at } => {
                write!(f, "export: function {from}: the word at image offset {at:#x} points outside the image")
            }
            ExportError::LandingPads { symbol } => write!(
                f,
                "export: function {symbol} has landing pads, and an exported file carries no LSDA rows yet (run.md, \"Export (writer, 2026-11-16)\")"
            ),
            ExportError::Unwinds { from, name, panic: Panic::Unwind } => write!(
                f,
                "export: function {from} calls `{name}`: the program can unwind (`panic = \"unwind\"`, Rust's default), and an exported file carries no unwind tables yet, so a panic would abort instead; refused, never exported wrong"
            ),
            ExportError::Unwinds { from, name, panic: Panic::Abort } => write!(
                f,
                "export: function {from} calls `{name}`: its record was made for `panic = \"unwind\"`, and the program's profile says `panic = \"abort\"`"
            ),
            ExportError::TooLarge { what, len } => write!(f, "export: the image is too large for {what} ({len} bytes)"),
            ExportError::StartRecord(error) => write!(f, "export: the start record blaze-export made is not a record: {error}"),
        }
    }
}

impl core::error::Error for ExportError {}
