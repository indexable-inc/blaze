//! The header (ABI and safety) of a fn a MODULE-LEVEL `extern` block declares: its block's.
//!
//! A foreign fn's own text carries no ABI (`fn snprintf(s: *mut c_char, n: usize, format:
//! *const c_char, ...) -> c_int;`): the block's does. The Rust Reference, "External blocks":
//! [items.extern.abi] "By default external blocks assume that the library they are calling uses
//! the standard C ABI on the specific platform", and the default when no string is written,
//! [items.extern.abi.default], is "C" (the names source answers `"C"` for a bare `extern`:
//! front2/source.rs `extern_abi`); [items.extern.fn.safety] "A function declared in an extern
//! block is implicitly unsafe unless the safe function qualifier is present."
//!
//! heads.rs `fn_head` read the fn's OWN head (`abi_of`: no `extern` on the fn, so "Rust") for
//! every fn but those a block in a BODY declares (super::foreign_locals). So a C-variadic
//! foreign fn of a module-level block, `unsafe extern "C" { fn snprintf(.., ...) -> c_int; }`,
//! was a "Rust" fn and its `...` was refused: "`...` on a fn of an ABI other than \"C\" or
//! \"system\"" (std rungs s8-ffi-02 and s8-ffi-03, census class C3). The program side already
//! reads the block (program/head_abi.rs `head_abi`, through program.rs `placed`): this is the
//! same question asked of the same source, for the head.
//!
//! What it answers, by the block's string:
//! - "C", "C-unwind", "system", "system-unwind", "Rust", "rust-call": the checker's [`tc::Abi`]
//!   of that name, and `unsafe` unless the fn is written `safe`;
//! - "llvm-intrinsic" (front2/llvm_intrinsic.rs): the checker's [`tc::Abi`] has no such variant,
//!   and such a declaration is never called by an ABI: program/tabled.rs takes it by its
//!   `link_name` as a target intrinsic. Its header keeps the ABI of its own head, as before
//!   this file, BY NAME here (the wanted change is a `tc::Abi::LlvmIntrinsic`, the checker's);
//!   a `...` on one is then refused by heads.rs's variadic rule;
//! - any other string: refused by name (`aapcs`, `stdcall`, ..: none is written in a block on
//!   the hosts blaze builds for).
//! An item whose block the source did not answer (over records: its items row, whose `foreign`
//! field holds the block's ABI, did not read) is refused by name: a fn is never taken for a Rust
//! fn by a missing answer.

use blaze_id::ItemKey as LibKey;
use blaze_lower::machine::sig;
use blaze_surface::Range;
use blaze_typeck as tc;

use super::source::NamesSource;
use super::types::Why;

/// Why a module-level foreign fn's header is refused.
const UNANSWERED: &str = "a fn with no body whose `extern` block the names source did not answer (`extern_abi`: the items row's `foreign` field)";
const OTHER_ABI: &str = "a fn of an `extern` block whose ABI is none of \"C\", \"C-unwind\", \"system\", \"system-unwind\", \"Rust\", \"rust-call\" and \"llvm-intrinsic\"";

/// What a module-level block's ABI string says of the fns it declares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BlockAbi {
    /// One of the checker's ABIs.
    Checker(tc::Abi),
    /// `extern "llvm-intrinsic"`: no checker ABI (module doc); the fn's own head's is kept.
    LlvmIntrinsic,
}

/// The block ABI `written` (unquoted, as `NamesSource::extern_abi` gives it), or `None` for a
/// string this front end does not read (the caller refuses it by name).
pub(super) fn block_abi(written: &str) -> Option<BlockAbi> {
    match written {
        "C" => Some(BlockAbi::Checker(tc::Abi::C)),
        "C-unwind" => Some(BlockAbi::Checker(tc::Abi::CUnwind)),
        "system" => Some(BlockAbi::Checker(tc::Abi::System)),
        "system-unwind" => Some(BlockAbi::Checker(tc::Abi::SystemUnwind)),
        "Rust" => Some(BlockAbi::Checker(tc::Abi::Rust)),
        "rust-call" => Some(BlockAbi::Checker(tc::Abi::RustCall)),
        // front2/llvm_intrinsic.rs `ABI` (that file's `mod` line is another wiring's).
        "llvm-intrinsic" => Some(BlockAbi::LlvmIntrinsic),
        _ => None,
    }
}

/// The header of fn `lib` (head `flags`, its own head's header `own`) when a module-level
/// `extern` block declares it; `own` when none does. Only a free fn with no body can be one
/// (a member of a trait or impl, or a fn or static with a body, never is: "External blocks",
/// [items.extern.fn.body] "they must not have a body"), so nothing else asks the source.
pub(super) fn module_foreign_header<'n, S: NamesSource<'n>>(names: S, lib: LibKey, member: bool, flags: u32, own: tc::FnPtrHeader, whole: Range) -> Result<tc::FnPtrHeader, Why> {
    if member || flags & sig::HAS_BODY != 0 {
        return Ok(own);
    }
    let block = match names.extern_abi(lib) {
        None => return Err(Why::Unsupported(UNANSWERED, whole)),
        Some(None) => return Ok(own),
        Some(Some(written)) => written,
    };
    let safety = if flags & sig::SAFE != 0 { tc::Safety::Safe } else { tc::Safety::Unsafe };
    match block_abi(block) {
        Some(BlockAbi::Checker(abi)) => Ok(tc::FnPtrHeader { abi, safety, ..own }),
        Some(BlockAbi::LlvmIntrinsic) => Ok(tc::FnPtrHeader { safety, ..own }),
        None => Err(Why::Unsupported(OTHER_ABI, whole)),
    }
}

#[cfg(test)]
mod tests {
    use super::{BlockAbi, block_abi};
    use blaze_typeck as tc;

    /// A block's string names its fns' ABI; the default ("C" for a bare `extern`, which the
    /// names source already answers) and the unwinding forms stay distinct, and an unread
    /// string is `None` (refused by the caller), never "C" or "Rust".
    ///
    /// A fix that read every block as "C" fails the "C-unwind" and "Rust" lines; one that
    /// guessed an unread string fails the last.
    #[test]
    fn a_block_string_is_its_fns_abi() {
        assert_eq!(block_abi("C"), Some(BlockAbi::Checker(tc::Abi::C)));
        assert_eq!(block_abi("C-unwind"), Some(BlockAbi::Checker(tc::Abi::CUnwind)));
        assert_eq!(block_abi("system"), Some(BlockAbi::Checker(tc::Abi::System)));
        assert_eq!(block_abi("Rust"), Some(BlockAbi::Checker(tc::Abi::Rust)));
        assert_eq!(block_abi("llvm-intrinsic"), Some(BlockAbi::LlvmIntrinsic));
        assert_eq!(block_abi("aapcs"), None);
        println!("block abi: C, C-unwind, system, Rust read; llvm-intrinsic by name; aapcs refused");
    }
}
