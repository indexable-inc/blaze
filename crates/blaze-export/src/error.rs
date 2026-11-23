//! Why an export was refused: each error names the function or datum it is about, where in it,
//! and the rule that refused it. Nothing is guessed and nothing is skipped (rule 3).

use core::fmt;

use blaze_code::eh::EhError;
use blaze_code::image::ImageError;
use blaze_code::place::PlaceError;
use blaze_code::record::{RecordError, Symbol, Target};
use blaze_code::unwind::UnwindError;
use blaze_tier::record::FormError;
use blaze_native::unwind::UnwindInfoError;

use crate::native_merge::MergeError;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ExportError {
    /// A site of a placed function could not be completed (its instruction is not the one its
    /// site says, or its target is past the instruction's reach): blaze-code's own error.
    Patch(ImageError),
    /// One function is given twice.
    TwoRecords { symbol: Symbol },
    /// One static or vtable is given twice.
    TwoData { target: Target },
    /// A datum is given under a space that is not a static's, a vtable's, or a thread-local's.
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
    /// A datum's word names a thread-local: a thread-local has no single address across threads.
    DataTlsTarget { from: Target, target: Target },
    /// A function or datum names a Rust thread-local (`Space::Tls`), and the program names no
    /// [`crate::TlsRuntime`] (`descriptor` and `ender`).
    NoTlsRuntime { target: Target },
    /// `TlsRuntime::ender` is not among the program's functions.
    TlsEnderNotPlaced { ender: Symbol },
    /// An address relocation (`Page21`, `PageOff12`) names a thread-local: a thread-local is
    /// reached through its index word (`GotPage21`/`GotPageOff12`) or relro `Abs64`.
    TlsAddress { from: Symbol, at: u32 },
    /// A call site names something that is not a function (a static, a vtable).
    CallOfData { from: Symbol, at: u32 },
    /// A record names an outside (C) symbol whose name text it does not hold.
    OutsideNameMissing { from: Symbol, target: Target },
    /// A word names an outside symbol plus an offset: the loader binds a name, not a name plus
    /// an offset.
    OutsideAddend { from: Symbol, target: Target },
    /// A word's address (its target's place plus its addend) is outside the image.
    WordOutside { from: Symbol, at: u64 },
    /// A function's unwind rows broke blaze-code's rules (unwind.rs `read`).
    UnwindRows { symbol: Symbol, error: UnwindError },
    /// A function's LSDA could not be written (blaze-code eh.rs `lsda_table`).
    Lsda { symbol: Symbol, error: EhError },
    /// A function's row lands, and the program names no personality routine to land it.
    NoPersonality { symbol: Symbol },
    /// The personality routine the program names is not among the functions placed.
    PersonalityNotPlaced { personality: Symbol },
    /// A placed function has no record among those the unwind tables were given: a bug here.
    EhNoRecord { symbol: Symbol },
    /// The unwind tables outgrew the room `eh_room` measured for them (`Laid::tables_room`, not
    /// the page the layout rounded it to): a bug in the measure, refused in every build.
    EhRoom { need: u64, room: u64 },
    /// A second pass wrote `what` at `written` bytes where its first pass measured `measured`:
    /// a bug in the writer, refused in every build (never a debug assertion).
    Remeasured { what: &'static str, measured: u64, written: u64 },
    /// The image grew past what one of its encodings can say (`what`).
    TooLarge { what: &'static str, len: u64 },
    /// `what`'s header and load commands need `need` bytes and the layout kept `room` before the
    /// code (macho.rs `command_room`, computed from the same commands): refused in every build.
    HeaderRoom { what: &'static str, need: u64, room: u64 },
    /// A start record this crate makes broke the record's form: a bug here, named.
    StartRecord(RecordError),
    /// A `bzcode02` record of `symbol` (a start record brought into the form by
    /// optimized.rs `v1_to_v2`) broke the form's rules (blaze-tier record/form.rs).
    Record { symbol: Symbol, error: FormError },
    /// A relocation of a `bzcode02` record could not be completed by blaze-tier's placer rules
    /// (a page offset's instruction or alignment, an x86-64 kind): its error, naming the
    /// function and the record offset.
    Place(PlaceError),
    /// An address relocation (`Page21`, `PageOff12`, `Branch26` aside) names a C name: only the
    /// loader knows its address, through its GOT word.
    AddressOfOutside { from: Symbol, target: Target, at: u32 },
    /// The program's native image could not be merged (native_merge.rs, NATIVE.md W9).
    Native(MergeError),
    /// The file's one `__unwind_info` (Rust rows and the native image's) was refused by its
    /// writer (blaze-native unwind.rs).
    NativeUnwind { error: UnwindInfoError },
    /// `Platform::Wasm64` takes Wasm64 IR via [`crate::wasm64::export_wasm64`], not AArch64 machine code records.
    Wasm64RequiresWasmProgram,
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
                write!(f, "export: a datum is given as {:?} {}; data are statics, vtables, and thread-locals", target.space, target.symbol)
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
            ExportError::DataTlsTarget { from, target } => write!(
                f,
                "export: {:?} {} holds a word naming thread-local {}; a thread-local has no single address across threads",
                from.space, from.symbol, target.symbol
            ),
            ExportError::NoTlsRuntime { target } => write!(
                f,
                "export: {:?} {} is a thread-local, and the program names no TlsRuntime (descriptor and ender)",
                target.space, target.symbol
            ),
            ExportError::TlsEnderNotPlaced { ender } => {
                write!(f, "export: the thread-local ender {ender} is not among the program's functions")
            }
            ExportError::TlsAddress { from, at } => write!(
                f,
                "export: function {from}: the address relocation at byte {at} names a thread-local; a thread-local is reached through its index word"
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
            ExportError::UnwindRows { symbol, error } => write!(f, "export: function {symbol}'s unwind rows are refused: {error}"),
            ExportError::Lsda { symbol, error } => write!(f, "export: function {symbol}'s LSDA: {error}"),
            ExportError::NoPersonality { symbol } => write!(
                f,
                "export: function {symbol} has a row that lands, and the program names no personality routine (its `eh_personality` lang item) for the unwind tables (blaze-export eh_out.rs)"
            ),
            ExportError::PersonalityNotPlaced { personality } => {
                write!(f, "export: the personality routine {personality} is not among the program's functions (blaze-export eh_out.rs)")
            }
            ExportError::EhNoRecord { symbol } => {
                write!(f, "export: placed function {symbol} has no record among those given to the unwind tables (a bug in blaze-export eh_out.rs)")
            }
            ExportError::EhRoom { need, room } => write!(
                f,
                "export: the unwind tables need {need} bytes after the code and the layout kept {room} (a bug in blaze-export eh_out.rs `eh_room`)"
            ),
            ExportError::Remeasured { what, measured, written } => write!(
                f,
                "export: {what} was measured at {measured} bytes and written at {written} (a bug in blaze-export: the measure and the writer disagree)"
            ),
            ExportError::TooLarge { what, len } => write!(f, "export: the image is too large for {what} ({len} bytes)"),
            ExportError::HeaderRoom { what, need, room } => write!(
                f,
                "export: {what}'s header and load commands need {need} bytes and the layout kept {room} before the code (blaze-export macho.rs `command_room`)"
            ),
            ExportError::StartRecord(error) => write!(f, "export: the start record blaze-export made is not a record: {error}"),
            ExportError::Record { symbol, error } => {
                write!(f, "export: function {symbol}: its bzcode02 record breaks the form (blaze-tier record/form.rs): {error}")
            }
            ExportError::Place(error) => write!(f, "export: {error}"),
            ExportError::AddressOfOutside { from, target, at } => write!(
                f,
                "export: function {from}: the address relocation at byte {at} names the C name {}, whose address only the loader knows; a C name is reached through its GOT word (rule: export never leaves a hole (run.md, \"Export\"))",
                target.symbol
            ),
            ExportError::Native(error) => write!(f, "export: {error} (NATIVE.md 3.9)"),
            ExportError::NativeUnwind { error } => {
                write!(f, "export: the unwind index over the program's and the native image's rows is refused: {error:?} (NATIVE.md 3.9)")
            }
            ExportError::Wasm64RequiresWasmProgram => {
                write!(f, "export: Platform::Wasm64 takes a Wasm64Program via blaze_export::export_wasm64, not AArch64 code records")
            }
        }
    }
}

impl core::error::Error for ExportError {}
