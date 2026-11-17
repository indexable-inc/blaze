//! A placed function's unwind entry: its record's call frame information made a CIE and an
//! FDE at the function's address, and told to the system's unwinder.
//!
//! The bytes are `.eh_frame`'s, with every address absolute (`DW_EH_PE_absptr`): the
//! function, its length, its language-specific data area and its personality routine, which
//! is the routine's entry thunk, so it too is compiled when the unwinder first calls it. A
//! panic then unwinds through code the launcher placed as through linked code, a lazily
//! bound body and a replaced one alike: an entry is registered before a word names its body
//! and taken back only when the body is freed, which is after no thread can be in it.
//!
//! How the unwinder is told (`__register_frame`, `__deregister_frame`): Apple's libunwind
//! takes the address of one FDE; libgcc's and LLVM's on ELF take a run of entries that a
//! zero length ends. Both symbols are in the library the launcher is linked against
//! (libSystem; libgcc_s), and `tests::every_macos_symbol_resolves` looks them up.

use blaze_launch::record::Unwind;

unsafe extern "C" {
    fn __register_frame(entry: *const u8);
    fn __deregister_frame(entry: *const u8);
}

/// Clears `out` and writes the entry of a function at `start`, `len` bytes long
/// (`blaze_launch::frame::entry`, where the bytes are made and their layout is said). Gives
/// the FDE's offset in `out`.
pub fn entry(
    unwind: &Unwind<'_>,
    start: u64,
    len: u64,
    personality: Option<u64>,
    lsda: Option<u64>,
    out: &mut Vec<u8>,
) -> Result<usize, String> {
    blaze_launch::frame::entry(unwind, start, len, personality, lsda, out).map(|made| made.fde)
}

/// What [`register`] takes of an entry at `at` whose FDE is `fde` bytes in.
fn handle(at: usize, fde: usize) -> *const u8 {
    // Apple's libunwind is told of one FDE; the ELF unwinders of a run of entries.
    #[cfg(target_os = "macos")]
    let entry = at + fde;
    #[cfg(not(target_os = "macos"))]
    let entry = {
        let _ = fde;
        at
    };
    entry as *const u8
}

/// Tells the unwinder of the entry at address `at` ([`entry`]'s bytes, copied there), whose
/// FDE is `fde` bytes in.
///
/// # Safety
///
/// `at` is the address of the bytes [`entry`] wrote, in memory that stays as it is until
/// [`deregister`] is called for the same entry; the function it describes is placed at the
/// address the entry says.
pub unsafe fn register(at: usize, fde: usize) {
    // SAFETY: the caller's contract: a whole, ended run of entries that outlives its
    // registration.
    unsafe { __register_frame(handle(at, fde)) }
}

/// Takes back what [`register`] told.
///
/// # Safety
///
/// `at` and `fde` are those of an entry [`register`] was called with, once, and no thread
/// is, or will be, in the function it describes.
pub unsafe fn deregister(at: usize, fde: usize) {
    // SAFETY: the caller's contract: registered, and deregistered once.
    unsafe { __deregister_frame(handle(at, fde)) }
}
